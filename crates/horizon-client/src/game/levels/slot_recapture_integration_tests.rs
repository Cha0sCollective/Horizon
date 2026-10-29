//! Restored 3/T only: exact native roots and validated inherited snapshot files.
use super::super::tests::{loaded, source};
use super::*;
use crate::game::{
    integrated::{
        capture_completion_tests::{
            dispatch_slot_capture, dispatch_slot_recapture,
            dispatch_slot_recapture_after_observer_timeout,
        },
        transition_ownership_tests::Fixture,
    },
    slot_player::TransferOutcome,
};
use horizon_client::{
    chunk::RolledLoot,
    containers::{Banner, BlockEntity},
    counter_trial::Snapshot,
    lan::Out,
    motion::PlayerMotion,
    slot_transfer::Direction,
    world::WorldOwnerKind as K,
};
use horizon_core::{
    position::BlockPos,
    random::RandomSource,
    ticks::{SavedTick, TickPriority, TickTypeId},
};
use horizon_world::nbt::Compound;
use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};
const T: i64 = 126;
const COUNTER_FILE: &str = "data/horizon/counter_checkpoint.json";
const SLOT_FILE: &str = "data/horizon/slot_checkpoint.nbt";
type Disk = BTreeMap<PathBuf, Option<Vec<u8>>>;
fn disk(root: &Path) -> Disk {
    fn walk(path: &Path, out: &mut Disk) {
        let Ok(meta) = fs::symlink_metadata(path) else {
            return;
        };
        if meta.file_type().is_symlink() {
            use std::os::unix::ffi::OsStrExt;
            let mut bytes = b"symlink-target\0".to_vec();
            bytes.extend_from_slice(fs::read_link(path).unwrap().as_os_str().as_bytes());
            out.insert(path.to_owned(), Some(bytes));
        } else if meta.is_dir() {
            out.insert(path.to_owned(), None);
            for row in fs::read_dir(path).unwrap() {
                walk(&row.unwrap().path(), out);
            }
        } else {
            assert!(meta.is_file());
            out.insert(path.to_owned(), Some(fs::read(path).unwrap()));
        }
    }
    let mut result = BTreeMap::new();
    walk(root, &mut result);
    result
}
fn epoch(request: &CaptureRequest) -> PathBuf {
    request
        .archive_root
        .join(format!("epoch-{:020}", request.epoch))
}
fn second(request: &CaptureRequest) -> CaptureRequest {
    CaptureRequest {
        epoch: 2,
        ..request.clone()
    }
}
fn seed(game: &mut Game<'_>, binding: &NativeBinding) -> SlotKey {
    assert!(game.level.world_spawn.is_none());
    game.level.world_spawn = Some([0, 64, 0]);
    let owner = game
        .lan
        .as_mut()
        .unwrap()
        .hub
        .guests
        .iter_mut()
        .find(|guest| guest.uuid == 11)
        .unwrap();
    assert_eq!(owner.spawn, Some([4.5, 65.0, -6.5]));
    owner.inventory[0] = Some(ItemStack::new("minecraft:stone", 10));
    let key = game
        .world
        .initialize_persistent_world_slot(binding, &target(game.dim).unwrap())
        .unwrap()
        .unwrap();
    let mut host = game.lan.take().unwrap();
    let result = game
        .as_guest(&mut host, 11, |game| {
            game.transfer_world_slot_inventory_cell(&key, 0, Direction::IntoMachine, 7)
        })
        .unwrap()
        .unwrap();
    assert!(matches!(result,TransferOutcome::Committed(result) if result.moved==7));
    game.lan = Some(host);
    assert!(game.player.is_none() && game.lan_acting.is_none());
    key
}
fn expected_root() -> Compound {
    let Tag::Compound(root) = Tag::compound([
        ("InhabitedTime", Tag::Long(0)),
        (
            "structures",
            Tag::compound([
                ("References", Tag::Compound(Compound::new())),
                ("starts", Tag::Compound(Compound::new())),
            ]),
        ),
        ("PostProcessing", Tag::List(vec![Tag::List(Vec::new()); 24])),
    ]) else {
        unreachable!()
    };
    root
}
fn enumerate_native_roots(game: &Game<'_>) {
    assert_eq!(game.world.chunks.len(), 9);
    for z in -1..=1 {
        for x in -1..=1 {
            let chunk = game.world.chunks.get(&(x, z)).unwrap();
            let saved = chunk
                .saved
                .as_ref()
                .expect("actual native saved column, never synthesized");
            eprintln!(
                "SLOT_RECAPTURE_NATIVE_ROOT column=({x},{z}) root={:?} last_update={} raw={} source={} order={} loot={} block_ticks={} fluid_ticks={} staged_be={} staged_entities={} postprocess={}",
                saved.root,
                saved.last_update,
                saved.block_entities.len(),
                saved.block_entity_sources.len(),
                saved.block_entity_order.len(),
                saved.rolled_loot.len(),
                saved.block_ticks.len(),
                saved.fluid_ticks.len(),
                chunk.block_entities.len(),
                chunk.entities.len(),
                chunk.postprocess.len()
            );
            // Unexpected native values stop here for source analysis. No normalization.
            assert_eq!(
                saved.root,
                expected_root(),
                "unexpected actual native root at ({x},{z})"
            );
            assert_eq!(saved.last_update, T);
            assert!(
                saved.block_entities.is_empty()
                    && saved.block_entity_sources.is_empty()
                    && saved.block_entity_order.is_empty()
                    && saved.rolled_loot.is_empty()
                    && saved.block_ticks.is_empty()
                    && saved.fluid_ticks.is_empty()
            );
            assert!(
                chunk.block_entities.is_empty()
                    && chunk.entities.is_empty()
                    && chunk.postprocess.is_empty()
            );
            assert!(chunk.lit);
        }
    }
}
#[derive(Debug, PartialEq)]
struct RootState {
    position: (i32, i32),
    chunk: usize,
    saved: usize,
    root: Compound,
    time: i64,
    raw: Vec<Tag>,
    sources: Vec<((i32, i32, i32), Tag)>,
    loot: Vec<((i32, i32, i32), String, Option<i64>, Tag)>,
    ticks: (Vec<SavedTick>, Vec<SavedTick>),
    order: Vec<(i32, i32, i32)>,
    staged: Vec<Tag>,
    postprocess: Vec<[i32; 3]>,
    lengths: [usize; 7],
}
#[derive(Debug, PartialEq)]
// Read-only diagnostic value copies are never used as restore/transfer inputs.
struct ItemState {
    label: String,
    value: ItemStack,
    allocations: Vec<(usize, i32)>,
}
#[derive(Debug, PartialEq)]
struct State {
    counter: CounterFrameV2,
    slot: Vec<u8>,
    players: Vec<Vec<u8>>,
    roots: Vec<RootState>,
    items: Vec<ItemState>,
    events: Vec<horizon_world::level::LevelEvent>,
    pending_metadata: String,
    clock: (i64, i64, u64, u64, [i32; 2]),
}
fn state(game: &Game<'_>) -> State {
    let (counter, slot) = game.slot_epoch_projection().unwrap();
    let players = game
        .slot_player_images()
        .unwrap()
        .into_iter()
        .flat_map(|row| [row.nbt, row.advancements, row.stats])
        .collect();
    let mut roots: Vec<_> = game
        .world
        .chunks
        .iter()
        .map(|(position, chunk)| {
            let saved = chunk.saved.as_ref().unwrap();
            RootState {
                position: *position,
                chunk: Arc::as_ptr(chunk) as usize,
                saved: Arc::as_ptr(saved) as usize,
                root: saved.root.clone(),
                time: saved.last_update,
                raw: saved.block_entities.clone(),
                sources: saved.block_entity_sources.clone(),
                loot: saved
                    .rolled_loot
                    .iter()
                    .map(|row| (row.pos, row.table.clone(), row.seed, row.items.clone()))
                    .collect(),
                ticks: (saved.block_ticks.clone(), saved.fluid_ticks.clone()),
                order: saved.block_entity_order.clone(),
                staged: chunk.entities.clone(),
                postprocess: chunk.postprocess.clone(),
                lengths: [
                    saved.block_entity_order.len(),
                    saved.rolled_loot.len(),
                    saved.block_ticks.len(),
                    saved.fluid_ticks.len(),
                    chunk.block_entities.len(),
                    chunk.entities.len(),
                    chunk.postprocess.len(),
                ],
            }
        })
        .collect();
    roots.sort_by_key(|row| row.position);
    let mut rng = game.world.random.clone();
    let mut pending: Vec<_> = game
        .pending
        .banner
        .iter()
        .map(|(p, v)| format!("banner{p:?}:{v:?}"))
        .chain(
            game.pending
                .names
                .iter()
                .map(|(p, v)| format!("name{p:?}:{v:?}")),
        )
        .chain(
            game.pending
                .copied
                .iter()
                .map(|(p, v)| format!("copied{p:?}:{v:?}")),
        )
        .collect();
    pending.sort();
    State {
        counter,
        slot,
        players,
        roots,
        items: ownership_rows(game),
        events: game.level_events.clone(),
        pending_metadata: format!(
            "{pending:?}:{:?}:{:?}:{:?}:{:?}:{:?}",
            game.spill.orbs,
            game.world.watched_writes,
            crate::game::brush::world_owner_test_brushing_snapshot(&game.brushing),
            game.world.moving_pistons,
            game.world.moving_piston_fallback
        ),
        clock: (
            game.world.game_time,
            game.world.day_time,
            game.world.revision,
            game.world.chunks.membership_revision(),
            [rng.next_int(), rng.next_int()],
        ),
    }
}
fn ownership_rows(game: &Game<'_>) -> Vec<ItemState> {
    fn visit(stack: &ItemStack, rows: &mut Vec<(usize, i32)>) {
        if let Some(data) = stack.extra.as_deref() {
            rows.push((data as *const _ as usize, data.bundle_selected.0));
            for (_, child) in &data.container {
                visit(child, rows);
            }
            for child in data.charged.iter().chain(&data.bundle_contents) {
                visit(child, rows);
            }
            if let Some(pots) = &data.pot_decorations {
                for child in pots.iter().flatten() {
                    visit(child, rows);
                }
            }
        }
    }
    let mut rows = Vec::new();
    let mut add = |label: String, stack: &ItemStack| {
        let mut allocations = Vec::new();
        visit(stack, &mut allocations);
        rows.push(ItemState {
            label,
            value: stack.clone(),
            allocations,
        });
    };
    for (pos, entity) in game.world.block_entities.iter() {
        for (index, stack) in entity.slots().iter().enumerate() {
            if let Some(stack) = stack {
                add(format!("world{pos:?}:{index}"), stack);
            }
        }
    }
    for (pos, chunk) in &game.world.chunks {
        for (block, entity) in &chunk.block_entities {
            for (index, stack) in entity.slots().iter().enumerate() {
                if let Some(stack) = stack {
                    add(format!("chunk{pos:?}:{block:?}:{index}"), stack);
                }
            }
        }
    }
    for (pos, items) in &game.pending.shulker {
        for (index, stack) in items {
            add(format!("shulker{pos:?}:{index}"), stack);
        }
    }
    for (pos, pots) in &game.pending.pot {
        for (index, stack) in pots.iter().enumerate() {
            if let Some(stack) = stack {
                add(format!("pot{pos:?}:{index}"), stack);
            }
        }
    }
    for (index, spawn) in game.spill.spawns.iter().enumerate() {
        add(
            format!(
                "spawn{index}:{:?}:{:?}:{}",
                spawn.pos, spawn.motion, spawn.delay
            ),
            &spawn.stack,
        );
    }
    for (pos, entity) in &game.spill.removed {
        for (index, stack) in entity.slots().iter().enumerate() {
            if let Some(stack) = stack {
                add(format!("removed{pos:?}:{index}"), stack);
            }
        }
    }
    rows.sort_by(|a, b| a.label.cmp(&b.label));
    rows
}
fn share(game: &Game<'_>, key: &SlotKey) {
    assert!(plain_stone(
        game.world.read_world_slot(key).unwrap().slot.stack(),
        7
    ));
    assert_eq!(game.world.read_world_slot(key).unwrap().slot.revision(), 1);
    assert!(plain_stone(
        game.lan
            .as_ref()
            .unwrap()
            .hub
            .guests
            .iter()
            .find(|guest| guest.uuid == 11)
            .unwrap()
            .inventory[0]
            .as_ref(),
        3
    ));
}
fn record(game: &Game<'_>) -> (u64, Option<i64>) {
    let snapshot = game.world.counter_trial_snapshot().unwrap();
    let record = snapshot.record().unwrap();
    (record.counter(), record.last_advanced())
}
fn canonical_players(root: &Path) {
    for (first, uuid) in [
        (true, uuid_text(OWNER_RECORD)),
        (false, uuid_text([0, 0, 0, 12])),
    ] {
        let mut expected =
            crate::game::levels::capture_integration::tests::expected_player(first, first, false);
        if first {
            let Some(Tag::List(items)) = expected.get_mut("Inventory") else {
                panic!()
            };
            let Tag::Compound(item) = items
                .iter_mut()
                .find(|tag| tag.get("Slot") == Some(&Tag::Byte(0)))
                .unwrap()
            else {
                panic!()
            };
            item.insert("count".into(), Tag::Int(3));
        }
        assert_eq!(
            strict_player(&fs::read(root.join(format!("players/data/{uuid}.dat"))).unwrap())
                .unwrap(),
            expected
        );
    }
}
struct History<'a> {
    source: Game<'a>,
    game: Game<'a>,
    binding: NativeBinding,
    request: CaptureRequest,
    counter: RuntimeCounterKey,
    slot: SlotKey,
    original_counter: RuntimeCounterKey,
    original_slot: SlotKey,
    receipt: RecoveryReceipt,
    _body: Option<PlayerMotion>,
    _source_rx: Vec<mpsc::Receiver<Out>>,
    _rx: Vec<mpsc::Receiver<Out>>,
    _held: Snapshot,
}
fn history<'a>(fixture: &'a Fixture) -> History<'a> {
    let (mut original, body, source_rx, identity, binding, original_counter, held) =
        source(fixture);
    let original_slot = seed(&mut original, &binding);
    let archive_root = original
        .world_root
        .parent()
        .unwrap()
        .join("recapture-archive");
    fs::create_dir(&archive_root).unwrap();
    let request = CaptureRequest {
        archive_root,
        epoch: 1,
        identity,
    };
    dispatch_slot_capture(&mut original, request.clone()).unwrap();
    let destination = original
        .world_root
        .parent()
        .unwrap()
        .join("recapture-working-world");
    let recovery = recover_slot_fixture_epoch(
        &request.archive_root,
        &request.identity,
        &destination,
        &binding,
        original.res.items(),
        original_counter.placement().world,
        &target(original.dim).unwrap(),
    )
    .unwrap();
    let (mut game, rx) = loaded(fixture, &destination);
    // Mandatory first runtime evidence, before a recapture or any fixture mutation.
    enumerate_native_roots(&game);
    let (counter, slot) = game
        .install_slot_epoch(
            &binding,
            original_counter.placement().world,
            &recovery.counter,
            recovery.slot,
        )
        .unwrap();
    share(&game, &slot);
    assert_eq!(record(&game), (3, Some(T)));
    History {
        source: original,
        game,
        binding,
        request,
        counter,
        slot,
        original_counter,
        original_slot,
        receipt: recovery.receipt,
        _body: body,
        _source_rx: source_rx,
        _rx: rx,
        _held: held,
    }
}
fn recover(history: &History<'_>, archive: &Path, destination: &Path) -> CResult<SlotRecovery> {
    recover_slot_fixture_epoch(
        archive,
        &history.request.identity,
        destination,
        &history.binding,
        history.game.res.items(),
        history.counter.placement().world,
        &target(Dim::Overworld).unwrap(),
    )
}
fn copied_world(image: &Path, destination: &Path) {
    let expected: Disk = disk(&image.join("world"))
        .into_iter()
        .map(|(path, bytes)| {
            (
                destination.join(path.strip_prefix(image.join("world")).unwrap()),
                bytes,
            )
        })
        .collect();
    let mut actual = disk(destination);
    actual.remove(&destination.join("session.lock"));
    assert_eq!(actual, expected);
}
fn copy_epoch(source: &Path, destination: &Path) {
    let entries = tree(source, &[], Phase::Copy).unwrap();
    fresh_dir(destination, Phase::Copy).unwrap();
    for dir in entries.directories {
        fresh_dir(&destination.join(dir), Phase::Copy).unwrap();
    }
    for member in entries.files {
        physical_copy(
            &source.join(&member.path),
            &destination.join(&member.path),
            &member,
            Phase::Copy,
        )
        .unwrap();
    }
}
#[test]
#[ignore = "requires isolated actual Game resources and coordinator-bound identities; run FIRST for native-root enumeration"]
fn slot_recapture_native_roots_second_epoch_and_fresh_continuation() {
    let fixture = Fixture::new("slot-recapture-positive");
    let mut history = history(&fixture);
    let first = epoch(&history.request);
    let first_bytes = disk(&first);
    let original = disk(&history.source.world_root);
    let original_state = state_source(&history.source);
    assert_eq!(history.receipt.epoch, 1);
    assert_ne!(
        history.counter.session_nonce(),
        history.original_counter.session_nonce()
    );
    assert_ne!(history.slot, history.original_slot);
    assert!(
        history
            .game
            .world
            .read_persistent_counter(&history.original_counter)
            .is_err()
    );
    assert!(
        history
            .game
            .world
            .read_world_slot(&history.original_slot)
            .is_err()
    );
    assert!(history.game.observe_checkpoint_world_owners().is_err());
    history.game.observe_slot_recapture_world_owners().unwrap();
    assert_eq!(
        history
            .game
            .capture_profile_checks(CaptureProfile::NativeSlotPatch)
            .unwrap_err()
            .code,
        Failure::World
    );
    let live = state(&history.game);
    let working_before = disk(&history.game.world_root);
    let inherited = InheritedSlotFiles::read(&history.game.world_root, Phase::Validate).unwrap();
    let request = second(&history.request);
    let receipt = dispatch_slot_recapture(&mut history.game, request.clone()).unwrap();
    assert_eq!(receipt.epoch, 2);
    assert_eq!(state(&history.game), live);
    inherited
        .require_unchanged(&history.game.world_root, Phase::Save)
        .unwrap();
    assert_native_footprint(&history.game.world_root, working_before);
    let second = epoch(&request);
    verify_complete_envelope(&second, 2, &request.identity).unwrap();
    canonical_players(&second.join("world"));
    let second_bytes = disk(&second);
    let working_bytes = disk(&history.game.world_root);
    let destination = history
        .game
        .world_root
        .parent()
        .unwrap()
        .join("recapture-final-world");
    let recovery = recover(&history, &request.archive_root, &destination).unwrap();
    assert_eq!(recovery.receipt.epoch, 2);
    copied_world(&second, &destination);
    canonical_players(&destination);
    let (mut final_game, _rx) = loaded(&fixture, &destination);
    enumerate_native_roots(&final_game);
    let (counter, slot) = final_game
        .install_slot_epoch(
            &history.binding,
            history.counter.placement().world,
            &recovery.counter,
            recovery.slot,
        )
        .unwrap();
    assert_ne!(counter.session_nonce(), history.counter.session_nonce());
    assert_ne!(slot, history.slot);
    assert!(
        final_game
            .world
            .read_persistent_counter(&history.counter)
            .is_err()
    );
    assert!(final_game.world.read_world_slot(&history.slot).is_err());
    share(&final_game, &slot);
    assert_eq!(record(&final_game), (3, Some(T)));
    final_game.tick_counter_trial(true);
    assert_eq!(record(&final_game), (3, Some(T)));
    final_game.world.game_time = T + 1;
    final_game.tick_counter_trial(true);
    final_game.tick_counter_trial(true);
    assert_eq!(record(&final_game), (4, Some(T + 1)));
    share(&final_game, &slot);
    assert_eq!(disk(&first), first_bytes);
    assert_eq!(disk(&second), second_bytes);
    assert_eq!(disk(&history.source.world_root), original);
    assert_eq!(state_source(&history.source), original_state);
    assert_eq!(disk(&history.game.world_root), working_bytes);
}
fn state_source(game: &Game<'_>) -> (CounterFrameV2, Vec<u8>, Vec<Vec<u8>>) {
    let (counter, slot) = game.slot_epoch_projection().unwrap();
    let players = game
        .slot_player_images()
        .unwrap()
        .into_iter()
        .flat_map(|row| [row.nbt, row.advancements, row.stats])
        .collect();
    (counter, slot, players)
}
fn assert_native_footprint(root: &Path, mut before: Disk) {
    let mut after = disk(root);
    let mut files = Vec::new();
    for x in -1..=0 {
        for z in -1..=0 {
            files.push(
                horizon_client::save::dimension_dir(root, Dim::Overworld.id())
                    .join(format!("region/r.{x}.{z}.mca")),
            );
        }
    }
    files.push(root.join("level.dat"));
    for name in [
        "weather",
        "world_clocks",
        "world_gen_settings",
        "game_rules",
        "random_sequences",
    ] {
        files.push(root.join(format!("data/minecraft/{name}.dat")));
    }
    for (kind, ext) in [("data", "dat"), ("advancements", "json"), ("stats", "json")] {
        files.push(root.join(format!(
            "players/{kind}/ffffffff-ffff-fffe-0000-000300000004.{ext}"
        )));
    }
    for p in files.clone() {
        if p.extension().and_then(|s| s.to_str()) == Some("dat") {
            let name = p.file_name().unwrap().to_str().unwrap();
            files.push(p.with_file_name(format!("{name}_new")));
            files.push(p.with_file_name(format!("{name}_old")));
        }
    }
    let mut parents = BTreeSet::new();
    for p in files {
        let mut parent = p.parent();
        while let Some(d) = parent {
            if !d.starts_with(root) {
                break;
            }
            parents.insert(d.to_owned());
            parent = d.parent();
        }
        let old = before.remove(&p);
        let new = after.remove(&p);
        if old != new {
            assert!(old.as_ref().is_none_or(Option::is_some));
            assert!(new.as_ref().is_some_and(Option::is_some));
        }
    }
    for p in parents {
        if !before.contains_key(&p) && after.get(&p) == Some(&None) {
            after.remove(&p);
        }
    }
    assert_eq!(after, before, "unknown native save write/type/deletion");
}

