//! Actual Game player cells, including the real server guest-turn swap.
use super::super::super::slot_player::{PlayerTransferError as E, TransferOutcome};
use super::{Arc, Chunk, Dim, Fixture, Game, ItemStack, Tag};
use horizon_client::{
    counter_trial::Configuration,
    item_data::BundleSelected,
    lan::{Joined, NetConfig, Out, PlayerSender, guest::Guest, hub::HubConfig},
    mutation_observer::AuthorityScope,
    slot_transfer::{Direction, NoMove, ResolvedLimits, SlotProjection},
    streaming::{ChunkGenerator, Proto, Streaming},
    world_slot_trial::{self, Preparation, SlotKey},
};
use horizon_protocol::packets::configuration::ClientInformation;
use horizon_world::{chunk::BlockState, nbt::Tag as Nbt};
use std::{
    fs,
    sync::{atomic::AtomicI32, mpsc},
};

const POS: [i32; 3] = [2, 70, 2];
fn rich(count: u32) -> ItemStack {
    let mut stack = ItemStack::new("minecraft:stone", count);
    stack.damage = 7;
    let data = stack.data_mut();
    data.custom_name = Some("actual player cell".into());
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
fn assert_cell(actual: &Option<ItemStack>, expected: &Option<ItemStack>) {
    match (actual, expected) {
        (Some(a), Some(e)) => assert_raw(a, e),
        (None, None) => {}
        _ => panic!("cell representation changed"),
    }
}
fn seed(
    game: &mut Game<'_>,
    fixture: &Fixture,
    stack: Option<ItemStack>,
    scope: Option<AuthorityScope>,
) -> SlotKey {
    let dir = fixture.root.join("player-slot-metadata");
    for (name, bytes) in [
        ("horizon-paired.toml", b"format=1\nhost-contract='paired-preview-1'\n[content]\nid='slot_player'\nname='Slot'\nversion='0'\napi='0.2'\nside='both'\ncapabilities=['registries']\n[client]\nentry='client.wasm'\napi='0.1'\ncapabilities=[]\n[server]\nentry='server.wasm'\napi='0.3'\ncapabilities=[]\n".as_slice()),
        ("client.wasm", b"not executed".as_slice()),
        ("server.wasm", b"not executed".as_slice()),
        ("data/slot_player/horizon/machine/type.json", br#"{"format":1,"save-schema":1}"#.as_slice()),
        ("data/slot_player/horizon/block/cube.json", br#"{"template":"minecraft:iron_block","machine-type":"slot_player:type"}"#.as_slice()),
    ] {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    let prepared = horizon_mods::paired::PreparedBundle::read_folder(&dir).unwrap();
    let metadata = prepared
        .content()
        .machine_types()
        .get("slot_player:type")
        .unwrap();
    let state = game
        .pack
        .assets
        .registry
        .default_state("minecraft:iron_block")
        .unwrap();
    // A real local connection replaces the fixture World with an empty client
    // World. Supply this owned test column before seeding its refusal case.
    if !game.world.chunks.contains_key(&(0, 0)) {
        game.world
            .chunks
            .insert((0, 0), Arc::new(Chunk::empty(0, 0)));
    }
    game.world.set(POS[0], POS[1], POS[2], state).unwrap();
    let scope = scope.unwrap_or_else(|| game.observation_scope().unwrap());
    let configuration = Configuration::new(
        scope,
        metadata,
        BlockState {
            name: "slot_player:cube".into(),
            properties: Default::default(),
        },
        state,
        POS,
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
        let amount = stack.count;
        let mut source = SlotProjection::new(Some(stack));
        let limits = ResolvedLimits::new(u32::MAX, u32::MAX).unwrap();
        let plan = match game
            .world
            .prepare_world_slot_transfer(&key, &source, Direction::IntoMachine, amount, limits)
            .unwrap()
        {
            Preparation::Prepared(plan) => *plan,
            other => panic!("seed needs positive source: {other:?}"),
        };
        game.world
            .commit_world_slot_transfer(plan, &mut source, limits)
            .unwrap();
        assert!(source.into_stack().is_none());
    }
    key
}
fn assert_machine(game: &Game<'_>, key: &SlotKey, expected: Option<ItemStack>) {
    let actual = game.world.read_world_slot(key).unwrap();
    assert_cell(&actual.slot.stack().cloned(), &expected);
}
fn owners(game: &Game<'_>) -> (world_slot_trial::Snapshot, Vec<Option<ItemStack>>) {
    (
        game.world.world_slot_snapshot().unwrap(),
        game.inventory.clone(),
    )
}
fn assert_owners(game: &Game<'_>, before: &(world_slot_trial::Snapshot, Vec<Option<ItemStack>>)) {
    assert_eq!(game.inventory.len(), before.1.len());
    for (a, e) in game.inventory.iter().zip(&before.1) {
        assert_cell(a, e);
    }
    let now = game.world.world_slot_snapshot().unwrap();
    assert_eq!(now.fault, before.0.fault);
    fn active(s: &world_slot_trial::Snapshot) -> Option<&world_slot_trial::ActiveSnapshot> {
        s.active
            .as_ref()
            .or_else(|| s.pending.as_ref().map(|p| &p.allocation))
    }
    match (active(&now), active(&before.0)) {
        (Some(a), Some(e)) => {
            assert_eq!(a.key, e.key);
            assert_eq!(a.slot.revision(), e.slot.revision());
            assert_eq!(a.resident, e.resident);
            assert_cell(&a.slot.stack().cloned(), &e.slot.stack().cloned());
        }
        (None, None) => {}
        _ => panic!("World slot owner changed"),
    }
    assert_eq!(now.active.is_some(), before.0.active.is_some());
    assert_eq!(now.pending.is_some(), before.0.pending.is_some());
}
fn moved(result: Result<TransferOutcome, E>, amount: u32) {
    let Ok(TransferOutcome::Committed(committed)) = result else {
        panic!("expected committed transfer: {result:?}");
    };
    assert_eq!(committed.moved, amount);
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn player_cell_transfers_both_directions_and_retains_raw_metadata() {
    let fixture = Fixture::new("player-both-directions");
    let mut game = fixture.game();
    game.inventory = vec![None; 36];
    game.inventory[0] = Some(rich(100));
    game.inventory[35] = Some(ItemStack::new("minecraft:diamond", 13));
    let key = seed(&mut game, &fixture, None, None);
    moved(
        game.transfer_world_slot_inventory_cell(&key, 0, Direction::IntoMachine, u32::MAX),
        64,
    );
    assert_cell(&game.inventory[0], &Some(rich(36)));
    assert_machine(&game, &key, Some(rich(64)));
    moved(
        game.transfer_world_slot_inventory_cell(&key, 5, Direction::IntoPlayer, 9),
        9,
    );
    assert_cell(&game.inventory[5], &Some(rich(9)));
    assert_machine(&game, &key, Some(rich(55)));
    moved(
        game.transfer_world_slot_inventory_cell(&key, 5, Direction::IntoPlayer, u32::MAX),
        55,
    );
    assert_cell(&game.inventory[5], &Some(rich(64)));
    assert_machine(&game, &key, None);
    moved(
        game.transfer_world_slot_inventory_cell(&key, 0, Direction::IntoMachine, u32::MAX),
        36,
    );
    assert!(game.inventory[0].is_none());
    assert_machine(&game, &key, Some(rich(36)));
    assert_cell(
        &game.inventory[35],
        &Some(ItemStack::new("minecraft:diamond", 13)),
    );
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn player_cell_no_move_preserves_original_allocation_and_both_owners() {
    let fixture = Fixture::new("player-no-move");
    let mut game = fixture.game();
    game.inventory = vec![None; 36];
    let key = seed(&mut game, &fixture, Some(rich(64)), None);
    game.inventory[0] = Some(rich(3));
    for (amount, reason) in [(0, NoMove::ZeroRequested), (9, NoMove::NoCapacity)] {
        let before = owners(&game);
        let allocation = game.inventory[0]
            .as_ref()
            .unwrap()
            .extra
            .as_deref()
            .unwrap() as *const _;
        assert_eq!(
            game.transfer_world_slot_inventory_cell(&key, 0, Direction::IntoMachine, amount),
            Ok(TransferOutcome::NoMove(reason))
        );
        assert_owners(&game, &before);
        assert_eq!(
            game.inventory[0]
                .as_ref()
                .unwrap()
                .extra
                .as_deref()
                .unwrap() as *const _,
            allocation
        );
    }
    game.inventory[0].as_mut().unwrap().data_mut().custom_name = Some("incompatible".into());
    let before = owners(&game);
    assert_eq!(
        game.transfer_world_slot_inventory_cell(&key, 0, Direction::IntoMachine, 9),
        Ok(TransferOutcome::NoMove(NoMove::Incompatible))
    );
    assert_owners(&game, &before);
    game.inventory[0] = None;
    let before = owners(&game);
    assert_eq!(
        game.transfer_world_slot_inventory_cell(&key, 0, Direction::IntoMachine, 9),
        Ok(TransferOutcome::NoMove(NoMove::EmptySource))
    );
    assert_owners(&game, &before);
    let empty_fixture = Fixture::new("player-both-empty");
    let mut empty = empty_fixture.game();
    empty.inventory = vec![None; 36];
    let empty_key = seed(&mut empty, &empty_fixture, None, None);
    assert_eq!(
        empty.transfer_world_slot_inventory_cell(&empty_key, 0, Direction::IntoPlayer, 1),
        Ok(TransferOutcome::NoMove(NoMove::EmptySource))
    );
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn player_cell_uses_native_registry_and_component_stack_limits() {
    for direction in [Direction::IntoMachine, Direction::IntoPlayer] {
        for (item, component, limit) in [
            ("minecraft:stone", None, 64),
            ("minecraft:diamond_sword", None, 1),
            ("minecraft:stone", Some(Nbt::Int(-42)), 1),
            ("minecraft:stone", Some(Nbt::Int(0)), 1),
            ("minecraft:stone", Some(Nbt::Int(7)), 7),
            ("minecraft:stone", Some(Nbt::Int(512)), 99),
            (
                "minecraft:stone",
                Some(Nbt::String("wrong type".into())),
                64,
            ),
        ] {
            let fixture = Fixture::new("player-native-limits");
            let mut game = fixture.game();
            game.inventory = vec![None; 36];
            let mut source = ItemStack::new(item, 120);
            if let Some(component) = component {
                source
                    .data_mut()
                    .set_other("minecraft:max_stack_size", Some(component));
            }
            let key = seed(
                &mut game,
                &fixture,
                (direction == Direction::IntoPlayer).then(|| source.clone()),
                None,
            );
            if direction == Direction::IntoMachine {
                game.inventory[0] = Some(source.clone());
            }
            moved(
                game.transfer_world_slot_inventory_cell(&key, 0, direction, u32::MAX),
                limit,
            );
            let mut destination = source.clone();
            destination.count = limit;
            source.count -= limit;
            if direction == Direction::IntoMachine {
                assert_machine(&game, &key, Some(destination));
                assert_cell(&game.inventory[0], &Some(source));
            } else {
                assert_machine(&game, &key, Some(source));
                assert_cell(&game.inventory[0], &Some(destination));
            }
        }
    }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn player_cell_invalid_existing_data_refuses_before_no_move_without_loss() {
    let mut invalid = vec![rich(0), rich(u32::MAX)];
    for item in ["minecraft:air", "example:unknown"] {
        let mut s = rich(5);
        s.item = item.into();
        invalid.push(s);
    }
    for count in [0, u32::MAX] {
        for collection in 0..4 {
            let mut stack = rich(5);
            let child = rich(count);
            match collection {
                0 => stack.data_mut().container.push((3, child)),
                1 => stack.data_mut().charged.push(child),
                2 => stack.data_mut().bundle_contents.push(child),
                _ => {
                    stack.data_mut().pot_decorations =
                        Some(Box::new([Some(child), None, None, None]))
                }
            }
            invalid.push(stack);
        }
    }
    for stack in invalid {
        for machine in [false, true] {
            // Zero roots cannot enter the current World slot admission/transfer path.
            if machine && stack.count == 0 {
                continue;
            }
            let fixture = Fixture::new("player-invalid-data");
            let mut game = fixture.game();
            game.inventory = vec![None; 36];
            let key = seed(&mut game, &fixture, machine.then(|| stack.clone()), None);
            if !machine {
                game.inventory[0] = Some(stack.clone());
            }
            let before = owners(&game);
            assert_eq!(
                game.transfer_world_slot_inventory_cell(&key, 0, Direction::IntoMachine, 0),
                Err(E::UnsupportedStack)
            );
            assert_owners(&game, &before);
        }
    }
}

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
#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn player_cell_context_address_and_streaming_refusals_restore_for_retry() {
    let fixture = Fixture::new("player-context");
    let mut game = fixture.game();
    game.inventory = vec![None; 40];
    game.inventory[0] = Some(rich(5));
    let key = seed(&mut game, &fixture, None, None);
    for index in [36, 40, usize::MAX] {
        let before = owners(&game);
        assert_eq!(
            game.transfer_world_slot_inventory_cell(&key, index, Direction::IntoMachine, 1),
            Err(E::InvalidIndex)
        );
        assert_owners(&game, &before);
    }
    game.inventory.truncate(2);
    assert_eq!(
        game.transfer_world_slot_inventory_cell(&key, 2, Direction::IntoMachine, 1),
        Err(E::InvalidIndex)
    );
    let player = game.player.take();
    let before = owners(&game);
    assert_eq!(
        game.transfer_world_slot_inventory_cell(&key, 0, Direction::IntoMachine, 1),
        Err(E::NoPlayer)
    );
    assert_owners(&game, &before);
    game.player = player;
    game.dim = Dim::Nether;
    let before = owners(&game);
    assert_eq!(
        game.transfer_world_slot_inventory_cell(&key, 0, Direction::IntoMachine, 0),
        Err(E::WrongContext)
    );
    assert_owners(&game, &before);
    game.dim = Dim::Overworld;
    let original = game.world.chunks.get(&(0, 0)).unwrap().clone();
    let mut streaming = Streaming::new(
        Arc::new(RetainedColumn(original)),
        game.pack.context.clone(),
        1,
    );
    streaming.mesh = false;
    streaming.radius = 0;
    streaming.center = (40, 0);
    assert!(
        streaming
            .update(&mut game.world)
            .1
            .iter()
            .any(|c| (c.x, c.z) == (0, 0))
    );
    let before = owners(&game);
    let allocation = game.inventory[0]
        .as_ref()
        .unwrap()
        .extra
        .as_deref()
        .unwrap() as *const _;
    assert_eq!(
        game.transfer_world_slot_inventory_cell(&key, 0, Direction::IntoMachine, 1),
        Err(E::World(world_slot_trial::Error::NotResident))
    );
    assert_owners(&game, &before);
    assert_eq!(
        game.inventory[0]
            .as_ref()
            .unwrap()
            .extra
            .as_deref()
            .unwrap() as *const _,
        allocation
    );
    streaming.center = (0, 0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !game.world.chunks.contains_key(&(0, 0)) {
        streaming.update(&mut game.world);
        assert!(std::time::Instant::now() < deadline);
        if !game.world.chunks.contains_key(&(0, 0)) {
            streaming.wait(std::time::Duration::from_millis(10));
        }
    }
    moved(
        game.transfer_world_slot_inventory_cell(&key, 0, Direction::IntoMachine, 1),
        1,
    );
    let state = game
        .pack
        .assets
        .registry
        .default_state("minecraft:iron_block")
        .unwrap();
    game.world
        .set(POS[0], POS[1], POS[2], game.world.air)
        .unwrap();
    let before = owners(&game);
    assert_eq!(
        game.transfer_world_slot_inventory_cell(&key, 0, Direction::IntoMachine, 1),
        Err(E::World(world_slot_trial::Error::PendingRetirement))
    );
    assert_owners(&game, &before);
    let retired = game.world.take_world_slot_retirement().unwrap();
    game.world.set(POS[0], POS[1], POS[2], state).unwrap();
    let current = game
        .world
        .reconcile_world_slot_trial()
        .unwrap()
        .active
        .unwrap()
        .key;
    assert_ne!(current, key);
    let before = owners(&game);
    assert_eq!(
        game.transfer_world_slot_inventory_cell(&key, 0, Direction::IntoMachine, 1),
        Err(E::WrongContext)
    );
    assert_owners(&game, &before);
    // Keep the removed owner alive; this test is not a retirement receiver.
    assert!(retired.snapshot().allocation.slot.stack().is_some());
    let old = game.world.take_world_slot_trial().unwrap();
    let foreign = seed(&mut game, &fixture, None, Some(current.scope().clone()));
    assert_eq!(foreign.incarnation(), key.incarnation());
    let before = owners(&game);
    assert_eq!(
        game.transfer_world_slot_inventory_cell(&key, 0, Direction::IntoMachine, 1),
        Err(E::WrongContext)
    );
    assert_owners(&game, &before);
    assert!(old.active.is_some());
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn player_cell_refuses_integrated_frontend_and_actual_remote_memory_client() {
    let fixture = Fixture::new("player-frontends");
    let mut game = fixture.game();
    game.inventory = vec![None; 36];
    game.inventory[0] = Some(rich(5));
    let key = seed(&mut game, &fixture, None, None);
    let before = owners(&game);
    let (tasks, _) = mpsc::channel();
    game.integrated = Some(super::super::Integrated {
        flags: Arc::new(super::super::Flags::default()),
        thread: None,
        tasks,
        world_name: "inert player-cell fixture".into(),
    });
    assert_eq!(
        game.transfer_world_slot_inventory_cell(&key, 0, Direction::IntoMachine, 1),
        Err(E::NotAuthoritative)
    );
    assert_owners(&game, &before);
    game.integrated.take().unwrap().stop().unwrap();
    let remote_fixture = Fixture::new("player-remote");
    let mut remote = remote_fixture.game();
    assert!(horizon_client::auth::current().is_none());
    let (client, peer) = horizon_protocol::connection::Pipe::pair();
    remote.connect_local(client, "PlayerCellFixture").unwrap();
    peer.shutdown();
    assert!(remote.remote.is_some());
    remote.inventory = vec![None; 36];
    remote.inventory[0] = Some(rich(5));
    let remote_key = seed(&mut remote, &remote_fixture, None, None);
    let before = owners(&remote);
    assert_eq!(
        remote.transfer_world_slot_inventory_cell(&remote_key, 0, Direction::IntoMachine, 1),
        Err(E::NotAuthoritative)
    );
    assert_owners(&remote, &before);
    drop(remote.remote.take());
}

fn guest(conn: u32, stack: ItemStack) -> (Guest, mpsc::Receiver<Out>) {
    let (tx, rx) = mpsc::channel();
    let mut guest = Guest::new(
        Joined {
            conn,
            uuid: u128::from(conn),
            name: format!("CellGuest{conn}"),
            properties: Vec::new(),
            info: ClientInformation::new(2),
            brand: "local fixture".into(),
            sender: PlayerSender::new(tx, Arc::default()),
            latency: Arc::new(AtomicI32::new(0)),
            address: "127.0.0.1:25565".parse().unwrap(),
        },
        conn as i32,
        [8.0, 72.0, 8.0],
        2,
    );
    guest.placed = true;
    guest.loaded = true;
    guest.dimension = Dim::Overworld.id().into();
    guest.inventory[0] = Some(stack);
    (guest, rx)
}
#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn player_cell_actual_guest_turn_changes_only_the_selected_players_inventory() {
    let fixture = Fixture::new("player-real-guest-turn");
    let mut game = fixture.game();
    game.inventory = vec![None; 36];
    game.inventory[0] = Some(rich(41));
    let key = seed(&mut game, &fixture, None, None);
    let net = NetConfig {
        listen: false,
        announce: false,
        online: None,
        ..NetConfig::lan(0, "owned fixture".into(), 4)
    };
    let config = HubConfig {
        default_game_mode: 0,
        guest_commands: false,
        force_game_mode: false,
        hardcore: false,
        view_distance: 2,
        simulation_distance: 2,
        dimension: game.dim.id().into(),
        seed: 17,
        max_players: 4,
        dedicated: true,
        integrated: true,
    };
    game.lan_host(
        super::super::super::lan::LanSettings {
            port: 0,
            guest_commands: false,
            force_game_mode: false,
        },
        net,
        config,
        None,
    )
    .unwrap();
    let mut host = game.lan.take().unwrap();
    let (first, _first_rx) = guest(11, rich(12));
    let (other, _other_rx) = guest(12, rich(17));
    host.hub.guests.extend([first, other]);
    let result = game
        .as_guest(&mut host, 11, |game| {
            assert_eq!(game.lan_acting, Some(11));
            assert_cell(&game.inventory[0], &Some(rich(12)));
            game.transfer_world_slot_inventory_cell(&key, 0, Direction::IntoMachine, 5)
        })
        .unwrap();
    moved(result, 5);
    assert_cell(&game.inventory[0], &Some(rich(41)));
    assert_cell(&host.hub.guests[0].inventory[0], &Some(rich(7)));
    assert_cell(&host.hub.guests[1].inventory[0], &Some(rich(17)));
    assert_machine(&game, &key, Some(rich(5)));
    let result = game
        .as_guest(&mut host, 11, |game| {
            game.transfer_world_slot_inventory_cell(&key, 0, Direction::IntoPlayer, 2)
        })
        .unwrap();
    moved(result, 2);
    assert_cell(&host.hub.guests[0].inventory[0], &Some(rich(9)));
    assert_machine(&game, &key, Some(rich(3)));
    let result = game
        .as_guest(&mut host, 11, |game| {
            game.transfer_world_slot_inventory_cell(&key, 36, Direction::IntoMachine, 5)
        })
        .unwrap();
    assert_eq!(result, Err(E::InvalidIndex));
    assert_cell(&host.hub.guests[0].inventory[0], &Some(rich(9)));
    assert_cell(&host.hub.guests[1].inventory[0], &Some(rich(17)));
    assert_cell(&game.inventory[0], &Some(rich(41)));
    assert!(game.lan_acting.is_none());
    game.world
        .set(POS[0], POS[1], POS[2], game.world.air)
        .unwrap();
    let result = game
        .as_guest(&mut host, 11, |game| {
            game.transfer_world_slot_inventory_cell(&key, 0, Direction::IntoMachine, 1)
        })
        .unwrap();
    assert_eq!(
        result,
        Err(E::World(world_slot_trial::Error::PendingRetirement))
    );
    assert_cell(&host.hub.guests[0].inventory[0], &Some(rich(9)));
    assert_cell(&host.hub.guests[1].inventory[0], &Some(rich(17)));
    assert_cell(&game.inventory[0], &Some(rich(41)));
    let pending = game.world.world_slot_snapshot().unwrap().pending.unwrap();
    assert_cell(&pending.allocation.slot.stack().cloned(), &Some(rich(3)));
    assert!(game.lan_acting.is_none());
    game.lan = Some(host);
}
