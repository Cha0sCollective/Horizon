//! Genuine materialized persistent Guest Game families. The external coordinator
//! supplies the individually pinned normal/trap/reject/invalid/fuel pair per case.
//! No environment mutation, compiler or nested test process runs here.
use super::*;
use horizon_client::{
    counter_trial::{Backend, Completion, Fault, GuestFailure, Proposal},
    world::counter_restore::{
        GuestCounterFrameV3, GuestFaultCode, GuestRuntimeState, GuestStatus,
        PersistentGuestPhaseToken,
    },
};
use horizon_mods::{CounterRequest, Mods, Runs, Status};

fn guest_session<'a>(f: &'a Materialized, label: &str, owner: bool) -> Session<'a> {
    let root = f.scratch.join(format!("guest-world-{label}-{owner}"));
    let mut game = f.fresh(owner);
    open_native(&mut game, &root);
    residency(&mut game, 2, (0, 0));
    let pair = game
        .options
        .mod_packages
        .catalog()
        .unwrap()
        .prepared()
        .values()
        .next()
        .unwrap()
        .clone();
    let binding =
        NativeBinding::from_registered_guest_prepared(&pair, &f.type_id, game.pack.assets.registry)
            .unwrap();
    game.world
        .set(POSITION[0], POSITION[1], POSITION[2], binding.state_id())
        .unwrap();
    let (counter, slot) = game
        .bootstrap_registered_guest_counter(&f.type_id, POSITION)
        .unwrap();
    let (mut host, receivers) = make_host(&mut game, owner, [10, 20], true);
    host.hub.guests[0].inventory[0] = Some(session_items_rich());
    host.hub.guests[0].inventory[1] = Some(session_items_rich());
    host.hub.guests[1].inventory[0] = Some(ItemStack::new("minecraft:stone", 4));
    game.lan = Some(host);
    transfer(&mut game, &slot, 10, Direction::IntoMachine, 7);
    // A SECOND original stack reaches a real native dropped owner through Player.drop.
    let mut host = game.lan.take().unwrap();
    game.as_guest(&mut host, 10, |g| {
        g.selected = 1;
        g.drop_held(true);
    })
    .unwrap();
    game.lan = Some(host);
    assert_eq!(game.dropped.len(), 1);
    assert_eq!(game.dropped[0].stack, session_items_rich());
    game.tickets.update(Some(Sources {
        players: vec![(0, 0)],
        forced: Vec::new(),
        view_distance: 2,
        simulation_distance: 2,
    }));
    let archive = f.scratch.join(format!("guest-archive-{label}-{owner}"));
    fs::create_dir(&archive).unwrap();
    let target = CounterTarget::new(Dim::Overworld.id(), POSITION).unwrap();
    Session {
        game,
        counter,
        slot,
        archive,
        type_id: f.type_id.clone(),
        target,
        receivers,
    }
}
fn frame(game: &Game<'_>) -> GuestCounterFrameV3 {
    game.world.guest_storage_view().unwrap().counter().clone()
}
fn slot_bytes(game: &Game<'_>) -> Vec<u8> {
    game.world
        .guest_slot_checkpoint_bytes(game.res.items(), game.world.game_time)
        .unwrap()
}
fn advance_three(s: &mut Session<'_>) {
    for expected in 1..=3 {
        s.game.world.game_time += 1;
        s.game.tick_counter_trial(true);
        let record = s.game.world.read_persistent_counter(&s.counter).unwrap();
        assert_eq!(
            (record.counter(), record.last_advanced()),
            (expected, Some(s.game.world.game_time))
        );
        let before = record;
        s.game.tick_counter_trial(true);
        assert_eq!(
            s.game.world.read_persistent_counter(&s.counter).unwrap(),
            before
        );
    }
}
fn recover_guest<'a>(
    f: &'a Materialized,
    s: &Session<'_>,
    label: &str,
    owner: bool,
) -> (Game<'a>, GuestRecoveryReceipt, Vec<mpsc::Receiver<Out>>) {
    let mut candidate = f.fresh(owner);
    let (host, receivers) = make_host(&mut candidate, owner, [10, 20], false);
    let (game, receipt) = candidate
        .recover_registered_guest_session_checkpoint(request(s, f.scratch.join(label)), host)
        .unwrap();
    (game, receipt, receivers)
}
fn evaluate(game: &Game<'_>, token: &PersistentGuestPhaseToken) -> Proposal {
    let phase = token.phase();
    let configuration = phase.configuration();
    let Backend::Guest {
        primary_owner,
        package_identity,
    } = configuration.backend()
    else {
        panic!("actual Guest configuration required");
    };
    let [x, y, z] = configuration.position();
    let input = CounterRequest {
        world_instance: configuration.scope().world_instance(),
        dimension: configuration.scope().dimension().into(),
        type_id: configuration.machine_type().id().into(),
        schema: configuration.machine_type().save_schema(),
        x,
        y,
        z,
        instance: phase.record().instance(),
        counter: phase.record().counter(),
        game_time: phase.game_time(),
    };
    let p = game
        .mods
        .mods
        .counter_evaluator(primary_owner, package_identity)
        .unwrap()
        .evaluate(&input)
        .unwrap();
    Proposal {
        world_instance: p.world_instance,
        instance: p.instance,
        game_time: p.game_time,
        expected_counter: p.expected_counter,
        next_counter: p.next_counter,
    }
}

