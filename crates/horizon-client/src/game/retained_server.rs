//! Explicit headless native Game. It owns the original worker, transport and
//! native close sequence; ordinary dedicated/integrated startup is unchanged.
use super::paired_close::{CloseOwner, CloseProfile, CloseStatus, NativeReady};
use super::world_loading::{ColdPreparation, PreparationCloseStatus, RetainedEntryIdentity};
use super::*;
use horizon_client::{
    control::{RetainedControl, RetainedControlPhase},
    lan::{
        RetainedOfflineConfig, RetainedOutcome,
        chunks::ChunkEncoder,
        hub::{
            HostEvent, Hub, HubConfig, RetainedHubCloseView, RetainedHubIdentity, RetainedHubPhase,
            RetainedHubRefusal,
        },
    },
};
use horizon_protocol::{connection::Pipe, packets::play::Serverbound};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Mutex, mpsc},
    thread::JoinHandle,
};

#[cfg(test)]
#[path = "retained_server_tests.rs"]
mod tests;

const CLOSE_WAIT: Duration = Duration::from_secs(10);

pub(super) struct Launch {
    root: PathBuf,
    args: Vec<String>,
}
impl Launch {
    pub(super) fn parse(args: &[String]) -> Result<Option<Self>> {
        if !args.iter().any(|a| a.starts_with("--retained-server")) {
            return Ok(None);
        }
        if !cfg!(target_os = "linux") || cfg!(feature = "controllers") || cfg!(feature = "audio") {
            return Err("retained server requires Linux without audio/controllers".into());
        }
        for name in [
            "HORIZON_SESSION_FILE",
            "HORIZON_PLAYER_NAME",
            "HORIZON_BENCHMARK",
            "HORIZON_COMMAND_HARNESS",
            "HORIZON_NET_HARNESS",
        ] {
            if std::env::var_os(name).is_some_and(|v| !v.is_empty()) {
                return Err(format!("retained server refuses {name}").into());
            }
        }
        let mut root = None;
        let mut ordinary = Vec::new();
        let mut input = args.iter();
        while let Some(arg) = input.next() {
            match arg.as_str() {
                "--retained-server" if root.is_none() => {
                    root = Some(PathBuf::from(
                        input.next().ok_or("missing fresh retained server root")?,
                    ))
                }
                "--resources" | "--control-socket" | "--timeout-seconds" => {
                    ordinary.push(arg.clone());
                    ordinary.push(
                        input
                            .next()
                            .ok_or("missing retained server option value")?
                            .clone(),
                    );
                }
                _ => return Err(format!("unsupported retained server option {arg}").into()),
            }
        }
        let root = root.ok_or("missing retained server root")?;
        if !root.is_absolute()
            || root
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(
                "retained server requires an absolute root without parent components".into(),
            );
        }
        match std::fs::symlink_metadata(&root) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err("retained server root must not already exist".into()),
        }
        ordinary.extend(["--in-frame".into(), "--test-terrain".into()]);
        Ok(Some(Self {
            root,
            args: ordinary,
        }))
    }
    pub(super) fn ordinary_args(&self) -> Vec<String> {
        self.args.clone()
    }
    pub(super) fn configure(&self, options: &mut Options) {
        options.execution_role = execution_role::ExecutionRole::DedicatedServer;
        options.server_thread = false;
        options.test_terrain = true;
        options.world = Some("retained-server".into());
        options.distance = 2;
        options.distance_override = Some(2);
        options.player_name = Some("RetainedServer".into());
        options
            .control
            .get_or_insert_with(|| self.root.join("control.sock"));
    }
}

