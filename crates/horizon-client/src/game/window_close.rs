//! Explicit cold, in-frame native-world presentation. No ordinary startup or
//! gameplay handler enters this application, including while close is blocked.
use super::*;
use horizon_client::{
    control::{
        ClosedControl, RetainedControl, RetainedControlPhase, RetainedEndpointState, WindowCommand,
        WindowReply,
    },
    lod::retained::{ClosedLod, LodClosePhase, LodRefusal, RetainedLod},
    world::counter_restore::WorldId,
};
use paired_close::{CloseOwner, CloseProfile, CloseStatus, NativeReady};
use world_loading::{
    ColdPreparation, PreparationCloseStatus, PreparationClosed, RetainedEntryIdentity,
};

const CLOSE_WAIT: Duration = Duration::from_secs(10);

pub(super) struct Launch {
    root: PathBuf,
    args: Vec<String>,
}
impl Launch {
    pub(super) fn parse(args: &[String]) -> Result<Option<Self>> {
        if !args
            .iter()
            .any(|a| a.starts_with("--paired-native-window-close"))
        {
            return Ok(None);
        }
        if !cfg!(target_os = "linux") {
            return Err("native window close is supported only on Linux".into());
        }
        for name in [
            "HORIZON_SESSION_FILE",
            "HORIZON_PLAYER_NAME",
            "HORIZON_BENCHMARK",
            "HORIZON_COMMAND_HARNESS",
            "HORIZON_NET_HARNESS",
        ] {
            if std::env::var_os(name).is_some_and(|v| !v.is_empty()) {
                return Err(format!("native window close refuses {name}").into());
            }
        }
        let mut root = None;
        let mut ordinary = Vec::new();
        let mut input = args.iter();
        while let Some(arg) = input.next() {
            match arg.as_str() {
                "--paired-native-window-close" if root.is_none() => {
                    root = Some(PathBuf::from(
                        input
                            .next()
                            .ok_or("native window close needs a fresh absolute root")?,
                    ));
                }
                "--resources" | "--control-socket" | "--width" | "--height" | "--frames"
                | "--timeout-seconds" => {
                    ordinary.push(arg.clone());
                    ordinary.push(
                        input
                            .next()
                            .ok_or("missing native window option value")?
                            .clone(),
                    );
                }
                "--in-frame" | "--test-terrain" => {}
                _ => return Err(format!("unsupported native window option {arg}").into()),
            }
        }
        let root = root.ok_or("missing native window root")?;
        if !root.is_absolute()
            || root
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("native window root must be absolute without parent components".into());
        }
        match std::fs::symlink_metadata(&root) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err("native window root must not already exist".into()),
        }
        ordinary.push("--in-frame".into());
        ordinary.push("--test-terrain".into());
        Ok(Some(Self {
            root,
            args: ordinary,
        }))
    }
    pub(super) fn ordinary_args(&self) -> Vec<String> {
        self.args.clone()
    }
    pub(super) fn configure(&self, options: &mut Options) {
        options.server_thread = false;
        options.test_terrain = true;
        options.world = Some("native-window".into());
        options.distance = 2;
        options.distance_override = Some(2);
        options
            .control
            .get_or_insert_with(|| self.root.join("control.sock"));
    }
}

/// A fixed constructor-issued association. It is never reconstructed from an
/// observation, pointer address, command, or mutable Game supplied by a caller.
struct Construction {
    save: Arc<SaveDir>,
    world: WorldId,
    root: PathBuf,
}
impl Construction {
    fn from_prepared(prepared: &world_loading::PreparedWorld) -> Self {
        Self {
            save: prepared.save.clone(),
            world: *prepared
                .session_lock
                .expected_world()
                .expect("cold constructor identified world"),
            root: prepared.save_root.clone(),
        }
    }
    fn matches(&self, game: &Game<'_>) -> bool {
        Arc::ptr_eq(&self.save, &game.save)
            && self.root == game.world_root
            && game.session_lock.as_ref().and_then(|s| s.expected_world()) == Some(&self.world)
    }
}

enum LodCompletion {
    Closed(Box<ClosedLod>),
    Refused { _input: Box<LodRefusal> },
}
/// Only this application's fixed final transition can construct this value.
/// It consumes the actual original library completions, including failed native
/// cleanup. A failure handoff is never relabeled endpoint release success.
pub(super) struct ClientCompletion {
    construction: Construction,
    lod: LodCompletion,
    control: ClosedControl,
}
impl ClientCompletion {
    pub(super) fn matches(&self, game: &Game<'_>) -> bool {
        let lod_closed = match &self.lod {
            LodCompletion::Closed(closed) => closed.observe().phase == LodClosePhase::Ready,
            LodCompletion::Refused { .. } => true,
        };
        self.construction.matches(game)
            && lod_closed
            && self.control.observation().native_quiescent
            && game
                .renderer
                .as_ref()
                .is_some_and(Renderer::has_inline_occlusion_provenance)
            && game.window.is_some()
    }
}