#[test]
#[ignore = "isolated normal captured pair and materialized resources: actual Guest commit and healthy native roundtrip both roles"]
fn persistent_guest_actual_normal_commit_and_native_roundtrip_both_roles() {
    let f = Materialized::new("guest-normal");
    for owner in [true, false] {
        let mut s = guest_session(&f, "normal", owner);
        advance_three(&mut s);
        let current = s.game.world.read_persistent_counter(&s.counter).unwrap();
        s.game.world.game_time += 1;
        let paused_calls = crate::game::counter_control::guest_evaluation_calls();
        s.game.tick_counter_trial(false);
        assert_eq!(
            crate::game::counter_control::guest_evaluation_calls(),
            paused_calls
        );
        assert_eq!(
            s.game.world.read_persistent_counter(&s.counter).unwrap(),
            current
        );
        s.game.tickets.update(Some(Sources {
            players: Vec::new(),
            forced: Vec::new(),
            view_distance: 2,
            simulation_distance: 2,
        }));
        s.game.world.game_time += 1;
        s.game.tick_counter_trial(true);
        assert_eq!(
            s.game.world.read_persistent_counter(&s.counter).unwrap(),
            current
        );
        s.game.tickets.update(Some(Sources {
            players: vec![(0, 0)],
            forced: Vec::new(),
            view_distance: 2,
            simulation_distance: 2,
        }));
        residency(&mut s.game, 0, (80, 80));
        assert!(!s.game.world.chunks.contains_key(&(0, 0)));
        let unloaded_calls = crate::game::counter_control::guest_evaluation_calls();
        s.game.world.game_time += 5;
        s.game.tick_counter_trial(true);
        assert_eq!(
            crate::game::counter_control::guest_evaluation_calls(),
            unloaded_calls
        );
        let unloaded = s
            .game
            .world
            .counter_trial_snapshot()
            .unwrap()
            .record()
            .unwrap();
        assert_eq!(
            (unloaded.counter(), unloaded.last_advanced()),
            (current.counter(), current.last_advanced())
        );
        residency(&mut s.game, 2, (0, 0));
        s.game.tick_counter_trial(true);
        let reloaded = s.game.world.read_persistent_counter(&s.counter).unwrap();
        assert_eq!(
            (reloaded.counter(), reloaded.last_advanced()),
            (4, Some(s.game.world.game_time))
        );
        assert_eq!(
            crate::game::counter_control::guest_evaluation_calls(),
            unloaded_calls + 1
        );
        s.game.tick_counter_trial(true);
        assert_eq!(
            crate::game::counter_control::guest_evaluation_calls(),
            unloaded_calls + 1
        );
        let before = frame(&s.game);
        let slot = slot_bytes(&s.game);
        s.game
            .checkpoint_registered_guest_session(&s.archive, 1, until())
            .unwrap();
        assert!(
            s.game
                .checkpoint_registered_session(&s.archive, 2, until())
                .is_err()
        );
        let items = s.game.project_session_checkpoint_items().unwrap();
        let (mut game, receipt, _receivers) =
            recover_guest(&f, &s, &format!("normal-recovery-{owner}"), owner);
        let GuestRuntimeState::ReadyActive { counter, slot: key } = receipt.state else {
            panic!("healthy Guest keys required");
        };
        assert_ne!(counter, s.counter);
        assert_ne!(key, s.slot);
        assert_eq!(frame(&game), before);
        assert_eq!(slot_bytes(&game), slot);
        assert_eq!(game.project_session_checkpoint_items().unwrap(), items);
        assert!(game.world.read_persistent_counter(&s.counter).is_err());
        game.checkpoint_registered_guest_session(&s.archive, 2, until())
            .unwrap();
        game.world.game_time += 1;
        game.tick_counter_trial(true);
        assert_eq!(
            game.world
                .read_persistent_counter(&counter)
                .unwrap()
                .counter(),
            5
        );
    }
}
fn fault_family(label: &str) {
    let f = Materialized::new(label);
    for owner in [true, false] {
        let mut s = guest_session(&f, label, owner);
        assert_eq!(
            s.game.prepare_session_checkpoint_item_identities().unwrap(),
            1
        );
        advance_three(&mut s);
        let record = s.game.world.read_persistent_counter(&s.counter).unwrap();
        let slot_facts = original_slot_facts(&s.game);
        let drop = s.game.dropped[0].stack.clone();
        s.game.world.game_time += 1;
        let failing_calls = crate::game::counter_control::guest_evaluation_calls();
        s.game.tick_counter_trial(true); // actual selected Wasm artifact fails at input3
        assert_eq!(
            crate::game::counter_control::guest_evaluation_calls(),
            failing_calls + 1
        );
        let original = frame(&s.game);
        assert_eq!(
            original.execution.status,
            GuestStatus::Faulted {
                code: GuestFaultCode::GuestEvaluation,
                observed_tick: s.game.world.game_time
            }
        );
        assert!(s.game.world.read_persistent_counter(&s.counter).is_err());
        assert_eq!(
            s.game.world.counter_trial_snapshot().unwrap().fault(),
            Some(Fault::GuestFailure(GuestFailure::Evaluation))
        );
        assert_eq!(
            s.game.world.counter_trial_snapshot().unwrap().record(),
            Some(record)
        );
        assert_eq!(original_slot_facts(&s.game), slot_facts);
        assert_eq!(s.game.dropped[0].stack, drop);
        assert!(s.game.world.read_world_slot(&s.slot).is_err());
        let mut host = s.game.lan.take().unwrap();
        let player_before = host.hub.guests[0].inventory.clone();
        let transfer = s
            .game
            .as_guest(&mut host, 10, |game| {
                game.transfer_world_slot_inventory_cell(&s.slot, 0, Direction::IntoPlayer, 1)
            })
            .unwrap();
        assert!(transfer.is_err());
        assert_eq!(host.hub.guests[0].inventory, player_before);
        s.game.lan = Some(host);
        assert_eq!(original_slot_facts(&s.game), slot_facts);
        let latched_calls = crate::game::counter_control::guest_evaluation_calls();
        for _ in 0..3 {
            s.game.world.game_time += 1;
            s.game.tick_counter_trial(true);
        }
        assert_eq!(
            crate::game::counter_control::guest_evaluation_calls(),
            latched_calls
        );
        assert_eq!(frame(&s.game).execution.status, original.execution.status);
        assert_eq!(
            s.game.world.counter_trial_snapshot().unwrap().record(),
            Some(record)
        );
        let owner_items = s.game.project_session_checkpoint_items().unwrap();
        let owner_slot = original_slot_facts(&s.game);
        let owner_status = frame(&s.game);
        let copied = s.game.checkpoint_session_profile_steps(
            NativeBindingProfile::RegisteredCube,
            true,
            &s.archive,
            1,
            until(),
            |step| {
                if step == Step::Copied {
                    Err(fail("actual faulted Guest post-copy refusal"))
                } else {
                    Ok(())
                }
            },
        );
        assert!(copied.is_err());
        assert!(!published(&s, 1).exists());
        assert_eq!(frame(&s.game), owner_status);
        assert_eq!(original_slot_facts(&s.game), owner_slot);
        assert_eq!(
            s.game.project_session_checkpoint_items().unwrap(),
            owner_items
        );
        s.game
            .checkpoint_registered_guest_session(&s.archive, 2, until())
            .unwrap();
        let before = frame(&s.game);
        let bytes = slot_bytes(&s.game);
        let items = s.game.project_session_checkpoint_items().unwrap();
        let (mut recovered, receipt, _receivers) =
            recover_guest(&f, &s, &format!("{label}-recovery-{owner}"), owner);
        let GuestRuntimeState::FaultedActive { receipt: storage } = receipt.state else {
            panic!("stopped storage receipt required");
        };
        recovered
            .world
            .validate_guest_storage_receipt(&storage)
            .unwrap();
        assert_eq!(frame(&recovered), before);
        assert_eq!(slot_bytes(&recovered), bytes);
        assert_eq!(recovered.project_session_checkpoint_items().unwrap(), items);
        assert!(recovered.world.persistent_counter_key().is_err());
        assert!(
            recovered
                .world
                .prepare_persistent_guest_counter_phase(true, true)
                .is_err()
        );
        let stopped_calls = crate::game::counter_control::guest_evaluation_calls();
        let stopped_record = recovered.world.counter_trial_snapshot().unwrap().record();
        for _ in 0..3 {
            recovered.world.game_time += 1;
            assert!(
                recovered
                    .world
                    .prepare_persistent_guest_counter_phase(true, true)
                    .is_err()
            );
            recovered.tick_counter_trial(true);
        }
        assert_eq!(
            crate::game::counter_control::guest_evaluation_calls(),
            stopped_calls
        );
        assert_eq!(
            recovered.world.counter_trial_snapshot().unwrap().record(),
            stopped_record
        );
        assert_eq!(frame(&recovered).execution.status, before.execution.status);
        // Faulted storage exposes no transfer key. The stale original key refuses
        // during an actual player-cell lend without moving either original owner.
        let slot_original = original_slot_facts(&recovered);
        let mut host = recovered.lan.take().unwrap();
        let player_before = host.hub.guests[0].inventory.clone();
        let refused = recovered
            .as_guest(&mut host, 10, |game| {
                game.transfer_world_slot_inventory_cell(&s.slot, 0, Direction::IntoPlayer, 1)
            })
            .unwrap();
        assert!(refused.is_err());
        assert_eq!(host.hub.guests[0].inventory, player_before);
        recovered.lan = Some(host);
        assert_eq!(original_slot_facts(&recovered), slot_original);
        recovered
            .checkpoint_registered_guest_session(&s.archive, 3, until())
            .unwrap();
        let twice_before = frame(&recovered);
        let twice_bytes = slot_bytes(&recovered);
        let twice_items = recovered.project_session_checkpoint_items().unwrap();
        let (mut twice, twice_receipt, _twice_receivers) =
            recover_guest(&f, &s, &format!("{label}-second-recovery-{owner}"), owner);
        let GuestRuntimeState::FaultedActive {
            receipt: twice_storage,
        } = twice_receipt.state
        else {
            panic!("second stopped receipt required");
        };
        twice
            .world
            .validate_guest_storage_receipt(&twice_storage)
            .unwrap();
        assert_eq!(frame(&twice), twice_before);
        assert_eq!(slot_bytes(&twice), twice_bytes);
        assert_eq!(
            twice.project_session_checkpoint_items().unwrap(),
            twice_items
        );
        let twice_record = twice
            .world
            .counter_trial_snapshot()
            .unwrap()
            .record()
            .unwrap();
        let first_record = stopped_record.unwrap();
        assert_ne!(twice_record.instance(), first_record.instance());
        assert_eq!(
            (
                twice_record.counter(),
                twice_record.last_advanced(),
                twice_record.resident()
            ),
            (
                first_record.counter(),
                first_record.last_advanced(),
                first_record.resident()
            )
        );
        let calls = crate::game::counter_control::guest_evaluation_calls();
        for _ in 0..3 {
            twice.world.game_time += 1;
            twice.tick_counter_trial(true);
        }
        assert_eq!(
            crate::game::counter_control::guest_evaluation_calls(),
            calls
        );
        assert_eq!(
            twice.world.counter_trial_snapshot().unwrap().record(),
            Some(twice_record)
        );
        assert_eq!(frame(&twice).execution.status, before.execution.status);
        twice
            .checkpoint_registered_guest_session(&s.archive, 4, until())
            .unwrap();
        // Direct unsupported faulted removal keeps original custody and blocks archive.
        let air = recovered
            .pack
            .assets
            .registry
            .default_state("minecraft:air")
            .unwrap();
        recovered
            .world
            .set(POSITION[0], POSITION[1], POSITION[2], air)
            .unwrap();
        assert!(
            recovered
                .world
                .world_slot_snapshot()
                .unwrap()
                .pending
                .is_some()
        );
        assert!(
            recovered
                .checkpoint_registered_guest_session(&s.archive, 5, until())
                .is_err()
        );
    }
}
#[test]
#[ignore = "isolated genuine trap pair: persistent Guest faulted archive/recovery both roles"]
fn persistent_guest_actual_trap_faulted_roundtrip_both_roles() {
    fault_family("guest-trap");
}
#[test]
#[ignore = "isolated genuine reject pair: persistent Guest faulted archive/recovery both roles"]
fn persistent_guest_actual_reject_faulted_roundtrip_both_roles() {
    fault_family("guest-reject");
}
#[test]
#[ignore = "isolated genuine invalid-proposal pair: persistent Guest Evaluation and faulted archive/recovery both roles"]
fn persistent_guest_actual_invalid_faulted_roundtrip_both_roles() {
    fault_family("guest-invalid");
}
#[test]
#[ignore = "isolated genuine fuel-loop pair: persistent Guest Evaluation and faulted archive/recovery both roles"]
fn persistent_guest_actual_fuel_faulted_roundtrip_both_roles() {
    fault_family("guest-fuel");
}