fn refuse(history: &mut History<'_>, label: &str, code: Failure) {
    eprintln!("SLOT_RECAPTURE_REFUSAL {label}");
    let before = state(&history.game);
    let working = disk(&history.game.world_root);
    let archive = disk(&history.request.archive_root);
    let original = disk(&history.source.world_root);
    let error = dispatch_slot_recapture(&mut history.game, second(&history.request)).unwrap_err();
    assert_eq!(error.code, code, "{label}: {error:?}");
    assert_eq!(error.publication, Publication::NotPublished);
    assert_eq!(state(&history.game), before);
    share(&history.game, &history.slot);
    assert_eq!(disk(&history.game.world_root), working);
    assert_eq!(disk(&history.request.archive_root), archive);
    assert_eq!(disk(&history.source.world_root), original);
}
fn world_refuse(history: &mut History<'_>, label: &str) {
    history.game.observe_slot_epoch_entity_owners().unwrap();
    assert!(
        history.game.observe_slot_recapture_world_owners().is_err(),
        "{label}"
    );
    refuse(history, label, Failure::World);
}
fn raw() -> Tag {
    Tag::compound([
        ("opaque", Tag::ByteArray(vec![0, 128, 255])),
        (
            "items",
            Tag::List(vec![Tag::compound([
                ("id", Tag::String("minecraft:stone".into())),
                ("count", Tag::Int(7)),
            ])]),
        ),
    ])
}
fn held_allocations(entity: &BlockEntity) -> Vec<(usize, i32)> {
    fn visit(stack: &ItemStack, rows: &mut Vec<(usize, i32)>) {
        if let Some(data) = stack.extra.as_deref() {
            rows.push((data as *const _ as usize, data.bundle_selected.0));
            for (_, child) in &data.container {
                visit(child, rows);
            }
            for child in data.charged.iter().chain(&data.bundle_contents) {
                visit(child, rows);
            }
            if let Some(pots) = &data.pot_decorations {
                for child in pots.iter().flatten() {
                    visit(child, rows);
                }
            }
        }
    }
    let mut rows = Vec::new();
    for stack in entity.slots().iter().flatten() {
        visit(stack, &mut rows);
    }
    rows
}
#[test]
#[ignore = "requires isolated actual Game resources and coordinator-bound identities"]
fn slot_recapture_root_shapes_and_all_remaining_world_refusals() {
    let fixture = Fixture::new("slot-recapture-world-refusals");
    let mut history = history(&fixture);
    for case in 0..13 {
        let original = history.game.world.chunks.get(&(-1, -1)).unwrap().clone();
        let mut chunk = (*original).clone();
        let saved = Arc::make_mut(chunk.saved.as_mut().unwrap());
        match case {
            0 => {
                saved.root.remove(&"InhabitedTime".into());
            }
            1 => {
                saved.root.insert("fixture:opaque".into(), raw());
            }
            2 => {
                saved.root.insert("InhabitedTime".into(), Tag::Int(0));
            }
            3 => {
                saved.root.insert("InhabitedTime".into(), Tag::Long(1));
            }
            4 => {
                saved.root.insert(
                    "structures".into(),
                    Tag::compound([("References", Tag::Compound(Compound::new()))]),
                );
            }
            5 => {
                saved.root.insert(
                    "structures".into(),
                    Tag::compound([
                        ("References", raw()),
                        ("starts", Tag::Compound(Compound::new())),
                    ]),
                );
            }
            6 => {
                saved.root.insert(
                    "structures".into(),
                    Tag::compound([
                        ("References", Tag::Compound(Compound::new())),
                        ("starts", raw()),
                    ]),
                );
            }
            7 => {
                saved.root.insert(
                    "PostProcessing".into(),
                    Tag::List(vec![Tag::List(Vec::new()); 23]),
                );
            }
            8 => {
                saved.root.insert(
                    "PostProcessing".into(),
                    Tag::List(vec![Tag::List(Vec::new()); 25]),
                );
            }
            9 => {
                saved
                    .root
                    .insert("PostProcessing".into(), Tag::List(vec![Tag::Int(0); 24]));
            }
            10 => {
                let mut marks = vec![Tag::List(Vec::new()); 24];
                marks[0] = Tag::List(vec![Tag::Short(0)]);
                saved.root.insert("PostProcessing".into(), Tag::List(marks));
            }
            11 => saved.last_update = T - 1,
            12 => saved.block_entity_order.push((-1, 71, -1)),
            _ => unreachable!(),
        }
        history.game.world.chunks.insert((-1, -1), Arc::new(chunk));
        world_refuse(&mut history, &format!("root-shape-{case}"));
        history.game.world.chunks.insert((-1, -1), original);
    }
    for kind in [
        K::StagedBlockEntities,
        K::StagedEntities,
        K::SavedBlockEntities,
        K::SavedBlockEntitySources,
        K::RolledLoot,
        K::Postprocess,
        K::SavedBlockTicks,
        K::SavedFluidTicks,
    ] {
        let original = history.game.world.chunks.get(&(-1, -1)).unwrap().clone();
        let mut chunk = (*original).clone();
        let saved = Arc::make_mut(chunk.saved.as_mut().unwrap());
        let tick = SavedTick {
            kind: TickTypeId(3),
            pos: BlockPos::new(-1, 71, -1),
            delay: -7,
            priority: TickPriority::High,
        };
        match kind {
            K::StagedBlockEntities => chunk.block_entities.push((
                (-1, 71, -1),
                BlockEntity::Chest(vec![Some(crate::game::levels::entity_owner_tests::rich(7))]),
            )),
            K::StagedEntities => chunk.entities.push(raw()),
            K::SavedBlockEntities => saved.block_entities.push(raw()),
            K::SavedBlockEntitySources => saved.block_entity_sources.push(((-1, 71, -1), raw())),
            K::RolledLoot => saved.rolled_loot.push(RolledLoot {
                pos: (-1, 71, -1),
                table: "fixture:loot".into(),
                seed: Some(-3),
                items: raw(),
            }),
            K::Postprocess => chunk.postprocess.push([-1, 71, -1]),
            K::SavedBlockTicks => saved.block_ticks.push(tick),
            K::SavedFluidTicks => saved.fluid_ticks.push(tick),
            _ => unreachable!(),
        }
        history.game.world.chunks.insert((-1, -1), Arc::new(chunk));
        assert_eq!(
            history
                .game
                .world
                .checkpoint_world_owner_for_native_empty_roots()
                .unwrap()
                .kind,
            kind
        );
        world_refuse(&mut history, &format!("column-{kind:?}"));
        history.game.world.chunks.insert((-1, -1), original);
    }
    let p = [1, 71, 1];
    let key = (1, 71, 1);
    let expected_payload = [Some(crate::game::levels::entity_owner_tests::rich(7))];
    history.game.world.block_entities.insert(
        key,
        BlockEntity::Chest(vec![Some(crate::game::levels::entity_owner_tests::rich(7))]),
    );
    world_refuse(&mut history, "pending-native-ticker");
    history.game.world.block_entities.tickers(|_| 0);
    world_refuse(&mut history, "native-container");
    let held = history.game.world.block_entities.take(&key).unwrap();
    let pointer = held.slots().as_ptr();
    let allocations = held_allocations(&held);
    assert_eq!(held.slots(), &expected_payload);
    assert_eq!(
        allocations.iter().map(|row| row.1).collect::<Vec<_>>(),
        [4, 9, 9, 9, 9]
    );
    world_refuse(&mut history, "borrowed-container");
    assert_eq!(held.slots().as_ptr(), pointer);
    assert_eq!(held.slots(), &expected_payload);
    assert_eq!(held_allocations(&held), allocations);
    history.game.world.block_entities.put(key, held);
    let removed = history.game.world.block_entities.remove(&key).unwrap();
    world_refuse(&mut history, "added-native-ticker");
    assert_eq!(removed.slots(), &expected_payload);
    assert_eq!(held_allocations(&removed), allocations);
    history.game.world.block_entities.tickers(|_| 0);
    let moving = horizon_world::piston_shape::MovingPistonState {
        moved_state: history.game.res.facts().air,
        direction: horizon_core::geometry::Direction::Up,
        extending: true,
        source: false,
        progress: 0.25,
        previous_progress: 0.0,
    };
    history.game.world.watched_writes.push((key, 13, 0));
    world_refuse(&mut history, "watched");
    history.game.world.watched_writes.clear();
    history.game.world.moving_pistons.push((p, moving));
    world_refuse(&mut history, "pistons");
    history.game.world.moving_pistons.clear();
    history.game.world.moving_piston_fallback.push((p, moving));
    world_refuse(&mut history, "piston-fallback");
    history.game.world.moving_piston_fallback.clear();
    for kind in [
        PendingKind::Shulker,
        PendingKind::Banner,
        PendingKind::Names,
        PendingKind::Pot,
        PendingKind::Copied,
    ] {
        match kind {
            PendingKind::Shulker => {
                history
                    .game
                    .pending
                    .shulker
                    .insert(p, vec![(2, ItemStack::new("minecraft:stone", 7))]);
            }
            PendingKind::Banner => {
                history.game.pending.banner.insert(p, Banner::default());
            }
            PendingKind::Names => {
                history
                    .game
                    .pending
                    .names
                    .insert(p, ("minecraft:chest".into(), "retained".into()));
            }
            PendingKind::Pot => {
                history.game.pending.pot.insert(
                    p,
                    [None, Some(ItemStack::new("minecraft:stone", 7)), None, None],
                );
            }
            PendingKind::Copied => {
                history
                    .game
                    .pending
                    .copied
                    .insert(p, ("minecraft:player_head".into(), Compound::new()));
            }
        }
        world_refuse(&mut history, &format!("pending-{kind:?}"));
        match kind {
            PendingKind::Shulker => {
                history.game.pending.shulker.remove(&p);
            }
            PendingKind::Banner => {
                history.game.pending.banner.remove(&p);
            }
            PendingKind::Names => {
                history.game.pending.names.remove(&p);
            }
            PendingKind::Pot => {
                history.game.pending.pot.remove(&p);
            }
            PendingKind::Copied => {
                history.game.pending.copied.remove(&p);
            }
        }
    }
    history
        .game
        .level_events
        .push(horizon_world::level::LevelEvent::LevelEvent {
            id: 2001,
            pos: BlockPos::new(1, 71, 1),
            data: 1,
        });
    world_refuse(&mut history, "level-event");
    history.game.level_events.clear();
    history.game.spill.spawns.push(crate::game::spill::Spawn {
        stack: ItemStack::new("minecraft:stone", 7),
        pos: [1.5, 71.0, 1.5],
        motion: None,
        delay: 37,
    });
    world_refuse(&mut history, "spill-spawns");
    history.game.spill.spawns.pop();
    history.game.spill.removed.push((
        p,
        BlockEntity::Chest(vec![Some(ItemStack::new("minecraft:stone", 7))]),
    ));
    world_refuse(&mut history, "spill-removed");
    history.game.spill.removed.pop();
    history.game.spill.orbs.push(([1.5, 71.0, 1.5], 7, -7));
    world_refuse(&mut history, "spill-orbs");
    history.game.spill.orbs.pop();
    history
        .game
        .brushing
        .insert(p, crate::game::brush::world_owner_test_brush_state());
    let brushing = crate::game::brush::world_owner_test_brushing_snapshot(&history.game.brushing);
    world_refuse(&mut history, "brushing");
    assert_eq!(
        crate::game::brush::world_owner_test_brushing_snapshot(&history.game.brushing),
        brushing
    );
    history.game.brushing.remove(&p);
    history.game.item_columns = Some(crate::game::machines::ItemColumns::default());
    world_refuse(&mut history, "hopper");
    history.game.item_columns = None;
    assert!(
        history.game.cmds.pregen.is_none(),
        "private native pregen task remains source-only; never construct/pump it to inspect"
    );
    // Library category wins over a pending Game owner; no opaque-root catch skips either.
    history.game.pending.shulker.insert(p, vec![]);
    history.game.world.watched_writes.push((key, 13, 0));
    assert!(
        matches!(history.game.observe_slot_recapture_world_owners(),Err(WorldOwnerObservationError::World {cause,..}) if cause.kind==K::WatchedWrites)
    );
    world_refuse(&mut history, "deterministic-priority");
    history.game.world.watched_writes.clear();
    history.game.pending.shulker.remove(&p);
    assert!(history.game.observe_checkpoint_world_owners().is_err());
    assert_eq!(
        history
            .game
            .capture_profile_checks(CaptureProfile::NativeSlotPatch)
            .unwrap_err()
            .code,
        Failure::World
    );
    let mut host = history.game.lan.take().unwrap();
    history
        .game
        .as_guest(&mut host, 11, |game| {
            assert_eq!(
                game.observe_slot_recapture_world_owners().err(),
                Some(WorldOwnerObservationError::ActingGuest)
            )
        })
        .unwrap();
    history.game.lan = Some(host);
    let role = history.game.options.execution_role;
    history.game.options.execution_role = crate::game::execution_role::ExecutionRole::Client;
    assert_eq!(
        history.game.observe_slot_recapture_world_owners().err(),
        Some(WorldOwnerObservationError::NotAuthoritative)
    );
    history.game.options.execution_role = role;
    history.game.enter_level(None, Dim::Nether).unwrap();
    history.game.enter_level(None, Dim::Overworld).unwrap();
    let parked = history.game.levels.parked.remove(&Dim::Nether).unwrap();
    history.game.levels.parked.insert(Dim::Overworld, parked);
    assert_eq!(
        history.game.observe_slot_recapture_world_owners().err(),
        Some(WorldOwnerObservationError::DuplicateActiveDimension)
    );
    share(&history.game, &history.slot);
}

