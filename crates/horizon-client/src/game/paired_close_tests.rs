//! Level/participant component qualification using actual headless Game owners.
//! The runner supplies isolated process paths and pinned resources (plus the
//! prebuilt pair for the two populated-session cases). No close witness is made.
#![cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions,
    target_os = "linux"
))]

use crate::game::{
    Game, NativeLevelInput, NativeLevelIo, NativeLevelPost,
    integrated::transition_ownership_tests::Fixture,
    levels::session_checkpoint::tests::coherent_close_fixture, native_levels_equal,
};
use horizon_client::{containers::BlockEntity, items::ItemStack, save};
use horizon_core::geometry::Vec3;
use horizon_world::nbt::Tag;
use std::fs;

fn local(game: &mut Game<'_>) {
    game.options.dedicated = None;
    game.level.player_uuid = Some([1, 2, 3, 4]);
    game.prepare_close_identity_fields().unwrap();
}

fn run_level(input: NativeLevelInput) -> NativeLevelIo {
    // Only owned immutable inputs cross the boundary. Always join before any
    // assertion, including when the native result is an expected error.
    std::thread::spawn(move || {
        input.capture_native()?;
        Ok::<_, String>(input.write_native())
    })
    .join()
    .expect("native Level worker panicked")
    .expect("native Level capture")
}

fn written(input: NativeLevelInput) -> NativeLevelPost {
    let io = run_level(input);
    assert_eq!(
        io.native_effects[5],
        save::NativeLevelWriteEffect::Completed
    );
    match io.result {
        Ok(post) => post,
        Err(error) => panic!("native Level write: {error}"),
    }
}

