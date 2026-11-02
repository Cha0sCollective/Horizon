//! Native family files and genuine owned worker outcomes. The external runner
//! supplies a fresh cwd/home/temp and pinned resources for each Game case.
use super::*;
use crate::game::{functions::Scheduled, integrated::transition_ownership_tests::Fixture};
use horizon_client::{advancements::Tree, save};
use horizon_world::nbt::Tag;
use std::{fs, time::Duration};

const WAIT: Duration = Duration::from_secs(120);
const UUID: [i32; 4] = [1, 2, 3, 4];

fn loaded(game: &mut Game<'_>) {
    game.options.dedicated = None;
    game.level.player_uuid = Some(UUID);
    game.stats.loaded = true;
    game.adv.loaded = true;
    // Fixture setup may explicitly load; production prepare/poll must not.
    game.adv.tree = Arc::new(Tree::load(&game.options.resources.join("data")));
    game.load_scoreboard();
    game.load_bossbars();
    game.ensure_datapacks();
}
fn rich(game: &mut Game<'_>) {
    loaded(game);
    game.stats.add("custom", "minecraft:jump", 17);
    game.score
        .storage
        .set("test:opaque", Tag::compound([("kept", Tag::Long(73))]));
    game.score.stopwatches.create("test:running");
    game.score.board.add_objective(
        "retained",
        "dummy",
        horizon_client::scoreboard::literal("Retained"),
        horizon_client::scoreboard::RenderType::Integer,
        false,
        None,
    );
    game.score.board.set("Player", "retained", 23);
    game.border.set_center([43.5, -12.0]);
    game.bosses.bars.add(
        "test:bar",
        horizon_client::scoreboard::literal("Retained bar"),
    );
    let bar = game.bosses.bars.get_mut("test:bar").unwrap();
    bar.uuid = 99;
    bar.value = 7;
    bar.max = 11;
    bar.music = true;
    bar.players.push(UUID);
    game.publish_boss_bars();
    game.fns.schedule = vec![
        Scheduled {
            id: "test:late".into(),
            time: 20,
            seq: 3,
        },
        Scheduled {
            id: "#test:tag".into(),
            time: 10,
            seq: 2,
        },
        Scheduled {
            id: "test:first".into(),
            time: 10,
            seq: 1,
        },
    ];
    game.ui.clock.rate = 0.5;
    game.ui.clock.partial = 0.25;
    game.ui.clock.paused = true;
    game.rand_sequences.get("test:world", 17);
    let adv = game
        .adv
        .tree
        .list
        .iter()
        .find(|adv| !adv.criteria.is_empty())
        .expect("pinned native advancement criterion required");
    assert!(
        game.adv
            .progress
            .grant(&adv.id, &adv.criteria[0].name, "2026-10-28 12:00:00 +0000")
    );
    assert!(!game.adv.progress.criteria.is_empty());
}
fn until_settled(owner: &mut NativeMetadata<'_, '_>) -> MetadataStatus {
    loop {
        let status = owner.poll();
        if !matches!(status, MetadataStatus::Preparing | MetadataStatus::Pending) {
            return status;
        }
        std::thread::yield_now();
    }
}
fn reap(owner: &mut NativeMetadata<'_, '_>) {
    // Call only after releasing every intentional stall. No assertion can
    // unwind an intentionally held worker or drop its original owner.
    while owner.handle.is_some() || owner.tree.unresolved() {
        owner.poll();
        std::thread::yield_now();
    }
}
fn ack(owner: Box<NativeMetadata<'_, '_>>) -> MetadataAcknowledged {
    match owner.acknowledge() {
        Ok(done) => done,
        Err(owner) => {
            eprintln!(
                "metadata qualification unexpectedly blocked: {:?}",
                owner.failures()
            );
            // The fatal ownership backstop deliberately makes this a failed case.
            drop(owner);
            unreachable!()
        }
    }
}
fn complete(owner: &mut NativeMetadata<'_, '_>) {
    let _ = until_settled(owner);
}
fn storage_dirty(game: &Game<'_>) -> bool {
    let mut dirty = game.score.storage.clone();
    dirty.set("test:opaque", dirty.get("test:opaque"));
    dirty == game.score.storage
}
fn watches_file(game: &Game<'_>) -> Tag {
    save::read_dat(&game.world_root.join("data/minecraft/stopwatches.dat")).unwrap()
}

#[test]
fn native_batch_nondefault_files_and_notifications() {
    let fixture = Fixture::new("metadata-rich");
    let mut game = fixture.game();
    rich(&mut game);
    let events = game.stats.events.clone();
    let criteria = game.adv.progress.criteria.clone();
    let schedule = game.fns.schedule.clone();
    let random = game.rand_sequences.to_nbt();
    let tree = game.adv.tree.clone();
    let watches = game.score.stopwatches.retained_state_snapshot();
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    complete(&mut owner);
    let done = ack(owner);
    assert_eq!(
        done.effects[MetadataFamily::Stats as usize],
        MetadataEffect::Completed
    );
    assert_eq!(game.stats.events, events);
    assert_eq!(game.adv.progress.criteria, criteria);
    assert!(Arc::ptr_eq(&game.adv.tree, &tree));
    assert_eq!(game.fns.schedule, schedule);
    assert_eq!(game.rand_sequences.to_nbt(), random);
    assert!(!storage_dirty(&game));
    // Timer origin survives acknowledgment, while its saved dirty marker changes.
    assert!(!game.score.stopwatches.matches_retained_state(&watches));
    let stats_path = crate::game::stats::player_json(&game.world_root, "stats", UUID);
    let stats: serde_json::Value = serde_json::from_slice(&fs::read(stats_path).unwrap()).unwrap();
    assert_eq!(stats["stats"]["minecraft:custom"]["minecraft:jump"], 17);
    let adv_path = crate::game::stats::player_json(&game.world_root, "advancements", UUID);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(adv_path).unwrap()).unwrap(),
        game.adv
            .progress
            .to_json(&tree, horizon_world::DATA_VERSION)
    );
    assert_eq!(
        horizon_client::command_storage::CommandStorage::load(&game.world_root).get("test:opaque"),
        game.score.storage.get("test:opaque")
    );
    assert!(
        watches_file(&game)
            .get("data")
            .unwrap()
            .get("stopwatches")
            .unwrap()
            .get("test:running")
            .is_some()
    );
    let bars = save::read_dat(
        &game
            .world_root
            .join("data/minecraft/custom_boss_events.dat"),
    )
    .unwrap();
    assert_eq!(bars.get("data"), Some(&game.bosses.bars.pack()));
    assert_eq!(game.bosses.bars.get("test:bar").unwrap().uuid, 99);
    let border = save::read_dat(&game.world_root.join("data/minecraft/world_border.dat")).unwrap();
    assert_eq!(border.get("data"), Some(&game.border.pack()));
    let board = save::read_dat(&game.world_root.join("data/minecraft/scoreboard.dat")).unwrap();
    assert_eq!(board.get("data"), Some(&game.score.board.pack()));
    let schedule =
        save::read_dat(&game.world_root.join("data/minecraft/scheduled_events.dat")).unwrap();
    let Some(Tag::List(events)) = schedule.get("data").and_then(|v| v.get("events")) else {
        panic!("native schedule")
    };
    assert_eq!(
        events
            .iter()
            .map(|v| v.get("id").unwrap().clone())
            .collect::<Vec<_>>(),
        vec![
            Tag::String("test:first".into()),
            Tag::String("#test:tag".into()),
            Tag::String("test:late".into())
        ]
    );
    let clocks = save::read_world_clocks(&game.world_root).unwrap().unwrap();
    let clock = clocks
        .get("data")
        .unwrap()
        .get("minecraft:overworld")
        .unwrap();
    assert_eq!(
        clock.get("total_ticks"),
        Some(&Tag::Long(game.world.day_time))
    );
    assert_eq!(
        clock.get("partial_tick"),
        Some(&Tag::Float(0.25f32.to_bits()))
    );
    assert!(done.failures.is_empty());
}