#[test]
#[ignore = "isolated normal captured pair: real persistent token stale slot and resident allocation controls both roles"]
fn persistent_guest_actual_stale_success_and_failure_both_roles() {
    let f = Materialized::new("guest-stale");
    for owner in [true, false] {
        let mut s = guest_session(&f, "stale", owner);
        s.game.world.game_time += 1;
        let success = s
            .game
            .world
            .prepare_persistent_guest_counter_phase(true, true)
            .unwrap()
            .unwrap();
        let proposal = evaluate(&s.game, &success);
        let failure = s
            .game
            .world
            .prepare_persistent_guest_counter_phase(true, true)
            .unwrap()
            .unwrap();
        let at = (POSITION[0] >> 4, POSITION[2] >> 4);
        let column = s.game.world.chunks.get(&at).unwrap().clone();
        residency(&mut s.game, 0, (80, 80));
        assert!(!s.game.world.chunks.contains_key(&at));
        assert!(
            !s.game
                .world
                .counter_trial_snapshot()
                .unwrap()
                .record()
                .unwrap()
                .resident()
        );
        residency(&mut s.game, 2, at);
        assert!(!Arc::ptr_eq(&column, s.game.world.chunks.get(&at).unwrap()));
        assert!(
            s.game
                .world
                .counter_trial_snapshot()
                .unwrap()
                .record()
                .unwrap()
                .resident()
        );
        assert_eq!(
            s.game
                .world
                .commit_persistent_guest_counter_phase(success, proposal, true, true),
            Ok(Completion::Stale)
        );
        assert_eq!(
            s.game.world.fail_persistent_guest_counter_phase(
                failure,
                GuestFailure::Trap,
                true,
                true
            ),
            Ok(Completion::Stale)
        );
        assert!(
            s.game
                .world
                .counter_trial_snapshot()
                .unwrap()
                .fault()
                .is_none()
        );
        let token = s
            .game
            .world
            .prepare_persistent_guest_counter_phase(true, true)
            .unwrap()
            .unwrap();
        let proposal = evaluate(&s.game, &token);
        transfer(&mut s.game, &s.slot, 10, Direction::IntoPlayer, 1);
        assert_eq!(
            s.game
                .world
                .commit_persistent_guest_counter_phase(token, proposal, true, true),
            Ok(Completion::Stale)
        );
        assert_eq!(
            s.game
                .world
                .read_persistent_counter(&s.counter)
                .unwrap()
                .counter(),
            0
        );
        s.game.tick_counter_trial(true);
        assert_eq!(
            s.game
                .world
                .read_persistent_counter(&s.counter)
                .unwrap()
                .counter(),
            1
        );
    }
}

