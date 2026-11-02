//! One qualified counter phase only: inherited3/T baseline and current4/T+1.
use super::super::tests::{loaded, source, tickets};
use super::*;
use crate::game::lan::{LanSettings, Owner};
use crate::game::{
    integrated::{
        capture_completion_tests::{
            dispatch_slot_advance, dispatch_slot_advance_after_observer_timeout,
            dispatch_slot_capture, dispatch_slot_recapture,
        },
        transition_ownership_tests::Fixture,
    },
    slot_player::TransferOutcome,
};
use horizon_client::{
    chunk::RolledLoot,
    containers::{Banner, BlockEntity},
    counter_trial::Snapshot,
    entities::{ItemEntity, ItemFrame},
    lan::Out,
    motion::PlayerMotion,
    slot_transfer::Direction,
    world::WorldOwnerKind as K,
};
use horizon_client::{
    lan::{Joined, NetConfig, PlayerSender, guest::Guest, hub::HubConfig, playerdata},
    save::Level,
    world::World,
};
use horizon_core::{
    geometry::Vec3,
    position::BlockPos,
    random::RandomSource,
    ticks::{SavedTick, TickPriority, TickTypeId},
};
use horizon_protocol::packets::configuration::ClientInformation;
use horizon_world::nbt::Compound;
use std::sync::atomic::AtomicI32;
use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};
const T: i64 = 126;
const COUNTER_FILE: &str = "data/horizon/counter_checkpoint.json";
const SLOT_FILE: &str = "data/horizon/slot_checkpoint.nbt";
fn third(request: &CaptureRequest) -> CaptureRequest {
    CaptureRequest {
        epoch: 3,
        ..request.clone()
    }
}
fn advance(history: &mut History<'_>) {
    assert_eq!(record(&history.game), (3, Some(T)));
    let slot = history
        .game
        .world
        .slot_checkpoint_bytes(history.game.res.items(), T)
        .unwrap();
    let players = history.game.slot_player_images().unwrap();
    let regions = slot_region_fingerprint(&history.game.world_root).unwrap();
    assert!(history.game.world.modified.is_empty());
    history.game.world.game_time = T + 1;
    history.game.tick_counter_trial(true);
    history.game.tick_counter_trial(true);
    assert_eq!(record(&history.game), (4, Some(T + 1)));
    share(&history.game, &history.slot);
    assert!(history.game.world.modified.is_empty());
    assert_eq!(
        slot_region_fingerprint(&history.game.world_root).unwrap(),
        regions
    );
    for chunk in history.game.world.chunks.values() {
        assert_eq!(chunk.saved.as_ref().unwrap().last_update, T);
        assert_eq!(chunk.saved.as_ref().unwrap().root, expected_root());
    }
    let current = decode_slot_frame(
        &history
            .game
            .world
            .slot_checkpoint_bytes(history.game.res.items(), T + 1)
            .unwrap(),
        history.game.res.items(),
    )
    .unwrap();
    let baseline = decode_slot_frame(&slot, history.game.res.items()).unwrap();
    assert_eq!(baseline.view().saved_game_time, T);
    assert_eq!(current.view().saved_game_time, T + 1);
    assert!(
        matches!(current.view().allocation,SlotFrameAllocation::Active{slot_generation:1,machine_generation:1,revision:1,stack:Some(stack)}if plain_stone(Some(stack),7))
    );
    let after = history.game.slot_player_images().unwrap();
    for (a, b) in after.iter().zip(&players) {
        assert_eq!(a.descriptor, b.descriptor);
        assert_eq!(a.nbt, b.nbt);
        assert_eq!(a.advancements, b.advancements);
        assert_eq!(a.stats, b.stats);
    }
}
fn recover_advance(
    history: &History<'_>,
    archive: &Path,
    destination: &Path,
) -> CResult<AdvanceRecovery> {
    recover_advanced_slot_fixture_epoch(
        &history.game,
        archive,
        &history.request.identity,
        destination,
        &history.binding,
        history.counter.placement().world,
        &target(Dim::Overworld).unwrap(),
    )
}
fn refuse(history: &mut History<'_>, label: &str, code: Failure) {
    eprintln!("SLOT_ADVANCE_REFUSAL {label}");
    let before = state(&history.game);
    let disk_before = disk(&history.game.world_root);
    let archive = disk(&history.request.archive_root);
    let original = disk(&history.source.world_root);
    let dirty = history.game.world.modified.clone();
    let loaded = history.game.world.loaded_entities.clone();
    // read_world_slot returns an owned diagnostic snapshot: its cloned stack's
    // address is not the store allocation. Retain the opaque authority instead;
    // State also compares the complete canonical native slot frame byte-for-byte.
    let slot_key = history.slot.clone();
    let allocation = history.game.world.read_world_slot(&slot_key).unwrap();
    let heads = (
        history.game.world.counter_domain_snapshot(),
        history.game.world.persistent_world_slot_metadata(),
    );
    let error = dispatch_slot_advance(&mut history.game, third(&history.request)).unwrap_err();
    assert_eq!(error.code, code, "{label}: {error:?}");
    assert_eq!(error.publication, Publication::NotPublished);
    assert_eq!(state(&history.game), before);
    assert_eq!(history.game.world.modified, dirty);
    assert_eq!(history.game.world.loaded_entities, loaded);
    assert_eq!(history.slot, slot_key);
    let after = history.game.world.read_world_slot(&slot_key).unwrap();
    assert_eq!(after.key, allocation.key);
    assert_eq!(after.key.incarnation(), allocation.key.incarnation());
    assert_eq!(after.resident, allocation.resident);
    assert_eq!(after.slot.revision(), allocation.slot.revision());
    assert_eq!(after.slot.stack(), allocation.slot.stack());
    assert_eq!(
        (
            history.game.world.counter_domain_snapshot(),
            history.game.world.persistent_world_slot_metadata(),
        ),
        heads
    );
    assert!(
        history
            .game
            .world
            .read_persistent_counter(&history.counter)
            .is_ok()
    );
    assert_eq!(disk(&history.game.world_root), disk_before);
    assert_eq!(disk(&history.request.archive_root), archive);
    assert_eq!(disk(&history.source.world_root), original);
}
fn world_refuse(history: &mut History<'_>, label: &str) {
    history.game.observe_slot_advance_entity_owners().unwrap();
    assert!(
        history.game.observe_slot_advance_world_owners().is_err(),
        "{label}"
    );
    refuse(history, label, Failure::World);
}
fn current_controls(history: &mut History<'_>) {
    history.game.slot_epoch_projection().unwrap();
    history.game.observe_slot_epoch_entity_owners().unwrap();
    history.game.observe_slot_recapture_world_owners().unwrap();
    assert!(history.game.slot_advance_projection().is_err());
    refuse(history, "three is not current4", Failure::Entity);
}
fn current_four_controls(history: &mut History<'_>) {
    history.game.slot_advance_projection().unwrap();
    history.game.observe_slot_advance_entity_owners().unwrap();
    history.game.observe_slot_advance_world_owners().unwrap();
    assert!(history.game.slot_epoch_projection().is_err());
    assert!(history.game.observe_slot_epoch_entity_owners().is_err());
    assert!(history.game.observe_checkpoint_entity_owners().is_err());
    assert!(history.game.observe_checkpoint_world_owners().is_err());
    assert!(history.game.observe_slot_recapture_world_owners().is_err());
    history.game.world.modified.insert((0, 0));
    refuse(history, "dirty native column", Failure::Profile);
    history.game.world.modified.remove(&(0, 0));
    let before = state(&history.game);
    let source = disk(&history.game.world_root);
    let archive = disk(&history.request.archive_root);
    for time in [i64::MIN, -1, 0, T, T + 2, i64::MAX] {
        history.game.world.game_time = time;
        let error = dispatch_slot_advance(&mut history.game, third(&history.request)).unwrap_err();
        assert_eq!(error.code, Failure::Entity);
        assert_eq!(error.publication, Publication::NotPublished);
        assert_eq!(history.game.world.game_time, time);
        history.game.world.game_time = T + 1;
        assert_eq!(state(&history.game), before);
        assert_eq!(disk(&history.game.world_root), source);
        assert_eq!(disk(&history.request.archive_root), archive);
    }
    for different_dimension in [false, true] {
        let guest = history
            .game
            .lan
            .as_mut()
            .unwrap()
            .hub
            .guests
            .iter_mut()
            .find(|guest| guest.uuid == 12)
            .unwrap();
        if different_dimension {
            guest.dimension = Dim::Nether.id().into();
        } else {
            guest.inventory[1] = Some(ItemStack::new("minecraft:diamond", 1));
        }
        refuse(history, "unsupported PLAYER or dimension", Failure::Player);
        let guest = history
            .game
            .lan
            .as_mut()
            .unwrap()
            .hub
            .guests
            .iter_mut()
            .find(|guest| guest.uuid == 12)
            .unwrap();
        if different_dimension {
            guest.dimension = Dim::Overworld.id().into();
        } else {
            guest.inventory[1] = None;
        }
    }
    let payload = raw();
    let loaded_row = ((0, 0), vec![payload.clone()]);
    history.game.world.loaded_entities.push(loaded_row.clone());
    refuse(
        history,
        "loaded entity after accounted slot4",
        Failure::Entity,
    );
    assert_eq!(history.game.world.loaded_entities.last(), Some(&loaded_row));
    history.game.world.loaded_entities.pop();
    history.game.persist.raw.insert(777, payload.clone());
    let address = history.game.persist.raw.get(&777).unwrap() as *const _ as usize;
    refuse(
        history,
        "raw entity memory after accounted slot4",
        Failure::Entity,
    );
    assert_eq!(history.game.persist.raw.get(&777), Some(&payload));
    assert_eq!(
        history.game.persist.raw.get(&777).unwrap() as *const _ as usize,
        address
    );
    history.game.persist.raw.remove(&777);
    entity_controls(history);
    let body = history._body.as_ref().unwrap();
    let plan = history.game.portal_plan(
        BlockPos::new(2, 70, 2),
        body.position,
        body.dimensions.width,
        body.dimensions.height,
        true,
    );
    history.game.portals.travel = Some(plan);
    let plan = format!("{:?}", history.game.portals.travel);
    let address = history
        .game
        .portals
        .travel
        .as_ref()
        .map(|plan| plan as *const _ as usize);
    refuse(
        history,
        "retained native portal plan, no completed transition",
        Failure::Entity,
    );
    assert_eq!(format!("{:?}", history.game.portals.travel), plan);
    assert_eq!(
        history
            .game
            .portals
            .travel
            .as_ref()
            .map(|plan| plan as *const _ as usize),
        address
    );
    history.game.portals.travel = None;
}
// Each independent fixture owns three actual Games. Keep their construction in
// separate, sequential frames rather than nesting more Histories under the main
// refusal fixture on the test worker's stack.
#[inline(never)]
fn streamer_fixture_control() {
    let fixture = Fixture::new("slot-advance-streamer");
    let mut streaming = self::history(&fixture);
    advance(&mut streaming);
    assert!(
        streaming
            .game
            .streaming
            .expect_existing_mut()
            .update(&mut streaming.game.world)
            .1
            .is_empty()
    );
    assert_eq!(
        streaming
            .game
            .streaming
            .expect_existing()
            .require_pristine_fixture_capture(),
        Err(horizon_client::streaming::CapturePristineError::PreviouslySubmitted)
    );
    streaming.game.observe_slot_advance_entity_owners().unwrap();
    streaming.game.observe_slot_advance_world_owners().unwrap();
    refuse(
        &mut streaming,
        "real native submission order",
        Failure::Streamer,
    );
}
#[inline(never)]
fn revision_fixture_control() {
    let fixture = Fixture::new("slot-advance-revision");
    let mut revision = self::history(&fixture);
    advance(&mut revision);
    revision_controls(&mut revision);
}
fn stack_allocations(stack: &ItemStack) -> Vec<(usize, i32)> {
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
    visit(stack, &mut rows);
    rows
}
fn entity_controls(history: &mut History<'_>) {
    history.game.item_frames.push(ItemFrame {
        id: 403,
        pos: [2, 70, 2],
        facing: 2,
        glow: true,
        item: Some(crate::game::levels::entity_owner_tests::rich(7)),
        rotation: 3,
        drop_chance: 0.75,
        fixed: false,
        invisible: false,
        check_interval: 7,
    });
    let value = format!("{:?}", history.game.item_frames);
    let allocations = stack_allocations(history.game.item_frames[0].item.as_ref().unwrap());
    assert!(matches!(
        history.game.observe_slot_advance_entity_owners(),
        Err(EntityOwnerObservationError::UnsupportedEntityFamily {
            family: UnsupportedEntityFamily::ItemFrames,
            ..
        })
    ));
    refuse(
        history,
        "item-frame family with retained rich native payload",
        Failure::Entity,
    );
    assert_eq!(format!("{:?}", history.game.item_frames), value);
    assert_eq!(
        stack_allocations(history.game.item_frames[0].item.as_ref().unwrap()),
        allocations
    );
    history.game.item_frames.pop();
    let at = Vec3::new(1000.0, 70.0, 1000.0);
    history.game.dropped.push(ItemEntity {
        id: 701,
        stack: crate::game::levels::entity_owner_tests::rich(7),
        position: at,
        previous: at,
        velocity: Vec3::ZERO,
        on_ground: false,
        age: 1,
        pickup_delay: 9,
        bob_offset: 0.25,
        ticks: 1,
        health: 5,
        fire_ticks: -17,
        thrower: None,
        target: None,
    });
    let value = format!("{:?}", history.game.dropped);
    let allocations = stack_allocations(&history.game.dropped[0].stack);
    assert!(matches!(
        history.game.observe_slot_advance_entity_owners(),
        Err(EntityOwnerObservationError::OffResidentDropped { id: 701, .. })
    ));
    refuse(
        history,
        "off-resident drop after accounted slot4",
        Failure::Entity,
    );
    assert_eq!(format!("{:?}", history.game.dropped), value);
    assert_eq!(
        stack_allocations(&history.game.dropped[0].stack),
        allocations
    );
    history.game.dropped.pop();
}
fn revision_controls(history: &mut History<'_>) {
    let mut host = history.game.lan.take().unwrap();
    let key = history.slot.clone();
    for (direction, label) in [
        (Direction::IntoPlayer, "native slot6/player4"),
        (Direction::IntoMachine, "native slot7 revision3"),
    ] {
        let moved = history
            .game
            .as_guest(&mut host, 11, |game| {
                game.transfer_world_slot_inventory_cell(&key, 0, direction, 1)
            })
            .unwrap()
            .unwrap();
        assert!(matches!(moved,TransferOutcome::Committed(value)if value.moved==1));
        history.game.lan = Some(host);
        refuse(history, label, Failure::Entity);
        host = history.game.lan.take().unwrap();
    }
    history.game.lan = Some(host);
    assert_eq!(
        history
            .game
            .world
            .read_world_slot(&history.slot)
            .unwrap()
            .slot
            .revision(),
        3
    );
}
type Disk = BTreeMap<PathBuf, Option<Vec<u8>>>;
fn disk(root: &Path) -> Disk {
    fn walk(path: &Path, out: &mut Disk) {
        use std::os::unix::fs::FileTypeExt;

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
        } else if meta.file_type().is_fifo() {
            out.insert(
                path.to_owned(),
                Some(b"retained FIFO, never opened".to_vec()),
            );
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
    let counter = game
        .world
        .counter_checkpoint_frame(game.world.game_time)
        .unwrap();
    let slot = game
        .world
        .slot_checkpoint_bytes(game.res.items(), game.world.game_time)
        .unwrap();
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
    _middle: Game<'a>,
    _middle_rx: Vec<mpsc::Receiver<Out>>,
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
    let (_middle_counter, slot) = game
        .install_slot_epoch(
            &binding,
            original_counter.placement().world,
            &recovery.counter,
            recovery.slot,
        )
        .unwrap();
    share(&game, &slot);
    assert_eq!(record(&game), (3, Some(T)));
    dispatch_slot_recapture(&mut game, second(&request)).unwrap();
    let final_root = original
        .world_root
        .parent()
        .unwrap()
        .join("advance-working-world");
    let recovered = recover_slot_fixture_epoch(
        &request.archive_root,
        &request.identity,
        &final_root,
        &binding,
        original.res.items(),
        original_counter.placement().world,
        &target(Dim::Overworld).unwrap(),
    )
    .unwrap();
    assert_eq!(recovered.receipt.epoch, 2);
    let (mut final_game, final_rx) = loaded(fixture, &final_root);
    enumerate_native_roots(&final_game);
    let (counter, slot) = final_game
        .install_slot_epoch(
            &binding,
            original_counter.placement().world,
            &recovered.counter,
            recovered.slot,
        )
        .unwrap();
    share(&final_game, &slot);
    History {
        source: original,
        _middle: game,
        _middle_rx: rx,
        game: final_game,
        binding,
        request,
        counter,
        slot,
        original_counter,
        original_slot,
        receipt: recovered.receipt,
        _body: body,
        _source_rx: source_rx,
        _rx: final_rx,
        _held: held,
    }
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
fn state_source(game: &Game<'_>) -> (CounterFrameV2, Vec<u8>, Vec<Vec<u8>>) {
    let counter = game
        .world
        .counter_checkpoint_frame(game.world.game_time)
        .unwrap();
    let slot = game
        .world
        .slot_checkpoint_bytes(game.res.items(), game.world.game_time)
        .unwrap();
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
    command.args(["--exact","game::levels::capture_integration::counter_restore_integration::slot_epoch_integration::advance_tests::slot_advance_process_child","--ignored","--nocapture","--test-threads=1"])
        .current_dir(&plan.context).env("HORIZON_SLOT_ADVANCE_CHILD_PLAN",serde_json::to_string(plan).unwrap())
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
                    if let Some(encoded) = text.trim().strip_prefix("H08_SLOT_ADVANCE_CELL ") {
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
fn channel_guest(conn: u32) -> (Guest, mpsc::Receiver<Out>) {
    let (tx, rx) = mpsc::channel();
    let mut g = Guest::new(
        Joined {
            conn,
            uuid: u128::from(conn),
            name: format!("Restored{conn}"),
            properties: Vec::new(),
            info: ClientInformation::new(2),
            brand: "native checkpoint fixture".into(),
            sender: PlayerSender::new(tx, Arc::default()),
            latency: Arc::new(AtomicI32::new(0)),
            address: "127.0.0.1:25565".parse().unwrap(),
        },
        conn as i32,
        [8.0, 72.0, 8.0],
        2,
    );
    g.placed = true;
    g.loaded = true;
    (g, rx)
}
/// Fresh native Game, no call to setup() and no trial until checked install at the end.
fn loaded_four<'a>(fixture: &'a Fixture, root: &Path) -> (Game<'a>, Vec<mpsc::Receiver<Out>>) {
    let mut game = fixture.game();
    let _discarded_fresh_body = game.player.take();
    assert!(game.inventory.iter().all(Option::is_none));
    assert!(game.armor.iter().all(Option::is_none));
    assert!(game.offhand.is_none());
    game.world_root = root.to_owned();
    game.dim = Dim::Overworld;
    game.level = Level::read(root).expect("native saved Level");
    game.options.test_terrain = true;
    let (streaming, save, locator) = crate::game::world_loading::make_streaming(
        &game.res,
        &game.pack,
        &game.options,
        &game.settings,
        root,
        game.dim,
        game.level.seed,
    )
    .unwrap();
    game.streaming = crate::game::streaming_owner::GameStreaming::from_existing(streaming);
    game.save = save;
    game.end.set_locator(locator);
    let mut world = World::new(game.res.facts().air, game.level.seed);
    world.game_time = game.level.time;
    world.day_time = game.level.day_time;
    world.weather = game.level.weather;
    for z in -1..=1 {
        for x in -1..=1 {
            let chunk = game
                .save
                .load(x, z, game.pack.assets.registry, game.res.biomes())
                .expect("all nine native stored columns");
            assert!(chunk.lit);
            assert!(chunk.saved.is_some(), "retain opaque native extras");
            world.chunks.insert((x, z), Arc::new(chunk));
        }
    }
    game.world = world;
    assert!(game.world.counter_trial_snapshot().is_none());
    assert!(game.world.counter_domain_snapshot().is_none());
    let net = NetConfig {
        listen: false,
        announce: false,
        online: None,
        ..NetConfig::lan(0, "restored checkpoint fixture".into(), 4)
    };
    let hub = HubConfig {
        default_game_mode: 0,
        guest_commands: false,
        force_game_mode: false,
        hardcore: false,
        view_distance: 2,
        simulation_distance: 2,
        dimension: game.dim.id().into(),
        seed: game.level.seed,
        max_players: 4,
        dedicated: false,
        integrated: true,
    };
    game.lan_host(
        LanSettings {
            port: 0,
            guest_commands: false,
            force_game_mode: false,
        },
        net,
        hub,
        Some(11),
    )
    .unwrap();
    let mut host = game.lan.take().unwrap();
    host.owner = Some(Owner {
        uuid: 11,
        saved: None,
    });
    let mut receivers = Vec::new();
    for conn in [11, 12] {
        let (mut guest, rx) = channel_guest(conn);
        receivers.push(rx);
        let uuid = if conn == 11 {
            uuid_text(OWNER_RECORD)
        } else {
            uuid_text([0, 0, 0, 12])
        };
        let tag = strict_player(
            &bounded_read(
                &root.join(format!("players/data/{uuid}.dat")),
                2 << 20,
                Phase::Recover,
            )
            .unwrap(),
        )
        .unwrap();
        let mut level = Level::default();
        level.read_player(&tag);
        level.player_extra = Some(tag);
        playerdata::apply(&mut guest, &level);
        let adv: Value = parse_json(
            &bounded_read(
                &root.join(format!("players/advancements/{uuid}.json")),
                JSON_LIMIT,
                Phase::Recover,
            )
            .unwrap(),
            JSON_LIMIT,
        )
        .unwrap();
        guest.advancements = horizon_client::advancements::Progress::from_json(&adv);
        guest.advancements_loaded = true;
        host.hub.guests.push(guest);
        game.as_guest(&mut host, conn, |g| assert_eq!(g.lan_acting, Some(conn)))
            .unwrap();
        host.play.get_mut(&conn).unwrap().stats =
            crate::game::stats::Stats::read(&root.join(format!("players/stats/{uuid}.json")));
    }
    game.lan = Some(host);
    tickets(&mut game, true);
    assert_eq!(game.world.game_time, T + 1);
    assert!(game.world.counter_domain_snapshot().is_none());
    (game, receivers)
}

#[test]
#[ignore = "requires isolated actual Game resources and coordinator-bound identities"]
fn slot_advance_actual_phase_epoch3_and_fresh_current4_recovery() {
    let fixture = Fixture::new("slot-advance-positive");
    let mut history = history(&fixture);
    assert_eq!(history.receipt.epoch, 2);
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
    let first = epoch(&history.request);
    let second_image = epoch(&second(&history.request));
    let frozen_first = disk(&first);
    let frozen_second = disk(&second_image);
    let original = disk(&history.source.world_root);
    let original_live = state_source(&history.source);
    let inherited = InheritedSlotFiles::read(&history.game.world_root, Phase::Validate).unwrap();
    advance(&mut history);
    let live = state(&history.game);
    let working = disk(&history.game.world_root);
    let regions = slot_region_fingerprint(&history.game.world_root).unwrap();
    let request = third(&history.request);
    let receipt = dispatch_slot_advance(&mut history.game, request.clone()).unwrap();
    assert_eq!(receipt.epoch, 3);
    assert_eq!(state(&history.game), live);
    assert_native_footprint(&history.game.world_root, working);
    assert_eq!(
        slot_region_fingerprint(&history.game.world_root).unwrap(),
        regions
    );
    inherited
        .require_unchanged(&history.game.world_root, Phase::Save)
        .unwrap();
    share(&history.game, &history.slot);
    let image = epoch(&request);
    let envelope = verify_complete_envelope(&image, 3, &request.identity).unwrap();
    check_recovery_profile(
        &envelope.manifest,
        envelope.slot_manifest.as_ref(),
        RecoveryProfile::AdvancedSlot,
    )
    .unwrap();
    canonical_players(&image.join("world"));
    let current = CounterFrameV2::decode(&fs::read(image.join(FRAME_PATH)).unwrap()).unwrap();
    assert!(
        matches!(current.placement,Placement::Active {counter:4,last_advanced:Some(time),..}if time==T+1)
    );
    assert_eq!(current.saved_game_time, T + 1);
    assert_eq!(
        slot_region_fingerprint(&image.join("world")).unwrap(),
        regions
    );
    assert_ne!(fs::read(image.join(FRAME_PATH)).unwrap(), inherited.counter);
    assert_ne!(fs::read(image.join(SLOT_MEMBER)).unwrap(), inherited.slot);
    let image_frozen = disk(&image);
    let working = disk(&history.game.world_root);
    let destination = history
        .game
        .world_root
        .parent()
        .unwrap()
        .join("advance-final-world");
    let recovery = recover_advance(&history, &request.archive_root, &destination).unwrap();
    assert_eq!(recovery.disposition, AdvanceDisposition::Current4);
    assert_eq!(recovery.receipt.epoch, 3);
    copied_world(&image, &destination);
    canonical_players(&destination);
    let (mut game, _rx) = loaded_four(&fixture, &destination);
    assert!(game.world.modified.is_empty());
    assert_eq!(game.level.time, T + 1);
    for chunk in game.world.chunks.values() {
        assert_eq!(chunk.saved.as_ref().unwrap().last_update, T);
        assert_eq!(chunk.saved.as_ref().unwrap().root, expected_root());
    }
    let (key, slot) = game
        .install_slot_for(
            &history.binding,
            history.counter.placement().world,
            &recovery.counter,
            recovery.slot,
            SlotPhase::Four,
        )
        .unwrap();
    assert_ne!(key.session_nonce(), history.counter.session_nonce());
    assert_ne!(slot, history.slot);
    assert!(
        game.world
            .read_persistent_counter(&history.counter)
            .is_err()
    );
    assert!(game.world.read_world_slot(&history.slot).is_err());
    share(&game, &slot);
    assert_eq!(record(&game), (4, Some(T + 1)));
    game.tick_counter_trial(true);
    assert_eq!(record(&game), (4, Some(T + 1)));
    game.world.game_time = T + 2;
    game.tick_counter_trial(true);
    game.tick_counter_trial(true);
    assert_eq!(record(&game), (5, Some(T + 2)));
    share(&game, &slot);
    assert_eq!(disk(&first), frozen_first);
    assert_eq!(disk(&second_image), frozen_second);
    assert_eq!(disk(&image), image_frozen);
    assert_eq!(disk(&history.source.world_root), original);
    assert_eq!(state_source(&history.source), original_live);
    assert_eq!(disk(&history.game.world_root), working);
}
#[test]
#[ignore = "requires isolated actual Game resources and coordinator-bound identities"]
fn slot_advance_current_baseline_and_remaining_owner_refusals() {
    world_refusal_cell();
    streamer_fixture_control();
    revision_fixture_control();
}
#[inline(never)]
fn world_refusal_cell() {
    let fixture = Fixture::new("slot-advance-world-refusals");
    let mut history = history(&fixture);
    current_controls(&mut history);
    advance(&mut history);
    current_four_controls(&mut history);
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
                .checkpoint_world_owner_for_native_previous_tick_roots()
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
        matches!(history.game.observe_slot_advance_world_owners(),Err(WorldOwnerObservationError::World {cause,..}) if cause.kind==K::WatchedWrites)
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
        Failure::Entity
    );
    let mut host = history.game.lan.take().unwrap();
    history
        .game
        .as_guest(&mut host, 11, |game| {
            assert_eq!(
                game.observe_slot_advance_world_owners().err(),
                Some(WorldOwnerObservationError::ActingGuest)
            )
        })
        .unwrap();
    history.game.lan = Some(host);
    let role = history.game.options.execution_role;
    history.game.options.execution_role = crate::game::execution_role::ExecutionRole::Client;
    assert_eq!(
        history.game.observe_slot_advance_world_owners().err(),
        Some(WorldOwnerObservationError::NotAuthoritative)
    );
    history.game.options.execution_role = role;
    history.game.enter_level(None, Dim::Nether).unwrap();
    history.game.enter_level(None, Dim::Overworld).unwrap();
    let parked = history.game.levels.parked.remove(&Dim::Nether).unwrap();
    history.game.levels.parked.insert(Dim::Overworld, parked);
    assert_eq!(
        history.game.observe_slot_advance_world_owners().err(),
        Some(WorldOwnerObservationError::DuplicateActiveDimension)
    );
    share(&history.game, &history.slot);
}

#[test]
#[ignore = "requires isolated actual Game resources and coordinator-bound identities"]
fn slot_advance_baseline_inputs_and_pending_overlay_preserve_sources() {
    let fixture = Fixture::new("slot-advance-inputs");
    let mut history = history(&fixture);
    advance(&mut history);
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
                Failure::Profile
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
    advance_pending_failures(&mut history, &counter, &slot);
    assert_eq!(disk(&first), frozen);
    assert_eq!(disk(&history.source.world_root), original);
}

fn edit_native_column(root: &Path, edit: impl FnOnce(&mut Tag)) -> PathBuf {
    use horizon_core::position::ChunkPos;
    use horizon_world::region::{ChunkRecord, RegionFile};
    let path =
        horizon_client::save::dimension_dir(root, Dim::Overworld.id()).join("region/r.-1.-1.mca");
    let mut region = RegionFile::open_writable(&path, false).unwrap();
    let pos = ChunkPos::new(-1, -1);
    let record = region.read(pos).unwrap().unwrap();
    let mut named = record
        .nbt(Limits {
            max_bytes: 2 << 20,
            max_nodes: NODE_LIMIT,
            max_depth: 32,
        })
        .unwrap();
    edit(&mut named.tag);
    let changed = ChunkRecord::from_nbt(
        &named,
        record.compression,
        record.timestamp,
        Limits {
            max_bytes: 2 << 20,
            max_nodes: NODE_LIMIT,
            max_depth: 32,
        },
    )
    .unwrap();
    region.write(pos, &changed).unwrap();
    path
}
fn external_stub(root: &Path) -> (PathBuf, PathBuf) {
    use horizon_core::position::ChunkPos;
    use horizon_world::region::{ChunkRecord, RegionFile};
    let directory = horizon_client::save::dimension_dir(root, Dim::Overworld.id()).join("region");
    let region_path = directory.join("r.-1.-1.mca");
    let mut region = RegionFile::open_writable(&region_path, false).unwrap();
    region
        .write(
            ChunkPos::new(-1, -1),
            &ChunkRecord {
                compression: Compression::None,
                compressed: vec![0; 1 << 20],
                timestamp: 17,
            },
        )
        .unwrap();
    let external = directory.join("c.-1.-1.mcc");
    assert!(external.is_file());
    (region_path, external)
}
fn native_owner_forms(game: &Game<'_>) {
    use horizon_client::save::{LevelShape, SaveTables, encode, encode_with};
    let before = state(game);
    let source = disk(&game.world_root);
    let chunk = game.world.chunks.get(&(-1, -1)).unwrap();
    let no_tables = encode(chunk, game.pack.assets.registry, game.res.biomes());
    assert!(no_tables.tag.get("block_ticks").is_none());
    assert!(no_tables.tag.get("fluid_ticks").is_none());
    assert_eq!(
        no_tables.tag.get("block_entities"),
        Some(&Tag::List(Vec::new()))
    );
    native_empty_owner_lists(&no_tables.tag).unwrap();
    let tables = SaveTables::new(&game.pack.assets);
    let with_tables = encode_with(
        chunk,
        game.pack.assets.registry,
        game.res.biomes(),
        Some(&tables),
        LevelShape::OVERWORLD,
    );
    assert_eq!(
        with_tables.tag.get("block_ticks"),
        Some(&Tag::List(Vec::new()))
    );
    assert_eq!(
        with_tables.tag.get("fluid_ticks"),
        Some(&Tag::List(Vec::new()))
    );
    native_empty_owner_lists(&with_tables.tag).unwrap();
    for case in [
        "missing-block-entities",
        "wrong-block-entities",
        "nonempty-block-entities",
        "missing-block-ticks",
        "missing-fluid-ticks",
        "wrong-block-ticks",
        "wrong-fluid-ticks",
        "nonempty-block-ticks",
        "nonempty-fluid-ticks",
    ] {
        let mut tag = with_tables.tag.clone();
        match case {
            "missing-block-entities" | "missing-block-ticks" | "missing-fluid-ticks" => {
                let key = match case {
                    "missing-block-entities" => "block_entities",
                    "missing-block-ticks" => "block_ticks",
                    _ => "fluid_ticks",
                };
                let Tag::Compound(values) = &mut tag else {
                    panic!()
                };
                values.remove(&key.into());
            }
            "wrong-block-entities" => put_field(&mut tag, "block_entities", Tag::Int(0)),
            "nonempty-block-entities" => {
                put_field(&mut tag, "block_entities", Tag::List(vec![raw()]))
            }
            "wrong-block-ticks" => put_field(&mut tag, "block_ticks", Tag::Int(0)),
            "wrong-fluid-ticks" => put_field(&mut tag, "fluid_ticks", Tag::Int(0)),
            "nonempty-block-ticks" => put_field(&mut tag, "block_ticks", Tag::List(vec![raw()])),
            "nonempty-fluid-ticks" => put_field(&mut tag, "fluid_ticks", Tag::List(vec![raw()])),
            _ => unreachable!(),
        }
        assert_eq!(
            native_empty_owner_lists(&tag).unwrap_err().code,
            Failure::Profile,
            "{case}"
        );
    }
    let binding = fixture_binding(game).unwrap();
    let columns = game
        .checked_slot_columns(&game.world_root, &binding, T, SlotPhase::Three)
        .unwrap();
    assert_eq!(columns.len(), 9);
    assert_eq!(state(game), before);
    assert_eq!(disk(&game.world_root), source);
}
fn native_input_controls(history: &mut History<'_>) {
    native_owner_forms(&history.game);
    let counter_path = history.game.world_root.join(COUNTER_FILE);
    let slot_path = history.game.world_root.join(SLOT_FILE);
    let counter = fs::read(&counter_path).unwrap();
    let slot = fs::read(&slot_path).unwrap();
    for case in [
        "coherent-baseline-generation2",
        "coherent-baseline-time-current",
        "coherent-current4-is-not-baseline3",
    ] {
        let mut frame: CounterFrameV2 = CounterFrameV2::decode(&counter).unwrap();
        let bytes = if case == "coherent-current4-is-not-baseline3" {
            let (current, bytes) = history.game.slot_advance_projection().unwrap();
            frame = current;
            bytes
        } else {
            slot_bytes(&slot, |tag| {
                if case == "coherent-baseline-generation2" {
                    frame.allocation_high_water = 2;
                    if let Placement::Active { generation, .. } = &mut frame.placement {
                        *generation = 2;
                    }
                    put_field(
                        tag,
                        "machine_high_water",
                        Tag::ByteArray(2u64.to_be_bytes().to_vec()),
                    );
                    put_field(
                        tag.get_mut("ownership").unwrap(),
                        "machine_generation",
                        Tag::ByteArray(2u64.to_be_bytes().to_vec()),
                    );
                } else {
                    frame.saved_game_time = T + 1;
                    if let Placement::Active { last_advanced, .. } = &mut frame.placement {
                        *last_advanced = Some(T + 1);
                    }
                    put_field(tag, "saved_game_time", Tag::Long(T + 1));
                }
            })
        };
        fs::write(&counter_path, frame.encode().unwrap()).unwrap();
        fs::write(&slot_path, bytes).unwrap();
        refuse(history, case, Failure::Profile);
        fs::write(&counter_path, &counter).unwrap();
        fs::write(&slot_path, &slot).unwrap();
    }
    let region = horizon_client::save::dimension_dir(&history.game.world_root, Dim::Overworld.id())
        .join("region/r.-1.-1.mca");
    let original = fs::read(&region).unwrap();
    for case in [
        "last-int",
        "last-stale",
        "inhabited-int",
        "post-type",
        "structures-type",
        "owner-list-type",
        "owner-list-nonempty",
        "owner-list-missing",
        "tick-list-nonempty",
        "tick-list-mixed",
        "status",
    ] {
        edit_native_column(&history.game.world_root, |tag| match case {
            "last-int" => put_field(tag, "LastUpdate", Tag::Int(T as i32)),
            "last-stale" => put_field(tag, "LastUpdate", Tag::Long(T - 1)),
            "inhabited-int" => put_field(tag, "InhabitedTime", Tag::Int(0)),
            "post-type" => put_field(tag, "PostProcessing", Tag::List(vec![Tag::Int(0); 24])),
            "structures-type" => put_field(tag, "structures", Tag::Int(0)),
            "owner-list-type" => put_field(tag, "block_ticks", Tag::Int(0)),
            "owner-list-nonempty" => put_field(tag, "block_entities", Tag::List(vec![raw()])),
            "owner-list-missing" => {
                let Tag::Compound(values) = tag else { panic!() };
                values.remove(&"block_entities".into());
            }
            "tick-list-nonempty" => {
                put_field(tag, "block_ticks", Tag::List(vec![raw()]));
                put_field(tag, "fluid_ticks", Tag::List(Vec::new()));
            }
            "tick-list-mixed" => {
                put_field(tag, "block_ticks", Tag::List(Vec::new()));
                let Tag::Compound(values) = tag else { panic!() };
                values.remove(&"fluid_ticks".into());
            }
            "status" => put_field(tag, "Status", Tag::String("minecraft:empty".into())),
            _ => unreachable!(),
        });
        refuse(history, case, Failure::Profile);
        fs::write(&region, &original).unwrap();
    }
    let (_, external) = external_stub(&history.game.world_root);
    let target = history
        .game
        .world_root
        .parent()
        .unwrap()
        .join("external-symlink-target");
    fs::write(&target, b"retained external target bytes").unwrap();
    fs::remove_file(&external).unwrap();
    std::os::unix::fs::symlink(&target, &external).unwrap();
    refuse(
        history,
        "native external stub symlink refused before native read",
        Failure::InvalidImage,
    );
    assert_eq!(fs::read_link(&external).unwrap(), target);
    assert_eq!(
        fs::read(&target).unwrap(),
        b"retained external target bytes"
    );
    fs::remove_file(&external).unwrap();
    fs::write(&region, &original).unwrap();
}
fn stage_from(image: &PendingImage, history: &History<'_>, epoch: u64) -> PendingImage {
    let root = history
        .request
        .archive_root
        .join(format!(".pending-{epoch:020}"));
    copy_epoch(&image.root, &root);
    PendingImage {
        root,
        request: CaptureRequest {
            epoch,
            ..history.request.clone()
        },
        players: history.game.slot_player_images().unwrap(),
        counter: parse_json(&json_bytes(&image.counter, JSON_LIMIT).unwrap(), JSON_LIMIT).unwrap(),
        dimensions: image.dimensions.clone(),
        manifest: None,
        manifest_bytes: Vec::new(),
    }
}
fn advance_pending_failures(history: &mut History<'_>, counter: &[u8], slot: &[u8]) {
    native_input_controls(history);
    let first = disk(&epoch(&history.request));
    let second_image = epoch(&second(&history.request));
    let second_frozen = disk(&second_image);
    let original = disk(&history.source.world_root);
    let live = state(&history.game);
    let before = disk(&history.game.world_root);
    let regions = slot_region_fingerprint(&history.game.world_root).unwrap();
    let (current, current_slot) = history.game.slot_advance_projection().unwrap();
    let files = history
        .game
        .checked_advance_baseline(&history.binding, &current, &current_slot)
        .unwrap();
    let material = history
        .game
        .profile_material(
            CaptureRequest {
                epoch: 4,
                ..history.request.clone()
            },
            CaptureProfile::NativeSlotAdvance,
        )
        .unwrap();
    files
        .require_unchanged(&history.game.world_root, Phase::Save)
        .unwrap();
    assert_eq!(state(&history.game), live);
    assert_eq!(
        slot_region_fingerprint(&history.game.world_root).unwrap(),
        regions
    );
    assert_native_footprint(&history.game.world_root, before);
    let mut base = material.copy().unwrap();
    base.overlay().unwrap();
    let counter_out = current.encode().unwrap();
    let counter_path = history.game.world_root.join(COUNTER_FILE);
    let slot_path = history.game.world_root.join(SLOT_FILE);
    // One actual native save supplies detached pending snapshots for these failure
    // cases. No pre-save, repeated save/repair, or successful retry is claimed.
    for (index, case) in [
        "copied-counter",
        "copied-slot",
        "source-counter",
        "source-slot",
        "copied-directory",
        "player-overlay-parent-file",
    ]
    .into_iter()
    .enumerate()
    {
        let mut image = stage_from(&base, history, 5 + index as u64);
        let copied_counter = image.root.join(FRAME_PATH);
        let copied_slot = image.root.join(SLOT_MEMBER);
        match case {
            "copied-counter" => fs::write(copied_counter, b"changed copy").unwrap(),
            "copied-slot" => fs::write(copied_slot, b"changed copy").unwrap(),
            "source-counter" => fs::write(&counter_path, b"changed source").unwrap(),
            "source-slot" => fs::write(&slot_path, b"changed source").unwrap(),
            "copied-directory" => {
                fs::remove_file(&copied_slot).unwrap();
                fs::create_dir(&copied_slot).unwrap();
            }
            "player-overlay-parent-file" => {
                let parent = image.root.join("world/players/data");
                for row in fs::read_dir(&parent).unwrap() {
                    fs::remove_file(row.unwrap().path()).unwrap();
                }
                fs::remove_dir(&parent).unwrap();
                fs::write(&parent, b"retained I/O obstruction").unwrap();
            }
            _ => unreachable!(),
        }
        let stage = disk(&image.root);
        let working = disk(&history.game.world_root);
        let error = if case == "player-overlay-parent-file" {
            image.overlay().unwrap_err()
        } else {
            image
                .replace_inherited_slot_files(
                    &history.game.world_root,
                    &files,
                    &counter_out,
                    &current_slot,
                )
                .unwrap_err()
        };
        if case == "player-overlay-parent-file" {
            assert_eq!(error.code, Failure::Io);
        }
        assert_eq!(error.publication, Publication::NotPublished);
        assert_eq!(disk(&image.root), stage);
        assert_eq!(disk(&history.game.world_root), working);
        assert_eq!(state(&history.game), live);
        assert_eq!(disk(&epoch(&history.request)), first);
        assert_eq!(disk(&second_image), second_frozen);
        assert_eq!(disk(&history.source.world_root), original);
        fs::write(&counter_path, counter).unwrap();
        fs::write(&slot_path, slot).unwrap();
    }
    let mut image = stage_from(&base, history, 11);
    image
        .replace_inherited_slot_files(
            &history.game.world_root,
            &files,
            &counter_out,
            &current_slot,
        )
        .unwrap();
    assert_eq!(fs::read(image.root.join(FRAME_PATH)).unwrap(), counter_out);
    assert_eq!(
        fs::read(image.root.join(SLOT_MEMBER)).unwrap(),
        current_slot
    );
    files
        .require_unchanged(&history.game.world_root, Phase::Save)
        .unwrap();
    fs::create_dir(image.root.join("manifest.json.tmp")).unwrap();
    let stage = disk(&image.root);
    assert!(
        image
            .write_manifest_temp_for(RecoveryProfile::AdvancedSlot)
            .is_err()
    );
    assert_eq!(disk(&image.root), stage);
    assert_eq!(disk(&epoch(&history.request)), first);
    assert_eq!(disk(&second_image), second_frozen);
    assert_eq!(disk(&history.source.world_root), original);
    assert_eq!(state(&history.game), live);
}

fn fork_three(history: &History<'_>, label: &str) -> (PathBuf, PathBuf) {
    let archive = history.game.world_root.parent().unwrap().join(label);
    fs::create_dir(&archive).unwrap();
    for number in 1..=3 {
        copy_epoch(
            &epoch(&CaptureRequest {
                epoch: number,
                ..history.request.clone()
            }),
            &archive.join(format!("epoch-{number:020}")),
        );
    }
    let newer = archive.join("epoch-00000000000000000003");
    (archive, newer)
}
fn semantic_advance_refusals(history: &History<'_>) {
    for (index, case) in [
        "slot-count",
        "slot-revision",
        "slot-time",
        "slot-world",
        "counter5",
        "counter3",
        "coherent-three-epoch3",
        "counter-time",
        "archive-counter",
        "archive-schema",
        "slot-path-int",
        "counter-entry",
        "slot-entry",
        "level-time-int",
        "region-time-int",
        "region-time-future",
        "region-root-type",
        "region-owner-type",
        "region-owner-nonempty",
        "region-owner-missing",
        "region-tick-nonempty",
        "region-tick-mixed",
    ]
    .into_iter()
    .enumerate()
    {
        let (archive, newer) = fork_three(history, &format!("advance-semantic-{index}"));
        match case {
            "slot-count" | "slot-revision" | "slot-time" | "slot-world" => {
                let bytes = slot_bytes(
                    &fs::read(newer.join(SLOT_MEMBER)).unwrap(),
                    |tag| match case {
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
                        "slot-revision" => put_field(
                            tag.get_mut("ownership").unwrap(),
                            "revision",
                            Tag::ByteArray(2u64.to_be_bytes().to_vec()),
                        ),
                        "slot-time" => put_field(tag, "saved_game_time", Tag::Long(T)),
                        "slot-world" => put_field(
                            tag,
                            "world_uuid",
                            Tag::String("00000000-0000-4000-8000-000000000123".into()),
                        ),
                        _ => unreachable!(),
                    },
                );
                rewrite_member(&newer, SLOT_MEMBER, &bytes);
            }
            "counter5" | "counter3" | "coherent-three-epoch3" | "counter-time" => {
                let mut frame: Value =
                    parse_json(&fs::read(newer.join(FRAME_PATH)).unwrap(), 16 << 10).unwrap();
                if case == "counter-time" {
                    frame["saved_game_time"] = T.into();
                } else {
                    frame["placement"]["counter"] = if case == "counter5" {
                        5.into()
                    } else {
                        3.into()
                    };
                }
                rewrite_member(&newer, FRAME_PATH, &serde_json::to_vec(&frame).unwrap());
                if case == "coherent-three-epoch3" {
                    let mut archive: Value = parse_json(
                        &fs::read(newer.join("research/counter.json")).unwrap(),
                        JSON_LIMIT,
                    )
                    .unwrap();
                    archive["counter"] = 3.into();
                    rewrite_member(
                        &newer,
                        "research/counter.json",
                        &serde_json::to_vec(&archive).unwrap(),
                    );
                }
            }
            "archive-counter" | "archive-schema" => {
                let mut value: Value = parse_json(
                    &fs::read(newer.join("research/counter.json")).unwrap(),
                    JSON_LIMIT,
                )
                .unwrap();
                if case == "archive-counter" {
                    value["counter"] = 3.into();
                } else {
                    value["save_schema"] = 2.into();
                }
                rewrite_member(
                    &newer,
                    "research/counter.json",
                    &serde_json::to_vec(&value).unwrap(),
                );
            }
            "slot-path-int" | "counter-entry" | "slot-entry" => {
                let mut value: Value = parse_json(
                    &fs::read(newer.join("manifest.json")).unwrap(),
                    MANIFEST_LIMIT,
                )
                .unwrap();
                if case == "slot-path-int" {
                    value["slot_path"] = 17.into();
                } else {
                    let required = if case == "counter-entry" {
                        FRAME_PATH
                    } else {
                        SLOT_MEMBER
                    };
                    value["files"]
                        .as_array_mut()
                        .unwrap()
                        .retain(|row| row["path"] != required);
                }
                fs::write(
                    newer.join("manifest.json"),
                    serde_json::to_vec(&value).unwrap(),
                )
                .unwrap();
            }
            "level-time-int" => {
                let mut level =
                    strict_player(&fs::read(newer.join("world/level.dat")).unwrap()).unwrap();
                put_field(
                    level.get_mut("Data").unwrap(),
                    "Time",
                    Tag::Int((T + 1) as i32),
                );
                rewrite_member(&newer, "world/level.dat", &player_bytes(&level).unwrap());
            }
            _ => {
                let path = edit_native_column(&newer.join("world"), |tag| match case {
                    "region-time-int" => put_field(tag, "LastUpdate", Tag::Int(T as i32)),
                    "region-time-future" => put_field(tag, "LastUpdate", Tag::Long(T + 1)),
                    "region-root-type" => put_field(tag, "structures", Tag::Int(0)),
                    "region-owner-type" => put_field(tag, "block_ticks", Tag::Int(0)),
                    "region-owner-nonempty" => {
                        put_field(tag, "block_entities", Tag::List(vec![raw()]))
                    }
                    "region-owner-missing" => {
                        let Tag::Compound(values) = tag else { panic!() };
                        values.remove(&"block_entities".into());
                    }
                    "region-tick-nonempty" => {
                        put_field(tag, "block_ticks", Tag::List(vec![raw()]));
                        put_field(tag, "fluid_ticks", Tag::List(Vec::new()));
                    }
                    "region-tick-mixed" => {
                        put_field(tag, "block_ticks", Tag::List(Vec::new()));
                        let Tag::Compound(values) = tag else { panic!() };
                        values.remove(&"fluid_ticks".into());
                    }
                    _ => unreachable!(),
                });
                let relative = path.strip_prefix(&newer).unwrap().to_str().unwrap();
                rewrite_member(&newer, relative, &fs::read(&path).unwrap());
            }
        }
        let frozen = disk(&archive);
        let destination = history
            .game
            .world_root
            .parent()
            .unwrap()
            .join(format!("advance-semantic-dest-{index}"));
        // Preserve the retained envelope helper's boundary: V2/native payload
        // semantics are checked AFTER whole-image selection, never by physical
        // verification or by falling back to an older owner.
        if matches!(
            case,
            "archive-schema" | "slot-path-int" | "counter-entry" | "slot-entry"
        ) {
            assert_eq!(
                verify_complete_envelope(&newer, 3, &history.request.identity)
                    .err()
                    .unwrap()
                    .code,
                Failure::Profile,
                "{case}"
            );
        } else {
            verify_complete_envelope(&newer, 3, &history.request.identity).unwrap();
        }
        let error = recover_advance(history, &archive, &destination).unwrap_err();
        if matches!(
            case,
            "counter5" | "counter3" | "coherent-three-epoch3" | "counter-time" | "archive-counter"
        ) {
            assert_eq!(
                error.code,
                Failure::Profile,
                "{case}: selected scalar semantics cannot fall back"
            );
        }
        assert!(
            matches!(error.code, Failure::Profile | Failure::InvalidImage),
            "{case}: {error:?}"
        );
        assert_eq!(error.publication, Publication::NotPublished);
        assert!(
            !destination.exists(),
            "{case}: semantic refusal before copy"
        );
        assert_eq!(disk(&archive), frozen);
    }
}

#[test]
#[ignore = "requires isolated actual Game resources and coordinator-bound identities"]
fn slot_advance_timeout_interruption_and_typed_whole_epoch_selection() {
    let fixture = Fixture::new("slot-advance-restart");
    let mut history = history(&fixture);
    let first = disk(&epoch(&history.request));
    let second_image = epoch(&second(&history.request));
    let second_frozen = disk(&second_image);
    let original = disk(&history.source.world_root);
    advance(&mut history);
    let live = state(&history.game);
    let working = disk(&history.game.world_root);
    let regions = slot_region_fingerprint(&history.game.world_root).unwrap();
    let inherited = InheritedSlotFiles::read(&history.game.world_root, Phase::Validate).unwrap();
    let request = third(&history.request);
    let receipt =
        dispatch_slot_advance_after_observer_timeout(&mut history.game, request.clone()).unwrap();
    assert_eq!(receipt.epoch, 3);
    assert_eq!(state(&history.game), live);
    assert_native_footprint(&history.game.world_root, working);
    assert_eq!(
        slot_region_fingerprint(&history.game.world_root).unwrap(),
        regions
    );
    inherited
        .require_unchanged(&history.game.world_root, Phase::Save)
        .unwrap();
    let working = disk(&history.game.world_root);
    let third_image = epoch(&request);
    let third_frozen = disk(&third_image);
    let old_dest = history
        .game
        .world_root
        .parent()
        .unwrap()
        .join("advance-old3-refusal");
    let archive = disk(&request.archive_root);
    let error = recover_slot_fixture_epoch(
        &request.archive_root,
        &request.identity,
        &old_dest,
        &history.binding,
        history.game.res.items(),
        history.counter.placement().world,
        &target(Dim::Overworld).unwrap(),
    )
    .unwrap_err();
    assert_eq!(error.code, Failure::Profile);
    assert!(!old_dest.exists());
    assert_eq!(disk(&request.archive_root), archive);
    for (index, profile) in [RecoveryProfile::ArchiveOnly, RecoveryProfile::CounterOnly]
        .into_iter()
        .enumerate()
    {
        let dest = history
            .game
            .world_root
            .parent()
            .unwrap()
            .join(format!("advance-old-profile-{index}"));
        assert_eq!(
            select_fixture_epoch_for(&request.archive_root, &request.identity, &dest, profile)
                .err()
                .unwrap()
                .code,
            Failure::Profile
        );
        assert!(!dest.exists());
    }
    for mode in ["before-rename", "after-rename"] {
        let parent = history.game.world_root.parent().unwrap();
        let archive = parent.join(format!("advance-stop-{mode}"));
        fs::create_dir(&archive).unwrap();
        for number in 1..=2 {
            copy_epoch(
                &epoch(&CaptureRequest {
                    epoch: number,
                    ..history.request.clone()
                }),
                &archive.join(format!("epoch-{number:020}")),
            );
        }
        let pending = archive.join(".pending-00000000000000000003");
        copy_epoch(&third_image, &pending);
        let frozen = disk(&archive);
        let plan = ChildPlan {
            context: parent.join(format!("advance-child-{mode}")),
            archive: archive.clone(),
            destination: parent.join(format!("advance-after-stop-{mode}")),
            content: parent.join("counter-content"),
            identity: request.identity.clone(),
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
                        .join("epoch-00000000000000000003")
                        .join(path.strip_prefix(&pending).unwrap())
                } else {
                    path
                };
                (path, bytes)
            })
            .collect();
        assert_eq!(disk(&archive), expected);
        if mode == "after-rename" {
            let selected = recover_advance(&history, &archive, &plan.destination).unwrap();
            assert_eq!(selected.receipt.epoch, 3);
            assert_eq!(selected.disposition, AdvanceDisposition::Current4);
            copied_world(
                &archive.join("epoch-00000000000000000003"),
                &plan.destination,
            );
        } else {
            assert_eq!(
                recover_advance(&history, &archive, &plan.destination)
                    .unwrap_err()
                    .code,
                Failure::Profile
            );
            assert!(!plan.destination.exists());
            let baseline = recover_slot_fixture_epoch(
                &archive,
                &request.identity,
                &plan.destination,
                &history.binding,
                history.game.res.items(),
                history.counter.placement().world,
                &target(Dim::Overworld).unwrap(),
            )
            .unwrap();
            assert_eq!(baseline.receipt.epoch, 2);
            copied_world(
                &archive.join("epoch-00000000000000000002"),
                &plan.destination,
            );
        }
        assert_eq!(disk(&archive), expected);
    }
    for (index, path) in [FRAME_PATH, SLOT_MEMBER].into_iter().enumerate() {
        for missing in [false, true] {
            let (archive, newer) =
                fork_three(&history, &format!("advance-physical-{index}-{missing}"));
            if missing {
                fs::remove_file(newer.join(path)).unwrap();
            } else {
                fs::write(newer.join(path), b"declared physical corruption").unwrap();
            }
            let frozen = disk(&archive);
            let destination = history
                .game
                .world_root
                .parent()
                .unwrap()
                .join(format!("advance-fallback-{index}-{missing}"));
            let selected = recover_advance(&history, &archive, &destination).unwrap();
            assert_eq!(
                selected.disposition,
                AdvanceDisposition::Baseline3Fallback { selected_epoch: 2 }
            );
            assert_eq!(selected.receipt.epoch, 2);
            copied_world(&archive.join("epoch-00000000000000000002"), &destination);
            let (mut game, _rx) = loaded(&fixture, &destination);
            let (key, slot) = game
                .install_slot_for(
                    &history.binding,
                    history.counter.placement().world,
                    &selected.counter,
                    selected.slot,
                    SlotPhase::Three,
                )
                .unwrap();
            assert_ne!(key.session_nonce(), history.counter.session_nonce());
            assert_ne!(slot, history.slot);
            assert_eq!(record(&game), (3, Some(T)));
            share(&game, &slot);
            game.tick_counter_trial(true);
            assert_eq!(record(&game), (3, Some(T)));
            assert_eq!(disk(&archive), frozen);
        }
    }
    semantic_advance_refusals(&history);
    for case in ["identity", "world", "target"] {
        let destination = history
            .game
            .world_root
            .parent()
            .unwrap()
            .join(format!("advance-independent-{case}"));
        let mut identity = history.request.identity.clone();
        if case == "identity" {
            identity.test_binary = "f".repeat(64);
        }
        let world = if case == "world" {
            WorldId::parse("00000000-0000-4000-8000-000000000123").unwrap()
        } else {
            history.counter.placement().world
        };
        let host = if case == "target" {
            CounterTarget::new(Dim::Overworld.id(), [3, 70, 2]).unwrap()
        } else {
            target(Dim::Overworld).unwrap()
        };
        let frozen = disk(&request.archive_root);
        let error = recover_advanced_slot_fixture_epoch(
            &history.game,
            &request.archive_root,
            &identity,
            &destination,
            &history.binding,
            world,
            &host,
        )
        .unwrap_err();
        if case == "identity" {
            assert_eq!(error.code, Failure::Identity);
        }
        assert!(!destination.exists());
        assert_eq!(disk(&request.archive_root), frozen);
    }
    let parent = history.game.world_root.parent().unwrap();
    let baseline_archive = parent.join("advance-baseline-alone");
    fs::create_dir(&baseline_archive).unwrap();
    copy_epoch(
        &second_image,
        &baseline_archive.join("epoch-00000000000000000002"),
    );
    let destination = parent.join("advance-no-implicit-baseline");
    assert_eq!(
        recover_advance(&history, &baseline_archive, &destination)
            .unwrap_err()
            .code,
        Failure::Profile
    );
    assert!(!destination.exists());
    let plan = ChildPlan {
        context: parent.join("advance-child-external-fifo"),
        archive: request.archive_root.clone(),
        destination: parent.join("unused-fifo-destination"),
        content: parent.join("counter-content"),
        identity: request.identity.clone(),
        world: history.counter.placement().world.to_string(),
        mode: "external-fifo".into(),
    };
    let signal = child_run(&plan, false);
    assert_eq!(signal["mode"], "external-fifo");
    assert_eq!(signal["refused_before_native_save"], true);
    assert_eq!(disk(&epoch(&history.request)), first);
    assert_eq!(disk(&second_image), second_frozen);
    assert_eq!(disk(&third_image), third_frozen);
    assert_eq!(disk(&history.source.world_root), original);
    assert_eq!(disk(&history.game.world_root), working);
    assert_eq!(state(&history.game), live);
}