#[test]
fn native_fresh_defaults_stay_sparse_and_existing_defaults_replace() {
    let fixture = Fixture::new("metadata-sparse");
    let mut game = fixture.game();
    loaded(&mut game);
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    complete(&mut owner);
    let first = ack(owner);
    for name in [
        "scoreboard.dat",
        "world_border.dat",
        "custom_boss_events.dat",
        "scheduled_events.dat",
        "stopwatches.dat",
    ] {
        assert!(
            !game.world_root.join("data/minecraft").join(name).exists(),
            "{name}"
        );
    }
    assert_eq!(
        first.effects[MetadataFamily::Bossbars as usize],
        MetadataEffect::Skipped
    );
    for name in [
        "scoreboard.dat",
        "world_border.dat",
        "custom_boss_events.dat",
        "scheduled_events.dat",
    ] {
        save::write_dat(
            &game.world_root.join("data/minecraft").join(name),
            Tag::compound([("obsolete", Tag::Int(4))]),
        )
        .unwrap();
    }
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    complete(&mut owner);
    let second = ack(owner);
    assert_eq!(
        second.effects[MetadataFamily::Bossbars as usize],
        MetadataEffect::Completed
    );
    for name in [
        "scoreboard.dat",
        "world_border.dat",
        "custom_boss_events.dat",
        "scheduled_events.dat",
    ] {
        let root = save::read_dat(&game.world_root.join("data/minecraft").join(name)).unwrap();
        assert!(root.get("obsolete").is_none());
        assert!(root.get("data").is_some());
    }
}

#[test]
fn native_unloaded_and_dedicated_no_player_guards() {
    let fixture = Fixture::new("metadata-unloaded");
    let mut game = fixture.game();
    let (_, stats) = game.prepare_metadata_stats();
    assert_eq!(stats.guard(), Some(crate::game::stats::StatsSkip::Unloaded));
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    complete(&mut owner);
    let done = ack(owner);
    for family in [
        MetadataFamily::Stats,
        MetadataFamily::Advancements,
        MetadataFamily::Stopwatches,
        MetadataFamily::Bossbars,
        MetadataFamily::Schedule,
    ] {
        assert_eq!(done.effects[family as usize], MetadataEffect::Skipped);
    }
    game.stats.loaded = true;
    game.adv.loaded = true;
    game.level.player_uuid = Some(UUID);
    let (_, stats) = game.prepare_metadata_stats();
    assert_eq!(stats.guard(), Some(crate::game::stats::StatsSkip::NoPlayer));
    let tree = RetainedTreeState::new();
    let (_, adv) = game.prepare_metadata_advancements(&tree).unwrap();
    assert_eq!(
        adv.guard(),
        Some(crate::game::advancements::AdvancementsSkip::NoPlayer)
    );
    game.options.dedicated = None;
    game.level.player_uuid = None;
    let (_, stats) = game.prepare_metadata_stats();
    assert_eq!(
        stats.guard(),
        Some(crate::game::stats::StatsSkip::MissingUuid)
    );
    let (_, adv) = game.prepare_metadata_advancements(&tree).unwrap();
    assert_eq!(
        adv.guard(),
        Some(crate::game::advancements::AdvancementsSkip::MissingUuid)
    );
    assert!(stats.path().is_none() && adv.path().is_none());
}

#[test]
fn native_remote_guard_uses_actual_memory_session() {
    let fixture = Fixture::new("metadata-remote");
    let mut game = fixture.game();
    rich(&mut game);
    let (client, server) = horizon_protocol::connection::Pipe::pair();
    game.connect_local(client, "MetadataFixture").unwrap();
    let (_, random) = game.prepare_metadata_random_sequences().unwrap();
    assert_eq!(
        random.guard(),
        Some(crate::game::trading::RandomSequencesSkip::Remote)
    );
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    complete(&mut owner);
    let done = ack(owner);
    // Clock intentionally follows its ordinary root guard, independent of
    // remote. This fixture proves adapter guards, not a connected game trial.
    for family in [
        MetadataFamily::RandomSequences,
        MetadataFamily::Stats,
        MetadataFamily::Advancements,
        MetadataFamily::Storage,
        MetadataFamily::Bossbars,
        MetadataFamily::Schedule,
    ] {
        assert_eq!(done.effects[family as usize], MetadataEffect::Skipped);
    }
    server.shutdown();
    drop(server);
    drop(game.remote.take());
}

#[test]
fn native_stats_projection_identity_and_notification_mismatch() {
    let fixture = Fixture::new("metadata-stats-identity");
    let mut game = fixture.game();
    loaded(&mut game);
    game.stats.add("custom", "minecraft:jump", 3);
    let (expected, input) = game.prepare_metadata_stats();
    let (_, foreign) = game.prepare_metadata_stats();
    let actual = input.write_native().unwrap();
    let foreign = foreign.write_native().unwrap();
    assert!(game.preflight_metadata_stats(&expected, &foreign).is_err());
    game.stats
        .events
        .push(("test:event".into(), "test:key".into(), None));
    assert!(game.preflight_metadata_stats(&expected, &actual).is_err());
    game.stats.events.pop();
    assert!(game.preflight_metadata_stats(&expected, &actual).is_ok());
    assert_eq!(game.stats.events.len(), 1);
}

#[test]
fn native_local_merchant_loan_projection_conflict_and_owner_membership() {
    use crate::game::{stations, trading};
    use horizon_core::random::RandomSource;
    let fixture = Fixture::new("metadata-loan");
    let mut game = fixture.game();
    let mut session = horizon_client::merchant::Session::new(Vec::new(), true);
    let mut random = game.rand_sequences.lend("test:loan", game.level.seed);
    random.next_long();
    session.sequences.insert("test:loan".into(), random);
    let mut station = stations::Station::new_kind(stations::Kind::Merchant, [0, 0, 0]);
    station.merchant = Some(Box::new(trading::MerchantState {
        session: Box::new(session),
        scroll: 0,
        focused: None,
        mob: None,
        name: None,
    }));
    game.station = Some(station);
    let loan = game
        .station
        .as_ref()
        .unwrap()
        .merchant
        .as_ref()
        .unwrap()
        .session
        .sequences["test:loan"]
        .clone();
    let before = game.rand_sequences.to_nbt();
    let (expected, input) = game.prepare_metadata_random_sequences().unwrap();
    let written = input.write_native().unwrap();
    assert_eq!(game.rand_sequences.to_nbt(), before);
    assert!(
        game.preflight_metadata_random_sequences(&expected, &written)
            .is_ok()
    );
    // Equal merged bytes with a different real loan owner still refuse.
    game.rand_sequences
        .give_back("test:loan".into(), loan.clone());
    assert!(
        game.preflight_metadata_random_sequences(&expected, &written)
            .is_err()
    );
    game.station
        .as_mut()
        .unwrap()
        .merchant
        .as_mut()
        .unwrap()
        .session
        .sequences
        .get_mut("test:loan")
        .unwrap()
        .next_long();
    let conflict = game.prepare_metadata_random_sequences().is_err();
    assert!(conflict);
    assert!(save::read_dat(input.path().unwrap()).is_some());
}