fn put_field(tag: &mut Tag, key: &str, value: Tag) {
    let Tag::Compound(map) = tag else {
        panic!("fixture compound")
    };
    map.insert(key.into(), value);
}
fn slot_bytes(bytes: &[u8], edit: impl FnOnce(&mut Tag)) -> Vec<u8> {
    let (mut named, used) = nbt::read_named(
        bytes,
        Limits {
            max_bytes: 1 << 20,
            max_nodes: 16_384,
            max_depth: 32,
        },
    )
    .unwrap();
    assert_eq!(used, bytes.len());
    edit(&mut named.tag);
    nbt::write_named(
        &named,
        Limits {
            max_bytes: 1 << 20,
            max_nodes: 16_384,
            max_depth: 32,
        },
    )
    .unwrap()
}
fn prepare_recapture(
    history: &mut History<'_>,
    request: CaptureRequest,
) -> (PendingImage, InheritedSlotFiles, Vec<u8>, Vec<u8>) {
    let (counter, slot) = history.game.slot_epoch_projection().unwrap();
    let files = history
        .game
        .checked_inherited_slot_files(&history.binding, &counter, &slot)
        .unwrap();
    let material = history
        .game
        .profile_material(request, CaptureProfile::NativeSlotRecapture)
        .unwrap();
    files
        .require_unchanged(&history.game.world_root, Phase::Save)
        .unwrap();
    let mut image = material.copy().unwrap();
    image.overlay().unwrap();
    (image, files, counter.encode().unwrap(), slot)
}
#[test]
#[ignore = "requires isolated actual Game resources and coordinator-bound identities"]
fn slot_recapture_inherited_validation_and_pending_overlay_preserve_sources() {
    let fixture = Fixture::new("slot-recapture-inputs");
    let mut history = history(&fixture);
    let first = epoch(&history.request);
    let frozen = disk(&first);
    let original = disk(&history.source.world_root);
    let counter_path = history.game.world_root.join(COUNTER_FILE);
    let slot_path = history.game.world_root.join(SLOT_FILE);
    let counter = fs::read(&counter_path).unwrap();
    let slot = fs::read(&slot_path).unwrap();
    for case in [
        "missing-counter",
        "missing-slot",
        "directory-counter",
        "directory-slot",
        "symlink-counter",
        "symlink-slot",
        "counter-bound",
        "slot-bound",
        "counter-codec",
        "slot-codec",
        "counter-canonical",
        "world",
        "dimension",
        "target",
        "package",
        "type",
        "state",
        "schema",
        "time",
        "head",
        "generation",
        "slot-world",
        "slot-time",
        "slot-generation",
        "slot-revision",
        "count",
        "unsupported",
    ] {
        let path = if case.ends_with("slot")
            || case.starts_with("slot-")
            || matches!(case, "count" | "unsupported")
        {
            &slot_path
        } else {
            &counter_path
        };
        let mut link_target = None;
        let expected = match case {
            "missing-counter" | "missing-slot" => {
                fs::remove_file(path).unwrap();
                Failure::Io
            }
            "directory-counter" | "directory-slot" => {
                fs::remove_file(path).unwrap();
                fs::create_dir(path).unwrap();
                Failure::Limit
            }
            "symlink-counter" | "symlink-slot" => {
                let target = history
                    .game
                    .world_root
                    .parent()
                    .unwrap()
                    .join(format!("{case}-target"));
                fs::write(
                    &target,
                    if case.ends_with("slot") {
                        &slot
                    } else {
                        &counter
                    },
                )
                .unwrap();
                fs::remove_file(path).unwrap();
                std::os::unix::fs::symlink(&target, path).unwrap();
                link_target = Some(target);
                Failure::Path
            }
            "counter-bound" | "slot-bound" => {
                fs::write(
                    path,
                    vec![
                        0;
                        if case == "slot-bound" {
                            (1 << 20) + 1
                        } else {
                            (16 << 10) + 1
                        }
                    ],
                )
                .unwrap();
                Failure::Limit
            }
            "counter-codec" | "slot-codec" => {
                fs::write(path, b"malformed inherited codec").unwrap();
                Failure::InvalidImage
            }
            "counter-canonical" => {
                let mut changed = counter.clone();
                changed.push(b'\n');
                fs::write(path, changed).unwrap();
                Failure::InvalidImage
            }
            "slot-world" | "slot-time" | "slot-generation" | "slot-revision" => {
                let bytes = slot_bytes(&slot, |tag| match case {
                    "slot-world" => put_field(
                        tag,
                        "world_uuid",
                        Tag::String("00000000-0000-4000-8000-000000000123".into()),
                    ),
                    "slot-time" => put_field(tag, "saved_game_time", Tag::Long(T - 1)),
                    "slot-generation" => {
                        put_field(
                            tag,
                            "slot_high_water",
                            Tag::ByteArray(2u64.to_be_bytes().to_vec()),
                        );
                        put_field(
                            tag.get_mut("ownership").unwrap(),
                            "slot_generation",
                            Tag::ByteArray(2u64.to_be_bytes().to_vec()),
                        );
                    }
                    "slot-revision" => put_field(
                        tag.get_mut("ownership").unwrap(),
                        "revision",
                        Tag::ByteArray(2u64.to_be_bytes().to_vec()),
                    ),
                    _ => unreachable!(),
                });
                fs::write(path, bytes).unwrap();
                Failure::Profile
            }
            "count" | "unsupported" => {
                let bytes = slot_bytes(&slot, |tag| {
                    let value = tag
                        .get_mut("ownership")
                        .unwrap()
                        .get_mut("stack")
                        .unwrap()
                        .get_mut("value")
                        .unwrap();
                    if case == "count" {
                        put_field(value, "count", Tag::Int(6));
                    } else {
                        put_field(value, "id", Tag::String("fixture:unsupported_item".into()));
                    }
                });
                fs::write(path, bytes).unwrap();
                if case == "count" {
                    Failure::Profile
                } else {
                    Failure::InvalidImage
                }
            }
            _ => {
                let mut frame: Value = parse_json(&counter, 16 << 10).unwrap();
                match case {
                    "world" => frame["world_uuid"] = "00000000-0000-4000-8000-000000000123".into(),
                    "dimension" => frame["dimension"] = Dim::Nether.id().into(),
                    "target" => frame["position"] = serde_json::json!([3, 70, 2]),
                    "package" => frame["package_identity"] = "f".repeat(64).into(),
                    "type" => frame["type_id"] = "capture_counter:other".into(),
                    "state" => frame["observed_state"]["name"] = "minecraft:stone".into(),
                    "schema" => frame["save_schema"] = 2.into(),
                    "time" => frame["saved_game_time"] = (T - 1).into(),
                    "head" => frame["allocation_high_water"] = 2.into(),
                    "generation" => {
                        frame["allocation_high_water"] = 2.into();
                        frame["placement"]["generation"] = 2.into();
                    }
                    _ => unreachable!(),
                };
                fs::write(path, serde_json::to_vec(&frame).unwrap()).unwrap();
                if matches!(case, "state" | "schema" | "time") {
                    Failure::InvalidImage
                } else {
                    Failure::Profile
                }
            }
        };
        refuse(&mut history, case, expected);
        if let Some(target) = link_target {
            assert_eq!(fs::read_link(path).unwrap(), target);
            assert_eq!(
                fs::read(&target).unwrap(),
                if case.ends_with("slot") {
                    slot.clone()
                } else {
                    counter.clone()
                }
            );
            fs::remove_file(path).unwrap();
        } else if path.is_dir() {
            fs::remove_dir(path).unwrap();
        }
        fs::write(&counter_path, &counter).unwrap();
        fs::write(&slot_path, &slot).unwrap();
    }
    // Mutations are deliberate invalid inputs between validations, never a hook in
    // capture or native save. Both pending originals must match before either write.
    for (index, case) in [
        "copied-counter",
        "copied-slot",
        "source-counter",
        "source-slot",
        "copied-directory",
        "player-overlay-directory",
        "player-overlay-parent-file",
    ]
    .into_iter()
    .enumerate()
    {
        let request = CaptureRequest {
            epoch: 2 + index as u64,
            ..history.request.clone()
        };
        let (mut image, files, counter_out, slot_out) = prepare_recapture(&mut history, request);
        let copied_counter = image.root.join(FRAME_PATH);
        let copied_slot = image.root.join(SLOT_MEMBER);
        match case {
            "copied-counter" => fs::write(&copied_counter, b"changed copied counter").unwrap(),
            "copied-slot" => fs::write(&copied_slot, b"changed copied slot").unwrap(),
            "source-counter" => fs::write(&counter_path, b"changed source counter").unwrap(),
            "source-slot" => fs::write(&slot_path, b"changed source slot").unwrap(),
            "copied-directory" => {
                fs::remove_file(&copied_slot).unwrap();
                fs::create_dir(&copied_slot).unwrap();
            }
            "player-overlay-directory" => {
                let owner = image
                    .players
                    .iter()
                    .find(|row| row.descriptor.integrated_owner)
                    .unwrap();
                let target = image.root.join(player_target(&owner.descriptor, "data"));
                fs::remove_file(&target).unwrap();
                fs::create_dir(&target).unwrap();
            }
            "player-overlay-parent-file" => {
                let directory = image.root.join("world/players/data");
                for row in fs::read_dir(&directory).unwrap() {
                    fs::remove_file(row.unwrap().path()).unwrap();
                }
                fs::remove_dir(&directory).unwrap();
                fs::write(&directory, b"retained obstruction").unwrap();
            }
            _ => unreachable!(),
        }
        let stage = disk(&image.root);
        let working = disk(&history.game.world_root);
        let live = state(&history.game);
        let error = if case.starts_with("player-overlay") {
            image.overlay().unwrap_err()
        } else {
            image
                .replace_inherited_slot_files(
                    &history.game.world_root,
                    &files,
                    &counter_out,
                    &slot_out,
                )
                .unwrap_err()
        };
        if case == "player-overlay-parent-file" {
            assert_eq!(error.code, Failure::Io);
        } else {
            assert_eq!(error.phase, Phase::Overlay);
        }
        assert_eq!(error.publication, Publication::NotPublished);
        assert_eq!(disk(&image.root), stage);
        assert_eq!(disk(&history.game.world_root), working);
        assert_eq!(state(&history.game), live);
        share(&history.game, &history.slot);
        assert_eq!(disk(&first), frozen);
        assert_eq!(disk(&history.source.world_root), original);
        fs::write(&counter_path, &counter).unwrap();
        fs::write(&slot_path, &slot).unwrap();
    }
    // Valid copied input is replaced only in pending, then a genuine native
    // manifest descriptor obstruction still leaves publication absent.
    let request = CaptureRequest {
        epoch: 9,
        ..history.request.clone()
    };
    let (mut image, files, counter_out, slot_out) = prepare_recapture(&mut history, request);
    image
        .replace_inherited_slot_files(&history.game.world_root, &files, &counter_out, &slot_out)
        .unwrap();
    files
        .require_unchanged(&history.game.world_root, Phase::Save)
        .unwrap();
    fs::create_dir(image.root.join("manifest.json.tmp")).unwrap();
    let before = disk(&image.root);
    let error = image
        .write_manifest_temp_for(RecoveryProfile::ActiveSlot)
        .unwrap_err();
    assert_eq!(error.code, Failure::InvalidImage);
    assert_eq!(error.publication, Publication::NotPublished);
    assert_eq!(disk(&image.root), before);
    assert_eq!(disk(&first), frozen);
    assert_eq!(disk(&history.source.world_root), original);
}