#[test]
#[ignore = "AUXILIARY: explicit bounded parent plan and isolated process only; never a primary"]
fn slot_advance_process_child() {
    let encoded = std::env::var("HORIZON_SLOT_ADVANCE_CHILD_PLAN")
        .expect("auxiliary requires explicit parent plan");
    assert!(encoded.len() <= 8192);
    let plan: ChildPlan = parse_json(encoded.as_bytes(), 8192).unwrap();
    assert_eq!(std::env::current_dir().unwrap(), plan.context);
    assert!(std::env::var_os("HORIZON_SESSION_FILE").is_none());
    let signal = if plan.mode == "external-fifo" {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let fixture = Fixture::new("slot-advance-fifo");
        let mut history = history(&fixture);
        advance(&mut history);
        let (_, external) = external_stub(&history.game.world_root);
        fs::remove_file(&external).unwrap();
        assert!(
            Command::new("mkfifo")
                .arg("--")
                .arg(&external)
                .status()
                .unwrap()
                .success()
        );
        let before = fs::symlink_metadata(&external).unwrap();
        assert!(before.file_type().is_fifo());
        // Parent bounds this entire process. Without the strict tree preflight,
        // native RegionFile::read would block opening this external stub's FIFO.
        refuse(
            &mut history,
            "external FIFO refused before native read/save",
            Failure::InvalidImage,
        );
        let after = fs::symlink_metadata(&external).unwrap();
        assert!(after.file_type().is_fifo());
        assert_eq!(
            (after.dev(), after.ino(), after.mode()),
            (before.dev(), before.ino(), before.mode())
        );
        assert_eq!(Level::read(&history.game.world_root).unwrap().time, T);
        assert_eq!(record(&history.game), (4, Some(T + 1)));
        share(&history.game, &history.slot);
        serde_json::json!({"mode":"external-fifo","refused_before_native_save":true})
    } else {
        assert!(matches!(
            plan.mode.as_str(),
            "before-rename" | "after-rename"
        ));
        let fixture = Fixture::new("slot-advance-child");
        let game = fixture.game();
        let world = WorldId::parse(&plan.world).unwrap();
        let bundle = horizon_mods::paired::PreparedBundle::read_folder(&plan.content).unwrap();
        let binding = NativeBinding::from_prepared(
            &bundle,
            "capture_counter:type",
            game.pack.assets.registry,
        )
        .unwrap();
        assert_eq!(
            digest_identity(binding.package_identity()),
            plan.identity.counter_content
        );
        let root = plan.archive.join(".pending-00000000000000000003");
        let envelope = verify_complete_envelope(&root, 3, &plan.identity).unwrap();
        check_recovery_profile(
            &envelope.manifest,
            envelope.slot_manifest.as_ref(),
            RecoveryProfile::AdvancedSlot,
        )
        .unwrap();
        let (counter, _slot) = read_slot_image_for(
            &root,
            &envelope.manifest,
            &binding,
            game.res.items(),
            world,
            &target(Dim::Overworld).unwrap(),
            SlotPhase::Four,
        )
        .unwrap();
        game.checked_slot_columns(
            &root.join("world"),
            &binding,
            counter.saved_game_time,
            SlotPhase::Four,
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
                epoch: 3,
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
                .rename_published_for(RecoveryProfile::AdvancedSlot)
                .unwrap();
        }
        serde_json::json!({"mode":plan.mode})
    };
    println!(
        "\nH08_SLOT_ADVANCE_CELL {}",
        serde_json::to_string(&signal).unwrap()
    );
    std::io::stdout().flush().unwrap();
    let (send, recv) = mpsc::channel();
    let _input = std::thread::spawn(move || {
        let mut byte = [0];
        let result = std::io::stdin()
            .read_exact(&mut byte)
            .map_err(|error| error.to_string());
        let _ = send.send(result);
    });
    recv.recv_timeout(Duration::from_secs(120))
        .expect("parent disappeared")
        .unwrap();
}