fn primary_write_failure(family: MetadataFamily) {
    let fixture = Fixture::new("metadata-family-failure");
    let mut game = fixture.game();
    rich(&mut game);
    let path = match family {
        MetadataFamily::Stats => crate::game::stats::player_json(&game.world_root, "stats", UUID),
        MetadataFamily::Advancements => {
            crate::game::stats::player_json(&game.world_root, "advancements", UUID)
        }
        _ => game.world_root.join(match family {
            MetadataFamily::RandomSequences => "data/minecraft/random_sequences.dat_new",
            MetadataFamily::Storage => "data/test/command_storage.dat_new",
            MetadataFamily::Stopwatches => "data/minecraft/stopwatches.dat_new",
            MetadataFamily::Border => "data/minecraft/world_border.dat_new",
            MetadataFamily::Scoreboard => "data/minecraft/scoreboard.dat_new",
            MetadataFamily::Bossbars => "data/minecraft/custom_boss_events.dat_new",
            MetadataFamily::Schedule => "data/minecraft/scheduled_events.dat_new",
            MetadataFamily::Clock => "data/minecraft/world_clocks.dat_new",
            _ => unreachable!(),
        }),
    };
    fs::create_dir_all(&path).unwrap();
    let (control, receipt) = MetadataTestControl::new(MetadataTestAction::HoldBeforeFamily(family));
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    owner.control = Some(control);
    owner.poll();
    let until = Instant::now() + WAIT;
    while !receipt.entered() && Instant::now() < until {
        owner.poll();
        std::thread::yield_now();
    }
    let entered = receipt.entered();
    let input = owner.input.clone();
    owner.deadline = Instant::now();
    let blocked = matches!(owner.poll(), MetadataStatus::Blocked(_));
    let busy = owner.retry(Instant::now() + WAIT) == Err(MetadataRetryRefusal::Busy);
    receipt.release();
    reap(&mut owner);
    let late_blocked = matches!(owner.poll(), MetadataStatus::Blocked(_));
    let effects = owner.effects();
    let dirty = storage_dirty(owner.game);
    let failure = owner
        .outcome
        .as_ref()
        .is_some_and(|outcome| outcome.error.is_some());
    fs::remove_dir(&path).unwrap();
    let retry = owner.retry(Instant::now() + WAIT).is_ok();
    complete(&mut owner);
    let same = Arc::ptr_eq(input.as_ref().unwrap(), owner.input.as_ref().unwrap());
    let done = ack(owner);
    assert!(entered && blocked && busy && late_blocked && failure && dirty && retry && same);
    assert_eq!(effects[family as usize], MetadataEffect::Unknown);
    assert!(
        effects[..family as usize]
            .iter()
            .all(|e| *e == MetadataEffect::Completed)
    );
    assert!(
        effects[family as usize + 1..]
            .iter()
            .all(|e| *e == MetadataEffect::Unattempted)
    );
    assert!(done.failures.len() >= 2);
}
#[test]
fn native_random_file_failure_repair_replays_original() {
    primary_write_failure(MetadataFamily::RandomSequences);
}
#[test]
fn native_stats_file_failure_repair_replays_original() {
    primary_write_failure(MetadataFamily::Stats);
}
#[test]
fn native_advancement_file_failure_repair_replays_original() {
    primary_write_failure(MetadataFamily::Advancements);
}

#[test]
fn native_ready_result_cannot_ack_after_unpolled_deadline() {
    let fixture = Fixture::new("metadata-ack-deadline");
    let mut game = fixture.game();
    rich(&mut game);
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    complete(&mut owner);
    owner.deadline = Instant::now();
    let mut owner = match owner.acknowledge() {
        Err(owner) => owner,
        Ok(_) => panic!("expired ack"),
    };
    let dirty = storage_dirty(owner.game);
    let attempts = owner.next_attempt;
    let retry = owner.retry(Instant::now() + WAIT).is_ok();
    complete(&mut owner);
    let _ = ack(owner);
    assert!(dirty && retry);
    assert_eq!(attempts, 1);
}

#[test]
fn native_clock_bit_change_refuses_preflight_and_default_removes_controls() {
    let fixture = Fixture::new("metadata-clock-bits");
    let mut game = fixture.game();
    loaded(&mut game);
    game.ui.clock.partial = -0.0;
    let (expected, input) = game.prepare_retained_clock_metadata();
    let post = input.write_native().result.unwrap();
    game.ui.clock.partial = 0.0;
    assert!(
        game.preflight_retained_clock_metadata(&expected, &post)
            .is_err()
    );
    game.ui.clock.partial = -0.0;
    assert!(
        game.preflight_retained_clock_metadata(&expected, &post)
            .is_ok()
    );
    let read = save::read_world_clocks(&game.world_root).unwrap().unwrap();
    let clock = read
        .get("data")
        .unwrap()
        .get("minecraft:overworld")
        .unwrap();
    for key in ["partial_tick", "rate", "paused"] {
        assert!(clock.get(key).is_none());
    }
}

#[test]
fn native_metadata_delay_deadline_busy_and_explicit_late_success() {
    let fixture = Fixture::new("metadata-delay");
    let mut game = fixture.game();
    rich(&mut game);
    let (control, receipt) = MetadataTestControl::new(MetadataTestAction::HoldBeforeIo);
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    owner.control = Some(control);
    owner.poll();
    let until = Instant::now() + WAIT;
    while !receipt.entered() && Instant::now() < until {
        std::thread::yield_now();
    }
    let entered = receipt.entered();
    let sample = owner.sampled_ms();
    let input = owner.input.clone();
    let mut pending = true;
    for _ in 0..16 {
        pending &= owner.poll() == MetadataStatus::Pending;
    }
    owner.deadline = Instant::now();
    let expired = matches!(owner.poll(), MetadataStatus::Blocked(_));
    let busy = owner.retry(Instant::now() + WAIT) == Err(MetadataRetryRefusal::Busy);
    receipt.release();
    reap(&mut owner);
    let blocked = matches!(owner.poll(), MetadataStatus::Blocked(_));
    let dirty = storage_dirty(owner.game);
    let retry = owner.retry(Instant::now() + WAIT).is_ok();
    let same_input = input
        .as_ref()
        .is_some_and(|i| Arc::ptr_eq(i, owner.input.as_ref().unwrap()));
    let attempts = owner.next_attempt;
    complete(&mut owner);
    let done = ack(owner);
    assert!(entered && pending && expired && busy && blocked && dirty && retry && same_input);
    assert_eq!(attempts, 1);
    assert_eq!(sample, done.sampled_ms);
    assert!(!done.failures.is_empty());
}

fn controlled_refusal(action: MetadataTestAction) {
    let fixture = Fixture::new("metadata-refusal");
    let mut game = fixture.game();
    rich(&mut game);
    let (control, receipt) = MetadataTestControl::new(action);
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    owner.control = Some(control);
    let blocked = matches!(until_settled(&mut owner), MetadataStatus::Blocked(_));
    reap(&mut owner);
    receipt.release();
    let input = owner.input.clone();
    let dirty = storage_dirty(owner.game);
    let retry = owner.retry(Instant::now() + WAIT).is_ok();
    complete(&mut owner);
    let same = input
        .as_ref()
        .is_some_and(|i| Arc::ptr_eq(i, owner.input.as_ref().unwrap()));
    let done = ack(owner);
    assert!(blocked && dirty && retry && same);
    assert!(!done.failures.is_empty());
}
#[test]
fn native_metadata_spawn_refusal_retains_original_input() {
    controlled_refusal(MetadataTestAction::RefuseSpawn);
}
#[test]
fn native_metadata_dispatch_disconnect_reaps_before_retry() {
    controlled_refusal(MetadataTestAction::DisconnectDispatch);
}

