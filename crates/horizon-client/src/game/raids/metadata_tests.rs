//! Real Game save producers; executed only with pinned resources and owned homes.
use super::Game;
use crate::game::integrated::transition_ownership_tests::Fixture;
use horizon_client::{
    items::ItemStack,
    save::{SaveDir, read_dat},
};
use horizon_world::nbt::Tag;
use std::{
    collections::HashSet,
    fs,
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

fn seed_raids(game: &mut Game<'_>) -> Tag {
    game.raiding.loaded = Some(game.dim);
    let (id, _) = game
        .raiding
        .raids
        .create_or_extend([19, 71, 23], 2, Some(2), 73);
    let raid = game.raiding.raids.get_mut(id).unwrap();
    raid.ticks_active = 1234;
    raid.groups_spawned = 2;
    raid.total_health = 47.5;
    raid.add_hero([11, 22, 33, 44]);
    game.raiding.raids.tick = 91;
    game.raiding.raids.dirty = true;
    game.raiding.omen_loaded = true;
    game.raiding.omen_position = Some([9, 72, 11]);
    expected_tag(game)
}

fn expected_tag(game: &Game<'_>) -> Tag {
    Tag::compound([
        ("data", game.raiding.raids.to_nbt()),
        ("DataVersion", Tag::Int(horizon_world::DATA_VERSION)),
    ])
}

fn obstruct_raids(game: &Game<'_>) -> PathBuf {
    let target = game.raids_path(game.dim).with_file_name("raids.dat_new");
    fs::create_dir_all(&target).unwrap();
    target
}

fn assert_raid_error(game: &Game<'_>, error: &str) {
    assert!(
        error.contains(&game.raids_path(game.dim).display().to_string()),
        "{error}"
    );
}

fn seed_entity(game: &mut Game<'_>) -> Vec<Tag> {
    let mut stack = ItemStack::new("minecraft:cobblestone", 7);
    stack.data_mut().custom_name = Some("raid-save owner".into());
    stack.data_mut().set_other(
        "test:opaque",
        Some(Tag::compound([("value", Tag::Long(57))])),
    );
    game.spawn_item(stack, [2.5, 70.0, 2.5], [0.125, 0.25, -0.125], 40);
    let id = game.dropped.last().unwrap().id;
    game.persist.uuids.insert(id, [41, 42, 43, 44]);
    game.persist
        .raw
        .insert(id, Tag::compound([("opaque_entity", Tag::Int(67))]));
    let chunks = game.entity_chunks(&HashSet::from([(0, 0)]), false);
    assert_eq!(chunks.len(), 1);
    let expected = chunks.into_iter().next().unwrap().1;
    assert_eq!(
        expected.len(),
        1,
        "nonempty native entity snapshot required"
    );
    expected
}

fn assert_disk_world(game: &Game<'_>, expected_entities: &[Tag]) {
    // A new reader cannot satisfy this from Game.save's retained pending values.
    let disk = SaveDir::new(game.save.root.clone());
    let chunk = disk
        .load(0, 0, game.pack.assets.registry, game.res.biomes())
        .expect("queued chunk reached disk");
    assert_eq!(chunk.block(1, 70, 1), game.world.block(1, 70, 1));
    assert_eq!(disk.load_entities(0, 0), expected_entities);
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn raid_metadata_write_error_retains_dirty_and_exact_state_for_repair() {
    let fixture = Fixture::new("raid-writer-repair");
    let mut game = fixture.game();
    let expected = seed_raids(&mut game);
    let obstruction = obstruct_raids(&game);
    let error = game.save_raids().unwrap_err().to_string();
    assert_raid_error(&game, &error);
    assert_eq!(expected_tag(&game), expected);
    assert!(game.raiding.raids.dirty);
    assert!(!game.raids_path(game.dim).is_file());
    assert_eq!(
        game.level
            .player_extra
            .as_ref()
            .and_then(|p| p.get("raid_omen_position")),
        Some(&Tag::IntArray(vec![9, 72, 11]))
    );

    fs::remove_dir(obstruction).unwrap();
    game.raiding.omen_position = None;
    game.save_raids().unwrap();
    assert!(!game.raiding.raids.dirty);
    assert_eq!(read_dat(&game.raids_path(game.dim)), Some(expected.clone()));
    assert_eq!(expected_tag(&game), expected);
    assert!(
        game.level
            .player_extra
            .as_ref()
            .unwrap()
            .get("raid_omen_position")
            .is_none()
    );
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn raid_metadata_unloaded_clean_and_remote_guards_do_not_write() {
    for guard in ["unloaded", "clean", "remote"] {
        let fixture = Fixture::new(&format!("raid-guard-{guard}"));
        let mut game = fixture.game();
        let peer = if guard == "remote" {
            assert!(horizon_client::auth::current().is_none());
            let (client, peer) = horizon_protocol::connection::Pipe::pair();
            game.connect_local(client, "RaidMetadataFixture").unwrap();
            peer.shutdown();
            assert!(game.remote.is_some());
            Some(peer)
        } else {
            None
        };
        let expected = seed_raids(&mut game);
        if guard == "unloaded" {
            game.raiding.loaded = None;
        }
        if guard == "clean" {
            game.raiding.raids.dirty = false;
        }
        let dirty = game.raiding.raids.dirty;
        let extra = game.level.player_extra.clone();
        game.save_raids().unwrap();
        assert_eq!(expected_tag(&game), expected);
        assert_eq!(game.raiding.raids.dirty, dirty);
        assert_eq!(game.level.player_extra, extra);
        assert!(!game.raids_path(game.dim).exists());
        assert!(
            !game
                .raids_path(game.dim)
                .with_file_name("raids.dat_new")
                .exists()
        );
        drop(game.remote.take());
        drop(peer);
    }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn raid_metadata_synchronous_error_still_finishes_queued_chunk_and_entities() {
    let fixture = Fixture::new("raid-sync-queued");
    let mut game = fixture.game();
    let expected = seed_raids(&mut game);
    let entities = seed_entity(&mut game);
    let obstruction = obstruct_raids(&game);
    let error = game.save_level(true).unwrap_err().to_string();
    assert_raid_error(&game, &error);
    assert_eq!(expected_tag(&game), expected);
    assert!(game.raiding.raids.dirty);
    assert_disk_world(&game, &entities);
    assert!(game.save.writes_finished().unwrap());
    assert_eq!(game.dropped.len(), 1);

    fs::remove_dir(obstruction).unwrap();
    game.save_level(true).unwrap();
    assert!(!game.raiding.raids.dirty);
    assert_eq!(read_dat(&game.raids_path(game.dim)), Some(expected));
    assert_disk_world(&game, &entities);
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn raid_metadata_error_precedes_simultaneous_sticky_entity_writer_failure() {
    let fixture = Fixture::new("raid-sync-two-errors");
    let mut game = fixture.game();
    let expected = seed_raids(&mut game);
    let entities = seed_entity(&mut game);
    let _raid_obstruction = obstruct_raids(&game);
    fs::create_dir_all(&game.save.root).unwrap();
    fs::write(
        game.save.root.join("entities"),
        b"owned entity-folder obstruction",
    )
    .unwrap();
    let error = game.save_level(true).unwrap_err().to_string();
    assert_raid_error(&game, &error);
    assert!(game.raiding.raids.dirty);
    assert_eq!(expected_tag(&game), expected);
    let writer_error = game.save.flush().unwrap_err().to_string();
    assert!(writer_error.contains("entities"), "{writer_error}");
    assert!(game.save.writes_finished().is_err());
    // Pending remains readable, but the independent reader proves it is not disk data.
    assert_eq!(game.save.load_entities(0, 0), entities);
    let disk = SaveDir::new(game.save.root.clone());
    assert!(disk.load_entities(0, 0).is_empty());
    assert!(
        disk.load(0, 0, game.pack.assets.registry, game.res.biomes())
            .is_some()
    );
    assert_eq!(game.dropped.len(), 1);
    // Do not clear the writer's sticky error or claim repair succeeded here.
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn raid_metadata_incremental_error_keeps_pending_save_and_repair_completes() {
    let fixture = Fixture::new("raid-incremental-repair");
    let mut game = fixture.game();
    let expected = seed_raids(&mut game);
    let entities = seed_entity(&mut game);
    let obstruction = obstruct_raids(&game);
    let (reply, receiver) = mpsc::channel();
    game.begin_control_save(true, reply).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let error = loop {
        match game.poll_closing_save() {
            Err(error) => break error.to_string(),
            Ok(can_exit) => assert!(!can_exit),
        }
        assert!(game.pending_save.is_some(), "failed save cannot complete");
        assert!(
            Instant::now() < deadline,
            "incremental save did not reach raid writer"
        );
        std::thread::yield_now();
    };
    assert_raid_error(&game, &error);
    assert!(game.pending_save.is_some());
    assert_eq!(expected_tag(&game), expected);
    assert!(game.raiding.raids.dirty);
    let reply: serde_json::Value =
        serde_json::from_str(&receiver.try_recv().expect("error reply")).unwrap();
    assert!(reply.get("ok").is_none());
    assert!(reply["error"].as_str().unwrap().contains("raids.dat"));
    assert!(matches!(
        receiver.try_recv(),
        Err(mpsc::TryRecvError::Disconnected)
    ));

    fs::remove_dir(obstruction).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while game.pending_save.is_some() {
        assert!(!game.poll_closing_save().unwrap());
        assert!(
            Instant::now() < deadline,
            "repaired incremental save did not complete"
        );
        std::thread::yield_now();
    }
    assert!(!game.raiding.raids.dirty);
    assert_eq!(read_dat(&game.raids_path(game.dim)), Some(expected));
    assert_disk_world(&game, &entities);
    // The error consumed its reply sender; successful retry does not fabricate
    // a second response on that old channel. A new request gets a fresh success.
    assert!(matches!(
        receiver.try_recv(),
        Err(mpsc::TryRecvError::Disconnected)
    ));
    let (reply, receiver) = mpsc::channel();
    game.begin_control_save(false, reply).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while game.pending_save.is_some() {
        assert!(!game.poll_closing_save().unwrap());
        assert!(
            Instant::now() < deadline,
            "fresh control save did not complete"
        );
        std::thread::yield_now();
    }
    let reply: serde_json::Value = serde_json::from_str(&receiver.try_recv().unwrap()).unwrap();
    assert_eq!(reply["ok"], true);
    assert!(reply.get("error").is_none());
}
