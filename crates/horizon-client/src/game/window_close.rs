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

#[cfg(all(test, feature = "mods", target_os = "linux"))]
pub(in crate::game) fn ordinary_owned_dispatch_refusal_controls(
    working: &std::path::Path,
    anchor: &std::path::Path,
    original: Arc<paired_close::OrdinaryClosedWorld>,
) {
    tests::ordinary_owned_dispatch_refusal_controls(working, anchor, original);
}

#[cfg(test)]
pub(super) fn hold_ordinary_rest_loader() -> std::result::Result<(), String> {
    tests::hold_ordinary_rest_loader()
}

#[cfg(feature = "mods")]
fn pump_ordinary(event_loop: &mut EventLoop<()>, application: &mut impl ApplicationHandler) {
    use winit::platform::pump_events::{EventLoopExtPumpEvents, PumpStatus};
    if let PumpStatus::Exit(_) =
        event_loop.pump_app_events(Some(Duration::from_millis(8)), application)
    {
        eprintln!("fatal: ordinary event loop stopped outside its owned completion transition");
        std::process::abort();
    }
}

/// Normal selected-package entry, bounded to the owner's explicit test provider.
/// Every Game borrows one lexical resource generation; only owned presentation
/// returns from its completed application. No title Game or leaked Arc is used.
#[cfg(feature = "mods")]
pub(super) fn run_ordinary(
    options: Options,
    settings: Settings,
    vanilla_root: PathBuf,
    started: Instant,
) -> Result<()> {
    if !cfg!(target_os = "linux")
        || cfg!(feature = "audio")
        || cfg!(feature = "controllers")
        || !options.test_terrain
        || !options.mod_packages.has_pairs()
        || settings.horizon.lod_distance != 0
        || options.dimension.is_some()
        || options.benchmark.is_some()
        || options.offscreen
    {
        return Err("selected paired S1 startup requires Linux, explicit --test-terrain, no LOD/audio/controllers, and the bounded Overworld profile".into());
    }
    horizon_threads::catch_retained_stop_signals();
    if options.execution_role == execution_role::ExecutionRole::DedicatedServer {
        return retained_server::run_ordinary_dedicated(options, settings, vanilla_root, started);
    }
    let mut event_loop = new_event_loop()?;
    let menu = startup_menu::StartupMenu::new(&options, None)?;
    let progress = Progress::new();
    let (receiver, handle) =
        startup_menu::spawn_loader(&options, &settings, &vanilla_root, started, &progress)?;
    let mut frontend = Loading {
        retained: Some(RetainedFrontendState::default()),
        window: None,
        renderer: None,
        gui: None,
        settings,
        fullscreen: horizon_client::window::FullscreenState::new(false),
        options: Some(options),
        vanilla_root,
        pack_error: None,
        started,
        panorama_rotation: Default::default(),
        progress,
        receiver,
        loaded: None,
        handle: Some(handle),
        shown: false,
        menu,
        error: None,
    };
    let mut entry_token = 0u64;
    loop {
        while !frontend.retained_frontend_helpers_closed()
            || (!frontend.retained_menu_closing() && !matches!(frontend.loaded, Some(Ok(_))))
        {
            pump_ordinary(&mut event_loop, &mut frontend);
        }
        if frontend.retained_menu_closing() {
            break;
        }
        let loaded = frontend.take_retained_generation()?;
        // This call owns Resources lexically until all borrowing worlds finish.
        let reload = ordinary_generation(&mut event_loop, &mut frontend, loaded, &mut entry_token)?;
        if !reload || frontend.retained_menu_closing() {
            break;
        }
        let options = frontend
            .options
            .as_mut()
            .expect("original frontend options");
        options.mod_packages =
            mod_packages::Packages::read(&frontend.settings.mods.enabled, &options.dev_mods)?;
        if !options.mod_packages.has_pairs() {
            return Err("changing to an unpaired startup requires a new launch".into());
        }
        let progress = Progress::new();
        let (receiver, handle) = startup_menu::spawn_loader(
            options,
            &frontend.settings,
            &frontend.vanilla_root,
            started,
            &progress,
        )?;
        frontend.menu.reload_packs = false;
        frontend.retained.as_mut().unwrap().generation_issued = false;
        frontend.receiver = receiver;
        frontend.handle = Some(handle);
        frontend.progress = progress;
    }
    // No event-loop exit was used for a phase change. Resource helpers and every
    // world application are actually complete before this owning-thread disposal.
    if let Some(error) = frontend.error.take() {
        return Err(error);
    }
    Ok(())
}