#[test]
fn native_metadata_actual_worker_panic_replays_fixed_input() {
    let fixture = Fixture::new("metadata-panic");
    let mut game = fixture.game();
    rich(&mut game);
    let (control, receipt) = MetadataTestControl::new(MetadataTestAction::HoldThenPanic);
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    owner.control = Some(control);
    owner.poll();
    let until = Instant::now() + WAIT;
    while !receipt.entered() && Instant::now() < until {
        std::thread::yield_now();
    }
    let entered = receipt.entered();
    receipt.release();
    reap(&mut owner);
    let blocked = matches!(owner.poll(), MetadataStatus::Blocked(_));
    let input = owner.input.clone();
    let unknown = owner
        .failures()
        .iter()
        .any(|f| f.effects == [MetadataEffect::Unknown; MetadataFamily::ALL.len()]);
    let retry = owner.retry(Instant::now() + WAIT).is_ok();
    complete(&mut owner);
    let same = Arc::ptr_eq(input.as_ref().unwrap(), owner.input.as_ref().unwrap());
    let done = ack(owner);
    assert!(entered && blocked && unknown && retry && same);
    assert_eq!(done.effects[9], MetadataEffect::Completed);
}

#[test]
fn native_partial_namespace_unknown_repair_keeps_dirty_and_fixed_sample() {
    let fixture = Fixture::new("metadata-storage-failure");
    let mut game = fixture.game();
    rich(&mut game);
    game.score
        .storage
        .set("a:first", Tag::compound([("v", Tag::Long(11))]));
    game.score
        .storage
        .set("z:last", Tag::compound([("v", Tag::Long(22))]));
    let obstruction = game.world_root.join("data/z/command_storage.dat_new");
    fs::create_dir_all(&obstruction).unwrap();
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    let status = until_settled(&mut owner);
    let effects = owner.effects();
    let dirty = storage_dirty(owner.game);
    let sample = owner.sampled_ms();
    let partial =
        save::read_dat(&owner.game.world_root.join("data/a/command_storage.dat")).is_some();
    fs::remove_dir(&obstruction).unwrap();
    let retry = owner.retry(Instant::now() + WAIT).is_ok();
    complete(&mut owner);
    let done = ack(owner);
    assert!(matches!(status, MetadataStatus::Blocked(_)) && dirty && partial && retry);
    assert_eq!(effects[3], MetadataEffect::Unknown);
    assert_eq!(effects[4], MetadataEffect::Unattempted);
    assert_eq!(sample, done.sampled_ms);
    assert!(!storage_dirty(&game));
}

#[test]
fn native_later_board_failure_keeps_all_original_markers_and_native_artifact() {
    let fixture = Fixture::new("metadata-late-failure");
    let mut game = fixture.game();
    rich(&mut game);
    // Existing file forces replacement even for a default board.
    let board = game.world_root.join("data/minecraft/scoreboard.dat");
    save::write_dat(&board, Tag::compound([("old", Tag::Int(1))])).unwrap();
    let obstruction = board.with_file_name("scoreboard.dat_new");
    fs::create_dir(&obstruction).unwrap();
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    let status = until_settled(&mut owner);
    let effects = owner.effects();
    let dirty = storage_dirty(owner.game);
    let watches = owner.game.score.stopwatches.retained_state_snapshot();
    let saved = watches_file(owner.game);
    let sample = owner.sampled_ms();
    fs::remove_dir(&obstruction).unwrap();
    let retry = owner.retry(Instant::now() + WAIT).is_ok();
    complete(&mut owner);
    let done = ack(owner);
    assert!(matches!(status, MetadataStatus::Blocked(_)) && dirty && retry);
    assert_eq!(effects[3], MetadataEffect::Completed);
    assert_eq!(effects[4], MetadataEffect::Completed);
    assert_eq!(effects[6], MetadataEffect::Unknown);
    assert_eq!(effects[7], MetadataEffect::Unattempted);
    assert_eq!(saved, watches_file(&game));
    assert_eq!(sample, done.sampled_ms);
    assert!(!game.score.stopwatches.matches_retained_state(&watches));
    assert!(save::read_dat(&board.with_file_name("scoreboard.dat_old")).is_some());
}

#[test]
fn native_clock_strict_read_error_repair_and_unknown_clock_merge() {
    let fixture = Fixture::new("metadata-clock");
    let mut game = fixture.game();
    rich(&mut game);
    let path = game.world_root.join("data/minecraft/world_clocks.dat");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"malformed native clock").unwrap();
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    let status = until_settled(&mut owner);
    let effects = owner.effects();
    let dirty = storage_dirty(owner.game);
    let opaque = Tag::compound([
        ("total_ticks", Tag::Long(987)),
        ("unknown", Tag::String("kept".into())),
    ]);
    save::write_dat(
        &path,
        Tag::compound([("data", Tag::compound([("test:clock", opaque.clone())]))]),
    )
    .unwrap();
    let retry = owner.retry(Instant::now() + WAIT).is_ok();
    complete(&mut owner);
    let _done = ack(owner);
    let read = save::read_world_clocks(&game.world_root).unwrap().unwrap();
    assert!(matches!(status, MetadataStatus::Blocked(_)) && dirty && retry);
    assert_eq!(effects[9], MetadataEffect::Unknown);
    assert_eq!(read.get("data").unwrap().get("test:clock"), Some(&opaque));
}

#[test]
fn native_whole_batch_late_preflight_mismatch_never_cleans_score_markers() {
    let fixture = Fixture::new("metadata-preflight");
    let mut game = fixture.game();
    rich(&mut game);
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    complete(&mut owner);
    let original = owner.game.fns.schedule.clone();
    owner.game.fns.schedule[0].seq += 1;
    let mut owner = match owner.acknowledge() {
        Err(owner) => owner,
        Ok(_) => panic!("changed schedule acknowledged"),
    };
    let dirty = storage_dirty(owner.game);
    let failure = owner.failures().last().map(|v| v.detail.clone());
    owner.game.fns.schedule = original;
    let retry = owner.retry(Instant::now() + WAIT).is_ok();
    complete(&mut owner);
    let _done = ack(owner);
    assert!(dirty && retry);
    assert!(failure.unwrap().contains("schedule"));
    assert!(!storage_dirty(&game));
}

#[test]
fn native_equal_bytes_foreign_batch_and_attempt_refuse_then_original_acks() {
    let fixture = Fixture::new("metadata-foreign");
    let mut game = fixture.game();
    rich(&mut game);
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    complete(&mut owner);
    // Real native output, retained intact. Only private association is changed
    // to prove equality cannot substitute for its original owner/attempt.
    let original = owner.outcome.as_ref().unwrap().batch.clone();
    owner.outcome.as_mut().unwrap().batch = Arc::new(BatchIdentity);
    let refused_batch = owner.preflight().is_err();
    let dirty_batch = storage_dirty(owner.game);
    owner.outcome.as_mut().unwrap().batch = original;
    let original = owner.outcome.as_ref().unwrap().attempt.clone();
    owner.outcome.as_mut().unwrap().attempt = Arc::new(AttemptIdentity {
        number: original.number,
    });
    let refused_attempt = owner.preflight().is_err();
    let dirty_attempt = storage_dirty(owner.game);
    owner.outcome.as_mut().unwrap().attempt = original;
    let _done = ack(owner);
    assert!(refused_batch && refused_attempt && dirty_batch && dirty_attempt);
}

#[test]
fn native_deadline_before_start_requires_explicit_retry() {
    let fixture = Fixture::new("metadata-expired-start");
    let mut game = fixture.game();
    loaded(&mut game);
    let mut owner = NativeMetadata::begin(&mut game, Instant::now());
    let blocked = matches!(owner.poll(), MetadataStatus::Blocked(_));
    let unstarted = owner.input.is_none() && !owner.tree_started;
    let retry = owner.retry(Instant::now() + WAIT).is_ok();
    complete(&mut owner);
    let _done = ack(owner);
    assert!(blocked && unstarted && retry);
}

