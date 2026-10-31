//! Real worker cases are run individually by the coordinator with isolated resources.
use super::*;
use crate::game::{
    integrated::transition_ownership_tests::Fixture,
    lan::connected_recovery::tests::{
        Scratch, Source, client_matches, disconnect, fresh, inventory_count, inventory_tree,
        request, rows_equal, source, source_core_matches,
    },
    levels::session_checkpoint::{CheckpointTestFault, RecoveryRequest},
};
use horizon_client::save::SessionLock;
use horizon_protocol::crypto::offline_uuid;

const WAIT: Duration = Duration::from_secs(120);

fn start(
    fixture: &Fixture,
    source: &Source,
    scratch: &Scratch,
    fault: Option<CheckpointTestFault>,
) -> ManagedConnectedStart {
    let template = fresh(fixture, ExecutionRole::IntegratedServer);
    ManagedConnectedStart {
        resources: template.res.clone(),
        pack: template.pack.clone(),
        options: template.options.clone(),
        settings: template.settings.clone(),
        // This is the explicit authority directory created by the reused fixture.
        namespace: source.root.parent().unwrap().join("authority"),
        request: request(source, scratch, "recovered"),
        continuation: scratch.dir("continuation"),
        attempt_timeout: WAIT,
        first_stop_fault: fault,
    }
}

fn record(errors: &mut Vec<String>, detail: impl Into<String>) {
    let detail = detail.into();
    if errors.len() < 16 && !errors.contains(&detail) {
        errors.push(detail);
    }
}

