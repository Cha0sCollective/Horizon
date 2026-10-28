use super::*;
use crate::{chunk::Chunk, item_data::BundleSelected, items::ItemStack};
use horizon_mods::content::machines::MachineType;
use horizon_world::{chunk::BlockState, nbt::Tag};
use std::{fs, sync::OnceLock};

const POSITION: [i32; 3] = [2, 70, 2];
fn metadata() -> &'static MachineType {
    static TYPE: OnceLock<MachineType> = OnceLock::new();
    TYPE.get_or_init(|| {
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("horizon-slot-metadata-{}-{unique}", std::process::id()));
        let descriptor = b"format=1\nhost-contract='paired-preview-1'\n[content]\nid='slot_demo'\nname='Slot'\nversion='0'\napi='0.2'\nside='both'\ncapabilities=['registries']\n[client]\nentry='client.wasm'\napi='0.1'\ncapabilities=[]\n[server]\nentry='server.wasm'\napi='0.3'\ncapabilities=[]\n";
        for (name, bytes) in [
            ("horizon-paired.toml", descriptor.as_slice()),
            ("client.wasm", b"not executed".as_slice()), ("server.wasm", b"not executed".as_slice()),
            ("data/slot_demo/horizon/machine/type.json", br#"{"format":1,"save-schema":1}"#.as_slice()),
            ("data/slot_demo/horizon/block/cube.json", br#"{"template":"minecraft:iron_block","machine-type":"slot_demo:type"}"#.as_slice()),
        ] {
            let path = dir.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap(); fs::write(path, bytes).unwrap();
        }
        let package = horizon_mods::paired::PreparedBundle::read_folder(&dir).unwrap();
        let metadata = package.content().machine_types().get("slot_demo:type").unwrap().clone();
        fs::remove_dir_all(dir).unwrap();
        metadata
    })
}
fn configuration(instance: u64, dimension: &str, state: u32) -> Configuration {
    Configuration::new(
        AuthorityScope::new(instance, dimension).unwrap(),
        metadata(),
        BlockState {
            name: "slot_demo:cube".into(),
            properties: Default::default(),
        },
        BlockStateId(state),
        POSITION,
    )
    .unwrap()
}
fn config() -> Configuration {
    configuration(7, "minecraft:overworld", 1)
}
fn chunk(state: u32) -> Chunk {
    let mut chunk = Chunk::empty(0, 0);
    chunk.set(2, 70, 2, state);
    chunk
}
fn resident(state: u32) -> World {
    let mut world = World::new(BlockStateId(0), 0);
    world.insert_streamed_chunk(chunk(state));
    world
}
fn enabled() -> World {
    let mut world = resident(1);
    world.enable_world_slot_trial(config()).unwrap();
    world
}
fn key(world: &World) -> SlotKey {
    world.world_slot_snapshot().unwrap().active.unwrap().key
}
fn limits(n: u32) -> ResolvedLimits {
    ResolvedLimits::new(n, n).unwrap()
}
fn rich(count: u32) -> ItemStack {
    let mut stack = ItemStack::new("minecraft:stone", count);
    stack.damage = 3;
    let data = stack.data_mut();
    data.set_other(
        "example:opaque",
        Some(Tag::compound([("retained", Tag::Long(19))])),
    );
    data.bundle_selected = BundleSelected(4);
    let mut nested = ItemStack::new("minecraft:dirt", 2);
    nested.data_mut().bundle_selected = BundleSelected(8);
    data.bundle_contents.push(nested);
    stack
}
fn raw(stack: &ItemStack) {
    assert_eq!(stack.data().unwrap().bundle_selected.0, 4);
    assert_eq!(
        stack.data().unwrap().bundle_contents[0]
            .data()
            .unwrap()
            .bundle_selected
            .0,
        8
    );
    assert_eq!(
        stack.data().unwrap().other("example:opaque"),
        rich(1).data().unwrap().other("example:opaque")
    );
}
fn plan(
    world: &World,
    key: &SlotKey,
    player: &SlotProjection,
    direction: Direction,
    count: u32,
) -> TransferPlan {
    match world
        .prepare_world_slot_transfer(key, player, direction, count, limits(64))
        .unwrap()
    {
        Preparation::Prepared(plan) => *plan,
        other => panic!("expected transfer: {other:?}"),
    }
}
fn fill(world: &mut World, count: u32) -> SlotProjection {
    let mut player = SlotProjection::new(Some(rich(count)));
    let plan = plan(world, &key(world), &player, Direction::IntoMachine, count);
    world
        .commit_world_slot_transfer(plan, &mut player, limits(64))
        .unwrap();
    assert!(player.snapshot().stack().is_none());
    player
}
fn assert_slot(world: &World, expected: &SlotKey, count: u32, revision: u64, resident: bool) {
    let active = world.read_world_slot(expected).unwrap();
    assert_eq!(&active.key, expected);
    assert_eq!(active.resident, resident);
    assert_eq!(active.slot.revision(), revision);
    assert_eq!(
        active.slot.stack().map(|s| s.count),
        if count == 0 { None } else { Some(count) }
    );
    if let Some(stack) = active.slot.stack() {
        raw(stack);
    }
}
fn conserved(world: &World, key: &SlotKey, player: &SlotProjection, original: &ItemStack) {
    let machine = world.read_world_slot(key).unwrap();
    let player = player.snapshot();
    let mut total = 0u64;
    for stack in [machine.slot.stack(), player.stack()].into_iter().flatten() {
        assert!(stack.same_item_same_components(original));
        raw(stack);
        total += u64::from(stack.count);
    }
    assert_eq!(total, u64::from(original.count));
}
fn recover(allocation: OwnedSlot, count: u32) {
    let (_, mut machine, _) = allocation.into_parts();
    let mut receiver = SlotProjection::new(None);
    let prepared = match slot_transfer::prepare(
        &machine,
        &receiver,
        Direction::IntoPlayer,
        u32::MAX,
        limits(64),
    )
    .unwrap()
    {
        slot_transfer::Preparation::Prepared(plan) => *plan,
        other => panic!("expected recovery: {other:?}"),
    };
    prepared
        .commit(&mut machine, &mut receiver, limits(64))
        .unwrap();
    assert!(machine.snapshot().stack().is_none());
    let received = receiver.snapshot();
    assert_eq!(received.stack().unwrap().count, count);
    raw(received.stack().unwrap());
}

