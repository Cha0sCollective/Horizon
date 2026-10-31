//! Explicit isolated frozen recovery window. Ordinary gameplay never enters here.
use super::managed_stop::{
    Command, ConnectedPhase, ManagedConnectedStart, ManagedStopControl, ManagedWorker, State,
    SupervisorView,
};
use crate::game::{
    Game, Options, PackResources, Resources, UiResources,
    execution_role::ExecutionRole,
    levels::session_checkpoint::{CheckpointReceipt, ManagedRecoveryRequest},
    loading_frame, loading_renderer,
};
use horizon_client::{
    Result, gfx::Renderer, hud::HudAtlas, settings::Settings, widgets::Cursor,
    world::counter_restore::CounterTarget,
};
use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, MouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{KeyCode, PhysicalKey},
    window::{CursorIcon, Window, WindowId},
};
#[path = "managed_application_view.rs"]
mod view;
use view::{FrozenView, FrozenViewAction, FrozenViewModel};

const WAIT: Duration = Duration::from_secs(120);
const PACKAGE: &str = "guest_counter_fixture";
const MACHINE: &str = "guest_counter_fixture:counter";
const TITLE: &str = "Horizon — Frozen recovery";

pub(in crate::game) struct ManagedLaunch {
    root: PathBuf,
    resources: PathBuf,
    package: PathBuf,
    arguments: Vec<String>,
}

/// Detect the special route before ordinary argument parsing has side effects.
fn requested(arguments: &[String]) -> bool {
    arguments
        .iter()
        .any(|a| a == "--managed-frozen-recovery" || a.starts_with("--managed-frozen-recovery="))
}
struct LaunchTokens {
    root: PathBuf,
    resources: PathBuf,
    package: PathBuf,
    ordinary: Vec<String>,
}
fn checked_tokens(
    arguments: &[String],
    session: bool,
    signed_in: bool,
) -> Result<Option<LaunchTokens>> {
    if !requested(arguments) {
        return Ok(None);
    }
    if session
        || signed_in
        || arguments
            .iter()
            .any(|a| a == "--session-file" || a.starts_with("--session-file="))
    {
        return Err(
            "frozen recovery requires an isolated offline launch without account configuration"
                .into(),
        );
    }
    let mut seen = BTreeSet::new();
    let (mut root, mut resources, mut package) = (None, None, None);
    let mut ordinary = Vec::new();
    let mut args = arguments.iter();
    while let Some(flag) = args.next() {
        if !matches!(
            flag.as_str(),
            "--managed-frozen-recovery"
                | "--resources"
                | "--dev-mod"
                | "--width"
                | "--height"
                | "--name"
        ) || !seen.insert(flag.as_str())
        {
            return Err("unsupported or repeated frozen recovery argument".into());
        }
        let value = args
            .next()
            .filter(|v| !v.is_empty() && !v.starts_with("--"))
            .ok_or("missing frozen recovery argument value")?;
        match flag.as_str() {
            "--managed-frozen-recovery" => root = Some(PathBuf::from(value)),
            "--resources" => resources = Some(PathBuf::from(value)),
            "--dev-mod" => package = Some(PathBuf::from(value)),
            "--name" if value != "MemoryOwner" => {
                return Err(
                    "frozen recovery requires the fixed offline MemoryOwner profile".into(),
                );
            }
            "--width" | "--height" => {
                let dimension = value
                    .parse::<u32>()
                    .map_err(|_| "invalid frozen recovery window size")?;
                if !(64..=3840).contains(&dimension) {
                    return Err("frozen recovery window size out of range".into());
                }
            }
            _ => {}
        }
        if flag != "--managed-frozen-recovery" {
            ordinary.extend([flag.clone(), value.clone()]);
        }
    }
    Ok(Some(LaunchTokens {
        root: root.ok_or("missing frozen recovery root")?,
        resources: resources.ok_or("explicit resources required")?,
        package: package.ok_or("explicit prepared package required")?,
        ordinary,
    }))
}

