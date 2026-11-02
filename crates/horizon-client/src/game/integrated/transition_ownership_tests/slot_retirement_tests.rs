//! Actual Game receiver tests; reuse the isolated resource-backed parent fixture.
use super::super::super::slot_retirement::{Delivery, DeliveryError};
use super::{Arc, Chunk, Dim, Fixture, Game, ItemStack, Tag};
use horizon_client::{
    counter_trial::Configuration,
    item_data::BundleSelected,
    mutation_observer::AuthorityScope,
    slot_transfer::{Direction, ResolvedLimits, SlotProjection},
    streaming::{ChunkGenerator, Proto, Streaming},
    world_slot_trial::{Preparation, SlotKey},
};
use horizon_core::random::{LegacyRandom, RandomSource};
use horizon_mods::content::machines::MachineType;
use horizon_world::chunk::BlockState;
use std::fs;

const POSITION: [i32; 3] = [2, 70, 2];
fn metadata(fixture: &Fixture) -> MachineType {
    let dir = fixture.root.join("slot-metadata");
    let descriptor = b"format=1\nhost-contract='paired-preview-1'\n[content]\nid='slot_demo'\nname='Slot'\nversion='0'\napi='0.2'\nside='both'\ncapabilities=['registries']\n[client]\nentry='client.wasm'\napi='0.1'\ncapabilities=[]\n[server]\nentry='server.wasm'\napi='0.3'\ncapabilities=[]\n";
    for (name, bytes) in [
        ("horizon-paired.toml", descriptor.as_slice()),
        ("client.wasm", b"not executed".as_slice()),
        ("server.wasm", b"not executed".as_slice()),
        (
            "data/slot_demo/horizon/machine/type.json",
            br#"{"format":1,"save-schema":1}"#.as_slice(),
        ),
        (
            "data/slot_demo/horizon/block/cube.json",
            br#"{"template":"minecraft:iron_block","machine-type":"slot_demo:type"}"#.as_slice(),
        ),
    ] {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    horizon_mods::paired::PreparedBundle::read_folder(&dir)
        .unwrap()
        .content()
        .machine_types()
        .get("slot_demo:type")
        .unwrap()
        .clone()
}
fn rich(count: u32) -> ItemStack {
    let mut stack = ItemStack::new("minecraft:stone", count);
    stack.damage = 7;
    let data = stack.data_mut();
    data.custom_name = Some("owned retirement".into());
    data.set_other(
        "example:opaque",
        Some(Tag::compound([("count", Tag::Int(-73))])),
    );
    data.bundle_selected = BundleSelected(4);
    let mut child = ItemStack::new("minecraft:dirt", 2);
    child.data_mut().bundle_selected = BundleSelected(8);
    data.container.push((2, child.clone()));
    data.charged.push(child.clone());
    data.pot_decorations = Some(Box::new([Some(child.clone()), None, None, None]));
    data.bundle_contents.push(child);
    stack
}
// A concrete fixture source: Streaming still owns admission/removal and workers.
struct RetainedColumn(Arc<Chunk>);
impl ChunkGenerator for RetainedColumn {
    type Worker = ();
    fn worker(&self) {}
    fn load(&self, x: i32, z: i32) -> Option<Chunk> {
        ((x, z) == (self.0.x, self.0.z)).then(|| self.0.as_ref().clone())
    }
    fn proto(&self, _: &mut (), x: i32, z: i32) -> Proto {
        Box::new(Chunk::empty(x, z))
    }
    fn finish(&self, proto: Proto) -> Chunk {
        *proto.downcast::<Chunk>().unwrap()
    }
}
fn unload_source(game: &mut Game<'_>) {
    let original = game.world.chunks.get(&(0, 0)).unwrap().clone();
    let mut streaming = Streaming::new(
        Arc::new(RetainedColumn(original)),
        game.pack.context.clone(),
        1,
    );
    streaming.mesh = false;
    streaming.radius = 0;
    streaming.center = (40, 0);
    let (_, removed) = streaming.update(&mut game.world);
    assert!(removed.iter().any(|c| (c.x, c.z) == (0, 0)));
    assert!(!game.world.chunks.contains_key(&(0, 0)));
    game.streaming = crate::game::streaming_owner::GameStreaming::from_existing(streaming);
}
fn reload_source(game: &mut Game<'_>) {
    game.streaming.expect_existing_mut().center = (0, 0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !game.world.chunks.contains_key(&(0, 0)) {
        game.streaming.expect_existing_mut().update(&mut game.world);
        assert!(
            std::time::Instant::now() < deadline,
            "authoritative fixture column did not reload"
        );
        if !game.world.chunks.contains_key(&(0, 0)) {
            game.streaming
                .expect_existing_mut()
                .wait(std::time::Duration::from_millis(10));
        }
    }
}
fn assert_raw(actual: &ItemStack, expected: &ItemStack) {
    assert_eq!(actual, expected);
    if let (Some(a), Some(e)) = (actual.data(), expected.data()) {
        assert_eq!(a.bundle_selected.0, e.bundle_selected.0);
        for ((_, a), (_, e)) in a.container.iter().zip(&e.container) {
            assert_raw(a, e);
        }
        for (a, e) in a.charged.iter().zip(&e.charged) {
            assert_raw(a, e);
        }
        for (a, e) in a.bundle_contents.iter().zip(&e.bundle_contents) {
            assert_raw(a, e);
        }
        if let (Some(a), Some(e)) = (&a.pot_decorations, &e.pot_decorations) {
            for (a, e) in a.iter().flatten().zip(e.iter().flatten()) {
                assert_raw(a, e);
            }
        }
    }
}
fn seed(
    game: &mut Game<'_>,
    fixture: &Fixture,
    stack: Option<ItemStack>,
    scope: Option<AuthorityScope>,
) -> SlotKey {
    let state = game
        .pack
        .assets
        .registry
        .default_state("minecraft:iron_block")
        .unwrap();
    if !game.world.chunks.contains_key(&(0, 0)) {
        game.world
            .chunks
            .insert((0, 0), Arc::new(Chunk::empty(0, 0)));
    }
    game.world.set(2, 70, 2, state).unwrap();
    let scope = scope.unwrap_or_else(|| game.observation_scope().unwrap());
    // Explicit native binding only: this test does not install custom block resources.
    let configuration = Configuration::new(
        scope,
        &metadata(fixture),
        BlockState {
            name: "slot_demo:cube".into(),
            properties: Default::default(),
        },
        state,
        POSITION,
    )
    .unwrap();
    let key = game
        .world
        .enable_world_slot_trial(configuration)
        .unwrap()
        .active
        .unwrap()
        .key;
    if let Some(stack) = stack {
        let count = stack.count;
        let mut player = SlotProjection::new(Some(stack));
        let limits = ResolvedLimits::new(u32::MAX, u32::MAX).unwrap();
        let plan = match game
            .world
            .prepare_world_slot_transfer(&key, &player, Direction::IntoMachine, count, limits)
            .unwrap()
        {
            Preparation::Prepared(plan) => *plan,
            other => panic!("expected transfer: {other:?}"),
        };
        game.world
            .commit_world_slot_transfer(plan, &mut player, limits)
            .unwrap();
        assert!(player.into_stack().is_none());
    }
    game.world.set(2, 70, 2, game.world.air).unwrap();
    key
}
fn samples(mut random: LegacyRandom) -> [u32; 3] {
    std::array::from_fn(|_| random.next_float().to_bits())
}
struct Before {
    pending: horizon_client::world_slot_trial::RetirementSnapshot,
    entities: Vec<horizon_client::entities::ItemEntity>,
    id: u32,
    random: [u32; 3],
}
fn capture(game: &mut Game<'_>) -> Before {
    Before {
        pending: game.world.world_slot_snapshot().unwrap().pending.unwrap(),
        entities: game.dropped.clone(),
        id: game.next_entity,
        random: samples(game.particles.random().clone()),
    }
}
fn assert_entity(
    actual: &horizon_client::entities::ItemEntity,
    expected: &horizon_client::entities::ItemEntity,
) {
    assert_raw(&actual.stack, &expected.stack);
    assert_eq!(
        (actual.position, actual.previous, actual.velocity),
        (expected.position, expected.previous, expected.velocity)
    );
    assert_eq!(
        (
            actual.health,
            actual.fire_ticks,
            actual.id,
            actual.thrower,
            actual.target,
            actual.on_ground,
            actual.age,
            actual.pickup_delay,
            actual.bob_offset,
            actual.ticks
        ),
        (
            expected.health,
            expected.fire_ticks,
            expected.id,
            expected.thrower,
            expected.target,
            expected.on_ground,
            expected.age,
            expected.pickup_delay,
            expected.bob_offset,
            expected.ticks
        )
    );
}
fn assert_retained(game: &mut Game<'_>, before: &Before) {
    let now = game.world.world_slot_snapshot().unwrap().pending.unwrap();
    assert_eq!(now.allocation.key, before.pending.allocation.key);
    assert_eq!(
        now.allocation.slot.revision(),
        before.pending.allocation.slot.revision()
    );
    assert_eq!(now.allocation.resident, before.pending.allocation.resident);
    assert_eq!(now.reason, before.pending.reason);
    assert_eq!(now.game_time, before.pending.game_time);
    match (
        now.allocation.slot.stack(),
        before.pending.allocation.slot.stack(),
    ) {
        (Some(a), Some(e)) => assert_raw(a, e),
        (None, None) => {}
        _ => panic!("retirement representation changed"),
    }
    assert_eq!(game.next_entity, before.id);
    assert_eq!(samples(game.particles.random().clone()), before.random);
    assert_eq!(game.dropped.len(), before.entities.len());
    for (actual, expected) in game.dropped.iter().zip(&before.entities) {
        assert_entity(actual, expected);
    }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn slot_receiver_moves_original_metadata_once_and_accepts_representable_oversize() {
    for count in [5, 99, i32::MAX as u32] {
        let fixture = Fixture::new("slot-deliver");
        let mut game = fixture.game();
        game.spawn_item(rich(1), [1.0, 70.0, 1.0], [0.0; 3], 10);
        let expected = rich(count);
        let key = seed(&mut game, &fixture, Some(expected.clone()), None);
        let before = capture(&mut game);
        let mut random = game.particles.random().clone();
        let bob = random.next_float() * std::f32::consts::PI * 2.0;
        assert_eq!(
            game.deliver_pending_world_slot(&key),
            Ok(Delivery::Delivered {
                entity_id: before.id + 1
            })
        );
        assert!(game.world.world_slot_snapshot().unwrap().pending.is_none());
        assert_eq!(game.dropped.len(), before.entities.len() + 1);
        assert_eq!(game.next_entity, before.id + 1);
        assert_entity(&game.dropped[0], &before.entities[0]);
        let entity = game.dropped.last().unwrap();
        assert_raw(&entity.stack, &expected);
        assert_eq!(entity.id, before.id + 1);
        assert_eq!(entity.position, super::Vec3::new(2.5, 70.5, 2.5));
        assert_eq!(entity.previous, entity.position);
        assert_eq!(entity.velocity, super::Vec3::new(0.0, 0.0, 0.0));
        assert_eq!(
            (
                entity.health,
                entity.fire_ticks,
                entity.age,
                entity.pickup_delay,
                entity.ticks
            ),
            (5, 0, 0, 10, 0)
        );
        assert!(!entity.on_ground);
        assert_eq!(entity.thrower, None);
        assert_eq!(entity.target, None);
        assert_eq!(entity.bob_offset, bob);
        assert_eq!(samples(game.particles.random().clone()), samples(random));
        assert_eq!(
            game.deliver_pending_world_slot(&key),
            Err(DeliveryError::NoPending)
        );
        assert_eq!(game.dropped.len(), 2);
        assert_eq!(game.next_entity, before.id + 1);
        assert!(game.world.world_slot_snapshot().unwrap().active.is_none());
    }
    let fixture = Fixture::new("slot-empty");
    let mut game = fixture.game();
    let key = seed(&mut game, &fixture, None, None);
    let before = capture(&mut game);
    assert_eq!(
        game.deliver_pending_world_slot(&key),
        Ok(Delivery::EmptyAcknowledged)
    );
    assert_eq!(game.next_entity, before.id);
    assert_eq!(game.dropped.len(), before.entities.len());
    assert_eq!(samples(game.particles.random().clone()), before.random);
    assert_eq!(
        game.deliver_pending_world_slot(&key),
        Err(DeliveryError::NoPending)
    );
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn slot_receiver_context_id_and_residency_refusals_retain_owner_for_retry() {
    let fixture = Fixture::new("slot-refusals");
    let mut game = fixture.game();
    game.spawn_item(rich(1), [1.0, 70.0, 1.0], [0.0; 3], 10);
    let old = seed(&mut game, &fixture, None, None);
    let empty = game.world.take_world_slot_trial().unwrap();
    assert!(
        empty
            .pending
            .unwrap()
            .into_parts()
            .0
            .into_parts()
            .1
            .into_stack()
            .is_none()
    );
    let key = seed(
        &mut game,
        &fixture,
        Some(rich(5)),
        Some(old.scope().clone()),
    );
    let before = capture(&mut game);
    assert_eq!(old.scope(), key.scope());
    assert_eq!(old.incarnation(), key.incarnation());
    assert_ne!(old, key);
    assert_eq!(
        game.deliver_pending_world_slot(&old),
        Err(DeliveryError::WrongContext)
    );
    assert_retained(&mut game, &before);
    game.dim = Dim::Nether;
    assert_eq!(
        game.deliver_pending_world_slot(&key),
        Err(DeliveryError::WrongContext)
    );
    assert_retained(&mut game, &before);
    game.dim = Dim::Overworld;
    game.next_entity = u32::MAX;
    let exhausted = capture(&mut game);
    assert_eq!(
        game.deliver_pending_world_slot(&key),
        Err(DeliveryError::EntityIdExhausted)
    );
    assert_retained(&mut game, &exhausted);
    game.next_entity = before.id;
    unload_source(&mut game);
    assert_eq!(
        game.deliver_pending_world_slot(&key),
        Err(DeliveryError::NotResident)
    );
    assert_retained(&mut game, &before);
    reload_source(&mut game);
    assert_eq!(
        game.deliver_pending_world_slot(&key),
        Ok(Delivery::Delivered {
            entity_id: before.id + 1
        })
    );
    assert_eq!(game.dropped.len(), 2);
    assert_raw(&game.dropped[1].stack, &rich(5));
    assert_entity(&game.dropped[0], &before.entities[0]);
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn slot_receiver_rejects_root_and_all_modeled_nested_count_failures_without_metadata_loss() {
    for item in ["example:unknown", "minecraft:air", "minecraft:stone"] {
        let fixture = Fixture::new("slot-invalid-root");
        let mut game = fixture.game();
        game.spawn_item(rich(1), [1.0, 70.0, 1.0], [0.0; 3], 10);
        let mut stack = rich(if item == "minecraft:stone" {
            u32::MAX
        } else {
            5
        });
        stack.item = item.into();
        let key = seed(&mut game, &fixture, Some(stack), None);
        let before = capture(&mut game);
        assert_eq!(
            game.deliver_pending_world_slot(&key),
            Err(DeliveryError::UnsupportedStack)
        );
        assert_retained(&mut game, &before);
    }
    for collection in 0..5 {
        for count in [0, u32::MAX] {
            let fixture = Fixture::new("slot-invalid-nested");
            let mut game = fixture.game();
            game.spawn_item(rich(1), [1.0, 70.0, 1.0], [0.0; 3], 10);
            let mut stack = rich(5);
            let child = rich(count);
            let data = stack.data_mut();
            match collection {
                0 => data.container.push((3, child)),
                1 => data.charged.push(child),
                2 => data.bundle_contents.push(child),
                3 => data.pot_decorations = Some(Box::new([Some(child), None, None, None])),
                _ => {
                    let mut intermediate = rich(1);
                    intermediate.data_mut().charged.push(child);
                    data.container.push((3, intermediate));
                }
            }
            let key = seed(&mut game, &fixture, Some(stack), None);
            let before = capture(&mut game);
            assert_eq!(
                game.deliver_pending_world_slot(&key),
                Err(DeliveryError::UnsupportedStack)
            );
            assert_retained(&mut game, &before);
        }
    }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn slot_receiver_parked_world_follows_actual_active_dimension_not_level_metadata() {
    let fixture = Fixture::new("slot-parked");
    let mut game = fixture.game();
    let overworld = seed(&mut game, &fixture, Some(rich(3)), None);
    game.enter_level(None, Dim::Nether).unwrap();
    assert_eq!(game.dim, Dim::Nether);
    assert_eq!(game.level.dimension, Dim::Overworld.id());
    let nether = seed(&mut game, &fixture, Some(rich(5)), None);
    let before = capture(&mut game);
    assert_eq!(
        game.deliver_pending_world_slot(&overworld),
        Err(DeliveryError::WrongContext)
    );
    assert_retained(&mut game, &before);
    assert_eq!(
        game.deliver_pending_world_slot(&nether),
        Ok(Delivery::Delivered {
            entity_id: before.id + 1
        })
    );
    assert_eq!(game.dropped.len(), 1);
    assert_raw(&game.dropped[0].stack, &rich(5));
    game.enter_level(None, Dim::Overworld).unwrap();
    let overworld_before = capture(&mut game);
    assert_eq!(overworld_before.pending.allocation.key, overworld);
    assert!(game.dropped.is_empty());
    assert!(matches!(
        game.deliver_pending_world_slot(&overworld),
        Ok(Delivery::Delivered { .. })
    ));
    assert_eq!(game.dropped.len(), 1);
    assert_raw(&game.dropped[0].stack, &rich(3));
    game.enter_level(None, Dim::Nether).unwrap();
    assert_eq!(game.dropped.len(), 1);
    assert_eq!(
        game.deliver_pending_world_slot(&nether),
        Err(DeliveryError::NoPending)
    );
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn slot_receiver_integrated_frontend_retains_pending_without_dispatching() {
    let fixture = Fixture::new("slot-frontend");
    let mut game = fixture.game();
    let key = seed(&mut game, &fixture, Some(rich(5)), None);
    let before = capture(&mut game);
    let (tasks, _) = std::sync::mpsc::channel();
    game.integrated = Some(super::super::Integrated {
        flags: Arc::new(super::super::Flags::default()),
        thread: None,
        tasks,
        world_name: "inert receiver fixture".into(),
    });
    assert_eq!(
        game.deliver_pending_world_slot(&key),
        Err(DeliveryError::NotAuthoritative)
    );
    assert_retained(&mut game, &before);
    game.integrated.take().unwrap().stop().unwrap();
    assert!(matches!(
        game.deliver_pending_world_slot(&key),
        Ok(Delivery::Delivered { .. })
    ));
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn slot_receiver_remote_memory_client_refuses_without_losing_pending() {
    let fixture = Fixture::new("slot-remote");
    let mut game = fixture.game();
    // current() only reads the process-local OnceLock. This fixture must never
    // inherit a launcher session; the local Pipe route contacts no account service.
    assert!(horizon_client::auth::current().is_none());
    let (client, peer) = horizon_protocol::connection::Pipe::pair();
    game.connect_local(client, "SlotFixture").unwrap();
    peer.shutdown(); // Release any configuration read before Session is dropped.
    assert!(game.remote.is_some());
    let key = seed(&mut game, &fixture, Some(rich(5)), None);
    let before = capture(&mut game);
    assert_eq!(
        game.deliver_pending_world_slot(&key),
        Err(DeliveryError::NotAuthoritative)
    );
    assert_retained(&mut game, &before);
    drop(game.remote.take());
    assert!(matches!(
        game.deliver_pending_world_slot(&key),
        Ok(Delivery::Delivered { .. })
    ));
}