fn rewrite_member(root: &Path, path: &str, bytes: &[u8]) {
    fs::write(root.join(path), bytes).unwrap();
    let mut manifest: SlotManifest = parse_json(
        &fs::read(root.join("manifest.json")).unwrap(),
        MANIFEST_LIMIT,
    )
    .unwrap();
    let (length, hash) = file_hash(&root.join(path), Phase::Recover).unwrap();
    let member = manifest
        .files
        .iter_mut()
        .find(|row| row.path == path)
        .unwrap();
    member.bytes = length;
    member.sha256 = hash;
    fs::write(
        root.join("manifest.json"),
        json_bytes(&manifest, MANIFEST_LIMIT).unwrap(),
    )
    .unwrap();
}
fn fork_history(history: &History<'_>, label: &str) -> (PathBuf, PathBuf) {
    let archive = history.game.world_root.parent().unwrap().join(label);
    fs::create_dir(&archive).unwrap();
    copy_epoch(
        &epoch(&history.request),
        &archive.join("epoch-00000000000000000001"),
    );
    let newer = archive.join("epoch-00000000000000000002");
    copy_epoch(&epoch(&second(&history.request)), &newer);
    (archive, newer)
}
fn semantic_recovery_refusals(history: &History<'_>) {
    // Corrected member hashes must not hide a selected semantic refusal behind
    // whole-epoch corruption fallback or mix epoch1's older slot into epoch2.
    for (index, case) in [
        "slot-count",
        "slot-world",
        "slot-revision",
        "counter-codec",
        "archive-resident",
        "manifest-slot-path",
        "required-counter-entry",
        "required-slot-entry",
    ]
    .into_iter()
    .enumerate()
    {
        let (archive, newer) = fork_history(history, &format!("recapture-semantic-{index}"));
        match case {
            "slot-count" | "slot-world" | "slot-revision" => {
                let old = fs::read(newer.join(SLOT_MEMBER)).unwrap();
                let bytes = slot_bytes(&old, |tag| match case {
                    "slot-count" => put_field(
                        tag.get_mut("ownership")
                            .unwrap()
                            .get_mut("stack")
                            .unwrap()
                            .get_mut("value")
                            .unwrap(),
                        "count",
                        Tag::Int(6),
                    ),
                    "slot-world" => put_field(
                        tag,
                        "world_uuid",
                        Tag::String("00000000-0000-4000-8000-000000000123".into()),
                    ),
                    "slot-revision" => put_field(
                        tag.get_mut("ownership").unwrap(),
                        "revision",
                        Tag::ByteArray(2u64.to_be_bytes().to_vec()),
                    ),
                    _ => unreachable!(),
                });
                rewrite_member(&newer, SLOT_MEMBER, &bytes);
            }
            "counter-codec" => rewrite_member(&newer, FRAME_PATH, b"selected malformed counter"),
            "archive-resident" => {
                let mut value: Value = parse_json(
                    &fs::read(newer.join("research/counter.json")).unwrap(),
                    JSON_LIMIT,
                )
                .unwrap();
                value["resident"] = false.into();
                rewrite_member(
                    &newer,
                    "research/counter.json",
                    &serde_json::to_vec(&value).unwrap(),
                );
            }
            _ => {
                let mut value: Value = parse_json(
                    &fs::read(newer.join("manifest.json")).unwrap(),
                    MANIFEST_LIMIT,
                )
                .unwrap();
                if case == "manifest-slot-path" {
                    value["slot_path"] = 17.into();
                } else {
                    let path = if case == "required-counter-entry" {
                        FRAME_PATH
                    } else {
                        SLOT_MEMBER
                    };
                    value["files"]
                        .as_array_mut()
                        .unwrap()
                        .retain(|row| row["path"] != path);
                }
                fs::write(
                    newer.join("manifest.json"),
                    serde_json::to_vec(&value).unwrap(),
                )
                .unwrap();
            }
        }
        let frozen = disk(&archive);
        let destination = history
            .game
            .world_root
            .parent()
            .unwrap()
            .join(format!("recapture-semantic-destination-{index}"));
        let error = recover(history, &archive, &destination).unwrap_err();
        assert!(
            matches!(error.code, Failure::Profile | Failure::InvalidImage),
            "{case}: {error:?}"
        );
        assert!(!destination.exists(), "{case}: refusal before copy");
        assert_eq!(disk(&archive), frozen);
    }
}
#[test]
#[ignore = "requires isolated actual Game resources and coordinator-bound identities"]
fn slot_recapture_interruption_timeout_and_whole_epoch_selection() {
    let fixture = Fixture::new("slot-recapture-restart");
    let mut history = history(&fixture);
    let first = epoch(&history.request);
    let first_bytes = disk(&first);
    let original = disk(&history.source.world_root);
    let before = state(&history.game);
    let working = disk(&history.game.world_root);
    let inherited = InheritedSlotFiles::read(&history.game.world_root, Phase::Validate).unwrap();
    // The real observer expires, then receives the definite Task completion.
    let receipt =
        dispatch_slot_recapture_after_observer_timeout(&mut history.game, second(&history.request))
            .unwrap();
    assert_eq!(receipt.epoch, 2);
    assert_eq!(state(&history.game), before);
    share(&history.game, &history.slot);
    inherited
        .require_unchanged(&history.game.world_root, Phase::Save)
        .unwrap();
    assert_native_footprint(&history.game.world_root, working);
    let working = disk(&history.game.world_root);
    let second_bytes = disk(&epoch(&second(&history.request)));
    for mode in ["before-rename", "after-rename"] {
        let parent = history.game.world_root.parent().unwrap();
        let archive = parent.join(format!("recapture-stop-{mode}"));
        fs::create_dir(&archive).unwrap();
        let old = archive.join("epoch-00000000000000000001");
        copy_epoch(&first, &old);
        let pending = archive.join(".pending-00000000000000000002");
        // The TWO actual epochs supply detached restart bytes, never live owners.
        copy_epoch(&epoch(&second(&history.request)), &pending);
        let frozen = disk(&archive);
        let old_bytes = disk(&old);
        let plan = ChildPlan {
            context: parent.join(format!("recapture-child-{mode}")),
            archive: archive.clone(),
            destination: parent.join(format!("recapture-after-stop-{mode}")),
            content: parent.join("counter-content"),
            identity: history.request.identity.clone(),
            world: history.counter.placement().world.to_string(),
            mode: mode.into(),
        };
        let signal = child_run(&plan, true);
        assert_eq!(signal["mode"], mode);
        let expected: Disk = frozen
            .into_iter()
            .map(|(path, bytes)| {
                let path = if mode == "after-rename" && path.starts_with(&pending) {
                    archive
                        .join("epoch-00000000000000000002")
                        .join(path.strip_prefix(&pending).unwrap())
                } else {
                    path
                };
                (path, bytes)
            })
            .collect();
        assert_eq!(disk(&archive), expected);
        assert_eq!(disk(&old), old_bytes);
        let selected = recover(&history, &archive, &plan.destination).unwrap();
        assert_eq!(
            selected.receipt.epoch,
            if mode == "after-rename" { 2 } else { 1 }
        );
        assert!(
            matches!(selected.slot.view().allocation,SlotFrameAllocation::Active{stack:Some(stack),revision:1,..}if plain_stone(Some(stack),7))
        );
        copied_world(
            &if mode == "after-rename" {
                archive.join("epoch-00000000000000000002")
            } else {
                old
            },
            &plan.destination,
        );
        canonical_players(&plan.destination);
        assert_eq!(disk(&archive), expected);
    }
    for (index, path) in [FRAME_PATH, SLOT_MEMBER].into_iter().enumerate() {
        for missing in [false, true] {
            let (archive, newer) =
                fork_history(&history, &format!("recapture-physical-{index}-{missing}"));
            if missing {
                fs::remove_file(newer.join(path)).unwrap();
            } else {
                fs::write(newer.join(path), b"genuine declared member hash corruption").unwrap();
            }
            let frozen = disk(&archive);
            let destination = history
                .game
                .world_root
                .parent()
                .unwrap()
                .join(format!("recapture-fallback-{index}-{missing}"));
            let selected = recover(&history, &archive, &destination).unwrap();
            assert_eq!(selected.receipt.epoch, 1);
            copied_world(&archive.join("epoch-00000000000000000001"), &destination);
            canonical_players(&destination);
            assert_eq!(disk(&archive), frozen);
        }
    }
    semantic_recovery_refusals(&history);
    let destination = history
        .game
        .world_root
        .parent()
        .unwrap()
        .join("recapture-wrong-identity");
    let mut wrong = history.request.identity.clone();
    wrong.test_binary = "f".repeat(64);
    let archive = disk(&history.request.archive_root);
    assert!(
        recover_slot_fixture_epoch(
            &history.request.archive_root,
            &wrong,
            &destination,
            &history.binding,
            history.game.res.items(),
            history.counter.placement().world,
            &target(history.game.dim).unwrap()
        )
        .is_err()
    );
    assert!(!destination.exists());
    assert_eq!(disk(&history.request.archive_root), archive);
    assert_eq!(disk(&first), first_bytes);
    assert_eq!(disk(&epoch(&second(&history.request))), second_bytes);
    assert_eq!(disk(&history.source.world_root), original);
    assert_eq!(disk(&history.game.world_root), working);
    assert_eq!(state(&history.game), before);
    share(&history.game, &history.slot);
}