#[test]
fn default_enable_validation_air_placement_and_noops() {
    let mut world = World::new(BlockStateId(0), 0);
    assert!(world.world_slot_snapshot().is_none());
    assert!(world.take_world_slot_trial().is_none());
    assert_eq!(
        world.enable_world_slot_trial(config()).unwrap_err(),
        ConfigError::NotResident
    );
    world.insert_streamed_chunk(chunk(0));
    assert_eq!(
        world
            .enable_world_slot_trial(configuration(7, "minecraft:overworld", 0))
            .unwrap_err(),
        ConfigError::InvalidMetadata
    );
    assert!(
        world
            .enable_world_slot_trial(config())
            .unwrap()
            .active
            .is_none()
    );
    assert_eq!(
        world.enable_world_slot_trial(config()).unwrap_err(),
        ConfigError::AlreadyEnabled
    );
    world.set(2, 70, 2, BlockStateId(1)).unwrap();
    let original = key(&world);
    fill(&mut world, 5);
    world.set(2, 70, 2, BlockStateId(1)).unwrap();
    world.set(3, 70, 2, BlockStateId(2)).unwrap();
    assert_slot(&world, &original, 5, 1, true);
    let snapshot = world.world_slot_snapshot().unwrap();
    assert!(snapshot.pending.is_none());
    let teardown = world.take_world_slot_trial().unwrap();
    recover(teardown.active.unwrap(), 5);
    assert_eq!(
        world.read_world_slot(&original).unwrap_err(),
        Error::Disabled
    );
}