fn controlled_tree(action: crate::game::advancements::RetainedTreeTestAction) {
    use crate::game::advancements::RetainedTreeTestControl;
    let fixture = Fixture::new("metadata-tree");
    let mut game = fixture.game();
    game.options.dedicated = None;
    game.level.player_uuid = Some(UUID);
    game.adv.loaded = true;
    // Empty actual data directory exercises a legitimately selected empty tree,
    // whose readiness must come from its real helper, never handle absence.
    game.options.resources = game.world_root.join("empty-resources");
    fs::create_dir_all(game.options.resources.join("data")).unwrap();
    let (control, receipt) = RetainedTreeTestControl::new(action);
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    owner.tree.set_test_control(control).unwrap();
    owner.poll();
    let refused = matches!(owner.tree.status(), RetainedTreeStatus::Failed(_));
    let mut held = true;
    let mut entered = true;
    if !refused {
        let until = Instant::now() + WAIT;
        while !receipt.entered() && Instant::now() < until {
            std::thread::yield_now();
        }
        entered = receipt.entered();
        for _ in 0..16 {
            held &= owner.poll() == MetadataStatus::Preparing;
        }
        owner.deadline = Instant::now();
        owner.poll();
    }
    receipt.release();
    reap(&mut owner);
    let blocked = matches!(owner.poll(), MetadataStatus::Blocked(_));
    let retry = owner.retry(Instant::now() + WAIT).is_ok();
    complete(&mut owner);
    let legitimate_empty =
        owner.game.adv.tree.list.is_empty() && *owner.tree.status() == RetainedTreeStatus::Ready;
    let done = ack(owner);
    assert!(entered && held && blocked && retry && legitimate_empty);
    assert_eq!(
        done.effects[MetadataFamily::Advancements as usize],
        MetadataEffect::Completed
    );
}
#[test]
fn native_advancement_held_empty_tree_late_success_is_retained() {
    controlled_tree(crate::game::advancements::RetainedTreeTestAction::BlockBeforeLoad);
}
#[test]
fn native_advancement_actual_tree_panic_requires_retry() {
    controlled_tree(crate::game::advancements::RetainedTreeTestAction::BlockThenPanic);
}
#[test]
fn native_advancement_tree_spawn_refusal_requires_retry() {
    controlled_tree(crate::game::advancements::RetainedTreeTestAction::RefuseSpawn);
}

#[test]
fn native_existing_prefetch_is_reaped_even_when_unloaded() {
    let fixture = Fixture::new("metadata-existing-tree");
    let mut game = fixture.game();
    game.prefetch_advancement_tree();
    let actual = game.adv.pending_tree.is_some();
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    complete(&mut owner);
    let reaped = owner.game.adv.pending_tree.is_none() && !owner.tree.unresolved();
    let done = ack(owner);
    assert!(actual && reaped);
    assert_eq!(done.effects[2], MetadataEffect::Skipped);
}

#[test]
#[ignore = "coordinator direct prebuilt fatal child only"]
fn native_metadata_unresolved_drop_child() {
    if std::env::var("HORIZON_METADATA_FATAL_CHILD").as_deref() != Ok("drop") {
        return;
    }
    let fixture = Fixture::new("metadata-fatal");
    let mut game = fixture.game();
    rich(&mut game);
    let (control, receipt) = MetadataTestControl::new(MetadataTestAction::HoldBeforeIo);
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    owner.control = Some(control);
    owner.poll();
    let until = Instant::now() + WAIT;
    while !receipt.entered() && Instant::now() < until {
        std::thread::yield_now();
    }
    if !receipt.entered() {
        receipt.release();
        reap(&mut owner);
        complete(&mut owner);
        let _ = ack(owner);
        std::process::exit(91);
    }
    eprintln!("HORIZON_NATIVE_METADATA_OWNER_DROP_ARMED");
    drop(owner);
    std::process::exit(91);
}

#[test]
fn native_storage_family_stall_late_failure_and_retry() {
    primary_write_failure(MetadataFamily::Storage);
}
#[test]
fn native_stopwatches_family_stall_late_failure_and_retry() {
    primary_write_failure(MetadataFamily::Stopwatches);
}
#[test]
fn native_border_family_stall_late_failure_and_retry() {
    primary_write_failure(MetadataFamily::Border);
}
#[test]
fn native_scoreboard_family_stall_late_failure_and_retry() {
    primary_write_failure(MetadataFamily::Scoreboard);
}
#[test]
fn native_bossbars_family_stall_late_failure_and_retry() {
    primary_write_failure(MetadataFamily::Bossbars);
}
#[test]
fn native_schedule_family_stall_late_failure_and_retry() {
    primary_write_failure(MetadataFamily::Schedule);
}
#[test]
fn native_clock_family_stall_late_failure_and_retry() {
    primary_write_failure(MetadataFamily::Clock);
}

fn remaining_rich(game: &mut Game<'_>) {
    use crate::game::{Dim, dragon::Fight, maps::Maps};
    rich(game);
    game.maps = Maps::retained_metadata_fixture(Some(12), &[9, 3], &[12]);
    game.fight = Fight::retained_metadata_fixture(true, true, true);
    game.set_retained_trader_fixture(true, true);
    // Deliberately unloaded yet dirty: loading is NOT the arrivals save guard.
    game.set_retained_arrivals_fixture(false, false, true);
    game.dim = Dim::Overworld;
    game.level.player_extra = Some(Tag::compound([
        ("opaque", Tag::Long(91)),
        ("raid_omen_position", Tag::IntArray(vec![-1, -2, -3])),
    ]));
    game.seed_retained_raids_fixture(Some(Dim::Nether), true, true, Some([7, 8, 9]));
}
fn raid_file(game: &Game<'_>) -> std::path::PathBuf {
    horizon_client::save::dimension_dir(&game.world_root, "minecraft:the_nether")
        .join("data/minecraft/raids.dat")
}
fn fight_file(game: &Game<'_>) -> std::path::PathBuf {
    horizon_client::save::dimension_dir(&game.world_root, "minecraft:the_end")
        .join("data/minecraft/ender_dragon_fight.dat")
}

fn native_decoded(tag: Tag) -> Tag {
    // Native NBT reads canonicalize negative zero. Compare native disk reads
    // against that same decoded representation; live-state snapshots above
    // continue to require the original raw floating-point bits.
    use horizon_world::nbt::{self, Limits, NamedTag};
    let bytes = nbt::write_named(
        &NamedTag {
            name: "".into(),
            tag,
        },
        Limits::default(),
    )
    .unwrap();
    let (decoded, used) = nbt::read_named(&bytes, Limits::default()).unwrap();
    assert_eq!(used, bytes.len());
    decoded.tag
}

