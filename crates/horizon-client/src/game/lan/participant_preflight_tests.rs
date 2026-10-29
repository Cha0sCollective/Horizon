// Actual Game observations. The coordinator supplies pinned resources and owned process homes.
use super::super::{
    Dim, execution_role::ExecutionRole, integrated::transition_ownership_tests::Fixture,
};
use super::*;
use ParticipantPreflightError as E;
use horizon_client::{
    item_data::BundleSelected,
    lan::{Joined, Out, PlayerSender, guest::Guest},
    motion::PlayerMotion,
};
use horizon_protocol::packets::configuration::ClientInformation;
use horizon_world::nbt::Tag;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicI32, Ordering},
        mpsc,
    },
};

fn rich() -> ItemStack {
    let mut stack = ItemStack::new("minecraft:stone", 7);
    stack.damage = 3;
    let data = stack.data_mut();
    data.bundle_selected = BundleSelected(4);
    data.set_other(
        "example:opaque",
        Some(Tag::compound([("value", Tag::Int(-73))])),
    );
    let mut nested = ItemStack::new("minecraft:dirt", 2);
    nested.data_mut().bundle_selected = BundleSelected(9);
    data.container.push((2, nested));
    stack
}
fn raw(stack: &ItemStack) -> (i32, i32) {
    let data = stack.data().unwrap();
    (
        data.bundle_selected.0,
        data.container[0].1.data().unwrap().bundle_selected.0,
    )
}
fn guest(conn: u32) -> (Guest, mpsc::Receiver<Out>) {
    let (tx, rx) = mpsc::channel();
    let mut g = Guest::new(
        Joined {
            conn,
            uuid: u128::from(conn),
            name: format!("RosterGuest{conn}"),
            properties: Vec::new(),
            info: ClientInformation::new(2),
            brand: "owned fixture".into(),
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
    g.dimension = Dim::Overworld.id().into();
    g.saved = Some(Tag::compound([("fixture:opaque", Tag::Int(-83))]));
    if conn == 11 {
        g.inventory[0] = Some(rich());
    }
    (g, rx)
}
fn start_host(game: &mut Game<'_>, owner: Option<u128>) {
    let net = NetConfig {
        listen: false,
        announce: false,
        online: None,
        ..NetConfig::lan(0, "owned roster fixture".into(), 4)
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
        dedicated: owner.is_none(),
        integrated: owner.is_some(),
    };
    game.lan_host(
        LanSettings {
            port: 0,
            guest_commands: false,
            force_game_mode: false,
        },
        net,
        config,
        owner,
    )
    .unwrap();
}
fn ready(
    fixture: &Fixture,
    owner: bool,
) -> (Game<'_>, Option<PlayerMotion>, Vec<mpsc::Receiver<Out>>) {
    let mut game = fixture.game();
    let local_body = game.player.take(); // Keep the fixture's synthetic body owned by the test.
    assert!(local_body.is_some());
    assert!(game.inventory.iter().all(Option::is_none));
    start_host(&mut game, owner.then_some(11));
    let mut host = game.lan.take().unwrap();
    let (first, first_rx) = guest(11);
    let (second, second_rx) = guest(12);
    host.hub.guests.extend([first, second]);
    for conn in [11, 12] {
        game.as_guest(&mut host, conn, |g| assert_eq!(g.lan_acting, Some(conn)))
            .unwrap();
    }
    if owner {
        game.level.player_uuid = Some([101, 202, 303, 404]);
        host.owner = Some(Owner {
            uuid: 11,
            saved: None,
        });
    }
    game.lan = Some(host);
    (game, local_body, vec![first_rx, second_rx])
}
fn files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn visit(root: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        if !root.exists() {
            return;
        }
        for entry in fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(&path, out);
            } else {
                out.push((path.clone(), fs::read(path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    visit(root, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}
#[derive(Debug, PartialEq)]
struct OwnerView {
    conn: u32,
    tag: Tag,
    stacks: Vec<Option<ItemStack>>,
    selections: Vec<(i32, i32)>,
    allocations: Vec<Option<usize>>,
    notifications: Vec<String>,
    criteria_notifications: Vec<usize>,
}
type Owners = Vec<OwnerView>;
fn owners(game: &Game<'_>) -> Owners {
    game.lan
        .as_ref()
        .unwrap()
        .hub
        .guests
        .iter()
        .map(|g| {
            let mut level = horizon_client::save::Level::default();
            playerdata::store(g, &mut level);
            let values: Vec<_> = g
                .inventory
                .iter()
                .chain(g.armor.iter())
                .chain(std::iter::once(&g.offhand))
                .chain(std::iter::once(&g.carried))
                .chain(g.craft_grid.iter())
                .chain(std::iter::once(&g.craft_result))
                .chain(g.ender_items.iter())
                .collect();
            OwnerView {
                conn: g.conn,
                tag: level.player_tag(),
                stacks: values.iter().map(|s| (**s).clone()).collect(),
                selections: values
                    .iter()
                    .filter_map(|s| s.as_ref())
                    .filter(|s| s.data().is_some_and(|d| !d.container.is_empty()))
                    .map(raw)
                    .collect(),
                allocations: values
                    .iter()
                    .map(|s| {
                        s.as_ref()
                            .and_then(|s| s.extra.as_deref())
                            .map(|d| d as *const _ as usize)
                    })
                    .collect(),
                notifications: g.recipes_unlocked.clone(),
                criteria_notifications: g.advancements_changed.iter().copied().collect(),
            }
        })
        .collect()
}
fn assert_unchanged(game: &Game<'_>, before: &Owners, disk: &[(PathBuf, Vec<u8>)]) {
    assert_eq!(&owners(game), before);
    assert_eq!(files(&game.world_root), disk);
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn participant_native_mapping_is_order_independent_and_observations_do_not_mutate() {
    for owner in [false, true] {
        let fixture = Fixture::new("participant-mapping");
        let (mut game, _body, _receivers) = ready(&fixture, owner);
        let expected = game.observe_checkpoint_participants().unwrap();
        let first = expected.identities.iter().find(|p| p.conn == 11).unwrap();
        assert_eq!(
            first.storage,
            if owner {
                ParticipantStorage::IntegratedOwner {
                    record_uuid: [101, 202, 303, 404],
                }
            } else {
                ParticipantStorage::External {
                    record_uuid: playerdata::uuid_ints(11),
                }
            }
        );
        assert_eq!(
            expected
                .identities
                .iter()
                .find(|p| p.conn == 12)
                .unwrap()
                .storage,
            ParticipantStorage::External {
                record_uuid: playerdata::uuid_ints(12)
            }
        );
        game.lan.as_mut().unwrap().hub.guests[0]
            .recipes_unlocked
            .push("fixture:notification".into());
        game.lan.as_mut().unwrap().hub.guests[0]
            .advancements_changed
            .insert(0);
        let before = owners(&game);
        let disk = files(&game.world_root);
        for _ in 0..3 {
            assert_eq!(game.preflight_checkpoint_participants(&expected), Ok(()));
        }
        assert_unchanged(&game, &before, &disk);
        game.lan.as_mut().unwrap().hub.guests.swap(0, 1);
        game.lan.as_ref().unwrap().hub.guests[0]
            .sender
            .pending
            .store(37, Ordering::Relaxed);
        let reordered = owners(&game);
        assert_eq!(game.preflight_checkpoint_participants(&expected), Ok(()));
        assert_unchanged(&game, &reordered, &disk);
    }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn participant_roster_rejects_changed_missing_duplicate_and_invalid_mappings() {
    for case in 0..13 {
        let fixture = Fixture::new("participant-mismatch");
        let (mut game, _body, mut receivers) = ready(&fixture, true);
        let mut expected = game.observe_checkpoint_participants().unwrap();
        let host = game.lan.as_mut().unwrap();
        let removed = match case {
            0 => Some(host.hub.guests.pop().unwrap()),
            1 => {
                host.hub.guests[1].conn = 99;
                None
            }
            2 => {
                host.hub.guests[1].uuid = 99;
                None
            }
            3 => {
                host.hub.guests[1].dimension = Dim::Nether.id().into();
                None
            }
            4 => {
                host.hub.guests[1].conn = 11;
                None
            }
            5 => {
                host.hub.guests[1].uuid = 11;
                None
            }
            6 => {
                host.hub.guests[1].dimension = "example:invalid".into();
                None
            }
            7 => {
                game.level.player_uuid = None;
                None
            }
            8 => {
                host.owner.as_mut().unwrap().uuid = 99;
                None
            }
            9 => {
                host.owner.as_mut().unwrap().saved = Some(Box::default());
                None
            }
            10 => {
                game.level.player_uuid = Some(playerdata::uuid_ints(12));
                None
            }
            11 => {
                expected.identities[1].conn = expected.identities[0].conn;
                None
            }
            _ => {
                let (extra, rx) = guest(13);
                host.hub.guests.push(extra);
                receivers.push(rx);
                None
            }
        };
        let before = owners(&game);
        let disk = files(&game.world_root);
        let error = match case {
            0..=3 | 12 => E::RosterMismatch,
            4 | 11 => E::DuplicateConnection { conn: 11 },
            5 => E::DuplicateUuid { conn: 12 },
            6 => E::InvalidIdentity { conn: 12 },
            7 => E::MissingMapping,
            8 => E::MissingOwner,
            9 => E::OwnerMismatch,
            10 => E::DuplicateRecord { conn: 12 },
            _ => unreachable!(),
        };
        assert_eq!(
            game.preflight_checkpoint_participants(&expected),
            Err(error),
            "case {case}"
        );
        assert_unchanged(&game, &before, &disk);
        drop(removed); // A missing participant is retained by the test until refusal is checked.
    }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn participant_host_and_sender_allocation_identity_refuse_same_tuple_replacements() {
    let fixture = Fixture::new("participant-identity");
    let (mut game, _body, _receivers) = ready(&fixture, false);
    let expected = game.observe_checkpoint_participants().unwrap();
    let original = game.lan.as_ref().unwrap().hub.guests[0].sender.clone();
    let (tx, _rx) = mpsc::channel();
    game.lan.as_mut().unwrap().hub.guests[0].sender = PlayerSender::new(tx, Arc::default());
    assert_eq!(
        game.preflight_checkpoint_participants(&expected),
        Err(E::RosterMismatch)
    );
    game.lan.as_mut().unwrap().hub.guests[0].sender = original.clone();
    assert_eq!(game.preflight_checkpoint_participants(&expected), Ok(()));
    let mut old = game.lan.take().unwrap();
    start_host(&mut game, None);
    let host = game.lan.as_mut().unwrap();
    host.hub.guests = std::mem::take(&mut old.hub.guests);
    host.play = std::mem::take(&mut old.play);
    assert_eq!(
        game.preflight_checkpoint_participants(&expected),
        Err(E::RosterMismatch)
    );
    let new = game.observe_checkpoint_participants().unwrap();
    assert_eq!(game.preflight_checkpoint_participants(&new), Ok(()));
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn participant_player_owners_and_admitted_activity_refuse_without_consuming_values() {
    for case in 0..17 {
        let fixture = Fixture::new("participant-owner-refusal");
        let (mut game, _body, _receivers) = ready(&fixture, false);
        let expected = game.observe_checkpoint_participants().unwrap();
        let g = &mut game.lan.as_mut().unwrap().hub.guests[0];
        match case {
            0 => g.carried = Some(rich()),
            1 => g.craft_grid[0] = Some(rich()),
            2 => g.craft_result = Some(rich()),
            3 => g.drag = Some((0, vec![0, 1])),
            4 => g.awaiting_teleport = Some((1, [1.0, 2.0, 3.0])),
            5 => g.vehicle = Some(7),
            6 => g.sleeping = true,
            7 => g.bed = Some([1, 2, 3]),
            8 => g.dead = true,
            9 => g.left_reason = "native pending retirement".into(),
            10 => g.loaded = false,
            11 => {
                g.motion.take();
            }
            12 => g
                .inbox
                .push_back(horizon_protocol::packets::play::Serverbound::ClientTickEnd(
                    horizon_protocol::packets::Empty,
                )),
            13 => {
                g.inventory.pop();
            }
            14 => g.selected = 9,
            15 => {
                g.loaded_body = Some(horizon_client::lan::guest::SavedBody {
                    fire_ticks: 7,
                    air: 13,
                    fall_distance: 3.5,
                })
            }
            _ => {
                g.ender_items.pop();
            }
        }
        let before = owners(&game);
        let disk = files(&game.world_root);
        for _ in 0..2 {
            assert!(
                game.preflight_checkpoint_participants(&expected).is_err(),
                "case {case}"
            );
        }
        assert_unchanged(&game, &before, &disk);
        if case == 0 {
            assert_eq!(
                raw(game.lan.as_ref().unwrap().hub.guests[0]
                    .carried
                    .as_ref()
                    .unwrap()),
                (4, 9)
            );
        }
    }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn participant_real_menu_openers_orphans_and_guest_turn_are_not_capture_admission() {
    for kind in 0..3 {
        let fixture = Fixture::new("participant-menu");
        let (mut game, _body, _receivers) = ready(&fixture, false);
        let expected = game.observe_checkpoint_participants().unwrap();
        let mut host = game.lan.take().unwrap();
        if kind < 2 {
            let state = game
                .pack
                .assets
                .registry
                .default_state(if kind == 0 {
                    "minecraft:crafting_table"
                } else {
                    "minecraft:anvil"
                })
                .unwrap();
            game.world.set(2, 70, 2, state).unwrap();
        }
        match kind {
            0 => game.lan_open_crafting(&mut host, 11, [2, 70, 2]),
            1 => {
                let station = super::super::stations::Station::new_kind(
                    super::super::stations::Kind::Anvil,
                    [2, 70, 2],
                );
                assert!(game.lan_open_station(&mut host, 11, &station));
            }
            _ => game.lan_open_merchant(
                &mut host,
                11,
                super::super::trading::MerchantState {
                    session: Box::new(horizon_client::merchant::Session::new(Vec::new(), false)),
                    scroll: 0,
                    focused: None,
                    mob: None,
                    name: None,
                },
            ),
        }
        game.lan = Some(host);
        let before = owners(&game);
        let disk = files(&game.world_root);
        assert_eq!(
            game.preflight_checkpoint_participants(&expected),
            Err(E::UnsupportedPlayer { conn: 11 })
        );
        assert_unchanged(&game, &before, &disk);
    }
    let fixture = Fixture::new("participant-turn");
    let (mut game, _body, _receivers) = ready(&fixture, false);
    let expected = game.observe_checkpoint_participants().unwrap();
    let mut host = game.lan.take().unwrap();
    assert_eq!(
        game.as_guest(&mut host, 11, |g| g
            .preflight_checkpoint_participants(&expected))
            .unwrap(),
        Err(E::ActingGuest)
    );
    host.play.insert(99, Default::default());
    game.lan = Some(host);
    assert_eq!(
        game.preflight_checkpoint_participants(&expected),
        Err(E::OrphanPlay { conn: 99 })
    );
    game.lan.as_mut().unwrap().play.remove(&99);
    game.lan
        .as_mut()
        .unwrap()
        .play
        .get_mut(&11)
        .unwrap()
        .left_game = true;
    assert_eq!(
        game.preflight_checkpoint_participants(&expected),
        Err(E::UnsupportedPlayer { conn: 11 })
    );
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn participant_authority_and_server_local_ownership_are_refused() {
    let fixture = Fixture::new("participant-server-state");
    let (mut game, mut body, _receivers) = ready(&fixture, false);
    let expected = game.observe_checkpoint_participants().unwrap();
    game.player = body.take();
    assert_eq!(
        game.preflight_checkpoint_participants(&expected),
        Err(E::UnsupportedServerState)
    );
    body = game.player.take();
    game.scratch_slot = Some(rich());
    assert_eq!(
        game.preflight_checkpoint_participants(&expected),
        Err(E::UnsupportedServerState)
    );
    let scratch = game.scratch_slot.take().unwrap();
    assert_eq!(raw(&scratch), (4, 9));
    game.saved_for_exit = true;
    assert_eq!(
        game.preflight_checkpoint_participants(&expected),
        Err(E::UnsupportedServerState)
    );
    game.saved_for_exit = false;
    let (reply, _response) = mpsc::channel();
    game.begin_control_save(false, reply).unwrap();
    assert_eq!(
        game.preflight_checkpoint_participants(&expected),
        Err(E::UnsupportedServerState)
    );
    let _pending = game.pending_save.take().unwrap();
    game.options.execution_role = ExecutionRole::Client;
    assert_eq!(
        game.preflight_checkpoint_participants(&expected),
        Err(E::NotAuthoritative)
    );
    game.options.execution_role = ExecutionRole::IntegratedServer;
    let host = game.lan.take().unwrap();
    assert_eq!(
        game.preflight_checkpoint_participants(&expected),
        Err(E::MissingHost)
    );
    game.lan = Some(host);
    assert_eq!(game.preflight_checkpoint_participants(&expected), Ok(()));
    let remote_fixture = Fixture::new("participant-remote");
    let mut remote = remote_fixture.game();
    assert!(horizon_client::auth::current().is_none());
    let (client, peer) = horizon_protocol::connection::Pipe::pair();
    remote.connect_local(client, "RosterFixture").unwrap();
    peer.shutdown();
    assert!(remote.remote.is_some());
    assert_eq!(
        remote.preflight_checkpoint_participants(&expected),
        Err(E::NotAuthoritative)
    );
    drop(remote.remote.take());
    drop(body);
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn participant_success_is_explicitly_not_global_entity_readiness() {
    let fixture = Fixture::new("participant-not-global");
    let (mut game, _body, _receivers) = ready(&fixture, false);
    let expected = game.observe_checkpoint_participants().unwrap();
    game.spawn_item(rich(), [10000.5, 72.0, 10000.5], [0.0, 0.0, 0.0], 10);
    assert!(!game.world.chunks.contains_key(&(10000 >> 4, 10000 >> 4)));
    let before = owners(&game);
    let disk = files(&game.world_root);
    assert_eq!(game.preflight_checkpoint_participants(&expected), Ok(()));
    assert_eq!(game.dropped.len(), 1);
    assert_eq!(raw(&game.dropped[0].stack), (4, 9));
    assert_unchanged(&game, &before, &disk);
}