#[test]
fn both_directions_conserve_components_and_fresh_limits_or_player_changes_abort() {
    let mut world = enabled();
    let original = key(&world);
    let mut player = SlotProjection::new(Some(rich(8)));
    let prepared = plan(&world, &original, &player, Direction::IntoMachine, 3);
    let result = world
        .commit_world_slot_transfer(prepared, &mut player, limits(64))
        .unwrap();
    assert_eq!(result.moved, 3);
    assert_slot(&world, &original, 3, 1, true);
    conserved(&world, &original, &player, &rich(8));
    let before = player.snapshot();
    assert_eq!(before.stack().unwrap().count, 5);
    raw(before.stack().unwrap());
    let prepared = plan(&world, &original, &player, Direction::IntoPlayer, 2);
    let duplicate = plan(&world, &original, &player, Direction::IntoPlayer, 2);
    world
        .commit_world_slot_transfer(prepared, &mut player, limits(64))
        .unwrap();
    assert_eq!(
        world.commit_world_slot_transfer(duplicate, &mut player, limits(64)),
        Err(Error::Transfer(TransferError::Stale))
    );
    assert_slot(&world, &original, 1, 2, true);
    conserved(&world, &original, &player, &rich(8));
    assert_eq!(player.snapshot().stack().unwrap().count, 7);
    raw(player.snapshot().stack().unwrap());
    let prepared = plan(&world, &original, &player, Direction::IntoMachine, 1);
    assert_eq!(
        world.commit_world_slot_transfer(prepared, &mut player, limits(99)),
        Err(Error::Transfer(TransferError::Stale))
    );
    assert_slot(&world, &original, 1, 2, true);
    assert_eq!(player.snapshot().revision(), 2);
    let prepared = plan(&world, &original, &player, Direction::IntoMachine, 1);
    player.replace(Some(rich(6))).unwrap();
    assert_eq!(
        world.commit_world_slot_transfer(prepared, &mut player, limits(64)),
        Err(Error::Transfer(TransferError::Stale))
    );
    assert_slot(&world, &original, 1, 2, true);
    assert_eq!(player.snapshot().stack().unwrap().count, 6);
    let prepared = plan(&world, &original, &player, Direction::IntoMachine, 1);
    drop(prepared);
    assert_slot(&world, &original, 1, 2, true);
}

#[test]
fn pending_retains_owner_through_repeated_writes_replacement_and_aba() {
    let mut world = enabled();
    fill(&mut world, 5);
    let original = key(&world);
    let mut player = SlotProjection::new(Some(rich(2)));
    let pending_plan = plan(&world, &original, &player, Direction::IntoMachine, 1);
    let stale_plan = plan(&world, &original, &player, Direction::IntoMachine, 1);
    world.game_time = 7;
    world.set(2, 70, 2, BlockStateId(0)).unwrap();
    for state in [1, 0, 1] {
        world.game_time += 1;
        world.set(2, 70, 2, BlockStateId(state)).unwrap();
    }
    world.insert_streamed_chunk(chunk(1));
    assert_eq!(world.block(2, 70, 2), BlockStateId(1));
    assert!(matches!(
        world.prepare_world_slot_transfer(
            &original,
            &player,
            Direction::IntoMachine,
            1,
            limits(64)
        ),
        Err(Error::PendingRetirement)
    ));
    assert_eq!(
        world.commit_world_slot_transfer(pending_plan, &mut player, limits(64)),
        Err(Error::PendingRetirement)
    );
    assert!(matches!(
        world.reconcile_world_slot_trial(),
        Err(Error::PendingRetirement)
    ));
    let pending = world.take_world_slot_retirement().unwrap();
    let snapshot = pending.snapshot();
    assert_eq!(snapshot.reason, RetirementReason::BlockChanged);
    assert_eq!(snapshot.game_time, 7);
    assert_eq!(snapshot.allocation.key, original);
    assert_eq!(snapshot.allocation.slot.revision(), 1);
    assert_eq!(snapshot.allocation.slot.stack().unwrap().count, 5);
    raw(snapshot.allocation.slot.stack().unwrap());
    assert!(world.take_world_slot_retirement().is_none());
    assert!(world.world_slot_snapshot().unwrap().active.is_none());
    let fresh = world.reconcile_world_slot_trial().unwrap().active.unwrap();
    assert_ne!(fresh.key, original);
    assert_eq!(
        world.read_world_slot(&original).unwrap_err(),
        Error::StaleKey
    );
    assert_eq!(
        world.commit_world_slot_transfer(stale_plan, &mut player, limits(64)),
        Err(Error::StaleKey)
    );
    assert_eq!(player.snapshot().stack().unwrap().count, 2);
    assert_eq!(player.snapshot().revision(), 0);
    assert!(
        world
            .read_world_slot(&fresh.key)
            .unwrap()
            .slot
            .stack()
            .is_none()
    );
    let (allocation, reason, time) = pending.into_parts();
    assert_eq!(reason, RetirementReason::BlockChanged);
    assert_eq!(time, 7);
    recover(allocation, 5);
}

