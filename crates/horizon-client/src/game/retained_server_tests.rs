//! Fixed original Game/worker fixtures. The caller supplies pinned resources
//! and isolated cwd/home/temp; no test borrows player installations or saves.
use super::*;
#[cfg(all(feature = "mods", target_os = "linux"))]
use horizon_client::gamerules::GameRules;
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

#[cfg(all(
    feature = "mods",
    target_os = "linux",
    feature = "native-writer-test-control",
    debug_assertions
))]
#[derive(Default)]
pub(in crate::game) struct OrdinaryRecoveryWriter {
    control: Mutex<Option<horizon_client::save::NativeWriterTestControl>>,
    original: Mutex<Option<std::sync::Weak<horizon_client::save::SaveDir>>>,
}
#[cfg(all(
    feature = "mods",
    target_os = "linux",
    feature = "native-writer-test-control",
    debug_assertions
))]
impl OrdinaryRecoveryWriter {
    pub(super) fn arm(&self, game: &Game<'_>) -> std::result::Result<(), String> {
        let save = game
            .save
            .as_ref()
            .ok_or("actual recovered SaveDir absent")?;
        let far = (4096, 4096);
        let record = save.root.join("entities/r.128.128.mca");
        match fs::symlink_metadata(&record) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err("actual distant entity region must be absent".into()),
        }
        let control = save
            .arm_native_writer_test_control(horizon_client::save::NativeWriterTestAction::Block)
            .map_err(|error| error.to_string())?;
        save.save_entities_later(vec![(far, Vec::new())])
            .map_err(|error| error.to_string())?;
        *self.original.lock().unwrap() = Some(Arc::downgrade(save));
        *self.control.lock().unwrap() = Some(control);
        Ok(())
    }
}
#[cfg(all(
    feature = "mods",
    target_os = "linux",
    feature = "native-writer-test-control",
    debug_assertions
))]
struct OrdinaryWriterRelease {
    writer: Arc<OrdinaryRecoveryWriter>,
    commands: mpsc::Sender<Command>,
    view: Arc<Mutex<View>>,
}
#[cfg(all(
    feature = "mods",
    target_os = "linux",
    feature = "native-writer-test-control",
    debug_assertions
))]
impl Drop for OrdinaryWriterRelease {
    fn drop(&mut self) {
        // The worker also holds the shared fixture. Root cancellation must not
        // wait for that worker to drop its own blocked writer command.
        drop(
            self.writer
                .control
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take(),
        );
        let view = self
            .view
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if view.blocked.is_some() {
            let _ = self.commands.send(Command::Retry(view.attempt));
        }
    }
}

#[cfg(all(feature = "mods", target_os = "linux"))]
#[derive(Clone, Copy)]
enum OrdinaryDomainKind {
    Native,
    Guest,
}
#[cfg(all(feature = "mods", target_os = "linux"))]
pub(in crate::game) struct OrdinaryControl {
    kind: OrdinaryDomainKind,
    position: [i32; 3],
    namespace: world_loading::host_identity::session_authority::AuthorityNamespace,
    archive: PathBuf,
    continuation: PathBuf,
    slot: Mutex<Option<horizon_client::world_slot_trial::SlotKey>>,
    capture_requested: AtomicBool,
    captured_once: AtomicBool,
    source: Mutex<Option<super::super::levels::session_checkpoint::tests::OrdinaryFixtureCapture>>,
    recovered:
        Mutex<Option<super::super::levels::session_checkpoint::tests::OrdinaryFixtureObservation>>,
}
#[cfg(all(feature = "mods", target_os = "linux"))]
impl OrdinaryControl {
    pub(super) fn bootstrap(&self, game: &mut Game<'_>) -> std::result::Result<(), String> {
        use horizon_client::world::counter_restore::{CounterTarget, NativeBinding};
        let catalog = game
            .options
            .mod_packages
            .catalog()
            .ok_or("actual fixture catalog absent")?;
        let mut types = catalog.prepared().values().flat_map(|prepared| {
            prepared
                .content()
                .machine_types()
                .types()
                .iter()
                .map(move |kind| (prepared.clone(), kind.id().to_owned()))
        });
        let (pair, type_id) = types
            .next()
            .ok_or("actual registered fixture type absent")?;
        if types.next().is_some() {
            return Err("ordinary fixture requires exactly one captured machine type".into());
        }
        let binding = match self.kind {
            OrdinaryDomainKind::Native => {
                NativeBinding::from_registered_prepared(&pair, &type_id, game.pack.assets.registry)
            }
            OrdinaryDomainKind::Guest => NativeBinding::from_registered_guest_prepared(
                &pair,
                &type_id,
                game.pack.assets.registry,
            ),
        }
        .map_err(|error| error.to_string())?;
        if game.world.counter_domain_snapshot().is_some()
            || self.slot.lock().unwrap().is_some()
            || !game
                .world
                .chunks
                .contains_key(&(self.position[0] >> 4, self.position[2] >> 4))
        {
            return Err(
                "ordinary fixture requires unused actual bootstrap and resident target".into(),
            );
        }
        game.world
            .set(
                self.position[0],
                self.position[1],
                self.position[2],
                binding.state_id(),
            )
            .map_err(|error| error.to_string())?;
        let slot = match self.kind {
            OrdinaryDomainKind::Native => {
                game.bootstrap_registered_host_counter(&type_id, self.position)
                    .map_err(|error| error.to_string())?;
                game.world
                    .initialize_persistent_world_slot(
                        &binding,
                        &CounterTarget::new(Dim::Overworld.id(), self.position)
                            .map_err(|error| error.to_string())?,
                    )
                    .map_err(|error| format!("{error:?}"))?
                    .ok_or("actual fixture slot was not allocated")?
            }
            OrdinaryDomainKind::Guest => {
                game.bootstrap_registered_guest_counter(&type_id, self.position)
                    .map_err(|error| error.to_string())?
                    .1
            }
        };
        *self.slot.lock().unwrap() = Some(slot);
        Ok(())
    }
    pub(super) fn capture(&self, game: &mut Game<'_>) -> std::result::Result<(), String> {
        if !self.capture_requested.load(Ordering::SeqCst)
            || self.captured_once.load(Ordering::SeqCst)
        {
            return Ok(());
        }
        let expected = if game
            .retained_server
            .as_ref()
            .unwrap()
            .integrated_owner()
            .is_some()
        {
            1
        } else {
            2
        };
        let host = game.lan.as_ref().ok_or("actual fixture host absent")?;
        if host.hub.guests.len() != expected
            || host
                .hub
                .guests
                .iter()
                .any(|guest| !guest.placed || !guest.loaded || guest.motion.is_none())
        {
            return Ok(());
        }
        let slot = self
            .slot
            .lock()
            .unwrap()
            .as_ref()
            .ok_or("actual fixture slot absent")?
            .clone();
        let mut host = game.lan.take().unwrap();
        let conn = host.hub.guests[0].conn;
        if host
            .hub
            .guests
            .iter()
            .any(|guest| guest.inventory.iter().any(Option::is_some))
        {
            game.lan = Some(host);
            return Err("fixture gift requires genuinely empty original inventories".into());
        }
        // Authored fixture setup, never ordinary machine ingress. Transfer uses
        // the real inventory-cell lend and slot transaction on the actual Guest.
        host.hub.guests[0].inventory[0] = Some(ItemStack::new("minecraft:stone", 9));
        if expected == 2 {
            host.hub.guests[1].inventory[0] = Some(ItemStack::new("minecraft:stone", 4));
        }
        let transfer = game.as_guest(&mut host, conn, |game| {
            game.transfer_world_slot_inventory_cell(
                &slot,
                0,
                horizon_client::slot_transfer::Direction::IntoMachine,
                7,
            )
        });
        game.lan = Some(host);
        let transfer = transfer
            .ok_or("actual fixture Guest was not lent")?
            .map_err(|error| format!("{error:?}"))?;
        if !matches!(transfer, super::super::slot_player::TransferOutcome::Committed(row) if row.moved == 7)
        {
            return Err("fixture transfer did not commit exactly seven original items".into());
        }
        let machine = game
            .world
            .read_world_slot(&slot)
            .map_err(|error| format!("{error:?}"))?;
        let host = game.lan.as_ref().unwrap();
        let counts = host
            .hub
            .guests
            .iter()
            .map(|guest| {
                guest
                    .inventory
                    .iter()
                    .flatten()
                    .map(|stack| stack.count)
                    .sum::<u32>()
            })
            .collect::<Vec<_>>();
        if !machine.slot.stack().is_some_and(|stack| {
            &*stack.item == "minecraft:stone"
                && stack.count == 7
                && stack.damage == 0
                && stack.extra.is_none()
        }) || counts[0] != 2
            || (expected == 2 && counts[1] != 4)
            || host.hub.guests.iter().any(|guest| {
                guest
                    .armor
                    .iter()
                    .chain(std::iter::once(&guest.offhand))
                    .chain(guest.ender_items.iter())
                    .any(Option::is_some)
            })
            || !game.dropped.is_empty()
        {
            return Err("authored fixture must own exactly machine7/player2[/player4] with no additional item owners".into());
        }
        let captured = super::super::levels::session_checkpoint::tests::capture_ordinary_fixture(
            game,
            &self.namespace,
            &self.archive,
            &self.continuation,
        )
        .map_err(|error| error.to_string())?;
        *self.source.lock().unwrap() = Some(captured);
        self.captured_once.store(true, Ordering::SeqCst);
        Ok(())
    }
    pub(in crate::game) fn record_recovered(
        &self,
        observation: super::super::levels::session_checkpoint::tests::OrdinaryFixtureObservation,
    ) {
        let mut recovered = self.recovered.lock().unwrap();
        assert!(
            recovered.is_none(),
            "one actual verified recovery observation"
        );
        *recovered = Some(observation);
    }
}