#[test]
#[ignore = "AUXILIARY: explicit bounded parent plan and isolated process only; never a primary"]
fn slot_recapture_process_child() {
    let encoded = std::env::var("HORIZON_SLOT_RECAPTURE_CHILD_PLAN")
        .expect("auxiliary requires explicit parent plan");
    assert!(encoded.len() <= 8192);
    let plan: ChildPlan = parse_json(encoded.as_bytes(), 8192).unwrap();
    assert!(std::env::var_os("HORIZON_SESSION_FILE").is_none());
    assert_eq!(std::env::current_dir().unwrap(), plan.context);
    assert!(matches!(
        plan.mode.as_str(),
        "before-rename" | "after-rename"
    ));
    let world = WorldId::parse(&plan.world).unwrap();
    let fixture = Fixture::new("slot-recapture-child");
    let game = fixture.game();
    let bundle = horizon_mods::paired::PreparedBundle::read_folder(&plan.content).unwrap();
    let binding =
        NativeBinding::from_prepared(&bundle, "capture_counter:type", game.pack.assets.registry)
            .unwrap();
    assert_eq!(
        digest_identity(binding.package_identity()),
        plan.identity.counter_content
    );
    let root = plan.archive.join(".pending-00000000000000000002");
    let envelope = verify_complete_envelope(&root, 2, &plan.identity).unwrap();
    check_recovery_profile(
        &envelope.manifest,
        envelope.slot_manifest.as_ref(),
        RecoveryProfile::ActiveSlot,
    )
    .unwrap();
    read_slot_image(
        &root,
        &envelope.manifest,
        &binding,
        game.res.items(),
        world,
        &target(Dim::Overworld).unwrap(),
    )
    .unwrap();
    let counter: CounterArchive = parse_json(
        &fs::read(root.join("research/counter.json")).unwrap(),
        JSON_LIMIT,
    )
    .unwrap();
    let manifest_bytes = fs::read(root.join("manifest.json")).unwrap();
    let image = PendingImage {
        root,
        request: CaptureRequest {
            archive_root: plan.archive.clone(),
            epoch: 2,
            identity: plan.identity.clone(),
        },
        players: Vec::new(),
        counter,
        dimensions: envelope.manifest.dimensions.clone(),
        manifest: Some(envelope.manifest),
        manifest_bytes,
    };
    if plan.mode == "after-rename" {
        let _no_ack = image
            .rename_published_for(RecoveryProfile::ActiveSlot)
            .unwrap();
    }
    println!(
        "\nH08_SLOT_RECAPTURE_CELL {}",
        serde_json::json!({"mode":plan.mode})
    );
    std::io::stdout().flush().unwrap();
    let (send, recv) = mpsc::channel();
    let _input = std::thread::spawn(move || {
        let mut byte = [0];
        let result = std::io::stdin()
            .read_exact(&mut byte)
            .map_err(|e| e.to_string());
        let _ = send.send(result);
    });
    recv.recv_timeout(Duration::from_secs(120))
        .expect("parent disappeared")
        .unwrap();
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildPlan {
    context: PathBuf,
    archive: PathBuf,
    destination: PathBuf,
    content: PathBuf,
    identity: Identity,
    world: String,
    mode: String,
}
struct OwnedChild {
    child: std::process::Child,
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn child_run(plan: &ChildPlan, kill: bool) -> Value {
    fs::create_dir(&plan.context).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact","game::levels::capture_integration::counter_restore_integration::slot_epoch_integration::recapture_tests::slot_recapture_process_child","--ignored","--nocapture","--test-threads=1"])
        .current_dir(&plan.context).env("HORIZON_SLOT_RECAPTURE_CHILD_PLAN",serde_json::to_string(plan).unwrap())
        .env_remove("HORIZON_SESSION_FILE").env("HORIZON_LAUNCHER_NO_SELF_UPDATE","1").env("HORIZON_NO_AUDIO","1")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::from(File::create(plan.context.join("stderr.log")).unwrap()));
    for (name, folder) in [
        ("HOME", "home"),
        ("USERPROFILE", "home"),
        ("HORIZON_HOME", "game"),
        ("HORIZON_LAUNCHER_HOME", "launcher"),
        ("HORIZON_LAUNCHER_TRASH", "trash"),
        ("HORIZON_LAUNCHER_CLIPBOARD", "clipboard"),
        ("HORIZON_LAUNCHER_DESKTOP", "desktop"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_CACHE_HOME", "cache"),
        ("TMPDIR", "tmp"),
        ("TMP", "tmp"),
        ("TEMP", "tmp"),
    ] {
        let path = plan.context.join(folder);
        fs::create_dir_all(&path).unwrap();
        command.env(name, path);
    }
    let mut owned = OwnedChild {
        child: command.spawn().unwrap(),
    };
    let stdout = owned.child.stdout.take().unwrap();
    let log_path = plan.context.join("stdout.log");
    let (send, recv) = mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut input = BufReader::new(stdout);
        let mut log = File::create(log_path).unwrap();
        let mut total = 0usize;
        loop {
            let mut line = Vec::new();
            match (&mut input).take(4097).read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => {
                    total += line.len();
                    log.write_all(&line).unwrap();
                    if line.len() > 4096 || total > 64 << 10 {
                        let _ = send.try_send(Err("bounded stdout exceeded".to_owned()));
                        break;
                    }
                    let text = String::from_utf8_lossy(&line);
                    if let Some(encoded) = text.trim().strip_prefix("H08_SLOT_RECAPTURE_CELL ") {
                        let message =
                            parse_json::<Value>(encoded.as_bytes(), 4096).map_err(|e| e.detail);
                        let _ = send.try_send(message);
                    }
                }
                Err(e) => {
                    let _ = send.try_send(Err(e.to_string()));
                    break;
                }
            }
        }
    });
    let signal = recv.recv_timeout(Duration::from_secs(120));
    if kill || signal.is_err() {
        owned.child.kill().unwrap();
    } else {
        owned.child.stdin.as_mut().unwrap().write_all(b"x").unwrap();
    }
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = owned.child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > Duration::from_secs(15) {
            owned.child.kill().unwrap();
            let status = owned.child.wait().unwrap();
            reader.join().unwrap();
            panic!("child did not settle: {status}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    reader.join().unwrap();
    if !kill {
        assert!(
            status.success(),
            "child stderr {}",
            fs::read_to_string(plan.context.join("stderr.log")).unwrap()
        );
    }
    signal
        .unwrap_or_else(|error| {
            panic!(
                "child signal {error}: {}",
                fs::read_to_string(plan.context.join("stderr.log")).unwrap()
            )
        })
        .unwrap()
}