struct ColdUi {
    pack: PackResources,
    gui: UiResources,
    renderer: Option<Renderer>,
    window: Option<Arc<Window>>,
}
struct ColdState {
    owner: Option<ColdPreparation>,
    closed: Option<PreparationClosed>,
    ui: ColdUi,
    entry: RetainedEntryIdentity,
}
enum Phase<'a> {
    Running(Box<Game<'a>>),
    Closing(Box<CloseOwner<'a>>),
    Ready(NativeReady<'a>),
    Cold(Box<ColdState>),
    Done,
}

struct Application<'a> {
    phase: Phase<'a>,
    construction: Option<Construction>,
    control: Option<RetainedControl>,
    closed_control: Option<ClosedControl>,
    lod: Option<RetainedLod>,
    closed_lod: Option<LodCompletion>,
    attempt: u64,
    frame_generation: u64,
    quit: bool,
    blocked: bool,
    detail: String,
    original_error: Option<String>,
    late_error: Option<String>,
    terminal_failure: bool,
    exit_acknowledged: bool,
    started: Instant,
    frames: u64,
    frame_limit: Option<u64>,
    timeout: Duration,
    timeout_observed: bool,
    unclaimed_columns: Vec<Arc<horizon_client::chunk::Chunk>>,
    status_presented: bool,
    surface_occluded: bool,
    #[cfg(all(
        test,
        feature = "native-writer-test-control",
        debug_assertions,
        target_os = "linux"
    ))]
    test: Option<tests::Control>,
}

pub(super) fn run(options: Options, launch: Launch) -> Result<()> {
    horizon_threads::catch_retained_stop_signals();
    let settings = Settings::load(&std::env::current_dir()?.join("options.txt"));
    preflight(&options, &settings)?;
    let root = std::fs::canonicalize(&options.resources)?;
    let started = Instant::now();
    let loaded = load_resources(
        root.clone(),
        &root,
        &settings,
        started,
        false,
        &Progress::new(),
        &mut |_, _| {},
    )?;
    if loaded.pack_error.is_some()
        || loaded.rest_loader.is_some()
        || std::fs::canonicalize(&loaded.root)? != root
    {
        return Err(
            "native window requires complete synchronous resources without fallback".into(),
        );
    }
    let gui = loaded
        .gui
        .ok_or("native window requires the complete GUI bundle")?;
    let resources = loaded.resources;
    let cpu = GameCpuResources::load(&root)?;
    let player =
        horizon_client::motion::PlayerMotion::new(resources.motion(), Vec3::new(0.5, 100.0, 0.5))?;
    let audio = Audio::silent(&root);
    let event_loop = new_event_loop()?;
    // Refuse an existing root, including a symlink, before original acquisition.
    std::fs::create_dir(&launch.root)?;
    let entry = RetainedEntryIdentity {
        token: 1,
        owner: Arc::new(()),
    };
    let mut request = world_loading::WorldRequest::new(
        "native-window",
        Some("Native window"),
        Some(17),
        false,
        None,
    )?
    .with_identity_intent(world_loading::host_identity::IdentityIntent::CreateNewIdentity);
    request.save_root = launch.root.join("world");
    let cold = ColdPreparation::prepare(
        request,
        &resources,
        &loaded.pack,
        &options,
        &settings,
        &entry,
    );
    let frame_limit = options.frames;
    let timeout = options.timeout;
    let control =
        RetainedControl::prepare_window(options.control.as_ref().expect("profile control path"));
    let mut application = Application {
        phase: Phase::Done,
        construction: None,
        control: Some(control),
        closed_control: None,
        lod: None,
        closed_lod: None,
        attempt: 0,
        frame_generation: 0,
        quit: false,
        blocked: false,
        detail: String::new(),
        original_error: None,
        late_error: None,
        terminal_failure: false,
        exit_acknowledged: false,
        started,
        frames: 0,
        frame_limit,
        timeout,
        timeout_observed: false,
        unclaimed_columns: Vec::new(),
        status_presented: false,
        surface_occluded: false,
        #[cfg(all(
            test,
            feature = "native-writer-test-control",
            debug_assertions,
            target_os = "linux"
        ))]
        test: tests::Control::from_environment(),
    };
    match cold.into_prepared(&entry) {
        Ok(prepared) => {
            application.construction = Some(Construction::from_prepared(prepared.prepared()));
            let mut game = Game::new_with_world(
                &resources,
                loaded.pack,
                gui,
                options,
                settings,
                audio,
                Some(cpu),
                InitialGameWorld::Retained(Box::new(prepared)),
            )?;
            game.player = Some(player);
            game.previous_position = Vec3::new(0.5, 100.0, 0.5);
            game.streaming.radius = 2;
            game.streaming.center = (0, 0);
            game.load_world_state();
            let source = game.streaming.lod_source();
            let capability = source.clone().retained_lod_capability();
            match RetainedLod::prepare(source, capability, game.pack.context.clone()) {
                Ok(lod) => application.lod = Some(lod),
                Err(refusal) => {
                    application.original_error = Some(refusal.failure.message.clone());
                    application.closed_lod = Some(LodCompletion::Refused {
                        _input: Box::new(refusal),
                    });
                }
            }
            application.phase = Phase::Running(Box::new(game));
            if let Some(lod) = &mut application.lod
                && let Err(error) = lod.start()
            {
                application.original_error = Some(error.message);
            }
        }
        Err(cold) => {
            application.original_error = Some(
                cold.original_error()
                    .unwrap_or("cold transfer refused")
                    .to_owned(),
            );
            application.phase = Phase::Cold(Box::new(ColdState {
                owner: Some(cold),
                closed: None,
                ui: ColdUi {
                    pack: loaded.pack,
                    gui,
                    renderer: None,
                    window: None,
                },
                entry,
            }));
        }
    }
    #[cfg(all(
        test,
        feature = "native-writer-test-control",
        debug_assertions,
        target_os = "linux"
    ))]
    if let Some(test) = &mut application.test {
        test.prepare_control(application.control.as_mut().unwrap());
    }
    // Store the original controller before any actual listener acquisition.
    if let Err(error) = application.control.as_mut().unwrap().start() {
        application.original_error.get_or_insert(error);
    }
    if application.original_error.is_some() {
        application.begin_close(true);
    }
    let outcome = event_loop.run_app(&mut application);
    if !matches!(application.phase, Phase::Done) {
        // The OS event loop has irreversibly stopped. Do not convert missing
        // native completion into a successful report or legacy save fallback.
        eprintln!("fatal: native window event loop ended with unresolved original owners");
        std::process::abort();
    }
    outcome?;
    if let Some(error) = application.late_error.take() {
        log::warn!("native window additional terminal failure: {error}");
    }
    if let Some(error) = application.original_error.take() {
        return Err(error.into());
    }
    Ok(())
}

