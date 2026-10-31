//! Dormant closed-profile stop ownership. No ordinary launch/UI caller.
#![allow(dead_code)]
use super::server_game;
use crate::game::{
    Game, Options, PackResources, Resources,
    execution_role::ExecutionRole,
    lan::LanSettings,
    levels::session_checkpoint::{CheckpointReceipt, Publication, next_session_checkpoint_epoch},
    world_loading::{
        WorldRequest,
        host_identity::{IdentityIntent, session_authority::AuthorityNamespace},
    },
};
use horizon_client::{
    lan::{MAX_PLAYERS, NetConfig, hub::HubConfig},
    settings::Settings,
    world::counter_restore::{CounterTarget, NativeBinding, RuntimeCounterKey},
    world_slot_trial::SlotKey,
};
use std::{
    fs,
    io::{self, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone)]
struct StartPaths {
    world: PathBuf,
    archive: PathBuf,
    namespace: PathBuf,
}
struct ManagedStart {
    resources: Arc<Resources>,
    pack: PackResources,
    options: Options,
    settings: Settings,
    paths: StartPaths,
    counter_type: String,
    position: [i32; 3],
    attempt_timeout: Duration,
    #[cfg(test)]
    first_stop_fault: Option<crate::game::levels::session_checkpoint::CheckpointTestFault>,
}

#[derive(Clone, Debug)]
enum State {
    Preparing,
    Ready,
    Stopping {
        attempt: u64,
        epoch: Option<u64>,
    },
    RetainedFailure {
        attempt: u64,
        epoch: Option<u64>,
        publication: Publication,
        detail: String,
    },
    Stopped {
        attempt: u64,
        receipt: CheckpointReceipt,
    },
    StartupFailed(String),
    FatalWorkerFailure(String),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    State,
    StaleAttempt,
    InFlight,
    Disconnected,
}
#[derive(Clone, Copy, Debug)]
enum Command {
    RequestStop { attempt: u64 },
    RetryStop { attempt: u64, failed_attempt: u64 },
}
impl Command {
    fn attempt(self) -> u64 {
        match self {
            Self::RequestStop { attempt } | Self::RetryStop { attempt, .. } => attempt,
        }
    }
}
#[derive(Clone, Copy)]
enum DriverCommand {
    Wait,
    RequestStop(u64),
    RetryStop { attempt: u64, failed_attempt: u64 },
}
#[derive(Clone, Debug)]
struct SupervisorView {
    state: State,
    last_admitted: u64,
    last_refusal: Option<Refusal>,
    #[cfg(test)]
    observation: Option<Observation>,
}