#[cfg(feature = "mods")]
fn ordinary_generation(
    event_loop: &mut EventLoop<()>,
    frontend: &mut Loading,
    loaded: Loaded,
    entry_token: &mut u64,
) -> Result<bool> {
    if loaded.rest_loader.is_some() || loaded.pack_error.is_some() {
        return Err(
            "paired generation requires actual completed resources without fallback".into(),
        );
    }
    let resources = loaded.resources;
    let root = loaded.root;
    let mut pack = Some(loaded.pack);
    let mut cpu = loaded.game_cpu;
    if frontend.gui.is_none() {
        frontend.gui = loaded.gui;
    }
    let sync = Arc::new(horizon_client::lan::data::SyncData::load(&root)?);
    let startup = Arc::new(
        frontend
            .options
            .as_ref()
            .expect("original options")
            .mod_packages
            .freeze_peer_inputs(sync, pack.as_ref().unwrap().assets.registry)?
            .ok_or("ordinary selected-pair generation missing its captured package owners")?,
    );
    loop {
        if frontend.retained_menu_closing() {
            return Ok(false);
        }
        if frontend.menu.reload_packs {
            return Ok(true);
        }
        if let Some(entry) = frontend.menu.pending.take() {
            let options = frontend.options.as_ref().expect("original options");
            let mut options = options.clone();
            options.resources = root.clone();
            let settings = frontend.settings.clone();
            let result: Result<Option<(ReturnedFrontend, bool, Option<String>)>> = (|| {
                let gui = frontend
                    .gui
                    .as_ref()
                    .ok_or("complete menu bundle missing")?;
                let window = frontend
                    .window
                    .clone()
                    .ok_or("original frontend window missing")?;
                if !frontend
                    .renderer
                    .as_ref()
                    .is_some_and(Renderer::has_inline_occlusion_provenance)
                {
                    if let Some(old) = &mut frontend.renderer {
                        old.release_surface();
                    }
                    frontend.renderer = Some(renderer(
                        window,
                        pack.as_ref().unwrap(),
                        gui,
                        settings.horizon.api,
                    )?);
                }
                let mut prepared_cpu = match cpu.take() {
                    Some(cpu) => cpu,
                    None => GameCpuResources::load(&root)?,
                };
                prepare_ordinary_creative(
                    &mut prepared_cpu,
                    &resources,
                    &options.mod_packages,
                    pack.as_ref().unwrap(),
                    &startup,
                )?;
                *entry_token = entry_token
                    .checked_add(1)
                    .ok_or("ordinary entry token exhausted")?;
                let association = RetainedEntryIdentity {
                    token: *entry_token,
                    owner: Arc::new(()),
                };
                let mut application = match entry {
                    PendingWorldEntry::Server {
                        address,
                        name,
                        cookies,
                    } => {
                        if cookies.is_some() {
                            return Err("paired S1 transfer cookies are unsupported".into());
                        }
                        let activated = startup
                            .activate(
                                options.execution_role,
                                options.server_thread,
                                true,
                                &settings,
                            )
                            .map_err(|refused| refused.reason.clone())?;
                        let prepared = RemotePreparation::new_paired(
                            options.clone(),
                            settings,
                            prepared_cpu,
                            name,
                            startup.clone(),
                            pack.as_ref().unwrap().assets.registry,
                        )?;
                        let attached = match prepared.attach(
                            horizon_client::net::RetainedTarget::Address(address.clone()),
                            address,
                            RemoteFrontend::External,
                        ) {
                            Ok(attached) => attached,
                            Err((_original, _target, reason)) => return Err(reason.into()),
                        };
                        let mut game = attached.into_game(
                            &resources,
                            pack.take().unwrap(),
                            frontend.gui.take().unwrap(),
                        );
                        if let Err(original) = game.adopt_activated_startup(activated) {
                            eprintln!(
                                "fatal: original external client runtime association refused: {}",
                                original.1
                            );
                            std::process::abort();
                        }
                        install_ordinary_presentation(&mut game, frontend);
                        ordinary_application(
                            Phase::RemoteRunning(Box::new(game)),
                            &options,
                            None,
                            None,
                            None,
                            frontend,
                        )
                    }
                    local => {
                        let request = ordinary_local_request(local)?;
                        let original = frontend
                            .retained
                            .as_ref()
                            .unwrap()
                            .closed_worlds
                            .iter()
                            .find(|original| original.root() == request.save_root)
                            .cloned();
                        if options.server_thread {
                            let selection = request.into_ordinary_network(
                                &std::env::current_dir()?,
                                original,
                                Instant::now() + Duration::from_secs(120),
                            )?;
                            let backend_cpu = GameCpuResources::load(&root)?;
                            let tree = Arc::new(horizon_client::advancements::Tree::load(
                                &root.join("data"),
                            ));
                            let name = options.player_name();
                            let activated = startup
                                .activate(
                                    options.execution_role,
                                    options.server_thread,
                                    true,
                                    &settings,
                                )
                                .map_err(|refused| refused.reason.clone())?;
                            let prepared = RemotePreparation::new_paired(
                                options.clone(),
                                settings.clone(),
                                prepared_cpu,
                                name.clone(),
                                startup.clone(),
                                pack.as_ref().unwrap().assets.registry,
                            )?;
                            let mut backend_options = options.clone();
                            backend_options.execution_role =
                                execution_role::ExecutionRole::IntegratedServer;
                            backend_options.server_thread = false;
                            backend_options.server = None;
                            backend_options.dedicated = Some(Vec::new());
                            backend_options.player_name = Some(name);
                            backend_options.control = None;
                            let mut backend_settings = settings;
                            backend_settings.render_distance = 2;
                            backend_settings.simulation_distance = 2;
                            let server_root = selection.anchor;
                            let backend = retained_server::OrdinaryServerInput {
                                options: backend_options,
                                settings: backend_settings,
                                request: selection.request,
                                entry: association,
                                resources: resources.clone(),
                                pack: pack.as_ref().unwrap().clone(),
                                gui: UiResources::load(&root, &frontend.vanilla_root)?,
                                cpu: backend_cpu,
                                tree,
                                startup: startup.clone(),
                                offline: horizon_client::lan::RetainedOfflineConfig {
                                    bind: None,
                                    port: 0,
                                    motd: "Singleplayer".into(),
                                    max_players: 1,
                                    compression_threshold: -1,
                                    enable_status: false,
                                    hide_online_players: true,
                                    announce: false,
                                },
                                dedicated_setup: None,
                                continuation_archive: None,
                                #[cfg(test)]
                                test: Default::default(),
                            };
                            let phase = Phase::IntegratedPreparing(Box::new(IntegratedPreparing {
                                res: &resources,
                                prepared: Some(prepared),
                                ui: ColdUi {
                                    pack: pack.take().unwrap(),
                                    gui: frontend.gui.take().unwrap(),
                                    renderer: frontend.renderer.take(),
                                    window: frontend.window.take(),
                                },
                                server_root,
                                refusal: None,
                                targets: Vec::new(),
                            }));
                            ordinary_application(
                                phase,
                                &options,
                                None,
                                Some(backend),
                                Some(activated),
                                frontend,
                            )
                        } else {
                            let selection = request.into_ordinary_local(
                                &std::env::current_dir()?,
                                original,
                                Instant::now() + Duration::from_secs(120),
                            )?;
                            let anchor = selection.anchor;
                            let activated = startup
                                .activate(options.execution_role, false, false, &settings)
                                .map_err(|refused| refused.reason.clone())?;
                            let (mut game, construction) = match selection.request {
                                retained_server::OrdinaryRequest::Native(request) => {
                                    let Some(prepared) = prepare_ordinary_in_frame(
                                        event_loop,
                                        frontend,
                                        request,
                                        &resources,
                                        pack.as_ref().unwrap(),
                                        &options,
                                        &settings,
                                        &association,
                                        startup.clone(),
                                    )?
                                    else {
                                        return Ok(None);
                                    };
                                    let construction =
                                        Construction::from_prepared(prepared.prepared());
                                    let mut game = Box::new(Game::new_with_world(&resources,
                                        pack.take().unwrap(), frontend.gui.take().unwrap(), options.clone(),
                                        settings, Audio::silent(&root), Some(prepared_cpu),
                                        InitialGameWorld::Ordinary(Box::new(prepared)))
                                        .unwrap_or_else(|error| {
                                            eprintln!("fatal: original ordinary in-frame transfer: {error}");
                                            std::process::abort();
                                        }));
                                    if let Err(original) = game.adopt_activated_startup(activated) {
                                        eprintln!(
                                            "fatal: original local runtime association refused: {}",
                                            original.1
                                        );
                                        std::process::abort();
                                    }
                                    game.title = None;
                                    game.screen = Screen::Playing;
                                    game.restore_ordinary_native_player();
                                    game.load_world_state();
                                    (game, construction)
                                }
                                retained_server::OrdinaryRequest::RecoverOwned(request) => {
                                    let Some((gui, tree)) =
                                        prepare_local_seed_resources(event_loop, frontend, &root)?
                                    else {
                                        return Ok(None);
                                    };
                                    // Genuine renderer-free, no-world Game on its owning UI thread.
                                    let mut seed = Box::new(Game::new_with_world(
                                        &resources,
                                        pack.as_ref().unwrap().clone(),
                                        gui,
                                        options.clone(),
                                        settings,
                                        Audio::silent(&root),
                                        Some(prepared_cpu),
                                        InitialGameWorld::OrdinaryRecoverySeed(startup.clone()),
                                    )?);
                                    if let Err(original) = seed.adopt_activated_startup(activated) {
                                        eprintln!(
                                            "fatal: original LOCAL seed runtime association refused: {}",
                                            original.1
                                        );
                                        std::process::abort();
                                    }
                                    seed.adv.tree = tree;
                                    let Some((mut game, construction)) = prepare_owned_in_frame(
                                        event_loop,
                                        frontend,
                                        seed,
                                        association.clone(),
                                        *request,
                                    )?
                                    else {
                                        return Ok(None);
                                    };
                                    // The original atlas and renderer stayed with Loading throughout.
                                    // Transfer that same presentation only after actual publication.
                                    std::mem::swap(&mut game.gui, frontend.gui.as_mut().unwrap());
                                    let _ = frontend.gui.take();
                                    let _ = pack.take();
                                    (game, construction)
                                }
                                retained_server::OrdinaryRequest::Recover(_) => {
                                    return Err("ordinary LOCAL selection returned an unrequested process route".into());
                                }
                            };
                            game.options.world = Some(anchor.to_string_lossy().into_owned());
                            if let Some(owned) = game
                                .session_lock
                                .as_ref()
                                .and_then(|session| session.owned())
                            {
                                game.local_continuation_archive = Some(owned.archive().to_owned());
                            }
                            install_ordinary_presentation(&mut game, frontend);
                            ordinary_application(
                                Phase::Running(game),
                                &options,
                                Some(construction),
                                None,
                                None,
                                frontend,
                            )
                        }
                    }
                };
                if let Some(control) = &mut application.control
                    && let Err(reason) = control.start()
                {
                    application.fail(reason);
                }
                if let Phase::RemoteRunning(game) = &mut application.phase
                    && let Err(reason) = game.start_retained_remote()
                {
                    application.fail(reason);
                }
                while !matches!(application.phase, Phase::Done) {
                    pump_ordinary(event_loop, &mut application);
                }
                let returned = application
                    .ordinary
                    .as_mut()
                    .unwrap()
                    .returned
                    .take()
                    .expect("actual completed ordinary frontend");
                Ok(Some((
                    returned,
                    application.quit,
                    application.original_error.take(),
                )))
            })();
            frontend.menu.reset_retained_entry();
            match result {
                Ok(Some((returned, quit, error))) => {
                    if let Some(closed) = returned.closed_world {
                        let retained = frontend.retained.as_mut().unwrap();
                        retained
                            .closed_worlds
                            .retain(|previous| previous.root() != closed.root());
                        retained.closed_worlds.push(Arc::new(closed));
                    }
                    pack = Some(returned.pack);
                    frontend.gui = Some(returned.gui);
                    frontend.renderer = Some(returned.renderer);
                    frontend.window = Some(returned.window);
                    let mut options = returned.options;
                    options.world = None;
                    options.server = None;
                    frontend.options = Some(options);
                    frontend.settings = returned.settings;
                    frontend.fullscreen = returned.fullscreen;
                    frontend.panorama_rotation = returned.panorama_rotation;
                    if let Some(error) = error {
                        if quit {
                            return Err(error.into());
                        }
                        // Completed native/transport custody permits returning
                        // the same frontend while its original error stays
                        // visible and remains the final application outcome.
                        frontend.error.get_or_insert_with(|| error.clone().into());
                        frontend.menu.retained_entry_error(&error);
                    }
                    if quit {
                        return Ok(false);
                    }
                    frontend.apply_menu_settings();
                }
                Ok(None) => {}
                Err(error) => frontend.menu.retained_entry_error(&error.to_string()),
            }
        }
        pump_ordinary(event_loop, frontend);
    }
}

