//! Real parked saves and native writer faults; the harness owns process isolation.
use super::entity_owner_tests::{players, ready, rich};
use super::*;
use crate::game::integrated::transition_ownership_tests::Fixture;
use horizon_client::{
    chunk::Chunk,
    counter_trial::Configuration,
    items::ItemStack,
    mutation_observer::AuthorityScope,
    save::{Level, dimension_dir, read_dat},
    slot_transfer::{Direction, ResolvedLimits, SlotProjection},
    world_slot_trial::{Preparation, SlotKey},
};
use horizon_core::{geometry::Vec3, random::RandomSource};
use horizon_world::{chunk::BlockState, nbt::Tag, registry::BlockStateId};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

fn ordinal(dim: Dim) -> u32 {
    match dim {
        Dim::Overworld => 1,
        Dim::Nether => 2,
        Dim::End => 3,
    }
}
fn tickets(root: &Path) -> PathBuf {
    root.join("data/minecraft/chunk_tickets.dat")
}
fn native_drop(id: u32, stack: ItemStack) -> ItemEntity {
    let at = Vec3::new(2.5, 71.0, 2.5);
    ItemEntity {
        health: 5,
        fire_ticks: -17,
        id,
        stack,
        position: at,
        previous: at,
        velocity: Vec3::new(0.125, 0.25, -0.125),
        thrower: Some([1, 2, 3, 4]),
        target: None,
        on_ground: false,
        age: 23,
        pickup_delay: 37,
        bob_offset: 0.25,
        ticks: 11,
    }
}
fn seed_current(game: &mut Game<'_>) -> Vec<Tag> {
    let index = ordinal(game.dim);
    game.world
        .chunks
        .insert((0, 0), Arc::new(Chunk::empty(0, 0)));
    let block = game
        .pack
        .assets
        .registry
        .default_state(match game.dim {
            Dim::Overworld => "minecraft:stone",
            Dim::Nether => "minecraft:cobblestone",
            Dim::End => "minecraft:gold_block",
        })
        .unwrap();
    game.world.set(1, 70, 1, block).unwrap();
    let mut stack = rich(index * 4 + 3);
    stack.item = match game.dim {
        Dim::Overworld => "minecraft:stone",
        Dim::Nether => "minecraft:dirt",
        Dim::End => "minecraft:gold_ingot",
    }
    .into();
    let id = 700 + index;
    game.dropped.push(native_drop(id, stack));
    game.persist.uuids.insert(id, [index as i32, -22, 33, -44]);
    game.persist.raw.insert(
        id,
        Tag::compound([
            ("fixture:dimension", Tag::Int(index as i32)),
            ("fixture:opaque", Tag::compound([("value", Tag::Long(-73))])),
        ]),
    );
    game.streaming
        .forced
        .insert((index as i32, -(index as i32)));
    fs::create_dir_all(&game.save.root).unwrap();
    fs::write(
        game.save.root.join("unrelated-owner-sentinel.bin"),
        [index as u8, 0x83, 0x97],
    )
    .unwrap();
    let entities = game.entity_chunks(&HashSet::from([(0, 0)]), false);
    assert_eq!(entities.len(), 1);
    let tags = entities.into_iter().next().unwrap().1;
    assert!(!tags.is_empty());
    tags
}
fn prepare(game: &mut Game<'_>, dimensions: &[Dim]) -> Vec<(Dim, Vec<Tag>)> {
    let mut expected = Vec::new();
    for &dim in dimensions {
        game.enter_level(None, dim).unwrap();
        expected.push((dim, seed_current(game)));
    }
    game.enter_level(None, dimensions[0]).unwrap();
    players(game);
    expected
}
fn disk(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    fn visit(path: &Path, out: &mut Vec<(PathBuf, Option<Vec<u8>>)>) {
        if !path.exists() {
            return;
        }
        if path.is_dir() {
            out.push((path.to_owned(), None));
            for entry in fs::read_dir(path).unwrap() {
                visit(&entry.unwrap().path(), out);
            }
        } else {
            out.push((path.to_owned(), Some(fs::read(path).unwrap())));
        }
    }
    let mut out = Vec::new();
    visit(root, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}
fn assert_write_footprint(
    before: &[(PathBuf, Option<Vec<u8>>)],
    after: Vec<(PathBuf, Option<Vec<u8>>)>,
    participating: &[PathBuf],
) {
    let mut before: std::collections::BTreeMap<_, _> = before.iter().cloned().collect();
    let mut after: std::collections::BTreeMap<_, _> = after.into_iter().collect();
    for root in participating {
        // Native configured SaveTables may enqueue POI for the same column.
        // Empty POI with no old record is skipped; an old record is rewritten
        // as an empty POI root, so no whole-file deletion is permitted.
        for relative in [
            "data/minecraft/chunk_tickets.dat",
            "data/minecraft/chunk_tickets.dat_old",
            "region/r.0.0.mca",
            "entities/r.0.0.mca",
            "poi/r.0.0.mca",
        ] {
            let path = root.join(relative);
            let old = before.remove(&path);
            let new = after.remove(&path);
            if old != new {
                assert!(
                    old.as_ref().is_none_or(Option::is_some),
                    "unexpected old directory at {path:?}"
                );
                assert!(
                    new.as_ref().is_some_and(Option::is_some),
                    "native file must exist after writing: {path:?}"
                );
            }
        }
        for relative in ["", "data", "data/minecraft", "region", "entities", "poi"] {
            let path = root.join(relative);
            let old = before.get(&path);
            let new = after.get(&path);
            if old.is_none() && new == Some(&None) {
                after.remove(&path);
            }
        }
    }
    // Includes all unrelated bytes, empty directories, unknown creations and
    // deletions, and obstruction file/directory type transitions.
    assert_eq!(after, before);
}
fn raw(stack: &ItemStack, out: &mut Vec<(usize, i32)>) {
    if let Some(data) = stack.extra.as_deref() {
        out.push((data as *const _ as usize, data.bundle_selected.0));
        for (_, child) in &data.container {
            raw(child, out);
        }
        for child in data.charged.iter().chain(data.bundle_contents.iter()) {
            raw(child, out);
        }
        if let Some(pots) = &data.pot_decorations {
            for child in pots.iter().flatten() {
                raw(child, out);
            }
        }
    }
}
#[derive(Debug, PartialEq)]
struct LevelOwners {
    dimension: Dim,
    save: usize,
    root: PathBuf,
    chunks: Vec<((i32, i32), usize, BlockStateId)>,
    drops: Vec<String>,
    uuids: std::collections::HashMap<u32, [i32; 4]>,
    raw: std::collections::HashMap<u32, Tag>,
    forced: Vec<(i32, i32)>,
    slot: String,
    pending: Option<SlotKey>,
    random: [i64; 4],
}
fn level_owners(
    dimension: Dim,
    world: &World,
    save: &Arc<SaveDir>,
    dropped: &[ItemEntity],
    memory: &persist::EntityMemory,
    streaming: &Streaming,
) -> LevelOwners {
    let mut chunks: Vec<_> = world
        .chunks
        .iter()
        .map(|(key, chunk)| (*key, Arc::as_ptr(chunk) as usize, chunk.block(1, 70, 1)))
        .collect();
    chunks.sort_by_key(|(key, ..)| *key);
    let drops = dropped
        .iter()
        .map(|item| {
            let mut metadata = Vec::new();
            raw(&item.stack, &mut metadata);
            format!("{item:?}:{metadata:?}")
        })
        .collect();
    let mut forced: Vec<_> = streaming.forced.iter().copied().collect();
    forced.sort_unstable();
    let slot = world.world_slot_snapshot();
    let pending = slot
        .as_ref()
        .and_then(|snapshot| snapshot.pending.as_ref())
        .map(|pending| pending.allocation.key.clone());
    let mut random = world.random.clone();
    let random: [i64; 4] = std::array::from_fn(|_| random.next_long());
    LevelOwners {
        dimension,
        save: Arc::as_ptr(save) as usize,
        root: save.root.clone(),
        chunks,
        drops,
        uuids: memory.uuids.clone(),
        raw: memory.raw.clone(),
        forced,
        slot: format!("{slot:?}"),
        pending,
        random,
    }
}
fn owners(game: &Game<'_>) -> Vec<LevelOwners> {
    let mut out = vec![level_owners(
        game.dim,
        &game.world,
        &game.save,
        &game.dropped,
        &game.persist,
        &game.streaming,
    )];
    for (&dim, parked) in &game.levels.parked {
        out.push(level_owners(
            dim,
            &parked.world,
            &parked.save,
            &parked.dropped,
            &parked.persist,
            &parked.streaming,
        ));
    }
    out.sort_by_key(|level| level.dimension.id());
    out
}
fn player_owners(game: &Game<'_>) -> Vec<String> {
    let host = game.lan.as_ref().unwrap();
    let mut out = Vec::new();
    for guest in &host.hub.guests {
        let mut level = Level::default();
        horizon_client::lan::playerdata::store(guest, &mut level);
        let mut metadata = Vec::new();
        for stack in guest
            .inventory
            .iter()
            .chain(guest.armor.iter())
            .chain(std::iter::once(&guest.offhand))
            .chain(guest.ender_items.iter())
            .flatten()
        {
            raw(stack, &mut metadata);
        }
        out.push(format!(
            "{:?}",
            (
                guest.conn,
                guest.uuid,
                level,
                &guest.saved,
                metadata,
                guest
                    .motion
                    .as_deref()
                    .map(|motion| motion as *const _ as usize),
                guest
                    .motion
                    .as_deref()
                    .map(horizon_client::lan::guest::SavedBody::of),
                &guest.advancements.criteria,
                &guest.advancements_changed,
                &guest.recipes_unlocked
            )
        ));
    }
    let mut play: Vec<_> = host.play.iter().collect();
    play.sort_by_key(|(conn, _)| **conn);
    for (conn, play) in play {
        out.push(format!(
            "{:?}",
            (
                conn,
                &play.stats.values,
                &play.stats.events,
                play.stats.loaded,
                play.checkpoint_participant_idle(),
                play.left_game,
                play.won_game,
                play.respawn_requested
            )
        ));
    }
    out
}
fn assert_restored(game: &Game<'_>, back: Dim, before: &[LevelOwners], players_before: &[String]) {
    assert_eq!(game.dim, back);
    assert_eq!(game.lan.as_ref().unwrap().hub.config.dimension, back.id());
    assert_eq!(owners(game), before);
    assert_eq!(player_owners(game), players_before);
    players(game);
}
fn modified(game: &Game<'_>) -> Vec<(Dim, Vec<(i32, i32)>)> {
    let mut active: Vec<_> = game.world.modified.iter().copied().collect();
    active.sort_unstable();
    let mut out = vec![(game.dim, active)];
    for (&dim, parked) in &game.levels.parked {
        let mut keys: Vec<_> = parked.world.modified.iter().copied().collect();
        keys.sort_unstable();
        out.push((dim, keys));
    }
    out.sort_by_key(|(dim, _)| dim.id());
    out
}
fn obstruct_tickets(game: &Game<'_>, dimension: Dim) -> PathBuf {
    let root = &game.levels.parked[&dimension].save.root;
    let obstruction = tickets(root).with_file_name("chunk_tickets.dat_new");
    fs::create_dir_all(&obstruction).unwrap();
    obstruction
}
fn assert_native_io(error: &(dyn std::error::Error + 'static), obstruction: &Path) {
    assert!(obstruction.is_dir());
    let io = error
        .downcast_ref::<std::io::Error>()
        .expect("native fs::write failure retains its IO error");
    assert_ne!(io.kind(), std::io::ErrorKind::NotFound);
    assert_ne!(io.kind(), std::io::ErrorKind::Other);
    assert!(!io.to_string().is_empty());
}
fn readback(game: &Game<'_>, expected: &[(Dim, Vec<Tag>)]) {
    for (dim, entities) in expected {
        let save = if game.dim == *dim {
            &game.save
        } else {
            &game.levels.parked[dim].save
        };
        save.flush().unwrap();
        let root = dimension_dir(&game.world_root, dim.id());
        assert_eq!(save.root, root);
        let independent = SaveDir::new(root.clone());
        let chunk = independent
            .load(0, 0, game.pack.assets.registry, game.res.biomes())
            .expect("actual independent disk chunk readback");
        let state = game
            .pack
            .assets
            .registry
            .default_state(match dim {
                Dim::Overworld => "minecraft:stone",
                Dim::Nether => "minecraft:cobblestone",
                Dim::End => "minecraft:gold_block",
            })
            .unwrap();
        assert_eq!(chunk.block(1, 70, 1), state);
        assert_eq!(independent.load_entities(0, 0), *entities);
        let index = ordinal(*dim) as i32;
        assert_eq!(
            read_dat(&tickets(&root)),
            Some(Tag::compound([
                ("DataVersion", Tag::Int(horizon_world::DATA_VERSION)),
                (
                    "data",
                    Tag::compound([(
                        "tickets",
                        Tag::List(vec![Tag::compound([
                            ("type", Tag::String("minecraft:forced".into())),
                            ("level", Tag::Int(31)),
                            ("chunk_pos", Tag::IntArray(vec![index, -index])),
                        ])])
                    )])
                ),
            ]))
        );
        assert_eq!(
            fs::read(root.join("unrelated-owner-sentinel.bin")).unwrap(),
            vec![ordinal(*dim) as u8, 0x83, 0x97]
        );
    }
}
fn pending_slot(game: &mut Game<'_>) -> SlotKey {
    let state = game
        .pack
        .assets
        .registry
        .default_state("minecraft:iron_block")
        .unwrap();
    let folder = game
        .world_root
        .join("owned-parked-slot-metadata")
        .join(format!("{:?}", game.dim));
    for (name, bytes) in [
        ("horizon-paired.toml", b"format=1\nhost-contract='paired-preview-1'\n[content]\nid='parked_slot'\nname='Parked Slot'\nversion='0'\napi='0.2'\nside='both'\ncapabilities=['registries']\n[client]\nentry='client.wasm'\napi='0.1'\ncapabilities=[]\n[server]\nentry='server.wasm'\napi='0.3'\ncapabilities=[]\n".as_slice()),
        ("client.wasm", b"not executed".as_slice()), ("server.wasm", b"not executed".as_slice()),
        ("data/parked_slot/horizon/machine/type.json", br#"{"format":1,"save-schema":1}"#.as_slice()),
        ("data/parked_slot/horizon/block/cube.json", br#"{"template":"minecraft:iron_block","machine-type":"parked_slot:type"}"#.as_slice()),
    ] { let path = folder.join(name); fs::create_dir_all(path.parent().unwrap()).unwrap(); fs::write(path, bytes).unwrap(); }
    let prepared = horizon_mods::paired::PreparedBundle::read_folder(&folder).unwrap();
    let config = Configuration::new(
        AuthorityScope::new(190 + u64::from(ordinal(game.dim)), game.dim.id()).unwrap(),
        prepared
            .content()
            .machine_types()
            .get("parked_slot:type")
            .unwrap(),
        BlockState {
            name: "parked_slot:cube".into(),
            properties: Default::default(),
        },
        state,
        [2, 70, 2],
    )
    .unwrap();
    game.world.set(2, 70, 2, state).unwrap();
    let key = game
        .world
        .enable_world_slot_trial(config)
        .unwrap()
        .active
        .unwrap()
        .key;
    let mut source = SlotProjection::new(Some(rich(7)));
    let limits = ResolvedLimits::new(64, 64).unwrap();
    let plan = match game
        .world
        .prepare_world_slot_transfer(&key, &source, Direction::IntoMachine, 7, limits)
        .unwrap()
    {
        Preparation::Prepared(plan) => *plan,
        other => panic!("positive native slot transfer required: {other:?}"),
    };
    game.world
        .commit_world_slot_transfer(plan, &mut source, limits)
        .unwrap();
    assert!(source.into_stack().is_none());
    game.world.set(2, 70, 2, game.res.facts().air).unwrap();
    let snapshot = game.world.world_slot_snapshot().unwrap();
    let pending = snapshot.pending.unwrap();
    assert_eq!(pending.allocation.key, key);
    assert_eq!(pending.allocation.slot.stack().unwrap(), &rich(7));
    assert_eq!(
        pending
            .allocation
            .slot
            .stack()
            .unwrap()
            .data()
            .unwrap()
            .bundle_selected
            .0,
        4
    );
    assert_eq!(
        pending
            .allocation
            .slot
            .stack()
            .unwrap()
            .data()
            .unwrap()
            .container[0]
            .1
            .data()
            .unwrap()
            .bundle_selected
            .0,
        9
    );
    key
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn parked_save_early_ticket_error_restores_owners_in_both_wait_modes_and_repairs() {
    for wait in [false, true] {
        let fixture = Fixture::new("parked-save-early-repair");
        let (mut game, _body, _receivers) = ready(&fixture);
        let expected = prepare(&mut game, &[Dim::Overworld, Dim::Nether]);
        let active_key = pending_slot(&mut game);
        game.enter_level(None, Dim::Nether).unwrap();
        let parked_key = pending_slot(&mut game);
        game.enter_level(None, Dim::Overworld).unwrap();
        assert_ne!(active_key, parked_key);
        let obstruction = obstruct_tickets(&game, Dim::Nether);
        let before = owners(&game);
        let players_before = player_owners(&game);
        let dirty = modified(&game);
        let files = disk(&game.world_root);
        let level = format!("{:?}", game.level);
        players(&game);
        let error = game.save_parked_levels(wait).unwrap_err();
        assert_native_io(error.as_ref(), &obstruction);
        assert_restored(&game, Dim::Overworld, &before, &players_before);
        assert_eq!(modified(&game), dirty);
        assert_eq!(disk(&game.world_root), files);
        assert_eq!(format!("{:?}", game.level), level);
        assert_eq!(
            game.world
                .world_slot_snapshot()
                .unwrap()
                .pending
                .unwrap()
                .allocation
                .key,
            active_key
        );
        assert_eq!(
            game.levels.parked[&Dim::Nether]
                .world
                .world_slot_snapshot()
                .unwrap()
                .pending
                .unwrap()
                .allocation
                .key,
            parked_key
        );
        fs::remove_dir(&obstruction).unwrap();
        game.save_parked_levels(wait).unwrap();
        assert_restored(&game, Dim::Overworld, &before, &players_before);
        // This method saves parked levels only; the original active dimension is untouched.
        readback(&game, &expected[1..]);
        assert!(!tickets(&game.save.root).exists());
        assert!(!game.save.root.join("region").exists());
        assert!(!game.save.root.join("entities").exists());
    }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn parked_save_sticky_entity_error_restores_same_native_writer_on_failure_and_retry() {
    let fixture = Fixture::new("parked-save-sticky-writer");
    let (mut game, _body, _receivers) = ready(&fixture);
    let expected = prepare(&mut game, &[Dim::Overworld, Dim::Nether]);
    let parked_save = game.levels.parked[&Dim::Nether].save.clone();
    let obstruction = parked_save.root.join("entities");
    fs::write(&obstruction, b"owned entity-directory obstruction").unwrap();
    let before = owners(&game);
    let players_before = player_owners(&game);
    let files_before = disk(&game.world_root);
    players(&game);
    let error = game.save_parked_levels(true).unwrap_err().to_string();
    assert!(error.contains("entities"), "{error}");
    assert_restored(&game, Dim::Overworld, &before, &players_before);
    assert!(Arc::ptr_eq(
        &game.levels.parked[&Dim::Nether].save,
        &parked_save
    ));
    let sticky = parked_save.flush().unwrap_err().to_string();
    assert!(sticky.contains("entities"), "{sticky}");
    assert!(parked_save.writes_finished().is_err());
    assert_eq!(parked_save.load_entities(0, 0), expected[1].1);
    let independent = SaveDir::new(parked_save.root.clone());
    assert!(independent.load_entities(0, 0).is_empty());
    let chunk = independent
        .load(0, 0, game.pack.assets.registry, game.res.biomes())
        .unwrap();
    assert_eq!(
        chunk.block(1, 70, 1),
        game.pack
            .assets
            .registry
            .default_state("minecraft:cobblestone")
            .unwrap()
    );
    assert_write_footprint(
        &files_before,
        disk(&game.world_root),
        std::slice::from_ref(&parked_save.root),
    );
    fs::remove_file(&obstruction).unwrap();
    let retry_files = disk(&game.world_root);
    let error = game.save_parked_levels(true).unwrap_err().to_string();
    assert!(error.contains("entities"), "{error}");
    assert_restored(&game, Dim::Overworld, &before, &players_before);
    assert!(Arc::ptr_eq(
        &game.levels.parked[&Dim::Nether].save,
        &parked_save
    ));
    assert_eq!(parked_save.flush().unwrap_err().to_string(), sticky);
    assert!(parked_save.writes_finished().is_err());
    assert_eq!(parked_save.load_entities(0, 0), expected[1].1);
    // The retry still enqueues native writes. The flush above drains them before
    // returning the retained historical error; successful writes do not reset it.
    assert_eq!(
        SaveDir::new(parked_save.root.clone()).load_entities(0, 0),
        expected[1].1,
    );
    assert_write_footprint(
        &retry_files,
        disk(&game.world_root),
        std::slice::from_ref(&parked_save.root),
    );
    assert!(!game.save.root.join("region").exists() && !game.save.root.join("entities").exists());
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn parked_save_error_propagates_through_full_caller_before_metadata_publication() {
    for wait in [false, true] {
        let fixture = Fixture::new("parked-save-caller-metadata");
        let (mut game, _body, _receivers) = ready(&fixture);
        let expected = prepare(&mut game, &[Dim::Overworld, Dim::Nether]);
        let mut sentinel = game.level.clone();
        sentinel.name = "existing metadata sentinel".into();
        sentinel.player_extra = Some(Tag::compound([("fixture:metadata", Tag::Long(-83))]));
        sentinel.write(&game.world_root).unwrap();
        game.level.player_extra = Some(Tag::compound([("fixture:live_player", Tag::Int(97))]));
        let level_path = game.world_root.join("level.dat");
        let bytes = fs::read(&level_path).unwrap();
        assert!(read_dat(&level_path).is_some());
        let obstruction = obstruct_tickets(&game, Dim::Nether);
        let before = owners(&game);
        let players_before = player_owners(&game);
        let level = format!("{:?}", game.level);
        let files_before = disk(&game.world_root);
        players(&game);
        let error = game.save_game_with(wait).unwrap_err();
        assert_native_io(error.as_ref(), &obstruction);
        assert_restored(&game, Dim::Overworld, &before, &players_before);
        assert_eq!(fs::read(&level_path).unwrap(), bytes);
        assert_eq!(format!("{:?}", game.level), level);
        readback(&game, &expected[..1]);
        assert_write_footprint(
            &files_before,
            disk(&game.world_root),
            &[game.save.root.clone()],
        );
        let parked = &game.levels.parked[&Dim::Nether];
        assert!(
            !parked.save.root.join("region").exists()
                && !parked.save.root.join("entities").exists()
        );
        assert!(!tickets(&parked.save.root).exists());
        assert!(obstruction.is_dir());
    }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn parked_save_success_preserves_three_dimension_owners_and_independent_disk_records() {
    for wait in [false, true] {
        for back in [Dim::Overworld, Dim::End] {
            let fixture = Fixture::new("parked-save-three-dimensions");
            let (mut game, _body, _receivers) = ready(&fixture);
            let expected = prepare(&mut game, &[Dim::Overworld, Dim::Nether, Dim::End]);
            game.enter_level(None, back).unwrap();
            let before = owners(&game);
            let players_before = player_owners(&game);
            let files_before = disk(&game.world_root);
            players(&game);
            game.save_level(wait).unwrap();
            game.save_parked_levels(wait).unwrap();
            // Save the active producer then parked producers; full-caller failure is covered separately.
            assert_restored(&game, back, &before, &players_before);
            readback(&game, &expected);
            let roots: Vec<_> = expected
                .iter()
                .map(|(dim, _)| dimension_dir(&game.world_root, dim.id()))
                .collect();
            assert_write_footprint(&files_before, disk(&game.world_root), &roots);
        }
    }
}
#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn parked_save_empty_population_is_an_io_free_native_noop() {
    let fixture = Fixture::new("parked-save-empty");
    let (mut game, _body, _receivers) = ready(&fixture);
    prepare(&mut game, &[Dim::Overworld]);
    let before = owners(&game);
    let players_before = player_owners(&game);
    let files = disk(&game.world_root);
    let level = format!("{:?}", game.level);
    for wait in [false, true] {
        players(&game);
        game.save_parked_levels(wait).unwrap();
        assert_restored(&game, Dim::Overworld, &before, &players_before);
        assert_eq!(disk(&game.world_root), files);
        assert_eq!(format!("{:?}", game.level), level);
    }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn parked_save_first_native_failure_does_not_run_later_parked_producers() {
    let fixture = Fixture::new("parked-save-first-error");
    let (mut game, _body, _receivers) = ready(&fixture);
    prepare(&mut game, &[Dim::Overworld, Dim::Nether, Dim::End]);
    // Observe the current native map order rather than imposing an ordering policy.
    let first = *game.levels.parked.keys().next().unwrap();
    let obstruction = obstruct_tickets(&game, first);
    let before = owners(&game);
    let players_before = player_owners(&game);
    let files = disk(&game.world_root);
    players(&game);
    let error = game.save_parked_levels(true).unwrap_err();
    assert_native_io(error.as_ref(), &obstruction);
    assert_restored(&game, Dim::Overworld, &before, &players_before);
    assert_eq!(disk(&game.world_root), files);
    for parked in game.levels.parked.values() {
        assert!(!tickets(&parked.save.root).exists());
        assert!(
            !parked.save.root.join("region").exists()
                && !parked.save.root.join("entities").exists()
        );
    }
}