#[test]
#[ignore = "isolated normal captured pair: real server removal produces Unavailable storage; fresh missing runtime/client-only recovery refuses"]
fn persistent_guest_actual_unavailable_storage_and_runtime_refusals_both_roles() {
    let f = Materialized::new("guest-unavailable");
    for owner in [true, false] {
        let mut s = guest_session(&f, "unavailable", owner);
        advance_three(&mut s);
        s.game.mods.mods = Mods::default(); // drop the actual ordinary paired server owners
        assert!(!s.game.mods.mods.runs_server());
        s.game.world.game_time += 1;
        s.game.tick_counter_trial(true);
        assert!(matches!(
            frame(&s.game).execution.status,
            GuestStatus::Faulted {
                code: GuestFaultCode::GuestUnavailable,
                ..
            }
        ));
        s.game
            .checkpoint_registered_guest_session(&s.archive, 1, until())
            .unwrap();
        for client_only in [false, true] {
            let mut candidate = f.fresh(owner);
            let (host, _receivers) = make_host(&mut candidate, owner, [10, 20], false);
            candidate.mods.mods = if client_only {
                let captures = candidate
                    .options
                    .mod_packages
                    .catalog()
                    .unwrap()
                    .prepared()
                    .clone();
                let paths = captures.keys().cloned().collect::<Vec<_>>();
                Mods::load_for_with_prepared(Runs::Client, &paths, &captures, |_| true, |_| false)
            } else {
                Mods::default()
            };
            assert!(!candidate.mods.mods.runs_server());
            let destination = f
                .scratch
                .join(format!("unavailable-refusal-{owner}-{client_only}"));
            assert!(
                candidate
                    .recover_registered_guest_session_checkpoint(
                        request(&s, destination.clone()),
                        host
                    )
                    .is_err()
            );
            assert!(!destination.exists());
        }
        let (recovered, receipt, _receivers) =
            recover_guest(&f, &s, &format!("unavailable-recovered-{owner}"), owner);
        assert!(matches!(
            receipt.state,
            GuestRuntimeState::FaultedActive { .. }
        ));
        assert_eq!(
            frame(&recovered).execution.status,
            frame(&s.game).execution.status
        );
    }
}

