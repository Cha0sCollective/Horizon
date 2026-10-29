//! Real idle-close transitions and native writer faults; process isolation is external.
use super::entity_owner_tests::{configuration, players, ready, rich};
use super::*;
use crate::game::integrated::transition_ownership_tests::Fixture;
use horizon_client::{
    chunk::Chunk,
    counter_trial::Configuration,
    items::ItemStack,
    mutation_observer::AuthorityScope,
    save::{Level, dimension_dir, read_dat, write_dat},
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
    assert!(game.streaming.forced.is_empty());
    write_dat(&tickets(&game.save.root), ticket_record(game.dim)).unwrap();
    let state = game
        .pack
        .assets
        .registry
        .default_state("minecraft:iron_block")
        .unwrap();
    game.world.set(2, 70, 2, state).unwrap();
    let config = configuration(&game.world_root, game.dim, state);
    let snapshot = game.world.enable_counter_trial(config).unwrap();
    let record = snapshot.record().expect("resident native counter");
    eprintln!(
        "IDLE_CLOSE_TEST_COUNTER {}",
        serde_json::json!({
            "world_instance": snapshot.configuration().scope().world_instance(),
            "dimension": game.dim.id(), "instance": record.instance(),
        })
    );
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
    counter: String,
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
        counter: format!("{:?}", world.counter_trial_snapshot()),
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
fn ticket_record(dim: Dim) -> Tag {
    Tag::compound([
        ("DataVersion", Tag::Int(horizon_world::DATA_VERSION)),
        (
            "data",
            Tag::compound([
                ("fixture:opaque", Tag::Int(ordinal(dim) as i32)),
                (
                    "tickets",
                    Tag::List(vec![Tag::compound([
                        ("type", Tag::String("fixture:persistent".into())),
                        ("level", Tag::Int(29)),
                        ("chunk_pos", Tag::IntArray(vec![-9, ordinal(dim) as i32])),
                        ("fixture:retained", Tag::Long(-711)),
                    ])]),
                ),
            ]),
        ),
    ])
}
fn readback(game: &Game<'_>, dim: Dim, save: &Arc<SaveDir>, entities: &[Tag]) {
    save.flush().unwrap();
    let root = dimension_dir(&game.world_root, dim.id());
    assert_eq!(save.root, root);
    let independent = SaveDir::new(root.clone());
    let chunk = independent
        .load(0, 0, game.pack.assets.registry, game.res.biomes())
        .expect("new native reader sees saved chunk");
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
    assert_eq!(independent.load_entities(0, 0), entities);
    assert_eq!(read_dat(&tickets(&root)), Some(ticket_record(dim)));
    assert_eq!(
        fs::read(root.join("unrelated-owner-sentinel.bin")).unwrap(),
        vec![ordinal(dim) as u8, 0x83, 0x97]
    );
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
        .join("owned-idle-slot-metadata")
        .join(format!("{:?}", game.dim));
    for (name, bytes) in [
        ("horizon-paired.toml", b"format=1\nhost-contract='paired-preview-1'\n[content]\nid='idle_slot'\nname='Idle Slot'\nversion='0'\napi='0.2'\nside='both'\ncapabilities=['registries']\n[client]\nentry='client.wasm'\napi='0.1'\ncapabilities=[]\n[server]\nentry='server.wasm'\napi='0.3'\ncapabilities=[]\n".as_slice()),
        ("client.wasm", b"not executed".as_slice()), ("server.wasm", b"not executed".as_slice()),
        ("data/idle_slot/horizon/machine/type.json", br#"{"format":1,"save-schema":1}"#.as_slice()),
        ("data/idle_slot/horizon/block/cube.json", br#"{"template":"minecraft:iron_block","machine-type":"idle_slot:type"}"#.as_slice()),
    ] { let path = folder.join(name); fs::create_dir_all(path.parent().unwrap()).unwrap(); fs::write(path, bytes).unwrap(); }
    let prepared = horizon_mods::paired::PreparedBundle::read_folder(&folder).unwrap();
    let config = Configuration::new(
        AuthorityScope::new(190 + u64::from(ordinal(game.dim)), game.dim.id()).unwrap(),
        prepared
            .content()
            .machine_types()
            .get("idle_slot:type")
            .unwrap(),
        BlockState {
            name: "idle_slot:cube".into(),
            properties: Default::default(),
        },
        state,
        [3, 70, 3],
    )
    .unwrap();
    game.world.set(3, 70, 3, state).unwrap();
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
    game.world.set(3, 70, 3, game.res.facts().air).unwrap();
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
fn eligible(game: &mut Game<'_>, dim: Dim) {
    let parked = game.levels.parked.get_mut(&dim).unwrap();
    assert!(parked.streaming.forced.is_empty());
    assert!(!game.levels_to_tick().contains(&dim));
    game.levels.parked.get_mut(&dim).unwrap().idle = IDLE_TICKS - 1;
}
fn wait_for_retry(game: &mut Game<'_>, dim: Dim) {
    assert_eq!(game.levels.parked[&dim].idle, 0);
    let files = disk(&game.world_root);
    let before = owners(game);
    let players_before = player_owners(game);
    let back = game.dim;
    for tick in 1..IDLE_TICKS {
        game.close_idle_levels().unwrap();
        assert_eq!(game.levels.parked[&dim].idle, tick);
        assert_eq!(game.dim, back);
    }
    assert_restored(game, back, &before, &players_before);
    assert_eq!(disk(&game.world_root), files);
}
fn expected_for(expected: &[(Dim, Vec<Tag>)], dim: Dim) -> &[Tag] {
    &expected.iter().find(|(d, _)| *d == dim).unwrap().1
}
fn retained_except(before: Vec<LevelOwners>, removed: Dim) -> Vec<LevelOwners> {
    before
        .into_iter()
        .filter(|owner| owner.dimension != removed)
        .collect()
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn idle_close_early_ticket_failure_restores_native_owners_and_pending_slot_values() {
    for pending in [false, true] {
        let fixture = Fixture::new("idle-close-early");
        let (mut game, _body, _receivers) = ready(&fixture);
        prepare(&mut game, &[Dim::Overworld, Dim::Nether]);
        let pending_key = if pending {
            game.enter_level(None, Dim::Nether).unwrap();
            let key = pending_slot(&mut game);
            game.enter_level(None, Dim::Overworld).unwrap();
            Some(key)
        } else {
            None
        };
        eligible(&mut game, Dim::Nether);
        let obstruction = obstruct_tickets(&game, Dim::Nether);
        assert_eq!(
            read_dat(&tickets(&game.levels.parked[&Dim::Nether].save.root)),
            Some(ticket_record(Dim::Nether))
        );
        let save = game.levels.parked[&Dim::Nether].save.clone();
        let before = owners(&game);
        let players_before = player_owners(&game);
        let dirty = modified(&game);
        let files = disk(&game.world_root);
        let error = game.close_idle_levels().unwrap_err();
        assert_native_io(error.as_ref(), &obstruction);
        assert_restored(&game, Dim::Overworld, &before, &players_before);
        assert_eq!(game.levels.parked[&Dim::Nether].idle, 0);
        assert!(Arc::ptr_eq(&save, &game.levels.parked[&Dim::Nether].save));
        assert_eq!(modified(&game), dirty);
        assert_eq!(disk(&game.world_root), files);
        save.flush().unwrap();
        if let Some(key) = pending_key {
            let snapshot = game.levels.parked[&Dim::Nether]
                .world
                .world_slot_snapshot()
                .unwrap();
            let pending = snapshot.pending.unwrap();
            assert_eq!(pending.allocation.key, key);
            assert_eq!(pending.allocation.slot.stack().unwrap(), &rich(7));
            // Native snapshots deep-clone; this proves key/payload values,
            // not the original private SlotProjection allocation address.
        }
        // This fixture deliberately never closes the unsupported pending slot.
    }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn idle_close_repaired_ticket_failure_waits_native_interval_then_closes_with_disk_records() {
    for back in [Dim::Overworld, Dim::End] {
        let fixture = Fixture::new("idle-close-repair");
        let (mut game, _body, _receivers) = ready(&fixture);
        let expected = prepare(&mut game, &[back, Dim::Nether]);
        assert!(
            game.levels.parked[&Dim::Nether]
                .world
                .world_slot_snapshot()
                .is_none()
        );
        eligible(&mut game, Dim::Nether);
        let obstruction = obstruct_tickets(&game, Dim::Nether);
        let save = game.levels.parked[&Dim::Nether].save.clone();
        let before = owners(&game);
        let players_before = player_owners(&game);
        let error = game.close_idle_levels().unwrap_err();
        assert_native_io(error.as_ref(), &obstruction);
        assert_restored(&game, back, &before, &players_before);
        fs::remove_dir(&obstruction).unwrap();
        wait_for_retry(&mut game, Dim::Nether);
        let files = disk(&game.world_root);
        game.close_idle_levels().unwrap();
        assert!(!game.levels.parked.contains_key(&Dim::Nether));
        assert_restored(
            &game,
            back,
            &retained_except(before, Dim::Nether),
            &players_before,
        );
        readback(
            &game,
            Dim::Nether,
            &save,
            expected_for(&expected, Dim::Nether),
        );
        assert_write_footprint(
            &files,
            disk(&game.world_root),
            std::slice::from_ref(&save.root),
        );
    }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn idle_close_sticky_entity_failure_retains_writer_and_native_retry_can_write() {
    let fixture = Fixture::new("idle-close-sticky");
    let (mut game, _body, _receivers) = ready(&fixture);
    let expected = prepare(&mut game, &[Dim::Overworld, Dim::Nether]);
    eligible(&mut game, Dim::Nether);
    let save = game.levels.parked[&Dim::Nether].save.clone();
    let obstruction = save.root.join("entities");
    fs::write(&obstruction, b"owned native entity-directory obstruction").unwrap();
    let before = owners(&game);
    let players_before = player_owners(&game);
    let files = disk(&game.world_root);
    let error = game.close_idle_levels().unwrap_err().to_string();
    assert!(error.contains("entities"), "{error}");
    assert_restored(&game, Dim::Overworld, &before, &players_before);
    assert_eq!(game.levels.parked[&Dim::Nether].idle, 0);
    assert!(Arc::ptr_eq(&save, &game.levels.parked[&Dim::Nether].save));
    let sticky = save.flush().unwrap_err().to_string();
    assert!(sticky.contains("entities"), "{sticky}");
    assert_eq!(error, sticky);
    assert!(save.writes_finished().is_err());
    assert_eq!(
        save.load_entities(0, 0),
        expected_for(&expected, Dim::Nether)
    );
    assert!(
        SaveDir::new(save.root.clone())
            .load_entities(0, 0)
            .is_empty()
    );
    assert_write_footprint(
        &files,
        disk(&game.world_root),
        std::slice::from_ref(&save.root),
    );
    fs::remove_file(&obstruction).unwrap();
    wait_for_retry(&mut game, Dim::Nether);
    let files = disk(&game.world_root);
    let error = game.close_idle_levels().unwrap_err().to_string();
    assert!(error.contains("entities"), "{error}");
    assert_restored(&game, Dim::Overworld, &before, &players_before);
    assert_eq!(game.levels.parked[&Dim::Nether].idle, 0);
    assert!(Arc::ptr_eq(&save, &game.levels.parked[&Dim::Nether].save));
    assert_eq!(error, sticky);
    assert_eq!(save.flush().unwrap_err().to_string(), sticky);
    assert!(save.writes_finished().is_err());
    // Native count-zero flush returns its historical error after draining retry writes.
    assert_eq!(
        SaveDir::new(save.root.clone()).load_entities(0, 0),
        expected_for(&expected, Dim::Nether)
    );
    let independent = SaveDir::new(save.root.clone());
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
    assert_eq!(
        read_dat(&tickets(&save.root)),
        Some(ticket_record(Dim::Nether))
    );
    assert_write_footprint(
        &files,
        disk(&game.world_root),
        std::slice::from_ref(&save.root),
    );
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn idle_close_second_failure_keeps_completed_first_closed_and_failed_owner_retained() {
    let fixture = Fixture::new("idle-close-second-failure");
    let (mut game, _body, _receivers) = ready(&fixture);
    let expected = prepare(&mut game, &[Dim::Overworld, Dim::Nether, Dim::End]);
    // Capture exactly the current HashMap-derived closing order; do not sort it.
    let order: Vec<_> = game.levels.parked.keys().copied().collect();
    assert_eq!(order.len(), 2);
    for &dim in &order {
        eligible(&mut game, dim);
    }
    let first = order[0];
    let second = order[1];
    let first_save = game.levels.parked[&first].save.clone();
    let second_save = game.levels.parked[&second].save.clone();
    let obstruction = obstruct_tickets(&game, second);
    let before = owners(&game);
    let players_before = player_owners(&game);
    let files = disk(&game.world_root);
    let error = game.close_idle_levels().unwrap_err();
    assert_native_io(error.as_ref(), &obstruction);
    assert!(!game.levels.parked.contains_key(&first));
    assert!(Arc::ptr_eq(&second_save, &game.levels.parked[&second].save));
    assert_eq!(game.levels.parked[&second].idle, 0);
    assert_restored(
        &game,
        Dim::Overworld,
        &retained_except(before, first),
        &players_before,
    );
    readback(&game, first, &first_save, expected_for(&expected, first));
    second_save.flush().unwrap();
    assert!(
        SaveDir::new(second_save.root.clone())
            .load_entities(0, 0)
            .is_empty()
    );
    assert_write_footprint(
        &files,
        disk(&game.world_root),
        std::slice::from_ref(&first_save.root),
    );
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn idle_close_first_failure_leaves_later_producer_and_counter_unprocessed() {
    let fixture = Fixture::new("idle-close-first-failure");
    let (mut game, _body, _receivers) = ready(&fixture);
    prepare(&mut game, &[Dim::Overworld, Dim::Nether, Dim::End]);
    let order: Vec<_> = game.levels.parked.keys().copied().collect();
    assert_eq!(order.len(), 2);
    for &dim in &order {
        eligible(&mut game, dim);
    }
    let obstruction = obstruct_tickets(&game, order[0]);
    let before = owners(&game);
    let players_before = player_owners(&game);
    let files = disk(&game.world_root);
    let error = game.close_idle_levels().unwrap_err();
    assert_native_io(error.as_ref(), &obstruction);
    assert_restored(&game, Dim::Overworld, &before, &players_before);
    assert_eq!(game.levels.parked[&order[0]].idle, 0);
    assert_eq!(game.levels.parked[&order[1]].idle, IDLE_TICKS);
    assert_eq!(disk(&game.world_root), files);
    for dim in order {
        let save = &game.levels.parked[&dim].save;
        save.flush().unwrap();
        assert!(
            SaveDir::new(save.root.clone())
                .load_entities(0, 0)
                .is_empty()
        );
        assert!(!save.root.join("region").exists());
        assert!(!save.root.join("entities").exists());
    }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn idle_close_native_guest_forced_threshold_and_overworld_eligibility_regression() {
    let fixture = Fixture::new("idle-close-eligibility");
    let (mut game, _body, _receivers) = ready(&fixture);
    let expected = prepare(&mut game, &[Dim::Overworld, Dim::Nether, Dim::End]);
    {
        let guest = &mut game.lan.as_mut().unwrap().hub.guests[0];
        guest.dimension = Dim::Nether.id().into();
        guest.placed = false;
        guest.loaded = false;
    }
    game.levels.parked.get_mut(&Dim::Nether).unwrap().idle = IDLE_TICKS;
    let end = game.levels.parked.get_mut(&Dim::End).unwrap();
    end.idle = IDLE_TICKS;
    end.streaming.forced.insert((7, -8));
    assert!(game.levels_to_tick().contains(&Dim::Nether));
    assert!(game.levels_to_tick().contains(&Dim::End));
    let before = owners(&game);
    let player_before = player_owners(&game);
    let files = disk(&game.world_root);
    game.close_idle_levels().unwrap();
    assert_eq!(game.dim, Dim::Overworld);
    assert_eq!(
        game.lan.as_ref().unwrap().hub.config.dimension,
        Dim::Overworld.id()
    );
    assert_eq!(owners(&game), before);
    assert_eq!(player_owners(&game), player_before);
    assert_eq!(game.levels.parked[&Dim::Nether].idle, 0);
    assert_eq!(game.levels.parked[&Dim::End].idle, 0);
    assert_eq!(disk(&game.world_root), files);
    {
        let guest = &mut game.lan.as_mut().unwrap().hub.guests[0];
        guest.dimension = Dim::Overworld.id().into();
        guest.placed = true;
        guest.loaded = true;
    }
    game.levels
        .parked
        .get_mut(&Dim::End)
        .unwrap()
        .streaming
        .forced
        .clear();
    game.levels.parked.get_mut(&Dim::Nether).unwrap().idle = IDLE_TICKS - 2;
    let before = owners(&game);
    let player_before = player_owners(&game);
    let files = disk(&game.world_root);
    game.close_idle_levels().unwrap();
    assert_restored(&game, Dim::Overworld, &before, &player_before);
    assert_eq!(game.levels.parked[&Dim::Nether].idle, IDLE_TICKS - 1);
    assert_eq!(game.levels.parked[&Dim::End].idle, 1);
    assert_eq!(disk(&game.world_root), files);
    let save = game.levels.parked[&Dim::Nether].save.clone();
    game.close_idle_levels().unwrap();
    assert_restored(
        &game,
        Dim::Overworld,
        &retained_except(before, Dim::Nether),
        &player_before,
    );
    assert!(!game.levels.parked.contains_key(&Dim::Nether));
    assert_eq!(game.levels.parked[&Dim::End].idle, 2);
    readback(
        &game,
        Dim::Nether,
        &save,
        expected_for(&expected, Dim::Nether),
    );
    assert_write_footprint(
        &files,
        disk(&game.world_root),
        std::slice::from_ref(&save.root),
    );

    // Exclude Overworld even when genuinely unoccupied and beyond threshold.
    game.enter_level(None, Dim::End).unwrap();
    for guest in &mut game.lan.as_mut().unwrap().hub.guests {
        guest.dimension = Dim::End.id().into();
    }
    game.levels.parked.get_mut(&Dim::Overworld).unwrap().idle = IDLE_TICKS;
    assert!(!game.levels_to_tick().contains(&Dim::Overworld));
    let before = owners(&game);
    let player_before = player_owners(&game);
    let files = disk(&game.world_root);
    game.close_idle_levels().unwrap();
    assert_restored(&game, Dim::End, &before, &player_before);
    assert_eq!(game.levels.parked[&Dim::Overworld].idle, IDLE_TICKS + 1);
    assert_eq!(disk(&game.world_root), files);
    // Pregen (including paused tasks) is source-supported; no global tree flag changes.
}
