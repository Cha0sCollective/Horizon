//! Fixed original Game/worker fixtures. The caller supplies pinned resources
//! and isolated cwd/home/temp; no test borrows player installations or saves.
use super::*;
use horizon_client::{
    items::ItemStack,
    net::{Event, RetainedClose, RetainedSession, RetainedSettings, RetainedTarget, SendAdmission},
    save::{Level, read_dat_checked, write_dat},
};
use horizon_protocol::{
    crypto::{offline_uuid, uuid_string},
    packets::{
        Empty,
        play::{self, Clientbound as C, Serverbound as S},
        recipes::RecipeBookChangeSettings,
    },
};
use horizon_world::nbt::Tag;
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

const OWNER_STATS_JSON: &str = "{\"stats\":{\"minecraft:custom\":{\"minecraft:jump\":7}}}";
const OWNER_ADVANCEMENTS_JSON: &str = "{\"DataVersion\":5023,\"test:owner_keep\":{\"criteria\":{\"test:keep\":\"2026-01-01 00:00:00 +0000\"},\"done\":false}}";

#[derive(Default)]
pub(super) struct Gate {
    entered: AtomicBool,
    release: AtomicBool,
    registered: AtomicBool,
}
impl Gate {
    fn held(&self) -> bool {
        self.entered.store(true, Ordering::SeqCst);
        !self.release.load(Ordering::SeqCst)
    }
    fn release(&self) {
        self.release.store(true, Ordering::SeqCst);
    }
}