fn new_event_loop() -> Result<EventLoop<()>> {
    let mut builder = EventLoop::builder();
    #[cfg(all(test, target_os = "linux"))]
    {
        // Rust's exact test runs on its own thread. This only changes the
        // event-loop thread admission; the real window/renderer/Application are
        // the same production path and remain on that owning thread.
        winit::platform::x11::EventLoopBuilderExtX11::with_any_thread(&mut builder, true);
        winit::platform::wayland::EventLoopBuilderExtWayland::with_any_thread(&mut builder, true);
    }
    Ok(builder.build()?)
}

fn preflight(options: &Options, settings: &Settings) -> Result<()> {
    if !cfg!(target_os = "linux")
        || options.server_thread
        || !options.test_terrain
        || options.server.is_some()
        || options.dedicated.is_some()
        || options.dimension.is_some()
        || options.benchmark.is_some()
        || options.offscreen
        || !options.dev_mods.is_empty()
        || options.mod_packages.has_pairs()
        || !settings.mods.enabled.is_empty()
        || !settings.resource_packs.is_empty()
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
        || options.control.is_none()
    {
        return Err(
            "native window refuses unsupported topology, resources, extras or No AI control policy"
                .into(),
        );
    }
    Ok(())
}

/// Fixed renderer construction, never an upgrade of an existing renderer.
pub(super) fn renderer(
    window: Arc<Window>,
    pack: &PackResources,
    gui: &UiResources,
    api: horizon_client::graphics::GraphicsApi,
) -> Result<Renderer> {
    let assets = &pack.render;
    let atlas = &gui.atlas;
    atlas.font.reset_cache();
    let mut renderer = pollster::block_on(Renderer::new_inline_occlusion(
        window,
        false,
        &assets.pixels,
        assets.sprite_size,
        assets.sprite_minimum_size,
        assets.texture_layers,
        &assets.sprite_mips,
        &assets.animations,
        (&atlas.pixels, atlas.width, atlas.height),
        (&atlas.glint.0, atlas.glint.1, atlas.glint.2),
        (
            &atlas.glint_armor.0,
            atlas.glint_armor.1,
            atlas.glint_armor.2,
        ),
        (&pack.forcefield.2, pack.forcefield.0),
        &gui.panorama,
        None,
        None,
        api,
    ))?;
    renderer.hud.sprite_animations = atlas
        .sprite_animations
        .iter()
        .map(|(rect, animation)| (*rect, animation.clone(), (usize::MAX, 0, 0)))
        .collect();
    Ok(renderer)
}

