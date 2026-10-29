//! Fixture-only dispatch through the actual native owner task queue.
use super::{Flags, Game, Integrated, Serving};
use crate::game::levels::capture_integration::{CaptureCompletion, CaptureError, CaptureRequest};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender, TryRecvError, channel, sync_channel},
    },
    time::{Duration, Instant},
};

/// A test assertion must not strand the native owner in the admission wrapper.
struct ReleaseOnDrop(Option<SyncSender<()>>);
impl ReleaseOnDrop {
    fn release(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.try_send(());
        }
    }
}
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.release();
    }
}

fn settle_completion(receiver: Receiver<CaptureCompletion>) -> CaptureCompletion {
    receiver.recv().unwrap_or_else(|_| {
        Err(CaptureError::completion_lost(
            "native capture completion sender was lost; inspect complete epochs before retry",
        ))
    })
}

pub(in crate::game) fn dispatch_capture(
    game: &mut Game<'_>,
    request: CaptureRequest,
) -> CaptureCompletion {
    dispatch(game, request, false, CaptureKind::Archive)
}

pub(in crate::game) fn dispatch_capture_after_observer_timeout(
    game: &mut Game<'_>,
    request: CaptureRequest,
) -> CaptureCompletion {
    dispatch(game, request, true, CaptureKind::Archive)
}

pub(in crate::game) fn dispatch_machine_capture(
    game: &mut Game<'_>,
    request: CaptureRequest,
) -> CaptureCompletion {
    dispatch(game, request, false, CaptureKind::Machine)
}
pub(in crate::game) fn dispatch_slot_capture(
    game: &mut Game<'_>,
    request: CaptureRequest,
) -> CaptureCompletion {
    dispatch(game, request, false, CaptureKind::Slot)
}
pub(in crate::game) fn dispatch_slot_capture_after_observer_timeout(
    game: &mut Game<'_>,
    request: CaptureRequest,
) -> CaptureCompletion {
    dispatch(game, request, true, CaptureKind::Slot)
}

#[derive(Clone, Copy)]
enum CaptureKind {
    Archive,
    Machine,
    Slot,
}

fn dispatch(
    game: &mut Game<'_>,
    request: CaptureRequest,
    observe_timeout: bool,
    kind: CaptureKind,
) -> CaptureCompletion {
    // The fixture has a channel-backed integrated owner with profile UUID 11.
    // Game stays on this test thread; only owned handles/channels cross threads.
    assert!(
        game.serving.is_none(),
        "fixture must not replace a live server"
    );
    let flags = Arc::new(Flags::default());
    let (tasks, pending) = channel();
    game.serving = Some(Serving {
        flags: flags.clone(),
        tasks: pending,
        owner: 11,
        pending: None,
    });
    let integrated = Integrated {
        flags,
        thread: None,
        tasks,
        world_name: "native capture fixture".into(),
    };
    let client = std::thread::spawn(move || {
        let (completed, completion) = sync_channel(1);
        let (admitted, admission) = sync_channel(1);
        let (release, blocked) = sync_channel(1);
        let mut release = ReleaseOnDrop(Some(release));
        let finished = Arc::new(AtomicBool::new(false));
        let task_finished = finished.clone();
        let started = Instant::now();
        let observer = integrated.call(move |owner| {
            if observe_timeout {
                admitted
                    .send(())
                    .expect("client retains admission receiver");
                blocked.recv().expect("client releases admitted task");
            }
            // No callback, delay, tick or pump is injected into capture itself.
            let result = match kind {
                CaptureKind::Archive => owner.capture_fixture_epoch(request),
                CaptureKind::Machine => owner.capture_machine_fixture_epoch(request),
                CaptureKind::Slot => owner.capture_slot_fixture_epoch(request),
            };
            task_finished.store(true, Ordering::SeqCst);
            let _ = completed.send(result);
        });
        if observe_timeout {
            assert!(
                observer.is_none(),
                "actual ten-second observer must time out"
            );
            assert!(started.elapsed() >= Duration::from_secs(10));
            admission
                .recv_timeout(Duration::from_secs(5))
                .expect("the actual owner task was admitted before timeout");
            assert!(!finished.load(Ordering::SeqCst));
            let (later, later_seen) = sync_channel(1);
            let later_finished = finished.clone();
            integrated
                .tasks
                .send(Box::new(move |_| {
                    assert!(later_finished.load(Ordering::SeqCst));
                    let _ = later.send(());
                }))
                .expect("queue later native task");
            assert_eq!(later_seen.try_recv(), Err(TryRecvError::Empty));
            release.release();
            let result = settle_completion(completion);
            assert!(finished.load(Ordering::SeqCst));
            // run_tasks collected the first batch before this later task existed.
            // A subsequent native batch can run it only after capture has finished.
            later_seen
                .recv()
                .expect("later task runs after capture settles");
            println!(
                "CAPTURE_TASK_TIMEOUT observer=None admitted=true later_excluded=true final_receipts=1"
            );
            result
        } else {
            // Even an unexpectedly long native IO operation may outlast call().
            // Always wait for the independent final result, with no implicit retry.
            let result = settle_completion(completion);
            assert!(finished.load(Ordering::SeqCst));
            println!(
                "CAPTURE_TASK_COMPLETE observer_some={} final_receipts=1",
                observer.is_some()
            );
            result
        }
    });
    while !client.is_finished() {
        game.run_tasks();
        std::thread::yield_now();
    }
    // A failed client might have queued work before losing its receipt. Settle
    // that native batch too; dropping the observer never cancels queued capture.
    game.run_tasks();
    // No tick/network pump occurred, including after the observer was lost.
    let result = client.join().unwrap_or_else(|_| {
        Err(CaptureError::completion_lost(
            "capture client panicked; publication may have completed",
        ))
    });
    let _ = game.serving.take();
    result
}

#[test]
fn capture_completion_owned_types_are_send() {
    fn assert_send<T: Send>() {}
    assert_send::<CaptureRequest>();
    assert_send::<CaptureCompletion>();
}

#[test]
fn capture_completion_lost_sender_is_uncertain() {
    use crate::game::levels::capture_integration::{Failure, Phase, Publication};
    let (sender, receiver) = sync_channel(1);
    drop(sender);
    let error = settle_completion(receiver).unwrap_err();
    assert_eq!(error.phase, Phase::Dispatch);
    assert_eq!(error.code, Failure::CompletionLost);
    assert_eq!(error.publication, Publication::Uncertain);
}

#[test]
fn capture_completion_unsent_native_task_does_not_execute_or_report_success() {
    use crate::game::levels::capture_integration::{Failure, Publication};
    let (tasks, owner) = channel();
    drop(owner);
    let integrated = Integrated {
        flags: Arc::default(),
        thread: None,
        tasks,
        world_name: "closed capture fixture".into(),
    };
    let (completed, completion) = sync_channel::<CaptureCompletion>(1);
    let invoked = Arc::new(AtomicBool::new(false));
    let task_invoked = invoked.clone();
    assert!(
        integrated
            .call(move |_| {
                task_invoked.store(true, Ordering::SeqCst);
                drop(completed);
            })
            .is_none()
    );
    assert!(!invoked.load(Ordering::SeqCst));
    let error = settle_completion(completion).unwrap_err();
    assert_eq!(error.code, Failure::CompletionLost);
    assert_eq!(error.publication, Publication::Uncertain);
}
