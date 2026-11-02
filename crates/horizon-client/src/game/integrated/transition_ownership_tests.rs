//! Actual headless Game transitions. The coordinator runs these from an owned,
//! isolated process cwd/home/temp; this module changes no process-wide settings.
#[cfg(feature = "paired-packages")]
mod slot_player_tests;
#[cfg(feature = "paired-packages")]
mod slot_retirement_tests;
use super::super::{
    Dim, Game, Options, PackResources, Progress, Resources, boats, execution_role, load_resources,
    minecarts, mod_packages,
};
use super::server_game;
use horizon_client::{
    chunk::Chunk,
    items::ItemStack,
    motion::PlayerMotion,
    save::{SaveDir, dimension_dir},
    settings::Settings,
    world::World,
};
use horizon_core::geometry::Vec3;
use horizon_world::nbt::Tag;
use std::{
    collections::HashSet,
    fs,
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

struct Shared {
    root: PathBuf,
    resources: Arc<Resources>,
    pack: PackResources,
}

fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    let root = PathBuf::from(
        std::env::var_os("HORIZON_TEST_RESOURCES").expect("requires pinned HORIZON_TEST_RESOURCES"),
    );
    let shared = SHARED.get_or_init(|| {
        let loaded = load_resources(
            root.clone(),
            &root,
            &Settings::default(),
            Instant::now(),
            false,
            &Progress::new(),
            &mut |_, _| {},
        )
        .expect("load pinned headless Game resources");
        Shared {
            root: root.clone(),
            resources: loaded.resources,
            pack: loaded.pack,
        }
    });
    assert_eq!(
        shared.root, root,
        "resource root is fixed for this test process"
    );
    shared
}

pub(in crate::game) struct Fixture {
    root: PathBuf,
    shared: &'static Shared,
}

impl Fixture {
    pub(in crate::game) fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "horizon-transition-{}-{unique}-{label}",
            std::process::id()
        ));
        assert!(!root.exists(), "fresh owned scratch path required");
        fs::create_dir_all(&root).unwrap();
        Self {
            root,
            shared: shared(),
        }
    }

    pub(in crate::game) fn game(&self) -> Game<'_> {
        // Deliberately explicit: Options::read consumes test-runner arguments
        // and can inherit session/account configuration.
        let options = Options {
            resources: self.shared.root.clone(),
            distance: 2,
            distance_override: Some(2),
            seed: 17,
            creative: false,
            test_terrain: false,
            frames: None,
            world_frames: None,
            screenshot_after: None,
            timeout: Duration::from_secs(120),
            report: None,
            world: None,
            look: None,
            time: None,
            weather: None,
            dimension: None,
            no_pause: false,
            control: None,
            spikes: None,
            offscreen: false,
            window_size: [1280, 720],
            server: None,
            player_name: Some("TransitionFixture".into()),
            dedicated: Some(Vec::new()),
            benchmark: None,
            dev_mods: Vec::new(),
            mod_packages: mod_packages::Packages::default(),
            server_thread: false,
            execution_role: execution_role::ExecutionRole::IntegratedServer,
        };
        let mut game = server_game(
            &self.shared.resources,
            self.shared.pack.clone(),
            options,
            Settings::default(),
        )
        .expect("construct actual headless Game");
        game.world_root = self.root.join("world");
        game.dim = Dim::Overworld;
        game.level.dimension = Dim::Overworld.id().into();
        game.level.seed = 17;
        game.save = Some(Arc::new(SaveDir::new(dimension_dir(
            &game.world_root,
            game.dim.id(),
        ))));
        game.world = World::new(game.res.facts().air, 17);
        game.world
            .chunks
            .insert((0, 0), Arc::new(Chunk::empty(0, 0)));
        game.world.game_time = 123;
        game.world.day_time = 321;
        let stone = game
            .pack
            .assets
            .registry
            .default_state("minecraft:stone")
            .unwrap();
        game.world.set(1, 70, 1, stone).unwrap();
        let mut player = PlayerMotion::new(game.res.motion(), Vec3::new(8.0, 72.0, 8.0)).unwrap();
        player.velocity = Vec3::new(0.125, -0.25, 0.375);
        player.yaw = -42.5;
        player.fall_distance = 9.5;
        player.fire_ticks = 96;
        player.air = 250;
        game.player = Some(player);
        game
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Games borrow this fixture and are dropped first, joining their writers.
        // Cleanup must not mask an assertion failure.
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn rich_stack() -> ItemStack {
    let mut stack = ItemStack::new("minecraft:diamond_sword", 1);
    stack.damage = 7;
    stack.data_mut().custom_name = Some("Departure blade".into());
    stack.data_mut().other.push((
        "test:opaque_component".into(),
        Tag::compound([("value", Tag::Long(73))]),
    ));
    stack
}