/// Issued only after actual native installation on this cold-constructed Game.
/// Native enter/leave_guest moves/reboxes the body without creating a new
/// logical installation. Unsupported replacement transitions are fenced first.
struct NativeInstallation {
    construction: Arc<()>,
    uuid: u128,
}
pub(super) struct ServerGameState {
    construction: Arc<()>,
    identity: Option<RetainedHubIdentity>,
    hub_refusal: Option<Box<RetainedHubRefusal>>,
    frozen: bool,
    failure: Option<String>,
    pub(super) pending: VecDeque<HostEvent>,
    pub(super) admissions: HashMap<u32, super::lan::NativeAdmission>,
    installations: HashMap<u32, NativeInstallation>,
    unsent_pipes: Vec<Pipe>,
    #[cfg(test)]
    pub(super) test: tests::Startup,
    #[cfg(test)]
    pub(super) consume_refusal_checked: bool,
}
impl ServerGameState {
    pub(super) fn from_cold() -> Self {
        Self {
            construction: Arc::new(()),
            identity: None,
            hub_refusal: None,
            frozen: false,
            failure: None,
            pending: VecDeque::new(),
            admissions: HashMap::new(),
            installations: HashMap::new(),
            unsent_pipes: Vec::new(),
            #[cfg(test)]
            test: tests::Startup::default(),
            #[cfg(test)]
            consume_refusal_checked: false,
        }
    }
    pub(super) fn can_bind_hub(&self) -> bool {
        self.identity.is_none() && self.hub_refusal.is_none() && !self.frozen
    }
    pub(super) fn bind_hub(&mut self, identity: RetainedHubIdentity) {
        assert!(
            self.can_bind_hub(),
            "original cold server binds its Hub once"
        );
        self.identity = Some(identity);
    }
    pub(super) fn hold_hub_refusal(&mut self, input: Box<RetainedHubRefusal>) {
        self.hub_refusal = Some(input);
    }
    pub(super) fn hub_identity(&self) -> Option<&RetainedHubIdentity> {
        self.identity.as_ref()
    }
    pub(super) fn frozen(&self) -> bool {
        self.frozen
    }
    pub(super) fn fail(&mut self, detail: String) {
        self.failure.get_or_insert(detail);
        self.frozen = true;
    }
    fn freeze(&mut self) {
        self.frozen = true;
    }
    pub(super) fn check_hub(&mut self, hub: &Hub) {
        if !self
            .identity
            .as_ref()
            .is_some_and(|identity| identity.matches(hub))
        {
            self.fail("original server Hub identity changed".into());
            return;
        }
        if let Some(view) = hub.retained_close_view() {
            if let Some(failure) = view.failure {
                self.fail(failure);
            }
            if view.phase != RetainedHubPhase::Open {
                self.freeze();
            }
        } else {
            self.fail("original retained Hub missing".into());
        }
    }
    pub(super) fn check_inboxes(&mut self, hub: &Hub) {
        if hub
            .guests
            .iter()
            .any(|guest| guest.inbox.iter().any(|packet| !supported_packet(packet)))
        {
            self.fail(
                "fixed retained server refuses an unsupported play transition before dispatch"
                    .into(),
            );
        }
    }
    pub(super) fn record_installation(&mut self, conn: u32, uuid: u128) {
        assert!(
            !self.installations.contains_key(&conn),
            "native installation is single-use"
        );
        self.installations.insert(
            conn,
            NativeInstallation {
                construction: self.construction.clone(),
                uuid,
            },
        );
    }
    pub(super) fn native_installed(&self, conn: u32, uuid: u128) -> bool {
        self.installations.get(&conn).is_some_and(|receipt| {
            receipt.uuid == uuid && Arc::ptr_eq(&receipt.construction, &self.construction)
        })
    }
    pub(super) fn is_untouched(&self, conn: u32) -> bool {
        !self.admissions.contains_key(&conn)
            && !self.installations.contains_key(&conn)
            && self
                .pending
                .iter()
                .any(|event| matches!(event, HostEvent::Joined(joined) if *joined == conn))
    }
    fn incomplete(&self) -> bool {
        self.admissions
            .values()
            .any(|admission| !admission.installed())
    }
    pub(super) fn authorizes_close(&self, game: &Game<'_>) -> bool {
        self.frozen
            && !self.incomplete()
            && game.options.execution_role == execution_role::ExecutionRole::DedicatedServer
            && game.settings.render_distance == 2
            && game.settings.simulation_distance == 2
            && game.settings.horizon.lod_distance == 0
            && game.player.is_none()
            && game.levels.parked_dims().is_empty()
            && game.lod.is_none()
            && game.renderer.is_none()
            && game.window.is_none()
            && match (&self.identity, &game.lan) {
                (Some(identity), Some(host)) => {
                    identity.matches(&host.hub)
                        && host.hub.retained_close_view().is_some_and(|view| {
                            view.phase == RetainedHubPhase::Ready && view.transport.quiescent
                        })
                }
                (None, None) => true,
                _ => false,
            }
    }
}

fn supported_packet(packet: &Serverbound) -> bool {
    use Serverbound as S;
    match packet {
        S::AcceptTeleportation(_)
        | S::PlayerLoaded(_)
        | S::ChunkBatchReceived(_)
        | S::ClientInformation(_)
        | S::MovePlayerPos(_)
        | S::MovePlayerPosRot(_)
        | S::MovePlayerRot(_)
        | S::MovePlayerStatus(_)
        | S::PlayerInput(_)
        | S::PlayerAbilities(_)
        | S::ClientTickEnd(_)
        | S::KeepAlive(_)
        | S::RecipeBookChangeSettings(_) => true,
        S::PlayerCommand(command) => matches!(command.action, 1 | 2),
        S::ClientCommand(command) => command.action == 1,
        _ => false,
    }
}

impl Game<'_> {
    pub(super) fn retained_server_check_running(&self) -> Result<()> {
        if let Some(server) = &self.retained_server
            && server.frozen
        {
            return Err(server
                .failure
                .as_deref()
                .unwrap_or("original retained server frozen")
                .into());
        }
        Ok(())
    }
    fn retained_server_settle_step(&mut self) -> Result<bool> {
        self.retained_server_check_running()?;
        self.update_world();
        self.retained_server_check_running()?;
        self.postprocess_chunks()?;
        if self.spawn.is_none() {
            let Some([x, y, z]) = self.find_world_spawn()? else {
                return Ok(false);
            };
            self.spawn = Some(Vec3::new(
                f64::from(x) + 0.5,
                f64::from(y),
                f64::from(z) + 0.5,
            ));
            self.level.spawn = Some([f64::from(x), f64::from(y), f64::from(z)]);
            self.level.world_spawn = Some([x, y, z]);
        }
        let spawn = self.spawn.unwrap();
        let (cx, cz) = ((spawn.x.floor() as i32) >> 4, (spawn.z.floor() as i32) >> 4);
        self.streaming.expect_existing_mut().center = (cx, cz);
        Ok((-1..=1)
            .all(|dz| (-1..=1).all(|dx| self.streaming.expect_existing().is_lit(cx + dx, cz + dz))))
    }
}