/// Fixed UI projection shared by the original Box while running, closing, and
/// native-ready. These operations touch only the original renderer/window/HUD.
pub(super) fn draw_status(game: &mut Game<'_>, text: &str) -> Result<bool> {
    draw_hud(game.renderer.as_mut(), &game.gui, text)
}
fn draw_hud(renderer: Option<&mut Renderer>, gui: &UiResources, text: &str) -> Result<bool> {
    if let Some(renderer) = renderer {
        let mut hud = HudBatch::new([renderer.config.width, renderer.config.height]);
        for (index, line) in text.lines().take(6).enumerate() {
            hud.text_styled(
                &gui.atlas,
                &horizon_client::text::Text::literal(line),
                12.0,
                20.0 + index as f32 * 14.0,
                u32::MAX,
                true,
            );
        }
        return renderer.render(&loading_frame(&hud));
    }
    Ok(false)
}

#[derive(Clone, Copy)]
pub(super) struct Presentation {
    id: Option<WindowId>,
    usable: bool,
    focused: bool,
}
pub(super) fn presentation(game: &Game<'_>) -> Presentation {
    Presentation {
        id: game.window.as_ref().map(|w| w.id()),
        focused: game.window.as_ref().is_some_and(|w| w.has_focus()),
        usable: game
            .window
            .as_ref()
            .is_some_and(|w| w.is_visible() != Some(false) && w.is_minimized() != Some(true))
            && game
                .renderer
                .as_ref()
                .is_some_and(Renderer::has_inline_occlusion_provenance),
    }
}
pub(super) fn resize(game: &mut Game<'_>, width: u32, height: u32) {
    if let Some(renderer) = &mut game.renderer {
        renderer.resize(width, height);
    }
}
pub(super) fn request_redraw(game: &Game<'_>, title: &str) {
    if let Some(window) = &game.window {
        window.set_title(title);
        window.request_redraw();
    }
}