#[derive(Clone, Default)]
pub(in crate::game) struct Startup {
    pub(super) fail_after_streaming: bool,
    pub(in crate::game) stop_on_untouched_join: bool,
    staging: Option<PathBuf>,
    profiles: Vec<u128>,
    fail_native_write: bool,
    write_fault_installed: bool,
    pub(super) refuse_live_consume: bool,
    first_poll: Option<Arc<Gate>>,
    frozen: Option<Arc<Gate>>,
    final_reply: Option<Arc<Gate>>,
    integrated: bool,
    owner_items: u32,
    owner_bad_stats: bool,
    pristine_owner: bool,
}
impl Startup {
    pub(super) fn install_owner_inputs(
        &self,
        game: &mut Game<'_>,
    ) -> std::result::Result<(), String> {
        let Some((_, record)) = game.retained_server.as_ref().unwrap().integrated_owner() else {
            return Ok(());
        };
        game.level.inventory = vec![(0, "minecraft:cobblestone".into(), self.owner_items, 0)];
        if self.owner_items == 0 {
            game.level.inventory.clear();
        }
        game.level.ender_items = vec![None; 27];
        game.level.ender_items[0] = Some(ItemStack::new("minecraft:stone", 2));
        game.level.position = (!self.pristine_owner).then_some([8.5, 80.0, 8.5]);
        game.level.rotation = [21.0, 0.0];
        game.level.xp_level = 7;
        game.level.creative = true;
        game.level.game_mode = 1;
        for (family, json) in [
            (
                "stats",
                if self.owner_bad_stats {
                    "not valid JSON"
                } else {
                    OWNER_STATS_JSON
                },
            ),
            ("advancements", OWNER_ADVANCEMENTS_JSON),
        ] {
            let path = crate::game::stats::player_json(&game.world_root, family, record);
            fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
            fs::write(path, json).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    pub(in crate::game) fn hold_first_poll(&self) -> bool {
        self.first_poll.as_ref().is_some_and(|gate| gate.held())
    }
    pub(super) fn hold_frozen(&self) -> bool {
        self.frozen.as_ref().is_some_and(|gate| gate.held())
    }
    pub(super) fn hold_parent_final(&self, queued: usize) -> bool {
        self.final_reply.as_ref().is_some_and(|gate| {
            if queued > 0 {
                gate.registered.store(true, Ordering::SeqCst);
            }
            gate.held()
        })
    }
    pub(super) fn install_staged_inputs(
        &self,
        world: &std::path::Path,
    ) -> std::result::Result<(), String> {
        let Some(staging) = &self.staging else {
            return Ok(());
        };
        let staging = fs::canonicalize(staging).map_err(|e| e.to_string())?;
        let world = fs::canonicalize(world).map_err(|e| e.to_string())?;
        if staging.starts_with(&world) || world.starts_with(&staging) {
            return Err("native fixture inputs must be staged outside the acquired world".into());
        }
        for uuid in &self.profiles {
            for (family, extension) in
                [("data", "dat"), ("stats", "json"), ("advancements", "json")]
            {
                let relative = PathBuf::from("players")
                    .join(family)
                    .join(format!("{}.{extension}", uuid_string(*uuid)));
                let source = staging.join(&relative);
                if !source.exists() {
                    continue;
                }
                let target = world.join(relative);
                fs::create_dir_all(target.parent().unwrap()).map_err(|e| e.to_string())?;
                fs::copy(&source, &target).map_err(|e| format!("{}: {e}", source.display()))?;
            }
        }
        Ok(())
    }
    pub(super) fn inject_native_write_failure(
        &mut self,
        world: &std::path::Path,
    ) -> std::result::Result<(), String> {
        if !self.fail_native_write || self.write_fault_installed {
            return Ok(());
        }
        let uuid = *self
            .profiles
            .first()
            .ok_or("native IO fixture profile missing")?;
        let file = crate::game::stats::player_json(
            world,
            "stats",
            horizon_client::lan::playerdata::uuid_ints(uuid),
        );
        if file.exists() {
            fs::rename(&file, file.with_extension("json.before-native-close"))
                .map_err(|e| e.to_string())?;
        }
        fs::create_dir_all(&file).map_err(|e| e.to_string())?;
        self.write_fault_installed = true;
        Ok(())
    }
}

struct Fixture {
    base: PathBuf,
    root: PathBuf,
    supervisor: Supervisor,
    clients: Vec<RetainedSession>,
    gates: Vec<Arc<Gate>>,
}
impl Fixture {
    fn inputs(
        label: &str,
        mut test: Startup,
        profiles: &[(&str, u32)],
        corrupt_stats: bool,
    ) -> (PathBuf, Input) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "original-server-{}-{}-{label}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        assert!(!base.exists(), "fresh isolated scratch required");
        fs::create_dir(&base).unwrap();
        let staging = base.join("staging");
        fs::create_dir(&staging).unwrap();
        for (name, count) in profiles {
            let uuid = offline_uuid(name);
            test.profiles.push(uuid);
            let path = horizon_client::lan::playerdata::path(&staging, uuid);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let level = fixture_player(uuid, *count);
            crate::game::lan::validate_session_player_tag(&level.player_tag())
                .expect("authored native fixture must validate before launch");
            write_dat(&path, level.player_tag()).unwrap();
            let stats = crate::game::stats::player_json(
                &staging,
                "stats",
                horizon_client::lan::playerdata::uuid_ints(uuid),
            );
            fs::create_dir_all(stats.parent().unwrap()).unwrap();
            fs::write(
                stats,
                if corrupt_stats {
                    "not valid JSON"
                } else {
                    "{\"stats\":{\"minecraft:custom\":{\"minecraft:jump\":7}}}"
                },
            )
            .unwrap();
        }
        test.staging = Some(staging);
        let root = base.join("application");
        let resources = PathBuf::from(
            std::env::var_os("HORIZON_TEST_RESOURCES")
                .expect("complete pinned resources are required"),
        );
        let launch = Launch {
            root: root.clone(),
            args: vec![
                "--resources".into(),
                resources.to_string_lossy().into_owned(),
                "--in-frame".into(),
                "--test-terrain".into(),
            ],
        };
        let mut options = Options::read_from(launch.ordinary_args())
            .unwrap()
            .unwrap()
            .options;
        launch.configure(&mut options);
        let mut settings = Settings {
            render_distance: 2,
            simulation_distance: 2,
            ..Default::default()
        };
        settings.horizon.lod_distance = 0;
        settings.controller.enabled = false;
        let integrated = test.integrated;
        let input = Input {
            options,
            settings,
            launch,
            owner: None,
            test,
        };
        let input = if integrated {
            input
                .with_integrated_owner("IntegratedOwner")
                .unwrap_or_else(|(_, reason)| panic!("{reason}"))
        } else {
            input
        };
        (base, input)
    }
    fn start(label: &str, test: Startup, profiles: &[(&str, u32)], corrupt_stats: bool) -> Self {
        let (base, input) = Self::inputs(label, test, profiles, corrupt_stats);
        let root = input.launch.root.clone();
        fs::create_dir(&root).unwrap();
        let gates = [
            input.test.first_poll.clone(),
            input.test.frozen.clone(),
            input.test.final_reply.clone(),
        ]
        .into_iter()
        .flatten()
        .collect();
        let supervisor = Supervisor::prepare(input)
            .unwrap_or_else(|(_, e)| panic!("original worker spawn: {e}"));
        Self {
            base,
            root,
            supervisor,
            clients: Vec::new(),
            gates,
        }
    }
    fn until(&mut self, mut check: impl FnMut(&mut Self) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(35);
        while Instant::now() < deadline {
            if check(self) {
                return true;
            }
            if self.supervisor.finished() {
                return check(self);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        check(self)
    }
    fn ready(&mut self) {
        assert!(
            self.until(|f| f.supervisor.snapshot().ready),
            "actual server not ready: {:?}",
            self.supervisor.snapshot()
        );
        let view = self.supervisor.snapshot();
        assert_eq!(
            view.port == 0,
            view.owner_record.is_some(),
            "only the integrated profile is Memory-only"
        );
    }
    fn connect(&mut self, tcp: bool, name: &str) {
        let target = if tcp {
            RetainedTarget::Address(format!("127.0.0.1:{}", self.supervisor.snapshot().port))
        } else {
            let (reply, receive) = mpsc::channel();
            self.supervisor
                .commands
                .send(Command::Offer(reply))
                .unwrap();
            RetainedTarget::Memory(
                receive
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap(),
            )
        };
        let mut client = RetainedSession::new_offline(
            target,
            RetainedSettings::offline(name.into(), 2, Arc::default()),
        )
        .unwrap_or_else(|r| panic!("{}", r.reason));
        client.start().expect("actual original client actors");
        self.clients.push(client);
    }
    fn close_clients(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        for client in &mut self.clients {
            client.begin_close(deadline);
        }
        while !self
            .clients
            .iter_mut()
            .all(|client| client.poll_close() == RetainedClose::Quiescent)
        {
            if Instant::now() >= deadline {
                eprintln!("fatal: unresolved original server fixture clients");
                std::process::abort();
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        for client in self.clients.drain(..) {
            if client.try_closed().is_err() {
                std::process::abort();
            }
        }
    }
    fn finish(&mut self) -> std::result::Result<(), String> {
        self.supervisor.commands.send(Command::Close).ok();
        assert!(
            self.until(|f| f.supervisor.finished()),
            "original worker not finished: {:?}",
            self.supervisor.snapshot()
        );
        let result = self.supervisor.join().expect("finished-only original join");
        self.close_clients();
        result
    }
    fn player(&self, name: &str) -> Level {
        let uuid = offline_uuid(name);
        let tag = read_dat_checked(&horizon_client::lan::playerdata::path(
            &self.root.join("world"),
            uuid,
        ))
        .unwrap()
        .expect("actual native player file");
        let mut level = Level::default();
        level.read_player(&tag);
        level.player_extra = Some(tag);
        level
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for gate in &self.gates {
            gate.release();
        }
        if self.supervisor.thread.is_some() {
            // A test assertion never detaches the original Game. Repair only
            // this fixture's known IO/read faults, then exact Retry and close.
            let mut repaired = false;
            let mut records = [
                "ServerMemory",
                "ServerTcp",
                "PartialPlayer",
                "UntouchedPlayer",
                "WriterPlayer",
            ]
            .map(|name| horizon_client::lan::playerdata::uuid_ints(offline_uuid(name)))
            .to_vec();
            records.extend(self.supervisor.snapshot().owner_record);
            for record in records {
                let path =
                    crate::game::stats::player_json(&self.root.join("world"), "stats", record);
                if path.is_dir() {
                    repaired |=
                        fs::rename(&path, path.with_extension("json.blocker-preserved")).is_ok();
                }
                if path.parent().is_some_and(std::path::Path::is_dir) && !path.exists() {
                    repaired |= fs::write(&path, "{}").is_ok();
                }
                if path.is_file()
                    && fs::read_to_string(&path).ok().is_some_and(|text| {
                        serde_json::from_str::<serde_json::Value>(&text).is_err()
                    })
                {
                    repaired |= fs::write(&path, "{}").is_ok();
                }
            }
            let _ = self.supervisor.commands.send(Command::Close);
            let deadline = Instant::now() + Duration::from_secs(25);
            let mut retried = false;
            while !self.supervisor.finished() {
                let view = self.supervisor.snapshot();
                if repaired && !retried && view.blocked.is_some() {
                    retried = true;
                    let _ = self.supervisor.commands.send(Command::Retry(view.attempt));
                }
                if Instant::now() >= deadline {
                    eprintln!("fatal: unresolved native fixture: {view:?}");
                    std::process::abort();
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            self.supervisor.join();
        }
        self.close_clients();
    }
}
fn fixture_player(uuid: u128, count: u32) -> Level {
    let stack = ItemStack::new("minecraft:cobblestone", count);
    Level {
        player_uuid: Some(horizon_client::lan::playerdata::uuid_ints(uuid)),
        position: Some([8.5, 80.0, 8.5]),
        creative: true,
        game_mode: 1,
        health: 20.0,
        food: 20,
        saturation: 5.0,
        inventory: vec![(0, stack.item.to_string(), count, 0)],
        player_extra: Some(Tag::compound([
            ("Fire", Tag::Short(-20)),
            ("Air", Tag::Short(300)),
        ])),
        ..Default::default()
    }
}

/// Drives the actual production parent and actual retained native endpoint.
/// The fixed test startup input only installs authored player files/IO faults.
struct Application {
    root: PathBuf,
    endpoint: PathBuf,
    thread: Option<JoinHandle<std::result::Result<(), String>>>,
    control: Option<BufReader<horizon_client::control::Stream>>,
    clients: Vec<RetainedSession>,
    gates: Vec<Arc<Gate>>,
}
impl Application {
    fn start(label: &str, test: Startup, profiles: &[(&str, u32)], bad_endpoint: bool) -> Self {
        let (_base, input) = Fixture::inputs(label, test, profiles, false);
        let Input {
            mut options,
            launch,
            test,
            ..
        } = input;
        let root = launch.root.clone();
        if bad_endpoint {
            options.control = Some(root.join(format!("{}.sock", "x".repeat(180))));
        }
        let endpoint = options.control.clone().unwrap();
        let gates = [
            test.first_poll.clone(),
            test.frozen.clone(),
            test.final_reply.clone(),
        ]
        .into_iter()
        .flatten()
        .collect();
        let thread = std::thread::Builder::new()
            .name("Original parent fixture".into())
            .spawn(move || run_configured(options, launch, test).map_err(|e| e.to_string()))
            .unwrap();
        Self {
            root,
            endpoint,
            thread: Some(thread),
            control: None,
            clients: Vec::new(),
            gates,
        }
    }
    fn finished(&self) -> bool {
        self.thread.as_ref().is_some_and(JoinHandle::is_finished)
    }
    fn request(&mut self, line: &str) -> Option<serde_json::Value> {
        if self.control.is_none() {
            let stream = horizon_client::control::connect(&self.endpoint).ok()?;
            stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
            self.control = Some(BufReader::new(stream));
        }
        let reader = self.control.as_mut().unwrap();
        if writeln!(reader.get_mut(), "{line}").is_err() {
            self.control = None;
            return None;
        }
        let mut reply = String::new();
        match reader.read_line(&mut reply) {
            Ok(n) if n > 0 => serde_json::from_str(&reply).ok(),
            _ => {
                self.control = None;
                None
            }
        }
    }
    fn until(&mut self, mut check: impl FnMut(&mut Self) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(40);
        while Instant::now() < deadline {
            if check(self) {
                return true;
            }
            if self.finished() {
                return check(self);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        check(self)
    }
    fn finish(&mut self) -> std::result::Result<(), String> {
        assert!(
            self.until(|app| app.finished()),
            "original parent/worker/endpoint did not complete"
        );
        let result = self
            .thread
            .take()
            .unwrap()
            .join()
            .expect("checked original parent fixture join");
        self.control.take();
        let deadline = Instant::now() + Duration::from_secs(15);
        for client in &mut self.clients {
            client.begin_close(deadline);
        }
        while !self
            .clients
            .iter_mut()
            .all(|c| c.poll_close() == RetainedClose::Quiescent)
        {
            if Instant::now() >= deadline {
                std::process::abort();
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        for client in self.clients.drain(..) {
            if client.try_closed().is_err() {
                std::process::abort();
            }
        }
        result
    }
}
impl Drop for Application {
    fn drop(&mut self) {
        for gate in &self.gates {
            gate.release();
        }
        if self.thread.is_some() {
            let path = crate::game::stats::player_json(
                &self.root.join("world"),
                "stats",
                horizon_client::lan::playerdata::uuid_ints(offline_uuid("WriterPlayer")),
            );
            let repaired = path.is_dir()
                && fs::rename(&path, path.with_extension("json.blocker-preserved")).is_ok();
            let _ = self.request("close");
            let deadline = Instant::now() + Duration::from_secs(25);
            let mut retried = false;
            while !self.finished() {
                if repaired
                    && !retried
                    && let Some(status) = self.request("status")
                    && status["state"]["blocked"].is_string()
                    && let Some(attempt) = status["state"]["attempt"].as_u64()
                {
                    retried = true;
                    let _ = self.request(&format!("retry {attempt}"));
                }
                if Instant::now() >= deadline {
                    eprintln!("fatal: unresolved actual parent/control fixture");
                    std::process::abort();
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = self.finish();
        }
    }
}
fn count(level: &Level) -> u32 {
    level.inventory.iter().map(|(_, _, count, _)| *count).sum()
}
fn integrated_fixture(label: &str, mut test: Startup) -> Fixture {
    test.integrated = true;
    test.owner_items = 3;
    let mut fixture = Fixture::start(label, test, &[], false);
    fixture.ready();
    assert!(fixture.supervisor.snapshot().owner_saved);
    fixture
}
fn drive_integrated_owner(fixture: &mut Fixture) -> f64 {
    fixture.connect(false, "IntegratedOwner");
    let (mut login, mut terrain, mut moved) = (false, false, false);
    let mut expected_x = None;
    assert!(
        fixture.until(|f| {
            let client = &mut f.clients[0];
            for event in client.poll_events(128) {
                match event {
                    Event::Packet(packet) => match *packet {
                        C::Login(_) => login = true,
                        C::PlayerPosition(p) => {
                            // TestTerrain's bounded surface is near y=68; an
                            // encoded empty Level's [0,0,0] is never valid feet.
                            assert!(
                                (40.0..100.0).contains(&p.position[1]),
                                "invalid native TestTerrain spawn: {:?}",
                                p.position
                            );
                            send(
                                client,
                                S::AcceptTeleportation(play::AcceptTeleportation {
                                    id: p.teleport,
                                    x: p.position[0],
                                    y: p.position[1],
                                    z: p.position[2],
                                    yaw: p.yaw,
                                    pitch: p.pitch,
                                }),
                            );
                            send(client, S::PlayerLoaded(Empty {}));
                            if !moved {
                                expected_x = Some(p.position[0] + 0.125);
                                send(
                                    client,
                                    S::RecipeBookChangeSettings(RecipeBookChangeSettings {
                                        book_type: 0,
                                        open: true,
                                        filtering: true,
                                    }),
                                );
                                send(
                                    client,
                                    S::MovePlayerPosRot(play::MovePlayerPosRot {
                                        x: p.position[0] + 0.125,
                                        y: p.position[1],
                                        z: p.position[2],
                                        yaw: 32.0,
                                        pitch: 0.0,
                                        flags: 0,
                                    }),
                                );
                                moved = true;
                            }
                        }
                        C::LevelChunkWithLight(p) => {
                            assert!(login && !p.chunk.buffer.is_empty());
                            terrain = true;
                        }
                        C::ChunkBatchFinished(_) => send(
                            client,
                            S::ChunkBatchReceived(play::ChunkBatchReceived {
                                chunks_per_tick: 9.0,
                            }),
                        ),
                        C::KeepAlive(p) => send(client, S::KeepAlive(p)),
                        _ => {}
                    },
                    Event::DecodeError(error) => panic!("native integrated decode: {error}"),
                    Event::Disconnected { reason, .. } => {
                        panic!("unexpected integrated disconnect: {reason}")
                    }
                    _ => {}
                }
            }
            let view = f.supervisor.snapshot();
            login
                && terrain
                && moved
                && view.installed == 1
                && !view.owner_saved
                && view.world_time > 0
                && view.players.len() == 1
                && view.players[0]["loaded"] == true
                && view.players[0]["position"][0].as_f64() == expected_x
                && view.players[0]["yaw"] == 32.0
                && view.players[0]["items"] == 3
                && view.players[0]["book_open"] == true
                && view.players[0]["creative_reach_modifiers"] == 2
        }),
        "integrated play incomplete: {:?}",
        fixture.supervisor.snapshot()
    );
    expected_x.unwrap()
}
fn assert_integrated_completion(fixture: &Fixture, result: &std::result::Result<(), String>) {
    let view = fixture.supervisor.snapshot();
    assert!(view.worker_joined);
    assert_eq!(view.phase, "NativeDisposed");
    let transport = view.transport.as_ref().unwrap();
    assert!(transport.transport.quiescent);
    assert!(transport.transport.attempts.iter().all(|attempt| {
        attempt
            .outbound
            .as_ref()
            .is_none_or(|out| out.failure.is_none())
    }));
    assert!(transport.transport.terminals.iter().all(|terminal| {
        matches!(terminal.outcome, RetainedOutcome::Returned | RetainedOutcome::Closed)
            || (terminal.actor == horizon_client::lan::RetainedActor::Outer
                && matches!(terminal.outcome, RetainedOutcome::Eof | RetainedOutcome::Receive(_)))
            || (terminal.actor == horizon_client::lan::RetainedActor::Shutdown
                && matches!(&terminal.outcome, RetainedOutcome::Io(detail) if detail == "Transport endpoint is not connected (os error 107)"))
    }));
    if let Err(error) = result {
        assert_eq!(Some(error.clone()), transport_failure(transport));
    } else {
        assert!(transport_failure(transport).is_none());
    }
}
fn assert_integrated_saved(fixture: &Fixture, yaw: f32, expected_x: Option<f64>) {
    let world = fixture.root.join("world");
    let level = Level::read(&world).expect("native integrated world/player readback");
    let record = fixture.supervisor.snapshot().owner_record.unwrap();
    assert_eq!(level.player_uuid, Some(record));
    assert_ne!(
        record,
        horizon_client::lan::playerdata::uuid_ints(offline_uuid("IntegratedOwner"))
    );
    assert_eq!(count(&level), 3);
    assert_eq!(
        level
            .ender_items
            .iter()
            .flatten()
            .map(|s| s.count)
            .sum::<u32>(),
        2
    );
    assert_eq!(level.xp_level, 7);
    assert_eq!(level.rotation[0], yaw);
    if let Some(x) = expected_x {
        assert_eq!(level.position.unwrap()[0], x);
    }
    assert_eq!(level.time, fixture.supervisor.snapshot().world_time);
    assert_eq!(level.dimension, "minecraft:overworld");
    assert!(
        !horizon_client::lan::playerdata::path(&world, offline_uuid("IntegratedOwner")).exists()
    );
    for family in ["stats", "advancements"] {
        assert!(
            !crate::game::stats::player_json(
                &world,
                family,
                horizon_client::lan::playerdata::uuid_ints(offline_uuid("IntegratedOwner"))
            )
            .exists()
        );
    }
}
fn assert_pending_owner_json(fixture: &Fixture) {
    let record = fixture.supervisor.snapshot().owner_record.unwrap();
    for (family, expected) in [
        ("stats", OWNER_STATS_JSON),
        ("advancements", OWNER_ADVANCEMENTS_JSON),
    ] {
        assert_eq!(
            fs::read_to_string(crate::game::stats::player_json(
                &fixture.root.join("world"),
                family,
                record
            ))
            .unwrap(),
            expected
        );
    }
}

#[test]
#[ignore = "pinned resources; genuine integrated native owner Memory play/save mapping"]
fn integrated_owner_memory_play_and_native_close() {
    let mut fixture = integrated_fixture("integrated-play", Startup::default());
    let x = drive_integrated_owner(&mut fixture);
    let result = fixture.finish();
    assert_integrated_completion(&fixture, &result);
    assert_integrated_saved(&fixture, 32.0, Some(x));
    let record = fixture.supervisor.snapshot().owner_record.unwrap();
    for family in ["stats", "advancements"] {
        assert!(
            crate::game::stats::player_json(&fixture.root.join("world"), family, record).is_file()
        );
    }
}

#[test]
#[ignore = "pinned resources; pristine no-position owner takes actual native spawn retaining player fields"]
fn integrated_owner_pristine_uses_native_spawn() {
    let mut fixture = integrated_fixture(
        "integrated-pristine",
        Startup {
            pristine_owner: true,
            ..Default::default()
        },
    );
    let x = drive_integrated_owner(&mut fixture);
    let result = fixture.finish();
    assert_integrated_completion(&fixture, &result);
    assert_integrated_saved(&fixture, 32.0, Some(x));
}

#[test]
#[ignore = "pinned resources; pending integrated native owner survives no-player close"]
fn integrated_owner_pending_close_preserves_native_player() {
    let mut fixture = integrated_fixture("integrated-pending", Startup::default());
    let result = fixture.finish();
    assert_integrated_completion(&fixture, &result);
    assert!(result.is_ok());
    assert_integrated_saved(&fixture, 21.0, Some(8.5));
    assert_pending_owner_json(&fixture);
}

#[test]
#[ignore = "pinned resources; untouched integrated owner Joined retains saved player"]
fn integrated_owner_untouched_join_preserves_native_player() {
    let mut fixture = integrated_fixture(
        "integrated-untouched",
        Startup {
            stop_on_untouched_join: true,
            ..Default::default()
        },
    );
    fixture.connect(false, "IntegratedOwner");
    assert!(fixture.until(|f| f.supervisor.finished()));
    let result = fixture.finish();
    assert!(result.unwrap_err().contains("controlled untouched"));
    let view = fixture.supervisor.snapshot();
    assert!(
        view.worker_joined && view.owner_saved && view.installed == 0 && view.pending_events > 0
    );
    assert_integrated_saved(&fixture, 21.0, Some(8.5));
    assert_pending_owner_json(&fixture);
}

#[test]
#[ignore = "pinned resources; wrong integrated profile never becomes incomplete admission"]
fn integrated_owner_wrong_profile_closes_without_installation() {
    let mut fixture = integrated_fixture("integrated-wrong-profile", Startup::default());
    fixture.connect(false, "WrongOwner");
    assert!(fixture.until(|f| f.supervisor.finished()));
    assert!(
        fixture
            .finish()
            .unwrap_err()
            .contains("different owner profile")
    );
    let view = fixture.supervisor.snapshot();
    assert!(view.worker_joined && view.installed == 0 && view.owner_saved);
    assert_integrated_saved(&fixture, 21.0, Some(8.5));
    assert_pending_owner_json(&fixture);
    assert!(
        !horizon_client::lan::playerdata::path(
            &fixture.root.join("world"),
            offline_uuid("WrongOwner")
        )
        .exists()
    );
}

#[test]
#[ignore = "pinned resources; actual mapped JSON failure and unplaced owner close-only Retry"]
fn integrated_owner_partial_read_retries_mapped_record() {
    let mut fixture = integrated_fixture(
        "integrated-read",
        Startup {
            owner_bad_stats: true,
            ..Default::default()
        },
    );
    fixture.connect(false, "IntegratedOwner");
    assert!(fixture.until(|f| f.supervisor.snapshot().blocked.is_some()));
    let before = fixture.supervisor.snapshot();
    assert!(before.owner_saved && before.installed == 0);
    let stats = crate::game::stats::player_json(
        &fixture.root.join("world"),
        "stats",
        before.owner_record.unwrap(),
    );
    assert!(
        before
            .original_error
            .as_ref()
            .unwrap()
            .contains(&stats.display().to_string())
    );
    fs::write(&stats, "{}").unwrap();
    fixture
        .supervisor
        .commands
        .send(Command::Retry(before.attempt))
        .unwrap();
    assert!(fixture.until(|f| f.supervisor.finished()));
    assert_eq!(
        fixture.finish().unwrap_err(),
        before.original_error.unwrap()
    );
    assert_integrated_saved(&fixture, 21.0, Some(8.5));
    assert!(fixture.supervisor.snapshot().worker_joined);
}

#[test]
#[ignore = "pinned resources; actual native mapped stats write failure preserves owner through Retry"]
fn integrated_owner_native_write_retries_mapped_record() {
    let mut fixture = integrated_fixture("integrated-write", Startup::default());
    let x = drive_integrated_owner(&mut fixture);
    let stats = crate::game::stats::player_json(
        &fixture.root.join("world"),
        "stats",
        fixture.supervisor.snapshot().owner_record.unwrap(),
    );
    if stats.exists() {
        fs::rename(&stats, stats.with_extension("json.before-io-fault")).unwrap();
    }
    fs::create_dir_all(&stats).unwrap();
    fixture.supervisor.commands.send(Command::Close).unwrap();
    assert!(fixture.until(|f| f.supervisor.snapshot().blocked.is_some()));
    let before = fixture.supervisor.snapshot();
    assert!(
        before
            .blocked
            .as_ref()
            .unwrap()
            .contains(&stats.display().to_string()),
        "{:?}",
        before.blocked
    );
    fs::rename(&stats, stats.with_extension("json.blocker-preserved")).unwrap();
    fixture
        .supervisor
        .commands
        .send(Command::Retry(before.attempt))
        .unwrap();
    assert!(fixture.until(|f| f.supervisor.finished()));
    let original_result = fixture.finish();
    assert_integrated_completion(&fixture, &original_result);
    assert_integrated_saved(&fixture, 32.0, Some(x));
    assert!(stats.is_file());
}
fn send(client: &RetainedSession, packet: S) {
    assert!(matches!(
        client.send(packet),
        SendAdmission::Admitted { .. }
    ));
}

#[test]
#[ignore = "pinned resources; original server Game, MemoryPipe and TCP, native player readback"]
fn original_server_memory_tcp_play_and_native_close() {
    original_server_players_play_and_native_close(false);
}

#[test]
#[ignore = "pinned resources; first Memory/TCP players have no staged native files"]
fn original_server_first_players_play_and_native_close() {
    original_server_players_play_and_native_close(true);
}

fn original_server_players_play_and_native_close(first_players: bool) {
    let saved_profiles = [("ServerMemory", 3), ("ServerTcp", 5)];
    let mut fixture = Fixture::start(
        if first_players {
            "first-memory-tcp"
        } else {
            "memory-tcp"
        },
        Startup {
            refuse_live_consume: true,
            ..Default::default()
        },
        if first_players { &[] } else { &saved_profiles },
        false,
    );
    fixture.ready();
    if first_players {
        for name in ["ServerMemory", "ServerTcp"] {
            let uuid = offline_uuid(name);
            for world in [fixture.base.join("staging"), fixture.root.join("world")] {
                assert!(!horizon_client::lan::playerdata::path(&world, uuid).exists());
                for family in ["stats", "advancements"] {
                    assert!(
                        !crate::game::stats::player_json(
                            &world,
                            family,
                            horizon_client::lan::playerdata::uuid_ints(uuid)
                        )
                        .exists()
                    );
                }
            }
        }
    }
    let first = fixture.supervisor.snapshot().world_time;
    fixture.connect(false, "ServerMemory");
    fixture.connect(true, "ServerTcp");
    let mut login = [false; 2];
    let mut terrain = [false; 2];
    let mut play_sent = [false; 2];
    assert!(
        fixture.until(|f| {
            for (index, client) in f.clients.iter_mut().enumerate() {
                for event in client.poll_events(128) {
                    match event {
                        Event::Packet(packet) => match *packet {
                            C::Login(_) => login[index] = true,
                            C::PlayerPosition(position) => {
                                assert_eq!(position.relatives, 0);
                                send(
                                    client,
                                    S::AcceptTeleportation(play::AcceptTeleportation {
                                        id: position.teleport,
                                        x: position.position[0],
                                        y: position.position[1],
                                        z: position.position[2],
                                        yaw: position.yaw,
                                        pitch: position.pitch,
                                    }),
                                );
                                send(client, S::PlayerLoaded(Empty {}));
                                if !play_sent[index] {
                                    send(
                                        client,
                                        S::RecipeBookChangeSettings(RecipeBookChangeSettings {
                                            book_type: 0,
                                            open: true,
                                            filtering: true,
                                        }),
                                    );
                                    send(
                                        client,
                                        S::MovePlayerPosRot(play::MovePlayerPosRot {
                                            x: position.position[0] + 0.125,
                                            y: position.position[1],
                                            z: position.position[2],
                                            yaw: 15.0,
                                            pitch: 0.0,
                                            flags: 0,
                                        }),
                                    );
                                    play_sent[index] = true;
                                }
                            }
                            C::LevelChunkWithLight(packet) => {
                                assert!(login[index]);
                                assert!(!packet.chunk.buffer.is_empty());
                                terrain[index] = true;
                            }
                            C::ChunkBatchFinished(_) => send(
                                client,
                                S::ChunkBatchReceived(play::ChunkBatchReceived {
                                    chunks_per_tick: 9.0,
                                }),
                            ),
                            C::KeepAlive(packet) => send(client, S::KeepAlive(packet)),
                            _ => {}
                        },
                        Event::DecodeError(error) => panic!("native decode: {error}"),
                        Event::Disconnected { reason, .. } => {
                            panic!("unexpected original server disconnect: {reason}")
                        }
                        _ => {}
                    }
                }
            }
            let view = f.supervisor.snapshot();
            login.iter().all(|v| *v)
                && terrain.iter().all(|v| *v)
                && view.world_time > first
                && view.installed == 2
                && view.consume_refusal_checked
                && view.players.len() == 2
                && view.players.iter().all(|g| {
                    g["loaded"] == true
                        && g["body"] == true
                        && g["book_open"] == true
                        && g["book_filtering"] == true
                        && g["yaw"] == 15.0
                        && g["creative_reach_modifiers"] == 2
                })
        }),
        "original native play incomplete: {:?}",
        fixture.supervisor.snapshot()
    );
    assert_eq!(
        fixture
            .supervisor
            .snapshot()
            .players
            .iter()
            .map(|g| g["items"].as_u64().unwrap())
            .sum::<u64>(),
        if first_players { 0 } else { 8 }
    );
    let before = fixture.supervisor.snapshot();
    assert!(before.transport.as_ref().unwrap().failure.is_none());
    assert!(
        before
            .transport
            .as_ref()
            .unwrap()
            .transport
            .terminals
            .iter()
            .all(|terminal| matches!(terminal.outcome, RetainedOutcome::Returned))
    );
    let outcome = fixture.finish();
    let complete = fixture.supervisor.snapshot();
    assert!(complete.worker_joined);
    assert_eq!(complete.phase, "NativeDisposed");
    let transport = complete.transport.as_ref().unwrap();
    assert!(transport.transport.quiescent);
    if let Err(error) = &outcome {
        // A reader's original Eof/Receive can first be harvested after the
        // deliberate stop. It remains an application error with unresolved
        // cause, separate from checked native completion/readback. The actual
        // first run also recorded Shutdown::Io(ENOTCONN); retain that exact
        // diagnostic as Err, without a normal-stop or successful-shutdown claim.
        // Never accept outbound/writer, encode, other IO, protocol or panic here.
        assert!(transport.transport.attempts.iter().all(|attempt| {
            attempt
                .outbound
                .as_ref()
                .is_none_or(|outbound| outbound.failure.is_none())
        }));
        assert!(
            transport
                .transport
                .terminals
                .iter()
                .all(|terminal| matches!(
                    terminal.outcome,
                    RetainedOutcome::Returned | RetainedOutcome::Closed
                ) || (terminal.actor
                    == horizon_client::lan::RetainedActor::Shutdown
                    && matches!(&terminal.outcome,RetainedOutcome::Io(detail)
                        if detail == "Transport endpoint is not connected (os error 107)"))
                    || (terminal.actor == horizon_client::lan::RetainedActor::Outer
                        && matches!(
                            terminal.outcome,
                            RetainedOutcome::Eof | RetainedOutcome::Receive(_)
                        )))
        );
        assert_eq!(Some(error.clone()), transport_failure(transport));
        assert_eq!(complete.original_error.as_ref(), Some(error));
    }
    eprintln!(
        "ORIGINAL_SERVER_NATIVE_COMPLETE worker_joined=true application_result={outcome:?} raw_transport={transport:?}"
    );
    assert_eq!(
        count(&fixture.player("ServerMemory")),
        if first_players { 0 } else { 3 }
    );
    assert_eq!(
        count(&fixture.player("ServerTcp")),
        if first_players { 0 } else { 5 }
    );
    for name in ["ServerMemory", "ServerTcp"] {
        let player = fixture.player(name);
        assert_eq!(player.dimension, "minecraft:overworld");
        assert_eq!(
            player.player_extra.as_ref().unwrap().get("UUID"),
            Some(&Tag::IntArray(
                horizon_client::lan::playerdata::uuid_ints(offline_uuid(name)).to_vec()
            ))
        );
    }
    assert_eq!(fixture.player("ServerMemory").rotation[0], 15.0);
    assert_eq!(fixture.player("ServerTcp").rotation[0], 15.0);
    let level = Level::read(&fixture.root.join("world")).expect("actual native level readback");
    assert!(level.time > first);
}

#[test]
#[ignore = "pinned resources; untouched original admission cannot overwrite native player files"]
fn original_server_untouched_join_preserves_native_file() {
    let frozen = Arc::new(Gate::default());
    let mut fixture = Fixture::start(
        "untouched",
        Startup {
            stop_on_untouched_join: true,
            frozen: Some(frozen.clone()),
            ..Default::default()
        },
        &[("UntouchedPlayer", 3)],
        false,
    );
    let staged = fs::read(horizon_client::lan::playerdata::path(
        &fixture.base.join("staging"),
        offline_uuid("UntouchedPlayer"),
    ))
    .unwrap();
    fixture.ready();
    fixture.connect(false, "UntouchedPlayer");
    assert!(fixture.until(|f| f.supervisor.snapshot().phase == "Frozen"));
    let before = fixture.supervisor.snapshot();
    assert_eq!(before.players.len(), 1);
    assert_eq!(before.players[0]["loaded"], false);
    assert_eq!(before.players[0]["placed"], false);
    fixture.connect(true, "LateFrozenPlayer");
    let mut sent = [false; 2];
    assert!(
        fixture.until(|f| {
            for (index, client) in f.clients.iter_mut().enumerate() {
                for event in client.poll_events(64) {
                    if matches!(event, Event::Joined { .. }) && !sent[index] {
                        send(
                            client,
                            S::RecipeBookChangeSettings(RecipeBookChangeSettings {
                                book_type: 0,
                                open: true,
                                filtering: true,
                            }),
                        );
                        sent[index] = true;
                    }
                }
            }
            sent.iter().all(|sent| *sent)
                && f.supervisor
                    .snapshot()
                    .transport
                    .as_ref()
                    .is_some_and(|v| v.transport.queued_events >= 3)
        }),
        "actual late frozen native Joined/packets were not retained"
    );
    let queued = fixture
        .supervisor
        .snapshot()
        .transport
        .unwrap()
        .transport
        .queued_events;
    frozen.release();
    assert!(fixture.until(|f| f.supervisor.finished()));
    let result = fixture.finish().unwrap_err();
    assert!(result.contains("untouched original Joined"));
    assert_eq!(fixture.supervisor.snapshot().installed, 0);
    assert!(fixture.supervisor.snapshot().pending_events > 0);
    let final_transport = fixture.supervisor.snapshot().transport.unwrap();
    assert_eq!(final_transport.guests, 1);
    assert_eq!(final_transport.inbox_packets, 0);
    assert!(final_transport.transport.queued_events >= queued);
    assert!(
        !horizon_client::lan::playerdata::path(
            &fixture.root.join("world"),
            offline_uuid("LateFrozenPlayer")
        )
        .exists()
    );
    assert_eq!(
        fs::read(horizon_client::lan::playerdata::path(
            &fixture.root.join("world"),
            offline_uuid("UntouchedPlayer")
        ))
        .unwrap(),
        staged
    );
}

#[test]
#[ignore = "pinned resources; actual failed JSON stage, cached NBT, close-only explicit Retry"]
fn original_server_partial_admission_retries_only_failed_read() {
    let first_poll = Arc::new(Gate::default());
    let mut fixture = Fixture::start(
        "partial",
        Startup {
            first_poll: Some(first_poll.clone()),
            ..Default::default()
        },
        &[("PartialPlayer", 3)],
        true,
    );
    fixture.ready();
    fixture.connect(false, "PartialPlayer");
    let mut sent = false;
    assert!(fixture.until(|f| {
        for event in f.clients[0].poll_events(64) {
            if matches!(event, Event::Joined { .. }) && !sent {
                send(
                    &f.clients[0],
                    S::RecipeBookChangeSettings(RecipeBookChangeSettings {
                        book_type: 0,
                        open: true,
                        filtering: true,
                    }),
                );
                send(&f.clients[0], S::PlayerLoaded(Empty {}));
                sent = true;
            }
        }
        sent && f
            .supervisor
            .snapshot()
            .transport
            .as_ref()
            .is_some_and(|v| v.transport.queued_events >= 3)
    }));
    first_poll.release();
    assert!(fixture.until(|f| f.supervisor.snapshot().blocked.is_some()));
    let before = fixture.supervisor.snapshot();
    assert_eq!(before.installed, 0);
    assert!(before.original_error.as_deref().unwrap().contains("json"));
    assert_eq!(before.players.len(), 1);
    let inbox = before.players[0]["inbox"].as_u64().unwrap();
    assert!(inbox >= 2);
    assert_eq!(before.players[0]["loaded"], false);
    assert_eq!(before.players[0]["placed"], false);
    assert_eq!(before.players[0]["awaiting_teleport"], false);
    let world = fixture.root.join("world");
    let uuid = offline_uuid("PartialPlayer");
    write_dat(
        &horizon_client::lan::playerdata::path(&world, uuid),
        fixture_player(uuid, 99).player_tag(),
    )
    .unwrap();
    let stats = crate::game::stats::player_json(
        &world,
        "stats",
        horizon_client::lan::playerdata::uuid_ints(uuid),
    );
    fs::write(
        stats,
        "{\"stats\":{\"minecraft:custom\":{\"minecraft:jump\":7}}}",
    )
    .unwrap();
    fixture
        .supervisor
        .commands
        .send(Command::Retry(before.attempt + 1))
        .unwrap();
    fixture
        .supervisor
        .commands
        .send(Command::Retry(before.attempt))
        .unwrap();
    assert!(
        fixture.until(|f| f.supervisor.finished()),
        "same original close did not finish: {:?}",
        fixture.supervisor.snapshot()
    );
    assert_eq!(
        fixture.finish().unwrap_err(),
        before.original_error.unwrap()
    );
    let after = fixture.supervisor.snapshot();
    assert_eq!(after.attempt, before.attempt + 1);
    assert_eq!(after.players[0]["inbox"].as_u64(), Some(inbox));
    assert_eq!(after.players[0]["loaded"], false);
    assert_eq!(after.players[0]["placed"], false);
    assert_eq!(after.players[0]["awaiting_teleport"], false);
    assert_eq!(after.players[0]["book_open"], false);
    assert_eq!(after.players[0]["body"], true);
    assert_eq!(
        count(&fixture.player("PartialPlayer")),
        3,
        "successful original NBT read must not be repeated"
    );
}

#[test]
#[ignore = "pinned resources; actual post-streaming preparation failure must close original actors"]
fn original_server_partial_startup_joins_original_owners() {
    let mut fixture = Fixture::start(
        "cold-failure",
        Startup {
            fail_after_streaming: true,
            ..Default::default()
        },
        &[],
        false,
    );
    assert!(fixture.until(|f| f.supervisor.finished()));
    assert!(fixture.finish().unwrap_err().contains("StreamingAcquired"));
    assert!(fixture.supervisor.snapshot().worker_joined);
}

#[test]
#[ignore = "pinned resources; actual native player stats IO failure, same Game and explicit Retry"]
fn original_server_native_writer_failure_retry_preserves_error() {
    let frozen = Arc::new(Gate::default());
    let final_reply = Arc::new(Gate::default());
    let mut app = Application::start(
        "io",
        Startup {
            fail_native_write: true,
            frozen: Some(frozen.clone()),
            final_reply: Some(final_reply.clone()),
            ..Default::default()
        },
        &[("WriterPlayer", 5)],
        false,
    );
    let mut port = 0;
    assert!(app.until(|app| app.request("status").is_some_and(|v| {
        port = v["state"]["port"].as_u64().unwrap_or(0);
        v["state"]["ready"] == true
    })));
    let mut client = RetainedSession::new_offline(
        RetainedTarget::Address(format!("127.0.0.1:{port}")),
        RetainedSettings::offline("WriterPlayer".into(), 2, Arc::default()),
    )
    .unwrap_or_else(|r| panic!("{}", r.reason));
    client.start().unwrap();
    app.clients.push(client);
    assert!(app.until(|app| {
        app.request("status")
            .is_some_and(|v| v["state"]["installed"] == 1)
    }));
    assert_eq!(app.request("close").unwrap()["accepted"], true);
    assert!(app.until(|app| {
        app.request("status")
            .is_some_and(|v| v["state"]["phase"] == "Frozen")
    }));
    assert!(
        app.until(|app| app.request("status").is_some_and(|v| {
            v["state"]["transport"]["outbound"]
                .as_array()
                .is_some_and(|outbound| {
                    !outbound.is_empty()
                        && outbound.iter().all(|queue| {
                            queue["queued_bodies"] == 0
                                && queue["active_bodies"] == 0
                                && queue["failure"].is_null()
                        })
                })
        })),
        "actual frozen native writer must drain before injecting peer departure"
    );
    // The Game is frozen but its original transport still owns the native
    // actors. An actual peer EOF now must survive later close harvesting.
    app.clients[0].begin_close(Instant::now() + Duration::from_secs(10));
    assert!(app.until(|app| app.clients[0].poll_close() == RetainedClose::Quiescent));
    assert!(app.until(|app| app.request("status").is_some_and(|v| {
        v["state"]["transport"]["queued_events"]
            .as_u64()
            .is_some_and(|n| n > 0)
    })));
    frozen.release();
    let mut failed = serde_json::Value::Null;
    assert!(app.until(|app| app.request("status").is_some_and(|v| {
        if v["state"]["blocked"].is_string() {
            failed = v;
            true
        } else {
            false
        }
    })));
    assert_eq!(failed["state"]["phase"], "NativeClosing");
    assert_eq!(failed["control_endpoint"], "Published");
    assert!(
        failed["state"]["blocked"]
            .as_str()
            .unwrap()
            .contains("json")
    );
    let original = failed["state"]["original_error"]
        .as_str()
        .unwrap()
        .to_owned();
    let first = failed["state"]["transport"]["terminals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|terminal| terminal["outcome"] != "Returned" && terminal["outcome"] != "Closed")
        .expect("original failed native terminal");
    assert!(
        first["outcome"] == "Eof"
            || first["outcome"]
                .as_str()
                .is_some_and(|value| value.starts_with("Receive("))
            || (first["actor"] == "Shutdown"
                && first["outcome"]
                    == r#"Io("Transport endpoint is not connected (os error 107)")"#)
    );
    assert!(
        original.contains(first["outcome"].as_str().unwrap()),
        "preserve actual first native failure without assuming terminal order"
    );
    assert!(
        failed["state"]["transport"]["terminals"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["actor"] == "Outer"
                && (v["outcome"] == "Eof"
                    || v["outcome"]
                        .as_str()
                        .is_some_and(|value| value.starts_with("Receive("))))
    );
    assert!(
        !app.finished(),
        "actual parent/control must retain the original blocked worker"
    );
    let attempt = failed["state"]["attempt"].as_u64().unwrap();
    assert!(app.request(&format!("retry {}", attempt + 1)).unwrap()["error"].is_string());
    assert_eq!(app.request("status").unwrap()["state"]["attempt"], attempt);
    let path = crate::game::stats::player_json(
        &app.root.join("world"),
        "stats",
        horizon_client::lan::playerdata::uuid_ints(offline_uuid("WriterPlayer")),
    );
    assert!(path.is_dir());
    fs::rename(&path, path.with_extension("json.blocker-preserved")).unwrap();
    assert_eq!(
        app.request(&format!("retry {attempt}")).unwrap()["accepted"],
        true,
        "actual endpoint must finish its Retry response"
    );
    assert!(
        app.until(|_| final_reply.entered.load(Ordering::SeqCst)),
        "actual post-join parent boundary not reached"
    );
    writeln!(app.control.as_mut().unwrap().get_mut(), "status").unwrap();
    assert!(
        app.until(|_| final_reply.registered.load(Ordering::SeqCst)),
        "actual post-join request not registered"
    );
    final_reply.release();
    let mut final_text = String::new();
    app.control
        .as_mut()
        .unwrap()
        .read_line(&mut final_text)
        .expect("actual final response must arrive before endpoint sealing");
    let final_status: serde_json::Value = serde_json::from_str(&final_text).unwrap();
    assert_eq!(final_status["worker_completed"], true);
    assert_eq!(final_status["state"]["worker_joined"], true);
    assert_eq!(final_status["state"]["transport"]["quiescent"], true);
    assert_eq!(final_status["state"]["original_error"], original);
    assert_eq!(app.finish().unwrap_err(), original);
    assert!(
        !app.endpoint.exists(),
        "actual endpoint released only after checked worker/native closure"
    );
    let tag = read_dat_checked(&horizon_client::lan::playerdata::path(
        &app.root.join("world"),
        offline_uuid("WriterPlayer"),
    ))
    .unwrap()
    .unwrap();
    let mut player = Level::default();
    player.read_player(&tag);
    assert_eq!(count(&player), 5);
}

#[test]
#[ignore = "pinned resources; actual asynchronous native endpoint acquisition failure and checked parent close"]
fn original_server_parent_preserves_async_endpoint_failure() {
    let mut app = Application::start("ep", Startup::default(), &[], true);
    let error = app.finish().unwrap_err();
    assert!(
        error.contains("Acquisition"),
        "original stage must be retained: {error}"
    );
    assert!(
        error.contains("NativeIo"),
        "original native kind/detail must be retained: {error}"
    );
    assert!(!app.endpoint.exists());
}