#[test]
#[ignore = "isolated normal captured pair: actual wrong type/alias/client-only bootstrap retains real virgin grant"]
fn persistent_guest_actual_bootstrap_refusals_preserve_grant_both_roles() {
    let f = Materialized::new("guest-bootstrap");
    for owner in [true, false] {
        let mut game = f.fresh(owner);
        open_native(&mut game, &f.scratch.join(format!("bootstrap-{owner}")));
        residency(&mut game, 2, (0, 0));
        assert!(
            game.bootstrap_registered_guest_counter(&f.type_id, POSITION)
                .is_err()
        );
        let alias = game.pack.assets.registry.default_state(&f.alias).unwrap();
        game.world
            .set(POSITION[0], POSITION[1], POSITION[2], alias)
            .unwrap();
        assert!(
            game.bootstrap_registered_guest_counter("missing:counter", POSITION)
                .is_err()
        );
        let captures = game
            .options
            .mod_packages
            .catalog()
            .unwrap()
            .prepared()
            .clone();
        let paths = captures.keys().cloned().collect::<Vec<_>>();
        game.mods.mods =
            Mods::load_for_with_prepared(Runs::Client, &paths, &captures, |_| true, |_| false);
        assert_eq!(game.mods.mods.entries()[0].status, Status::Running);
        assert!(!game.mods.mods.runs_server());
        assert!(
            game.bootstrap_registered_guest_counter(&f.type_id, POSITION)
                .is_err()
        );
        assert!(game.world.counter_domain_snapshot().is_none());
        assert!(!game.world.has_world_slot_trial());
        game.start_mods();
        let (counter, slot) = game
            .bootstrap_registered_guest_counter(&f.type_id, POSITION)
            .unwrap();
        assert_eq!(counter.placement().generation, 1);
        assert_eq!(slot.incarnation(), 1);
        assert!(
            game.bootstrap_registered_guest_counter(&f.type_id, POSITION)
                .is_err()
        );
    }
}

#[test]
#[ignore = "isolated normal captured pair: Guest3 malformed fault clocks and mixed native/Guest profiles refuse without changing actual owners"]
fn persistent_guest_actual_frame_and_native_boundary_refusals_both_roles() {
    let f = Materialized::new("guest-frame-refusals");
    for owner in [true, false] {
        let mut s = guest_session(&f, "frame-refusals", owner);
        advance_three(&mut s);
        let original = frame(&s.game);
        let bytes = original.encode().unwrap();
        assert!(CounterFrameV2::decode(&bytes).is_err());
        assert!(
            s.game
                .world
                .counter_checkpoint_frame(s.game.world.game_time)
                .is_err()
        );
        assert!(
            s.game
                .world
                .slot_checkpoint_bytes(s.game.res.items(), s.game.world.game_time)
                .is_err()
        );
        assert!(decode_slot_frame(&slot_bytes(&s.game), s.game.res.items()).is_err());
        for mode in 0..6 {
            let mut bad = original.clone();
            bad.execution.status = GuestStatus::Faulted {
                code: GuestFaultCode::GuestEvaluation,
                observed_tick: match mode {
                    0 => -1,
                    1 => s.game.world.game_time - 1,
                    2 => s.game.world.game_time,
                    _ => s.game.world.game_time + 1,
                },
            };
            if mode == 4 {
                bad.execution.profile = "unknown".into();
            }
            if mode == 5 {
                bad.fields.kind = "native-registered-counter-checkpoint".into();
            }
            assert!(bad.encode().is_err());
            assert_eq!(frame(&s.game), original);
        }
    }
}

