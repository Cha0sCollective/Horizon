//! One owned actual native tick. Observation never admits a used streamer.
use super::super::tests::{loaded, source};
use super::*;
use crate::game::{
    integrated::{
        capture_completion_tests::{
            dispatch_slot_advance, dispatch_slot_capture, dispatch_slot_recapture,
        },
        transition_ownership_tests::Fixture,
    },
    slot_player::TransferOutcome,
};
use horizon_client::{
    chunk::{Chunk, RolledLoot},
    counter_trial::Snapshot,
    lan::Out,
    motion::PlayerMotion,
    slot_transfer::Direction,
    streaming::{CapturePristineError, NativeWorkObservation, NativeWorkOrigin},
    world::counter_restore::DomainSnapshot,
    world::slot_restore::SlotMetadataSnapshot,
};
use horizon_core::{random::RandomSource, ticks::SavedTick};
use horizon_world::nbt::Compound;
use std::sync::mpsc;
const T: i64 = 126;
type Disk = BTreeMap<PathBuf, Option<Vec<u8>>>;
fn disk(root: &Path) -> Disk {
    fn visit(path: &Path, out: &mut Disk) {
        let meta = fs::symlink_metadata(path).unwrap();
        assert!(!meta.file_type().is_symlink());
        if meta.is_dir() {
            out.insert(path.to_owned(), None);
            for row in fs::read_dir(path).unwrap() {
                visit(&row.unwrap().path(), out);
            }
        } else {
            assert!(meta.is_file());
            out.insert(path.to_owned(), Some(fs::read(path).unwrap()));
        }
    }
    let mut out = BTreeMap::new();
    visit(root, &mut out);
    out
}
fn stone_share(game: &Game<'_>, key: &SlotKey) {
    let slot = game.world.read_world_slot(key).unwrap();
    assert_eq!(slot.slot.revision(), 1);
    assert!(plain_stone(slot.slot.stack(), 7));
    let owner = game
        .lan
        .as_ref()
        .unwrap()
        .hub
        .guests
        .iter()
        .find(|guest| guest.uuid == 11)
        .unwrap();
    assert!(plain_stone(owner.inventory[0].as_ref(), 3));
}
fn seed(game: &mut Game<'_>, binding: &NativeBinding) -> SlotKey {
    // Authored initial ten, before any capture; the native cell move owns seven.
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
    let slot = game
        .world
        .initialize_persistent_world_slot(binding, &target(game.dim).unwrap())
        .unwrap()
        .unwrap();
    let mut host = game.lan.take().unwrap();
    let moved = game
        .as_guest(&mut host, 11, |game| {
            game.transfer_world_slot_inventory_cell(&slot, 0, Direction::IntoMachine, 7)
        })
        .unwrap()
        .unwrap();
    assert!(matches!(moved,TransferOutcome::Committed(result) if result.moved == 7));
    game.lan = Some(host);
    assert!(game.player.is_none() && game.lan_acting.is_none());
    stone_share(game, &slot);
    slot
}
struct OwnedCell<'a> {
    original: Box<Game<'a>>,
    middle: Box<Game<'a>>,
    game: Box<Game<'a>>,
    binding: NativeBinding,
    request: CaptureRequest,
    counter: RuntimeCounterKey,
    slot: SlotKey,
    source_counter: RuntimeCounterKey,
    middle_counter: RuntimeCounterKey,
    source_slot: SlotKey,
    middle_slot: SlotKey,
    _first_receipt: RecoveryReceipt,
    _second_receipt: RecoveryReceipt,
    _body: Option<PlayerMotion>,
    _receivers: [Vec<mpsc::Receiver<Out>>; 3],
    _held: Snapshot,
}
#[inline(never)]
fn owned_epoch_two<'a>(fixture: &'a Fixture) -> OwnedCell<'a> {
    let (original, body, source_rx, identity, binding, source_counter, held) = source(fixture);
    let mut original = Box::new(original);
    let source_slot = seed(&mut original, &binding);
    let parent = original.world_root.parent().unwrap().to_owned();
    let archive_root = parent.join("used-streamer-archive");
    fs::create_dir(&archive_root).unwrap();
    let first = CaptureRequest {
        archive_root,
        epoch: 1,
        identity,
    };
    dispatch_slot_capture(&mut original, first.clone()).unwrap();
    // Authority comes from the original live host key, before reading any candidate.
    let expected_world = source_counter.placement().world;
    let host = target(original.dim).unwrap();
    let destination = parent.join("used-streamer-middle");
    let recovery = recover_slot_fixture_epoch(
        &first.archive_root,
        &first.identity,
        &destination,
        &binding,
        original.res.items(),
        expected_world,
        &host,
    )
    .unwrap();
    assert_eq!(recovery.receipt.epoch, 1);
    let (middle, middle_rx) = loaded(fixture, &destination);
    let mut middle = Box::new(middle);
    let (middle_counter, middle_slot) = middle
        .install_slot_epoch(&binding, expected_world, &recovery.counter, recovery.slot)
        .unwrap();
    stone_share(&middle, &middle_slot);
    let request = CaptureRequest { epoch: 2, ..first };
    dispatch_slot_recapture(&mut middle, request.clone()).unwrap();
    let destination = parent.join("used-streamer-working");
    let second = recover_slot_fixture_epoch(
        &request.archive_root,
        &request.identity,
        &destination,
        &binding,
        middle.res.items(),
        expected_world,
        &host,
    )
    .unwrap();
    assert_eq!(second.receipt.epoch, 2);
    let (game, rx) = loaded(fixture, &destination);
    let mut game = Box::new(game);
    let (counter, slot) = game
        .install_slot_epoch(&binding, expected_world, &second.counter, second.slot)
        .unwrap();
    assert_ne!(counter.session_nonce(), source_counter.session_nonce());
    assert_ne!(counter.session_nonce(), middle_counter.session_nonce());
    assert_ne!(slot, source_slot);
    assert_ne!(slot, middle_slot);
    assert!(game.world.read_persistent_counter(&source_counter).is_err());
    assert!(game.world.read_persistent_counter(&middle_counter).is_err());
    assert!(game.world.read_world_slot(&source_slot).is_err());
    assert!(game.world.read_world_slot(&middle_slot).is_err());
    assert_eq!(game.world.game_time, T);
    stone_share(&game, &slot);
    game.streaming
        .expect_existing()
        .require_pristine_fixture_capture()
        .unwrap();
    OwnedCell {
        original,
        middle,
        game,
        binding,
        request,
        counter,
        slot,
        source_counter,
        middle_counter,
        source_slot,
        middle_slot,
        _first_receipt: recovery.receipt,
        _second_receipt: second.receipt,
        _body: body,
        _receivers: [source_rx, middle_rx, rx],
        _held: held,
    }
}
#[derive(Debug, PartialEq)]
struct SectionState {
    pointer: usize,
    blocks: Vec<u32>,
    biomes: Vec<u32>,
    light: Vec<u8>,
}
#[derive(Debug, PartialEq)]
struct ExtrasState {
    pointer: usize,
    root: Compound,
    time: i64,
    raw: Vec<Tag>,
    sources: Vec<((i32, i32, i32), Tag)>,
    order: Vec<(i32, i32, i32)>,
    loot: Vec<RolledLoot>,
    blocks: Vec<SavedTick>,
    fluids: Vec<SavedTick>,
}
#[derive(Debug, PartialEq)]
struct ColumnState {
    position: (i32, i32),
    pointer: usize,
    sections: Vec<SectionState>,
    extras: Option<ExtrasState>,
    flags: (bool, bool, bool),
    entities: Vec<Tag>,
    names: Vec<((i32, i32, i32), String)>,
    postprocess: Vec<[i32; 3]>,
    references: Vec<(Arc<str>, (i32, i32))>,
    starts: Vec<usize>,
}
fn column(chunk: &Arc<Chunk>) -> ColumnState {
    assert!(
        chunk.block_entities.is_empty(),
        "unexpected native staged owner requires analysis"
    );
    assert!(
        chunk.structure_starts.is_empty(),
        "unexpected live native structure owner requires analysis"
    );
    ColumnState {
        position: (chunk.x, chunk.z),
        pointer: Arc::as_ptr(chunk) as usize,
        sections: chunk
            .sections
            .iter()
            .map(|section| SectionState {
                pointer: Arc::as_ptr(section) as usize,
                blocks: (0..4096).map(|i| section.get(i)).collect(),
                biomes: (0..64).map(|i| section.biomes.get(i)).collect(),
                light: (0..4096).map(|i| section.light(i)).collect(),
            })
            .collect(),
        extras: chunk.saved.as_ref().map(|saved| ExtrasState {
            pointer: Arc::as_ptr(saved) as usize,
            root: saved.root.clone(),
            time: saved.last_update,
            raw: saved.block_entities.clone(),
            sources: saved.block_entity_sources.clone(),
            order: saved.block_entity_order.clone(),
            loot: saved.rolled_loot.clone(),
            blocks: saved.block_ticks.clone(),
            fluids: saved.fluid_ticks.clone(),
        }),
        flags: (chunk.lit, chunk.light_loaded, chunk.fresh),
        entities: chunk.entities.clone(),
        names: chunk.block_names.clone(),
        postprocess: chunk.postprocess.clone(),
        references: chunk.structure_refs.clone(),
        starts: chunk
            .structure_starts
            .iter()
            .map(|start| Arc::as_ptr(start) as usize)
            .collect(),
    }
}
#[derive(Debug, PartialEq)]
struct State {
    counter: CounterFrameV2,
    slot: Vec<u8>,
    domain: DomainSnapshot,
    metadata: SlotMetadataSnapshot,
    players: Vec<(Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>)>,
    level_player: Tag,
    clock: (i64, i64, u64, u64, u64, [i32; 4]),
    level_clock: (i64, i64),
    weather: [u32; 9],
    columns: Vec<ColumnState>,
    inhabited: BTreeMap<(i32, i32), i64>,
    modified: BTreeSet<(i32, i32)>,
    dirty: BTreeMap<(i32, i32), u32>,
    dirty_light: BTreeSet<(i32, i32)>,
    light_edits: Vec<((i32, i32, i32), u32, u32)>,
    loaded: Vec<((i32, i32), Vec<Tag>)>,
    block_log: Option<Vec<(i32, i32, i32)>>,
    light_log: Option<BTreeMap<(i32, i32), u32>>,
    watched: Vec<((i32, i32, i32), u32, u32)>,
    events: Vec<horizon_world::level::LevelEvent>,
}
fn state(game: &Game<'_>) -> State {
    // All other E1 owners are checked after the accounted active slot. Unknown
    // post-tick owners stop this proof; none is removed to get a capture result.
    game.observe_slot_advance_entity_owners().unwrap();
    assert!(game.world.block_entities.is_empty());
    assert!(game.world.chest_openers.is_empty() && game.world.block_names.is_empty());
    assert!(game.world.moving_pistons.is_empty() && game.world.moving_piston_fallback.is_empty());
    assert!(
        game.pending.shulker.is_empty()
            && game.pending.banner.is_empty()
            && game.pending.names.is_empty()
            && game.pending.pot.is_empty()
            && game.pending.copied.is_empty()
    );
    assert!(
        game.spill.spawns.is_empty() && game.spill.removed.is_empty() && game.spill.orbs.is_empty()
    );
    let (counter, slot) = game.slot_advance_projection().unwrap();
    let players = game
        .slot_player_images()
        .unwrap()
        .into_iter()
        .map(|row| {
            (
                serde_json::to_vec(&row.descriptor).unwrap(),
                row.nbt,
                row.advancements,
                row.stats,
            )
        })
        .collect();
    let mut random = game.world.random.clone();
    let mut columns: Vec<_> = game.world.chunks.values().map(column).collect();
    columns.sort_by_key(|row| row.position);
    let w = game.world.weather;
    State {
        counter,
        slot,
        domain: game.world.counter_domain_snapshot().unwrap(),
        metadata: game.world.persistent_world_slot_metadata().unwrap(),
        players,
        level_player: game.level.player_tag(),
        clock: (
            game.world.game_time,
            game.world.day_time,
            game.ticks,
            game.world.revision,
            game.world.chunks.membership_revision(),
            [
                random.next_int(),
                random.next_int(),
                random.next_int(),
                random.next_int(),
            ],
        ),
        level_clock: (game.level.time, game.level.day_time),
        weather: [
            w.clear_time as u32,
            w.rain_time as u32,
            w.thunder_time as u32,
            u32::from(w.raining),
            u32::from(w.thundering),
            w.rain_level.0.to_bits(),
            w.rain_level.1.to_bits(),
            w.thunder_level.0.to_bits(),
            w.thunder_level.1.to_bits(),
        ],
        columns,
        inhabited: game.world.inhabited.iter().map(|(p, t)| (*p, *t)).collect(),
        modified: game.world.modified.iter().copied().collect(),
        dirty: game.world.dirty_sections.columns().collect(),
        dirty_light: game.world.dirty_light.iter().copied().collect(),
        light_edits: game.world.light_edits.clone(),
        loaded: game.world.loaded_entities.clone(),
        block_log: game.world.block_log.clone(),
        light_log: game
            .world
            .light_log
            .as_ref()
            .map(|log| log.iter().map(|(p, v)| (*p, *v)).collect()),
        watched: game.world.watched_writes.clone(),
        events: game.level_events.clone(),
    }
}
fn conserved(observation: &NativeWorkObservation) {
    assert!(
        !observation.accounting_fault
            && !observation.receipt_mismatch
            && !observation.worker_initialization_fault
            && !observation.worker_exit_fault
            && !observation.result_disconnected
            && !observation.stopped
    );
    for (origin, c) in observation.origins {
        assert_eq!(
            c.submitted,
            c.queued + c.worker_held + c.returned + c.reconciling + c.disposed + c.unresolved,
            "native lifetime conservation for {origin:?}"
        );
        assert_eq!(c.unresolved, 0);
        assert_eq!(c.storage_errors, 0);
        // This first update submits Generate only; no finished/proto save is
        // driven by another pump. This is not a general writer-absence claim.
        assert_eq!(c.accepted_storage_handoffs, 0);
    }
}
#[test]
#[ignore = "isolated actual Game resources: one native server tick and guarded Task refusal"]
fn used_streamer_actual_native_tick_observes_owners_and_preserves_refused_capture() {
    let fixture = Fixture::new("used-streamer-one-tick");
    let mut cell = owned_epoch_two(&fixture);
    let first_archive = disk(&cell.request.archive_root);
    let original_bytes = disk(&cell.original.world_root);
    let middle_bytes = disk(&cell.middle.world_root);
    let original = cell.original.slot_epoch_projection().unwrap();
    let middle = cell.middle.slot_epoch_projection().unwrap();
    let before = cell.game.streaming.expect_existing().observe_native_work();
    conserved(&before);
    assert!(before.origins.iter().all(|(_, c)| c.submitted == 0));
    assert!(!cell.game.agent.hold_server && !cell.game.clock.frozen() && !cell.game.clock.paused());
    assert!(
        cell.game.options.dedicated.is_some()
            && cell.game.player.is_none()
            && cell.game.remote.is_none()
    );
    assert_eq!(cell.game.levels_to_tick(), vec![Dim::Overworld]);
    let ticks = cell.game.ticks;
    let before_inhabited: BTreeMap<_, _> = cell
        .game
        .world
        .inhabited
        .iter()
        .map(|(p, t)| (*p, *t))
        .collect();
    let before_player = cell.game.slot_player_images().unwrap();
    let before_player_hashes: Vec<_> = before_player
        .iter()
        .map(|row| {
            (
                row.descriptor.profile_uuid.clone(),
                hash(&row.nbt),
                hash(&row.advancements),
                hash(&row.stats),
            )
        })
        .collect();
    let before_columns: Vec<_> = cell
        .game
        .world
        .chunks
        .values()
        .map(|c| (c.x, c.z, c.saved.as_ref().unwrap().last_update))
        .collect();
    eprintln!(
        "USED_STREAMER_NATIVE_BEFORE time={} counter=3 players={before_player_hashes:?} inhabited={before_inhabited:?} roots={before_columns:?} owners={before:?}",
        cell.game.world.game_time
    );
    // The only time advancement in this experiment is this real native entry.
    cell.game.tick_server(false).unwrap();
    assert_eq!(cell.game.ticks, ticks + 1);
    assert_eq!(cell.game.world.game_time, T + 1);
    let counter = cell
        .game
        .world
        .read_persistent_counter(&cell.counter)
        .unwrap();
    assert_eq!(
        (counter.counter(), counter.last_advanced()),
        (4, Some(T + 1))
    );
    stone_share(&cell.game, &cell.slot);
    assert_eq!(
        cell.game.world.counter_domain_snapshot().unwrap().world,
        cell.source_counter.placement().world
    );
    let after = cell.game.streaming.expect_existing().observe_native_work();
    conserved(&after);
    assert!(
        after.origins.iter().any(|(_, c)| c.submitted > 0),
        "native tick must reach actual submission"
    );
    assert!(
        after.origins[NativeWorkOrigin::Generate as usize]
            .1
            .submitted
            > 0
    );
    assert_eq!(
        cell.game
            .streaming
            .expect_existing()
            .require_pristine_fixture_capture(),
        Err(CapturePristineError::PreviouslySubmitted)
    );
    let post_tick = state(&cell.game);
    assert!(
        !post_tick.inhabited.is_empty(),
        "actual spawning chunks update inhabited before spawn rules"
    );
    for (position, time) in &post_tick.inhabited {
        assert!(*time >= before_inhabited.get(position).copied().unwrap_or(0));
    }
    assert!(
        post_tick
            .inhabited
            .iter()
            .any(|(p, t)| *t > before_inhabited.get(p).copied().unwrap_or(0))
    );
    let after_player_hashes: Vec<_> = post_tick
        .players
        .iter()
        .map(|(descriptor, nbt, adv, stats)| (hash(descriptor), hash(nbt), hash(adv), hash(stats)))
        .collect();
    eprintln!(
        "USED_STREAMER_NATIVE_AFTER time={} counter=4 players={after_player_hashes:?} inhabited={:?} level_time={} roots={:?} owners={after:?}",
        cell.game.world.game_time,
        post_tick.inhabited,
        cell.game.level.time,
        post_tick
            .columns
            .iter()
            .map(|row| (row.position, row.extras.as_ref().map(|e| e.time)))
            .collect::<Vec<_>>()
    );
    let working = disk(&cell.game.world_root);
    let request = CaptureRequest {
        epoch: 3,
        ..cell.request.clone()
    };
    let error = dispatch_slot_advance(&mut cell.game, request).unwrap_err();
    assert_eq!(error.publication, Publication::NotPublished);
    assert!(matches!(
        error.code,
        Failure::Player
            | Failure::Entity
            | Failure::World
            | Failure::Streamer
            | Failure::Profile
            | Failure::InvalidImage
    ));
    eprintln!(
        "USED_STREAMER_CAPTURE_REFUSED phase={:?} code={:?} detail={error:?}",
        error.phase, error.code
    );
    assert_eq!(state(&cell.game), post_tick);
    assert_eq!(disk(&cell.game.world_root), working);
    assert_eq!(disk(&cell.request.archive_root), first_archive);
    assert_eq!(disk(&cell.original.world_root), original_bytes);
    assert_eq!(disk(&cell.middle.world_root), middle_bytes);
    assert_eq!(cell.original.slot_epoch_projection().unwrap(), original);
    assert_eq!(cell.middle.slot_epoch_projection().unwrap(), middle);
    stone_share(&cell.original, &cell.source_slot);
    stone_share(&cell.middle, &cell.middle_slot);
    assert!(
        cell.game
            .world
            .read_persistent_counter(&cell.middle_counter)
            .is_err()
    );
    let refused = cell.game.streaming.expect_existing().observe_native_work();
    conserved(&refused);
    for ((origin, before), (_, after)) in after.origins.into_iter().zip(refused.origins) {
        assert_eq!(
            before.submitted, after.submitted,
            "capture cannot submit {origin:?}"
        );
        assert_eq!(
            before.disposed, after.disposed,
            "capture cannot reconcile {origin:?}"
        );
    }
    assert_eq!(
        cell.game
            .streaming
            .expect_existing()
            .require_pristine_fixture_capture(),
        Err(CapturePristineError::PreviouslySubmitted)
    );
    assert_eq!(cell.binding.state_id(), cell.game.world.block(2, 70, 2));
}