impl Application<'_> {
    fn view(&self) -> Presentation {
        match &self.phase {
            Phase::Running(game) => presentation(game),
            Phase::Closing(owner) => owner.window_presentation(),
            Phase::Ready(ready) => ready.window_presentation(),
            Phase::Cold(cold) => Presentation {
                id: cold.ui.window.as_ref().map(|w| w.id()),
                focused: cold.ui.window.as_ref().is_some_and(|w| w.has_focus()),
                usable: cold.ui.window.as_ref().is_some_and(|w| {
                    w.is_visible() != Some(false) && w.is_minimized() != Some(true)
                }) && cold
                    .ui
                    .renderer
                    .as_ref()
                    .is_some_and(Renderer::has_inline_occlusion_provenance),
            },
            Phase::Done => Presentation {
                id: None,
                usable: false,
                focused: false,
            },
        }
    }
    fn stage(&self) -> &'static str {
        if self.terminal_failure {
            "terminal-error: Enter exits; completed cleanup cannot Retry"
        } else if self.closed_control.is_some() {
            "complete"
        } else if self
            .control
            .as_ref()
            .is_some_and(|c| c.observation().admission_sealed)
        {
            "control-final"
        } else {
            match self.phase {
                Phase::Running(_) => "running fixed view: Esc closes; Q quits",
                Phase::Closing(_) if self.lod.is_some() => "closing LOD",
                Phase::Closing(_) => "closing native world",
                Phase::Ready(_) => "world closed; Q quits",
                Phase::Cold(_) => "closing original failed preparation",
                Phase::Done => "complete",
            }
        }
    }
    fn status_text(&self) -> String {
        format!(
            "{}\nAttempt {}{}\n{}\n{}",
            self.stage(),
            self.attempt,
            if self.blocked { " BLOCKED" } else { "" },
            if self.terminal_failure {
                "Enter: acknowledge failure and exit (no cleanup replay)"
            } else {
                "R: Retry / restore status; Q: quit; Esc: close world"
            },
            brief(&self.detail)
        )
    }
    fn fail(&mut self, error: impl Into<String>) {
        let error = error.into();
        self.original_error.get_or_insert_with(|| error.clone());
        if self.terminal_failure {
            // The actual native cleanup outcome stays visible until explicit
            // acknowledgment. Keep later UI/timeout diagnostics separately.
            self.late_error = Some(error);
        } else {
            self.detail = error;
        }
        self.begin_close(true);
    }
    fn begin_close(&mut self, quit: bool) {
        self.quit |= quit;
        if self.attempt != 0 {
            return;
        }
        self.attempt = 1;
        self.status_presented = false;
        let now = Instant::now();
        if let Some(lod) = &mut self.lod {
            lod.begin_close(now, CLOSE_WAIT);
        }
        let phase = std::mem::replace(&mut self.phase, Phase::Done);
        self.phase = match phase {
            Phase::Running(game) => Phase::Closing(CloseOwner::begin(
                game,
                CloseProfile::NativeWorld,
                now + CLOSE_WAIT,
            )),
            Phase::Cold(mut cold) => {
                if let Some(owner) = &mut cold.owner {
                    owner.begin_close(now + CLOSE_WAIT);
                }
                Phase::Cold(cold)
            }
            other => other,
        };
    }
    fn retry(&mut self, expected: u64) -> bool {
        if !self.blocked || expected != self.attempt {
            return false;
        }
        let Some(next) = self.attempt.checked_add(1) else {
            return false;
        };
        let now = Instant::now();
        let accepted = if let Some(control) = self
            .control
            .as_mut()
            .filter(|c| c.observation().admission_sealed)
        {
            control.retry_close(now + CLOSE_WAIT).is_ok()
        } else if let Some(lod) = &mut self.lod {
            lod.retry_close(now, CLOSE_WAIT).is_ok()
        } else {
            match &mut self.phase {
                Phase::Closing(owner) => owner.retry(now + CLOSE_WAIT).is_ok(),
                Phase::Cold(cold) => cold
                    .owner
                    .as_mut()
                    .is_some_and(|o| o.retry_close(now + CLOSE_WAIT).is_ok()),
                _ => false,
            }
        };
        if accepted {
            self.attempt = next;
            self.blocked = false;
            self.detail.clear();
        }
        accepted
    }
    fn poll_control(&mut self) {
        let Some(generation) = self.frame_generation.checked_add(1) else {
            self.fail("native window frame generation exhausted");
            return;
        };
        self.frame_generation = generation;
        if let Some(control) = &mut self.control {
            control.grant_window_frame(generation);
        }
        let started = Instant::now();
        for _ in 0..8 {
            if started.elapsed() >= Duration::from_millis(2) {
                break;
            }
            let Some(request) = self
                .control
                .as_mut()
                .and_then(RetainedControl::try_recv_window)
            else {
                break;
            };
            match request.command() {
                WindowCommand::Status | WindowCommand::CloseStatus => {
                    let stage = self.stage();
                    let detail = brief(&self.detail);
                    let _ = request.reply(WindowReply::Status {
                        phase: if self.blocked { "blocked" } else { "active" },
                        stage,
                        attempt: self.attempt,
                        detail,
                        world_closed: matches!(self.phase, Phase::Ready(_)),
                        control_sealed: false,
                    });
                }
                WindowCommand::Close { quit } => {
                    self.begin_close(quit);
                    let _ = request.reply(WindowReply::Admission {
                        accepted: true,
                        attempt: self.attempt,
                    });
                }
                WindowCommand::Retry { expected_attempt } => {
                    let accepted = self.retry(expected_attempt);
                    let _ = request.reply(WindowReply::Admission {
                        accepted,
                        attempt: self.attempt,
                    });
                }
                WindowCommand::Unsupported => {
                    let _ = request.reply(WindowReply::Error { message: "fixed view admits only status, close-status, close, world-leave, quit and close-retry ATTEMPT" });
                }
            }
        }
    }
    fn poll_close(&mut self) {
        if self.attempt == 0 {
            return;
        }
        if let Some(mut lod) = self.lod.take() {
            let status = lod.poll_close(Instant::now());
            if status.phase == LodClosePhase::Ready {
                match lod.try_closed(Instant::now()) {
                    Ok(closed) => {
                        if let Some(failure) = closed
                            .observe()
                            .workers
                            .workers
                            .iter()
                            .find_map(|w| w.failure.as_ref())
                        {
                            self.original_error
                                .get_or_insert_with(|| failure.message.clone());
                            self.detail = failure.message.clone();
                        }
                        self.closed_lod = Some(LodCompletion::Closed(Box::new(closed)));
                    }
                    Err(lod) => self.lod = Some(lod),
                }
            } else {
                if status.phase == LodClosePhase::Blocked {
                    self.blocked = true;
                    self.detail = "LOD work remains owned; R retries the current attempt".into();
                }
                self.lod = Some(lod);
            }
            if self.lod.is_some() {
                return;
            }
        }
        match &mut self.phase {
            Phase::Closing(owner) => match owner.poll() {
                CloseStatus::Pending(_) => {}
                CloseStatus::Blocked(error) => {
                    self.blocked = true;
                    self.detail = error;
                }
                CloseStatus::NativeReady => {
                    let Phase::Closing(owner) = std::mem::replace(&mut self.phase, Phase::Done)
                    else {
                        unreachable!()
                    };
                    self.phase = match owner.try_native_ready() {
                        Ok(ready) => Phase::Ready(ready),
                        Err(owner) => Phase::Closing(owner),
                    };
                    self.blocked = false;
                    self.status_presented = false;
                }
            },
            Phase::Cold(cold) => {
                if let Some(owner) = &mut cold.owner {
                    match owner.poll_close() {
                        PreparationCloseStatus::Pending => {}
                        PreparationCloseStatus::Blocked(error) => {
                            self.blocked = true;
                            self.detail = error;
                        }
                        PreparationCloseStatus::Complete => {
                            match cold.owner.take().unwrap().finish_close() {
                                Ok(closed) => {
                                    cold.closed = Some(closed);
                                    self.blocked = false;
                                    self.status_presented = false;
                                }
                                Err(owner) => cold.owner = Some(owner),
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        let world_closed = matches!(self.phase, Phase::Ready(_))
            || matches!(&self.phase, Phase::Cold(c) if c.closed.is_some());
        if !world_closed || !self.quit || !self.unclaimed_columns.is_empty() {
            return;
        }
        // Once sealed, keep polling actual control completion regardless of
        // drawing. Before seal, require a positively presented original status
        // frame after WORLD completion and no later surface invalidation.
        let sealed = self.closed_control.is_some()
            || self
                .control
                .as_ref()
                .is_some_and(|control| control.observation().admission_sealed);
        if !sealed && (!self.status_presented || self.surface_occluded || !self.view().usable) {
            return;
        }
        if let Some(control) = &mut self.control {
            control.begin_close(Instant::now() + CLOSE_WAIT);
            let observation = control.poll_close().observation;
            #[cfg(all(
                test,
                feature = "native-writer-test-control",
                debug_assertions,
                target_os = "linux"
            ))]
            if let Some(test) = &mut self.test {
                test.control_seal_admitted(observation.admission_sealed);
            }
            if observation.phase == RetainedControlPhase::Blocked {
                self.blocked = true;
                self.detail =
                    "native control work remains owned; R retries the current wait".into();
            }
            if observation.native_quiescent {
                match self.control.take().unwrap().try_closed() {
                    Ok(closed) => {
                        self.terminal_failure = !matches!(
                            observation.endpoint,
                            RetainedEndpointState::Released | RetainedEndpointState::Unpublished
                        ) || observation.failures.iter().any(|f| {
                            !matches!(
                                f.kind,
                                horizon_client::control::RetainedFailureKind::Deadline
                            )
                        });
                        if self.terminal_failure {
                            self.detail = observation
                                .failures
                                .iter()
                                .rev()
                                .find(|failure| {
                                    !matches!(
                                        failure.kind,
                                        horizon_client::control::RetainedFailureKind::Deadline
                                    )
                                })
                                .map(|failure| {
                                    format!(
                                        "{:?}: {:?}: {}",
                                        failure.stage, failure.kind, failure.detail
                                    )
                                })
                                .unwrap_or_else(|| format!("Endpoint {:?}", observation.endpoint));
                            self.original_error.get_or_insert_with(|| {
                                format!(
                                    "native control terminal failure: {:?}",
                                    observation.failures
                                )
                            });
                        }
                        self.closed_control = Some(closed);
                        self.blocked = false;
                    }
                    Err(control) => self.control = Some(control),
                }
            }
        }
    }
    fn complete(&mut self, event_loop: &ActiveEventLoop) {
        if self.closed_control.is_none() || (self.terminal_failure && !self.exit_acknowledged) {
            return;
        }
        let phase = std::mem::replace(&mut self.phase, Phase::Done);
        match phase {
            Phase::Ready(ready) => {
                let completion = ClientCompletion {
                    construction: self
                        .construction
                        .take()
                        .expect("original Game constructor custody"),
                    lod: self
                        .closed_lod
                        .take()
                        .expect("original LOD completion or constructor refusal"),
                    control: self.closed_control.take().unwrap(),
                };
                match ready.try_dispose_window(completion) {
                    Ok(_) => event_loop.exit(),
                    Err((ready, completion, error)) => {
                        self.phase = Phase::Ready(ready);
                        self.construction = Some(completion.construction);
                        self.closed_lod = Some(completion.lod);
                        self.closed_control = Some(completion.control);
                        self.blocked = true;
                        self.detail = error;
                    }
                }
            }
            Phase::Cold(cold)
                if cold
                    .closed
                    .as_ref()
                    .is_some_and(|c| c.matches_entry(&cold.entry)) =>
            {
                // Actual preparation closure precedes owning-thread UI disposal.
                drop(cold);
                drop(self.closed_control.take());
                event_loop.exit();
            }
            other => self.phase = other,
        }
    }
    fn redraw(&mut self) {
        #[cfg(all(
            test,
            feature = "native-writer-test-control",
            debug_assertions,
            target_os = "linux"
        ))]
        if self
            .test
            .as_mut()
            .is_some_and(|test| test.inject_draw_error(self.terminal_failure))
        {
            self.status_presented = false;
            self.fail("controlled native frame error");
            return;
        }
        let stage = self.status_text();
        #[cfg(all(
            test,
            feature = "native-writer-test-control",
            debug_assertions,
            target_os = "linux"
        ))]
        if let Some(test) = &mut self.test {
            test.before_draw();
        }
        let result = match &mut self.phase {
            Phase::Running(game) => game.render(),
            Phase::Closing(owner) => owner.draw_window_status(&stage),
            Phase::Ready(ready) => ready.draw_window_status(&stage),
            Phase::Cold(cold) => draw_hud(cold.ui.renderer.as_mut(), &cold.ui.gui, &stage),
            Phase::Done => Ok(false),
        };
        #[cfg(all(
            test,
            feature = "native-writer-test-control",
            debug_assertions,
            target_os = "linux"
        ))]
        if let Some(test) = &mut self.test {
            test.after_draw(&result);
        }
        match result {
            Ok(presented) => {
                if matches!(self.phase, Phase::Running(_)) {
                    if presented {
                        self.frames += 1;
                    }
                } else {
                    self.status_presented = presented;
                }
                #[cfg(all(
                    test,
                    feature = "native-writer-test-control",
                    debug_assertions,
                    target_os = "linux"
                ))]
                if presented && let Some(test) = &mut self.test {
                    test.presented(matches!(self.phase, Phase::Running(_)));
                }
            }
            Err(error) => {
                self.status_presented = false;
                self.fail(error.to_string());
            }
        }
    }
    fn update_view(&mut self) {
        if let Some(failure) = self.lod.as_ref().and_then(|lod| {
            lod.status()
                .workers
                .into_iter()
                .find_map(|worker| worker.failure)
        }) {
            self.fail(failure.message);
        }
        let Phase::Running(game) = &mut self.phase else {
            return;
        };
        let (uploads, unloaded) = game.streaming.update(&mut game.world);
        // This fixed constructor admits one constant center/radius and no
        // movement/dimension/replacement commands. Unexpected unload cannot be
        // silently discarded or counted as saved: retain its original Arc and
        // refuse terminal seal until a separately supported disposition exists.
        let unexpected = !unloaded.is_empty();
        self.unclaimed_columns.extend(unloaded);
        if let Some(renderer) = &mut game.renderer {
            for (key, mesh) in uploads {
                renderer.upload(key, &mesh);
            }
            if let Some(lod) = &mut self.lod {
                for chunk in game.world.chunks.values() {
                    let _ = lod.observe_chunk(chunk.clone(), game.res.facts());
                }
                if let Ok(meshes) = lod.update_frame([0.5, 100.0, 0.5], game.yaw, 2, 32, None) {
                    let removed = lod.removed().unwrap_or_default();
                    let cutout = lod.take_cutout().unwrap_or_default();
                    renderer.queue_lods(meshes, removed, cutout);
                    renderer.upload_lods(Duration::from_millis(2));
                }
            }
        }
        if unexpected {
            self.fail("fixed world view observed an unsupported unload; original columns retained, terminal seal refused");
            self.blocked = true;
        }
    }
}