#[derive(Clone, Debug)]
struct View {
    phase: &'static str,
    attempt: u64,
    ready: bool,
    port: u16,
    ticks: u64,
    world_time: i64,
    guests: usize,
    installed: usize,
    pending_events: usize,
    original_error: Option<String>,
    blocked: Option<String>,
    worker_joined: bool,
    players: Vec<serde_json::Value>,
    transport: Option<RetainedHubCloseView>,
    #[cfg(test)]
    consume_refusal_checked: bool,
}
impl Default for View {
    fn default() -> Self {
        Self {
            phase: "Preparing",
            attempt: 0,
            ready: false,
            port: 0,
            ticks: 0,
            world_time: 0,
            guests: 0,
            installed: 0,
            pending_events: 0,
            original_error: None,
            blocked: None,
            worker_joined: false,
            players: Vec::new(),
            transport: None,
            #[cfg(test)]
            consume_refusal_checked: false,
        }
    }
}
impl View {
    fn json(&self) -> serde_json::Value {
        serde_json::json!({"phase":self.phase,"attempt":self.attempt,"ready":self.ready,"port":self.port,
        "ticks":self.ticks,"world_time":self.world_time,"guests":self.guests,"installed":self.installed,
        "pending_events":self.pending_events,"original_error":self.original_error,"blocked":self.blocked,
        "worker_joined":self.worker_joined,"players":self.players,
        "transport":self.transport.as_ref().map(|view|serde_json::json!({
            "phase":format!("{:?}",view.phase),"attempt":view.attempt,"failure":view.failure,
            "quiescent":view.transport.quiescent,"pending_events":view.pending_events,"inbox_packets":view.inbox_packets,
            "queued_events":view.transport.queued_events,"applying_packet":view.applying_packet,
            "terminals":view.transport.terminals.iter().map(|terminal|serde_json::json!({"conn":terminal.conn,
                "actor":format!("{:?}",terminal.actor),"outcome":format!("{:?}",terminal.outcome)})).collect::<Vec<_>>(),
            "outbound":view.transport.attempts.iter().filter_map(|attempt|attempt.outbound.as_ref().map(|outbound|
                serde_json::json!({"conn":attempt.conn,"observation":format!("{outbound:?}"),
                    "queued_bodies":outbound.queued_bodies,"active_bodies":outbound.active_bodies,
                    "failure":outbound.failure.as_ref().map(|failure|format!("{failure:?}"))}))).collect::<Vec<_>>()
        }))})
    }
}