#[test]
fn unload_reload_and_temporary_map_lending_retain_identity_but_check_actual_storage() {
    let mut world = enabled();
    fill(&mut world, 5);
    let original = key(&world);
    let mut player = SlotProjection::new(None);
    let prepared = plan(&world, &original, &player, Direction::IntoPlayer, 1);
    world.remove_streamed_chunk(&(0, 0)).unwrap();
    assert_slot(&world, &original, 5, 1, false);
    assert!(matches!(
        world.prepare_world_slot_transfer(&original, &player, Direction::IntoPlayer, 1, limits(64)),
        Err(Error::NotResident)
    ));
    assert_eq!(
        world.commit_world_slot_transfer(prepared, &mut player, limits(64)),
        Err(Error::NotResident)
    );
    world.insert_streamed_chunk(chunk(1));
    assert_slot(&world, &original, 5, 1, true);
    let prepared = plan(&world, &original, &player, Direction::IntoPlayer, 1);
    let lent = std::mem::take(&mut world.chunks);
    assert_eq!(
        world.commit_world_slot_transfer(prepared, &mut player, limits(64)),
        Err(Error::NotResident)
    );
    world.chunks = lent;
    assert_slot(&world, &original, 5, 1, true);
    assert!(world.world_slot_snapshot().unwrap().pending.is_none());
    let prepared = plan(&world, &original, &player, Direction::IntoPlayer, 1);
    world
        .commit_world_slot_transfer(prepared, &mut player, limits(64))
        .unwrap();
    assert_slot(&world, &original, 4, 2, true);
    assert_eq!(player.snapshot().stack().unwrap().count, 1);
}

#[test]
fn replacement_reload_mismatch_and_empty_retirement_keep_distinct_reasons() {
    for replacement in [false, true] {
        let mut world = enabled();
        fill(&mut world, 5);
        let original = key(&world);
        if !replacement {
            world.remove_streamed_chunk(&(0, 0));
        }
        world.game_time = 11;
        world.insert_streamed_chunk(chunk(if replacement { 1 } else { 0 }));
        let pending = world.take_world_slot_retirement().unwrap();
        let snapshot = pending.snapshot();
        assert_eq!(
            snapshot.reason,
            if replacement {
                RetirementReason::ChunkReplaced
            } else {
                RetirementReason::ReloadMismatch
            }
        );
        assert_eq!(snapshot.game_time, 11);
        assert_eq!(snapshot.allocation.key, original);
        raw(snapshot.allocation.slot.stack().unwrap());
        recover(pending.into_parts().0, 5);
    }
    let mut world = enabled();
    world.set(2, 70, 2, BlockStateId(0)).unwrap();
    let pending = world.take_world_slot_trial().unwrap().pending.unwrap();
    assert!(
        pending
            .into_parts()
            .0
            .into_parts()
            .1
            .snapshot()
            .stack()
            .is_none()
    );
}

#[test]
fn scope_and_reenabled_trial_allocation_refuse_foreign_keys_and_plans() {
    let mut first = enabled();
    let old_key = key(&first);
    let player = SlotProjection::new(Some(rich(2)));
    for config in [
        config(),
        configuration(7, "minecraft:the_nether", 1),
        configuration(8, "minecraft:overworld", 1),
    ] {
        let prepared = plan(&first, &old_key, &player, Direction::IntoMachine, 1);
        let mut second = resident(1);
        second.enable_world_slot_trial(config).unwrap();
        let mut player_copy = SlotProjection::new(Some(rich(2)));
        assert_ne!(key(&second), old_key);
        assert_eq!(
            second.read_world_slot(&old_key).unwrap_err(),
            Error::StaleKey
        );
        assert_eq!(
            second.commit_world_slot_transfer(prepared, &mut player_copy, limits(64)),
            Err(Error::StaleKey)
        );
        assert_eq!(player_copy.snapshot().stack().unwrap().count, 2);
    }
    let prepared = plan(&first, &old_key, &player, Direction::IntoMachine, 1);
    let old = first.take_world_slot_trial().unwrap();
    assert!(
        old.active
            .unwrap()
            .into_parts()
            .1
            .snapshot()
            .stack()
            .is_none()
    );
    first.enable_world_slot_trial(config()).unwrap();
    let mut player = player;
    assert_eq!(
        first.commit_world_slot_transfer(prepared, &mut player, limits(64)),
        Err(Error::StaleKey)
    );
    assert_eq!(key(&first).incarnation(), old_key.incarnation());
    assert_ne!(key(&first), old_key);
}

