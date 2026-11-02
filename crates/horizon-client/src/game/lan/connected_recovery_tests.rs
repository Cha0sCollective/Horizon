//! Actual resource cases run individually by the coordinator, never nested builds.
use super::*;
use crate::game::{
    execution_role::ExecutionRole,
    integrated::transition_ownership_tests::Fixture,
    mod_packages::Packages,
    world_loading::{WorldRequest, host_identity::IdentityIntent},
};
use horizon_client::{
    save::SessionLock,
    world::counter_restore::{CounterTarget, NativeBinding},
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};
const WAIT: Duration = Duration::from_secs(120);
const POSITION: [i32; 3] = [2, 120, 2];

#[test]
#[ignore = "isolated pinned resources: three actual pre-Joined readers including partial frame close"]
fn connected_transport_closes_all_three_prejoined_attempts_including_partial_frame() {
    use std::io::Write;
    let resources =
        PathBuf::from(std::env::var_os("HORIZON_TEST_RESOURCES").expect("pinned resources"));
    let sync = sync_data(&resources).unwrap();
    let net = NetConfig {
        listen: false,
        announce: false,
        ..NetConfig::lan(0, "Pre-Joined closure".into(), 2)
    };
    let lan = Lan::start_staged(sync, net, lan::StagedProfiles::memory_pair()).unwrap();
    let mut first = lan.connect_local_staged().unwrap();
    let second = lan.connect_local_staged().unwrap();
    let third = lan.connect_local_staged().unwrap();
    // Length500 but incomplete body: native recv is inside a partial frame.
    first.write_all(&[0xf4, 0x03, 0x00]).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    assert!(lan.connect_local_staged().is_err());
    lan.close_staged().unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while !lan.staged_finished() {
        assert!(
            Instant::now() < until,
            "all pre-Joined readers must finish: {:?}",
            lan.staged_status()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let status = lan.staged_status().unwrap();
    assert_eq!(
        (
            status.attempts,
            status.terminal_count,
            status.pending_outbound
        ),
        (3, 3, 0)
    );
    assert!(status.threads_finished && status.closed);
    lan.join_staged_finished().unwrap();
    assert!(first.write_all(&[1]).is_err());
    drop((first, second, third));
}

#[test]
#[ignore = "isolated exception child only: expected staged-LAN owner SIGABRT"]
fn connected_transport_unjoined_lan_drop_is_process_fatal_child() {
    assert_eq!(
        std::env::var("HORIZON_STAGED_LAN_FATAL_CHILD").as_deref(),
        Ok("1")
    );
    let resources =
        PathBuf::from(std::env::var_os("HORIZON_TEST_RESOURCES").expect("pinned resources"));
    let sync = sync_data(&resources).unwrap();
    let net = NetConfig {
        listen: false,
        announce: false,
        ..NetConfig::lan(0, "Fatal LAN".into(), 2)
    };
    let lan = Lan::start_staged(sync, net, lan::StagedProfiles::memory_pair()).unwrap();
    let pipe = lan.connect_local_staged().unwrap();
    drop(lan);
    drop(pipe);
    panic!("fatal LAN guard returned");
}

#[test]
#[ignore = "isolated exception child only: expected actual staged-writer owner SIGABRT"]
fn connected_transport_unresolved_writer_is_process_fatal_child() {
    assert_eq!(
        std::env::var("HORIZON_STAGED_WRITER_FATAL_CHILD").as_deref(),
        Ok("1")
    );
    let fixture = Fixture::new("fatal-staged-writer");
    let resources =
        PathBuf::from(std::env::var_os("HORIZON_TEST_RESOURCES").expect("pinned resources"));
    let sync = sync_data(&resources).unwrap();
    let net = NetConfig {
        listen: false,
        announce: false,
        ..NetConfig::lan(0, "Fatal writer".into(), 2)
    };
    let lan = Lan::start_staged(sync, net, lan::StagedProfiles::memory_pair()).unwrap();
    let pipe = lan.connect_local_staged().unwrap();
    let mut client = Box::new(fresh(&fixture, ExecutionRole::Client));
    client.connect_local(pipe, "MemoryOwner").unwrap();
    loop {
        let _ = client.poll_stationary_client();
        std::thread::sleep(Duration::from_millis(1));
    }
}

pub(in crate::game) struct Scratch(PathBuf);
impl Scratch {
    pub(in crate::game) fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "horizon-connected-recovery-{}-{}-{label}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    pub(in crate::game) fn dir(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir(&path).unwrap();
        path
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        eprintln!("CONNECTED_RECOVERY_RETAINED {}", self.0.display());
    }
}

pub(in crate::game) fn fresh(fixture: &Fixture, role: ExecutionRole) -> Game<'_> {
    let template = fixture.game();
    static RES: OnceLock<Arc<crate::game::Resources>> = OnceLock::new();
    let resources = RES.get_or_init(|| template.res.clone());
    assert!(Arc::ptr_eq(resources, &template.res));
    let mut options = template.options.clone();
    options.test_terrain = true;
    options.server_thread = false;
    options.execution_role = role;
    options.player_name = Some("MemoryOwner".into());
    options.dedicated = if role.is_client() {
        None
    } else {
        Some(Vec::new())
    };
    let pair =
        PathBuf::from(std::env::var_os("HORIZON_COUNTER_PAIR_FIXTURE").expect("prebuilt pair"));
    options.mod_packages = Packages::read(&BTreeSet::new(), &[pair]).unwrap();
    let mut settings = template.settings.clone();
    settings.render_distance = 2;
    settings.simulation_distance = 2;
    let gui = crate::game::UiResources {
        atlas: horizon_client::hud::HudAtlas::fonts(&options.resources).unwrap(),
        panorama: Arc::new(horizon_client::gfx::panorama::Faces {
            size: 1,
            pixels: [0u8; 4].repeat(6),
        }),
    };
    let audio = horizon_client::audio::Audio::silent(&options.resources);
    let mut game = Game::new(
        resources,
        template.pack.clone(),
        gui,
        options,
        settings,
        audio,
        None,
    )
    .unwrap();
    game.start_mods();
    assert!(game.options.mod_packages.has_pairs());
    let _ = game.advancement_tree();
    game
}