/// Observers own no Game or JoinHandle. Their loss cannot stop or detach a worker.
#[derive(Clone)]
struct ManagedStopControl {
    shared: Arc<Mutex<SupervisorView>>,
    command: SyncSender<Command>,
}
impl ManagedStopControl {
    fn view(&self) -> SupervisorView {
        self.shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    fn request(&self, command: Command) -> Result<(), Refusal> {
        let mut view = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        let refusal = if command.attempt() == 0 || command.attempt() <= view.last_admitted {
            Some(Refusal::StaleAttempt)
        } else {
            match (&view.state, command) {
                (State::Ready, Command::RequestStop { .. }) => None,
                (
                    State::RetainedFailure {
                        attempt: failed, ..
                    },
                    Command::RetryStop { failed_attempt, .. },
                ) if *failed == failed_attempt => None,
                (State::Stopping { .. }, _) => Some(Refusal::InFlight),
                _ => Some(Refusal::State),
            }
        };
        if let Some(refusal) = refusal {
            view.last_refusal = Some(refusal);
            return Err(refusal);
        }
        // The worker cannot commit a new status until this admission lock releases.
        self.command.try_send(command).map_err(|error| {
            let refusal = match error {
                TrySendError::Full(_) => Refusal::InFlight,
                TrySendError::Disconnected(_) => Refusal::Disconnected,
            };
            view.last_refusal = Some(refusal);
            refusal
        })?;
        view.last_admitted = command.attempt();
        view.last_refusal = None;
        view.state = State::Stopping {
            attempt: command.attempt(),
            epoch: None,
        };
        Ok(())
    }
}

/// This private frame guard never joins in Drop. Unexpected unwind or an early
/// return with a live worker is actually process-fatal, not a detached thread.
struct AbortUnresolvedWorker(Option<JoinHandle<WorkerEnd>>);
impl Drop for AbortUnresolvedWorker {
    fn drop(&mut self) {
        if self.0.as_ref().is_some_and(|worker| !worker.is_finished()) {
            let _ = std::panic::catch_unwind(|| {
                let _ = io::stderr().write_all(b"HORIZON_MANAGED_SUPERVISOR_FATAL_LIVE_WORKER\n");
            });
            std::process::abort();
        }
    }
}
enum WorkerEnd {
    Stopped {
        attempt: u64,
        receipt: CheckpointReceipt,
    },
    StartupFailed(String),
}

/// The driver is owned by this synchronous supervisor scope, never by a detached
/// task. Its callback sees status/typed controls, not a mutable Game.
fn run_owned_managed_stop(
    start: ManagedStart,
    mut driver: impl FnMut(&SupervisorView, &ManagedStopControl) -> DriverCommand,
) -> Result<CheckpointReceipt, String> {
    checked_start_paths(&start.paths)?;
    let (command, commands) = mpsc::sync_channel(1);
    let (changed, changes) = mpsc::sync_channel(1);
    let shared = Arc::new(Mutex::new(SupervisorView {
        state: State::Preparing,
        last_admitted: 0,
        last_refusal: None,
        #[cfg(test)]
        observation: None,
    }));
    let control = ManagedStopControl {
        shared: shared.clone(),
        command,
    };
    // Install the private guard immediately; no fallible code between spawn and it.
    let mut worker = AbortUnresolvedWorker(Some(
        thread::Builder::new()
            .name("Managed stop worker".into())
            .spawn(move || owned_worker(start, shared, changed, commands))
            .map_err(|e| e.to_string())?,
    ));
    let mut previous = String::new();
    loop {
        let view = control.view();
        let display = format!("{:?}", view.state);
        if display != previous {
            log::info!("managed stop: {display}");
            previous = display;
        }
        if worker.0.as_ref().is_some_and(JoinHandle::is_finished) {
            // Only this positive completion check permits removing/joining the handle.
            let end = worker.0.take().expect("finished worker retained").join();
            return match end {
                Ok(WorkerEnd::Stopped { attempt, receipt }) => {
                    if matches!(&control.view().state, State::Stopped { attempt: current, receipt: published } if *current == attempt && published.epoch == receipt.epoch && published.root == receipt.root)
                    {
                        Ok(receipt)
                    } else {
                        Err("finished worker lacks matching stopped receipt".into())
                    }
                }
                Ok(WorkerEnd::StartupFailed(detail)) => Err(detail),
                Err(_) => {
                    let detail = "managed worker panicked; no shutdown receipt".to_owned();
                    control
                        .shared
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .state = State::FatalWorkerFailure(detail.clone());
                    Err(detail)
                }
            };
        }
        let request = driver(&view, &control);
        let command = match request {
            DriverCommand::Wait => None,
            DriverCommand::RequestStop(attempt) => Some(Command::RequestStop { attempt }),
            DriverCommand::RetryStop {
                attempt,
                failed_attempt,
            } => Some(Command::RetryStop {
                attempt,
                failed_attempt,
            }),
        };
        if let Some(command) = command {
            let _ = control.request(command);
        }
        // A disconnected status notification is not permission to drop a live worker.
        // Keep supervising and checking actual completion; the driver remains visible.
        let _ = changes.recv_timeout(Duration::from_millis(20));
    }
}

fn publish_view(
    shared: &Mutex<SupervisorView>,
    changed: &SyncSender<()>,
    state: State,
    observation: Option<Observation>,
) {
    let mut view = shared.lock().unwrap_or_else(|e| e.into_inner());
    view.state = state;
    #[cfg(test)]
    {
        view.observation = observation;
    }
    #[cfg(not(test))]
    let _ = observation;
    drop(view);
    let _ = changed.try_send(());
}

struct OwnedGame<'a> {
    game: Box<Game<'a>>,
    counter: RuntimeCounterKey,
    slot: SlotKey,
}
fn owned_worker(
    start: ManagedStart,
    shared: Arc<Mutex<SupervisorView>>,
    changed: SyncSender<()>,
    commands: Receiver<Command>,
) -> WorkerEnd {
    let mut owned = match new_closed_game(&start) {
        Ok(game) => game,
        Err(detail) => {
            publish_view(
                &shared,
                &changed,
                State::StartupFailed(detail.clone()),
                None,
            );
            return WorkerEnd::StartupFailed(detail);
        }
    };
    #[cfg(test)]
    let mut fault = start.first_stop_fault;
    publish_view(&shared, &changed, State::Ready, observation(&owned));
    loop {
        let command = match commands.recv() {
            Ok(command) => command,
            Err(_) => {
                // The supervisor retains a sender until checked terminal joining.
                // An unexpected disconnect must not drop its still-owned Game.
                log::error!("managed stop control disconnected with retained Game");
                loop {
                    thread::park_timeout(Duration::from_secs(1));
                }
            }
        };
        let attempt = command.attempt();
        let epoch = match next_session_checkpoint_epoch(&start.paths.archive) {
            Ok(epoch) => epoch,
            Err(error) => {
                publish_view(
                    &shared,
                    &changed,
                    State::RetainedFailure {
                        attempt,
                        epoch: None,
                        publication: error.publication,
                        detail: error.detail,
                    },
                    observation(&owned),
                );
                continue;
            }
        };
        publish_view(
            &shared,
            &changed,
            State::Stopping {
                attempt,
                epoch: Some(epoch),
            },
            observation(&owned),
        );
        let until = Instant::now() + start.attempt_timeout;
        #[cfg(test)]
        let result = if let Some(fault) = fault.take() {
            owned
                .game
                .checkpoint_session_with_test_fault(&start.paths.archive, epoch, until, fault)
        } else {
            owned
                .game
                .checkpoint_session(&start.paths.archive, epoch, until)
        };
        #[cfg(not(test))]
        let result = owned
            .game
            .checkpoint_session(&start.paths.archive, epoch, until);
        match result {
            Err(error) => {
                publish_view(
                    &shared,
                    &changed,
                    State::RetainedFailure {
                        attempt,
                        epoch: Some(epoch),
                        publication: error.publication,
                        detail: error.detail,
                    },
                    observation(&owned),
                );
            }
            Ok(receipt) => {
                // The checked complete image is the sole STOP commit boundary.
                // No leave statistics, packet dispatch or second native save.
                owned
                    .game
                    .retire_all_counter_trials("managed-confirmed-stop");
                let host = owned.game.lan.take();
                drop(host);
                drop(owned);
                publish_view(
                    &shared,
                    &changed,
                    State::Stopped {
                        attempt,
                        receipt: receipt.clone(),
                    },
                    None,
                );
                return WorkerEnd::Stopped { attempt, receipt };
            }
        }
    }
}