#[cfg(feature = "mods")]
fn ordinary_local_request(entry: PendingWorldEntry) -> Result<world_loading::WorldRequest> {
    use world_loading::host_identity::IdentityIntent;
    let (folder, name, seed, creative, create, explicit) = match entry {
        PendingWorldEntry::Local {
            folder,
            name,
            seed,
            creative,
            create,
        } => (folder, name, seed, creative, create, None),
        PendingWorldEntry::IdentifiedLocal {
            folder,
            name,
            seed,
            creative,
            create,
            identity_intent,
        } => (folder, name, seed, creative, create, Some(identity_intent)),
        PendingWorldEntry::Server { .. } => {
            return Err("local preparation received a server entry".into());
        }
    };
    let request = world_loading::WorldRequest::new(
        &folder,
        name.as_deref(),
        seed,
        creative,
        create.as_deref(),
    )?;
    let intent = match explicit {
        Some(intent) => intent,
        None if name.is_some() => IdentityIntent::CreateNewIdentity,
        None => match std::fs::symlink_metadata(&request.save_root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && seed.is_some() => {
                IdentityIntent::CreateNewIdentity
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
            _ => IdentityIntent::RequireExistingIdentity,
        },
    };
    Ok(request.with_identity_intent(intent))
}

#[cfg(feature = "mods")]
#[allow(clippy::too_many_arguments)]
fn prepare_ordinary_in_frame(
    event_loop: &mut EventLoop<()>,
    frontend: &mut Loading,
    request: world_loading::WorldRequest,
    resources: &Arc<Resources>,
    pack: &PackResources,
    options: &Options,
    settings: &Settings,
    entry: &RetainedEntryIdentity,
    startup: Arc<mod_packages::FrozenHostInputs>,
) -> Result<Option<world_loading::OrdinaryPreparedWorld>> {
    let mut loader = world_loading::OrdinaryWorldLoader::start(
        request,
        resources.clone(),
        pack.clone(),
        options.clone(),
        settings.clone(),
        entry.clone(),
        startup,
    )?;
    let state = frontend.retained.as_mut().expect("actual frontend owner");
    state.preparing = true;
    state.entry_cancel = false;
    state.entry_retry = false;
    let mut owner = None;
    let mut closing = false;
    let mut blocked = false;
    let mut reason = None;
    loop {
        pump_ordinary(event_loop, frontend);
        let state = frontend.retained.as_mut().unwrap();
        let cancelled = state.entry_cancel || state.close_requested;
        let retry = std::mem::take(&mut state.entry_retry);
        if cancelled {
            loader.request_cancel();
        }
        if owner.is_none()
            && let Some(prepared) = loader.poll()
        {
            owner = Some(prepared);
        }
        let Some(prepared) = owner.as_mut() else {
            continue;
        };
        if !closing {
            if !cancelled && prepared.original_error().is_none() {
                match owner.take().unwrap().into_prepared(entry) {
                    Ok(prepared) => {
                        frontend.retained.as_mut().unwrap().preparing = false;
                        return Ok(Some(prepared));
                    }
                    Err(prepared) => owner = Some(prepared),
                }
            }
            let prepared = owner.as_mut().unwrap();
            if !cancelled {
                reason = Some(
                    prepared
                        .original_error()
                        .unwrap_or("original ordinary preparation transfer refused")
                        .to_owned(),
                );
            }
            prepared.begin_close(Instant::now() + CLOSE_WAIT);
            closing = true;
        }
        let prepared = owner.as_mut().unwrap();
        if retry && blocked && prepared.retry_close(Instant::now() + CLOSE_WAIT).is_ok() {
            blocked = false;
        }
        match prepared.poll_close() {
            PreparationCloseStatus::Complete => match owner.take().unwrap().finish_close() {
                Ok(closed) if closed.matches_entry(entry) => {
                    frontend.retained.as_mut().unwrap().preparing = false;
                    return reason.map_or(Ok(None), |reason| Err(reason.into()));
                }
                Ok(_wrong) => {
                    eprintln!(
                        "fatal: original ordinary preparation completion changed association"
                    );
                    std::process::abort();
                }
                Err(original) => owner = Some(original),
            },
            PreparationCloseStatus::Blocked(_) => blocked = true,
            PreparationCloseStatus::Pending => {}
        }
    }
}

#[cfg(feature = "mods")]
fn prepare_local_seed_resources(
    event_loop: &mut EventLoop<()>,
    frontend: &mut Loading,
    root: &std::path::Path,
) -> Result<Option<(UiResources, Arc<horizon_client::advancements::Tree>)>> {
    let root = root.to_owned();
    let defaults = frontend.vanilla_root.clone();
    let handle = std::thread::Builder::new()
        .name("LOCAL seed resources".into())
        .spawn(move || {
            UiResources::load(&root, &defaults)
                .map(|gui| {
                    (
                        gui,
                        Arc::new(horizon_client::advancements::Tree::load(&root.join("data"))),
                    )
                })
                .map_err(|error| error.to_string())
        })?;
    let state = frontend.retained.as_mut().expect("actual frontend owner");
    state.preparing = true;
    state.entry_cancel = false;
    state.entry_retry = false;
    let mut cancelled = false;
    while !handle.is_finished() {
        pump_ordinary(event_loop, frontend);
        let state = frontend.retained.as_ref().unwrap();
        cancelled |= state.entry_cancel || state.close_requested;
    }
    let state = frontend.retained.as_mut().unwrap();
    cancelled |= state.entry_cancel || state.close_requested;
    state.preparing = false;
    let resources = handle
        .join()
        .map_err(|_| "LOCAL resource loader panicked")??;
    Ok((!cancelled).then_some(resources))
}

#[cfg(feature = "mods")]
fn prepare_owned_in_frame<'a>(
    event_loop: &mut EventLoop<()>,
    frontend: &mut Loading,
    seed: Box<Game<'a>>,
    entry: RetainedEntryIdentity,
    request: levels::session_checkpoint::OwnedRecoveryRequest,
) -> Result<Option<(Box<Game<'a>>, Construction)>> {
    let mut owner = levels::session_checkpoint::LocalOwnedRecovery::start(seed, entry, request);
    let state = frontend.retained.as_mut().expect("actual frontend owner");
    state.preparing = true;
    state.entry_retry = false;
    let mut closing = false;
    let mut blocked = false;
    let mut reason = None;
    loop {
        pump_ordinary(event_loop, frontend);
        let state = frontend.retained.as_mut().unwrap();
        let cancelled = state.entry_cancel || state.close_requested;
        let retry = std::mem::take(&mut state.entry_retry);
        if !closing {
            if !cancelled {
                match owner.poll_activate() {
                    Ok(Some(game)) => {
                        let construction = owner
                            .take_construction()
                            .expect("original recovered constructor stamp");
                        frontend.retained.as_mut().unwrap().preparing = false;
                        return Ok(Some((game, construction)));
                    }
                    Ok(None) => continue,
                    Err(error) => reason = Some(error.to_string()),
                }
            }
            owner.begin_close(Instant::now() + CLOSE_WAIT);
            closing = true;
        }
        if retry && blocked && owner.retry_close(Instant::now() + CLOSE_WAIT).is_ok() {
            blocked = false;
        }
        match owner.poll_close() {
            PreparationCloseStatus::Complete => {
                if reason.is_none() {
                    reason = owner.original_error().map(str::to_owned);
                }
                frontend.retained.as_mut().unwrap().preparing = false;
                return reason.map_or(Ok(None), |reason| Err(reason.into()));
            }
            PreparationCloseStatus::Blocked(error) => {
                if !blocked {
                    frontend.menu.retained_entry_error(&error);
                }
                blocked = true;
            }
            PreparationCloseStatus::Pending => {}
        }
    }
}