fn player_file(game: &Game<'_>) -> Tag {
    let uuid = game.level.player_uuid.unwrap();
    save::read_dat(
        &game
            .world_root
            .join("players/data")
            .join(format!("{}.dat", save::uuid_text(uuid))),
    )
    .unwrap()
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn close_level_native_raid_warden_sync_preserves_raw_fields_and_open_ender() {
    let fixture = Fixture::new("close-level-projection");
    let mut game = fixture.game();
    local(&mut game);
    game.level.player_extra = Some(Tag::compound([
        ("test:opaque", Tag::ByteArray(vec![9, 1, 7])),
        ("raid_omen_position", Tag::IntArray(vec![-1, -2, -3])),
        ("Fire", Tag::Short(-1)),
    ]));
    game.seed_retained_raids_fixture(Some(game.dim), true, true, Some([7, 8, 9]));
    game.sculk.tracker_loaded = true;
    game.sculk.tracker.warning_level = 3;
    game.sculk.tracker.cooldown_ticks = 17;
    let tracker = game.sculk.tracker.to_nbt();
    let mut stack = ItemStack::new("minecraft:diamond_sword", 1);
    stack.damage = 7;
    stack.data_mut().custom_name = Some("Retained native blade".into());
    game.inventory[2] = Some(stack.clone());
    let mut open = vec![None; 27];
    open[4] = Some(stack);
    game.ender_items[1] = Some(ItemStack::new("minecraft:stone", 2));
    game.ender_open = Some((1, 70, 1));
    game.world
        .block_entities
        .insert((1, 70, 1), BlockEntity::Chest(open.clone()));
    let original = game.level.clone();
    let original_ender = game.ender_items.clone();
    let (_, raids) = game.prepare_retained_level_raids_metadata().unwrap();
    let mut projection = game.level.clone();
    raids.apply_to_level(&mut projection);
    let (expected, input) = game
        .prepare_retained_level_metadata_from(projection)
        .unwrap();
    let post = written(input);

    assert!(native_levels_equal(&game.level, &original));
    assert_eq!(game.ender_items, original_ender);
    assert!(game.retained_raids_fixture_dirty());
    let player = player_file(&game);
    assert_eq!(
        player.get("test:opaque"),
        Some(&Tag::ByteArray(vec![9, 1, 7]))
    );
    assert_eq!(
        player.get("raid_omen_position"),
        Some(&Tag::IntArray(vec![7, 8, 9]))
    );
    assert_eq!(player.get("warden_spawn_tracker"), Some(&tracker));
    assert_eq!(player.get("Fire"), Some(&Tag::Short(96)));
    assert_eq!(player.get("Air"), Some(&Tag::Short(250)));
    assert_eq!(player, post.level.player_tag());
    let mut decoded = save::Level::default();
    decoded.read_player(&player);
    assert_eq!(decoded.ender_items, open);
    assert_eq!(
        decoded.inventory_components.get(&2),
        game.inventory[2].as_ref().unwrap().extra.as_ref()
    );
    game.preflight_retained_level_metadata(&expected, &post)
        .unwrap();
    game.commit_retained_level_metadata(post);
    assert_eq!(game.ender_items, open);
    assert_eq!(game.level.player_tag(), player);
    assert_eq!((game.level.time, game.level.day_time), (123, 321));
    assert!(
        game.retained_raids_fixture_dirty(),
        "Level acknowledgment is not raid marker acknowledgment"
    );
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn close_level_equal_payload_foreign_post_and_live_changes_refuse_before_ack() {
    let fixture = Fixture::new("close-level-preflight");
    let mut game = fixture.game();
    local(&mut game);
    game.ui.clock.partial = 0.0;
    let mut stack = ItemStack::new("minecraft:stone", 2);
    stack.data_mut().custom_name = Some("Original raw item owner".into());
    game.inventory[0] = Some(stack.clone());
    let original = game.level.clone();
    let (expected, input) = game.prepare_retained_level_metadata().unwrap();
    let (_, foreign_input) = game.prepare_retained_level_metadata().unwrap();
    let post = written(input);
    let foreign = written(foreign_input);
    assert_eq!(post.level.player_tag(), foreign.level.player_tag());
    assert!(native_levels_equal(&post.level, &foreign.level));
    assert!(
        game.preflight_retained_level_metadata(&expected, &foreign)
            .is_err()
    );
    game.level.name.push_str(" changed");
    assert!(
        game.preflight_retained_level_metadata(&expected, &post)
            .is_err()
    );
    game.level.name = original.name.clone();
    game.inventory[0].as_mut().unwrap().data_mut().custom_name = Some("Changed raw item".into());
    assert!(
        game.preflight_retained_level_metadata(&expected, &post)
            .is_err()
    );
    game.inventory[0] = Some(stack);
    game.ui.clock.partial = -0.0;
    assert_eq!(game.ui.clock.partial, 0.0);
    assert!(
        game.preflight_retained_level_metadata(&expected, &post)
            .is_err()
    );
    game.ui.clock.partial = 0.0;
    game.player.as_mut().unwrap().fire_ticks += 1;
    assert!(
        game.preflight_retained_level_metadata(&expected, &post)
            .is_err()
    );
    game.player.as_mut().unwrap().fire_ticks -= 1;
    assert!(native_levels_equal(&game.level, &original));
    game.preflight_retained_level_metadata(&expected, &post)
        .unwrap();
    game.commit_retained_level_metadata(post);
    assert_eq!(game.level.player_tag(), player_file(&game));
}

#[test]
#[ignore = "isolated pinned resources + prebuilt pair: genuine integrated owner body and idempotent native identity preparation"]
fn close_level_actual_owner_body_wins_and_spawn_identity_is_prepared_once() {
    let fixture = Fixture::new("close-level-owner");
    let mut source = coherent_close_fixture(&fixture, "close-level-owner");
    let game = &mut source.game;
    game.options.dedicated = None;
    game.level.player_uuid = None;
    game.level.world_spawn = None;
    game.spawn = Some(Vec3::new(400.0, 80.0, 400.0));
    let mut owner_ender = vec![None; 27];
    owner_ender[6] = Some(ItemStack::new("minecraft:stone", 5));
    let owner_uuid = game.lan.as_ref().unwrap().owner.as_ref().unwrap().uuid;
    let owner = game
        .lan
        .as_mut()
        .unwrap()
        .hub
        .guests
        .iter_mut()
        .find(|g| g.uuid == owner_uuid)
        .unwrap();
    owner.spawn = Some([23.5, 89.0, -7.5]);
    owner.ender_items = owner_ender.clone();
    let body = owner.motion.as_mut().unwrap();
    body.velocity = Vec3::new(-0.125, 0.5, 0.75);
    body.health.food.exhaustion = 3.25;
    body.health.food.timer = 117;
    let first = game.prepare_close_identity_fields().unwrap();
    let uuid = game.level.player_uuid.unwrap();
    let second = game.prepare_close_identity_fields().unwrap();
    assert_eq!((first.count(), second.count()), (2, 0));
    assert_eq!(game.level.player_uuid, Some(uuid));
    assert_eq!(game.level.world_spawn, Some([23, 89, -8]));
    game.level.player_extra = Some(Tag::compound([("test:stale_local", Tag::Long(71))]));
    game.seed_retained_raids_fixture(Some(game.dim), true, true, Some([7, 8, 9]));
    game.sculk.tracker_loaded = true;
    game.sculk.tracker.warning_level = 4;
    // Participant preflight requires the real server's local player caches and
    // UI owners to be empty. Keep that genuine shape; the owner has its own
    // populated chest while sync's local cache stays empty.
    let local_ender = game.ender_items.clone();
    assert!(local_ender.iter().all(Option::is_none));
    let original = game.level.clone();
    let roster = game.observe_checkpoint_participants().unwrap();
    let rows = game.project_session_participant_rows(&roster).unwrap();
    let actual_owner = rows
        .iter()
        .find(|row| row.integrated_owner)
        .unwrap()
        .player
        .clone();
    let (_, raids) = game.prepare_retained_level_raids_metadata().unwrap();
    let mut projection = game.level.clone();
    raids.apply_to_level(&mut projection);
    let (expected, input) = game
        .prepare_retained_level_metadata_from(projection)
        .unwrap();
    let post = written(input);

    assert!(native_levels_equal(&game.level, &original));
    assert_eq!(player_file(game), actual_owner);
    assert_eq!(post.level.motion, [-0.125, 0.5, 0.75]);
    assert_eq!(post.level.food_exhaustion, 3.25);
    assert_eq!(post.level.food_tick_timer, 117);
    assert_eq!(post.level.ender_items, owner_ender);
    assert_eq!(post.level.player_tag().get("test:stale_local"), None);
    assert_eq!(post.level.player_tag().get("raid_omen_position"), None);
    assert_eq!(post.level.player_tag().get("warden_spawn_tracker"), None);
    game.preflight_retained_level_metadata(&expected, &post)
        .unwrap();
    game.commit_retained_level_metadata(post);
    assert_eq!(game.level.motion, [-0.125, 0.5, 0.75]);
    assert_eq!(
        (game.level.food_exhaustion, game.level.food_tick_timer),
        (3.25, 117)
    );
    assert_eq!(game.level.ender_items, owner_ender);
    assert_eq!(game.ender_items, local_ender);
    assert_eq!(game.level.world_spawn, Some([23, 89, -8]));
    assert_eq!(game.level.player_uuid, Some(uuid));
    assert_eq!(game.lan.as_ref().unwrap().hub.guests.len(), 2);
    assert_eq!(source.receivers.len(), 2);
}

#[test]
#[ignore = "isolated pinned resources + prebuilt pair: actual participant record errors retain original owners and explicit retry uses captured records"]
fn close_level_actual_participant_advancement_error_still_writes_stats_and_level() {
    let fixture = Fixture::new("close-participant-records");
    let mut source = coherent_close_fixture(&fixture, "close-participant-records");
    let game = &mut source.game;
    let advancement = game
        .adv
        .tree
        .list
        .iter()
        .find(|adv| !adv.criteria.is_empty())
        .unwrap();
    let criterion = (advancement.id.clone(), advancement.criteria[0].name.clone());
    {
        let host = game.lan.as_mut().unwrap();
        for guest in &mut host.hub.guests {
            guest
                .advancements
                .grant(&criterion.0, &criterion.1, "2026-10-28 12:00:00 +0000");
            host.play
                .get_mut(&guest.conn)
                .unwrap()
                .stats
                .add("custom", "minecraft:jump", 37);
        }
    }
    game.prepare_close_identity_fields().unwrap();
    let original = game.level.clone();
    let roster = game.observe_checkpoint_participants().unwrap();
    let rows = game.project_session_participant_rows(&roster).unwrap();
    assert_eq!(rows.len(), 2);
    let first = &rows[0];
    let adv_path =
        crate::game::stats::player_json(&game.world_root, "advancements", first.record_uuid);
    let stats_path = crate::game::stats::player_json(&game.world_root, "stats", first.record_uuid);
    fs::remove_file(&adv_path).unwrap();
    fs::create_dir(&adv_path).unwrap();
    fs::remove_file(&stats_path).unwrap();
    fs::create_dir(&stats_path).unwrap();
    let (expected, input) = game.prepare_retained_level_metadata().unwrap();
    let both_failed = run_level(input.clone());
    fs::remove_dir(&stats_path).unwrap();
    let adv_failed = run_level(input.clone());
    let stats: serde_json::Value = serde_json::from_slice(&fs::read(&stats_path).unwrap()).unwrap();
    fs::remove_dir(&adv_path).unwrap();
    let post = written(input);

    let both_error = match both_failed.result {
        Err(error) => error,
        Ok(_) => panic!("both record obstructions must fail"),
    };
    assert!(
        both_error.contains(&adv_path.display().to_string()),
        "{both_error}"
    );
    assert!(
        !both_error.contains(&stats_path.display().to_string()),
        "first error is advancement before stats: {both_error}"
    );
    assert!(adv_failed.result.is_err());
    assert_eq!(
        adv_failed.native_effects[5],
        save::NativeLevelWriteEffect::Completed
    );
    assert_eq!(stats, first.stats);
    assert!(native_levels_equal(&game.level, &original));
    for row in &rows {
        let path = horizon_client::lan::playerdata::path(&game.world_root, row.profile_uuid);
        let path = if row.integrated_owner {
            game.world_root
                .join("players/data")
                .join(format!("{}.dat", save::uuid_text(row.record_uuid)))
        } else {
            path
        };
        assert_eq!(save::read_dat(&path).unwrap(), row.player);
        for (family, expected_json) in [("advancements", &row.advancements), ("stats", &row.stats)]
        {
            let path = crate::game::stats::player_json(&game.world_root, family, row.record_uuid);
            let json: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            assert_eq!(&json, expected_json);
        }
    }
    let conn = rows[0].conn;
    game.lan.as_mut().unwrap().hub.guests[0].conn = conn + 100;
    assert!(
        game.preflight_retained_level_metadata(&expected, &post)
            .is_err()
    );
    game.lan.as_mut().unwrap().hub.guests[0].conn = conn;
    let host = game.lan.as_mut().unwrap();
    let stats_values = host.play[&conn].stats.values.clone();
    host.play
        .get_mut(&conn)
        .unwrap()
        .stats
        .add("custom", "minecraft:jump", 1);
    assert!(
        game.preflight_retained_level_metadata(&expected, &post)
            .is_err()
    );
    game.lan
        .as_mut()
        .unwrap()
        .play
        .get_mut(&conn)
        .unwrap()
        .stats
        .values = stats_values;
    let guest = game
        .lan
        .as_mut()
        .unwrap()
        .hub
        .guests
        .iter_mut()
        .find(|g| g.conn == conn)
        .unwrap();
    let velocity = guest.motion.as_ref().unwrap().velocity;
    guest.motion.as_mut().unwrap().velocity.x += 0.25;
    assert!(
        game.preflight_retained_level_metadata(&expected, &post)
            .is_err()
    );
    game.lan
        .as_mut()
        .unwrap()
        .hub
        .guests
        .iter_mut()
        .find(|g| g.conn == conn)
        .unwrap()
        .motion
        .as_mut()
        .unwrap()
        .velocity = velocity;
    assert!(native_levels_equal(&game.level, &original));
    game.preflight_retained_level_metadata(&expected, &post)
        .unwrap();
    game.commit_retained_level_metadata(post);
    assert_eq!(game.lan.as_ref().unwrap().hub.guests.len(), 2);
    assert_eq!(source.receivers.len(), 2);
}

mod close_owner_cases {
    use super::super::{
        CloseObservation, CloseOwner, ClosePhase, CloseProfile, CloseRetryRefusal, CloseStatus,
        MetadataTestAction, MetadataTestControl, TestCloseCustody,
    };
    use super::*;
    use crate::game::{
        Dim,
        levels::session_checkpoint::{
            CoherentImagePhase, CoherentImageTestAction, CoherentImageTestControl, Publication,
            tests::{
                RetainedCloseFixtureObservation, RetainedCloseFixtureSetup,
                coherent_close_item_total, recover_coherent_close_fixture,
                retained_coherent_close_fixture, retained_native_close_game,
            },
        },
        world_loading::RetainedParkedConstruction,
    };
    use horizon_core::position::ChunkPos;
    use horizon_world::{nbt::Limits, region::RegionFile};
    use std::{
        path::Path,
        sync::Arc,
        time::{Duration, Instant},
    };

    fn until() -> Instant {
        Instant::now() + Duration::from_secs(120)
    }

    fn populated<'a>(fixture: &'a Fixture, label: &str) -> TestCloseCustody<'a> {
        retained_coherent_close_fixture(fixture, label, RetainedCloseFixtureSetup::Healthy, until())
    }

    fn settle(custody: &mut TestCloseCustody<'_>) -> CloseStatus {
        loop {
            let status = custody.poll();
            if !matches!(status, CloseStatus::Pending(_)) {
                return status;
            }
            std::thread::yield_now();
        }
    }

    fn reap(custody: &mut TestCloseCustody<'_>) {
        while custody.io_busy() {
            custody.poll();
            std::thread::yield_now();
        }
    }

    fn wait_metadata(
        custody: &mut TestCloseCustody<'_>,
        receipt: &super::super::MetadataTestReceipt,
    ) -> bool {
        while !receipt.entered() {
            let status = custody.poll();
            if !matches!(status, CloseStatus::Pending(_)) && !custody.io_busy() {
                return false;
            }
            std::thread::yield_now();
        }
        true
    }

    fn wait_image(
        custody: &mut TestCloseCustody<'_>,
        receipt: &crate::game::levels::session_checkpoint::CoherentImageTestReceipt,
    ) -> bool {
        while !receipt.entered() {
            let status = custody.poll();
            if !matches!(status, CloseStatus::Pending(_)) && !custody.io_busy() {
                return false;
            }
            std::thread::yield_now();
        }
        true
    }

    fn native_input(custody: &TestCloseCustody<'_>) -> Option<usize> {
        custody
            .owner
            .as_ref()
            .unwrap()
            .native_input
            .as_ref()
            .map(|input| Arc::as_ptr(input) as usize)
    }

    fn same_rows(
        a: &[crate::game::lan::SessionPlayerRow],
        b: &[crate::game::lan::SessionPlayerRow],
    ) -> bool {
        a.len() == b.len()
            && a.iter().zip(b).all(|(a, b)| {
                a.conn == b.conn
                    && a.profile_uuid == b.profile_uuid
                    && a.record_uuid == b.record_uuid
                    && a.integrated_owner == b.integrated_owner
                    && a.dimension == b.dimension
                    && a.player == b.player
                    && a.advancements == b.advancements
                    && a.stats == b.stats
            })
    }

    fn same_native(
        a: &RetainedCloseFixtureObservation,
        b: &RetainedCloseFixtureObservation,
    ) -> bool {
        a.counter == b.counter
            && a.items == b.items
            && a.dimension == b.dimension
            && a.level_uuid == b.level_uuid
            && a.uuids == b.uuids
            && a.raw == b.raw
            && a.clocks == b.clocks
            && a.random == b.random
            && matches!((&a.players,&b.players), (Ok(a),Ok(b)) if same_rows(a,b))
    }

    fn same_recovered_players(
        a: &[crate::game::lan::SessionPlayerRow],
        b: &[crate::game::lan::SessionPlayerRow],
    ) -> bool {
        // Existing recovery intentionally allocates fresh connection numbers.
        a.len() == b.len()
            && a.iter().all(|a| {
                b.iter().any(|b| {
                    a.profile_uuid == b.profile_uuid
                        && a.record_uuid == b.record_uuid
                        && a.integrated_owner == b.integrated_owner
                        && a.dimension == b.dimension
                        && a.player == b.player
                        && a.advancements == b.advancements
                        && a.stats == b.stats
                })
            })
    }

    fn finish(
        mut custody: TestCloseCustody<'_>,
    ) -> (CloseObservation, RetainedCloseFixtureObservation) {
        let status = settle(&mut custody);
        reap(&mut custody);
        let observation = custody.observe();
        let native = custody.observe_native();
        match custody.try_dispose() {
            Ok(_) => (observation, native),
            Err((retained, error)) => {
                eprintln!(
                    "unexpected retained close failure: {status:?}; {error}; {:?}",
                    retained.failures()
                );
                // Unexpected invariant failure retains the original armed owner;
                // its existing fatal backstop makes this a failed process.
                drop(retained);
                unreachable!()
            }
        }
    }

    #[test]
    #[ignore = "isolated pinned resources + prebuilt pair: genuine retained populated close, held lock and existing image recovery"]
    fn close_owner_populated_native_ready_preserves_owners_and_recovers_verified_image() {
        let fixture = Fixture::new("close-owner-populated");
        let mut custody = populated(&fixture, "close-owner-populated");
        let before = custody.observe_native();
        let root = custody.source_root();
        let request =
            custody.recovery_request(root.with_file_name("close-owner-recovered"), until());
        let status = settle(&mut custody);
        reap(&mut custody);
        let lock_held = save::SessionLock::acquire(&root).is_err();
        let refused = custody.production_disposal_refused();
        let (ready, after) = finish(custody);
        let lock_released = save::SessionLock::acquire(&root).is_ok();
        let (recovered, receipt, receivers) = recover_coherent_close_fixture(&fixture, request);
        let counter = recovered
            .world
            .read_persistent_counter(&receipt.counter)
            .unwrap()
            .counter();
        let items = coherent_close_item_total(&recovered, &receipt.slot);
        let roster = recovered.observe_checkpoint_participants().unwrap();
        let players = recovered.project_session_participant_rows(&roster).unwrap();
        assert_eq!(status, CloseStatus::NativeReady);
        assert!(lock_held && lock_released);
        assert_eq!(refused, Ok(true));
        assert!(same_native(&before, &after));
        assert_eq!((counter, items, receivers.len()), (3, 42, 2));
        assert!(same_recovered_players(
            after.players.as_ref().unwrap(),
            &players
        ));
        assert_eq!(ready.participants, 2);
        assert_eq!(ready.image.as_ref().unwrap().0, receipt.epoch);
        assert!(
            ready
                .image
                .as_ref()
                .unwrap()
                .1
                .join("manifest.json")
                .is_file()
        );
    }

    fn entity_row(root: &Path, dim: Dim, at: (i32, i32)) -> Result<Vec<Tag>, String> {
        let path = save::dimension_dir(root, dim.id())
            .join("entities")
            .join(format!(
                "r.{}.{}.mca",
                at.0.div_euclid(32),
                at.1.div_euclid(32)
            ));
        if !path.try_exists().map_err(|e| e.to_string())? {
            return Ok(Vec::new());
        }
        let mut region = RegionFile::open(path).map_err(|e| e.to_string())?;
        let Some(record) = region
            .read(ChunkPos::new(at.0, at.1))
            .map_err(|e| e.to_string())?
        else {
            return Ok(Vec::new());
        };
        let named = record.nbt(Limits::default()).map_err(|e| e.to_string())?;
        match named.tag.get("Entities") {
            Some(Tag::List(tags)) => Ok(tags.clone()),
            _ => Err("native entity row has no Entities list".into()),
        }
    }

    #[test]
    #[ignore = "isolated pinned resources + prebuilt pair: counterless retained active+parked native owners, off-resident saved falling and fixed-input panic retry"]
    fn close_owner_native_world_active_parked_rich_rows_survive_fixed_retry_and_dispose() {
        let fixture = Fixture::new("close-owner-native-world");
        let mut game = retained_native_close_game(&fixture, "close-owner-native-world");
        let parked = RetainedParkedConstruction::start(&game, Dim::Nether);
        game.add_retained_parked_level(parked).unwrap();
        let mut stack = ItemStack::new("minecraft:diamond_sword", 1);
        stack.data_mut().custom_name = Some("Original active and parked item".into());
        for (dim, at, ordinal) in [(Dim::Overworld, (0, 0), 0), (Dim::Nether, (2, 0), 1)] {
            game.seed_retained_native_level_fixture(dim, at, ordinal, stack.clone())
                .unwrap();
        }
        game.seed_retained_raids_fixture(Some(Dim::Overworld), true, true, Some([7, 8, 9]));
        game.seed_retained_parked_raids_fixture(Dim::Nether, Dim::Nether, true)
            .unwrap();
        let falling = [
            (Dim::Overworld, (100, 100), [71, 72, 73, 74]),
            (Dim::Nether, (-100, 100), [81, 82, 83, 84]),
        ]
        .into_iter()
        .map(|(dim, at, uuid)| {
            let raw = game
                .seed_retained_saved_falling_fixture(dim, at, uuid)
                .unwrap();
            (dim, at, uuid, raw)
        })
        .collect::<Vec<_>>();
        let root = game.world_root.clone();
        let dimensions = game.retained_level_dimensions().unwrap();
        let dimension = game.dim;
        let counterless = game.world.counter_trial_snapshot().is_none()
            && !game.world.has_persistent_guest_counter()
            && game
                .world
                .counter_checkpoint_frame(game.world.game_time)
                .is_err();
        let (control, receipt) = MetadataTestControl::new(MetadataTestAction::HoldThenPanic);
        let mut owner = CloseOwner::begin(game, CloseProfile::NativeWorld, until());
        owner.metadata.control = Some(control);
        while !receipt.entered() {
            if matches!(owner.poll(), CloseStatus::Blocked(_)) && owner.io_handle.is_none() {
                break;
            }
            std::thread::yield_now();
        }
        let entered = receipt.entered();
        let original_input = owner
            .native_input
            .as_ref()
            .map(|input| Arc::as_ptr(input) as usize);
        let identities = owner
            .payloads
            .as_ref()
            .map(|payloads| payloads.entity_uuid_rows());
        receipt.release();
        while owner.io_handle.is_some() {
            owner.poll();
            std::thread::yield_now();
        }
        let failed = owner.poll();
        let retry = owner.retry(until());
        loop {
            if !matches!(owner.poll(), CloseStatus::Pending(_)) {
                break;
            }
            std::thread::yield_now();
        }
        while owner.io_handle.is_some() {
            owner.poll();
            std::thread::yield_now();
        }
        let observation = owner.observe();
        let same_input = original_input
            == owner
                .native_input
                .as_ref()
                .map(|input| Arc::as_ptr(input) as usize);
        let same_identities = identities
            == owner
                .payloads
                .as_ref()
                .map(|payloads| payloads.entity_uuid_rows());
        let expected_rows = owner
            .payloads
            .as_ref()
            .unwrap()
            .levels()
            .iter()
            .flat_map(|level| {
                level
                    .entities()
                    .iter()
                    .map(move |(at, tags)| (level.dimension(), *at, tags.clone()))
            })
            .collect::<Vec<_>>();
        let final_dimensions = owner
            .payloads
            .as_ref()
            .unwrap()
            .levels()
            .iter()
            .map(|level| level.dimension())
            .collect::<Vec<_>>();
        let rows_match = expected_rows
            .iter()
            .all(|(dim, at, tags)| entity_row(&root, *dim, *at).as_ref() == Ok(tags));
        let falling_match = falling.iter().all(|(dim, at, uuid, raw)| {
            entity_row(&root, *dim, *at).is_ok_and(|tags| {
                tags.iter().any(|tag| {
                    tag.get("UUID") == Some(&Tag::IntArray(uuid.to_vec()))
                        && tag.get("fixture:saved_falling") == raw.get("fixture:saved_falling")
                })
            })
        });
        let forced_match = owner
            .payloads
            .as_ref()
            .unwrap()
            .levels()
            .iter()
            .all(|level| level.save().load_forced_chunks() == *level.forced());
        let raids_written = [Dim::Overworld, Dim::Nether].iter().all(|dim| {
            save::read_dat(&save::dimension_dir(&root, dim.id()).join("data/minecraft/raids.dat"))
                .is_some()
        });
        let ready = match owner.try_native_ready() {
            Ok(ready) => ready,
            Err(owner) => {
                drop(owner);
                unreachable!()
            }
        };
        let native_ready = ready.observe().phase;
        match ready.try_dispose_dormant() {
            Ok(_) => {}
            Err((ready, error)) => {
                eprintln!("{error}");
                drop(ready);
                unreachable!()
            }
        }
        assert!(entered && matches!(failed, CloseStatus::Blocked(_)));
        assert_eq!(retry, Ok(()));
        assert!(same_input && original_input.is_some() && same_identities);
        assert!(counterless && rows_match && falling_match && forced_match && raids_written);
        assert_eq!(dimensions, vec![Dim::Overworld, Dim::Nether]);
        assert_eq!(final_dimensions, dimensions);
        assert_eq!(
            (
                observation.dimension,
                observation.levels,
                observation.participants
            ),
            (dimension, 2, 0)
        );
        assert_eq!(native_ready, ClosePhase::NativeReady);
        assert!(observation.failed_attempts > 0);
    }

    fn settle_native(owner: &mut CloseOwner<'_>) -> CloseStatus {
        loop {
            let status = owner.poll();
            if !matches!(status, CloseStatus::Pending(_)) {
                return status;
            }
            std::thread::yield_now();
        }
    }

    fn reap_native(owner: &mut CloseOwner<'_>) {
        while owner.io_handle.is_some() {
            owner.poll();
            std::thread::yield_now();
        }
    }

    #[test]
    #[ignore = "isolated pinned resources + prebuilt pair: actual NativeWorld held/panicking reader, three off-resident malformed families, repairs and foreign-context refusal"]
    fn close_owner_native_reader_deadline_panic_and_each_region_family_repair_keep_original_custody()
     {
        let fixture = Fixture::new("close-owner-native-reader");
        let game = retained_native_close_game(&fixture, "close-owner-native-reader");
        let root = game.world_root.clone();
        let dimension_root = save::dimension_dir(&root, game.dim.id());
        let level_before = fs::read(root.join("level.dat")).unwrap();
        fs::copy(root.join("level.dat"), root.join("level.dat_old")).unwrap();
        let player = root.join("players/data").join(format!(
            "{}.dat",
            save::uuid_text(game.level.player_uuid.unwrap())
        ));
        fs::copy(
            &player,
            player.with_file_name(format!(
                "{}_old",
                player.file_name().unwrap().to_string_lossy()
            )),
        )
        .unwrap();
        let opaque = root.join("fixture-unknown-native.bin");
        let opaque_bytes = b"opaque regular native input: \x00\xff\x19keep exact bytes";
        fs::write(&opaque, opaque_bytes).unwrap();
        let region = dimension_root.join("region/r.9.9.mca");
        let entities = dimension_root.join("entities/r.9.9.mca");
        let poi = dimension_root.join("poi/r.9.9.mca");
        for path in [&region, &entities, &poi] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        let mut malformed = vec![0; 8192];
        malformed[..4].copy_from_slice(&1u32.to_be_bytes()); // Nonzero location inside the header.
        fs::write(&region, &malformed).unwrap();
        let (control, receipt) = CoherentImageTestControl::new(CoherentImageTestAction::HoldRead);
        let mut owner = CloseOwner::begin(game, CloseProfile::NativeWorld, until());
        owner.read_control = Some(control);
        while !receipt.entered() {
            if matches!(owner.poll(), CloseStatus::Blocked(_)) && owner.io_handle.is_none() {
                break;
            }
            std::thread::yield_now();
        }
        let held = receipt.entered();
        owner.deadline = Instant::now();
        owner.metadata.deadline = owner.deadline;
        let expired = owner.poll();
        let busy = owner.retry(until());
        receipt.release();
        reap_native(&mut owner);
        let held_result = owner.poll();
        let (panic_control, panic_receipt) =
            CoherentImageTestControl::new(CoherentImageTestAction::PanicRead);
        owner.read_control = Some(panic_control);
        let panic_retry = owner.retry(until());
        let panic_result = settle_native(&mut owner);
        panic_receipt.release();
        reap_native(&mut owner);
        let panicked = owner
            .failures()
            .iter()
            .any(|error| error.contains("panicked"));
        owner.read_control = None;
        let mut refusals = Vec::new();
        let mut unchanged = true;
        let mut retries = vec![panic_retry];
        for (index, path) in [&region, &entities, &poi].into_iter().enumerate() {
            if index != 0 {
                fs::write(path, &malformed).unwrap();
            }
            retries.push(owner.retry(until()));
            let status = settle_native(&mut owner);
            reap_native(&mut owner);
            refusals.push(status);
            unchanged &= owner.identity_preparations() == 0
                && owner.native_input.is_none()
                && owner.native_read.is_none()
                && fs::read(root.join("level.dat")).unwrap() == level_before;
            // Repair this exact file to a valid empty native region, retaining
            // the artifact and original close owner for the next explicit read.
            fs::write(path, vec![0; 8192]).unwrap();
        }
        retries.push(owner.retry(until()));
        owner.poll(); // Dispatch the real final read, without advancing its result.
        while owner.io_handle.is_some() {
            // Inspect the genuine read witness before poll can also perform the
            // next CPU preparation stage in the same turn.
            owner.harvest();
            std::thread::yield_now();
        }
        let foreign_refused = owner.foreign_native_read_context_refuses_for_test();
        let completed = settle_native(&mut owner);
        reap_native(&mut owner);
        let observation = owner.observe();
        let opaque_preserved = fs::read(&opaque).unwrap() == opaque_bytes;
        let ready = match owner.try_native_ready() {
            Ok(ready) => ready,
            Err(owner) => {
                drop(owner);
                unreachable!()
            }
        };
        match ready.try_dispose_dormant() {
            Ok(_) => {}
            Err((ready, error)) => {
                eprintln!("{error}");
                drop(ready);
                unreachable!()
            }
        }
        assert!(
            held && matches!(expired, CloseStatus::Blocked(_))
                && matches!(held_result, CloseStatus::Blocked(_))
        );
        assert_eq!(busy, Err(CloseRetryRefusal::Busy));
        assert!(matches!(panic_result, CloseStatus::Blocked(_)) && panicked);
        assert!(retries.iter().all(Result::is_ok));
        assert_eq!(refusals.len(), 3);
        assert!(
            refusals
                .iter()
                .all(|status| matches!(status, CloseStatus::Blocked(_)))
        );
        assert!(unchanged && opaque_preserved);
        assert_eq!(foreign_refused, Ok(true));
        assert_eq!(completed, CloseStatus::NativeReady);
        assert!(observation.failed_attempts > 0);
    }

    #[test]
    #[ignore = "isolated pinned resources + prebuilt pair: malformed original backup refuses before preparation, actual repair and explicit durable retry"]
    fn close_owner_malformed_old_backup_refuses_before_identities_and_repairs() {
        let fixture = Fixture::new("close-owner-old-backup");
        let mut custody = retained_coherent_close_fixture(
            &fixture,
            "close-owner-old-backup",
            RetainedCloseFixtureSetup::MalformedOldLevelBackup,
            until(),
        );
        let before = custody.observe_native();
        let root = custody.source_root();
        let level = fs::read(root.join("level.dat")).unwrap();
        let failed = settle(&mut custody);
        reap(&mut custody);
        let refused = custody.observe();
        let unchanged = same_native(&before, &custody.observe_native())
            && fs::read(root.join("level.dat")).unwrap() == level;
        fs::copy(root.join("level.dat"), root.join("level.dat_old")).unwrap();
        let retry = custody.retry(until());
        let (_, after) = finish(custody);
        assert!(matches!(failed, CloseStatus::Blocked(_)));
        assert_eq!(refused.identity_preparations, 0);
        assert!(unchanged);
        assert_eq!(retry, Ok(()));
        assert!(same_native(&before, &after));
    }

    #[test]
    #[ignore = "isolated pinned resources + prebuilt pair: actual held metadata worker, absolute deadline Busy, late result and explicit retry"]
    fn close_owner_metadata_deadline_keeps_busy_worker_and_requires_explicit_retry() {
        let fixture = Fixture::new("close-owner-metadata-deadline");
        let mut custody = populated(&fixture, "close-owner-metadata-deadline");
        let (control, receipt) = MetadataTestControl::new(MetadataTestAction::HoldBeforeIo);
        custody.metadata_control(control);
        let entered = wait_metadata(&mut custody, &receipt);
        let before = custody.observe_native();
        let input = native_input(&custody);
        custody.expire_deadline_for_test();
        let expired = custody.poll();
        let busy = custody.retry(until());
        receipt.release();
        reap(&mut custody);
        let late = custody.poll();
        let late_native = custody.observe_native();
        let retry = custody.retry(until());
        let same_input = input == native_input(&custody);
        let (ready, after) = finish(custody);
        assert!(
            entered
                && matches!(expired, CloseStatus::Blocked(_))
                && matches!(late, CloseStatus::Blocked(_))
        );
        assert_eq!(busy, Err(CloseRetryRefusal::Busy));
        assert_eq!(retry, Ok(()));
        assert!(same_input && input.is_some());
        assert!(same_native(&before, &late_native) && same_native(&before, &after));
        assert_eq!(ready.phase, ClosePhase::NativeReady);
    }

    fn dispatch_refusal(action: MetadataTestAction, label: &str) {
        let fixture = Fixture::new(label);
        let mut custody = populated(&fixture, label);
        let before = custody.observe_native();
        let (control, receipt) = MetadataTestControl::new(action);
        custody.metadata_control(control);
        let failed = settle(&mut custody);
        receipt.release();
        reap(&mut custody);
        let refused = custody.observe();
        let unchanged = same_native(&before, &custody.observe_native());
        let failures = custody.failures().to_vec();
        let retry = custody.retry(until());
        let (ready, after) = finish(custody);
        assert!(matches!(failed, CloseStatus::Blocked(_)) && unchanged);
        assert_eq!(refused.identity_preparations, 0);
        assert_eq!(retry, Ok(()));
        assert!(same_native(&before, &after));
        assert!(failures.iter().any(|error| error.contains(
            if action == MetadataTestAction::RefuseSpawn {
                "spawn refusal"
            } else {
                "dispatch disconnect"
            }
        )));
        if action == MetadataTestAction::DisconnectDispatch {
            assert!(ready.failed_attempts > 0);
        }
    }

    #[test]
    #[ignore = "isolated pinned resources + prebuilt pair: genuine dispatch spawn refusal keeps original close owner"]
    fn close_owner_actual_spawn_refusal_retains_owner_and_retries() {
        dispatch_refusal(MetadataTestAction::RefuseSpawn, "close-owner-spawn-refusal");
    }

    #[test]
    #[ignore = "isolated pinned resources + prebuilt pair: registered actual handle is reaped after send disconnect"]
    fn close_owner_send_disconnect_reaps_actual_handle_and_retries() {
        dispatch_refusal(
            MetadataTestAction::DisconnectDispatch,
            "close-owner-disconnect",
        );
    }

    #[test]
    #[ignore = "isolated pinned resources + prebuilt pair: held pre-rename image, deadline Busy and exact late verified success requires explicit retry"]
    fn close_owner_image_late_verified_success_requires_explicit_same_attempt_retry() {
        let fixture = Fixture::new("close-owner-image-late");
        let mut custody = populated(&fixture, "close-owner-image-late");
        let (control, receipt) =
            CoherentImageTestControl::new(CoherentImageTestAction::HoldBeforeRename);
        custody.image_control(control);
        let entered = wait_image(&mut custody, &receipt);
        let before = custody.observe_native();
        let attempt = custody.observe().image_attempt;
        custody.expire_deadline_for_test();
        let expired = custody.poll();
        let busy = custody.retry(until());
        receipt.release();
        reap(&mut custody);
        let late = custody.poll();
        let written = custody.observe();
        let retry = custody.retry(until());
        let (ready, after) = finish(custody);
        let attempt = attempt.unwrap();
        assert!(
            entered
                && matches!(expired, CloseStatus::Blocked(_))
                && matches!(late, CloseStatus::Blocked(_))
        );
        assert_eq!(busy, Err(CloseRetryRefusal::Busy));
        assert_eq!(retry, Ok(()));
        assert_eq!(attempt.phase, CoherentImagePhase::BeforePromotion);
        assert_eq!(attempt.publication, Publication::Uncertain);
        assert_eq!(written.image.as_ref().unwrap().0, attempt.epoch);
        assert_eq!(ready.image.as_ref().unwrap().0, attempt.epoch);
        assert!(ready.prior_images.is_empty());
        assert!(same_native(&before, &after));
        assert!(attempt.published.join("manifest.json").is_file() && !attempt.pending.exists());
    }

    fn image_panic(action: CoherentImageTestAction, label: &str) {
        let fixture = Fixture::new(label);
        let mut custody = populated(&fixture, label);
        let (control, receipt) = CoherentImageTestControl::new(action);
        custody.image_control(control);
        let failed = settle(&mut custody);
        receipt.release();
        reap(&mut custody);
        let before = custody.observe_native();
        let old = custody.observe().image_attempt;
        let input = native_input(&custody);
        let retry = custody.retry(until());
        let same_input = input == native_input(&custody);
        let (ready, after) = finish(custody);
        let old = old.unwrap();
        let old_root = if action == CoherentImageTestAction::PanicBeforeRename {
            &old.pending
        } else {
            &old.published
        };
        let new_root = &ready.image.as_ref().unwrap().1;
        let mut old_manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(old_root.join("manifest.json")).unwrap()).unwrap();
        let mut new_manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(new_root.join("manifest.json")).unwrap()).unwrap();
        old_manifest.as_object_mut().unwrap().remove("epoch");
        new_manifest.as_object_mut().unwrap().remove("epoch");
        assert!(matches!(failed, CloseStatus::Blocked(_)));
        assert_eq!(retry, Ok(()));
        assert!(same_input && input.is_some() && same_native(&before, &after));
        assert_eq!(old.publication, Publication::Uncertain);
        assert_eq!(
            old.phase,
            if action == CoherentImageTestAction::PanicBeforeRename {
                CoherentImagePhase::BeforePromotion
            } else {
                CoherentImagePhase::Promoted
            }
        );
        assert!(ready.image.as_ref().unwrap().0 > old.epoch);
        assert_eq!(ready.prior_images.len(), 1);
        assert_eq!(ready.prior_images[0].epoch, old.epoch);
        assert!(old_root.is_dir() && new_root.is_dir());
        assert_ne!(old_root, new_root);
        assert_eq!(
            old_manifest, new_manifest,
            "fresh epoch keeps the exact fixed native/item/player image"
        );
        assert!(ready.failed_attempts > 0);
    }

    #[test]
    #[ignore = "isolated pinned resources + prebuilt pair: pre-rename panic retains pending artifact and fixed image retries at fresh epoch"]
    fn close_owner_image_panic_before_rename_retains_pending_and_retries_fresh_epoch() {
        image_panic(
            CoherentImageTestAction::PanicBeforeRename,
            "close-owner-image-panic-before",
        );
    }

    #[test]
    #[ignore = "isolated pinned resources + prebuilt pair: post-rename panic retains published uncertainty and fixed image retries at fresh epoch"]
    fn close_owner_image_panic_after_rename_retains_published_and_retries_fresh_epoch() {
        image_panic(
            CoherentImageTestAction::PanicPromoted,
            "close-owner-image-panic-after",
        );
    }

    #[test]
    #[ignore = "fatal child only: external runner requires CLOSE_OWNER_FATAL_ARMED then actual unresolved-owner abort, never fallback 91"]
    fn close_owner_fatal_child_unresolved_original_owner_aborts() {
        use std::io::Write;
        let fixture = Fixture::new("close-owner-fatal");
        let game = retained_native_close_game(&fixture, "close-owner-fatal");
        let owner = CloseOwner::begin(game, CloseProfile::NativeWorld, until());
        let _ = writeln!(std::io::stderr().lock(), "CLOSE_OWNER_FATAL_ARMED");
        let _ = std::io::stderr().flush();
        drop(owner);
        std::process::exit(91);
    }
}