fn real_directory(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() || path.components().any(|p| matches!(p, Component::ParentDir)) {
        return Err("managed startup needs absolute paths without traversal".into());
    }
    for at in path.ancestors() {
        let metadata = fs::symlink_metadata(at).map_err(|e| e.to_string())?;
        #[cfg(windows)]
        let reparse = {
            use std::os::windows::fs::MetadataExt;
            metadata.file_attributes() & 0x400 != 0
        };
        #[cfg(not(windows))]
        let reparse = false;
        if !metadata.is_dir() || metadata.file_type().is_symlink() || reparse {
            return Err("managed startup linked/non-directory root".into());
        }
    }
    fs::canonicalize(path).map_err(|e| e.to_string())
}
fn checked_start_paths(paths: &StartPaths) -> Result<(), String> {
    let namespace = real_directory(&paths.namespace)?;
    let archive = real_directory(&paths.archive)?;
    match paths.world.symlink_metadata() {
        Ok(_) => return Err("managed startup world must be absent".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    if !paths.world.is_absolute()
        || paths
            .world
            .components()
            .any(|p| matches!(p, Component::ParentDir | Component::CurDir))
    {
        return Err("managed startup world path".into());
    }
    let parent = real_directory(paths.world.parent().ok_or("managed world parent")?)?;
    let world = parent.join(paths.world.file_name().ok_or("managed world name")?);
    for (a, b) in [
        (&namespace, &archive),
        (&namespace, &world),
        (&archive, &world),
    ] {
        if a.starts_with(b) || b.starts_with(a) {
            return Err("managed startup overlapping roots".into());
        }
    }
    if fs::read_dir(&archive)
        .map_err(|e| e.to_string())?
        .next()
        .is_some()
    {
        return Err("managed startup archive must be empty".into());
    }
    Ok(())
}

fn new_closed_game(start: &ManagedStart) -> Result<OwnedGame<'_>, String> {
    checked_start_paths(&start.paths)?;
    if start.attempt_timeout.is_zero() || start.attempt_timeout > Duration::from_secs(120) {
        return Err("managed stop deadline bound".into());
    }
    let namespace = AuthorityNamespace::new(&start.paths.namespace).map_err(|e| e.to_string())?;
    let mut options = start.options.clone();
    options.execution_role = ExecutionRole::DedicatedServer;
    options.dedicated = Some(Vec::new());
    options.server_thread = false;
    // This entry accepts the actual built-in native test terrain only.
    if !options.test_terrain || !options.mod_packages.has_pairs() {
        return Err("managed closed profile requires captured pair and native TestTerrain".into());
    }
    let mut game = Box::new(
        server_game(
            &start.resources,
            start.pack.clone(),
            options,
            start.settings.clone(),
        )
        .map_err(|e| e.to_string())?,
    );
    game.start_mods();
    let mut open = WorldRequest::new("managed", Some("Managed stop"), Some(17), false, None)
        .map_err(|e| e.to_string())?
        .with_identity_intent(IdentityIntent::CreateNewIdentity)
        .with_session_authority(namespace);
    open.save_root = start.paths.world.clone();
    let prepared = open
        .prepare(&game.res, &game.pack, &game.options, &game.settings)
        .map_err(|e| e.to_string())?;
    game.finish_open_world(prepared)
        .map_err(|e| e.to_string())?;
    let _ = game.advancement_tree();
    game.streaming.center = (start.position[0] >> 4, start.position[2] >> 4);
    game.streaming.radius = 2;
    let until = Instant::now() + start.attempt_timeout;
    loop {
        let (_, unloaded) = game.streaming.update(&mut game.world);
        if !unloaded.is_empty() {
            return Err("fresh managed setup unexpectedly unloaded native columns".into());
        }
        let (x, z) = game.streaming.center;
        if (-2..=2).all(|dz| (-2..=2).all(|dx| game.world.chunks.contains_key(&(x + dx, z + dz))))
            && game.world.chunks.get(&(x, z)).is_some_and(|c| c.lit)
        {
            break;
        }
        if Instant::now() >= until {
            return Err("managed native residency timeout".into());
        }
        thread::yield_now();
    }
    let binding = {
        let catalog = game
            .options
            .mod_packages
            .catalog()
            .ok_or("managed captured catalog absent")?;
        let mut owners = catalog.prepared().values().filter(|p| {
            p.content()
                .machine_types()
                .get(&start.counter_type)
                .is_some()
        });
        let owner = owners.next().ok_or("managed counter type absent")?;
        if owners.next().is_some() {
            return Err("managed counter type ambiguous".into());
        }
        NativeBinding::from_prepared(owner, &start.counter_type, game.pack.assets.registry)
            .map_err(|e| e.to_string())?
    };
    let at = start.position;
    game.world
        .set(at[0], at[1], at[2], binding.state_id())
        .map_err(|error| error.to_string())?;
    let counter = game
        .bootstrap_host_counter(&start.counter_type, at)
        .map_err(|e| e.to_string())?;
    let target = CounterTarget::new(game.dim.id(), at).map_err(|e| e.to_string())?;
    let slot = game
        .world
        .initialize_persistent_world_slot(&binding, &target)
        .map_err(|e| e.to_string())?
        .ok_or("managed slot bootstrap unavailable")?;
    let config = HubConfig {
        default_game_mode: game.level.mode(),
        guest_commands: false,
        force_game_mode: false,
        hardcore: game.level.hardcore,
        view_distance: 2,
        simulation_distance: 2,
        dimension: game.dim.id().into(),
        seed: game.level.seed,
        max_players: MAX_PLAYERS,
        dedicated: true,
        integrated: false,
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
            ..NetConfig::lan(0, game.level.name.clone(), MAX_PLAYERS)
        },
        config,
        None,
    )
    .map_err(|e| e.to_string())?;
    // Establish native level/player metadata and a real baseline before Ready.
    game.save_game_with(false).map_err(|e| e.to_string())?;
    let baseline =
        next_session_checkpoint_epoch(&start.paths.archive).map_err(|e| e.to_string())?;
    game.checkpoint_session(
        &start.paths.archive,
        baseline,
        Instant::now() + start.attempt_timeout,
    )
    .map_err(|e| e.to_string())?;
    Ok(OwnedGame {
        game,
        counter,
        slot,
    })
}