fn real_directory(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err("frozen recovery needs real absolute paths without traversal".into());
    }
    for at in path.ancestors() {
        let metadata = fs::symlink_metadata(at)?;
        #[cfg(windows)]
        let reparse = {
            use std::os::windows::fs::MetadataExt;
            metadata.file_attributes() & 0x400 != 0
        };
        #[cfg(not(windows))]
        let reparse = false;
        if !metadata.is_dir() || metadata.file_type().is_symlink() || reparse {
            return Err("linked or non-directory frozen recovery root".into());
        }
    }
    Ok(fs::canonicalize(path)?)
}
fn isolation_root() -> Result<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("isolated HOME required")?);
    let parent = real_directory(home.parent().ok_or("isolated HOME parent")?)?;
    if !parent
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("fw-"))
    {
        return Err("frozen recovery requires a coordinator-created fw- scratch directory".into());
    }
    for (key, child) in [
        ("HOME", "home"),
        ("USERPROFILE", "home"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_CACHE_HOME", "cache"),
        ("TMPDIR", "temp"),
        ("TMP", "temp"),
        ("TEMP", "temp"),
        ("HORIZON_HOME", "game"),
        ("HORIZON_LAUNCHER_HOME", "launcher"),
        ("HORIZON_LAUNCHER_TRASH", "trash"),
        ("HORIZON_LAUNCHER_CLIPBOARD", "clipboard"),
        ("HORIZON_LAUNCHER_DESKTOP", "desktop"),
    ] {
        let path = PathBuf::from(
            std::env::var_os(key).ok_or("explicit isolated runtime directories required")?,
        );
        if real_directory(&path)? != parent.join(child) {
            return Err("runtime directory outside the isolated frozen layout".into());
        }
    }
    if real_directory(&std::env::current_dir()?)? != parent.join("crates/horizon-client") {
        return Err(
            "frozen recovery must launch from its isolated coordinator working directory".into(),
        );
    }
    Ok(parent)
}
impl ManagedLaunch {
    pub(in crate::game) fn parse(arguments: &[String]) -> Result<Option<Self>> {
        let Some(LaunchTokens {
            root,
            resources,
            package,
            ordinary,
        }) = checked_tokens(
            arguments,
            std::env::var_os("HORIZON_SESSION_FILE").is_some(),
            horizon_client::auth::current().is_some(),
        )?
        else {
            return Ok(None);
        };
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if name.starts_with("HORIZON_COMMAND_")
                || name.starts_with("HORIZON_NET_")
                || name.starts_with("HORIZON_DISPENSE_")
                || name.starts_with("HORIZON_SCULK_")
                || name.starts_with("HORIZON_COMPONENT_")
                || name.ends_with("_FATAL_CHILD")
                || name == "HORIZON_CONNECTED_DEPARTURE_CHILD"
                || name == "HORIZON_MANAGED_CONNECTED_DEPARTURE_CHILD"
                || name == "HORIZON_SPIKES"
                || (name.starts_with("HORIZON_FROZEN_") && !cfg!(test))
            {
                return Err("ordinary command, network and component harnesses are excluded from frozen recovery".into());
            }
        }
        if std::env::var_os("HORIZON_PLAYER_NAME").is_some_and(|n| n != "MemoryOwner")
            || std::env::var_os("HORIZON_SERVER_THREAD").is_some_and(|v| v != "1")
        {
            return Err("conflicting frozen profile or execution configuration".into());
        }
        let isolated = isolation_root()?;
        let root = real_directory(&root)?;
        if !root.starts_with(isolated.join("temp")) {
            return Err("frozen world layout must stay within isolated temp".into());
        }
        for name in [
            "source-world",
            "source-archive",
            "authority",
            "continuation",
        ] {
            real_directory(&root.join(name))?;
        }
        if root.join("recovered").try_exists()?
            || fs::read_dir(root.join("continuation"))?.next().is_some()
        {
            return Err("fresh recovery destination and empty continuation required".into());
        }
        let resources = real_directory(&resources)?;
        let package = real_directory(&package)?;
        if !resources.join("horizon-resources.json").is_file()
            || !package.join("horizon-paired.toml").is_file()
        {
            return Err("prepared resources and paired package required".into());
        }
        Ok(Some(Self {
            root,
            resources,
            package,
            arguments: ordinary,
        }))
    }
    pub(in crate::game) fn ordinary_args(&self) -> Vec<String> {
        self.arguments.clone()
    }
    pub(in crate::game) fn configure(&self, options: &mut Options) {
        options.resources = self.resources.clone();
        options.dev_mods = vec![self.package.clone()];
        options.player_name = Some("MemoryOwner".into());
        options.server_thread = true;
        options.test_terrain = true;
        options.distance = 2;
        options.distance_override = Some(2);
        options.execution_role = ExecutionRole::Client;
    }
}

