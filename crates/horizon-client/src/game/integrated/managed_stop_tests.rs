//! Root runs the actual resource cases separately in fresh owned environments.
use super::*;
use crate::game::{
    integrated::transition_ownership_tests::Fixture,
    levels::session_checkpoint::CheckpointTestFault, mod_packages::Packages,
};
use horizon_client::save::SessionLock;
use std::{
    collections::BTreeSet,
    sync::atomic::{AtomicU64, Ordering},
};

struct Scratch(PathBuf, bool);
impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "horizon-managed-stop-{}-{}-{label}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path, false)
    }
    fn dir(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir(&path).unwrap();
        path
    }
    fn paths(&self) -> StartPaths {
        StartPaths {
            world: self.0.join("world"),
            archive: self.dir("archive"),
            namespace: self.dir("authority"),
        }
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        if self.1 || std::thread::panicking() {
            eprintln!("MANAGED_STOP_RETAINED {}", self.0.display());
        } else {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

fn start(paths: StartPaths, fault: CheckpointTestFault) -> ManagedStart {
    let fixture = Fixture::new("managed-stop-resources");
    let template = fixture.game();
    let mut options = template.options.clone();
    options.test_terrain = true;
    let pair = PathBuf::from(
        std::env::var_os("HORIZON_COUNTER_PAIR_FIXTURE")
            .expect("prebuilt pair required; no nested builds"),
    );
    options.mod_packages = Packages::read(&BTreeSet::new(), &[pair]).unwrap();
    let counter_type = options
        .mod_packages
        .catalog()
        .unwrap()
        .prepared()
        .values()
        .flat_map(|p| {
            p.content()
                .machine_types()
                .types()
                .iter()
                .map(|ty| ty.id().to_owned())
        })
        .next()
        .unwrap();
    ManagedStart {
        resources: template.res.clone(),
        pack: template.pack.clone(),
        options,
        settings: template.settings.clone(),
        paths,
        counter_type,
        position: [2, 120, 2],
        attempt_timeout: Duration::from_secs(120),
        first_stop_fault: Some(fault),
    }
}

#[test]
fn managed_epoch_selector_counts_pending_and_refuses_invalid_or_exhausted_archives() {
    let scratch = Scratch::new("epochs");
    let archive = scratch.dir("archive");
    assert_eq!(next_session_checkpoint_epoch(&archive).unwrap(), 1);
    fs::create_dir(archive.join("epoch-00000000000000000001")).unwrap();
    fs::create_dir(archive.join(".pending-00000000000000000003")).unwrap();
    assert_eq!(next_session_checkpoint_epoch(&archive).unwrap(), 4);
    fs::create_dir(archive.join("epoch-4")).unwrap();
    assert!(next_session_checkpoint_epoch(&archive).is_err());
    assert!(archive.join(".pending-00000000000000000003").is_dir());
    let full = scratch.dir("full");
    for epoch in 1..=256 {
        fs::create_dir(full.join(format!("epoch-{epoch:020}"))).unwrap();
    }
    assert!(next_session_checkpoint_epoch(&full).is_err());
    assert_eq!(fs::read_dir(&full).unwrap().count(), 256);
    let overflow = scratch.dir("overflow");
    fs::create_dir(overflow.join(format!(".pending-{:020}", u64::MAX))).unwrap();
    assert!(next_session_checkpoint_epoch(&overflow).is_err());
    let file = scratch.dir("file");
    fs::write(file.join("epoch-00000000000000000001"), b"unchanged").unwrap();
    assert!(next_session_checkpoint_epoch(&file).is_err());
    assert_eq!(
        fs::read(file.join("epoch-00000000000000000001")).unwrap(),
        b"unchanged"
    );
}

#[test]
fn managed_route_refuses_archive_namespace_overlap_before_any_write() {
    let scratch = Scratch::new("route");
    let archive = scratch.dir("archive");
    fs::write(archive.join("sentinel"), b"unchanged").unwrap();
    let paths = StartPaths {
        world: scratch.0.join("world"),
        archive: archive.clone(),
        namespace: archive.clone(),
    };
    assert!(checked_start_paths(&paths).is_err());
    assert_eq!(fs::read_dir(&archive).unwrap().count(), 1);
    assert_eq!(fs::read(archive.join("sentinel")).unwrap(), b"unchanged");
    assert!(!paths.world.exists());
    let paths = StartPaths {
        world: scratch.0.join("fresh"),
        archive: scratch.dir("empty"),
        namespace: scratch.dir("namespace"),
    };
    checked_start_paths(&paths).unwrap();
    assert!(!paths.world.exists());
}

#[cfg(unix)]
#[test]
fn managed_epoch_and_route_refuse_links_without_following_targets() {
    use std::os::unix::fs::symlink;
    let scratch = Scratch::new("links");
    let archive = scratch.dir("archive");
    let target = scratch.dir("target");
    symlink(&target, archive.join(".pending-00000000000000000001")).unwrap();
    assert!(next_session_checkpoint_epoch(&archive).is_err());
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    let namespace = scratch.dir("namespace");
    let alias = scratch.0.join("alias");
    symlink(&namespace, &alias).unwrap();
    assert!(
        checked_start_paths(&StartPaths {
            world: scratch.0.join("world"),
            archive: target,
            namespace: alias
        })
        .is_err()
    );
    assert_eq!(fs::read_dir(namespace).unwrap().count(), 0);
}

#[test]
fn managed_controls_admit_one_request_and_reject_stale_or_inflight_without_worker_tasks() {
    let (sender, receiver) = mpsc::sync_channel(1);
    let shared = Arc::new(Mutex::new(SupervisorView {
        state: State::Ready,
        last_admitted: 0,
        last_refusal: None,
        connected: None,
        observation: None,
        connected_observation: None,
    }));
    let control = ManagedStopControl {
        shared: shared.clone(),
        command: sender,
    };
    control
        .request(Command::RequestStop { attempt: 1 })
        .unwrap();
    assert_eq!(
        control.request(Command::RequestStop { attempt: 1 }),
        Err(Refusal::StaleAttempt)
    );
    assert_eq!(
        control.request(Command::RequestStop { attempt: 2 }),
        Err(Refusal::InFlight)
    );
    assert!(matches!(
        receiver.try_recv().unwrap(),
        Command::RequestStop { attempt: 1 }
    ));
    assert!(receiver.try_recv().is_err());
    shared.lock().unwrap().state = State::RetainedFailure {
        attempt: 1,
        epoch: Some(2),
        publication: Publication::NotPublished,
        detail: "fixture".into(),
    };
    assert_eq!(
        control.request(Command::RetryStop {
            attempt: 2,
            failed_attempt: 9
        }),
        Err(Refusal::State)
    );
    let observer = control.clone();
    drop(observer);
    control
        .request(Command::RetryStop {
            attempt: 2,
            failed_attempt: 1,
        })
        .unwrap();
    assert!(matches!(
        receiver.try_recv().unwrap(),
        Command::RetryStop {
            attempt: 2,
            failed_attempt: 1
        }
    ));
}

fn actual_failure_retry(fault: CheckpointTestFault, publication: Publication, label: &str) {
    let mut scratch = Scratch::new(label);
    scratch.1 = true;
    let paths = scratch.paths();
    let configured = start(paths.clone(), fault);
    let mut ready = None;
    let mut retained = None;
    let mut observer = None;
    let mut phase = 0;
    let mut failure_seen = None;
    let mut errors: Vec<String> = Vec::new();
    let started = Instant::now();
    let result = run_owned_managed_stop(configured, |view, control| {
        if started.elapsed() > Duration::from_secs(400) {
            panic!("fatal fixture deadline, retained owner must not detach");
        }
        match &view.state {
            State::Ready if phase == 0 => {
                ready = view.observation.clone();
                observer = Some(control.clone());
                phase = 1;
                if control
                    .request(Command::RequestStop { attempt: 1 })
                    .is_err()
                {
                    errors.push("first command refused".into());
                }
                if control.request(Command::RequestStop { attempt: 1 })
                    != Err(Refusal::StaleAttempt)
                {
                    errors.push("duplicate accepted".into());
                }
                if !matches!(
                    control.request(Command::RequestStop { attempt: 2 }),
                    Err(Refusal::InFlight | Refusal::State)
                ) {
                    errors.push("extra command accepted".into());
                }
            }
            State::RetainedFailure {
                attempt,
                epoch,
                publication: actual,
                ..
            } if phase == 1 => {
                retained = view.observation.clone();
                if *attempt != 1 || *epoch != Some(2) || *actual != publication {
                    errors.push("wrong publication/attempt".into());
                }
                if ready != retained {
                    errors.push("retained native owner/clock/projections changed".into());
                }
                if SessionLock::acquire(&paths.world).is_ok() {
                    errors.push("native lock released during failure".into());
                }
                let clone = scratch.dir("copied-anchor");
                fs::copy(
                    paths.world.join("horizon-world-identity.json"),
                    clone.join("horizon-world-identity.json"),
                )
                .unwrap();
                let ns = AuthorityNamespace::new(&paths.namespace).unwrap();
                if ns
                    .acquire_recovery(&clone, &paths.archive, &scratch.0.join("forbidden-copy"))
                    .is_ok()
                {
                    errors.push("UUID authority released during failure".into());
                }
                let evidence = paths.archive.join(match publication {
                    Publication::NotPublished => ".pending-00000000000000000002",
                    Publication::Uncertain => "epoch-00000000000000000002",
                });
                if !evidence.is_dir() {
                    errors.push("failure evidence missing".into());
                }
                drop(observer.take());
                observer = Some(control.clone());
                if control.request(Command::RetryStop {
                    attempt: 2,
                    failed_attempt: 99,
                }) != Err(Refusal::State)
                {
                    errors.push("stale failed attempt accepted".into());
                }
                phase = 2;
                failure_seen = Some(Instant::now());
            }
            State::RetainedFailure { .. }
                if phase == 2
                    && failure_seen
                        .is_some_and(|at: Instant| at.elapsed() >= Duration::from_millis(100)) =>
            {
                if observer.as_ref().unwrap().view().observation != ready {
                    errors.push("retained observation changed while waiting".into());
                }
                phase = 3;
                return DriverCommand::RetryStop {
                    attempt: 2,
                    failed_attempt: 1,
                };
            }
            _ => {}
        }
        DriverCommand::Wait
    });
    // Assert only after the predetermined retry has resolved and worker joined.
    let receipt = result.unwrap();
    assert_eq!(phase, 3);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(receipt.epoch, 3);
    assert_eq!(ready, retained);
    assert_eq!(ready.as_ref().unwrap().guests, 0);
    assert!(ready.as_ref().unwrap().empty_slot);
    assert!(paths.archive.join("epoch-00000000000000000001").is_dir());
    assert!(
        paths
            .archive
            .join(match publication {
                Publication::NotPublished => ".pending-00000000000000000002",
                Publication::Uncertain => "epoch-00000000000000000002",
            })
            .is_dir()
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(receipt.root.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["epoch"], 3);
    assert_eq!(manifest["saved_clock"], ready.as_ref().unwrap().clock);
    let saved_frame: serde_json::Value =
        serde_json::from_slice(&fs::read(receipt.root.join("host/counter.json")).unwrap()).unwrap();
    assert_eq!(
        saved_frame,
        serde_json::to_value(&ready.as_ref().unwrap().frame).unwrap()
    );
    assert!(SessionLock::acquire(&paths.world).is_ok());
    let clone = scratch.0.join("copied-anchor");
    let lease = AuthorityNamespace::new(&paths.namespace)
        .unwrap()
        .acquire_recovery(&clone, &paths.archive, &scratch.0.join("after-join"))
        .unwrap();
    assert_eq!(lease.world(), ready.unwrap().world);
    assert!(matches!(
        observer.unwrap().view().state,
        State::Stopped { attempt: 2, .. }
    ));
}

#[test]
#[ignore = "isolated pinned resources/prebuilt pair: actual worker retained after real copied bytes, explicit retry"]
fn managed_actual_not_published_retains_native_owners_and_explicit_retry_releases_after_receipt() {
    actual_failure_retry(
        CheckpointTestFault::Copied,
        Publication::NotPublished,
        "copied",
    );
}
#[test]
#[ignore = "isolated pinned resources/prebuilt pair: actual worker retained after real promotion, explicit retry"]
fn managed_actual_uncertain_retains_promoted_image_and_same_owners_until_explicit_retry() {
    actual_failure_retry(
        CheckpointTestFault::Promoted,
        Publication::Uncertain,
        "promoted",
    );
}
#[test]
#[ignore = "DEDICATED EXPECTED-FATAL CHILD ONLY: root verifies actual SIGABRT/fatal marker/artifacts, not a libtest pass"]
fn managed_supervisor_panic_is_actual_process_fatal_with_retained_native_game() {
    assert_eq!(
        std::env::var("HORIZON_MANAGED_FATAL_CHILD").as_deref(),
        Ok("1"),
        "requires dedicated root-orchestrated expected-fatal child"
    );
    let scratch = Scratch::new("fatal");
    let paths = scratch.paths();
    let configured = start(paths.clone(), CheckpointTestFault::Promoted);
    let mut requested = false;
    let result = run_owned_managed_stop(configured, |view, _| {
        if matches!(view.state, State::Ready) && !requested {
            requested = true;
            return DriverCommand::RequestStop(1);
        }
        if let State::RetainedFailure {
            attempt: 1,
            epoch: Some(2),
            publication: Publication::Uncertain,
            ..
        } = &view.state
        {
            eprintln!(
                "MANAGED_EXPECTED_FATAL_ARTIFACT {}",
                serde_json::json!({"root":scratch.0,"world":paths.world,"archive":paths.archive,"namespace":paths.namespace,"baseline_epoch":1,"promoted_epoch":2,"publication":"Uncertain"})
            );
            panic!("intentional supervisor driver panic with actual retained Game");
        }
        DriverCommand::Wait
    });
    panic!("expected fatal child unexpectedly returned: {result:?}");
}

#[test]
#[ignore = "isolated resources/prebuilt pair: direct closed pollable owner retains copied failure until explicit retry"]
fn managed_actual_closed_pollable_owner_retries_and_completes_once() {
    let mut scratch = Scratch::new("pollable-closed");
    scratch.1 = true;
    let paths = scratch.paths();
    let anchor = scratch.dir("pollable-copied-anchor");
    let namespace = AuthorityNamespace::new(&paths.namespace).unwrap();
    let mut worker =
        ManagedWorker::start_closed(start(paths.clone(), CheckpointTestFault::Copied)).unwrap();
    let control = worker.control();
    let mut observer = Some(worker.control());
    let mut errors = Vec::new();
    let mut premature = worker.take_completion();
    if premature.is_some() {
        errors.push("completion available before first explicit closed poll".to_owned());
    }
    let began = Instant::now();
    let mut ready = None;
    let mut retained = None;
    let mut ready_polls = 0;
    let mut failure_polls = 0;
    let mut pending_polls = 0;
    let mut close_sent = false;
    let mut retry_sent = false;
    let mut stopped_before_poll = false;
    loop {
        if began.elapsed() > Duration::from_secs(400) {
            panic!(
                "pollable closed owner did not resolve: {:?}",
                control.view()
            );
        }
        let snapshot = control.view();
        if retry_sent && matches!(snapshot.state, State::Stopping { .. }) {
            if let Some(early) = worker.take_completion() {
                errors.push("in-flight retry made completion consumable".to_owned());
                premature.get_or_insert(early);
            }
            thread::sleep(Duration::from_millis(1));
            continue;
        }
        if retry_sent && matches!(snapshot.state, State::Stopped { .. }) {
            stopped_before_poll = true;
            if let Some(early) = worker.take_completion() {
                errors.push("Stopped status classified closed completion without poll".to_owned());
                premature.get_or_insert(early);
            }
        }
        let polled = worker.poll();
        if polled.pipes.is_some() {
            errors.push("closed-profile owner exposed a connection offer".to_owned());
        }
        if polled.completed {
            break;
        }
        pending_polls += 1;
        if let Some(early) = worker.take_completion() {
            errors.push("unfinished closed poll made completion consumable".to_owned());
            premature.get_or_insert(early);
        }
        let view = polled.view;
        match &view.state {
            State::Ready if !close_sent => {
                ready_polls += 1;
                if let Some(previous) = &ready
                    && view.observation.as_ref() != Some(previous)
                {
                    errors
                        .push("native closed owner changed across pending Ready polls".to_owned());
                }
                if let Some(observation) = &view.observation {
                    ready.get_or_insert_with(|| observation.clone());
                } else {
                    errors.push("Ready lacks actual native closed observation".to_owned());
                }
                if ready_polls >= 3 {
                    if let Err(error) = fs::copy(
                        paths.world.join("horizon-world-identity.json"),
                        anchor.join("horizon-world-identity.json"),
                    ) {
                        errors.push(format!("copy explicit UUID anchor: {error}"));
                    }
                    match control.request(Command::RequestStop { attempt: 1 }) {
                        Ok(()) => close_sent = true,
                        Err(error) => errors.push(format!("direct closed stop refused: {error:?}")),
                    }
                }
            }
            State::RetainedFailure {
                attempt,
                epoch,
                publication,
                ..
            } if !retry_sent => {
                failure_polls += 1;
                retained = view.observation.clone();
                if *attempt != 1 || *epoch != Some(2) || *publication != Publication::NotPublished {
                    errors.push(
                        "direct closed copied fault has wrong attempt/epoch/publication".to_owned(),
                    );
                }
                if retained != ready {
                    errors.push("direct closed retained owner/keys/clock/frame changed".to_owned());
                }
                if SessionLock::acquire(&paths.world).is_ok() {
                    errors.push("direct closed retained native lock was released".to_owned());
                }
                if namespace
                    .acquire_recovery(&anchor, &paths.archive, &scratch.0.join("pollable-blocked"))
                    .is_ok()
                {
                    errors.push("direct closed retained UUID exclusion was released".to_owned());
                }
                if !paths.archive.join(".pending-00000000000000000002").is_dir() {
                    errors.push("direct closed copied evidence missing".to_owned());
                }
                if failure_polls == 1 {
                    drop(observer.take());
                    observer = Some(worker.control());
                    if control.request(Command::RetryStop {
                        attempt: 2,
                        failed_attempt: 99,
                    }) != Err(Refusal::State)
                    {
                        errors.push("direct closed mismatched retry was admitted".to_owned());
                    }
                }
                if failure_polls >= 3 {
                    match control.request(Command::RetryStop {
                        attempt: 2,
                        failed_attempt: 1,
                    }) {
                        Ok(()) => retry_sent = true,
                        Err(error) => {
                            errors.push(format!("direct closed explicit retry refused: {error:?}"))
                        }
                    }
                }
            }
            _ => {}
        }
        thread::sleep(Duration::from_millis(1));
    }
    let receipt = worker
        .take_completion()
        .or(premature)
        .expect("completed closed poll yields one classification")
        .unwrap();
    let consumed_once = worker.take_completion().is_none();
    for _ in 0..3 {
        let polled = worker.poll();
        if !polled.completed || polled.pipes.is_some() {
            errors.push("post-completion closed poll changed completion/offer state".to_owned());
        }
        if !matches!(&polled.view.state, State::Stopped { attempt: 2, receipt: published }
            if published.epoch == receipt.epoch && published.root == receipt.root)
        {
            errors.push("post-completion closed poll changed stopped receipt".to_owned());
        }
        if worker.take_completion().is_some() {
            errors.push("closed completion replayed after consumption".to_owned());
        }
    }
    assert!(errors.is_empty(), "{errors:?}");
    assert!(close_sent && retry_sent && stopped_before_poll && consumed_once);
    assert!(ready_polls >= 3 && failure_polls >= 3 && pending_polls >= 6);
    assert_eq!(ready, retained);
    let ready = ready.unwrap();
    assert_eq!(ready.guests, 0);
    assert!(ready.empty_slot);
    assert_eq!(receipt.epoch, 3);
    assert!(matches!(
        observer.unwrap().view().state,
        State::Stopped { attempt: 2, .. }
    ));
    assert!(paths.archive.join("epoch-00000000000000000001").is_dir());
    assert!(paths.archive.join(".pending-00000000000000000002").is_dir());
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(receipt.root.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["epoch"], 3);
    assert_eq!(manifest["saved_clock"], ready.clock);
    let saved_frame: serde_json::Value =
        serde_json::from_slice(&fs::read(receipt.root.join("host/counter.json")).unwrap()).unwrap();
    assert_eq!(saved_frame, serde_json::to_value(&ready.frame).unwrap());
    drop(SessionLock::acquire(&paths.world).unwrap());
    let lease = namespace
        .acquire_recovery(
            &anchor,
            &paths.archive,
            &scratch.0.join("pollable-after-join"),
        )
        .unwrap();
    assert_eq!(lease.world(), ready.world);
    drop(lease);
    eprintln!(
        "MANAGED_POLLABLE_RESULT {}",
        serde_json::json!({
            "kind":"closed-copied-retry","world":paths.world,"archive":paths.archive,
            "namespace":paths.namespace,"ready_polls":ready_polls,"failure_polls":failure_polls,
            "pending_polls":pending_polls,"early_take_after_stopped":stopped_before_poll,
            "completion_consumed_once":consumed_once,"post_completion_polls":3,
            "failed_attempt":1,"successful_attempt":2,"receipt_epoch":receipt.epoch,"worker_joined":true
        })
    );
}