#[cfg(not(test))]
type Observation = ();
fn observation(owned: &OwnedGame<'_>) -> Option<Observation> {
    #[cfg(test)]
    {
        Some(observe(owned))
    }
    #[cfg(not(test))]
    {
        let _ = owned;
        None
    }
}
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct Observation {
    world: horizon_client::world::counter_restore::WorldId,
    root: PathBuf,
    counter_key: RuntimeCounterKey,
    slot_key: SlotKey,
    frame: horizon_client::world::counter_restore::CounterFrameV2,
    slot_bytes: Vec<u8>,
    clock: i64,
    host_entity: i32,
    host_address: usize,
    guests: usize,
    empty_slot: bool,
}
#[cfg(test)]
fn observe(owned: &OwnedGame<'_>) -> Observation {
    let game = &owned.game;
    let host = game.lan.as_ref().expect("owned native host");
    Observation {
        world: *game
            .session_lock
            .as_ref()
            .expect("native session")
            .expected_world()
            .expect("identified native session"),
        root: game.world_root.clone(),
        counter_key: owned.counter.clone(),
        slot_key: owned.slot.clone(),
        frame: game
            .world
            .counter_checkpoint_frame(game.world.game_time)
            .expect("healthy native counter"),
        slot_bytes: game
            .world
            .slot_checkpoint_bytes(game.res.items(), game.world.game_time)
            .expect("healthy native slot"),
        clock: game.world.game_time,
        host_entity: host.host_entity,
        host_address: std::ptr::from_ref(host) as usize,
        guests: host.hub.guests.len(),
        empty_slot: game
            .world
            .read_world_slot(&owned.slot)
            .expect("live native slot")
            .slot
            .stack()
            .is_none(),
    }
}

#[cfg(test)]
#[path = "managed_stop_tests.rs"]
mod tests;