fn unknown(label: &str) -> Tag {
    // No Pos: retain the opaque record in EntityMemory's chunk-keyed unknown
    // pool through the ordinary loaded-entity ingestion path.
    Tag::compound([
        ("id", Tag::String("test:future_entity".into())),
        (
            "opaque",
            Tag::compound([("label", Tag::String(label.into()))]),
        ),
    ])
}

fn orb(position: Vec3, uuid: [i32; 4]) -> Tag {
    Tag::compound([
        ("id", Tag::String("minecraft:experience_orb".into())),
        ("UUID", Tag::IntArray(uuid.to_vec())),
        (
            "Pos",
            Tag::List(
                [position.x, position.y, position.z]
                    .map(|v| Tag::Double(v.to_bits()))
                    .to_vec(),
            ),
        ),
        ("Value", Tag::Short(7)),
        ("Count", Tag::Int(1)),
        ("Age", Tag::Short(3)),
        ("opaque", Tag::compound([("orb", Tag::Long(91))])),
    ])
}

fn seed_entities(game: &mut Game<'_>) -> u32 {
    game.spawn_item(rich_stack(), [2.5, 70.0, 2.5], [0.125, 0.25, -0.125], 40);
    let id = game.dropped.last().unwrap().id;
    game.persist.uuids.insert(id, [11, 22, 33, 44]);
    game.persist
        .raw
        .insert(id, Tag::compound([("opaque", Tag::Long(1234))]));
    let mut boat =
        boats::Boat::new(101, Arc::from("oak"), Vec3::new(4.0, 70.0, 4.0), -42.0).with_chest();
    boat.items[2] = Some(rich_stack());
    game.boats.push(boat);
    game.persist.uuids.insert(101, [51, 52, 53, 54]);
    game.persist
        .raw
        .insert(101, Tag::compound([("opaque", Tag::Long(5678))]));
    game.world.loaded_entities.push((
        (0, 0),
        vec![
            unknown("selected"),
            orb(Vec3::new(9.5, 71.0, 9.5), [61, 62, 63, 64]),
        ],
    ));
    game.spawn_loaded_entities();
    assert_eq!(game.orbs.len(), 1, "seed must reach native orb storage");
    id
}

fn snapshot(game: &mut Game<'_>, key: (i32, i32)) -> Vec<Tag> {
    let mut chunks = game.entity_chunks(&HashSet::from([key]), false);
    assert_eq!(chunks.len(), 1);
    chunks.pop().unwrap().1
}

fn assert_same_records(actual: &[Tag], expected: &[Tag]) {
    assert_eq!(actual.len(), expected.len());
    for tag in expected {
        assert_eq!(
            actual.iter().filter(|candidate| *candidate == tag).count(),
            1,
            "each exact saved record, including UUID/components/opaque fields, must survive: {tag:?}"
        );
    }
}