fn check_package(options: &Options) -> Result<()> {
    let catalog = options
        .mod_packages
        .catalog()
        .ok_or("captured package catalog absent")?;
    if !options.mod_packages.has_pairs()
        || catalog.prepared().len() != 1
        || catalog.entries().len() != 1
    {
        return Err("frozen recovery accepts exactly its one prepared counter fixture".into());
    }
    let prepared = catalog
        .prepared()
        .values()
        .next()
        .ok_or("prepared fixture absent")?;
    let owner = prepared.bundle().content_owner();
    let types = prepared.content().machine_types().types();
    if owner.id != PACKAGE
        || owner.version != "0.0.0"
        || types.len() != 1
        || types[0].id() != MACHINE
    {
        return Err("frozen recovery package/profile mismatch".into());
    }
    Ok(())
}
fn client_game<'a>(
    resources: &'a Arc<Resources>,
    pack: PackResources,
    mut options: Options,
    mut settings: Settings,
    name: &str,
) -> Result<Box<Game<'a>>> {
    if !cfg!(feature = "mods") {
        return Err("frozen recovery requires the mod runtime".into());
    }
    options.execution_role = ExecutionRole::Client;
    options.server_thread = true;
    options.dedicated = None;
    options.player_name = Some(name.into());
    settings.render_distance = 2;
    settings.simulation_distance = 2;
    let gui = UiResources {
        atlas: HudAtlas::fonts(&options.resources)?,
        panorama: Arc::new(horizon_client::gfx::panorama::Faces {
            size: 1,
            pixels: [0u8; 4].repeat(6),
        }),
    };
    let audio = horizon_client::audio::Audio::silent(&options.resources);
    let mut game = Box::new(Game::new(
        resources, pack, gui, options, settings, audio, None,
    )?);
    game.start_mods();
    #[cfg(feature = "mods")]
    game.options.mod_packages.check_running(&game.mods.mods)?;
    Ok(game)
}
fn clipped(mut text: String) -> String {
    let mut end = text.len().min(256);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

/// The worker guard is destroyed before clients/presentation during an unwind.
struct FrozenApplication<'a> {
    worker: Option<ManagedWorker>,
    clients: [Box<Game<'a>>; 2],
    start: Option<ManagedConnectedStart>,
    gui: UiResources,
    settings: Settings,
    window_size: [u32; 2],
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    view: FrozenView,
    pointer: Option<(f32, f32)>,
    pressed: Option<FrozenViewAction>,
    control: Option<ManagedStopControl>,
    snapshot: Option<SupervisorView>,
    completion: Option<std::result::Result<CheckpointReceipt, String>>,
    finished: bool,
    want_exit: bool,
    close_submitted: bool,
    offers: usize,
    signals: u32,
    error: String,
    occluded: bool,
    frames: u64,
    #[cfg(all(test, target_os = "linux"))]
    probe: Option<tests::Probe>,
}
impl FrozenApplication<'_> {
    fn fail(&mut self, error: impl ToString) {
        self.error = clipped(error.to_string());
        self.view.clear_hitboxes();
    }
    fn action(&mut self, action: FrozenViewAction) {
        if action == FrozenViewAction::Close {
            self.want_exit = true;
        }
        let Some(control) = &self.control else {
            return;
        };
        let view = control.view();
        let Some(attempt) = view.last_admitted.checked_add(1) else {
            self.fail("stop attempt identifiers exhausted; state retained");
            return;
        };
        let command = match (&view.state, action) {
            (State::Ready, FrozenViewAction::Close) if !self.close_submitted => {
                Some(Command::RequestStop { attempt })
            }
            (
                State::RetainedFailure {
                    attempt: failed, ..
                },
                FrozenViewAction::Retry,
            ) => Some(Command::RetryStop {
                attempt,
                failed_attempt: *failed,
            }),
            _ => None,
        };
        if let Some(command) = command {
            match control.request(command) {
                Ok(()) => self.close_submitted = true,
                Err(error) => self.fail(format!("stop request refused: {error:?}")),
            }
        }
    }
    fn turn(&mut self, event_loop: &ActiveEventLoop) {
        // Native exit can deliver another turn after the one-shot result was
        // classified. Keep that result until the enclosing application returns.
        if self.finished {
            return;
        }
        let signals = horizon_threads::stop_requests();
        if signals > self.signals {
            self.signals = signals;
            self.want_exit = true;
        }
        let Some(worker) = self.worker.as_mut() else {
            return;
        };
        let polled = worker.poll();
        self.snapshot = Some(polled.view);
        if polled.completed {
            self.completion = worker.take_completion();
            self.finished = true;
            for client in &mut self.clients {
                if let Some(remote) = client.remote.as_mut() {
                    remote.session.disconnect();
                }
            }
            event_loop.exit();
            return;
        }
        if let Some([a, b]) = polled.pipes {
            assert_eq!(
                self.offers, 0,
                "duplicate frozen application endpoint bundle"
            );
            self.offers += 1;
            let mut error = None;
            for ((client, pipe), name) in self
                .clients
                .iter_mut()
                .zip([a, b])
                .zip(["MemoryOwner", "MemoryGuest"])
            {
                if let Err(problem) = client.connect_local(pipe, name) {
                    error = Some(problem.to_string());
                    break;
                }
            }
            if let Some(error) = error {
                self.fail(error);
            }
        }
        let closing = self.snapshot.as_ref().is_some_and(|v| {
            matches!(v.state, State::Stopping { .. } | State::Stopped { .. })
                || v.connected
                    .as_ref()
                    .is_some_and(|s| s.phase == ConnectedPhase::Closing)
        });
        if self.offers == 1 && !closing {
            for client in &mut self.clients {
                if let Err(error) = client.poll_stationary_client() {
                    self.error = clipped(error.to_string());
                }
            }
        }
        #[cfg(all(test, target_os = "linux"))]
        let close_halted = self.probe_holds_close();
        #[cfg(not(all(test, target_os = "linux")))]
        let close_halted = false;
        if self.want_exit && !self.close_submitted && !close_halted {
            self.action(FrozenViewAction::Close);
        }
        #[cfg(all(test, target_os = "linux"))]
        self.probe_turn(event_loop);
    }
    fn draw(&mut self) {
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        let state = self.snapshot.as_ref().map(|v| &v.state);
        let (phase, status) = match state {
            None | Some(State::Preparing) => ("Preparing", "Preparing the frozen recovery"),
            Some(State::Connected(ConnectedPhase::Joining)) => {
                ("Connecting", "Connecting both offline clients")
            }
            Some(State::Connected(ConnectedPhase::Admitted | ConnectedPhase::Loading)) => {
                ("Loading terrain", "Restoring both players")
            }
            Some(State::Ready) => ("Ready (frozen)", "Close saves the recovered world"),
            Some(State::Stopping { .. }) => ("Saving and closing", "Waiting for the owned worker"),
            Some(State::RetainedFailure { .. }) => (
                "Save failed — retained",
                "Retry is required to save and close",
            ),
            Some(State::Connected(ConnectedPhase::CleanupBlocked)) => {
                ("Recovery retained", "A departed client prevents safe close")
            }
            Some(State::Connected(ConnectedPhase::Refusing)) => {
                ("Connection refused", "Finishing native cleanup")
            }
            Some(State::Stopped { .. }) => {
                ("Finishing close", "Waiting for explicit joined completion")
            }
            _ => ("Recovery status", "See diagnostic below"),
        };
        let facts: Vec<_> = self
            .clients
            .iter()
            .map(|g| {
                g.remote_status().map_or_else(
                    || "Not connected".to_owned(),
                    |s| format!("joined={} terrain={}", s["joined"], s["loaded"]),
                )
            })
            .collect();
        let counts: Vec<u32> = self
            .clients
            .iter()
            .map(|g| {
                g.inventory
                    .iter()
                    .flatten()
                    .filter(|s| s.item.as_ref() == "minecraft:stone")
                    .map(|s| s.count)
                    .sum()
            })
            .collect();
        let counts = format!("Owner: {} stone   Guest: {} stone", counts[0], counts[1]);
        let clocks = self
            .snapshot
            .as_ref()
            .and_then(|v| v.connected.as_ref())
            .map_or_else(String::new, |s| {
                format!(
                    "World {} / day {} / server ticks {}",
                    s.world_time, s.day_time, s.hub_ticks
                )
            });
        let failed = match state {
            Some(State::RetainedFailure { attempt, .. }) => Some(*attempt),
            _ => None,
        };
        let attempt = self
            .snapshot
            .as_ref()
            .map(|v| v.last_admitted)
            .filter(|a| *a != 0);
        let detail = if !self.error.is_empty() {
            self.error.as_str()
        } else {
            self.snapshot
                .as_ref()
                .and_then(|v| v.connected.as_ref())
                .map_or("", |s| s.detail.as_str())
        };
        let frame = self.view.draw(
            &self.gui,
            [renderer.config.width, renderer.config.height],
            self.pointer,
            &FrozenViewModel {
                phase,
                status,
                close_attempt: attempt,
                failed_attempt: failed,
                owner_facts: &facts[0],
                guest_facts: &facts[1],
                counts: &counts,
                clocks: &clocks,
                diagnostic: detail,
                close_allowed: matches!(state, Some(State::Ready)) && !self.close_submitted,
                retry_allowed: failed.is_some(),
            },
        );
        if let Some(window) = &self.window {
            window.set_cursor(match frame.cursor {
                Cursor::Arrow => CursorIcon::Default,
                Cursor::PointingHand => CursorIcon::Pointer,
                Cursor::NotAllowed => CursorIcon::NotAllowed,
            });
        }
        let rendered = renderer.render(&loading_frame(&frame.hud));
        #[cfg(all(test, target_os = "linux"))]
        let rendered = self.probe_render_result(rendered);
        match rendered {
            Ok(drawn) => {
                if drawn {
                    self.frames = self.frames.saturating_add(1);
                }
            }
            Err(error) => self.fail(error),
        }
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.gpu_times.clear();
        }
    }
    fn finish_exit(&mut self) {
        if let Some(worker) = self.worker.as_mut() {
            let polled = worker.poll();
            if polled.completed && !self.finished {
                self.completion = worker.take_completion();
                self.finished = true;
            }
            if !self.finished {
                use std::io::Write;
                let _ = writeln!(
                    std::io::stderr(),
                    "HORIZON_FROZEN_APPLICATION_FATAL_LIVE_WORKER"
                );
                std::process::abort();
            }
        }
    }
    fn physical_key(&mut self, key: PhysicalKey) {
        match key {
            PhysicalKey::Code(KeyCode::Escape | KeyCode::Enter) => {
                self.action(FrozenViewAction::Close)
            }
            PhysicalKey::Code(KeyCode::KeyR) => self.action(FrozenViewAction::Retry),
            _ => {}
        }
    }
}
impl ApplicationHandler for FrozenApplication<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() || self.finished {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(TITLE)
            .with_inner_size(winit::dpi::LogicalSize::new(
                self.window_size[0],
                self.window_size[1],
            ));
        let result = (|| -> Result<(Arc<Window>, Renderer)> {
            let window = Arc::new(event_loop.create_window(attributes)?);
            let renderer = loading_renderer(
                window.clone(),
                false,
                Some(&self.gui),
                None,
                self.settings.horizon.api,
            )?;
            Ok((window, renderer))
        })();
        match result {
            Ok((window, renderer)) => {
                self.window = Some(window);
                self.renderer = Some(renderer);
            }
            Err(error) => {
                self.fail(error);
                self.finished = true;
                self.completion = Some(Err(self.error.clone()));
                event_loop.exit();
                return;
            }
        }
        horizon_client::hud::GUI_SCALE.store(
            self.settings.gui_scale,
            std::sync::atomic::Ordering::Relaxed,
        );
        if let Some(mut start) = self.start.take() {
            start.request.deadline = Instant::now() + WAIT;
            match ManagedWorker::start_connected(start) {
                Ok(worker) => {
                    self.control = Some(worker.control());
                    self.worker = Some(worker);
                }
                Err(error) => {
                    self.fail(error);
                    self.finished = true;
                    self.completion = Some(Err(self.error.clone()));
                    event_loop.exit();
                }
            }
        }
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if self.window.as_ref().is_none_or(|w| w.id() != id) {
            return;
        }
        match event {
            WindowEvent::CloseRequested => self.action(FrozenViewAction::Close),
            WindowEvent::Resized(size) => {
                self.view.clear_hitboxes();
                self.pressed = None;
                if size.width > 0
                    && size.height > 0
                    && let Some(renderer) = self.renderer.as_mut()
                {
                    renderer.resize(size.width, size.height);
                }
            }
            WindowEvent::Occluded(value) => {
                self.occluded = value;
                self.view.clear_hitboxes();
            }
            WindowEvent::Focused(false) | WindowEvent::CursorLeft { .. } => {
                self.pointer = None;
                self.pressed = None;
                self.view.clear_hitboxes();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer = Some((position.x as f32, position.y as f32))
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                let action = self.pointer.and_then(|p| self.view.action_at(p));
                if state == ElementState::Pressed {
                    self.pressed = action;
                } else {
                    if action == self.pressed
                        && let Some(action) = action
                    {
                        self.action(action);
                    }
                    self.pressed = None;
                }
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed && !event.repeat =>
            {
                self.physical_key(event.physical_key)
            }
            WindowEvent::RedrawRequested if !self.occluded => self.draw(),
            _ => {}
        }
        self.turn(event_loop);
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.turn(event_loop);
        if !self.finished {
            if let Some(window) = &self.window
                && !self.occluded
            {
                window.request_redraw();
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(16),
            ));
        }
    }
    fn exiting(&mut self, _: &ActiveEventLoop) {
        self.finish_exit();
    }
}