#[cfg(feature = "mods")]
fn install_ordinary_presentation(game: &mut Game<'_>, frontend: &mut Loading) {
    game.window = frontend.window.take();
    game.renderer = frontend.renderer.take();
    game.world_renderer_ready = game.renderer.is_some();
    game.fullscreen = frontend.fullscreen;
    game.panorama_rotation = frontend.panorama_rotation;
    game.focused = game
        .window
        .as_ref()
        .is_some_and(|window| window.has_focus());
    game.previous = Instant::now();
}

#[cfg(feature = "mods")]
fn ordinary_application<'a>(
    phase: Phase<'a>,
    options: &Options,
    construction: Option<Construction>,
    backend: Option<retained_server::OrdinaryServerInput>,
    activation: Option<mod_packages::ActivatedStartup>,
    frontend: &Loading,
) -> Application<'a> {
    Application {
        ordinary: Some(OrdinaryApplication {
            backend,
            backend_refusal: None,
            activation,
            returned: None,
            control_requested: options.control.is_some(),
            fullscreen: frontend.fullscreen,
            panorama_rotation: frontend.panorama_rotation,
        }),
        phase,
        server: None,
        server_result: None,
        server_offer: None,
        client_attempt: 0,
        construction,
        control: options
            .control
            .as_ref()
            .map(|path| RetainedControl::prepare_window(path)),
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
        started: Instant::now(),
        frames: 0,
        frame_limit: options.frames,
        timeout: options.timeout,
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
        test: None,
    }
}

enum RemoteDestination {
    Address(String),
    Integrated,
}