fn assert_player_retained(game: &Game<'_>) {
    let player = game
        .player
        .as_ref()
        .expect("same taken player restored on error");
    assert_eq!(player.position, Vec3::new(8.0, 72.0, 8.0));
    assert_eq!(player.velocity, Vec3::new(0.125, -0.25, 0.375));
    assert_eq!(player.yaw, -42.5);
    assert_eq!(player.fall_distance, 9.5);
    assert_eq!((player.fire_ticks, player.air), (96, 250));
    assert!(game.travelling.is_none());
    assert_eq!(game.dim, Dim::Overworld);
    assert_eq!(game.level.dimension, Dim::Overworld.id());
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn transition_ownership_entity_io_failure_retains_native_player_and_retry_snapshot() {
    let fixture = Fixture::new("entity-io");
    let mut game = fixture.game();
    let id = seed_entities(&mut game);
    let expected = snapshot(&mut game, (0, 0));
    assert_eq!(expected.len(), 4);
    let uuids = game.persist.uuids.clone();
    let raw = game.persist.raw.clone();
    let progress = game.adv.progress.criteria.clone();
    fs::create_dir_all(&game.save.as_ref().unwrap().root).unwrap();
    let obstruction = game.save.as_ref().unwrap().root.join("entities");
    fs::write(&obstruction, b"owned entity-folder obstruction").unwrap();

    let error = game.enter_dimension(Dim::Nether).unwrap_err().to_string();
    assert!(error.contains("entities"), "{error}");
    assert_player_retained(&game);
    assert_eq!(game.adv.progress.criteria, progress);
    assert!(game.world.chunks.contains_key(&(0, 0)));
    assert_eq!(game.dropped[0].id, id);
    assert_eq!(game.dropped[0].stack, rich_stack());
    assert_eq!(game.boats[0].items[2], Some(rich_stack()));
    assert_eq!(game.persist.uuids, uuids);
    assert_eq!(game.persist.raw, raw);
    assert_same_records(&snapshot(&mut game, (0, 0)), &expected);
    // Disk failure already accepted a pending snapshot. It is not an empty delete.
    assert_same_records(&game.save.as_ref().unwrap().load_entities(0, 0), &expected);

    fs::remove_file(obstruction).unwrap();
    // A complete switch retry can stop at its earlier sticky writer barrier.
    // Retry the actual entity producer, without clearing or bypassing that error.
    assert!(game.save_dimension_entities().is_err());
    assert!(
        game.save.as_ref().unwrap().flush().is_err(),
        "writer failure remains sticky"
    );
    let disk = SaveDir::new(game.save.as_ref().unwrap().root.clone());
    assert_same_records(&disk.load_entities(0, 0), &expected);
    assert_same_records(&snapshot(&mut game, (0, 0)), &expected);
    assert_eq!(game.persist.uuids, uuids);
    assert_eq!(game.persist.raw, raw);
    assert_player_retained(&game);
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn transition_ownership_target_failure_retains_then_success_commits() {
    let fixture = Fixture::new("target-preparation");
    let mut game = fixture.game();
    seed_entities(&mut game);
    let expected = snapshot(&mut game, (0, 0));
    let uuids = game.persist.uuids.clone();
    let raw = game.persist.raw.clone();
    let progress = game.adv.progress.criteria.clone();
    game.world.modified.insert((99, 99));
    let source = game.world.chunks.get(&(0, 0)).unwrap().clone();
    game.options.resources = fixture.root.join("missing-target-resources");
    assert!(!game.options.resources.exists());

    let error = game.enter_dimension(Dim::Nether).unwrap_err().to_string();
    assert!(
        error.contains("worldgen"),
        "actual target preparation must fail: {error}"
    );
    assert_player_retained(&game);
    assert!(Arc::ptr_eq(
        game.world.chunks.get(&(0, 0)).unwrap(),
        &source
    ));
    assert_eq!(game.adv.progress.criteria, progress);
    assert_eq!(game.persist.uuids, uuids);
    assert_eq!(game.persist.raw, raw);
    assert_same_records(&snapshot(&mut game, (0, 0)), &expected);
    let disk = SaveDir::new(game.save.as_ref().unwrap().root.clone());
    assert_same_records(&disk.load_entities(0, 0), &expected);
    assert_eq!(
        game.world.modified,
        HashSet::from([(99, 99)]),
        "successful chunk save clears only submitted resident keys"
    );

    game.options.resources = fixture.shared.root.clone();
    game.enter_dimension(Dim::Nether)
        .expect("successful actual dimension commit");
    assert_eq!(game.dim, Dim::Nether);
    assert_eq!(game.level.dimension, Dim::Nether.id());
    assert!(game.player.is_none());
    let (player, destination) = game.travelling.as_ref().expect("successful player handoff");
    assert_eq!(player.position, Vec3::new(8.0, 72.0, 8.0));
    assert_eq!(*destination, Vec3::new(1.0, 72.0, 1.0));
    assert_eq!((game.world.game_time, game.world.day_time), (123, 321));
    assert!(game.world.fast_lava);
    assert!(game.world.chunks.is_empty());
    assert!(game.dropped.is_empty() && game.orbs.is_empty() && game.boats.is_empty());
    assert!(game.persist.uuids.is_empty() && game.persist.raw.is_empty());
    assert_eq!(
        game.persist_anon("minecraft:experience_orb", Vec3::new(9.5, 71.0, 9.5)),
        (None, None)
    );
    assert!(
        snapshot(&mut game, (0, 0)).is_empty(),
        "departed unknown record was released"
    );
    assert_same_records(&disk.load_entities(0, 0), &expected);
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn transition_ownership_chunk_failure_keeps_all_dirty_markers_and_player() {
    let fixture = Fixture::new("chunk-io");
    let mut game = fixture.game();
    seed_entities(&mut game);
    let expected = snapshot(&mut game, (0, 0));
    game.world.modified.insert((99, 99));
    let modified = game.world.modified.clone();
    fs::create_dir_all(&game.save.as_ref().unwrap().root).unwrap();
    fs::write(
        game.save.as_ref().unwrap().root.join("region"),
        b"owned region-folder obstruction",
    )
    .unwrap();
    let error = game.enter_dimension(Dim::Nether).unwrap_err().to_string();
    assert!(error.contains("region"), "{error}");
    assert_eq!(game.world.modified, modified);
    assert_player_retained(&game);
    assert_same_records(&snapshot(&mut game, (0, 0)), &expected);
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn transition_ownership_cleanup_preserves_outside_keys_and_vehicle_rider_rules() {
    let fixture = Fixture::new("cleanup");
    let mut game = fixture.game();
    let item = seed_entities(&mut game);
    let outside = Vec3::new(35.0, 70.0, 3.0);
    let selected = HashSet::from([(0, 0)]);
    game.boats.push(boats::Boat::new(
        202,
        Arc::from("oak"),
        Vec3::new(5.0, 70.0, 5.0),
        0.0,
    ));
    game.boats
        .push(boats::Boat::new(303, Arc::from("oak"), outside, 0.0));
    game.persist.uuids.insert(202, [202, 2, 3, 4]);
    game.persist.uuids.insert(303, [303, 2, 3, 4]);
    game.riding = Some(202);
    // A rider's own coordinates are outside: it must leave with boat 101,
    // while the rider of the retained ridden boat must stay with boat 202.
    let mob_tag = |uuid| {
        Tag::compound([
            ("id", Tag::String("minecraft:pig".into())),
            ("UUID", Tag::IntArray(vec![uuid, 2, 3, 4])),
            (
                "Pos",
                Tag::List(
                    [outside.x, outside.y, outside.z]
                        .map(|v| Tag::Double(v.to_bits()))
                        .to_vec(),
                ),
            ),
        ])
    };
    let leaving_rider = game
        .load_mob(mob_tag(81), Some(101))
        .expect("native pig rider");
    let staying_rider = game
        .load_mob(mob_tag(82), Some(202))
        .expect("native pig rider");
    let outside_mob = game
        .load_mob(mob_tag(83), None)
        .expect("native outside pig");
    game.world.loaded_entities.push((
        (2, 0),
        vec![unknown("outside"), orb(outside, [71, 72, 73, 74])],
    ));
    game.spawn_loaded_entities();
    let outside_expected = snapshot(&mut game, (2, 0));
    let selected_expected = snapshot(&mut game, (0, 0));
    assert_eq!(
        game.save_dimension_entities().unwrap(),
        selected,
        "synchronous capture returns exactly resident source coordinates"
    );
    assert_same_records(&snapshot(&mut game, (0, 0)), &selected_expected);
    assert_same_records(
        &game.save.as_ref().unwrap().load_entities(0, 0),
        &selected_expected,
    );
    assert!(
        game.boats.iter().any(|b| b.id == 101),
        "even successful saving is borrowed until target preparation commits"
    );
    let captured = game.entity_chunks(&selected, true); // existing async-unload producer
    assert_same_records(&captured[0].1, &selected_expected);
    assert!(captured[0].1.contains(&unknown("selected")));
    assert!(!game.boats.iter().any(|b| b.id == 101));
    assert!(game.boats.iter().any(|b| b.id == 202));
    assert!(game.boats.iter().any(|b| b.id == 303));
    assert_eq!(game.persist.uuids.get(&202), Some(&[202, 2, 3, 4]));
    assert_eq!(game.persist.uuids.get(&303), Some(&[303, 2, 3, 4]));
    assert!(!game.mobs.iter().any(|m| m.id == leaving_rider));
    assert!(game.mobs.iter().any(|m| m.id == staying_rider));
    assert!(game.mobs.iter().any(|m| m.id == outside_mob));
    assert!(!game.persist.uuids.contains_key(&item));
    assert!(!game.persist.raw.contains_key(&101));
    assert!(!game.persist.uuids.contains_key(&leaving_rider));
    assert!(game.persist.uuids.contains_key(&staying_rider));
    assert_eq!(
        game.persist_anon("minecraft:experience_orb", Vec3::new(9.5, 71.0, 9.5)),
        (None, None)
    );
    assert_eq!(
        game.persist_anon("minecraft:experience_orb", outside).0,
        Some([71, 72, 73, 74])
    );
    assert!(!snapshot(&mut game, (0, 0)).contains(&unknown("selected")));
    assert_same_records(&snapshot(&mut game, (2, 0)), &outside_expected);

    game.carts.push(minecarts::Minecart::new(
        404,
        minecarts::CartKind::Rideable,
        Vec3::new(6.0, 70.0, 6.0),
    ));
    game.carts.push(minecarts::Minecart::new(
        405,
        minecarts::CartKind::Rideable,
        Vec3::new(7.0, 70.0, 7.0),
    ));
    game.riding = Some(404);
    game.remove_saved_entities(&selected);
    assert!(game.carts.iter().any(|c| c.id == 404));
    assert!(!game.carts.iter().any(|c| c.id == 405));
}