#[test]
fn teardown_moves_active_unloaded_or_pending_contents_and_exhaustion_does_not_wrap() {
    for mode in 0..3 {
        let mut world = enabled();
        fill(&mut world, 5);
        if mode == 1 {
            world.remove_streamed_chunk(&(0, 0));
        }
        if mode == 2 {
            world.set(2, 70, 2, BlockStateId(0)).unwrap();
        }
        let teardown = world.take_world_slot_trial().unwrap();
        assert!(world.world_slot_snapshot().is_none());
        assert!(world.take_world_slot_trial().is_none());
        let allocation = if mode == 2 {
            assert!(teardown.active.is_none());
            teardown.pending.unwrap().into_parts().0
        } else {
            assert!(teardown.pending.is_none());
            teardown.active.unwrap()
        };
        assert_eq!(allocation.snapshot().resident, mode != 1);
        recover(allocation, 5);
    }
    let mut world = resident(0);
    world.enable_world_slot_trial(config()).unwrap();
    world.world_slot_trial.as_mut().unwrap().last_incarnation = u64::MAX;
    world.set(2, 70, 2, BlockStateId(1)).unwrap();
    assert_eq!(world.block(2, 70, 2), BlockStateId(1));
    let snapshot = world.world_slot_snapshot().unwrap();
    assert!(snapshot.active.is_none());
    assert_eq!(snapshot.fault, Some(Fault::IncarnationExhausted));
    assert!(matches!(
        world.reconcile_world_slot_trial(),
        Err(Error::Fault(Fault::IncarnationExhausted))
    ));
    world.set(2, 70, 2, BlockStateId(0)).unwrap();
    world.set(2, 70, 2, BlockStateId(1)).unwrap();
    assert_eq!(
        world.take_world_slot_trial().unwrap().fault,
        Some(Fault::IncarnationExhausted)
    );
    let mut world = enabled();
    fill(&mut world, 5);
    world.world_slot_trial.as_mut().unwrap().last_incarnation = u64::MAX;
    world.set(2, 70, 2, BlockStateId(0)).unwrap();
    world.set(2, 70, 2, BlockStateId(1)).unwrap();
    let retained = world.take_world_slot_retirement().unwrap();
    assert_eq!(
        world.reconcile_world_slot_trial().unwrap().fault,
        Some(Fault::IncarnationExhausted)
    );
    recover(retained.into_parts().0, 5);
}

#[test]
fn counter_fault_does_not_inhibit_physical_removal_or_consume_slot_retirement() {
    let mut world = enabled();
    world.enable_counter_trial(config()).unwrap();
    fill(&mut world, 5);
    let mut player = SlotProjection::new(None);
    let prepared = plan(&world, &key(&world), &player, Direction::IntoPlayer, 1);
    // A direct SDK-style map loan is not a lifecycle event. Fresh storage checks
    // still refuse stale binding; this does not claim arbitrary map writes tracked.
    let mut lent = std::mem::take(&mut world.chunks);
    Arc::make_mut(lent.get_mut(&(0, 0)).unwrap()).set(2, 70, 2, 2);
    world.chunks = lent;
    assert!(matches!(
        world.advance_counter_trial(true, true),
        Some(Err(crate::counter_trial::Fault::ResidentMismatch { .. }))
    ));
    assert_eq!(
        world.commit_world_slot_transfer(prepared, &mut player, limits(64)),
        Err(Error::StateMismatch)
    );
    assert!(player.snapshot().stack().is_none());
    assert!(matches!(
        world.prepare_world_slot_transfer(
            &key(&world),
            &player,
            Direction::IntoPlayer,
            1,
            limits(64)
        ),
        Err(Error::StateMismatch)
    ));
    world.set(2, 70, 2, BlockStateId(0)).unwrap();
    assert_eq!(world.block(2, 70, 2), BlockStateId(0));
    assert!(world.counter_trial_snapshot().unwrap().fault().is_some());
    recover(
        world.take_world_slot_retirement().unwrap().into_parts().0,
        5,
    );
}