#[test]
fn remaining_nondefault_native_fifteen_family_ack_and_runtime_preservation() {
    let fixture = Fixture::new("remaining-rich");
    let mut game = fixture.game();
    remaining_rich(&mut game);
    let maps = game.maps.retained_metadata_test_snapshot(true);
    let fight = game.fight.retained_metadata_test_snapshot(true);
    let trader = game.retained_trader_fixture_state();
    let arrivals = game.retained_arrivals_fixture_state();
    let raids = game.retained_raids_fixture_snapshot();
    let raid_tag = game.retained_raids_fixture_tag().unwrap();
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    complete(&mut owner);
    let done = ack(owner);
    assert_eq!(
        done.effects,
        [MetadataEffect::Completed; MetadataFamily::ALL.len()]
    );
    assert_eq!(game.maps.retained_metadata_test_snapshot(true), maps);
    assert!(game.maps.retained_metadata_test_dirty().is_empty());
    assert_eq!(game.fight.retained_metadata_test_snapshot(true), fight);
    assert!(!game.fight.retained_metadata_test_dirty());
    let after = game.retained_trader_fixture_state();
    assert_eq!(&after[..4], &trader[..4]);
    assert_eq!(after[4], 0);
    assert!(arrivals.runtime_eq(&game.retained_arrivals_fixture_state()));
    assert!(!game.retained_arrivals_fixture_state().dirty());
    assert!(game.retained_raids_fixture_runtime_matches(&raids));
    assert!(!game.retained_raids_fixture_dirty());
    let evidence = done.maps.unwrap();
    assert_eq!(
        evidence.maps,
        vec![
            (3, MetadataEffect::Completed),
            (9, MetadataEffect::Completed)
        ]
    );
    assert_eq!(evidence.last_id, MetadataEffect::Completed);
    for id in [3, 9] {
        let root = save::read_dat(
            &game
                .world_root
                .join(format!("data/minecraft/maps/{id}.dat")),
        )
        .unwrap();
        let map = &game.maps.data[&id];
        let data = root.get("data").unwrap();
        assert_eq!(
            data.get("colors"),
            Some(&Tag::ByteArray(map.colors.clone()))
        );
        assert_eq!(data.get("xCenter"), Some(&Tag::Int(map.center[0])));
        assert_eq!(
            data.get("dimension"),
            Some(&Tag::String(map.dimension.as_str().into()))
        );
    }
    assert!(!game.world_root.join("data/minecraft/maps/12.dat").exists());
    let index = save::read_dat(&game.world_root.join("data/minecraft/maps/last_id.dat")).unwrap();
    assert_eq!(index.get("data").unwrap().get("map"), Some(&Tag::Int(12)));
    let fight = save::read_dat(&fight_file(&game)).unwrap();
    let data = fight.get("data").unwrap();
    assert_eq!(
        data.get("exit_portal_location"),
        Some(&Tag::IntArray(vec![-2, 64, 4]))
    );
    assert_eq!(
        data.get("horizon_respawn_crystals"),
        Some(&Tag::IntArray(vec![1, 64, -2, -3, -66, 3]))
    );
    let trader =
        save::read_dat(&game.world_root.join("data/minecraft/wandering_trader.dat")).unwrap();
    assert_eq!(
        trader.get("data").unwrap().get("spawn_delay"),
        Some(&Tag::Int(12345))
    );
    assert_eq!(
        trader.get("data").unwrap().get("spawn_chance"),
        Some(&Tag::Int(63))
    );
    assert_eq!(
        save::read_dat(&game.world_root.join("data/horizon_portal_arrivals.dat")),
        Some(native_decoded(arrivals.native_data()))
    );
    assert_eq!(
        save::read_dat(&raid_file(&game)),
        Some(native_decoded(raid_tag))
    );
    assert!(!game.world_root.join("data/minecraft/raids.dat").exists());
    assert_eq!(
        game.level
            .player_extra
            .as_ref()
            .unwrap()
            .get("raid_omen_position"),
        Some(&Tag::IntArray(vec![7, 8, 9]))
    );
    assert_eq!(
        game.level.player_extra.as_ref().unwrap().get("opaque"),
        Some(&Tag::Long(91))
    );
}

fn remaining_map_failure(index: bool) {
    let fixture = Fixture::new("remaining-map-failure");
    let mut game = fixture.game();
    remaining_rich(&mut game);
    let before = game.maps.retained_metadata_test_snapshot(false);
    let extra = game.level.player_extra.clone();
    let obstruction = game.world_root.join(if index {
        "data/minecraft/maps/last_id.dat_new"
    } else {
        "data/minecraft/maps/9.dat_new"
    });
    fs::create_dir_all(&obstruction).unwrap();
    let (control, receipt) =
        MetadataTestControl::new(MetadataTestAction::HoldBeforeFamily(MetadataFamily::Maps));
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    owner.control = Some(control);
    owner.poll();
    let until = Instant::now() + WAIT;
    while !receipt.entered() && Instant::now() < until {
        owner.poll();
        std::thread::yield_now();
    }
    let entered = receipt.entered();
    owner.deadline = Instant::now();
    owner.poll();
    let busy = owner.retry(Instant::now() + WAIT) == Err(MetadataRetryRefusal::Busy);
    receipt.release();
    reap(&mut owner);
    let blocked = matches!(owner.poll(), MetadataStatus::Blocked(_));
    let unchanged = owner.game.maps.retained_metadata_test_snapshot(false) == before
        && owner.game.level.player_extra == extra
        && storage_dirty(owner.game);
    let evidence = owner.outcome.as_ref().and_then(|v| v.maps.clone());
    let input = owner.input.clone();
    fs::remove_dir(&obstruction).unwrap();
    let retry = owner.retry(Instant::now() + WAIT).is_ok();
    complete(&mut owner);
    let same = Arc::ptr_eq(input.as_ref().unwrap(), owner.input.as_ref().unwrap());
    let done = ack(owner);
    assert!(entered && busy && blocked && unchanged && retry && same);
    let evidence = evidence.unwrap();
    assert_eq!(
        evidence.maps,
        vec![
            (3, MetadataEffect::Completed),
            (
                9,
                if index {
                    MetadataEffect::Completed
                } else {
                    MetadataEffect::Unknown
                }
            )
        ]
    );
    assert_eq!(
        evidence.last_id,
        if index {
            MetadataEffect::Unknown
        } else {
            MetadataEffect::Unattempted
        }
    );
    assert!(done.failures.iter().any(|failure| {
        failure
            .maps
            .as_ref()
            .is_some_and(|files| files.last_id == evidence.last_id && files.maps == evidence.maps)
    }));
    assert!(game.maps.retained_metadata_test_dirty().is_empty());
}
#[test]
fn remaining_sorted_later_map_failure_keeps_every_original_dirty() {
    remaining_map_failure(false);
}
#[test]
fn remaining_index_failure_retains_completed_map_evidence_and_all_dirty() {
    remaining_map_failure(true);
}

#[test]
fn remaining_map_index_only_clean_maps_and_empty_sparsity() {
    use crate::game::maps::Maps;
    let fixture = Fixture::new("remaining-index-only");
    let mut game = fixture.game();
    game.maps = Maps::retained_metadata_fixture(Some(12), &[], &[3]);
    let before = game.maps.retained_metadata_test_snapshot(false);
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    complete(&mut owner);
    let done = ack(owner);
    assert_eq!(done.maps.unwrap().last_id, MetadataEffect::Completed);
    assert_eq!(game.maps.retained_metadata_test_snapshot(false), before);
    assert!(!game.world_root.join("data/minecraft/maps/3.dat").exists());
    assert!(save::read_dat(&game.world_root.join("data/minecraft/maps/last_id.dat")).is_some());
    game.maps = Maps::default();
    let (_, input) = game.prepare_retained_maps_metadata().unwrap();
    let io = input.write_native();
    assert_eq!(io.effect, MetadataEffect::Skipped);
    assert!(io.result.is_ok());
}