#[test]
#[ignore = "isolated normal captured pair: healthy Guest disposal remains explicit and ReadyDormant native recovery keeps real D items"]
fn persistent_guest_actual_ready_dormant_native_roundtrip_both_roles() {
    let f = Materialized::new("guest-dormant");
    for owner in [true, false] {
        let mut s = guest_session(&f, "dormant", owner);
        advance_three(&mut s);
        let air = s
            .game
            .pack
            .assets
            .registry
            .default_state("minecraft:air")
            .unwrap();
        s.game
            .world
            .set(POSITION[0], POSITION[1], POSITION[2], air)
            .unwrap();
        assert!(
            s.game
                .checkpoint_registered_guest_session(&s.archive, 1, until())
                .is_err()
        );
        s.game
            .deliver_pending_persistent_world_slot(&s.slot)
            .unwrap();
        assert_eq!(s.game.dropped.len(), 2);
        let before = frame(&s.game);
        assert_eq!(before.execution.status, GuestStatus::Ready);
        assert!(matches!(before.fields.placement, Placement::Dormant));
        s.game
            .checkpoint_registered_guest_session(&s.archive, 1, until())
            .unwrap();
        let items = s.game.project_session_checkpoint_items().unwrap();
        let (mut game, receipt, _receivers) =
            recover_guest(&f, &s, &format!("dormant-recovered-{owner}"), owner);
        assert!(matches!(receipt.state, GuestRuntimeState::ReadyDormant));
        assert_eq!(frame(&game), before);
        assert_eq!(game.project_session_checkpoint_items().unwrap(), items);
        assert!(game.world.persistent_counter_key().unwrap().is_none());
        game.checkpoint_registered_guest_session(&s.archive, 2, until())
            .unwrap();
        let alias = game.pack.assets.registry.default_state(&f.alias).unwrap();
        game.world
            .set(POSITION[0], POSITION[1], POSITION[2], alias)
            .unwrap();
        let slot = game
            .world
            .reconcile_persistent_world_slot()
            .unwrap()
            .unwrap();
        let counter = game.world.persistent_counter_key().unwrap().unwrap();
        assert!(counter.placement().generation > s.counter.placement().generation);
        assert!(slot.incarnation() > s.slot.incarnation());
        game.world.game_time += 1;
        game.tick_counter_trial(true);
        assert_eq!(
            game.world
                .read_persistent_counter(&counter)
                .unwrap()
                .counter(),
            1
        );
    }
}

#[test]
#[ignore = "isolated normal captured pair: genuine native image copy/promotion failure and fresh epoch retry retain Guest status and D owner"]
fn persistent_guest_actual_copy_failure_retry_preserves_all_owners_both_roles() {
    let f = Materialized::new("guest-copy-failure");
    for owner in [true, false] {
        let mut s = guest_session(&f, "copy-failure", owner);
        advance_three(&mut s);
        let before = frame(&s.game);
        let original = s.game.dropped[0].stack.clone();
        let result = s.game.checkpoint_session_profile_steps(
            NativeBindingProfile::RegisteredCube,
            true,
            &s.archive,
            1,
            until(),
            |step| {
                if step == Step::Copied {
                    Err(fail("defining actual post-copy refusal"))
                } else {
                    Ok(())
                }
            },
        );
        assert!(result.is_err());
        assert!(!published(&s, 1).exists());
        assert_eq!(frame(&s.game), before);
        assert_eq!(s.game.dropped.len(), 1);
        assert_eq!(s.game.dropped[0].stack, original);
        s.game
            .checkpoint_registered_guest_session(&s.archive, 2, until())
            .unwrap();
        let items = s.game.project_session_checkpoint_items().unwrap();
        let (game, _receipt, _receivers) =
            recover_guest(&f, &s, &format!("copy-recovered-{owner}"), owner);
        assert_eq!(game.project_session_checkpoint_items().unwrap(), items);
        assert_eq!(frame(&game), before);
    }
}