fn application<'a>(
    options: Options,
    settings: Settings,
    resources: &'a Arc<Resources>,
    pack: PackResources,
    gui: UiResources,
    launch: ManagedLaunch,
) -> Result<FrozenApplication<'a>> {
    check_package(&options)?;
    let owner = client_game(
        resources,
        pack.clone(),
        options.clone(),
        settings.clone(),
        "MemoryOwner",
    )?;
    let guest = client_game(
        resources,
        pack.clone(),
        options.clone(),
        settings.clone(),
        "MemoryGuest",
    )?;
    let start = ManagedConnectedStart {
        resources: resources.clone(),
        pack,
        options: options.clone(),
        settings: settings.clone(),
        namespace: launch.root.join("authority"),
        request: ManagedRecoveryRequest {
            owner_root: launch.root.join("source-world"),
            expected_target: CounterTarget::new("minecraft:overworld", [2, 120, 2])?,
            expected_type: MACHINE.into(),
            archive_root: launch.root.join("source-archive"),
            destination: launch.root.join("recovered"),
            deadline: Instant::now() + WAIT,
        },
        continuation: launch.root.join("continuation"),
        attempt_timeout: WAIT,
        #[cfg(test)]
        first_stop_fault: None,
    };
    Ok(FrozenApplication {
        worker: None,
        clients: [owner, guest],
        start: Some(start),
        gui,
        settings,
        window_size: options.window_size,
        window: None,
        renderer: None,
        view: FrozenView::default(),
        pointer: None,
        pressed: None,
        control: None,
        snapshot: None,
        completion: None,
        finished: false,
        want_exit: false,
        close_submitted: false,
        offers: 0,
        signals: 0,
        error: String::new(),
        occluded: false,
        frames: 0,
        #[cfg(all(test, target_os = "linux"))]
        probe: None,
    })
}
pub(in crate::game) fn run(
    options: Options,
    settings: Settings,
    resources: &Arc<Resources>,
    pack: PackResources,
    gui: UiResources,
    launch: ManagedLaunch,
) -> Result<()> {
    let mut app = application(options, settings, resources, pack, gui, launch)?;
    #[cfg(all(test, target_os = "linux"))]
    let instrumented = tests::attach(&mut app);
    #[cfg(all(test, target_os = "linux"))]
    let event_loop = if instrumented {
        tests::event_loop()?
    } else {
        EventLoop::new()?
    };
    #[cfg(not(all(test, target_os = "linux")))]
    let event_loop = EventLoop::new()?;
    let result = event_loop.run_app(&mut app);
    app.finish_exit();
    #[cfg(all(test, target_os = "linux"))]
    tests::complete(&mut app);
    result?;
    match app
        .completion
        .take()
        .ok_or("frozen application returned without joined classification")?
    {
        Ok(_) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "managed_application_tests.rs"]
mod tests;