fn brief(value: &str) -> &str {
    let end = value
        .char_indices()
        .nth(256)
        .map_or(value.len(), |(i, _)| i);
    &value[..end]
}

impl ApplicationHandler for Application<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.view().id.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title("Horizon native window")
            .with_inner_size(winit::dpi::LogicalSize::new(960.0, 540.0));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                self.fail(error.to_string());
                return;
            }
        };
        let result = match &mut self.phase {
            Phase::Running(game) => {
                // Store original window even if renderer acquisition fails.
                game.window = Some(window.clone());
                renderer(window, &game.pack, &game.gui, game.settings.horizon.api).map(|renderer| {
                    game.renderer = Some(renderer);
                    game.world_renderer_ready = true;
                })
            }
            Phase::Closing(owner) => owner.create_window_presentation(window),
            Phase::Ready(_) => {
                Err("native-ready reached before original presentation construction".into())
            }
            Phase::Cold(cold) => {
                cold.ui.window = Some(window.clone());
                renderer(
                    window,
                    &cold.ui.pack,
                    &cold.ui.gui,
                    horizon_client::graphics::GraphicsApi::default(),
                )
                .map(|renderer| cold.ui.renderer = Some(renderer))
            }
            Phase::Done => Ok(()),
        };
        if let Err(error) = result {
            self.fail(error.to_string());
        }
    }
    fn window_event(&mut self, _event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if self.view().id != Some(id) {
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                if self.terminal_failure {
                    self.exit_acknowledged = true;
                } else {
                    self.begin_close(true);
                }
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } if event.state == ElementState::Pressed && !event.repeat => {
                match event.physical_key {
                    PhysicalKey::Code(KeyCode::KeyR) => {
                        // A mapped X11 window need not receive Occluded(false).
                        // Recovery requires real native input to this original,
                        // focused, visible/non-minimized presentation. This only
                        // invalidates the old frame; a NEW successful status draw
                        // is still required before terminal control admission.
                        let view = self.view();
                        if !is_synthetic && self.surface_occluded && view.focused && view.usable {
                            self.surface_occluded = false;
                            self.status_presented = false;
                            #[cfg(all(
                                test,
                                feature = "native-writer-test-control",
                                debug_assertions,
                                target_os = "linux"
                            ))]
                            if let Some(test) = &mut self.test {
                                test.status_recovery_input();
                            }
                        }
                        self.retry(self.attempt);
                    }
                    PhysicalKey::Code(KeyCode::Enter) if self.terminal_failure => {
                        self.exit_acknowledged = true
                    }
                    PhysicalKey::Code(KeyCode::Escape) => self.begin_close(false),
                    PhysicalKey::Code(KeyCode::KeyQ) => self.begin_close(true),
                    _ => {}
                }
            }
            WindowEvent::Occluded(occluded) => {
                self.surface_occluded = occluded;
                self.status_presented = false;
            }
            WindowEvent::Resized(size) => {
                self.status_presented = false;
                #[cfg(all(
                    test,
                    feature = "native-writer-test-control",
                    debug_assertions,
                    target_os = "linux"
                ))]
                if let Some(test) = &mut self.test {
                    test.native_resizes += 1;
                }
                match &mut self.phase {
                    Phase::Running(game) => resize(game, size.width, size.height),
                    Phase::Closing(owner) => owner.resize_window(size.width, size.height),
                    Phase::Ready(ready) => ready.resize_window(size.width, size.height),
                    Phase::Cold(cold) => {
                        if let Some(renderer) = &mut cold.ui.renderer {
                            renderer.resize(size.width, size.height);
                        }
                    }
                    Phase::Done => {}
                }
            }
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if horizon_threads::stop_requests() > 0 {
            self.begin_close(true);
        }
        if self.frame_limit.is_some_and(|n| self.frames >= n) {
            self.begin_close(true);
        }
        if !self.timeout_observed && self.started.elapsed() >= self.timeout {
            self.timeout_observed = true;
            self.fail("native window time limit reached");
        }
        self.poll_control(); // Always before close polling, including Blocked.
        #[cfg(all(
            test,
            feature = "native-writer-test-control",
            debug_assertions,
            target_os = "linux"
        ))]
        if let Some(mut test) = self.test.take() {
            test.before_poll(self);
            self.test = Some(test);
        }
        self.update_view();
        self.poll_close();
        self.complete(event_loop);
        let title = self.status_text().replace('\n', " | ");
        #[cfg(all(
            test,
            feature = "native-writer-test-control",
            debug_assertions,
            target_os = "linux"
        ))]
        let title = match self
            .test
            .as_ref()
            .and_then(tests::Control::native_title_tag)
        {
            Some(tag) => format!("{title} | {tag}"),
            None => title,
        };
        match &self.phase {
            Phase::Running(game) => request_redraw(game, &title),
            Phase::Closing(owner) => owner.redraw_window(&title),
            Phase::Ready(ready) => ready.redraw_window(&title),
            Phase::Cold(cold) => {
                if let Some(window) = &cold.ui.window {
                    window.set_title(&title);
                    window.request_redraw();
                }
            }
            Phase::Done => {}
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(8),
        ));
    }
    fn exiting(&mut self, _: &ActiveEventLoop) {
        if !matches!(self.phase, Phase::Done) {
            eprintln!("fatal: unresolved original native window exit");
            std::process::abort();
        }
    }
}

#[cfg(test)]
#[path = "window_close_tests.rs"]
mod tests;