fn transport_failure(view: &RetainedHubCloseView) -> Option<String> {
    view.failure
        .clone()
        .or_else(|| {
            view.transport.attempts.iter().find_map(|attempt| {
                attempt
                    .outbound
                    .as_ref()
                    .and_then(|outbound| outbound.failure.as_ref())
                    .map(|failure| format!("original outbound {}: {failure:?}", attempt.conn))
            })
        })
        .or_else(|| {
            view.transport.terminals.iter().find_map(|terminal| {
                // These are concrete library stop/normal-return outcomes. EOF,
                // Receive, IO, panic and every other raw result remain failures,
                // even if first harvested after the deliberate close request.
                (!matches!(
                    terminal.outcome,
                    RetainedOutcome::Returned | RetainedOutcome::Closed
                ))
                .then(|| format!("original retained transport completion: {terminal:?}"))
            })
        })
}
#[cfg_attr(not(test), allow(dead_code))] // Memory offers are exercised by the fixed native fixture; default attachment remains separate.
enum Command {
    Close,
    Retry(u64),
    Offer(mpsc::Sender<std::result::Result<Pipe, String>>),
}
struct Input {
    options: Options,
    settings: Settings,
    launch: Launch,
    #[cfg(test)]
    test: tests::Startup,
}
struct Supervisor {
    thread: Option<JoinHandle<std::result::Result<(), String>>>,
    commands: mpsc::Sender<Command>,
    view: Arc<Mutex<View>>,
    _inert: Arc<Mutex<Option<Input>>>,
}
impl Supervisor {
    /// The handle is registered before the worker can acquire native owners.
    #[allow(clippy::result_large_err)] // Refusal returns the exact original inert inputs.
    fn prepare(input: Input) -> std::result::Result<Self, (Input, String)> {
        let inert = Arc::new(Mutex::new(Some(input)));
        let worker_input = inert.clone();
        let view = Arc::new(Mutex::new(View::default()));
        let worker_view = view.clone();
        let (commands, receive) = mpsc::channel();
        let (release, acquire) = mpsc::sync_channel(0);
        let handle = std::thread::Builder::new()
            .name("Retained original server".into())
            .spawn(move || {
                if acquire.recv().is_err() {
                    return Err("server acquisition latch refused".into());
                }
                let input = worker_input
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    .expect("original inert input");
                worker(input, receive, worker_view)
            });
        match handle {
            Err(error) => Err((
                inert
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    .unwrap(),
                error.to_string(),
            )),
            Ok(thread) => {
                let owner = Self {
                    thread: Some(thread),
                    commands,
                    view,
                    _inert: inert,
                };
                // A refused latch is an original finished worker result, joined
                // by the same supervisor; no acquisition has taken place.
                let _ = release.send(());
                Ok(owner)
            }
        }
    }
    fn snapshot(&self) -> View {
        self.view.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn finished(&self) -> bool {
        self.thread.as_ref().is_some_and(JoinHandle::is_finished)
    }
    fn join(&mut self) -> Option<std::result::Result<(), String>> {
        if !self.finished() {
            return None;
        }
        let result = self
            .thread
            .take()
            .unwrap()
            .join()
            .unwrap_or_else(|_| Err("original server worker panicked".into()));
        self.view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .worker_joined = true;
        Some(result)
    }
}
impl Drop for Supervisor {
    fn drop(&mut self) {
        if self.thread.is_some() {
            eprintln!("fatal: unresolved original server worker");
            std::process::abort();
        }
    }
}

struct HubInputs {
    sync: Arc<horizon_client::lan::data::SyncData>,
    encoder: ChunkEncoder,
    config: HubConfig,
    offline: RetainedOfflineConfig,
}
fn hub_inputs(
    resources: &Resources,
    pack: &PackResources,
    root: &std::path::Path,
) -> Result<HubInputs> {
    let sync = super::lan::sync_data(root)?;
    let biomes = resources
        .biomes()
        .list
        .iter()
        .map(|b| b.name.clone())
        .collect::<Vec<_>>();
    let registry = sync
        .dynamic
        .registries
        .get("minecraft:worldgen/biome")
        .ok_or("biome registry missing")?
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    let mut encoder = ChunkEncoder::new(
        resources.facts().states.iter().map(|state| {
            (
                state.name.clone(),
                state.fluid_level > 0 || state.waterlogged,
            )
        }),
        &sync.builtin,
        &biomes,
        &registry,
    );
    encoder.tables = Some(Arc::new(horizon_client::save::SaveTables::new(
        &pack.assets,
    )));
    Ok(HubInputs {
        sync,
        encoder,
        config: HubConfig {
            default_game_mode: 1,
            guest_commands: false,
            force_game_mode: false,
            hardcore: false,
            view_distance: 2,
            simulation_distance: 2,
            dimension: "minecraft:overworld".into(),
            seed: 17,
            max_players: 2,
            dedicated: true,
            integrated: false,
        },
        offline: RetainedOfflineConfig {
            bind: Some(std::net::Ipv4Addr::LOCALHOST.into()),
            port: 0,
            motd: "Retained original server".into(),
            max_players: 2,
            compression_threshold: -1,
            enable_status: true,
            hide_online_players: false,
            announce: false,
        },
    })
}

fn worker(
    input: Input,
    commands: mpsc::Receiver<Command>,
    view: Arc<Mutex<View>>,
) -> std::result::Result<(), String> {
    let Input {
        options,
        settings,
        launch,
        #[cfg(test)]
        test,
    } = input;
    // These operations precede native acquisition. Every complete resource
    // allocation stays lexical until Game and all dependent native actors close.
    let root = std::fs::canonicalize(&options.resources).map_err(|e| e.to_string())?;
    let loaded = load_resources(
        root.clone(),
        &root,
        &settings,
        Instant::now(),
        false,
        &Progress::new(),
        &mut |_, _| {},
    )
    .map_err(|e| e.to_string())?;
    if loaded.rest_loader.is_some() {
        eprintln!("fatal: synchronous retained server resources unexpectedly started a helper");
        std::process::abort();
    }
    if loaded.pack_error.is_some() || loaded.root != root {
        return Err("retained server requires complete synchronous resources".into());
    }
    let gui = loaded
        .gui
        .ok_or("retained server GUI/font inputs missing")?;
    let cpu = GameCpuResources::load(&root).map_err(|e| e.to_string())?;
    let tree = Arc::new(horizon_client::advancements::Tree::load(&root.join("data")));
    if tree.list.is_empty() {
        return Err("retained server advancement resource input missing".into());
    }
    let resources = loaded.resources;
    let inputs = hub_inputs(&resources, &loaded.pack, &root).map_err(|e| e.to_string())?;
    let mut stop_before_acquisition = horizon_threads::stop_requests() > 0;
    for command in commands.try_iter().take(16) {
        match command {
            Command::Close => stop_before_acquisition = true,
            Command::Offer(reply) => {
                let _ = reply.send(Err("original native server has not been acquired".into()));
            }
            Command::Retry(_) => {}
        }
    }
    if stop_before_acquisition {
        view.lock().unwrap_or_else(|e| e.into_inner()).phase = "UnacquiredClosed";
        return Ok(());
    }
    let mut request = world_loading::WorldRequest::new(
        "retained-server",
        Some("Retained original server"),
        Some(17),
        true,
        None,
    )
    .map_err(|e| e.to_string())?
    .with_identity_intent(world_loading::host_identity::IdentityIntent::CreateNewIdentity);
    request.save_root = launch.root.join("world");
    let entry = RetainedEntryIdentity {
        token: 1,
        owner: Arc::new(()),
    };
    #[cfg(not(test))]
    let cold = ColdPreparation::prepare_server(
        request,
        &resources,
        &loaded.pack,
        &options,
        &settings,
        &entry,
    );
    #[cfg(test)]
    let cold = if test.fail_after_streaming {
        ColdPreparation::prepare_server_failure_for_test(
            request,
            &resources,
            &loaded.pack,
            &options,
            &settings,
            &entry,
        )
    } else {
        ColdPreparation::prepare_server(
            request,
            &resources,
            &loaded.pack,
            &options,
            &settings,
            &entry,
        )
    };
    let prepared = match cold.into_prepared(&entry) {
        Ok(prepared) => prepared,
        Err(cold) => {
            return close_cold(
                cold,
                commands,
                &view,
                (loaded.pack, gui, inputs, resources, cpu),
            );
        }
    };
    let audio = Audio::silent(&root);
    let mut game = match Game::new_with_world(
        &resources,
        loaded.pack,
        gui,
        options,
        settings,
        audio,
        Some(cpu),
        InitialGameWorld::Retained(Box::new(prepared)),
    ) {
        Ok(game) => Box::new(game),
        // Preloaded CPU plus the typed retained branch has no fallible step
        // after selection of original native inputs. A violated invariant is
        // fatal, never an ordinary owner-discarding startup error return.
        Err(error) => {
            eprintln!("fatal: cold retained server transfer: {error}");
            std::process::abort();
        }
    };
    game.title = None;
    game.screen = Screen::Playing;
    game.adv.tree = tree;
    game.skin_profiles = RefCell::new(horizon_client::skins::Resolver::without_fetch());
    game.skin_downloads = RefCell::new(horizon_client::skins::Downloader::without_fetch(
        root.join("assets/skins"),
    ));
    game.streaming.expect_existing_mut().radius = 2;
    game.streaming.expect_existing_mut().center = (0, 0);
    game.load_world_state();
    #[cfg(test)]
    let fixture_error = {
        game.retained_server.as_mut().unwrap().test = test.clone();
        test.install_staged_inputs(&game.world_root).err()
    };
    #[cfg(not(test))]
    let fixture_error: Option<String> = None;
    let startup = match fixture_error {
        None => game.install_original_retained_host(
            inputs.sync,
            inputs.encoder,
            inputs.config,
            inputs.offline,
        ),
        Some(error) => {
            game.retained_server
                .as_mut()
                .unwrap()
                .hold_hub_refusal(Box::new(RetainedHubRefusal {
                    sync: inputs.sync,
                    encoder: inputs.encoder,
                    config: inputs.config,
                    offline: inputs.offline,
                    reason: error.clone(),
                }));
            Err(error)
        }
    };
    run_game(game, commands, &view, startup.err())
}

fn close_cold<T>(
    mut cold: ColdPreparation,
    commands: mpsc::Receiver<Command>,
    view: &Mutex<View>,
    _resources: T,
) -> std::result::Result<(), String> {
    let original = cold
        .original_error()
        .unwrap_or("original cold transfer refused")
        .to_owned();
    let mut attempt = 1;
    cold.begin_close(Instant::now() + CLOSE_WAIT);
    loop {
        let status = cold.poll_close();
        let blocked = match &status {
            PreparationCloseStatus::Blocked(detail) => Some(detail.clone()),
            _ => None,
        };
        *view.lock().unwrap_or_else(|e| e.into_inner()) = View {
            phase: "ColdClosing",
            attempt,
            original_error: Some(original.clone()),
            blocked: blocked.clone(),
            ..Default::default()
        };
        for command in commands.try_iter().take(16) {
            match command {
                Command::Retry(expected) if expected == attempt && blocked.is_some() => {
                    if cold.retry_close(Instant::now() + CLOSE_WAIT).is_ok() {
                        attempt += 1;
                    }
                }
                Command::Offer(reply) => {
                    let _ = reply.send(Err("original cold server is closing".into()));
                }
                _ => {}
            }
        }
        if status == PreparationCloseStatus::Complete {
            match cold.finish_close() {
                Ok(closed) => {
                    drop(closed);
                    return Err(original);
                }
                Err(owner) => cold = owner,
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

enum Phase<'a> {
    Running(Box<Game<'a>>),
    Transport(Box<Game<'a>>),
    Native(Box<CloseOwner<'a>>),
    Ready(NativeReady<'a>),
}

fn player_observations(game: &Game<'_>) -> Vec<serde_json::Value> {
    game.lan.as_ref().map(|h|h.hub.guests.iter().map(|g|serde_json::json!({
        "uuid":format!("{:032x}",g.uuid),"placed":g.placed,"loaded":g.loaded,"body":g.motion.is_some(),
        "position":g.position,"yaw":g.yaw,"awaiting_teleport":g.awaiting_teleport.is_some(),"inbox":g.inbox.len(),
        "items":g.inventory.iter().flatten().map(|s|u64::from(s.count)).sum::<u64>(),
        "creative_reach_modifiers":g.motion.as_ref().map_or(0,|body|body.attributes.instances().flat_map(|(_,_,value)|value.modifiers())
            .filter(|modifier|matches!(modifier.id(),"minecraft:creative_mode_block_range"|"minecraft:creative_mode_entity_range")).count()),
        "book_open":g.recipe_book.open[0],"book_filtering":g.recipe_book.filtering[0]})).collect()).unwrap_or_default()
}

fn run_game<'a>(
    game: Box<Game<'a>>,
    commands: mpsc::Receiver<Command>,
    shared: &Mutex<View>,
    startup_error: Option<String>,
) -> std::result::Result<(), String> {
    let mut phase = Some(Phase::Running(game));
    let mut view = View::default();
    let mut original_error = startup_error;
    let mut attempt = 0u64;
    let mut close = original_error.is_some();
    let mut retry = false;
    let mut settled = false;
    let mut listen_started = false;
    let mut admission_error = None;
    let mut close_deadline = None;
    let mut transport_started = false;
    let mut next_tick = Instant::now();
    let settle_deadline = Instant::now() + Duration::from_secs(60);
    loop {
        for command in commands.try_iter().take(16) {
            match command {
                Command::Close => close = true,
                Command::Retry(expected) if expected == attempt && view.blocked.is_some() => {
                    retry = true
                }
                Command::Offer(reply) => {
                    let offered = match phase.as_mut().unwrap() {
                        Phase::Running(game) if settled && !close => game
                            .lan
                            .as_mut()
                            .unwrap()
                            .hub
                            .retained_connect_local()
                            .map_err(|e| e.to_string()),
                        _ => Err("original server is not admitting pipes".into()),
                    };
                    if let Err(mpsc::SendError(Ok(pipe))) = reply.send(offered) {
                        pipe.shutdown();
                        if let Phase::Running(game) = phase.as_mut().unwrap() {
                            let server = game.retained_server.as_mut().unwrap();
                            server.unsent_pipes.push(pipe);
                            server.fail("original offered Pipe reply was lost".into());
                        }
                        close = true;
                        original_error.get_or_insert("original offered Pipe reply was lost".into());
                    }
                }
                _ => {}
            }
        }
        if horizon_threads::stop_requests() > 0 {
            close = true;
        }
        let mut next = None;
        match phase.as_mut().unwrap() {
            Phase::Running(game) => {
                view.phase = if settled { "Running" } else { "Settling" };
                if !close {
                    let result = if !settled {
                        match game.retained_server_settle_step() {
                            Ok(true) => {
                                settled = true;
                                Ok(())
                            }
                            Ok(false) if Instant::now() < settle_deadline => Ok(()),
                            Ok(false) => Err("original native server spawn did not settle".into()),
                            Err(error) => Err(error),
                        }
                    } else if !listen_started {
                        match game.lan.as_mut().unwrap().hub.retained_listen() {
                            Ok(port) => {
                                view.port = port;
                                listen_started = true;
                                Ok(())
                            }
                            Err(error) => Err(error.into()),
                        }
                    } else if Instant::now() >= next_tick {
                        next_tick = Instant::now() + Duration::from_millis(50);
                        game.tick_server_timed(false)
                    } else {
                        Ok(())
                    };
                    if let Err(error) = result {
                        let detail = error.to_string();
                        original_error.get_or_insert(detail.clone());
                        game.retained_server.as_mut().unwrap().fail(detail);
                        close = true;
                    }
                }
                view.ready = settled && listen_started && !close;
                view.ticks = game.ticks;
                view.world_time = game.world.game_time;
                view.guests = game.lan.as_ref().map_or(0, |h| h.hub.guests.len());
                view.installed = game.retained_server.as_ref().unwrap().installations.len();
                view.pending_events = game.retained_server.as_ref().unwrap().pending.len();
                view.transport = game
                    .lan
                    .as_ref()
                    .and_then(|host| host.hub.retained_close_view());
                view.players = player_observations(game);
                #[cfg(test)]
                {
                    if game
                        .retained_server
                        .as_ref()
                        .unwrap()
                        .test
                        .refuse_live_consume
                        && !game
                            .retained_server
                            .as_ref()
                            .unwrap()
                            .consume_refusal_checked
                        && view.players.len() == 2
                        && view.players.iter().all(|g| g["loaded"] == true)
                    {
                        game.test_refuse_original_retained_host();
                    }
                    view.consume_refusal_checked = game
                        .retained_server
                        .as_ref()
                        .unwrap()
                        .consume_refusal_checked;
                }
                if close {
                    attempt = 1;
                    close_deadline = Some(Instant::now() + CLOSE_WAIT);
                    game.retained_server.as_mut().unwrap().freeze();
                    if let Err(error) = game.fence_retained_native_motion() {
                        original_error.get_or_insert(error.to_string());
                    }
                    next = Some(1);
                }
            }
            Phase::Transport(game) => {
                view.phase = "TransportClosing";
                view.ready = false;
                #[cfg(test)]
                let hold_frozen = game.retained_server.as_ref().unwrap().test.hold_frozen();
                #[cfg(not(test))]
                let hold_frozen = false;
                if hold_frozen {
                    view.phase = "Frozen";
                    view.transport = game
                        .lan
                        .as_ref()
                        .and_then(|host| host.hub.retained_close_view());
                } else {
                    if !transport_started {
                        if let Some(host) = &mut game.lan {
                            host.hub.begin_retained_close(
                                close_deadline
                                    .unwrap()
                                    .saturating_duration_since(Instant::now()),
                            );
                        }
                        transport_started = true;
                    }
                    if retry {
                        attempt += 1;
                        close_deadline = Some(Instant::now() + CLOSE_WAIT);
                        if let Some(host) = &mut game.lan
                            && let Some(hub) = host.hub.retained_close_view()
                            && hub.phase == RetainedHubPhase::Blocked
                        {
                            host.hub.retry_retained_close(hub.attempt, CLOSE_WAIT);
                        }
                        admission_error = game
                            .retry_retained_admissions()
                            .err()
                            .map(|e| e.to_string());
                        retry = false;
                    }
                    view.guests = game.lan.as_ref().map_or(0, |h| h.hub.guests.len());
                    view.installed = game.retained_server.as_ref().unwrap().installations.len();
                    view.pending_events = game.retained_server.as_ref().unwrap().pending.len();
                    view.players = player_observations(game);
                    let transport = game
                        .lan
                        .as_mut()
                        .and_then(|host| host.hub.poll_retained_close());
                    if let Some(transport) = &transport {
                        if let Some(error) = transport_failure(transport) {
                            original_error.get_or_insert(error);
                        }
                        view.transport = Some(transport.clone());
                    }
                    let transport_ready = transport
                        .as_ref()
                        .is_none_or(|status| status.phase == RetainedHubPhase::Ready);
                    let incomplete = game.retained_server.as_ref().unwrap().incomplete();
                    view.blocked = admission_error
                        .clone()
                        .or_else(|| {
                            incomplete.then(|| {
                                "original partial native admission requires exact Retry".into()
                            })
                        })
                        .or_else(|| {
                            transport
                                .as_ref()
                                .filter(|s| s.phase == RetainedHubPhase::Blocked)
                                .map(|_| "original transport close blocked".into())
                        });
                    if transport_ready && !incomplete && admission_error.is_none() {
                        #[cfg(test)]
                        if let Err(error) = game
                            .retained_server
                            .as_mut()
                            .unwrap()
                            .test
                            .inject_native_write_failure(&game.world_root)
                        {
                            admission_error = Some(error);
                            continue;
                        }
                        next = Some(2);
                    }
                }
            }
            Phase::Native(owner) => {
                view.phase = "NativeClosing";
                if retry {
                    let deadline = Instant::now() + CLOSE_WAIT;
                    if owner.retry(deadline).is_ok() {
                        attempt += 1;
                        close_deadline = Some(deadline);
                        view.blocked = None;
                    }
                    retry = false;
                }
                match owner.poll() {
                    CloseStatus::Pending(_) => view.blocked = None,
                    CloseStatus::Blocked(error) => {
                        original_error.get_or_insert(error.clone());
                        view.blocked = Some(error);
                    }
                    CloseStatus::NativeReady => next = Some(3),
                }
            }
            Phase::Ready(_) => {
                if retry {
                    attempt += 1;
                    view.blocked = None;
                    retry = false;
                }
                if view.blocked.is_none() {
                    next = Some(4);
                }
            }
        }
        if let Some(transition) = next {
            phase = Some(match (transition, phase.take().unwrap()) {
                (1, Phase::Running(game)) => Phase::Transport(game),
                (2, Phase::Transport(game)) => Phase::Native(CloseOwner::begin(
                    game,
                    CloseProfile::NativeWorld,
                    close_deadline.unwrap(),
                )),
                (3, Phase::Native(owner)) => match owner.try_native_ready() {
                    Ok(ready) => Phase::Ready(ready),
                    Err(owner) => Phase::Native(owner),
                },
                (4, Phase::Ready(ready)) => match ready.try_dispose_server() {
                    Ok(disposed) => {
                        if let Some(transport) = disposed.server_transport() {
                            if let Some(error) = transport_failure(transport) {
                                original_error.get_or_insert(error);
                            }
                            view.transport = Some(transport.clone());
                        }
                        drop(disposed);
                        view.phase = "NativeDisposed";
                        view.blocked = None;
                        view.attempt = attempt;
                        view.original_error = original_error.clone();
                        *shared.lock().unwrap_or_else(|e| e.into_inner()) = view;
                        return original_error.map_or(Ok(()), Err);
                    }
                    Err((ready, error)) => {
                        view.blocked = Some(error);
                        Phase::Ready(ready)
                    }
                },
                _ => unreachable!("fixed original server transition"),
            });
        }
        view.attempt = attempt;
        view.original_error = original_error.clone();
        *shared.lock().unwrap_or_else(|e| e.into_inner()) = view.clone();
        std::thread::sleep(Duration::from_millis(2));
    }
}

pub(super) fn run(options: Options, launch: Launch) -> Result<()> {
    run_configured(
        options,
        launch,
        #[cfg(test)]
        tests::Startup::default(),
    )
}

fn run_configured(
    options: Options,
    launch: Launch,
    #[cfg(test)] test: tests::Startup,
) -> Result<()> {
    horizon_threads::catch_retained_stop_signals();
    let mut settings = Settings::load(&std::env::current_dir()?.join("options.txt"));
    if options.server.is_some()
        || options.dedicated.is_some()
        || options.dimension.is_some()
        || options.server_thread
        || !options.test_terrain
        || !options.dev_mods.is_empty()
        || options.mod_packages.catalog().is_some()
        || !settings.mods.enabled.is_empty()
        || !settings.resource_packs.is_empty()
        || options.timeout > Duration::from_secs(120)
        || settings.extras.no_ai
        || settings.extras.fullbright
        || settings.extras.dynamic_lights
        || settings.extras.zoom
        || settings.extras.fps
        || settings.extras.keystrokes
        || settings.extras.minimap
        || settings.extras.auto_reconnect
        || settings.extras.mouse_tweaks
        || settings.extras.sort
    {
        return Err(
            "fixed retained server refuses unsupported options, selected content or extras".into(),
        );
    }
    settings.render_distance = 2;
    settings.simulation_distance = 2;
    settings.horizon.lod_distance = 0;
    let timeout = options.timeout;
    let control_path = options
        .control
        .clone()
        .ok_or("retained server control path missing")?;
    if !control_path.is_absolute() || control_path.parent() != Some(launch.root.as_path()) {
        return Err(
            "retained server endpoint must be directly inside its fresh absolute application root"
                .into(),
        );
    }
    std::fs::create_dir(&launch.root)?;
    let mut control = Some(RetainedControl::prepare(&control_path));
    #[cfg(test)]
    let final_test = test.clone();
    let mut supervisor = match Supervisor::prepare(Input {
        options,
        settings,
        launch,
        #[cfg(test)]
        test,
    }) {
        Ok(supervisor) => supervisor,
        Err((_input, error)) => {
            let mut owner = control.take().unwrap();
            owner.begin_close(Instant::now() + CLOSE_WAIT);
            loop {
                if owner.poll_close().observation.native_quiescent {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            let _closed = owner
                .try_closed()
                .unwrap_or_else(|_| unreachable!("unstarted retained endpoint"));
            return Err(error.into());
        }
    };
    let mut original_error = control.as_mut().unwrap().start().err();
    if original_error.is_some() {
        let _ = supervisor.commands.send(Command::Close);
    }
    let started = Instant::now();
    let mut timeout_sent = false;
    let mut control_stop_sent = original_error.is_some();
    let result = loop {
        let control_observation = control.as_ref().unwrap().observation();
        if let Some(failure) = control_observation.failures.first() {
            original_error
                .get_or_insert_with(|| format!("original retained control failure: {failure:?}"));
            if !control_stop_sent {
                control_stop_sent = true;
                let _ = supervisor.commands.send(Command::Close);
            }
        }
        if !timeout_sent && (started.elapsed() >= timeout || horizon_threads::stop_requests() > 0) {
            timeout_sent = true;
            let _ = supervisor.commands.send(Command::Close);
        }
        for _ in 0..16 {
            let Some(request) = control.as_mut().unwrap().try_recv() else {
                break;
            };
            let words = request.line.split_whitespace().collect::<Vec<_>>();
            let state = supervisor.snapshot();
            let reply = match words.as_slice() {
                ["status"] => {
                    serde_json::json!({"state":state.json(),"parent_error":original_error,
                    "control_phase":format!("{:?}",control_observation.phase),"control_endpoint":format!("{:?}",control_observation.endpoint),
                    "control_failures":control_observation.failures.iter().map(|failure|format!("{failure:?}")).collect::<Vec<_>>()})
                }
                ["close"] | ["quit"] => {
                    serde_json::json!({"accepted":supervisor.commands.send(Command::Close).is_ok(),"state":state.json()})
                }
                ["retry", attempt] => match attempt.parse::<u64>() {
                    Ok(expected) if state.blocked.is_some() && state.attempt == expected => {
                        serde_json::json!({"accepted":supervisor.commands.send(Command::Retry(expected)).is_ok(),"attempt":expected})
                    }
                    _ => {
                        serde_json::json!({"error":"Retry requires the exact current blocked attempt","state":state.json()})
                    }
                },
                _ => {
                    serde_json::json!({"error":"fixed retained server commands: status, close, retry ATTEMPT, quit"})
                }
            };
            let _ = request.reply.send(reply.to_string());
        }
        if let Some(result) = supervisor.join() {
            break result;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    if let Err(error) = result {
        original_error.get_or_insert(error);
    }
    // Finish every response handed to this parent using actual native delivery
    // counters, not response-channel enqueue. New requests still get completed
    // status while the original endpoint and joined worker result remain owned.
    let response_deadline = Instant::now() + CLOSE_WAIT;
    let mut refused_responses = Vec::new();
    loop {
        let before = control.as_ref().unwrap().observation();
        #[cfg(test)]
        if final_test.hold_parent_final(before.queued_requests) {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        for _ in 0..16 {
            let Some(request) = control.as_mut().unwrap().try_recv() else {
                break;
            };
            let reply = serde_json::json!({"state":supervisor.snapshot().json(),"parent_error":original_error,"worker_completed":true,
                "response_delivery":{"required":before.handed_off_requests,"delivered":before.delivered_replies,
                    "blocked":Instant::now()>=response_deadline}});
            if let Err(mpsc::SendError(original)) = request.reply.send(reply.to_string()) {
                refused_responses.push(original);
                original_error
                    .get_or_insert("original final control response channel refused".into());
            }
        }
        let after = control.as_ref().unwrap().observation();
        if let Some(failure) = after.failures.first() {
            original_error
                .get_or_insert_with(|| format!("original retained control failure: {failure:?}"));
            break;
        }
        if !refused_responses.is_empty()
            || (after.queued_requests == 0 && after.delivered_replies >= after.handed_off_requests)
        {
            break;
        }
        if Instant::now() >= response_deadline {
            // No forced discard or deadline reset. Keep serving completed
            // status while a native writer is still pending; its real delivery
            // or concrete failure must resolve before endpoint sealing.
            original_error.get_or_insert(
                "original final control response delivery exceeded its attempt".into(),
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    // The original worker and every native world/transport actor have completed
    // before sealing the still-owned actual control endpoint.
    let mut owner = control.take().unwrap();
    owner.begin_close(Instant::now() + CLOSE_WAIT);
    loop {
        let status = owner.poll_close();
        if status.observation.native_quiescent {
            break;
        }
        if status.observation.phase == RetainedControlPhase::Blocked {
            // Keep the same absolute attempt and actual endpoint. Finished
            // native joins may still establish quiescence after the deadline.
            original_error
                .get_or_insert("original retained control cleanup exceeded its attempt".into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let closed = match owner.try_closed() {
        Ok(closed) => closed,
        Err(_) => unreachable!("checked native endpoint completion"),
    };
    if let Some(failure) = closed.observation().failures.first() {
        original_error
            .get_or_insert_with(|| format!("original retained control failure: {failure:?}"));
    }
    drop(supervisor);
    drop(closed);
    drop(refused_responses);
    original_error.map_or(Ok(()), |error| Err(error.into()))
}