#[cfg(all(feature = "mods", target_os = "linux"))]
struct OrdinaryBundle {
    scratch: PathBuf,
    base: PathBuf,
    root: PathBuf,
    resources: Arc<Resources>,
    pack: PackResources,
    packages: mod_packages::Packages,
    startup: Arc<mod_packages::FrozenHostInputs>,
    tree: Arc<horizon_client::advancements::Tree>,
}
#[cfg(all(feature = "mods", target_os = "linux"))]
impl OrdinaryBundle {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let scratch = std::env::temp_dir().join(format!(
            "horizon-ordinary-{}-{}-{label}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&scratch).unwrap();
        let base = PathBuf::from(
            std::env::var_os("HORIZON_TEST_RESOURCES").expect("pinned resources required"),
        );
        let pair = PathBuf::from(
            std::env::var_os("HORIZON_COUNTER_PAIR_FIXTURE")
                .expect("prebuilt captured pair required"),
        );
        let packages = mod_packages::Packages::read(
            &std::collections::BTreeSet::new(),
            std::slice::from_ref(&pair),
        )
        .unwrap();
        let settings = Self::settings();
        let root = mods::content_root(&base, &settings.mods.enabled, &[pair], &packages).unwrap();
        assert_ne!(root, base, "genuine selected materialization");
        let loaded = load_resources(
            root.clone(),
            &base,
            &settings,
            Instant::now(),
            false,
            &Progress::new(),
            &mut |_, _| {},
        )
        .unwrap();
        assert_eq!(loaded.root, root);
        assert!(loaded.pack_error.is_none() && loaded.rest_loader.is_none());
        let sync = Arc::new(horizon_client::lan::data::SyncData::load(&root).unwrap());
        let startup = Arc::new(
            packages
                .freeze_peer_inputs(sync, loaded.pack.assets.registry)
                .unwrap()
                .unwrap(),
        );
        let tree = Arc::new(horizon_client::advancements::Tree::load(&root.join("data")));
        assert!(!tree.list.is_empty());
        Self {
            scratch,
            base,
            root,
            resources: loaded.resources,
            pack: loaded.pack,
            packages,
            startup,
            tree,
        }
    }
    fn settings() -> Settings {
        let mut settings = Settings {
            render_distance: 2,
            simulation_distance: 2,
            ..Default::default()
        };
        settings.horizon.lod_distance = 0;
        settings.controller.enabled = false;
        settings
    }
    fn input(
        &self,
        role: execution_role::ExecutionRole,
        request: OrdinaryRequest,
        control: Option<Arc<OrdinaryControl>>,
        archive: Option<PathBuf>,
    ) -> OrdinaryServerInput {
        let mut options = Options::read_from(vec![
            "--resources".into(),
            self.root.to_string_lossy().into_owned(),
            "--in-frame".into(),
            "--test-terrain".into(),
            "--creative".into(),
            "--name".into(),
            "OrdinaryOwner".into(),
        ])
        .unwrap()
        .unwrap()
        .options;
        options.execution_role = role;
        options.dedicated = Some(Vec::new());
        options.server_thread = false;
        options.mod_packages = self.packages.clone();
        OrdinaryServerInput {
            options,
            settings: Self::settings(),
            request,
            entry: RetainedEntryIdentity {
                token: 1,
                owner: Arc::new(()),
            },
            resources: self.resources.clone(),
            pack: self.pack.clone(),
            gui: UiResources::load(&self.root, &self.base).unwrap(),
            cpu: GameCpuResources::load(&self.root).unwrap(),
            tree: self.tree.clone(),
            startup: self.startup.clone(),
            offline: RetainedOfflineConfig {
                bind: (!matches!(role, execution_role::ExecutionRole::IntegratedServer))
                    .then_some(std::net::Ipv4Addr::LOCALHOST.into()),
                port: 0,
                motd: "Ordinary fixture".into(),
                max_players: if role == execution_role::ExecutionRole::IntegratedServer {
                    1
                } else {
                    2
                },
                compression_threshold: -1,
                enable_status: false,
                hide_online_players: true,
                announce: false,
            },
            dedicated_setup: None,
            continuation_archive: archive,
            test: Startup {
                ordinary: control,
                ..Default::default()
            },
        }
    }
    fn control(&self, label: &str, kind: OrdinaryDomainKind) -> Arc<OrdinaryControl> {
        let directory = self.scratch.join(label);
        fs::create_dir(&directory).unwrap();
        for child in ["authority", "source-archive", "continuation"] {
            fs::create_dir(directory.join(child)).unwrap();
        }
        Arc::new(OrdinaryControl {
            kind,
            position: [2, 120, 2],
            namespace: world_loading::host_identity::session_authority::AuthorityNamespace::new(
                &directory.join("authority"),
            )
            .unwrap(),
            archive: directory.join("source-archive"),
            continuation: directory.join("continuation"),
            slot: Mutex::new(None),
            capture_requested: AtomicBool::new(false),
            captured_once: AtomicBool::new(false),
            source: Mutex::new(None),
            recovered: Mutex::new(None),
        })
    }
}

#[cfg(all(feature = "mods", target_os = "linux"))]
struct OrdinaryClient {
    session: RetainedSession,
    admission: Option<horizon_client::lan::login_negotiation::ConfiguredAdmission>,
    closed_envelope: Option<horizon_client::lan::login_negotiation::ConfiguredEnvelope>,
    login: bool,
    terrain: bool,
}
#[cfg(all(feature = "mods", target_os = "linux"))]
struct OrdinaryRun {
    supervisor: Supervisor,
    clients: Vec<OrdinaryClient>,
    closing: bool,
    publication_seen: bool,
    expected_server_close: bool,
}
#[cfg(all(feature = "mods", target_os = "linux"))]
impl OrdinaryRun {
    fn start(input: OrdinaryServerInput) -> Self {
        let supervisor = Supervisor::prepare_ordinary(input)
            .unwrap_or_else(|refused| panic!("ordinary original input: {}", refused.reason));
        Self {
            supervisor,
            clients: Vec::new(),
            closing: false,
            publication_seen: false,
            expected_server_close: false,
        }
    }
    fn until(&mut self, mut condition: impl FnMut(&mut Self) -> bool) -> bool {
        let until = Instant::now() + Duration::from_secs(120);
        loop {
            self.pump();
            if condition(self) {
                return true;
            }
            if Instant::now() >= until || (self.supervisor.finished() && !self.closing) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn connect(&mut self, bundle: &OrdinaryBundle, tcp: bool, name: &str) {
        let target = if tcp {
            RetainedTarget::Address(format!("127.0.0.1:{}", self.supervisor.snapshot().port))
        } else {
            let offer = self.supervisor.offer().unwrap();
            RetainedTarget::Memory(
                offer
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap()
                    .unwrap(),
            )
        };
        let required = horizon_client::lan::login_negotiation::RequiredNegotiation::new(
            bundle.startup.negotiation_expectation().unwrap(),
            Duration::from_secs(30),
        )
        .unwrap();
        let mut session = RetainedSession::new_offline_paired(
            target,
            RetainedSettings::offline(name.into(), 2, Arc::default()),
            required,
        )
        .unwrap_or_else(|refused| panic!("actual paired fixture client: {}", refused.reason));
        session.start().expect("actual paired fixture actors");
        self.clients.push(OrdinaryClient {
            session,
            admission: None,
            closed_envelope: None,
            login: false,
            terrain: false,
        });
    }
    fn pump(&mut self) {
        for client in &mut self.clients {
            for event in client.session.poll_events(128) {
                match event {
                    Event::PairedJoined(mut envelope) => {
                        assert!(client.admission.is_none() && client.closed_envelope.is_none());
                        match client.session.consume_paired(&mut envelope) {
                            Ok(admission) => client.admission = Some(admission),
                            Err(horizon_client::lan::login_negotiation::Refusal::Closed)
                                if self.closing || self.expected_server_close =>
                            {
                                // Server refusal can close the genuine Session
                                // before its queued callback is consumed. Keep
                                // that refused envelope until native teardown;
                                // never turn it into admission or ignore play.
                                client.closed_envelope = Some(envelope);
                            }
                            Err(error) => panic!("same actual configured Session: {error:?}"),
                        }
                    }
                    Event::Joined { .. } => {
                        panic!("ordinary fixture received generic Joined bypass")
                    }
                    Event::Packet(packet) => {
                        if self.closing
                            || (self.expected_server_close
                                && !client
                                    .admission
                                    .as_ref()
                                    .is_some_and(|admission| admission.is_live()))
                        {
                            // Negative tests must still observe forbidden late
                            // publication; only follow-up sends are suppressed.
                            match *packet {
                                C::Login(_) => client.login = true,
                                C::LevelChunkWithLight(_) => client.terrain = true,
                                _ => {}
                            }
                            continue;
                        }
                        assert!(
                            client
                                .admission
                                .as_ref()
                                .is_some_and(|admission| admission.is_live())
                        );
                        match *packet {
                            C::Login(_) => client.login = true,
                            C::PlayerPosition(position) => {
                                send(
                                    &client.session,
                                    S::AcceptTeleportation(play::AcceptTeleportation {
                                        id: position.teleport,
                                        x: position.position[0],
                                        y: position.position[1],
                                        z: position.position[2],
                                        yaw: position.yaw,
                                        pitch: position.pitch,
                                    }),
                                );
                                send(&client.session, S::PlayerLoaded(Empty {}));
                            }
                            C::LevelChunkWithLight(packet) => {
                                assert!(client.login && !packet.chunk.buffer.is_empty());
                                client.terrain = true;
                            }
                            C::ChunkBatchFinished(_) => send(
                                &client.session,
                                S::ChunkBatchReceived(play::ChunkBatchReceived {
                                    chunks_per_tick: 9.0,
                                }),
                            ),
                            C::KeepAlive(packet) => send(&client.session, S::KeepAlive(packet)),
                            _ => {}
                        }
                    }
                    Event::DecodeError(error) if !self.closing => {
                        panic!("ordinary fixture decode: {error}")
                    }
                    Event::Disconnected { reason, .. }
                        if !self.closing && !self.expected_server_close =>
                    {
                        panic!("ordinary fixture disconnected before requested close: {reason}")
                    }
                    _ => {}
                }
            }
            self.publication_seen |= client.login || client.terrain;
        }
    }
    fn finish(&mut self) -> Option<super::super::paired_close::OrdinaryClosedWorld> {
        self.closing = true;
        self.supervisor.request_close();
        assert!(
            self.until(
                |run| run.supervisor.finished() || run.supervisor.snapshot().blocked.is_some()
            ) && self.supervisor.finished(),
            "ordinary original close: {:?}",
            self.supervisor.snapshot()
        );
        let result = self
            .supervisor
            .join()
            .expect("original finished-only ordinary join");
        let view = self.supervisor.snapshot();
        assert_eq!(view.phase, "NativeDisposed", "{view:?}");
        if let Err(reason) = result {
            assert_eq!(
                view.transport
                    .as_ref()
                    .and_then(transport_failure)
                    .as_deref(),
                Some(reason.as_str()),
                "unrelated failure is not transport completion"
            );
        }
        self.close_clients();
        self.supervisor.take_ordinary_closed_world()
    }
    fn close_clients(&mut self) {
        let until = Instant::now() + Duration::from_secs(15);
        for client in &mut self.clients {
            client.session.begin_close(until);
        }
        while !self
            .clients
            .iter_mut()
            .all(|client| client.session.poll_close() == RetainedClose::Quiescent)
        {
            if Instant::now() >= until {
                eprintln!("fatal: original ordinary fixture clients unresolved");
                std::process::abort();
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        for client in self.clients.drain(..) {
            if client.session.try_closed().is_err() {
                std::process::abort();
            }
            drop(client.admission);
        }
    }
    fn finish_refusal(
        &mut self,
        phase: &str,
    ) -> (
        String,
        Option<super::super::paired_close::OrdinaryClosedWorld>,
    ) {
        self.closing = true;
        assert!(
            self.until(
                |run| run.supervisor.finished() || run.supervisor.snapshot().blocked.is_some()
            ) && self.supervisor.finished(),
            "original refusal cleanup: {:?}",
            self.supervisor.snapshot()
        );
        let error = self
            .supervisor
            .join()
            .expect("original finished-only refusal join")
            .expect_err("expected original refusal");
        let view = self.supervisor.snapshot();
        assert!(view.worker_joined && view.blocked.is_none(), "{view:?}");
        assert_eq!(view.phase, phase);
        assert_eq!(view.original_error.as_deref(), Some(error.as_str()));
        assert!(
            self.until(|run| run.clients.iter().all(|client| client
                .session
                .observe()
                .terminals
                .iter()
                .any(|terminal| terminal.actor == horizon_client::net::RetainedActor::Reader))),
            "original refused peer readers did not finish"
        );
        self.pump();
        self.close_clients();
        (error, self.supervisor.take_ordinary_closed_world())
    }
}
#[cfg(all(feature = "mods", target_os = "linux"))]
impl Drop for OrdinaryRun {
    fn drop(&mut self) {
        self.closing = true;
        self.supervisor.request_close();
        let until = Instant::now() + Duration::from_secs(30);
        while self.supervisor.thread.is_some() && !self.supervisor.finished() {
            self.pump();
            if Instant::now() >= until {
                eprintln!(
                    "fatal: original ordinary fixture remains owned: {:?}",
                    self.supervisor.snapshot()
                );
                std::process::abort();
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        if self.supervisor.finished() {
            self.supervisor.join();
        }
        self.close_clients();
    }
}

#[cfg(all(feature = "mods", target_os = "linux"))]
fn ordinary_own_selection(
    case: &std::path::Path,
    new: bool,
) -> world_loading::OrdinaryNetworkSelection {
    let mut request = world_loading::WorldRequest::new(
        "unused",
        new.then_some("Ordinary OWN"),
        new.then_some(17),
        true,
        None,
    )
    .unwrap()
    .with_identity_intent(if new {
        world_loading::host_identity::IdentityIntent::CreateNewIdentity
    } else {
        world_loading::host_identity::IdentityIntent::RequireExistingIdentity
    });
    request.save_root = case.join("saves/Owned");
    request
        .into_ordinary_network(case, None, Instant::now() + Duration::from_secs(120))
        .unwrap()
}
#[cfg(all(feature = "mods", target_os = "linux"))]
fn ordinary_own_archive(case: &std::path::Path) -> PathBuf {
    let identity: serde_json::Value = serde_json::from_slice(
        &fs::read(case.join("saves/Owned/horizon-world-identity.json")).unwrap(),
    )
    .unwrap();
    let record: serde_json::Value = serde_json::from_slice(
        &fs::read(
            case.join("backups/horizon-owned/authority")
                .join(identity["world_uuid"].as_str().unwrap())
                .join("owned.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let archive = PathBuf::from(record["archive"].as_str().unwrap());
    assert_eq!(
        archive.parent(),
        Some(case.join("backups/horizon-owned/archives").as_path())
    );
    archive
}
#[cfg(all(feature = "mods", target_os = "linux"))]
fn ordinary_own_observation(view: &View) -> serde_json::Value {
    let mut players=view.players.iter().map(|player|serde_json::json!({
        "uuid":player["uuid"],"position":player["position"],"yaw":player["yaw"],"items":player["items"],
    })).collect::<Vec<_>>();
    players.sort_by(|a, b| a["uuid"].as_str().unwrap().cmp(b["uuid"].as_str().unwrap()));
    serde_json::json!({"clock":view.world_time,"players":players})
}
#[cfg(all(feature = "mods", target_os = "linux"))]
#[test]
#[ignore = "isolated captured resources: ordinary network factories and original workers, serial fresh-process Memory/TCP OWN Unplaced reopen"]
fn ordinary_owned_network_new_close_reopen_memory_and_tcp() {
    use sha2::{Digest, Sha256};
    let root = std::env::temp_dir().join(format!(
        "horizon-ordinary-own-restart-{}",
        std::process::id()
    ));
    fs::create_dir(&root).unwrap();
    eprintln!("ORDINARY_OWN_NATIVE_EVIDENCE_ROOT={}", root.display());
    let exe = std::env::current_exe().unwrap();
    let digest = Sha256::digest(fs::read(&exe).unwrap());
    for phase in ["A", "B"] {
        let home = root.join(format!("process-{phase}"));
        fs::create_dir(&home).unwrap();
        let temp = home.join("temp");
        fs::create_dir(&temp).unwrap();
        let mut child = std::process::Command::new(&exe)
            .args([
                "--exact",
                "game::retained_server::tests::ordinary_owned_network_process_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("HORIZON_ORDINARY_OWN_PHASE", phase)
            .env("HORIZON_ORDINARY_OWN_ROOT", &root)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("XDG_CONFIG_HOME", &home)
            .env("XDG_DATA_HOME", &home)
            .env("XDG_CACHE_HOME", &home)
            .env("TMPDIR", &temp)
            .env("TMP", &temp)
            .env("TEMP", &temp)
            .current_dir(&home)
            .spawn()
            .unwrap();
        let until = Instant::now() + Duration::from_secs(600);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= until {
                child.kill().unwrap();
                let _ = child.wait();
                panic!(
                    "ordinary OWN child {phase} deadline; retained {}",
                    root.display()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(
            status.success(),
            "ordinary OWN child {phase} failed; retained {}",
            root.display()
        );
        assert_eq!(Sha256::digest(fs::read(&exe).unwrap()), digest);
    }
    let a: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("A.json")).unwrap()).unwrap();
    let b: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("B.json")).unwrap()).unwrap();
    assert_ne!(a["pid"], b["pid"]);
    assert_eq!(a["executable"], b["executable"]);
    assert_eq!(b["completed"], true);
    eprintln!("ORDINARY_OWN_FRESH_PROCESS_MEMORY_TWO_TCP_UNPLACED_PASS");
}
#[cfg(all(feature = "mods", target_os = "linux"))]
#[test]
#[ignore = "child only, selected by exact same-executable ordinary OWN parent"]
fn ordinary_owned_network_process_child() {
    use sha2::{Digest, Sha256};
    let phase = std::env::var("HORIZON_ORDINARY_OWN_PHASE").expect("explicit child phase");
    assert!(matches!(phase.as_str(), "A" | "B"));
    let root = PathBuf::from(
        std::env::var_os("HORIZON_ORDINARY_OWN_ROOT").expect("explicit native case root"),
    );
    let bundle = OrdinaryBundle::new(&format!("own-network-{phase}"));
    for (label, role) in [
        ("memory", execution_role::ExecutionRole::IntegratedServer),
        ("tcp", execution_role::ExecutionRole::DedicatedServer),
    ] {
        let case = root.join(label);
        if phase == "A" {
            fs::create_dir(&case).unwrap();
        }
        if phase == "B" && label == "memory" {
            let selected = ordinary_own_selection(&case, false);
            let mut input = bundle.input(role, selected.request, None, None);
            input.test.close_owned_before_placement = true;
            let mut early = OrdinaryRun::start(input);
            early.expected_server_close = true;
            early.supervisor.paused.store(true, Ordering::SeqCst);
            assert!(
                early.until(|run| run.supervisor.snapshot().ready),
                "original recovery offer before placement"
            );
            early.connect(&bundle, false, "OrdinaryOwner");
            assert!(
                early
                    .until(|run| run.supervisor.finished()
                        || run.supervisor.snapshot().blocked.is_some())
                    && early.supervisor.finished(),
                "published recovery must close coherently before placement: {:?}",
                early.supervisor.snapshot()
            );
            assert!(early.finish().is_some());
            let manifest: serde_json::Value = serde_json::from_slice(
                &fs::read(
                    ordinary_own_archive(&case).join("epoch-00000000000000000002/manifest.json"),
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(
                manifest["kind"],
                "horizon-owned-unplaced-session-checkpoint"
            );
            eprintln!("ORDINARY_OWN_PUBLISHED_PRE_PLACEMENT_COHERENT_CLOSE_PASS");
        }
        let selection = ordinary_own_selection(&case, phase == "A");
        assert_eq!(selection.anchor, case.join("saves/Owned"));
        if phase == "A" {
            assert!(matches!(&selection.request, OrdinaryRequest::Native(_)));
        } else {
            assert!(
                matches!(&selection.request, OrdinaryRequest::RecoverOwned(_)),
                "cold OWN has no old completion/claim"
            );
        }
        let mut run = OrdinaryRun::start(bundle.input(role, selection.request, None, None));
        run.supervisor.paused.store(true, Ordering::SeqCst);
        assert!(
            run.until(|run| run.supervisor.snapshot().ready),
            "ordinary OWN {phase}/{label} startup: {:?}",
            run.supervisor.snapshot()
        );
        let tcp = role == execution_role::ExecutionRole::DedicatedServer;
        run.connect(
            &bundle,
            tcp,
            if tcp { "OrdinaryA" } else { "OrdinaryOwner" },
        );
        if tcp {
            run.connect(&bundle, true, "OrdinaryB");
        }
        assert!(
            run.until(|run| run
                .clients
                .iter()
                .all(|client| client.login && client.terrain)
                && run.supervisor.snapshot().owned_editable),
            "actual OWN native population {phase}/{label}: {:?}",
            run.supervisor.snapshot()
        );
        if phase == "A" {
            let stone = bundle
                .startup
                .sync()
                .builtin
                .get("minecraft:item")
                .unwrap()
                .id("minecraft:stone")
                .unwrap();
            for (index, client) in run.clients.iter().enumerate() {
                send(
                    &client.session,
                    S::SetCreativeModeSlot(play::SetCreativeModeSlot {
                        slot: 36,
                        item: Some(horizon_protocol::item::Stack::new(
                            stone,
                            if index == 0 { 9 } else { 4 },
                        )),
                    }),
                );
            }
            assert!(
                run.until(|run| {
                    let view = run.supervisor.snapshot();
                    view.players.len() == (if tcp { 2 } else { 1 })
                        && view.players.iter().all(|player| {
                            let expected = if player["uuid"]
                                == format!(
                                    "{:032x}",
                                    offline_uuid(if tcp { "OrdinaryA" } else { "OrdinaryOwner" })
                                ) {
                                9
                            } else {
                                4
                            };
                            player["items"] == expected
                        })
                }),
                "legal actual configured stone item creation {label}"
            );
        }
        let observed = ordinary_own_observation(&run.supervisor.snapshot());
        if phase == "B" {
            let expected: serde_json::Value =
                serde_json::from_slice(&fs::read(case.join("A-observation.json")).unwrap())
                    .unwrap();
            assert_eq!(
                observed, expected,
                "complete native player/world state after real OWN dispatch {label}"
            );
        }
        let _closed = run
            .finish()
            .expect("actual ordinary coherent consuming close");
        let archive = ordinary_own_archive(&case);
        let epoch = if phase == "A" {
            1
        } else if label == "memory" {
            3
        } else {
            2
        };
        let image = archive.join(format!("epoch-{epoch:020}"));
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(image.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(
            manifest["kind"],
            "horizon-owned-unplaced-session-checkpoint"
        );
        assert_eq!(manifest["saved_clock"], observed["clock"]);
        assert_eq!(
            manifest["players"].as_array().unwrap().len(),
            if tcp { 2 } else { 1 }
        );
        assert!(
            manifest.get("counter").is_none()
                && manifest.get("slot").is_none()
                && manifest.get("target").is_none()
                && !image.join("host").exists()
        );
        if phase == "A" {
            fs::write(
                case.join("A-observation.json"),
                serde_json::to_vec_pretty(&observed).unwrap(),
            )
            .unwrap();
        } else {
            assert!(
                archive
                    .join("epoch-00000000000000000001/manifest.json")
                    .is_file()
            );
        }
        eprintln!(
            "ORDINARY_OWN_{phase}_CASE={label} complete_epoch={epoch} real_native_players={}",
            if tcp { 2 } else { 1 }
        );
    }
    let partial = root.join("partial");
    if phase == "A" {
        fs::create_dir(&partial).unwrap();
        let selection = ordinary_own_selection(&partial, true);
        let anchor = selection.anchor;
        let mut run = OrdinaryRun::start(bundle.input(
            execution_role::ExecutionRole::DedicatedServer,
            selection.request,
            None,
            None,
        ));
        run.supervisor.paused.store(true, Ordering::SeqCst);
        assert!(run.until(|run| run.supervisor.snapshot().ready));
        run.connect(&bundle, true, "OrdinaryA");
        assert!(run.until(|run| run.clients[0].login
            && run.clients[0].terrain
            && run.supervisor.snapshot().players.len() == 1
            && run.supervisor.snapshot().players[0]["loaded"] == true));
        assert!(
            !run.supervisor.snapshot().owned_editable,
            "one of two required players is not editable OWN publication"
        );
        let original = Arc::new(
            run.finish()
                .expect("actual original-new partial native cancellation"),
        );
        assert!(
            fs::read_dir(ordinary_own_archive(&partial))
                .unwrap()
                .next()
                .is_none(),
            "native cancellation did not invent a partial complete image"
        );
        super::super::window_close::ordinary_owned_dispatch_refusal_controls(
            &partial, &anchor, original,
        );
        eprintln!("ORDINARY_OWN_PARTIAL_NATIVE_CANCEL_AND_DISPATCH_REFUSALS_PASS");
    } else {
        let before = ordinary_tree(&partial.join("saves/Owned"));
        let selection = ordinary_own_selection(&partial, false);
        assert!(matches!(
            &selection.request,
            OrdinaryRequest::RecoverOwned(_)
        ));
        let mut refused = OrdinaryRun::start(bundle.input(
            execution_role::ExecutionRole::DedicatedServer,
            selection.request,
            None,
            None,
        ));
        let (error, closed) = refused.finish_refusal("RecoveryClosing");
        assert!(error.contains("no complete owned epoch"), "{error}");
        assert!(closed.is_none());
        assert_eq!(
            ordinary_tree(&partial.join("saves/Owned")),
            before,
            "zero complete epochs preserve original native partial world"
        );
        assert!(
            fs::read_dir(ordinary_own_archive(&partial))
                .unwrap()
                .next()
                .is_none()
        );
        eprintln!("ORDINARY_OWN_PARTIAL_ZERO_EPOCH_REFUSAL_PRESERVED_PASS");
    }
    let exe = std::env::current_exe().unwrap();
    fs::write(root.join(format!("{phase}.json")),serde_json::to_vec_pretty(&serde_json::json!({"pid":std::process::id(),"executable":format!("{:x}",Sha256::digest(fs::read(exe).unwrap())),"completed":true})).unwrap()).unwrap();
    eprintln!("ORDINARY_OWN_CHILD_{phase}_PASS");
    std::mem::forget(bundle);
}

#[cfg(all(feature = "mods", target_os = "linux"))]
#[test]
#[ignore = "isolated pinned resources and prepared pair: same ordinary worker, real configured participants, coherent source and verified recovery"]
fn ordinary_registered_domain_recovers_through_original_worker_memory_and_two_client_tcp() {
    use super::super::levels::session_checkpoint::tests::ordinary_fixture_observations_match;
    let bundle = OrdinaryBundle::new("coherent");
    for (label, role, kind) in [
        (
            "native-memory",
            execution_role::ExecutionRole::IntegratedServer,
            OrdinaryDomainKind::Native,
        ),
        (
            "guest-memory",
            execution_role::ExecutionRole::IntegratedServer,
            OrdinaryDomainKind::Guest,
        ),
        (
            "native-tcp",
            execution_role::ExecutionRole::DedicatedServer,
            OrdinaryDomainKind::Native,
        ),
        (
            "guest-tcp",
            execution_role::ExecutionRole::DedicatedServer,
            OrdinaryDomainKind::Guest,
        ),
    ] {
        let control = bundle.control(label, kind);
        let root = control.archive.parent().unwrap().join("source-world");
        let mut request =
            world_loading::WorldRequest::new("unused", Some(label), Some(17), true, None)
                .unwrap()
                .with_identity_intent(
                    world_loading::host_identity::IdentityIntent::CreateNewIdentity,
                )
                .with_session_authority(control.namespace.clone());
        request.save_root = root.clone();
        let mut source = OrdinaryRun::start(bundle.input(
            role,
            OrdinaryRequest::Native(request),
            Some(control.clone()),
            Some(control.archive.clone()),
        ));
        assert!(
            source.until(|run| run.supervisor.snapshot().ready),
            "source startup {label}: {:?}",
            source.supervisor.snapshot()
        );
        let tcp = role == execution_role::ExecutionRole::DedicatedServer;
        source.connect(
            &bundle,
            tcp,
            if tcp { "OrdinaryA" } else { "OrdinaryOwner" },
        );
        if tcp {
            source.connect(&bundle, true, "OrdinaryB");
        }
        assert!(
            source.until(|run| run
                .clients
                .iter()
                .all(|client| client.login && client.terrain)
                && run
                    .supervisor
                    .snapshot()
                    .players
                    .iter()
                    .all(|player| player["loaded"] == true)
                && run.supervisor.snapshot().installed == if tcp { 2 } else { 1 }),
            "actual source participants {label}"
        );
        control.capture_requested.store(true, Ordering::SeqCst);
        assert!(
            source.until(|_| control.captured_once.load(Ordering::SeqCst)),
            "actual domain capture {label}"
        );
        let captured = control
            .source
            .lock()
            .unwrap()
            .take()
            .expect("actual captured source owner");
        let closed = source.finish().expect("actual coherent source completion");
        assert!(!GameRules::load(&root).bool("spawn_mobs"));
        super::super::levels::session_checkpoint::tests::verify_ordinary_fixture_final_players(
            &root,
            &captured.observation,
        )
        .unwrap();
        assert!(control.archive.join(format!("epoch-{:020}", 1)).is_dir());
        let destination = root.parent().unwrap().join("recovered");
        let recovery = captured
            .source
            .into_request(
                closed,
                destination.clone(),
                Instant::now() + Duration::from_secs(120),
            )
            .unwrap_or_else(|(_, _, reason)| panic!("actual coherent association: {reason}"));
        let mut recovered = OrdinaryRun::start(bundle.input(
            role,
            OrdinaryRequest::Recover(Box::new(recovery)),
            Some(control.clone()),
            None,
        ));
        assert!(
            recovered.until(|run| run.supervisor.snapshot().ready),
            "recovery admission {label}: {:?}",
            recovered.supervisor.snapshot()
        );
        recovered.connect(
            &bundle,
            tcp,
            if tcp { "OrdinaryA" } else { "OrdinaryOwner" },
        );
        if tcp {
            recovered.connect(&bundle, true, "OrdinaryB");
        }
        assert!(
            recovered.until(|run| run
                .clients
                .iter()
                .all(|client| client.login && client.terrain)
                && run.supervisor.snapshot().installed == if tcp { 2 } else { 1 }),
            "actual recovered participants {label}"
        );
        let observation = control
            .recovered
            .lock()
            .unwrap()
            .take()
            .expect("observed only after actual checked recovery and publication");
        assert!(
            ordinary_fixture_observations_match(&captured.observation, &observation),
            "exact counter/slot/player/clock recovery {label}"
        );
        assert!(
            recovered.finish().is_some(),
            "actual recovered continuation completion {label}"
        );
        assert!(!GameRules::load(&destination).bool("spawn_mobs"));
        assert!(
            control
                .continuation
                .join(format!("epoch-{:020}", 1))
                .is_dir()
        );
        let lease = control
            .namespace
            .acquire_recovery(
                &destination,
                &control.continuation,
                &root.parent().unwrap().join("released-check"),
            )
            .unwrap();
        drop(lease);
        eprintln!(
            "ORDINARY_COHERENT_CASE={label} actual_configured_players={} exact_native_and_domain_recovery=true",
            if tcp { 2 } else { 1 }
        );
    }
}

#[cfg(all(feature = "mods", target_os = "linux"))]
#[test]
#[ignore = "isolated pinned resources and prepared pair: actual native completion claim, original refused preparation and checked adoption"]
fn ordinary_native_reopen_claim_is_unique_retained_on_refusal_and_spent_on_adoption() {
    use world_loading::host_identity::IdentityIntent;
    let bundle = OrdinaryBundle::new("native-claim");
    let root = bundle.scratch.join("native-world");
    let mut fresh =
        world_loading::WorldRequest::new("unused", Some("Native own world"), Some(17), true, None)
            .unwrap()
            .with_identity_intent(IdentityIntent::CreateNewIdentity);
    fresh.save_root = root.clone();
    let role = execution_role::ExecutionRole::DedicatedServer;
    let source_input = bundle.input(role, OrdinaryRequest::Native(fresh), None, None);
    // Exercise the actual native properties parser and captured ordinary
    // dedicated projection; the internal worker fixture does not run preflight.
    let preflight = bundle.scratch.join("native-dedicated-preflight");
    let universe = preflight.join("universe");
    fs::create_dir_all(&universe).unwrap();
    for (disabled, accepted) in [("", true), (" , ", true), ("example:disabled", false)] {
        let properties = preflight.join("server.properties");
        fs::write(
            &properties,
            format!(
                "online-mode=false\ngamemode=creative\nmax-players=2\nview-distance=2\nsimulation-distance=2\nspawn-protection=0\nwhite-list=false\npause-when-empty-seconds=0\nserver-ip=127.0.0.1\nlevel-seed=17\ninitial-disabled-packs={disabled}\n"
            ),
        )
        .unwrap();
        let props = horizon_client::server::properties::ServerProperties::from_file(
            &properties,
            &mut horizon_client::server::properties::SystemEntropy::default(),
        )
        .unwrap();
        if disabled.is_empty() {
            assert_eq!(props.initial_disabled_packs, [String::new()]);
        }
        let setup = super::super::dedicated::RetainedSetup {
            props,
            universe: universe.clone(),
            level_name: "fresh".into(),
            port: Some(0),
            started: Instant::now(),
            working_directory: preflight.clone(),
        };
        let projected = setup.retained_world_inputs(&source_input.options);
        if accepted {
            let (request, offline) = projected.expect("native empty disabled-pack selection");
            assert_eq!(request.save_root, universe.join("fresh"));
            assert_eq!(offline.max_players, 2);
        } else {
            assert!(
                projected.is_err(),
                "named disabled pack must remain refused"
            );
        }
        assert!(
            !universe.join("fresh").exists(),
            "projection cannot acquire a world"
        );
    }
    let mut source = OrdinaryRun::start(source_input);
    assert!(source.until(|run| run.supervisor.snapshot().ready));
    let original = Arc::new(
        source
            .finish()
            .expect("actual original NativeReady disposal"),
    );
    let rules_path = root.join("data/minecraft/game_rules.dat");
    let rules_before = fs::read(&rules_path).unwrap();
    let level_before = fs::read(root.join("level.dat")).unwrap();
    assert!(!GameRules::load(&root).bool("spawn_mobs"));
    assert_eq!(Level::read(&root).unwrap().game_rules["spawn_mobs"], 0);
    let request = || {
        let mut request = world_loading::WorldRequest::new("unused", None, None, true, None)
            .unwrap()
            .with_identity_intent(IdentityIntent::RequireExistingIdentity);
        request.save_root = root.clone();
        request
    };
    for missing in [false, true] {
        let mut conflicting = GameRules::load(&root);
        assert!(conflicting.set_string("spawn_mobs", "true"));
        let mut tag = conflicting.to_nbt().tag;
        if missing {
            let horizon_world::nbt::Tag::Compound(values) = &mut tag else {
                panic!("native rule compound");
            };
            let Some(horizon_world::nbt::Tag::Compound(data)) = values.get_mut(&"data".into())
            else {
                panic!("native rule data compound");
            };
            assert!(data.remove(&"minecraft:spawn_mobs".into()).is_some());
        }
        write_dat(&rules_path, tag).unwrap();
        let conflicting_bytes = fs::read(&rules_path).unwrap();
        assert!(GameRules::load(&root).bool("spawn_mobs"));
        let invalid = request().with_ordinary_reopen(original.clone()).unwrap();
        let OrdinaryServerInput {
            options,
            settings,
            request: invalid,
            entry,
            resources,
            pack,
            startup,
            ..
        } = bundle.input(role, OrdinaryRequest::Native(invalid), None, None);
        let OrdinaryRequest::Native(invalid) = invalid else {
            unreachable!()
        };
        let mut refused = world_loading::OrdinaryPreparation::prepare(
            invalid, &resources, &pack, &options, &settings, &entry, startup,
        );
        assert!(
            refused
                .original_error()
                .unwrap()
                .contains("spawn_mobs=false")
        );
        assert!(request().with_ordinary_reopen(original.clone()).is_err());
        refused.begin_close(Instant::now() + Duration::from_secs(120));
        loop {
            match refused.poll_close() {
                PreparationCloseStatus::Pending => std::thread::yield_now(),
                PreparationCloseStatus::Complete => break,
                PreparationCloseStatus::Blocked(reason) => {
                    panic!("conflicting-rule original close blocked: {reason}")
                }
            }
        }
        refused
            .finish_close()
            .unwrap_or_else(|_| panic!("conflicting-rule checked close refused"));
        assert_eq!(fs::read(&rules_path).unwrap(), conflicting_bytes);
        assert_eq!(fs::read(root.join("level.dat")).unwrap(), level_before);
        drop(request().with_ordinary_reopen(original.clone()).unwrap());
        fs::write(&rules_path, &rules_before).unwrap();
    }
    assert!(
        request().check_ordinary_native_entry().is_err(),
        "an identity file is not an own-world completion"
    );
    let first = request().with_ordinary_reopen(original.clone()).unwrap();
    first.check_ordinary_native_entry().unwrap();
    first.check_ordinary_native_entry().unwrap();
    assert!(
        request().with_ordinary_reopen(original.clone()).is_err(),
        "old Arc aliases cannot issue a second request"
    );
    let OrdinaryServerInput {
        options,
        settings,
        request: first,
        entry,
        resources,
        pack,
        gui,
        cpu,
        tree,
        startup,
        ..
    } = bundle.input(role, OrdinaryRequest::Native(first), None, None);
    let activated = bundle
        .startup
        .activate(role, false, false, &settings)
        .unwrap_or_else(|refused| panic!("original runtime activation: {}", refused.reason));
    let OrdinaryRequest::Native(first) = first else {
        unreachable!()
    };
    let prepared = world_loading::OrdinaryPreparation::prepare(
        first,
        &resources,
        &pack,
        &options,
        &settings,
        &entry,
        startup.clone(),
    );
    let wrong = RetainedEntryIdentity {
        token: entry.token,
        owner: Arc::new(()),
    };
    let mut refused = match prepared.into_prepared(&wrong) {
        Err(original) => original,
        Ok(_) => panic!("different entry allocation adopted the original preparation"),
    };
    assert!(
        request().with_ordinary_reopen(original.clone()).is_err(),
        "same original claim stays with refused native acquisition"
    );
    refused.begin_close(Instant::now() + Duration::from_secs(120));
    loop {
        match refused.poll_close() {
            PreparationCloseStatus::Pending => std::thread::yield_now(),
            PreparationCloseStatus::Complete => break,
            PreparationCloseStatus::Blocked(reason) => {
                panic!("original preparation close blocked: {reason}")
            }
        }
    }
    assert!(
        request().with_ordinary_reopen(original.clone()).is_err(),
        "status is not consuming completion"
    );
    let _closed = refused
        .finish_close()
        .unwrap_or_else(|_| panic!("actual closed preparation refused finish"));
    let second = request()
        .with_ordinary_reopen(original.clone())
        .expect("same claim returned only after original checked close");
    let prepared = world_loading::OrdinaryPreparation::prepare(
        second,
        &resources,
        &pack,
        &options,
        &settings,
        &entry,
        startup.clone(),
    );
    let prepared = prepared
        .into_prepared(&entry)
        .unwrap_or_else(|_| panic!("original entry association refused"));
    assert_eq!(prepared.prepared().level.game_rules["spawn_mobs"], 0);
    assert_eq!(fs::read(&rules_path).unwrap(), rules_before);
    let mut game = Box::new(
        Game::new_with_world(
            &bundle.resources,
            pack,
            gui,
            options,
            settings,
            Audio::silent(&bundle.root),
            Some(cpu),
            InitialGameWorld::Ordinary(Box::new(prepared)),
        )
        .unwrap(),
    );
    game.adopt_activated_startup(activated)
        .unwrap_or_else(|original| panic!("same original runtime transfer: {}", original.1));
    game.load_world_state();
    assert!(!game.ui.rules.bool("spawn_mobs"));
    game.adv.tree = tree;
    assert!(
        request().with_ordinary_reopen(original.clone()).is_err(),
        "adoption spends old aliases permanently"
    );
    game.retained_server.as_mut().unwrap().freeze();
    let mut owner = CloseOwner::begin(
        game,
        CloseProfile::NativeWorld,
        Instant::now() + Duration::from_secs(120),
    );
    loop {
        match owner.poll() {
            CloseStatus::Pending(_) => std::thread::yield_now(),
            CloseStatus::NativeReady => break,
            CloseStatus::Blocked(reason) => panic!("actual reopened native close: {reason}"),
        }
    }
    let ready = owner
        .try_native_ready()
        .unwrap_or_else(|_| panic!("actual NativeReady transfer refused"));
    let mut disposed = ready
        .try_dispose_server()
        .unwrap_or_else(|(_, reason)| panic!("actual native disposal: {reason}"));
    let next = Arc::new(
        disposed
            .take_ordinary_world()
            .expect("new actual completion"),
    );
    assert!(!GameRules::load(&root).bool("spawn_mobs"));
    assert!(
        request().with_ordinary_reopen(original).is_err(),
        "later completion cannot revive spent aliases"
    );
    assert!(
        request().with_ordinary_reopen(next).is_ok(),
        "only the newly completed owner can reopen again"
    );
}

#[cfg(all(
    feature = "mods",
    target_os = "linux",
    feature = "native-writer-test-control",
    debug_assertions
))]
fn ordinary_tree(root: &std::path::Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn visit(
        root: &std::path::Path,
        at: &std::path::Path,
        files: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>,
    ) {
        for entry in fs::read_dir(at).unwrap() {
            let path = entry.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            assert!(!metadata.is_symlink());
            if metadata.is_dir() {
                visit(root, &path, files);
            } else {
                assert!(metadata.is_file());
                files.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut files = std::collections::BTreeMap::new();
    visit(root, root, &mut files);
    files
}

#[cfg(all(
    feature = "mods",
    target_os = "linux",
    feature = "native-writer-test-control",
    debug_assertions
))]
fn ordinary_refusal_source(
    bundle: &OrdinaryBundle,
    label: &str,
) -> (Arc<OrdinaryControl>, OrdinaryRequest, PathBuf, PathBuf) {
    let control = bundle.control(label, OrdinaryDomainKind::Native);
    let root = control.archive.parent().unwrap().join("source-world");
    let mut request = world_loading::WorldRequest::new("unused", Some(label), Some(17), true, None)
        .unwrap()
        .with_identity_intent(world_loading::host_identity::IdentityIntent::CreateNewIdentity)
        .with_session_authority(control.namespace.clone());
    request.save_root = root.clone();
    let role = execution_role::ExecutionRole::DedicatedServer;
    let mut source = OrdinaryRun::start(bundle.input(
        role,
        OrdinaryRequest::Native(request),
        Some(control.clone()),
        Some(control.archive.clone()),
    ));
    assert!(source.until(|run| run.supervisor.snapshot().ready));
    source.connect(bundle, true, "OrdinaryA");
    source.connect(bundle, true, "OrdinaryB");
    assert!(source.until(|run| {
        run.clients
            .iter()
            .all(|client| client.login && client.terrain)
            && run.supervisor.snapshot().installed == 2
            && run
                .supervisor
                .snapshot()
                .players
                .iter()
                .all(|player| player["loaded"] == true)
    }));
    control.capture_requested.store(true, Ordering::SeqCst);
    assert!(source.until(|_| control.captured_once.load(Ordering::SeqCst)));
    let captured = control.source.lock().unwrap().take().unwrap();
    let closed = source
        .finish()
        .expect("actual coherent source native completion");
    super::super::levels::session_checkpoint::tests::verify_ordinary_fixture_final_players(
        &root,
        &captured.observation,
    )
    .unwrap();
    let destination = root.parent().unwrap().join("recovered");
    let request = captured
        .source
        .into_request(
            closed,
            destination.clone(),
            Instant::now() + Duration::from_secs(120),
        )
        .unwrap_or_else(|(_, _, error)| panic!("actual refusal source association: {error}"));
    (
        control,
        OrdinaryRequest::Recover(Box::new(request)),
        root,
        destination,
    )
}

#[cfg(all(
    feature = "mods",
    target_os = "linux",
    feature = "native-writer-test-control",
    debug_assertions
))]
#[test]
#[ignore = "isolated pinned resources and prepared pair: actual partial recovery cancel/wrong participant, original writer deadline and exact Retry without publication"]
fn ordinary_recovery_refusals_and_original_writer_retry_preserve_unpublished_owners() {
    let bundle = OrdinaryBundle::new("recovery-refusals");
    for (label, wrong, blocked) in [
        ("cancel", false, false),
        ("wrong", true, false),
        ("writer-cancel", false, true),
    ] {
        let (control, request, source, destination) = ordinary_refusal_source(&bundle, label);
        let source_before = ordinary_tree(&source);
        let archive_before = ordinary_tree(&control.archive);
        let writer = Arc::new(OrdinaryRecoveryWriter::default());
        let mut input = bundle.input(
            execution_role::ExecutionRole::DedicatedServer,
            request,
            Some(control.clone()),
            None,
        );
        if blocked {
            input.test.ordinary_recovery_writer = Some(writer.clone());
        }
        let mut recovered = OrdinaryRun::start(input);
        // Declared after the original run: root releases the actual batch and
        // retries an already-blocked attempt before the run's fatal-drop guard.
        let _release = OrdinaryWriterRelease {
            writer: writer.clone(),
            commands: recovered.supervisor.commands.clone(),
            view: recovered.supervisor.view.clone(),
        };
        assert!(
            recovered.until(|run| run.supervisor.snapshot().phase == "RecoveryJoining"
                && run.supervisor.snapshot().ready)
        );
        recovered.connect(&bundle, true, "OrdinaryA");
        assert!(recovered.until(|run| run.supervisor.snapshot().guests == 1
            && run.clients[0].admission.is_some()
            && !run.publication_seen));
        assert!(control.recovered.lock().unwrap().is_none());
        if blocked {
            let entered = {
                let mut owned = writer.control.lock().unwrap();
                owned
                    .as_mut()
                    .map(|control| control.wait_entered(Duration::from_secs(10)))
            };
            entered.expect("original writer receipt").unwrap();
        }
        recovered.closing = true;
        let expected = if wrong {
            recovered.connect(&bundle, true, "WrongRecoveryB");
            "session checkpoint NotPublished (0 item identities assigned): checked recovery profile missing from original Guests"
        } else {
            recovered.supervisor.request_close();
            "ordinary recovery cancelled before verified activation"
        };
        if blocked {
            assert!(recovered.until(|run| run.supervisor.snapshot().blocked.is_some()));
            let view = recovered.supervisor.snapshot();
            assert_eq!(view.phase, "RecoveryClosing");
            assert_eq!(view.original_error.as_deref(), Some(expected));
            assert!(
                view.blocked.as_deref().unwrap().contains("writer"),
                "{view:?}"
            );
            assert!(!view.worker_joined && !recovered.supervisor.finished());
            assert!(
                writer
                    .original
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .upgrade()
                    .is_some()
            );
            assert!(
                control
                    .namespace
                    .acquire_recovery(
                        &source,
                        &control.archive,
                        &source.parent().unwrap().join("still-owned")
                    )
                    .is_err()
            );
            recovered
                .supervisor
                .commands
                .send(Command::Retry(view.attempt + 1))
                .unwrap();
            // The close loop handles at most16 commands before publishing its
            // next View. These ordered refused offers prove the stale command
            // was consumed and its effect published, without a timing guess.
            for _ in 0..17 {
                let (reply, receive) = mpsc::channel();
                recovered
                    .supervisor
                    .commands
                    .send(Command::Offer(reply))
                    .unwrap();
                match receive.recv_timeout(Duration::from_secs(10)).unwrap() {
                    Err(reason) => assert_eq!(reason, "original recovery is closing"),
                    Ok(pipe) => {
                        pipe.shutdown();
                        panic!("closing recovery unexpectedly published a Memory offer");
                    }
                }
            }
            recovered.pump();
            let stale = recovered.supervisor.snapshot();
            assert_eq!(stale.attempt, view.attempt);
            assert_eq!(stale.original_error, view.original_error);
            assert_eq!(stale.blocked, view.blocked);
            let triggered = {
                let mut owned = writer.control.lock().unwrap();
                owned.as_mut().map(|control| control.trigger())
            };
            triggered.expect("same original writer receipt").unwrap();
            drop(writer.control.lock().unwrap().take());
            recovered
                .supervisor
                .commands
                .send(Command::Retry(view.attempt))
                .unwrap();
            assert!(
                recovered.until(|run| run.supervisor.finished()),
                "exact original retry: {:?}",
                recovered.supervisor.snapshot()
            );
            assert_eq!(recovered.supervisor.snapshot().attempt, view.attempt + 1);
        }
        let (error, completion) = recovered.finish_refusal("RecoveryClosing");
        assert_eq!(error, expected);
        assert!(completion.is_none() && !recovered.publication_seen);
        assert!(control.recovered.lock().unwrap().is_none());
        assert!(
            fs::read_dir(&control.continuation)
                .unwrap()
                .next()
                .is_none()
        );
        assert_eq!(ordinary_tree(&source), source_before);
        assert_eq!(ordinary_tree(&control.archive), archive_before);
        if blocked {
            assert!(
                writer
                    .original
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .upgrade()
                    .is_none()
            );
        }
        let lease = control
            .namespace
            .acquire_recovery(
                &source,
                &control.archive,
                &source.parent().unwrap().join("released-check"),
            )
            .unwrap();
        drop(lease);
        assert!(!GameRules::load(&destination).bool("spawn_mobs"));
        eprintln!(
            "ORDINARY_RECOVERY_REFUSAL={label} original_error_preserved=true unpublished=true actual_writer_block={blocked}"
        );
    }
}

#[cfg(all(feature = "mods", target_os = "linux"))]
#[test]
#[ignore = "isolated pinned resources and prepared pair: actual ordinary pending/untouched integrated native owner preserves original no-position record"]
fn ordinary_initial_pending_and_untouched_owner_keep_original_native_record() {
    let bundle = OrdinaryBundle::new("initial-owners");
    for untouched in [false, true] {
        let label = if untouched { "untouched" } else { "pending" };
        let control = bundle.control(label, OrdinaryDomainKind::Native);
        let root = control.archive.parent().unwrap().join("world");
        let mut request =
            world_loading::WorldRequest::new("unused", Some(label), Some(17), true, None)
                .unwrap()
                .with_identity_intent(
                    world_loading::host_identity::IdentityIntent::CreateNewIdentity,
                )
                .with_session_authority(control.namespace.clone());
        request.save_root = root.clone();
        let mut input = bundle.input(
            execution_role::ExecutionRole::IntegratedServer,
            OrdinaryRequest::Native(request),
            None,
            None,
        );
        input.test.stop_on_untouched_join = untouched;
        let mut run = OrdinaryRun::start(input);
        assert!(run.until(|run| run.supervisor.snapshot().ready));
        let record = Level::read(&root).unwrap().player_uuid.unwrap();
        let uuid = record
            .iter()
            .fold(0u128, |id, part| (id << 32) | u128::from(*part as u32));
        let path = horizon_client::lan::playerdata::path(&root, uuid);
        let original = fs::read(&path).unwrap();
        assert!(
            read_dat_checked(&path)
                .unwrap()
                .unwrap()
                .get("Pos")
                .is_none()
        );
        if untouched {
            run.closing = true;
            run.connect(&bundle, false, "OrdinaryOwner");
            let (error, completion) = run.finish_refusal("NativeDisposed");
            assert_eq!(error, "controlled untouched original Joined stop");
            assert!(completion.is_some());
            assert!(run.supervisor.snapshot().pending_events > 0);
        } else {
            assert!(run.finish().is_some());
        }
        let view = run.supervisor.snapshot();
        assert!(view.worker_joined && view.owner_saved && view.installed == 0);
        assert!(!run.publication_seen);
        assert!(view.players.iter().all(|player| player["body"] == false
            && player["loaded"] == false
            && player["placed"] == false));
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(
            read_dat_checked(&path)
                .unwrap()
                .unwrap()
                .get("Pos")
                .is_none()
        );
        assert!(fs::read_dir(&control.archive).unwrap().next().is_none());
        assert!(!GameRules::load(&root).bool("spawn_mobs"));
        eprintln!(
            "ORDINARY_INITIAL_OWNER={label} original_no_position_bytes_retained=true installed=0"
        );
    }
}

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
    pub(super) close_owned_before_placement: bool,
    #[cfg(all(feature = "mods", target_os = "linux"))]
    pub(in crate::game) ordinary: Option<Arc<OrdinaryControl>>,
    #[cfg(all(
        feature = "mods",
        target_os = "linux",
        feature = "native-writer-test-control",
        debug_assertions
    ))]
    pub(in crate::game) ordinary_recovery_writer: Option<Arc<OrdinaryRecoveryWriter>>,
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