fn remaining_native_failure(family: MetadataFamily) {
    let fixture = Fixture::new("remaining-family-failure");
    let mut game = fixture.game();
    remaining_rich(&mut game);
    let path = match family {
        MetadataFamily::Fight => fight_file(&game).with_file_name("ender_dragon_fight.dat_new"),
        MetadataFamily::Trader => game
            .world_root
            .join("data/minecraft/wandering_trader.dat_new"),
        MetadataFamily::Raids => raid_file(&game).with_file_name("raids.dat_new"),
        MetadataFamily::Arrivals => game.world_root.join("data/horizon_portal_arrivals.dat_new"),
        _ => unreachable!(),
    };
    fs::create_dir_all(&path).unwrap();
    let maps = game.maps.retained_metadata_test_snapshot(false);
    let fight = game.fight.retained_metadata_test_snapshot(false);
    let trader = game.retained_trader_fixture_state();
    let raids = game.retained_raids_fixture_snapshot();
    let arrivals = game.retained_arrivals_fixture_state();
    let (control, receipt) = MetadataTestControl::new(MetadataTestAction::HoldBeforeFamily(family));
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    owner.control = Some(control);
    owner.poll();
    let until = Instant::now() + WAIT;
    while !receipt.entered() && Instant::now() < until {
        owner.poll();
        std::thread::yield_now();
    }
    let entered = receipt.entered();
    owner.deadline = Instant::now();
    owner.poll();
    let busy = owner.retry(Instant::now() + WAIT) == Err(MetadataRetryRefusal::Busy);
    receipt.release();
    reap(&mut owner);
    let blocked = matches!(owner.poll(), MetadataStatus::Blocked(_));
    let effects = owner.effects();
    let unchanged = owner.game.maps.retained_metadata_test_snapshot(false) == maps
        && owner.game.fight.retained_metadata_test_snapshot(false) == fight
        && owner.game.retained_trader_fixture_state() == trader
        && owner.game.retained_raids_fixture_matches(&raids)
        && owner.game.retained_arrivals_fixture_state() == arrivals
        && storage_dirty(owner.game);
    let input = owner.input.clone();
    fs::remove_dir(&path).unwrap();
    let retry = owner.retry(Instant::now() + WAIT).is_ok();
    complete(&mut owner);
    let same = Arc::ptr_eq(input.as_ref().unwrap(), owner.input.as_ref().unwrap());
    let _ = ack(owner);
    assert!(entered && busy && blocked && unchanged && retry && same);
    assert_eq!(effects[family as usize], MetadataEffect::Unknown);
    assert!(
        effects[..family as usize]
            .iter()
            .all(|e| *e == MetadataEffect::Completed)
    );
    assert!(
        effects[family as usize + 1..]
            .iter()
            .all(|e| *e == MetadataEffect::Unattempted)
    );
}
#[test]
fn remaining_fight_native_stall_late_failure_and_fixed_retry() {
    remaining_native_failure(MetadataFamily::Fight);
}
#[test]
fn remaining_trader_native_stall_late_failure_and_fixed_retry() {
    remaining_native_failure(MetadataFamily::Trader);
}
#[test]
fn remaining_raid_native_stall_late_failure_keeps_player_extra() {
    remaining_native_failure(MetadataFamily::Raids);
}
#[test]
fn remaining_late_arrival_failure_keeps_all_fifteen_markers_and_omen() {
    remaining_native_failure(MetadataFamily::Arrivals);
}

#[test]
fn remaining_late_preflight_refusal_leaves_all_fifteen_owners_unacknowledged() {
    let fixture = Fixture::new("remaining-late-preflight");
    let mut game = fixture.game();
    remaining_rich(&mut game);
    let maps = game.maps.retained_metadata_test_snapshot(false);
    let raids = game.retained_raids_fixture_snapshot();
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    complete(&mut owner);
    let cooldown = owner.game.portals.player.cooldown;
    owner.game.portals.player.cooldown += 1;
    let mut owner = match owner.acknowledge() {
        Err(owner) => owner,
        Ok(_) => panic!("changed last family acknowledged"),
    };
    let untouched = storage_dirty(owner.game)
        && owner.game.maps.retained_metadata_test_snapshot(false) == maps
        && owner.game.retained_raids_fixture_matches(&raids);
    owner.game.portals.player.cooldown = cooldown;
    let attempts = owner.next_attempt;
    let retry = owner.retry(Instant::now() + WAIT).is_ok();
    complete(&mut owner);
    let _ = ack(owner);
    assert!(untouched && retry);
    assert_eq!(attempts, 1);
}

#[test]
fn remaining_fresh_unloaded_and_clean_guards_preserve_actual_owners() {
    use crate::game::{Dim, dragon::Fight};
    let fixture = Fixture::new("remaining-guards");
    let mut game = fixture.game();
    game.fight = Fight::retained_metadata_fixture(false, true, true);
    game.set_retained_trader_fixture(false, true);
    game.set_retained_arrivals_fixture(false, false, false);
    game.level.player_extra = None;
    game.seed_retained_raids_fixture(None, true, true, Some([1, 2, 3]));
    let raid = game.retained_raids_fixture_snapshot();
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    complete(&mut owner);
    let done = ack(owner);
    assert_eq!(&done.effects[10..], &[MetadataEffect::Skipped; 5]);
    assert!(game.retained_raids_fixture_matches(&raid));
    assert!(game.level.player_extra.is_none());
    assert!(game.fight.retained_metadata_test_dirty());
    assert_eq!(game.retained_trader_fixture_state()[4], 1);
    game.fight = Fight::retained_metadata_fixture(true, false, true);
    game.set_retained_trader_fixture(true, false);
    game.seed_retained_raids_fixture(Some(Dim::Nether), false, true, Some([1, 2, 3]));
    let (_, fight) = game.prepare_retained_fight_metadata().unwrap();
    assert_eq!(fight.write_native().effect, MetadataEffect::Skipped);
    let (_, trader) = game.prepare_retained_trader_metadata();
    assert_eq!(trader.write_native().effect, MetadataEffect::Skipped);
    let (_, raids) = game.prepare_retained_raids_metadata().unwrap();
    assert_eq!(raids.write_native().effect, MetadataEffect::Skipped);
    assert!(!fight_file(&game).exists());
    assert!(!raid_file(&game).exists());
    assert!(
        !game
            .world_root
            .join("data/minecraft/wandering_trader.dat")
            .exists()
    );
}

#[test]
fn remaining_remote_guard_preserves_native_fight_and_trader_selection() {
    let fixture = Fixture::new("remaining-remote");
    let mut game = fixture.game();
    let (client, server) = horizon_protocol::connection::Pipe::pair();
    game.connect_local(client, "RemainingMetadata").unwrap();
    remaining_rich(&mut game);
    let extra = game.level.player_extra.clone();
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    complete(&mut owner);
    let done = ack(owner);
    assert_eq!(
        done.effects[MetadataFamily::Maps as usize],
        MetadataEffect::Skipped
    );
    assert_eq!(
        done.effects[MetadataFamily::Raids as usize],
        MetadataEffect::Skipped
    );
    assert_eq!(
        done.effects[MetadataFamily::Arrivals as usize],
        MetadataEffect::Skipped
    );
    assert_eq!(
        done.effects[MetadataFamily::Fight as usize],
        MetadataEffect::Completed
    );
    assert_eq!(
        done.effects[MetadataFamily::Trader as usize],
        MetadataEffect::Completed
    );
    assert_eq!(game.maps.retained_metadata_test_dirty(), vec![3, 9]);
    assert!(game.retained_raids_fixture_dirty());
    assert!(game.retained_arrivals_fixture_state().dirty());
    assert_eq!(game.level.player_extra, extra);
    server.shutdown();
    drop(server);
    drop(game.remote.take());
}

#[test]
fn remaining_fight_optional_omission_uses_native_end_file() {
    use crate::game::dragon::Fight;
    let fixture = Fixture::new("remaining-fight-optionals");
    let mut game = fixture.game();
    game.fight = Fight::retained_metadata_fixture(true, true, false);
    let before = game.fight.retained_metadata_test_snapshot(true);
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    complete(&mut owner);
    let _ = ack(owner);
    let root = save::read_dat(&fight_file(&game)).unwrap();
    let data = root.get("data").unwrap();
    for key in [
        "exit_portal_location",
        "respawn_stage",
        "horizon_respawn_crystals",
    ] {
        assert!(data.get(key).is_none());
    }
    assert_eq!(data.get("respawn_time"), Some(&Tag::Int(0)));
    assert_eq!(game.fight.retained_metadata_test_snapshot(true), before);
}