pub(in crate::game) fn inventory_count(game: &Game<'_>) -> u32 {
    game.inventory
        .iter()
        .flatten()
        .filter(|s| s.item.as_ref() == "minecraft:stone")
        .map(|s| s.count)
        .sum()
}
pub(in crate::game) fn disconnect(game: &mut Game<'_>) {
    if let Some(remote) = game.remote.as_mut() {
        remote.io.existing_mut().session.disconnect();
    }
}
pub(in crate::game) fn inventory_tree(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, at: &Path, out: &mut BTreeMap<String, String>) {
        for entry in fs::read_dir(at).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).unwrap();
            assert!(!meta.file_type().is_symlink());
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .unwrap()
                .replace('\\', "/");
            if meta.is_dir() {
                out.insert(format!("{name}/"), String::new());
                walk(root, &path, out);
            } else {
                assert!(meta.is_file());
                out.insert(
                    name,
                    format!("{:x}", Sha256::digest(fs::read(path).unwrap())),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// Source setup owns the actual Game before admitting positive player owners.
/// Failure cannot unwind it through ordinary save/leave/destruction.
struct SourceGuard<'a> {
    game: Box<Game<'a>>,
    armed: bool,
}
impl Drop for SourceGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            use std::io::Write;
            let _ = writeln!(std::io::stderr(), "CONNECTED_SOURCE_UNRESOLVED_FATAL");
            std::process::abort();
        }
    }
}
pub(in crate::game) struct Source {
    pub(in crate::game) root: PathBuf,
    pub(in crate::game) archive: PathBuf,
    pub(in crate::game) namespace: AuthorityNamespace,
    pub(in crate::game) type_id: String,
    pub(in crate::game) target: CounterTarget,
    pub(in crate::game) rows: Vec<SessionPlayerRow>,
    pub(in crate::game) world_time: i64,
    pub(in crate::game) day_time: i64,
    pub(in crate::game) rounds: usize,
    pub(in crate::game) counter: horizon_client::world::counter_restore::CounterFrameV2,
    pub(in crate::game) slot_bytes: Vec<u8>,
}
pub(in crate::game) fn source(fixture: &Fixture, scratch: &Scratch) -> Source {
    let root = scratch.0.join("source-world");
    let archive = scratch.dir("source-archive");
    let namespace = AuthorityNamespace::new(&scratch.dir("authority")).unwrap();
    let mut game = Box::new(fresh(fixture, ExecutionRole::IntegratedServer));
    let mut request = WorldRequest::new(
        "source",
        Some("Connected recovery source"),
        Some(17),
        false,
        None,
    )
    .unwrap()
    .with_identity_intent(IdentityIntent::CreateNewIdentity)
    .with_session_authority(namespace.clone());
    request.save_root = root.clone();
    let prepared = request
        .prepare(&game.res, &game.pack, &game.options, &game.settings)
        .unwrap();
    game.finish_open_world(prepared).unwrap();
    game.level.allow_commands = Some(true);
    assert!(game.ui.rules.set_string("spawn_mobs", "false"));
    game.streaming.expect_existing_mut().center = (0, 0);
    game.streaming.expect_existing_mut().radius = 2;
    let until = Instant::now() + WAIT;
    loop {
        let _ = game.streaming.expect_existing_mut().update(&mut game.world);
        if (-2..=2).all(|z| (-2..=2).all(|x| game.world.chunks.contains_key(&(x, z))))
            && game.world.chunks.get(&(0, 0)).is_some_and(|c| c.lit)
        {
            break;
        }
        assert!(Instant::now() < until, "source residency");
        std::thread::yield_now();
    }
    let (binding, type_id) = {
        let catalog = game.options.mod_packages.catalog().unwrap();
        let (prepared, ty) = catalog
            .prepared()
            .values()
            .flat_map(|p| {
                p.content()
                    .machine_types()
                    .types()
                    .iter()
                    .map(move |ty| (p, ty.id()))
            })
            .next()
            .unwrap();
        (
            NativeBinding::from_prepared(prepared, ty, game.pack.assets.registry).unwrap(),
            ty.to_owned(),
        )
    };
    game.world
        .set(POSITION[0], POSITION[1], POSITION[2], binding.state_id())
        .unwrap();
    let _counter = game.bootstrap_host_counter(&type_id, POSITION).unwrap();
    let target = CounterTarget::new(super::super::super::Dim::Overworld.id(), POSITION).unwrap();
    let _slot = game
        .world
        .initialize_persistent_world_slot(&binding, &target)
        .unwrap()
        .unwrap();
    let owner_uuid = offline_uuid("MemoryOwner");
    // Select the existing default-attribute supported profile BEFORE admission.
    // Creative native ticks add derived reach modifiers which this primitive
    // deliberately refuses; do not clear them after any live player owns state.
    assert_eq!(game.level.mode(), 0);
    let config = HubConfig {
        default_game_mode: game.level.mode(),
        guest_commands: false,
        force_game_mode: false,
        hardcore: false,
        view_distance: 2,
        simulation_distance: 2,
        dimension: super::super::super::Dim::Overworld.id().into(),
        seed: 17,
        max_players: 2,
        dedicated: true,
        integrated: true,
    };
    game.lan_host(
        LanSettings {
            port: 0,
            guest_commands: false,
            force_game_mode: false,
        },
        NetConfig {
            listen: false,
            announce: false,
            ..NetConfig::lan(0, "Source".into(), 2)
        },
        config,
        None,
    )
    .unwrap();
    eprintln!("CONNECTED_SOURCE_PROFILE game_mode=0 owner_commands=true spawn_mobs=false");
    let saved = Box::new(game.level.clone());
    let mut guarded = SourceGuard { game, armed: true };
    let (a, b) = {
        let host = guarded.game.lan.as_mut().unwrap();
        host.owner = Some(Owner {
            uuid: owner_uuid,
            saved: Some(saved),
        });
        host.hub.singleplayer_owner = Some(owner_uuid);
        host.hub.ops.insert(owner_uuid, 4);
        (
            host.hub.lan.connect_local().unwrap(),
            host.hub.lan.connect_local().unwrap(),
        )
    };
    let mut clients = [
        Box::new(fresh(fixture, ExecutionRole::Client)),
        Box::new(fresh(fixture, ExecutionRole::Client)),
    ];
    clients[0].connect_local(a, "MemoryOwner").unwrap();
    clients[1].connect_local(b, "MemoryGuest").unwrap();
    let mut stage = 0;
    let mut discrepancy = None;
    let mut rounds = 0;
    for round in 0..512 {
        rounds = round + 1;
        if Instant::now() >= until {
            discrepancy = Some("source setup deadline".to_owned());
            break;
        }
        if let Err(error) = guarded.game.tick_server(false) {
            discrepancy = Some(error.to_string());
            break;
        }
        for client in &mut clients {
            if let Err(error) = client.poll_stationary_client() {
                discrepancy = Some(error.to_string());
            }
        }
        if discrepancy.is_some() {
            break;
        }
        let ready = guarded.game.lan.as_ref().unwrap().hub.guests.len() == 2
            && guarded
                .game
                .lan
                .as_ref()
                .unwrap()
                .hub
                .guests
                .iter()
                .all(|g| g.placed && g.loaded && g.awaiting_teleport.is_none());
        if ready && stage == 0 {
            clients[0].handle_chat_input("/give MemoryOwner minecraft:stone 3");
            stage = 1;
            eprintln!("CONNECTED_SOURCE_COMMAND round={round} /give MemoryOwner minecraft:stone 3");
        } else if stage == 1 && inventory_count(&clients[0]) == 3 {
            clients[0].handle_chat_input("/give MemoryGuest minecraft:stone 5");
            stage = 2;
            eprintln!("CONNECTED_SOURCE_COMMAND round={round} /give MemoryGuest minecraft:stone 5");
        } else if stage == 2
            && inventory_count(&clients[0]) == 3
            && inventory_count(&clients[1]) == 5
        {
            let host = guarded.game.lan.as_ref().unwrap();
            if host.hub.guests.iter().all(|g| {
                g.inventory
                    .iter()
                    .flatten()
                    .filter(|s| s.item.as_ref() == "minecraft:stone")
                    .map(|s| s.count)
                    .sum::<u32>()
                    == if g.uuid == owner_uuid { 3 } else { 5 }
            }) {
                stage = 3;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if stage != 3 && discrepancy.is_none() {
        discrepancy = Some("source command/inventory rounds exhausted".into());
    }
    let game = &mut guarded.game;
    let roster = game.observe_checkpoint_participants().unwrap();
    let rows = game.project_session_participant_rows(&roster).unwrap_or_else(|error| {
        eprintln!("CONNECTED_SOURCE_PREFLIGHT {}",serde_json::json!({
            "error":error.to_string(),"root":game.world_root,
            "native_player":game.player.is_some(),"inventory":game.inventory.iter().flatten().count(),
            "carried":game.carried.is_some(),"craft":game.craft_grid.iter().flatten().count(),
            "trips":game.server_trips.len(),"teleports":game.level_teleports.len(),
            "guests":game.lan.as_ref().map(|host|host.hub.guests.iter().map(|guest|serde_json::json!({
                "conn":guest.conn,"placed":guest.placed,"loaded":guest.loaded,"inbox":guest.inbox.len(),
                "awaiting_teleport":guest.awaiting_teleport.is_some(),"motion":guest.motion.is_some(),
                "loaded_body":guest.loaded_body.is_some(),"play_idle":host.play.get(&guest.conn).map(|play|play.checkpoint_participant_idle())
            })).collect::<Vec<_>>())
        }));
        panic!("source participant projection refused: {error}");
    });
    let clocks = (game.world.game_time, game.world.day_time);
    let counter = game
        .world
        .counter_checkpoint_frame(game.world.game_time)
        .unwrap();
    let slot_bytes = game
        .world
        .slot_checkpoint_bytes(game.res.items(), game.world.game_time)
        .unwrap();
    let receipt = game
        .checkpoint_session(&archive, 1, Instant::now() + WAIT)
        .unwrap();
    for client in &mut clients {
        disconnect(client);
    }
    let deadline = Instant::now() + WAIT;
    while game.lan.as_ref().unwrap().hub.lan.shared.online() != 0 {
        if Instant::now() >= deadline {
            panic!("source transport did not finish after receipt");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    game.retire_all_counter_trials("connected-source-confirmed-stop");
    guarded.armed = false;
    drop(guarded);
    assert_eq!(receipt.epoch, 1);
    assert!(
        discrepancy.is_none(),
        "source observation discrepancy retained and captured: {discrepancy:?}"
    );
    Source {
        root,
        archive,
        namespace,
        type_id,
        target,
        rows,
        world_time: clocks.0,
        day_time: clocks.1,
        rounds,
        counter,
        slot_bytes,
    }
}

pub(in crate::game) fn request(
    source: &Source,
    scratch: &Scratch,
    name: &str,
) -> ManagedRecoveryRequest {
    ManagedRecoveryRequest {
        owner_root: source.root.clone(),
        expected_target: source.target.clone(),
        expected_type: source.type_id.clone(),
        archive_root: source.archive.clone(),
        destination: scratch.0.join(name),
        deadline: Instant::now() + WAIT,
    }
}
pub(in crate::game) fn rows_equal(
    actual: &[SessionPlayerRow],
    expected: &[SessionPlayerRow],
) -> bool {
    actual.len() == expected.len()
        && actual.iter().all(|a| {
            expected.iter().any(|b| {
                a.profile_uuid == b.profile_uuid
                    && a.record_uuid == b.record_uuid
                    && a.integrated_owner == b.integrated_owner
                    && a.dimension == b.dimension
                    && session_player_tags_equal(&a.player, &b.player)
                    && a.advancements == b.advancements
                    && a.stats == b.stats
            })
        })
}

pub(in crate::game) fn client_matches(game: &Game<'_>, row: &SessionPlayerRow) -> bool {
    use horizon_world::nbt::Tag;
    let Some(Tag::List(position)) = row.player.get("Pos") else {
        return false;
    };
    if position.len() != 3 {
        return false;
    }
    let mut expected = [0.0; 3];
    for (out, tag) in expected.iter_mut().zip(position) {
        let Tag::Double(value) = tag else {
            return false;
        };
        *out = f64::from_bits(*value);
    }
    let Some(player) = game.player.as_ref() else {
        return false;
    };
    let Some(status) = game.remote_status() else {
        return false;
    };
    [player.position.x, player.position.y, player.position.z] == expected
        && status["loaded"] == true
        && status["joined"] == true
        && status["dimension"] == row.dimension
        && game.streaming.expect_existing().is_lit(
            (player.position.x.floor() as i32) >> 4,
            (player.position.z.floor() as i32) >> 4,
        )
        && game.remote_backlog() == 0
}
pub(in crate::game) fn source_core_matches(view: &Observation, source: &Source) -> bool {
    view.live_keys.is_ok()
        && view.world_time == source.world_time
        && view.day_time == source.day_time
        && view.hub_ticks == 0
        && view
            .counter
            .as_ref()
            .is_ok_and(|frame| *frame == source.counter)
        && view
            .slot_bytes
            .as_ref()
            .is_ok_and(|bytes| *bytes == source.slot_bytes)
        && view
            .players
            .as_ref()
            .is_ok_and(|rows| rows_equal(rows, &source.rows))
        && view.guests == 2
        && view.play_entries == 2
}

#[test]
#[ignore = "isolated pinned resources and prebuilt pair: actual two-client recovery and coherent close"]
fn connected_actual_two_clients_restore_nonzero_player_items_without_ticks_and_close() {
    let scratch = Scratch::new("positive");
    let fixture = Fixture::new("connected-positive");
    let source = source(&fixture, &scratch);
    let before = inventory_tree(&source.archive);
    let forbidden = scratch.0.join("authority/nested-continuation");
    fs::create_dir(&forbidden).unwrap();
    let refused = recover_connected(
        Box::new(fresh(&fixture, ExecutionRole::IntegratedServer)),
        &source.namespace,
        request(&source, &scratch, "must-not-be-created"),
        &forbidden,
        WAIT,
        |_, _| panic!("namespace route must refuse before offers"),
    );
    assert!(refused.is_err());
    assert!(!scratch.0.join("must-not-be-created").exists());
    assert_eq!(fs::read_dir(&forbidden).unwrap().count(), 0);
    assert_eq!(inventory_tree(&source.archive), before);
    let continuation = scratch.dir("continuation");
    let destination = scratch.0.join("recovered");
    let mut clients = [
        Box::new(fresh(&fixture, ExecutionRole::Client)),
        Box::new(fresh(&fixture, ExecutionRole::Client)),
    ];
    let mut ready = 0;
    let mut discrepancies = Vec::new();
    let end = recover_connected(
        Box::new(fresh(&fixture, ExecutionRole::IntegratedServer)),
        &source.namespace,
        request(&source, &scratch, "recovered"),
        &continuation,
        WAIT,
        |view, pipes| {
            if let Some([a, b]) = pipes {
                clients[0].connect_local(a, "MemoryOwner").unwrap();
                clients[1].connect_local(b, "MemoryGuest").unwrap();
            }
            for client in &mut clients {
                if let Err(error) = client.poll_stationary_client() {
                    discrepancies.push(error.to_string());
                }
            }
            if matches!(view.state, State::ReadyFrozen) {
                ready += 1;
                if view.world_time != source.world_time
                    || view.day_time != source.day_time
                    || view.hub_ticks != 0
                    || !view
                        .players
                        .as_ref()
                        .is_ok_and(|rows| rows_equal(rows, &source.rows))
                    || inventory_count(&clients[0]) != 3
                    || inventory_count(&clients[1]) != 5
                    || !source_core_matches(&view, &source)
                    || !clients
                        .iter()
                        .zip(["MemoryOwner", "MemoryGuest"])
                        .all(|(client, name)| {
                            source
                                .rows
                                .iter()
                                .find(|row| row.profile_uuid == offline_uuid(name))
                                .is_some_and(|row| client_matches(client, row))
                        })
                {
                    discrepancies.push(format!("ready observation: {view:?}"));
                }
                if SessionLock::acquire(&destination).is_ok() {
                    discrepancies.push("native lock released while ready".into());
                }
                if source
                    .namespace
                    .acquire_recovery(&source.root, &source.archive, &scratch.0.join("concurrent"))
                    .is_ok()
                {
                    discrepancies.push("UUID authority released while ready".into());
                }
                if ready >= 3 {
                    return Command::Close { attempt: 1 };
                }
            }
            if matches!(
                view.state,
                State::Refusing(_) | State::CleanupBlocked(_) | State::RetainedFailure { .. }
            ) {
                panic!("unexpected retained/refused positive: {view:?}");
            }
            Command::Wait
        },
    )
    .unwrap();
    for client in &mut clients {
        disconnect(client);
    }
    let End::Closed(receipt) = end else {
        panic!("positive refused")
    };
    assert_eq!(receipt.epoch, 1);
    assert!(ready >= 3);
    assert!(discrepancies.is_empty(), "{discrepancies:?}");
    assert_eq!(inventory_tree(&source.archive), before);
    drop(SessionLock::acquire(&destination).unwrap());
    drop(
        source
            .namespace
            .acquire_recovery(
                &source.root,
                &source.archive,
                &scratch.0.join("after-close"),
            )
            .unwrap(),
    );
    eprintln!(
        "CONNECTED_RECOVERY_RESULT {}",
        serde_json::json!({"kind":"positive","source_rounds":source.rounds,
        "source":source.root,"archive":source.archive,"destination":destination,"continuation":continuation,
        "ready_observations":ready,"player_counts":[3,5],"recovery_ticks":0,"receipt_epoch":receipt.epoch})
    );
}

#[derive(Clone, Copy, Debug)]
enum Negative {
    MissingProfile,
    JoinedBeforeInstall,
    WrongProfile,
    DuplicateProfile,
    PreJoinDisconnect,
    WithheldLoaded,
    WrongTeleport,
    Prohibited,
    Overflow,
}
fn negative(mode: Negative) {
    use horizon_protocol::packets::{
        Empty,
        play::{self, Serverbound as S},
    };
    let scratch = Scratch::new(&format!("{mode:?}"));
    let fixture = Fixture::new("connected-negative");
    let source = source(&fixture, &scratch);
    let before = inventory_tree(&source.archive);
    let continuation = scratch.dir("continuation");
    let destination = scratch.0.join("recovered");
    let mut clients = [
        Box::new(fresh(&fixture, ExecutionRole::Client)),
        Box::new(fresh(&fixture, ExecutionRole::Client)),
    ];
    let mut injected = false;
    let mut loading = 0;
    let mut was_ready = false;
    let mut missing_pipe = None;
    let end = recover_connected(
        Box::new(fresh(&fixture, ExecutionRole::IntegratedServer)),
        &source.namespace,
        request(&source, &scratch, "recovered"),
        &continuation,
        if matches!(mode, Negative::MissingProfile) {
            Duration::from_secs(2)
        } else {
            WAIT
        },
        |view, pipes| {
            if let Some([a, b]) = pipes {
                if matches!(mode, Negative::MissingProfile) {
                    clients[0].connect_local(a, "MemoryOwner").unwrap();
                    missing_pipe = Some(b);
                    injected = true;
                } else if matches!(mode, Negative::PreJoinDisconnect) {
                    a.shutdown();
                    drop(a);
                    drop(b);
                    injected = true;
                } else {
                    clients[0].connect_local(a, "MemoryOwner").unwrap();
                    let name = match mode {
                        Negative::WrongProfile => "WrongGuest",
                        Negative::DuplicateProfile => "MemoryOwner",
                        _ => "MemoryGuest",
                    };
                    clients[1].connect_local(b, name).unwrap();
                    if matches!(mode, Negative::WrongProfile | Negative::DuplicateProfile) {
                        injected = true;
                    }
                }
            }
            // Polling the ordinary packet processor still emits real teleport and
            // batch acknowledgements. Only PlayerLoaded's stationary helper is withheld.
            for client in &mut clients {
                let _ = client.poll_remote();
            }
            if matches!(mode, Negative::JoinedBeforeInstall)
                && matches!(view.state, State::Admitted)
                && !injected
            {
                assert_eq!(view.guests, 2);
                disconnect(&mut clients[1]);
                injected = true;
            }
            if matches!(view.state, State::Loading) {
                loading += 1;
                if !injected {
                    match mode {
                        Negative::WithheldLoaded if loading >= 20 => {
                            disconnect(&mut clients[1]);
                            injected = true;
                        }
                        Negative::WrongTeleport => {
                            clients[0].send(S::AcceptTeleportation(play::AcceptTeleportation {
                                id: i32::MAX,
                                x: 0.0,
                                y: 0.0,
                                z: 0.0,
                                yaw: 0.0,
                                pitch: 0.0,
                            }));
                            injected = true;
                        }
                        Negative::Prohibited => {
                            clients[0].send(S::ClientTickEnd(Empty));
                            injected = true;
                        }
                        Negative::Overflow => {
                            for _ in 0..80 {
                                clients[0].send(S::ClientTickEnd(Empty));
                            }
                            std::thread::sleep(Duration::from_millis(100));
                            injected = true;
                        }
                        _ => {}
                    }
                }
            }
            if matches!(view.state, State::ReadyFrozen) {
                was_ready = true;
                return Command::Close { attempt: 1 };
            }
            if matches!(view.state, State::CleanupBlocked(_)) {
                panic!("pre-ready cleanup retained: {view:?}");
            }
            Command::Wait
        },
    )
    .unwrap();
    for client in &mut clients {
        disconnect(client);
    }
    drop(missing_pipe);
    let End::Refused(reason) = end else {
        panic!("negative unexpectedly closed")
    };
    if matches!(mode, Negative::MissingProfile) {
        assert_eq!(reason, "startup deadline");
    }
    assert!(
        injected && !was_ready,
        "negative must really inject and never become ready"
    );
    assert_eq!(inventory_tree(&source.archive), before);
    assert_eq!(fs::read_dir(&continuation).unwrap().count(), 0);
    drop(SessionLock::acquire(&destination).unwrap());
    drop(
        source
            .namespace
            .acquire_recovery(&source.root, &source.archive, &scratch.0.join("released"))
            .unwrap(),
    );
    eprintln!(
        "CONNECTED_RECOVERY_RESULT {}",
        serde_json::json!({"kind":"pre-ready-refusal","mode":format!("{mode:?}"),
        "reason":reason,"source_rounds":source.rounds,"source":source.root,"archive":source.archive,
        "destination":destination,"continuation":continuation,"loading_observations":loading,"ready":false})
    );
}
macro_rules! negative_test {
    ($name:ident,$mode:ident)=>{
        #[test]
        #[ignore="isolated pinned resources + prebuilt pair: actual connection refusal and complete cleanup"]
        fn $name(){negative(Negative::$mode);}
    }
}
negative_test!(
    connected_actual_missing_profile_refuses_on_deadline,
    MissingProfile
);
negative_test!(
    connected_actual_joined_before_install_departure_refuses_and_releases,
    JoinedBeforeInstall
);
negative_test!(
    connected_actual_wrong_profile_refuses_and_releases,
    WrongProfile
);
negative_test!(
    connected_actual_duplicate_profile_refuses_and_releases,
    DuplicateProfile
);
negative_test!(
    connected_actual_prejoin_disconnect_refuses_and_releases,
    PreJoinDisconnect
);
negative_test!(
    connected_actual_withheld_loaded_then_disconnect_refuses_and_releases,
    WithheldLoaded
);
negative_test!(
    connected_actual_wrong_teleport_refuses_and_releases,
    WrongTeleport
);
negative_test!(
    connected_actual_prohibited_packet_refuses_and_releases,
    Prohibited
);
negative_test!(
    connected_actual_inbound_overflow_refuses_and_releases,
    Overflow
);

#[test]
#[ignore = "isolated exception child only: expected real unresolved-owner SIGABRT"]
fn connected_actual_ready_unwind_is_process_fatal_child() {
    assert_eq!(
        std::env::var("HORIZON_CONNECTED_FATAL_CHILD").as_deref(),
        Ok("1")
    );
    let scratch = Scratch::new("fatal");
    let fixture = Fixture::new("connected-fatal");
    let source = source(&fixture, &scratch);
    let continuation = scratch.dir("continuation");
    let mut clients = [
        Box::new(fresh(&fixture, ExecutionRole::Client)),
        Box::new(fresh(&fixture, ExecutionRole::Client)),
    ];
    let _ = recover_connected(
        Box::new(fresh(&fixture, ExecutionRole::IntegratedServer)),
        &source.namespace,
        request(&source, &scratch, "recovered"),
        &continuation,
        WAIT,
        |view, pipes| {
            if let Some([a, b]) = pipes {
                clients[0].connect_local(a, "MemoryOwner").unwrap();
                clients[1].connect_local(b, "MemoryGuest").unwrap();
            }
            for client in &mut clients {
                client.poll_stationary_client().unwrap();
            }
            if matches!(view.state, State::ReadyFrozen) {
                eprintln!(
                    "CONNECTED_RECOVERY_EXCEPTION {}",
                    serde_json::json!({"kind":"fatal-unwind","root":scratch.0,
                    "archive":source.archive,"world_time":view.world_time,"players":view.players.as_ref().map(Vec::len).ok()})
                );
                panic!("intentional connected ReadyFrozen unwind");
            }
            Command::Wait
        },
    );
    panic!("fatal child returned");
}

#[test]
#[ignore = "isolated exception child only: external termination after proven retained late departure"]
fn connected_actual_ready_departure_retains_owner_until_external_termination_child() {
    assert_eq!(
        std::env::var("HORIZON_CONNECTED_DEPARTURE_CHILD").as_deref(),
        Ok("1")
    );
    let scratch = Scratch::new("late-departure");
    let fixture = Fixture::new("connected-departure");
    let source = source(&fixture, &scratch);
    let continuation = scratch.dir("continuation");
    let destination = scratch.0.join("recovered");
    let mut clients = [
        Box::new(fresh(&fixture, ExecutionRole::Client)),
        Box::new(fresh(&fixture, ExecutionRole::Client)),
    ];
    let mut departed = false;
    let mut retained = 0;
    let mut ready_keys = None;
    let _ = recover_connected(
        Box::new(fresh(&fixture, ExecutionRole::IntegratedServer)),
        &source.namespace,
        request(&source, &scratch, "recovered"),
        &continuation,
        WAIT,
        |view, pipes| {
            if let Some([a, b]) = pipes {
                clients[0].connect_local(a, "MemoryOwner").unwrap();
                clients[1].connect_local(b, "MemoryGuest").unwrap();
            }
            if !departed {
                for client in &mut clients {
                    client.poll_stationary_client().unwrap();
                }
                if matches!(view.state, State::ReadyFrozen) {
                    assert!(source_core_matches(&view, &source));
                    ready_keys = Some((view.counter_key.clone(), view.slot_key.clone()));
                    disconnect(&mut clients[1]);
                    departed = true;
                }
            }
            if departed && matches!(view.state, State::CleanupBlocked(_)) {
                assert!(
                    source_core_matches(&view, &source),
                    "retained actual owner changed: {view:?}"
                );
                assert_eq!(
                    Some((view.counter_key.clone(), view.slot_key.clone())),
                    ready_keys
                );
                if retained > 0 {
                    assert!(
                        view.last_refusal.is_some(),
                        "invalid close must be observably refused"
                    );
                }
                assert!(SessionLock::acquire(&destination).is_err());
                assert!(
                    source
                        .namespace
                        .acquire_recovery(
                            &source.root,
                            &source.archive,
                            &scratch.0.join("blocked-copy")
                        )
                        .is_err()
                );
                assert_eq!(fs::read_dir(&continuation).unwrap().count(), 0);
                retained += 1;
                if retained == 10 {
                    eprintln!(
                        "CONNECTED_RECOVERY_EXCEPTION {}",
                        serde_json::json!({"kind":"late-departure-retained","root":scratch.0,
                        "archive":source.archive,"destination":destination,"continuation":continuation,
                        "world_time":view.world_time,"source_world_time":source.world_time,"retained_observations":retained,
                        "retained_players":view.guests,"retained_play_entries":view.play_entries,
                        "rows_equal":view.players.as_ref().is_ok_and(|rows|rows_equal(rows,&source.rows)),
                        "core_unchanged":source_core_matches(&view,&source),"close_refused":view.last_refusal.is_some()})
                    );
                }
                // Deliberate invalid close cannot serialize a disconnected roster.
                return Command::Close { attempt: retained };
            }
            Command::Wait
        },
    );
    panic!("late departure child unexpectedly returned");
}

fn retry_case(fault: crate::game::levels::session_checkpoint::CheckpointTestFault) {
    use crate::game::levels::session_checkpoint::CheckpointTestFault;
    let scratch = Scratch::new("retry");
    let fixture = Fixture::new("connected-retry");
    let source = source(&fixture, &scratch);
    let before = inventory_tree(&source.archive);
    let continuation = scratch.dir("continuation");
    let destination = scratch.0.join("recovered");
    let mut clients = [
        Box::new(fresh(&fixture, ExecutionRole::Client)),
        Box::new(fresh(&fixture, ExecutionRole::Client)),
    ];
    let mut keys = None;
    let mut failures = 0;
    let mut close_sent = false;
    let end = recover_connected(
        Box::new(fresh(&fixture, ExecutionRole::IntegratedServer)),
        &source.namespace,
        request(&source, &scratch, "recovered"),
        &continuation,
        WAIT,
        |view, pipes| {
            if let Some([a, b]) = pipes {
                clients[0].connect_local(a, "MemoryOwner").unwrap();
                clients[1].connect_local(b, "MemoryGuest").unwrap();
            }
            for client in &mut clients {
                let _ = client.poll_stationary_client();
            }
            if matches!(
                view.state,
                State::ReadyFrozen | State::RetainedFailure { .. }
            ) {
                assert!(source_core_matches(&view, &source));
                assert!(SessionLock::acquire(&destination).is_err());
                assert!(
                    source
                        .namespace
                        .acquire_recovery(&source.root, &source.archive, &scratch.0.join("blocked"))
                        .is_err()
                );
                let current = (view.counter_key.clone(), view.slot_key.clone());
                if let Some(keys) = &keys {
                    assert_eq!(&current, keys);
                } else {
                    keys = Some(current);
                }
            }
            if matches!(view.state, State::ReadyFrozen) && !close_sent {
                CLOSE_FAULT.with(|value| value.set(Some(fault)));
                close_sent = true;
                return Command::Close { attempt: 1 };
            }
            if let State::RetainedFailure {
                attempt,
                publication,
                ..
            } = view.state
            {
                assert_eq!(attempt, 1);
                assert_eq!(
                    publication,
                    match fault {
                        CheckpointTestFault::Copied => Publication::NotPublished,
                        CheckpointTestFault::Promoted => Publication::Uncertain,
                    }
                );
                failures += 1;
                if failures == 1 {
                    return Command::Close { attempt: 1 };
                }
                assert!(view.last_refusal.is_some());
                return Command::RetryClose {
                    attempt: 2,
                    failed_attempt: 1,
                };
            }
            if matches!(view.state, State::CleanupBlocked(_) | State::Refusing(_)) {
                panic!("retry refused: {view:?}");
            }
            Command::Wait
        },
    )
    .unwrap();
    for client in &mut clients {
        disconnect(client);
    }
    let End::Closed(receipt) = end else {
        panic!("retry failed")
    };
    assert_eq!(receipt.epoch, 2);
    assert!(failures >= 2);
    assert!(continuation.join("epoch-00000000000000000002").is_dir());
    let first = match fault {
        CheckpointTestFault::Copied => ".pending-00000000000000000001",
        CheckpointTestFault::Promoted => "epoch-00000000000000000001",
    };
    assert!(continuation.join(first).is_dir());
    assert_eq!(inventory_tree(&source.archive), before);
    drop(SessionLock::acquire(&destination).unwrap());
    drop(
        source
            .namespace
            .acquire_recovery(&source.root, &source.archive, &scratch.0.join("released"))
            .unwrap(),
    );
    eprintln!(
        "CONNECTED_RECOVERY_RESULT {}",
        serde_json::json!({"kind":"retained-publication-retry","fault":format!("{fault:?}"),
        "source":source.root,"archive":source.archive,"continuation":continuation,"destination":destination,
        "failed_attempt":1,"successful_attempt":2,"receipt_epoch":2,"failure_observations":failures})
    );
}
#[test]
#[ignore = "isolated pinned resources + prebuilt pair: connected NotPublished retains same actual owners until explicit retry"]
fn connected_actual_not_published_retains_and_retries() {
    retry_case(crate::game::levels::session_checkpoint::CheckpointTestFault::Copied);
}
#[test]
#[ignore = "isolated pinned resources + prebuilt pair: connected Uncertain retains same actual owners until explicit retry"]
fn connected_actual_uncertain_retains_and_retries() {
    retry_case(crate::game::levels::session_checkpoint::CheckpointTestFault::Promoted);
}