#[test]
#[ignore = "isolated normal pair: complete resealed Guest image rejects mixed counter/slot/status/capture profiles before native copy"]
fn persistent_guest_actual_resealed_profile_and_capture_refusals_both_roles() {
    let f = Materialized::new("guest-image-refusals");
    for owner in [true, false] {
        let mut s = guest_session(&f, "image-refusals", owner);
        advance_three(&mut s);
        s.game
            .checkpoint_registered_guest_session(&s.archive, 1, until())
            .unwrap();
        let image = published(&s, 1);
        let inventory = tree(&image, &[]).unwrap();
        for mode in 0..7 {
            let archive = f.scratch.join(format!("guest-bad-{owner}-{mode}"));
            fs::create_dir(&archive).unwrap();
            let bad = archive.join("epoch-00000000000000000002");
            copy_tree(&image, &bad, &inventory, until()).unwrap();
            if mode < 5 {
                let mut counter: Value =
                    serde_json::from_slice(&fs::read(bad.join(COUNTER)).unwrap()).unwrap();
                match mode {
                    0 => counter["package_identity"] = "ab".repeat(32).into(),
                    1 => counter["execution"]["primary_owner"] = "other".into(),
                    2 => counter["kind"] = "native-registered-counter-checkpoint".into(),
                    3 => {
                        counter["execution"]["status"] = serde_json::json!({"state":"faulted","code":"unknown","observed_tick":s.game.world.game_time+1})
                    }
                    _ => {
                        counter["execution"]["status"] = serde_json::json!({"state":"faulted","code":"guest-evaluation","observed_tick":s.game.world.game_time})
                    }
                }
                fs::write(bad.join(COUNTER), serde_json::to_vec(&counter).unwrap()).unwrap();
            } else if mode == 5 {
                let mut tag = raw_nbt(&fs::read(bad.join(SLOT)).unwrap()).unwrap();
                *tag.tag.get_mut("kind").unwrap() =
                    Tag::String("native-registered-slot-checkpoint".into());
                fs::write(bad.join(SLOT), nbt_bytes(&tag.tag).unwrap()).unwrap();
            } else {
                let mut manifest: Manifest = json(
                    &fs::read(bad.join("manifest.json")).unwrap(),
                    MANIFEST_LIMIT,
                )
                .unwrap();
                manifest.kind = KIND.into();
                fs::write(
                    bad.join("manifest.json"),
                    json_bytes(&manifest, MANIFEST_LIMIT).unwrap(),
                )
                .unwrap();
            }
            reseal(&bad, 2);
            let mut candidate = f.fresh(owner);
            let (host, _receivers) = make_host(&mut candidate, owner, [10, 20], false);
            let destination = f.scratch.join(format!("guest-refused-{owner}-{mode}"));
            let mut recovery = request(&s, destination.clone());
            recovery.archive_root = archive;
            assert!(
                candidate
                    .recover_registered_guest_session_checkpoint(recovery, host)
                    .is_err(),
                "mode{mode}"
            );
            assert!(!destination.exists());
            assert_eq!(tree(&image, &[]).unwrap(), inventory);
        }
    }
}