#[test]
fn remaining_dirty_empty_unloaded_arrivals_replace_existing_without_wrapper() {
    let fixture = Fixture::new("remaining-empty-arrivals");
    let mut game = fixture.game();
    game.set_retained_arrivals_fixture(false, false, true);
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    complete(&mut owner);
    let _ = ack(owner);
    let path = game.world_root.join("data/horizon_portal_arrivals.dat");
    let original = save::read_dat(&path).unwrap();
    game.set_retained_arrivals_fixture(true, false, true);
    let before = game.retained_arrivals_fixture_state();
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    complete(&mut owner);
    let done = ack(owner);
    assert_eq!(
        done.effects[MetadataFamily::Arrivals as usize],
        MetadataEffect::Completed
    );
    assert_eq!(
        save::read_dat(&path),
        Some(Tag::compound([("arrivals", Tag::List(Vec::new()))]))
    );
    assert_eq!(
        save::read_dat(&path.with_file_name("horizon_portal_arrivals.dat_old")),
        Some(original)
    );
    assert!(before.runtime_eq(&game.retained_arrivals_fixture_state()));
}

fn remaining_omen_case(case: u8) {
    use crate::game::Dim;
    let fixture = Fixture::new("remaining-omen");
    let mut game = fixture.game();
    let original = match case {
        0 => None,
        1 => Some(Tag::compound([
            ("opaque", Tag::Long(73)),
            ("raid_omen_position", Tag::IntArray(vec![4, 5, 6])),
        ])),
        2 => Some(Tag::String("opaque non-compound".into())),
        3 => Some(Tag::compound([(
            "raid_omen_position",
            Tag::IntArray(vec![4, 5, 6]),
        )])),
        _ => unreachable!(),
    };
    game.level.player_extra = original.clone();
    game.seed_retained_raids_fixture(Some(Dim::Nether), true, case != 3, None);
    let before = game.retained_raids_fixture_snapshot();
    let mut owner = NativeMetadata::begin(&mut game, Instant::now() + WAIT);
    complete(&mut owner);
    let _ = ack(owner);
    assert!(game.retained_raids_fixture_runtime_matches(&before));
    match case {
        0 => assert_eq!(
            game.level.player_extra,
            Some(Tag::compound(std::iter::empty::<(&str, Tag)>()))
        ),
        1 => assert_eq!(
            game.level.player_extra,
            Some(Tag::compound([("opaque", Tag::Long(73))]))
        ),
        2 | 3 => assert_eq!(game.level.player_extra, original),
        _ => unreachable!(),
    }
}
#[test]
fn remaining_loaded_omen_none_extra_creates_empty_compound() {
    remaining_omen_case(0);
}
#[test]
fn remaining_loaded_omen_removal_preserves_opaque_player_keys() {
    remaining_omen_case(1);
}
#[test]
fn remaining_loaded_omen_noncompound_extra_is_unchanged() {
    remaining_omen_case(2);
}
#[test]
fn remaining_unloaded_omen_preserves_existing_player_extra() {
    remaining_omen_case(3);
}

fn remaining_raid_corruption(orphan_map: bool) {
    use crate::game::Dim;
    let fixture = Fixture::new("remaining-raid-corruption");
    let mut game = fixture.game();
    game.seed_retained_raids_fixture(Some(Dim::Nether), true, true, Some([7, 8, 9]));
    let (expected, input) = game.prepare_retained_raids_metadata().unwrap();
    let written = input.write_native().result.unwrap();
    game.corrupt_retained_raids_fixture(orphan_map);
    assert!(game.prepare_retained_raids_metadata().is_err());
    assert!(
        game.preflight_retained_raids_metadata(&expected, &written)
            .is_err()
    );
    assert!(game.retained_raids_fixture_dirty());
}
#[test]
fn remaining_raid_map_only_membership_refuses_without_native_panic() {
    remaining_raid_corruption(true);
}
#[test]
fn remaining_raid_order_only_membership_refuses_without_native_panic() {
    remaining_raid_corruption(false);
}

#[test]
fn remaining_family_foreign_posts_and_route_state_staleness_refuse() {
    let fixture = Fixture::new("remaining-association");
    let mut game = fixture.game();
    remaining_rich(&mut game);
    let (expected, input) = game.prepare_retained_maps_metadata().unwrap();
    let (_, foreign) = game.prepare_retained_maps_metadata().unwrap();
    let actual = input.write_native().result.unwrap();
    let foreign = foreign.write_native().result.unwrap();
    assert!(
        game.preflight_retained_maps_metadata(&expected, &foreign)
            .is_err()
    );
    game.maps.data.get_mut(&3).unwrap().scale += 1;
    assert!(
        game.preflight_retained_maps_metadata(&expected, &actual)
            .is_err()
    );
    game.maps.data.get_mut(&3).unwrap().scale -= 1;
    let removed = game.maps.data.remove(&12).unwrap();
    assert!(
        game.preflight_retained_maps_metadata(&expected, &actual)
            .is_err()
    );
    game.maps.data.insert(12, removed);
    let original = std::mem::replace(
        &mut game.maps,
        crate::game::maps::Maps::retained_metadata_fixture(Some(13), &[9, 3], &[12]),
    );
    assert!(
        game.preflight_retained_maps_metadata(&expected, &actual)
            .is_err()
    );
    game.maps = original;
    let original = std::mem::replace(
        &mut game.maps,
        crate::game::maps::Maps::retained_metadata_fixture(Some(12), &[3], &[9, 12]),
    );
    assert!(
        game.preflight_retained_maps_metadata(&expected, &actual)
            .is_err()
    );
    game.maps = original;
    assert!(
        game.preflight_retained_maps_metadata(&expected, &actual)
            .is_ok()
    );
    let (expected, input) = game.prepare_retained_fight_metadata().unwrap();
    let (_, foreign) = game.prepare_retained_fight_metadata().unwrap();
    let actual = input.write_native().result.unwrap();
    let foreign = foreign.write_native().result.unwrap();
    assert!(
        game.preflight_retained_fight_metadata(&expected, &foreign)
            .is_err()
    );
    assert!(
        game.preflight_retained_fight_metadata(&expected, &actual)
            .is_ok()
    );
    let (expected, input) = game.prepare_retained_trader_metadata();
    let (_, foreign) = game.prepare_retained_trader_metadata();
    let actual = input.write_native().result.unwrap();
    let foreign = foreign.write_native().result.unwrap();
    assert!(
        game.preflight_retained_trader_metadata(&expected, &foreign)
            .is_err()
    );
    assert!(
        game.preflight_retained_trader_metadata(&expected, &actual)
            .is_ok()
    );
    let (expected, input) = game.prepare_retained_arrivals_metadata();
    let (_, foreign) = game.prepare_retained_arrivals_metadata();
    let actual = input.write_native().result.unwrap();
    let foreign = foreign.write_native().result.unwrap();
    assert!(
        game.preflight_retained_arrivals_metadata(&expected, &foreign)
            .is_err()
    );
    assert!(
        game.preflight_retained_arrivals_metadata(&expected, &actual)
            .is_ok()
    );
    let (expected, input) = game.prepare_retained_raids_metadata().unwrap();
    let (_, foreign) = game.prepare_retained_raids_metadata().unwrap();
    let actual = input.write_native().result.unwrap();
    let foreign = foreign.write_native().result.unwrap();
    assert!(
        game.preflight_retained_raids_metadata(&expected, &foreign)
            .is_err()
    );
    let original = game.level.player_extra.take();
    assert!(
        game.preflight_retained_raids_metadata(&expected, &actual)
            .is_err()
    );
    game.level.player_extra = original;
    game.dim = crate::game::Dim::End;
    assert!(
        game.preflight_retained_raids_metadata(&expected, &actual)
            .is_err()
    );
    game.dim = crate::game::Dim::Overworld;
    assert!(
        game.preflight_retained_raids_metadata(&expected, &actual)
            .is_ok()
    );
}