pub(super) struct RemoteLaunch {
    root: PathBuf,
    destination: RemoteDestination,
    args: Vec<String>,
}
impl RemoteLaunch {
    pub(super) fn parse(args: &[String]) -> Result<Option<Self>> {
        if !args.iter().any(|a| a.starts_with("--retained-client-")) {
            return Ok(None);
        }
        if !cfg!(target_os = "linux") || cfg!(feature = "controllers") {
            return Err("retained client requires Linux with controllers disabled".into());
        }
        for name in [
            "HORIZON_SESSION_FILE",
            "HORIZON_PLAYER_NAME",
            "HORIZON_BENCHMARK",
            "HORIZON_COMMAND_HARNESS",
            "HORIZON_NET_HARNESS",
        ] {
            if std::env::var_os(name).is_some_and(|v| !v.is_empty()) {
                return Err(format!("retained client refuses {name}").into());
            }
        }
        let (mut root, mut address, mut integrated) = (None, None, false);
        let mut ordinary = Vec::new();
        let mut input = args.iter();
        while let Some(arg) = input.next() {
            match arg.as_str() {
                "--retained-client-integrated" if address.is_none() && !integrated => {
                    integrated = true
                }
                "--retained-client-window" if address.is_none() && !integrated => {
                    address = Some(
                        input
                            .next()
                            .ok_or("missing retained client address")?
                            .clone(),
                    )
                }
                "--retained-client-root" if root.is_none() => {
                    root = Some(PathBuf::from(
                        input.next().ok_or("missing retained client root")?,
                    ))
                }
                "--resources" | "--control-socket" | "--width" | "--height" | "--frames"
                | "--timeout-seconds" => {
                    ordinary.push(arg.clone());
                    ordinary.push(
                        input
                            .next()
                            .ok_or("missing retained client option value")?
                            .clone(),
                    );
                }
                _ => return Err(format!("unsupported retained client option {arg}").into()),
            }
        }
        let root = root.ok_or("retained client needs a fresh absolute root")?;
        if !root.is_absolute()
            || root
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("retained client root must be absolute without parent components".into());
        }
        match std::fs::symlink_metadata(&root) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err("retained client root must not already exist".into()),
        }
        ordinary.push("--in-frame".into());
        ordinary.push("--test-terrain".into());
        Ok(Some(Self {
            root,
            destination: if integrated {
                RemoteDestination::Integrated
            } else {
                RemoteDestination::Address(address.ok_or("missing retained client address")?)
            },
            args: ordinary,
        }))
    }
    pub(super) fn ordinary_args(&self) -> Vec<String> {
        self.args.clone()
    }
    pub(super) fn configure(&self, options: &mut Options) {
        options.server_thread = false;
        options.test_terrain = true;
        options.distance = 2;
        options.distance_override = Some(2);
        options
            .control
            .get_or_insert_with(|| self.root.join("control.sock"));
    }
}

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
pub(in crate::game) struct Construction {
    save: Arc<SaveDir>,
    world: WorldId,
    root: PathBuf,
}
impl Construction {
    pub(in crate::game) fn from_prepared(prepared: &world_loading::PreparedWorld) -> Self {
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
        game.save
            .as_ref()
            .is_some_and(|save| Arc::ptr_eq(&self.save, save))
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
/// Only presentation/settings return to the no-world frontend. This contains no
/// Game borrow, native save/authority, Session or configured admission.
pub(super) struct ReturnedFrontend {
    pub(super) closed_world: Option<super::paired_close::OrdinaryClosedWorld>,
    pub(super) pack: PackResources,
    pub(super) gui: UiResources,
    pub(super) renderer: Renderer,
    pub(super) window: Arc<Window>,
    pub(super) options: Options,
    pub(super) settings: Settings,
    pub(super) fullscreen: horizon_client::window::FullscreenState,
    pub(super) panorama_rotation: horizon_client::gfx::panorama::Rotation,
}
impl ReturnedFrontend {
    /// Callers first prove actual native or remote completion. Moving the fields
    /// here disposes the completed world remainder without retaining its authority.
    pub(super) fn from_completed_game(mut game: Box<Game<'_>>) -> Self {
        let renderer = game
            .renderer
            .take()
            .expect("original presentation renderer");
        let window = game.window.take().expect("original presentation window");
        let Game {
            pack,
            gui,
            options,
            settings,
            fullscreen,
            panorama_rotation,
            ..
        } = *game;
        Self {
            closed_world: None,
            pack,
            gui,
            renderer,
            window,
            options,
            settings,
            fullscreen,
            panorama_rotation,
        }
    }
}
/// Constructor-bound completion for an ordinary world. Control is optional in
/// normal launches; absence is checked against the actual original option rather
/// than represented by a fabricated ClosedControl. LOD was never acquired here.
pub(super) struct OrdinaryClientCompletion {
    construction: Construction,
    control: Option<ClosedControl>,
    control_requested: bool,
}
impl OrdinaryClientCompletion {
    pub(super) fn matches(&self, game: &Game<'_>) -> bool {
        self.construction.matches(game)
            && game.ordinary_startup.is_some()
            && self.control_requested == game.options.control.is_some()
            && match &self.control {
                Some(control) => self.control_requested && control.observation().native_quiescent,
                None => !self.control_requested,
            }
            && game.lod.is_none()
            && game.settings.horizon.lod_distance == 0
            && game
                .renderer
                .as_ref()
                .is_some_and(Renderer::has_inline_occlusion_provenance)
            && game.window.is_some()
    }
}
struct ColdState {
    owner: Option<ColdPreparation>,
    closed: Option<PreparationClosed>,
    ui: ColdUi,
    entry: RetainedEntryIdentity,
}
struct IntegratedPreparing<'a> {
    res: &'a Arc<Resources>,
    prepared: Option<RemotePreparation>,
    ui: ColdUi,
    server_root: PathBuf,
    refusal: Option<Box<retained_server::IntegratedStartRefusal>>,
    targets: Vec<horizon_client::net::RetainedTarget>,
}
enum Phase<'a> {
    Running(Box<Game<'a>>),
    RemoteRunning(Box<Game<'a>>),
    RemoteClosing(Box<Game<'a>>),
    RemoteReady(Box<Game<'a>>),
    Closing(Box<CloseOwner<'a>>),
    Ready(NativeReady<'a>),
    Cold(Box<ColdState>),
    IntegratedPreparing(Box<IntegratedPreparing<'a>>),
    Done,
}

pub(super) fn run_remote(options: Options, launch: RemoteLaunch) -> Result<()> {
    horizon_threads::catch_retained_stop_signals();
    let settings = Settings::load(&std::env::current_dir()?.join("options.txt"));
    let integrated = matches!(launch.destination, RemoteDestination::Integrated);
    preflight(&options, &settings)?;
    if integrated
        && (cfg!(feature = "audio")
            || settings.render_distance != 2
            || settings.simulation_distance != 5)
    {
        return Err(
            "retained integrated view requires audio disabled, render distance two and client simulation distance five".into(),
        );
    }
    if cfg!(feature = "controllers") {
        return Err("retained client excludes controller acquisition".into());
    }
    // Admit only the explicit no-LOD profile; do not normalize user settings.
    if settings.horizon.lod_distance != 0 {
        return Err("retained client requires LOD disabled".into());
    }
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
            "retained client requires complete synchronous resources without fallback".into(),
        );
    }
    let gui = loaded
        .gui
        .ok_or("retained client requires complete GUI bundle")?;
    let resources = loaded.resources;
    let cpu = GameCpuResources::load(&root)?;
    let event_loop = new_event_loop()?;
    std::fs::create_dir(&launch.root)?;
    let frame_limit = options.frames;
    let timeout = options.timeout;
    let control_path = options
        .control
        .clone()
        .ok_or("retained client control path")?;
    let phase = match launch.destination {
        RemoteDestination::Address(address) => {
            Phase::RemoteRunning(Box::new(Game::new_retained_remote(
                &resources,
                loaded.pack,
                gui,
                options,
                settings,
                cpu,
                address,
                "RetainedGuest".into(),
            )?))
        }
        RemoteDestination::Integrated => {
            Phase::IntegratedPreparing(Box::new(IntegratedPreparing {
                res: &resources,
                prepared: Some(RemotePreparation::new(
                    options,
                    settings,
                    cpu,
                    "IntegratedOwner".into(),
                )?),
                ui: ColdUi {
                    pack: loaded.pack,
                    gui,
                    renderer: None,
                    window: None,
                },
                server_root: launch.root.join("server"),
                refusal: None,
                targets: Vec::new(),
            }))
        }
    };
    let mut application = Application {
        #[cfg(feature = "mods")]
        ordinary: None,
        phase,
        server: None,
        server_result: None,
        server_offer: None,
        client_attempt: 0,
        construction: None,
        control: Some(RetainedControl::prepare_window(&control_path)),
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
        test: None,
    };
    if !integrated && let Err(error) = application.control.as_mut().unwrap().start() {
        application.fail(error);
    }
    if let Phase::RemoteRunning(game) = &mut application.phase
        && let Err(error) = game.start_retained_remote()
    {
        application.fail(error);
    }
    let outcome = event_loop.run_app(&mut application);
    if !matches!(application.phase, Phase::Done) {
        eprintln!("fatal: retained client event loop ended with unresolved original owners");
        std::process::abort();
    }
    outcome?;
    if integrated {
        eprintln!(
            "INTEGRATED_APPLICATION_JOINED={} SERVER_RESULT={:?}",
            application.server_done(),
            application.server_result
        );
        if let Some(server) = &application.server {
            eprintln!(
                "INTEGRATED_APPLICATION_SERVER_FINAL={}",
                server.snapshot().json()
            );
        }
    }
    if let Some(error) = application.original_error.take() {
        return Err(error.into());
    }
    Ok(())
}

#[cfg(feature = "mods")]
struct OrdinaryApplication {
    backend: Option<retained_server::OrdinaryServerInput>,
    backend_refusal: Option<Box<retained_server::OrdinaryStartRefusal>>,
    activation: Option<mod_packages::ActivatedStartup>,
    returned: Option<ReturnedFrontend>,
    control_requested: bool,
    fullscreen: horizon_client::window::FullscreenState,
    panorama_rotation: horizon_client::gfx::panorama::Rotation,
}
struct Application<'a> {
    #[cfg(feature = "mods")]
    ordinary: Option<OrdinaryApplication>,
    phase: Phase<'a>,
    server: Option<retained_server::Supervisor>,
    server_result: Option<std::result::Result<(), String>>,
    server_offer: Option<
        std::sync::mpsc::Receiver<std::result::Result<horizon_protocol::connection::Pipe, String>>,
    >,
    client_attempt: u64,
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
        #[cfg(feature = "mods")]
        ordinary: None,
        phase: Phase::Done,
        server: None,
        server_result: None,
        server_offer: None,
        client_attempt: 0,
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
            game.streaming.expect_existing_mut().radius = 2;
            game.streaming.expect_existing_mut().center = (0, 0);
            game.load_world_state();
            let source = game.streaming.expect_existing().lod_source();
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
    fn is_ordinary(&self) -> bool {
        #[cfg(feature = "mods")]
        return self.ordinary.is_some();
        #[cfg(not(feature = "mods"))]
        false
    }
    fn server_done(&self) -> bool {
        self.server.is_none() || self.server_result.is_some()
    }
    fn frontend_done(&self) -> bool {
        matches!(self.phase, Phase::Ready(_) | Phase::RemoteReady(_))
            || matches!(&self.phase, Phase::Cold(cold) if cold.closed.is_some())
            || (self.attempt > 0 && matches!(self.phase, Phase::IntegratedPreparing(_)))
    }
    fn poll_integrated(&mut self) {
        let mut failure = None;
        let ordinary = self.is_ordinary();
        #[cfg(feature = "mods")]
        if self.attempt == 0
            && self.server.is_none()
            && self.view().usable
            && matches!(self.phase, Phase::IntegratedPreparing(_))
            && let Some(mode) = &mut self.ordinary
            && mode.backend_refusal.is_none()
            && let Some(input) = mode.backend.take()
        {
            match retained_server::Supervisor::prepare_ordinary(input) {
                Ok(server) => self.server = Some(server),
                Err(refused) => {
                    failure = Some(refused.reason.clone());
                    mode.backend_refusal = Some(refused);
                }
            }
        }
        #[cfg(test)]
        let hold_prepare = tests::hold_integrated_stage("prepare");
        #[cfg(not(test))]
        let hold_prepare = false;
        if !ordinary
            && self.attempt == 0
            && self.server.is_none()
            && self.view().usable
            && !hold_prepare
            && let Phase::IntegratedPreparing(seed) = &mut self.phase
            && seed.refusal.is_none()
        {
            let prepared = seed.prepared.as_ref().unwrap();
            match retained_server::Supervisor::prepare_integrated(
                prepared.options.clone(),
                prepared.settings.clone(),
                seed.server_root.clone(),
                &prepared.name,
            ) {
                Ok(server) => self.server = Some(server),
                Err(refusal) => {
                    failure = Some(refusal.reason.clone());
                    seed.refusal = Some(refusal);
                }
            }
        }
        if let Some(server) = &self.server {
            let state = server.snapshot();
            if let Some(error) = state.original_error {
                failure.get_or_insert(error);
            }
            if self.attempt == 0
                && state.ready
                && self.server_offer.is_none()
                && matches!(self.phase, Phase::IntegratedPreparing(_))
            {
                match server.offer() {
                    Ok(receive) => self.server_offer = Some(receive),
                    Err(error) => {
                        failure.get_or_insert(error);
                    }
                }
            }
            if let Phase::RemoteRunning(game) = &self.phase {
                server.set_paused(game.pauses_game());
            }
        }
        #[cfg(test)]
        let hold_offer = self.attempt == 0 && tests::hold_integrated_stage("offer");
        #[cfg(not(test))]
        let hold_offer = false;
        let offered = self
            .server_offer
            .as_ref()
            .filter(|_| !hold_offer)
            .and_then(|receive| match receive.try_recv() {
                Ok(value) => Some(value),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Some(Err("original integrated Offer channel closed".into()))
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
            });
        if let Some(offered) = offered {
            self.server_offer.take();
            self.status_presented = false;
            let phase = std::mem::replace(&mut self.phase, Phase::Done);
            self.phase = match phase {
                Phase::IntegratedPreparing(mut seed) => match offered {
                    Ok(pipe) if self.attempt == 0 && failure.is_none() => {
                        match seed.prepared.take().unwrap().attach(
                            horizon_client::net::RetainedTarget::Memory(pipe),
                            "Integrated world".into(),
                            RemoteFrontend::Integrated,
                        ) {
                            Ok(attached) => {
                                assert!(seed.targets.is_empty());
                                let mut game =
                                    attached.into_game(seed.res, seed.ui.pack, seed.ui.gui);
                                #[cfg(feature = "mods")]
                                if let Some(mode) = &mut self.ordinary {
                                    let activated = mode
                                        .activation
                                        .take()
                                        .expect("original activated client runtime");
                                    if let Err(original) = game.adopt_activated_startup(activated) {
                                        eprintln!(
                                            "fatal: original integrated client runtime association refused: {}",
                                            original.1
                                        );
                                        std::process::abort();
                                    }
                                    game.fullscreen = mode.fullscreen;
                                    game.panorama_rotation = mode.panorama_rotation;
                                }
                                game.window = seed.ui.window;
                                game.renderer = seed.ui.renderer;
                                game.world_renderer_ready = game.renderer.is_some();
                                game.focused = game
                                    .window
                                    .as_ref()
                                    .is_some_and(|window| window.has_focus());
                                if let Err(error) = game.start_retained_remote() {
                                    failure.get_or_insert(error.to_string());
                                }
                                Phase::RemoteRunning(Box::new(game))
                            }
                            Err((prepared, target, error)) => {
                                seed.prepared = Some(prepared);
                                if let horizon_client::net::RetainedTarget::Memory(pipe) = &target {
                                    pipe.shutdown();
                                }
                                seed.targets.push(target);
                                failure.get_or_insert(error);
                                Phase::IntegratedPreparing(seed)
                            }
                        }
                    }
                    Ok(pipe) => {
                        pipe.shutdown();
                        seed.targets
                            .push(horizon_client::net::RetainedTarget::Memory(pipe));
                        #[cfg(test)]
                        eprintln!("INTEGRATED_CANCELLED_ACTUAL_MEMORY_OFFER=true");
                        Phase::IntegratedPreparing(seed)
                    }
                    Err(error) => {
                        failure.get_or_insert(error);
                        Phase::IntegratedPreparing(seed)
                    }
                },
                other => {
                    // An Offer is requested only in preparing and that phase
                    // stays owned until its actual response is consumed.
                    self.phase = other;
                    eprintln!("fatal: integrated Offer lost its original preparing owner");
                    std::process::abort();
                }
            };
        }
        if self.server_result.is_none()
            && let Some(server) = &mut self.server
            && server.finished()
        {
            let result = server.join().expect("original finished supervisor join");
            if let Err(error) = &result {
                failure.get_or_insert(error.clone());
            }
            self.server_result = Some(result);
            self.status_presented = false;
        }
        if let Some(error) = failure
            && self.original_error.as_ref() != Some(&error)
            && self.late_error.as_ref() != Some(&error)
        {
            self.fail(error);
        }
    }
    fn view(&self) -> Presentation {
        match &self.phase {
            Phase::Running(game)
            | Phase::RemoteRunning(game)
            | Phase::RemoteClosing(game)
            | Phase::RemoteReady(game) => presentation(game),
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
            Phase::IntegratedPreparing(seed) => Presentation {
                id: seed.ui.window.as_ref().map(|w| w.id()),
                focused: seed.ui.window.as_ref().is_some_and(|w| w.has_focus()),
                usable: seed.ui.window.as_ref().is_some_and(|w| {
                    w.is_visible() != Some(false) && w.is_minimized() != Some(true)
                }) && seed
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
                Phase::RemoteRunning(_) => "remote running",
                Phase::RemoteClosing(_) => "closing original remote actors",
                Phase::RemoteReady(_) => "remote actors closed; Q quits",
                Phase::Running(_) => "running fixed view: Esc closes; Q quits",
                Phase::Closing(_) if self.lod.is_some() => "closing LOD",
                Phase::Closing(_) => "closing native world",
                Phase::Ready(_) => "world closed; Q quits",
                Phase::Cold(_) => "closing original failed preparation",
                Phase::IntegratedPreparing(_) => "integrated frontend preparing",
                Phase::Done => "complete",
            }
        }
    }
    fn status_text(&self) -> String {
        if let Phase::RemoteRunning(game) = &self.phase {
            return format!(
                "remote running | chunks={} sections={} frames={} | player={:?} | yaw={:.2} pitch={:.2} | {}",
                game.world.chunks.len(),
                game.renderer.as_ref().map_or(0, |r| r.drawn_sections),
                game.frames,
                game.player
                    .as_ref()
                    .map(|p| [p.position.x, p.position.y, p.position.z]),
                game.yaw,
                game.pitch,
                if game.remote_frontend == RemoteFrontend::Integrated {
                    "Esc pauses/resumes"
                } else {
                    "Esc leaves"
                }
            );
        }
        let server = self
            .server
            .as_ref()
            .map(|server| {
                format!(
                    "\nServer {} {:?}",
                    server.snapshot().attempt,
                    server.snapshot().blocked
                )
            })
            .unwrap_or_default();
        let detail = format!("{}{server}", brief(&self.visible_detail()));
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
            detail
        )
    }
    fn visible_detail(&self) -> String {
        if matches!(
            self.phase,
            Phase::RemoteRunning(_)
                | Phase::RemoteClosing(_)
                | Phase::RemoteReady(_)
                | Phase::IntegratedPreparing(_)
        ) && let Some(error) = &self.original_error
        {
            return format!("{error}; {}", self.detail);
        }
        self.detail.clone()
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
        // A late diagnostic cannot turn an already requested ordinary Leave
        // into Quit. Explicit Quit and failures before close retain that intent.
        let quit = !self.is_ordinary() || self.attempt == 0 || self.quit;
        self.begin_close(quit);
    }
    fn begin_close(&mut self, quit: bool) {
        self.quit |= quit;
        if self.attempt != 0 {
            return;
        }
        self.attempt = 1;
        if let Some(server) = &self.server {
            server.request_close();
        }
        self.status_presented = false;
        let now = Instant::now();
        if let Some(lod) = &mut self.lod {
            lod.begin_close(now, CLOSE_WAIT);
        }
        let phase = std::mem::replace(&mut self.phase, Phase::Done);
        self.phase = match phase {
            Phase::RemoteRunning(mut game) => {
                self.client_attempt = 1;
                if let Some(error) = game.retained_remote_failure() {
                    self.original_error.get_or_insert(error);
                }
                if let Some(error) = &self.original_error {
                    game.retained_remote_request_close(error.clone());
                } else {
                    game.retained_remote_request_leave();
                }
                game.capture(false);
                game.begin_retained_remote_close(now + CLOSE_WAIT);
                Phase::RemoteClosing(game)
            }
            Phase::Running(mut game) => {
                if game.ordinary_startup.is_some() {
                    game.clear_ordinary_input();
                    game.capture(false);
                }
                let profile = if game.ordinary_startup.is_some() && game.local_checkpoint_owner() {
                    game.session_lock
                        .as_ref()
                        .and_then(|session| session.owned())
                        .map(|owned| CloseProfile::CoherentSession {
                            archive: owned.archive().to_owned(),
                        })
                        .unwrap_or(CloseProfile::NativeWorld)
                } else {
                    CloseProfile::NativeWorld
                };
                Phase::Closing(CloseOwner::begin(game, profile, now + CLOSE_WAIT))
            }
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
        let server_accepted = self.server.as_ref().is_some_and(|server| {
            let state = server.snapshot();
            state.blocked.is_some() && server.retry(state.attempt)
        });
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
                Phase::RemoteClosing(game) => {
                    let accepted =
                        game.retry_retained_remote_close(self.client_attempt, now + CLOSE_WAIT);
                    if accepted {
                        self.client_attempt = self
                            .client_attempt
                            .checked_add(1)
                            .expect("validated client Retry counter");
                    }
                    accepted
                }
                Phase::Closing(owner) => owner.retry(now + CLOSE_WAIT).is_ok(),
                Phase::Cold(cold) => cold
                    .owner
                    .as_mut()
                    .is_some_and(|o| o.retry_close(now + CLOSE_WAIT).is_ok()),
                _ => false,
            }
        };
        let accepted = accepted || server_accepted;
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
                    let visible = if let Some(server) = &self.server {
                        let state = server.snapshot().json();
                        serde_json::json!({"server_phase":state["phase"],"server_attempt":state["attempt"],"time":state["world_time"],"paused":state["paused"],"record":state["owner_record"],"joined":state["worker_joined"],"blocked":state["blocked"],"offer_pending":self.server_offer.is_some(),"error":self.original_error.as_ref().map(|error| error.chars().take(80).collect::<String>())}).to_string()
                    } else {
                        self.visible_detail()
                    };
                    let detail = if self.server.is_some() {
                        visible.as_str()
                    } else {
                        brief(&visible)
                    };
                    let _ = request.reply(WindowReply::Status {
                        phase: if self.blocked { "blocked" } else { "active" },
                        stage,
                        attempt: self.attempt,
                        detail,
                        world_closed: self.frontend_done()
                            && self.server_done()
                            && self.server_offer.is_none(),
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
            Phase::RemoteClosing(game) => {
                let status = game.poll_retained_remote_close();
                if status.attempt != self.client_attempt {
                    self.blocked = true;
                    self.detail = "original remote parent attempt mismatch; owners retained".into();
                    return;
                }
                self.blocked = status.blocked;
                if !status.detail.is_empty() {
                    self.detail = status.detail;
                }
                if let Some(error) = game.retained_remote_failure() {
                    self.original_error.get_or_insert(error);
                }
                if status.quiescent {
                    let Phase::RemoteClosing(game) =
                        std::mem::replace(&mut self.phase, Phase::Done)
                    else {
                        unreachable!()
                    };
                    self.phase = Phase::RemoteReady(game);
                    self.blocked = false;
                    self.status_presented = false;
                }
            }
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
        if let Some(server) = &self.server
            && let Some(error) = server.snapshot().blocked
        {
            self.blocked = true;
            self.detail = format!("integrated native server: {error}");
        }
        let world_closed =
            self.frontend_done() && self.server_done() && self.server_offer.is_none();
        if !world_closed
            || (!self.quit && !self.is_ordinary())
            || !self.unclaimed_columns.is_empty()
        {
            return;
        }
        if self.is_ordinary()
            && self.control.is_none()
            && self.closed_control.is_none()
            && self.original_error.is_some()
        {
            self.terminal_failure = true;
        }
        // Once sealed, keep polling actual control completion regardless of
        // drawing. Before seal, require a positively presented original status
        // frame after WORLD completion and no later surface invalidation.
        let sealed = self.closed_control.is_some()
            || self
                .control
                .as_ref()
                .is_some_and(|control| control.observation().admission_sealed);
        let unstarted_presentation_failure = matches!(self.phase, Phase::IntegratedPreparing(_))
            && self.server.is_none()
            && !self.view().usable
            && self.original_error.is_some();
        if unstarted_presentation_failure {
            self.exit_acknowledged = true;
        }
        if !sealed
            && !unstarted_presentation_failure
            && (!self.status_presented || self.surface_occluded || !self.view().usable)
        {
            return;
        }
        #[cfg(test)]
        if self.control.is_some() && !sealed {
            eprintln!(
                "WINDOW_JOINT_CONTROL_SEAL server_done={} offer_pending={} status_presented={} unpublished_failure={}",
                self.server_done(),
                self.server_offer.is_some(),
                self.status_presented,
                unstarted_presentation_failure
            );
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
                        self.terminal_failure |= (matches!(
                            self.phase,
                            Phase::RemoteReady(_) | Phase::IntegratedPreparing(_)
                        ) && self.original_error.is_some())
                            || !matches!(
                                observation.endpoint,
                                RetainedEndpointState::Released
                                    | RetainedEndpointState::Unpublished
                            )
                            || observation.failures.iter().any(|f| {
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
        #[cfg(feature = "mods")]
        if self.ordinary.is_some() {
            self.complete_ordinary();
            return;
        }
        if !self.server_done() || self.server_offer.is_some() {
            return;
        }
        if self.closed_control.is_none() || (self.terminal_failure && !self.exit_acknowledged) {
            return;
        }
        let phase = std::mem::replace(&mut self.phase, Phase::Done);
        match phase {
            Phase::IntegratedPreparing(seed)
                if self.server_done() && self.server_offer.is_none() =>
            {
                drop(seed);
                drop(self.closed_control.take());
                event_loop.exit();
            }
            Phase::RemoteReady(game) => {
                // All three original actor owners are now closed and retained
                // in this same Game. No storage/world completion is fabricated.
                drop(game);
                drop(self.closed_control.take());
                event_loop.exit();
            }
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
    #[cfg(feature = "mods")]
    fn complete_ordinary(&mut self) {
        if !self.frontend_done()
            || !self.server_done()
            || self.server_offer.is_some()
            || self.control.is_some()
            || !self.unclaimed_columns.is_empty()
            || (self.terminal_failure && !self.exit_acknowledged)
        {
            return;
        }
        let mode = self
            .ordinary
            .as_mut()
            .expect("ordinary application constructor");
        if mode.control_requested != self.closed_control.is_some() {
            return;
        }
        let phase = std::mem::replace(&mut self.phase, Phase::Done);
        let mut returned = match phase {
            Phase::Ready(ready) => {
                let completion = OrdinaryClientCompletion {
                    construction: self
                        .construction
                        .take()
                        .expect("original native Game constructor"),
                    control: self.closed_control.take(),
                    control_requested: mode.control_requested,
                };
                match ready.try_return_window(completion) {
                    Ok(returned) => returned,
                    Err((ready, completion, reason)) => {
                        self.phase = Phase::Ready(ready);
                        self.construction = Some(completion.construction);
                        self.closed_control = completion.control;
                        self.blocked = true;
                        self.detail = reason;
                        return;
                    }
                }
            }
            Phase::RemoteReady(game) => {
                // RemoteReady is reached only after the same Game's actual
                // Session, decoder and streaming close. Joint server/Offer and
                // optional control completion were checked above.
                if mode.activation.is_some() {
                    self.phase = Phase::RemoteReady(game);
                    return;
                }
                ReturnedFrontend::from_completed_game(game)
            }
            Phase::IntegratedPreparing(mut seed) => {
                let prepared = seed.prepared.take().expect("unconsumed original frontend");
                ReturnedFrontend {
                    closed_world: None,
                    pack: seed.ui.pack,
                    gui: seed.ui.gui,
                    renderer: seed.ui.renderer.take().expect("original ordinary renderer"),
                    window: seed.ui.window.take().expect("original ordinary window"),
                    options: prepared.options,
                    settings: prepared.settings,
                    fullscreen: mode.fullscreen,
                    panorama_rotation: mode.panorama_rotation,
                }
            }
            other => {
                self.phase = other;
                return;
            }
        };
        if let Some(server) = self.server.as_mut()
            && let Some(closed) = server.take_ordinary_closed_world()
        {
            returned.closed_world = Some(closed);
        }
        mode.returned = Some(returned);
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
            Phase::RemoteRunning(game) => game.retained_remote_frame(),
            Phase::RemoteClosing(game) | Phase::RemoteReady(game) => draw_status(game, &stage),
            Phase::Running(game) if game.ordinary_startup.is_some() => game.ordinary_native_frame(),
            Phase::Running(game) => game.render(),
            Phase::Closing(owner) => owner.draw_window_status(&stage),
            Phase::Ready(ready) => ready.draw_window_status(&stage),
            Phase::Cold(cold) => draw_hud(cold.ui.renderer.as_mut(), &cold.ui.gui, &stage),
            Phase::IntegratedPreparing(seed) => {
                draw_hud(seed.ui.renderer.as_mut(), &seed.ui.gui, &stage)
            }
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
                if matches!(self.phase, Phase::Running(_) | Phase::RemoteRunning(_)) {
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
                    test.presented(matches!(
                        self.phase,
                        Phase::Running(_) | Phase::RemoteRunning(_)
                    ));
                }
            }
            Err(error) => {
                self.status_presented = false;
                self.fail(error.to_string());
            }
        }
    }
    fn update_view(&mut self) {
        if let Phase::RemoteRunning(game) = &self.phase {
            if let Some(error) = game.retained_remote_failure() {
                self.fail(error);
            } else if game.retained_remote_leave_requested() {
                self.begin_close(false);
            }
            return;
        }
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
        if game.ordinary_startup.is_some() && game.retained_entry_closing {
            self.begin_close(false);
            return;
        }
        if game.ordinary_startup.is_some() {
            let error = game.error.as_ref().map(ToString::to_string);
            if let Some(error) = error {
                self.fail(error);
            }
            return;
        }
        let (uploads, unloaded) = game.streaming.expect_existing_mut().update(&mut game.world);
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
        let size = match &self.phase {
            Phase::RemoteRunning(game) | Phase::RemoteClosing(game) | Phase::RemoteReady(game) => {
                game.options.window_size
            }
            Phase::IntegratedPreparing(seed) => seed.prepared.as_ref().unwrap().options.window_size,
            _ => [960, 540],
        };
        let attributes = Window::default_attributes()
            .with_title("Horizon native window")
            .with_inner_size(winit::dpi::LogicalSize::new(
                f64::from(size[0]),
                f64::from(size[1]),
            ));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                self.fail(error.to_string());
                return;
            }
        };
        let result = match &mut self.phase {
            Phase::Running(game)
            | Phase::RemoteRunning(game)
            | Phase::RemoteClosing(game)
            | Phase::RemoteReady(game) => {
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
            Phase::IntegratedPreparing(seed) => {
                seed.ui.window = Some(window.clone());
                #[cfg(test)]
                if tests::refuse_integrated_renderer() {
                    return self.fail("controlled integrated frontend renderer refusal");
                }
                renderer(
                    window,
                    &seed.ui.pack,
                    &seed.ui.gui,
                    horizon_client::graphics::GraphicsApi::default(),
                )
                .map(|renderer| seed.ui.renderer = Some(renderer))
            }
            Phase::Done => Ok(()),
        };
        if let Err(error) = result {
            self.fail(error.to_string());
        } else if matches!(self.phase, Phase::IntegratedPreparing(_))
            && let Some(control) = &mut self.control
            && let Err(error) = control.start()
        {
            self.fail(error);
        }
    }
    fn window_event(&mut self, _event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if self.view().id != Some(id) {
            return;
        }
        if let Phase::RemoteRunning(game) = &mut self.phase
            && !matches!(
                &event,
                WindowEvent::CloseRequested
                    | WindowEvent::Resized(_)
                    | WindowEvent::RedrawRequested
                    | WindowEvent::Occluded(_)
            )
        {
            let result = game.retained_remote_input(event);
            if let Err(error) = result {
                self.fail(error.to_string());
            }
            self.update_view();
            return;
        }
        if let Phase::Running(game) = &mut self.phase
            && game.ordinary_startup.is_some()
            && !matches!(
                &event,
                WindowEvent::CloseRequested
                    | WindowEvent::Resized(_)
                    | WindowEvent::RedrawRequested
                    | WindowEvent::Occluded(_)
            )
        {
            if let Err(error) = game.ordinary_player_input(event) {
                self.fail(error.to_string());
            }
            self.update_view();
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                if self.terminal_failure {
                    if self.is_ordinary() {
                        self.begin_close(true);
                    }
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
                    PhysicalKey::Code(KeyCode::Enter) if self.terminal_failure && !is_synthetic => {
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
                    Phase::Running(game)
                    | Phase::RemoteRunning(game)
                    | Phase::RemoteClosing(game)
                    | Phase::RemoteReady(game) => resize(game, size.width, size.height),
                    Phase::Closing(owner) => owner.resize_window(size.width, size.height),
                    Phase::Ready(ready) => ready.resize_window(size.width, size.height),
                    Phase::Cold(cold) => {
                        if let Some(renderer) = &mut cold.ui.renderer {
                            renderer.resize(size.width, size.height);
                        }
                    }
                    Phase::IntegratedPreparing(seed) => {
                        if let Some(renderer) = &mut seed.ui.renderer {
                            renderer.resize(size.width, size.height);
                        }
                    }
                    Phase::Done => {}
                }
            }
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
        self.update_view();
    }
    fn device_event(&mut self, _: &ActiveEventLoop, _: DeviceId, event: DeviceEvent) {
        if let Phase::RemoteRunning(game) = &mut self.phase {
            game.retained_remote_device(event);
        } else if let Phase::Running(game) = &mut self.phase
            && game.ordinary_startup.is_some()
        {
            game.retained_remote_device(event);
        }
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.poll_integrated();
        if horizon_threads::stop_requests() > 0 {
            self.begin_close(true);
        }
        if self.frame_limit.is_some_and(|n| self.frames >= n) {
            self.begin_close(true);
        }
        if !self.timeout_observed
            && (!self.is_ordinary() || self.frame_limit.is_some())
            && self.started.elapsed() >= self.timeout
        {
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
            Phase::Running(game)
            | Phase::RemoteRunning(game)
            | Phase::RemoteClosing(game)
            | Phase::RemoteReady(game) => request_redraw(game, &title),
            Phase::Closing(owner) => owner.redraw_window(&title),
            Phase::Ready(ready) => ready.redraw_window(&title),
            Phase::Cold(cold) => {
                if let Some(window) = &cold.ui.window {
                    window.set_title(&title);
                    window.request_redraw();
                }
            }
            Phase::IntegratedPreparing(seed) => {
                if let Some(window) = &seed.ui.window {
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