#[test]
#[ignore = "two distinct genuine captured pairs: actual wrong live server identity refuses bootstrap before grant spend"]
fn persistent_guest_actual_wrong_live_capture_refuses_bootstrap_both_roles() {
    let f = Materialized::new("guest-wrong-live-capture");
    let other = PathBuf::from(
        std::env::var_os("HORIZON_COUNTER_OTHER_PAIR_FIXTURE")
            .expect("second distinct pinned actual pair required"),
    );
    let other = other.canonicalize().unwrap();
    let captured = Arc::new(horizon_mods::paired::PreparedBundle::read_folder(&other).unwrap());
    assert_ne!(
        captured.identity(),
        f.packages
            .catalog()
            .unwrap()
            .prepared()
            .values()
            .next()
            .unwrap()
            .identity()
    );
    for owner in [true, false] {
        let mut game = f.fresh(owner);
        open_native(&mut game, &f.scratch.join(format!("wrong-capture-{owner}")));
        residency(&mut game, 2, (0, 0));
        let alias = game.pack.assets.registry.default_state(&f.alias).unwrap();
        game.world
            .set(POSITION[0], POSITION[1], POSITION[2], alias)
            .unwrap();
        game.mods.mods = Mods::load_for_with_prepared(
            Runs::Both,
            std::slice::from_ref(&other),
            &BTreeMap::from([(other.clone(), captured.clone())]),
            |_| true,
            |_| false,
        );
        assert!(game.mods.mods.runs_server());
        assert!(
            game.bootstrap_registered_guest_counter(&f.type_id, POSITION)
                .is_err()
        );
        assert!(game.world.counter_domain_snapshot().is_none());
        assert!(!game.world.has_world_slot_trial());
        game.start_mods();
        game.bootstrap_registered_guest_counter(&f.type_id, POSITION)
            .unwrap();
    }
}
// Native Eq intentionally omits BundleSelected; keep both exact runtime selections
// and the actual original root/Box allocations alongside the native component value.
fn original_slot_facts(game: &Game<'_>) -> (usize, Option<usize>, Vec<i32>, ItemStack) {
    fn selections(stack: &ItemStack, out: &mut Vec<i32>) {
        if let Some(data) = stack.data() {
            out.push(data.bundle_selected.0);
            for (_, child) in &data.container {
                selections(child, out);
            }
            for child in data.charged.iter().chain(data.bundle_contents.iter()) {
                selections(child, out);
            }
            if let Some(pots) = &data.pot_decorations {
                for child in pots.iter().flatten() {
                    selections(child, out);
                }
            }
        }
    }
    let view = game.world.guest_storage_view().unwrap();
    let stack = view.stack().expect("genuine rich slot original");
    let mut runtime = Vec::new();
    selections(stack, &mut runtime);
    (
        stack as *const ItemStack as usize,
        stack
            .extra
            .as_ref()
            .map(|data| &**data as *const _ as usize),
        runtime,
        stack.clone(),
    )
}
#[test]
#[ignore = "isolated normal captured pair: stale success/failure across actual retirement, replacement, recovery and another Game owner"]
fn persistent_guest_actual_stale_retirement_replacement_recovery_and_foreign_world_both_roles() {
    let f = Materialized::new("guest-stale-owners");
    for owner in [true, false] {
        let mut s = guest_session(&f, "stale-native-owners", owner);
        s.game.world.game_time += 1;
        let retired_success = s
            .game
            .world
            .prepare_persistent_guest_counter_phase(true, true)
            .unwrap()
            .unwrap();
        let retired_proposal = evaluate(&s.game, &retired_success);
        let retired_failure = s
            .game
            .world
            .prepare_persistent_guest_counter_phase(true, true)
            .unwrap()
            .unwrap();
        let replaced_success = s
            .game
            .world
            .prepare_persistent_guest_counter_phase(true, true)
            .unwrap()
            .unwrap();
        let replaced_proposal = evaluate(&s.game, &replaced_success);
        let replaced_failure = s
            .game
            .world
            .prepare_persistent_guest_counter_phase(true, true)
            .unwrap()
            .unwrap();
        let air = s
            .game
            .pack
            .assets
            .registry
            .default_state("minecraft:air")
            .unwrap();
        s.game
            .world
            .set(POSITION[0], POSITION[1], POSITION[2], air)
            .unwrap();
        assert!(
            s.game
                .world
                .counter_trial_snapshot()
                .unwrap()
                .record()
                .is_none()
        );
        assert!(
            s.game
                .world
                .world_slot_snapshot()
                .unwrap()
                .pending
                .is_some()
        );
        assert_eq!(
            s.game.world.commit_persistent_guest_counter_phase(
                retired_success,
                retired_proposal,
                true,
                true
            ),
            Ok(Completion::Stale)
        );
        assert_eq!(
            s.game.world.fail_persistent_guest_counter_phase(
                retired_failure,
                GuestFailure::Trap,
                true,
                true
            ),
            Ok(Completion::Stale)
        );
        assert!(
            s.game
                .world
                .counter_trial_snapshot()
                .unwrap()
                .fault()
                .is_none()
        );
        s.game
            .deliver_pending_persistent_world_slot(&s.slot)
            .unwrap();
        assert_eq!(s.game.dropped.len(), 2);
        let alias = s.game.pack.assets.registry.default_state(&f.alias).unwrap();
        s.game
            .world
            .set(POSITION[0], POSITION[1], POSITION[2], alias)
            .unwrap();
        let replacement_slot = s
            .game
            .world
            .reconcile_persistent_world_slot()
            .unwrap()
            .unwrap();
        let replacement_counter = s.game.world.persistent_counter_key().unwrap().unwrap();
        assert!(replacement_counter.placement().generation > s.counter.placement().generation);
        assert!(replacement_slot.incarnation() > s.slot.incarnation());
        let replacement = frame(&s.game);
        assert_eq!(
            s.game.world.commit_persistent_guest_counter_phase(
                replaced_success,
                replaced_proposal,
                true,
                true
            ),
            Ok(Completion::Stale)
        );
        assert_eq!(
            s.game.world.fail_persistent_guest_counter_phase(
                replaced_failure,
                GuestFailure::Trap,
                true,
                true
            ),
            Ok(Completion::Stale)
        );
        assert_eq!(frame(&s.game), replacement);
        let recovered_success = s
            .game
            .world
            .prepare_persistent_guest_counter_phase(true, true)
            .unwrap()
            .unwrap();
        let recovered_proposal = evaluate(&s.game, &recovered_success);
        let recovered_failure = s
            .game
            .world
            .prepare_persistent_guest_counter_phase(true, true)
            .unwrap()
            .unwrap();
        let foreign_success = s
            .game
            .world
            .prepare_persistent_guest_counter_phase(true, true)
            .unwrap()
            .unwrap();
        let foreign_proposal = evaluate(&s.game, &foreign_success);
        let foreign_failure = s
            .game
            .world
            .prepare_persistent_guest_counter_phase(true, true)
            .unwrap()
            .unwrap();
        s.game
            .checkpoint_registered_guest_session(&s.archive, 1, until())
            .unwrap();
        let (mut recovered, receipt, _receivers) =
            recover_guest(&f, &s, &format!("stale-owner-recovered-{owner}"), owner);
        assert!(matches!(
            receipt.state,
            GuestRuntimeState::ReadyActive { .. }
        ));
        let recovered_before = frame(&recovered);
        assert_eq!(
            recovered.world.commit_persistent_guest_counter_phase(
                recovered_success,
                recovered_proposal,
                true,
                true
            ),
            Ok(Completion::Stale)
        );
        assert_eq!(
            recovered.world.fail_persistent_guest_counter_phase(
                recovered_failure,
                GuestFailure::Trap,
                true,
                true
            ),
            Ok(Completion::Stale)
        );
        assert_eq!(frame(&recovered), recovered_before);
        let mut foreign = guest_session(&f, "stale-other-world", owner);
        foreign.game.world.game_time = s.game.world.game_time;
        let foreign_before = frame(&foreign.game);
        assert_eq!(
            foreign.game.world.commit_persistent_guest_counter_phase(
                foreign_success,
                foreign_proposal,
                true,
                true
            ),
            Ok(Completion::Stale)
        );
        assert_eq!(
            foreign.game.world.fail_persistent_guest_counter_phase(
                foreign_failure,
                GuestFailure::Trap,
                true,
                true
            ),
            Ok(Completion::Stale)
        );
        assert_eq!(frame(&foreign.game), foreign_before);
        assert!(
            recovered
                .world
                .counter_trial_snapshot()
                .unwrap()
                .fault()
                .is_none()
        );
        assert!(
            foreign
                .game
                .world
                .counter_trial_snapshot()
                .unwrap()
                .fault()
                .is_none()
        );
    }
}