fn drive_clients(
    clients: &mut [Box<Game<'_>>; 2],
    pipes: Option<[Pipe; 2]>,
    offers: &mut usize,
    errors: &mut Vec<String>,
) {
    if let Some([owner, guest]) = pipes {
        *offers += 1;
        for ((client, pipe), name) in clients
            .iter_mut()
            .zip([owner, guest])
            .zip(["MemoryOwner", "MemoryGuest"])
        {
            if let Err(error) = client.connect_local(pipe, name) {
                record(errors, format!("client connection: {error}"));
            }
        }
    }
    if *offers != 0 {
        for client in clients {
            if let Err(error) = client.poll_stationary_client() {
                record(errors, format!("client poll: {error}"));
            }
        }
    }
}

fn clients_match(clients: &[Box<Game<'_>>; 2], source: &Source) -> bool {
    inventory_count(&clients[0]) == 3
        && inventory_count(&clients[1]) == 5
        && clients
            .iter()
            .zip(["MemoryOwner", "MemoryGuest"])
            .all(|(client, name)| {
                source
                    .rows
                    .iter()
                    .find(|row| row.profile_uuid == offline_uuid(name))
                    .is_some_and(|row| client_matches(client, row))
            })
}

fn check_owned(
    observation: &connected::Observation,
    source: &Source,
    destination: &Path,
    blocked: &Path,
    errors: &mut Vec<String>,
) {
    if !source_core_matches(observation, source) {
        record(errors, format!("native owner changed: {observation:?}"));
    }
    if SessionLock::acquire(destination).is_ok() {
        record(
            errors,
            "native destination lock released before worker completion",
        );
    }
    if source
        .namespace
        .acquire_recovery(&source.root, &source.archive, blocked)
        .is_ok()
    {
        record(errors, "UUID exclusion released before worker completion");
    }
}

fn validate_continuation(
    fixture: &Fixture,
    source: &Source,
    scratch: &Scratch,
    receipt: &CheckpointReceipt,
    continuation: &Path,
    expected_epoch: u64,
) {
    assert_eq!(receipt.epoch, expected_epoch);
    assert_eq!(
        receipt.root,
        fs::canonicalize(continuation)
            .unwrap()
            .join(format!("epoch-{expected_epoch:020}"))
    );
    // Invoke the genuine image verifier and native loader after the worker joined.
    // No transport/Guest is constructed in this verification candidate.
    let destination = source.root.parent().unwrap().join("verified-continuation");
    let lease = source
        .namespace
        .acquire_recovery(&source.root, continuation, &destination)
        .unwrap();
    let mut parts = Box::new(fresh(fixture, ExecutionRole::IntegratedServer))
        .prepare_session_checkpoint(RecoveryRequest {
            expected_world: lease.world(),
            expected_target: source.target.clone(),
            expected_type: source.type_id.clone(),
            archive_root: continuation.to_owned(),
            destination: destination.clone(),
            deadline: Instant::now() + WAIT,
        })
        .unwrap();
    assert!(rows_equal(parts.players().rows(), &source.rows));
    parts.validate_runtime_keys().unwrap();
    let game = parts.game_mut();
    assert_eq!(
        (game.world.game_time, game.world.day_time),
        (source.world_time, source.day_time)
    );
    assert_eq!(
        game.world
            .counter_checkpoint_frame(game.world.game_time)
            .unwrap(),
        source.counter
    );
    assert_eq!(
        game.world
            .slot_checkpoint_bytes(game.res.items(), game.world.game_time)
            .unwrap(),
        source.slot_bytes
    );
    drop(parts);
    drop(lease);
    drop(SessionLock::acquire(&destination).unwrap());
    // The Scratch remains alive through verification; all evidence stays isolated.
    let _ = scratch;
}

// Real observations feed an isolated admission/publication schedule. This owns
// neither a Game nor a worker and cannot affect the running fixture's controls.
fn prove_inflight_publication(
    observation: &connected::Observation,
    retry: bool,
    errors: &mut Vec<String>,
) {
    let initial = if retry {
        let connected::State::RetainedFailure {
            attempt,
            publication,
            detail,
        } = &observation.state
        else {
            record(errors, "retry race lacked actual retained observation");
            return;
        };
        State::RetainedFailure {
            attempt: *attempt,
            epoch: None,
            publication: *publication,
            detail: detail.clone(),
        }
    } else {
        if !matches!(observation.state, connected::State::ReadyFrozen) {
            record(errors, "ready race lacked actual ReadyFrozen observation");
            return;
        }
        State::Ready
    };
    let (sender, receiver) = mpsc::sync_channel(1);
    let (changed, _changes) = mpsc::sync_channel(1);
    let shared = Arc::new(Mutex::new(SupervisorView {
        state: initial,
        last_admitted: if retry { 1 } else { 0 },
        last_refusal: None,
        connected: None,
        observation: None,
        connected_observation: None,
    }));
    let control = ManagedStopControl {
        shared: shared.clone(),
        command: sender,
    };
    let attempt = if retry { 2 } else { 1 };
    let command = if retry {
        Command::RetryStop {
            attempt,
            failed_attempt: 1,
        }
    } else {
        Command::RequestStop { attempt }
    };
    if control.request(command).is_err() {
        record(errors, "isolated race initial command refused");
        return;
    }
    publish_connected(&shared, &changed, observation.clone());
    if !matches!(control.view().state, State::Stopping { attempt: current, .. } if current == attempt)
    {
        record(
            errors,
            "older observation reopened admission during an in-flight command",
        );
    }
    if control.request(Command::RequestStop {
        attempt: attempt + 1,
    }) != Err(Refusal::InFlight)
    {
        record(
            errors,
            "isolated stale-publication race admitted an extra command",
        );
    }
    let one_matching = match receiver.try_recv() {
        Ok(Command::RequestStop { attempt: 1 }) => !retry,
        Ok(Command::RetryStop {
            attempt: 2,
            failed_attempt: 1,
        }) => retry,
        _ => false,
    };
    if !one_matching || receiver.try_recv().is_ok() {
        record(
            errors,
            "isolated stale-publication race changed its one-slot command",
        );
    }
}
fn actual_close(fault: Option<CheckpointTestFault>) {
    let label = match fault {
        None => "positive",
        Some(CheckpointTestFault::Copied) => "copied",
        Some(CheckpointTestFault::Promoted) => "promoted",
    };
    let scratch = Scratch::new(label);
    let fixture = Fixture::new("managed-connected");
    let source = source(&fixture, &scratch);
    let before = inventory_tree(&source.archive);
    let configured = start(&fixture, &source, &scratch, fault);
    let destination = configured.request.destination.clone();
    let continuation = configured.continuation.clone();
    let blocked = source.root.parent().unwrap().join("blocked-copy");
    let released = source.root.parent().unwrap().join("after-worker-join");
    let caller = thread::current().id();
    let mut clients = [
        Box::new(fresh(&fixture, ExecutionRole::Client)),
        Box::new(fresh(&fixture, ExecutionRole::Client)),
    ];
    let mut errors = Vec::new();
    let mut offers = 0;
    let mut ready = 0;
    let mut failures = 0;
    let mut keys = None;
    let mut observer = None;
    let mut first_sent = false;
    let mut retry_sent = false;
    let mut ready_race = false;
    let mut retained_race = false;
    let began = Instant::now();
    let result = run_owned_managed_connected(configured, |view, control, pipes| {
        if began.elapsed() > Duration::from_secs(400) {
            panic!("managed connected fixture deadline with unresolved worker: {view:?}");
        }
        if thread::current().id() != caller {
            record(&mut errors, "driver left the synchronous caller thread");
        }
        let closing = matches!(view.state, State::Stopping { .. } | State::Stopped { .. })
            || view
                .connected
                .as_ref()
                .is_some_and(|status| status.phase == ConnectedPhase::Closing);
        if closing {
            if pipes.is_some() {
                record(&mut errors, "pipe offer repeated during close");
            }
        } else {
            drive_clients(&mut clients, pipes, &mut offers, &mut errors);
        }
        if let Some(status) = &view.connected
            && status.detail.len() > 256
        {
            record(&mut errors, "status diagnostic exceeds bound");
        }
        if let Some(observation) = view
            .connected_observation
            .as_ref()
            .filter(|_| matches!(view.state, State::Ready | State::RetainedFailure { .. }))
            && matches!(
                observation.state,
                connected::State::ReadyFrozen | connected::State::RetainedFailure { .. }
            )
        {
            check_owned(observation, &source, &destination, &blocked, &mut errors);
            let current = (
                observation.counter_key.clone(),
                observation.slot_key.clone(),
            );
            if keys.as_ref().is_some_and(|keys| keys != &current) {
                record(&mut errors, "runtime keys changed while retained");
            }
            keys.get_or_insert(current);
            if first_sent && !clients_match(&clients, &source) {
                record(
                    &mut errors,
                    "actual client terrain/teleport/inventory mismatch",
                );
            }
        }
        match &view.state {
            State::Ready if !first_sent && clients_match(&clients, &source) => {
                ready += 1;
                if ready >= 3 {
                    if !ready_race && let Some(observation) = &view.connected_observation {
                        prove_inflight_publication(observation, false, &mut errors);
                        ready_race = true;
                    }
                    observer = Some(control.clone());
                    if control.request(Command::RequestStop { attempt: 0 })
                        != Err(Refusal::StaleAttempt)
                    {
                        record(&mut errors, "zero attempt admitted");
                    }
                    match control.request(Command::RequestStop { attempt: 1 }) {
                        Ok(()) => first_sent = true,
                        Err(error) => record(&mut errors, format!("first stop refused: {error:?}")),
                    }
                    if first_sent {
                        if control.request(Command::RequestStop { attempt: 1 })
                            != Err(Refusal::StaleAttempt)
                        {
                            record(&mut errors, "repeated attempt admitted");
                        }
                        if !matches!(
                            control.request(Command::RequestStop { attempt: 2 }),
                            Err(Refusal::InFlight | Refusal::State)
                        ) {
                            record(&mut errors, "additional live request was not refused");
                        }
                    }
                }
            }
            State::RetainedFailure {
                attempt,
                publication,
                ..
            } if first_sent && !retry_sent => {
                failures += 1;
                if !retained_race && let Some(observation) = &view.connected_observation {
                    prove_inflight_publication(observation, true, &mut errors);
                    retained_race = true;
                }
                let expected = match fault {
                    Some(CheckpointTestFault::Copied) => Publication::NotPublished,
                    Some(CheckpointTestFault::Promoted) => Publication::Uncertain,
                    None => {
                        record(&mut errors, "positive close unexpectedly retained");
                        *publication
                    }
                };
                if *attempt != 1 || *publication != expected {
                    record(&mut errors, "wrong failed attempt/publication");
                }
                if failures == 1 {
                    drop(observer.take());
                    if !ready_race && let Some(observation) = &view.connected_observation {
                        prove_inflight_publication(observation, false, &mut errors);
                        ready_race = true;
                    }
                    observer = Some(control.clone());
                    if control.request(Command::RequestStop { attempt: 1 })
                        != Err(Refusal::StaleAttempt)
                    {
                        record(&mut errors, "failed attempt replay admitted");
                    }
                    if control.request(Command::RequestStop { attempt: 2 }) != Err(Refusal::State) {
                        record(
                            &mut errors,
                            "fresh initial close admitted from retained failure",
                        );
                    }
                    if control.request(Command::RetryStop {
                        attempt: 2,
                        failed_attempt: 99,
                    }) != Err(Refusal::State)
                    {
                        record(&mut errors, "mismatched retry admitted");
                    }
                }
                if failures >= 3 {
                    if !matches!(
                        observer.as_ref().unwrap().view().state,
                        State::RetainedFailure { attempt: 1, .. }
                    ) {
                        record(&mut errors, "replacement observer lost retained status");
                    }
                    match control.request(Command::RetryStop {
                        attempt: 2,
                        failed_attempt: 1,
                    }) {
                        Ok(()) => retry_sent = true,
                        Err(error) => {
                            record(&mut errors, format!("explicit retry refused: {error:?}"))
                        }
                    }
                }
            }
            State::Connected(ConnectedPhase::CleanupBlocked) => {
                panic!("unexpected unresolved retained owner in finite fixture: {view:?}");
            }
            _ => {}
        }
        DriverCommand::Wait
    });
    for client in &mut clients {
        disconnect(client);
    }
    // Resolve the planned stop/retry and join before making normal-case assertions.
    let receipt = result.unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(offers, 1);
    assert!(first_sent && ready >= 3 && keys.is_some());
    assert_eq!(retry_sent, fault.is_some());
    assert!(ready_race);
    assert_eq!(retained_race, fault.is_some());
    assert_eq!(inventory_tree(&source.archive), before);
    assert!(
        matches!(observer.unwrap().view().state, State::Stopped { attempt, .. } if attempt == if retry_sent { 2 } else { 1 })
    );
    drop(SessionLock::acquire(&destination).unwrap());
    drop(
        source
            .namespace
            .acquire_recovery(&source.root, &source.archive, &released)
            .unwrap(),
    );
    if let Some(fault) = fault {
        assert!(failures >= 3);
        let first = match fault {
            CheckpointTestFault::Copied => ".pending-00000000000000000001",
            CheckpointTestFault::Promoted => "epoch-00000000000000000001",
        };
        assert!(continuation.join(first).is_dir());
    }
    validate_continuation(
        &fixture,
        &source,
        &scratch,
        &receipt,
        &continuation,
        if retry_sent { 2 } else { 1 },
    );
    eprintln!(
        "MANAGED_CONNECTED_RESULT {}",
        serde_json::json!({
            "kind":label,"source":source.root,"archive":source.archive,"destination":destination,
            "continuation":continuation,"source_rounds":source.rounds,"offers":offers,
            "ready_observations":ready,"failure_observations":failures,"player_counts":[3,5],
            "recovery_ticks":0,"failed_attempt":if retry_sent {Some(1)} else {None},
            "successful_attempt":if retry_sent {2} else {1},"receipt_epoch":receipt.epoch,
            "continuation_native_verified":true,"worker_joined":true
        })
    );
}

#[test]
#[ignore = "isolated resources/prebuilt pair: real connected worker and two actual clients"]
fn managed_connected_actual_two_clients_close_and_join() {
    actual_close(None);
}

#[test]
#[ignore = "isolated resources/prebuilt pair: copied fault runs on worker, explicit retry joins"]
fn managed_connected_actual_copied_retains_and_explicit_retry_joins() {
    actual_close(Some(CheckpointTestFault::Copied));
}

#[test]
#[ignore = "isolated resources/prebuilt pair: promoted fault runs on worker, explicit retry joins"]
fn managed_connected_actual_promoted_retains_and_explicit_retry_joins() {
    actual_close(Some(CheckpointTestFault::Promoted));
}

#[test]
#[ignore = "isolated resources/prebuilt pair: pre-Ready disconnect finishes all staged attempts"]
fn managed_connected_actual_pre_ready_refusal_joins_and_releases() {
    let scratch = Scratch::new("managed-pre-ready");
    let fixture = Fixture::new("managed-pre-ready");
    let source = source(&fixture, &scratch);
    let before = inventory_tree(&source.archive);
    let configured = start(&fixture, &source, &scratch, None);
    let destination = configured.request.destination.clone();
    let continuation = configured.continuation.clone();
    let mut offers = 0;
    let mut was_ready = false;
    let mut observer = None;
    let mut errors = Vec::new();
    let began = Instant::now();
    let result = run_owned_managed_connected(configured, |view, control, pipes| {
        if began.elapsed() > Duration::from_secs(300) {
            panic!("pre-ready refusal did not resolve: {view:?}");
        }
        observer = Some(control.clone());
        if let Some([first, second]) = pipes {
            offers += 1;
            first.shutdown();
            drop((first, second));
        }
        was_ready |= matches!(view.state, State::Ready);
        if let Some(observation) = view
            .connected_observation
            .as_ref()
            .filter(|_| matches!(view.state, State::Ready | State::RetainedFailure { .. }))
        {
            was_ready |= matches!(observation.state, connected::State::ReadyFrozen);
        }
        if matches!(view.state, State::Connected(ConnectedPhase::CleanupBlocked)) {
            panic!("pre-ready cleanup retained instead of joining: {view:?}");
        }
        if control.request(Command::RequestStop { attempt: 1 }) != Err(Refusal::State) {
            record(&mut errors, "pre-ready initial close accepted");
        }
        DriverCommand::Wait
    });
    let reason = result.unwrap_err();
    assert!(reason.starts_with("connected startup refused:"), "{reason}");
    assert!(matches!(
        observer.unwrap().view().state,
        State::StartupRefused(_)
    ));
    assert_eq!(offers, 1);
    assert!(!was_ready);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(inventory_tree(&source.archive), before);
    assert_eq!(fs::read_dir(&continuation).unwrap().count(), 0);
    drop(SessionLock::acquire(&destination).unwrap());
    drop(
        source
            .namespace
            .acquire_recovery(
                &source.root,
                &source.archive,
                &source.root.parent().unwrap().join("released"),
            )
            .unwrap(),
    );
    eprintln!(
        "MANAGED_CONNECTED_RESULT {}",
        serde_json::json!({
            "kind":"pre-ready-refusal","reason":reason,"source":source.root,
            "archive":source.archive,"destination":destination,"continuation":continuation,
            "offers":offers,"ready":false,"worker_joined":true,"native_cleanup_completed":true
        })
    );
}

#[test]
#[ignore = "DEDICATED EXPECTED-FATAL CHILD: supervisor panic with actual connected ReadyFrozen worker"]
fn managed_connected_actual_ready_supervisor_unwind_is_fatal_child() {
    assert_eq!(
        std::env::var("HORIZON_MANAGED_CONNECTED_FATAL_CHILD").as_deref(),
        Ok("1")
    );
    let scratch = Scratch::new("managed-connected-fatal");
    let fixture = Fixture::new("managed-connected-fatal");
    let source = source(&fixture, &scratch);
    let configured = start(&fixture, &source, &scratch, None);
    let destination = configured.request.destination.clone();
    let continuation = configured.continuation.clone();
    let mut clients = [
        Box::new(fresh(&fixture, ExecutionRole::Client)),
        Box::new(fresh(&fixture, ExecutionRole::Client)),
    ];
    let mut offers = 0;
    let mut errors = Vec::new();
    let began = Instant::now();
    let _ = run_owned_managed_connected(configured, |view, _, pipes| {
        if began.elapsed() > Duration::from_secs(300) {
            panic!("fatal child missed ready deadline");
        }
        drive_clients(&mut clients, pipes, &mut offers, &mut errors);
        if matches!(view.state, State::Ready) && clients_match(&clients, &source) {
            let observation = view
                .connected_observation
                .as_ref()
                .expect("actual connected observation");
            assert!(matches!(observation.state, connected::State::ReadyFrozen));
            assert!(source_core_matches(observation, &source));
            assert!(clients_match(&clients, &source));
            assert_eq!(offers, 1);
            assert!(errors.is_empty(), "{errors:?}");
            assert!(SessionLock::acquire(&destination).is_err());
            eprintln!(
                "MANAGED_CONNECTED_EXCEPTION {}",
                serde_json::json!({
                    "kind":"fatal-supervisor-unwind","source":source.root,"archive":source.archive,
                    "destination":destination,"continuation":continuation,"offers":offers,
                    "world_time":observation.world_time,"players":observation.guests,
                    "play_entries":observation.play_entries,"core_unchanged":true
                })
            );
            panic!("intentional connected supervisor ReadyFrozen unwind");
        }
        DriverCommand::Wait
    });
    panic!("fatal connected supervisor child unexpectedly returned");
}

#[test]
#[ignore = "DEDICATED RETAINED CHILD: external termination after observed connected late departure"]
fn managed_connected_actual_late_departure_retains_until_external_termination_child() {
    assert_eq!(
        std::env::var("HORIZON_MANAGED_CONNECTED_DEPARTURE_CHILD").as_deref(),
        Ok("1")
    );
    let scratch = Scratch::new("managed-connected-departure");
    let fixture = Fixture::new("managed-connected-departure");
    let source = source(&fixture, &scratch);
    let configured = start(&fixture, &source, &scratch, None);
    let destination = configured.request.destination.clone();
    let continuation = configured.continuation.clone();
    let blocked = source.root.parent().unwrap().join("blocked-departure-copy");
    let mut clients = [
        Box::new(fresh(&fixture, ExecutionRole::Client)),
        Box::new(fresh(&fixture, ExecutionRole::Client)),
    ];
    let mut offers = 0;
    let mut errors = Vec::new();
    let mut departed = false;
    let mut ready_keys = None;
    let mut retained = 0;
    let began = Instant::now();
    let _ = run_owned_managed_connected(configured, |view, control, pipes| {
        if began.elapsed() > Duration::from_secs(400) {
            panic!("retained departure child deadline");
        }
        if !departed {
            drive_clients(&mut clients, pipes, &mut offers, &mut errors);
            if matches!(view.state, State::Ready) && clients_match(&clients, &source) {
                let observation = view
                    .connected_observation
                    .as_ref()
                    .expect("actual connected observation");
                assert!(matches!(observation.state, connected::State::ReadyFrozen));
                assert!(source_core_matches(observation, &source));
                assert!(clients_match(&clients, &source));
                assert!(errors.is_empty(), "{errors:?}");
                assert_eq!(offers, 1);
                ready_keys = Some((
                    observation.counter_key.clone(),
                    observation.slot_key.clone(),
                ));
                disconnect(&mut clients[1]);
                departed = true;
            }
        } else {
            assert!(pipes.is_none(), "pipe offer replayed after departure");
        }
        if departed && matches!(view.state, State::Connected(ConnectedPhase::CleanupBlocked)) {
            let observation = view
                .connected_observation
                .as_ref()
                .expect("retained connected observation");
            assert!(matches!(
                observation.state,
                connected::State::CleanupBlocked(_)
            ));
            assert!(
                source_core_matches(observation, &source),
                "retained native owner changed: {observation:?}"
            );
            assert_eq!(
                Some((
                    observation.counter_key.clone(),
                    observation.slot_key.clone()
                )),
                ready_keys
            );
            assert!(SessionLock::acquire(&destination).is_err());
            assert!(
                source
                    .namespace
                    .acquire_recovery(&source.root, &source.archive, &blocked)
                    .is_err()
            );
            assert_eq!(fs::read_dir(&continuation).unwrap().count(), 0);
            assert_eq!(
                control.request(Command::RequestStop { attempt: 1 }),
                Err(Refusal::State)
            );
            assert_eq!(
                control.request(Command::RetryStop {
                    attempt: 2,
                    failed_attempt: 1
                }),
                Err(Refusal::State)
            );
            retained += 1;
            if retained == 10 {
                eprintln!(
                    "MANAGED_CONNECTED_EXCEPTION {}",
                    serde_json::json!({
                        "kind":"late-departure-retained","source":source.root,"archive":source.archive,
                        "destination":destination,"continuation":continuation,"offers":offers,
                        "world_time":observation.world_time,"source_world_time":source.world_time,
                        "retained_observations":retained,"retained_players":observation.guests,
                        "retained_play_entries":observation.play_entries,
                        "rows_equal":observation.players.as_ref().is_ok_and(|rows| rows_equal(rows, &source.rows)),
                        "core_unchanged":true,"close_refused":true,"worker_unjoined":true
                    })
                );
            }
        }
        DriverCommand::Wait
    });
    panic!("retained departure child unexpectedly returned");
}

#[test]
#[ignore = "isolated resources/prebuilt pair: direct pollable owner, actual two clients, explicit completion"]
fn managed_connected_actual_pollable_owner_moves_one_offer_and_completes_once() {
    let scratch = Scratch::new("pollable-connected");
    let fixture = Fixture::new("pollable-connected");
    let source = source(&fixture, &scratch);
    let before = inventory_tree(&source.archive);
    let configured = start(&fixture, &source, &scratch, None);
    let destination = configured.request.destination.clone();
    let continuation = configured.continuation.clone();
    let blocked = source.root.parent().unwrap().join("pollable-blocked-copy");
    let released = source.root.parent().unwrap().join("pollable-after-join");
    let mut clients = [
        Box::new(fresh(&fixture, ExecutionRole::Client)),
        Box::new(fresh(&fixture, ExecutionRole::Client)),
    ];
    let mut worker = ManagedWorker::start_connected(configured).unwrap();
    let control = worker.control();
    let mut observer = Some(worker.control());
    let mut errors = Vec::new();
    let mut premature = worker.take_completion();
    if premature.is_some() {
        record(
            &mut errors,
            "completion available before the first explicit poll",
        );
    }
    let began = Instant::now();
    let mut offers = 0;
    let mut ready = 0;
    let mut pending_polls = 0;
    let mut keys = None;
    let mut close_sent = false;
    let mut retry_sent = false;
    let mut stopped_before_poll = false;
    loop {
        if began.elapsed() > Duration::from_secs(400) {
            panic!(
                "pollable connected owner did not resolve: {:?}",
                control.view()
            );
        }
        // Once close is admitted, deliberately observe terminal shared status
        // without polling the owner. Status cannot itself classify/join it.
        let snapshot = control.view();
        if close_sent && matches!(snapshot.state, State::Stopping { .. }) {
            if let Some(early) = worker.take_completion() {
                record(&mut errors, "in-flight status made completion consumable");
                premature.get_or_insert(early);
            }
            thread::sleep(Duration::from_millis(1));
            continue;
        }
        if close_sent && matches!(snapshot.state, State::Stopped { .. }) {
            stopped_before_poll = true;
            if let Some(early) = worker.take_completion() {
                record(
                    &mut errors,
                    "Stopped status classified completion without an explicit poll",
                );
                premature.get_or_insert(early);
            }
        }
        let polled = worker.poll();
        if polled.completed {
            if polled.pipes.is_some() {
                record(&mut errors, "completed owner delivered an endpoint offer");
            }
            break;
        }
        pending_polls += 1;
        if let Some(early) = worker.take_completion() {
            record(&mut errors, "unfinished poll made completion consumable");
            premature.get_or_insert(early);
        }
        let view = polled.view;
        let closing = matches!(view.state, State::Stopping { .. } | State::Stopped { .. })
            || view
                .connected
                .as_ref()
                .is_some_and(|status| status.phase == ConnectedPhase::Closing);
        if closing {
            if polled.pipes.is_some() {
                record(&mut errors, "endpoint offer repeated while closing");
            }
        } else {
            drive_clients(&mut clients, polled.pipes, &mut offers, &mut errors);
        }
        if matches!(view.state, State::Ready)
            && let Some(observation) = &view.connected_observation
        {
            check_owned(observation, &source, &destination, &blocked, &mut errors);
            let current = (
                observation.counter_key.clone(),
                observation.slot_key.clone(),
            );
            if keys.as_ref().is_some_and(|keys| keys != &current) {
                record(&mut errors, "pollable Ready changed native runtime keys");
            }
            keys.get_or_insert(current);
            if clients_match(&clients, &source) && !close_sent {
                ready += 1;
                if ready >= 3 {
                    drop(observer.take());
                    observer = Some(worker.control());
                    match control.request(Command::RequestStop { attempt: 1 }) {
                        Ok(()) => close_sent = true,
                        Err(error) => record(
                            &mut errors,
                            format!("pollable first close refused: {error:?}"),
                        ),
                    }
                }
            }
        }
        // An unexpected publication failure is still resolved through the
        // existing explicit retry before positive-case discrepancies assert.
        if let State::RetainedFailure { attempt, .. } = view.state
            && !retry_sent
        {
            record(
                &mut errors,
                "fault-free pollable close unexpectedly retained",
            );
            match control.request(Command::RetryStop {
                attempt: 2,
                failed_attempt: attempt,
            }) {
                Ok(()) => retry_sent = true,
                Err(error) => record(&mut errors, format!("cleanup retry refused: {error:?}")),
            }
        }
        if matches!(view.state, State::Connected(ConnectedPhase::CleanupBlocked)) {
            panic!("pollable positive unexpectedly retains unresolved native owner: {view:?}");
        }
        thread::sleep(Duration::from_millis(1));
    }
    let completion = worker.take_completion().or(premature);
    for client in &mut clients {
        disconnect(client);
    }
    let receipt = completion
        .expect("explicit completed poll yields one classification")
        .unwrap();
    let consumed_once = worker.take_completion().is_none();
    for _ in 0..3 {
        let polled = worker.poll();
        if !polled.completed || polled.pipes.is_some() {
            record(
                &mut errors,
                "post-completion poll changed completion/offer state",
            );
        }
        if !matches!(&polled.view.state, State::Stopped { attempt: 1, receipt: published }
            if published.epoch == receipt.epoch && published.root == receipt.root)
        {
            record(
                &mut errors,
                "post-completion poll changed the matching stopped receipt",
            );
        }
        if worker.take_completion().is_some() {
            record(&mut errors, "completion replayed after consumption");
        }
    }
    // All ordinary assertions occur after explicit owner completion and join.
    assert!(errors.is_empty(), "{errors:?}");
    assert!(consumed_once && close_sent && stopped_before_poll && !retry_sent);
    assert!(pending_polls >= 3 && ready >= 3 && keys.is_some());
    assert_eq!(offers, 1);
    assert_eq!(inventory_tree(&source.archive), before);
    assert!(matches!(
        observer.unwrap().view().state,
        State::Stopped { attempt: 1, .. }
    ));
    drop(SessionLock::acquire(&destination).unwrap());
    drop(
        source
            .namespace
            .acquire_recovery(&source.root, &source.archive, &released)
            .unwrap(),
    );
    validate_continuation(&fixture, &source, &scratch, &receipt, &continuation, 1);
    eprintln!(
        "MANAGED_POLLABLE_RESULT {}",
        serde_json::json!({
            "kind":"connected-positive","source":source.root,"archive":source.archive,
            "destination":destination,"continuation":continuation,"offers":offers,
            "ready_observations":ready,"pending_polls":pending_polls,"player_counts":[3,5],
            "recovery_ticks":0,"early_take_after_stopped":stopped_before_poll,
            "completion_consumed_once":consumed_once,"post_completion_polls":3,
            "receipt_epoch":receipt.epoch,"continuation_native_verified":true,"worker_joined":true
        })
    );
}
