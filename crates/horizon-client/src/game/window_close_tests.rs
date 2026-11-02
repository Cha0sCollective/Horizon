//! Defining tests of the fixed production application. Window child cases need
//! a real display and complete pinned resources; absent inputs are failures.
use super::*;

#[test]
fn retained_client_launch_rejects_unsupported_entry_before_acquisition() {
    let root = fresh_root("remote-refusal");
    assert!(!root.exists());
    for unsupported in [
        "--server",
        "--dedicated",
        "--session-file",
        "--dev-mod",
        "--world",
        "--in-frame",
        "--paired-native-window-close",
        "--offscreen",
    ] {
        let args = vec![
            "--retained-client-window".into(),
            "127.0.0.1:25565".into(),
            "--retained-client-root".into(),
            root.to_string_lossy().into_owned(),
            unsupported.into(),
            "unused".into(),
        ];
        assert!(RemoteLaunch::parse(&args).is_err());
        assert!(!root.exists());
    }
    for invalid in [
        std::env::temp_dir(),
        PathBuf::from("relative-client"),
        PathBuf::from("/tmp/../client"),
    ] {
        assert!(
            RemoteLaunch::parse(&[
                "--retained-client-window".into(),
                "127.0.0.1:25565".into(),
                "--retained-client-root".into(),
                invalid.to_string_lossy().into_owned()
            ])
            .is_err()
        );
    }
    assert!(
        RemoteLaunch::parse(&["--in-frame".into()])
            .unwrap()
            .is_none()
    );
}

#[test]
#[ignore = "requires a real isolated display and pinned complete resources"]
fn retained_client_actual_original_window_child() {
    let root = PathBuf::from(std::env::var_os("HORIZON_RETAINED_CLIENT_ROOT").expect("fresh root"));
    let address =
        std::env::var("HORIZON_RETAINED_CLIENT_ADDRESS").expect("real isolated server address");
    let resources = std::env::var("HORIZON_RETAINED_CLIENT_RESOURCES").expect("pinned resources");
    let args = vec![
        "--retained-client-window".into(),
        address,
        "--retained-client-root".into(),
        root.to_string_lossy().into_owned(),
        "--resources".into(),
        resources,
        "--width".into(),
        "960".into(),
        "--height".into(),
        "540".into(),
        "--timeout-seconds".into(),
        "180".into(),
    ];
    let parsed = Options::read_from(args)
        .expect("fixed options")
        .expect("launch");
    let outcome = run_remote(
        parsed.options,
        parsed.retained_remote.expect("selected remote recipe"),
    );
    match std::env::var("HORIZON_RETAINED_CLIENT_CASE").as_deref() {
        Ok("tcp-disconnect") => assert!(
            outcome
                .unwrap_err()
                .to_string()
                .contains("ServerDisconnect")
        ),
        Ok("tcp-receive-failure") => assert!(outcome.unwrap_err().to_string().contains("Receive(")),
        Ok("tcp-clean-leave") => outcome.expect("checked original window completion"),
        _ => panic!("explicit actual window case required"),
    }
}

#[test]
#[ignore = "ordinary isolated server fixture; does not qualify retained server closure"]
fn retained_client_ordinary_server_fixture_child() {
    let root = PathBuf::from(std::env::var_os("HORIZON_TEST_RESOURCES").expect("pinned resources"));
    let port = std::env::var("HORIZON_RETAINED_FIXTURE_PORT").expect("isolated listener port");
    let args = vec![
        "--resources".into(),
        root.to_string_lossy().into_owned(),
        "--test-terrain".into(),
        "--control-socket".into(),
        std::env::current_dir()
            .unwrap()
            .join("server-control.sock")
            .to_string_lossy()
            .into_owned(),
        "--dedicated".into(),
        "--nogui".into(),
        "--port".into(),
        port,
        "--world".into(),
        "remote-fixture".into(),
    ];
    let mut options = Options::read_from(args).unwrap().unwrap().options;
    assert!(dedicated::preflight(&mut options).unwrap());
    let settings = Settings {
        render_distance: 2,
        simulation_distance: 2,
        ..Settings::default()
    };
    let started = Instant::now();
    let loaded = load_resources(
        root.clone(),
        &root,
        &settings,
        started,
        false,
        &Progress::new(),
        &mut |_, _| {},
    )
    .unwrap();
    assert!(loaded.pack_error.is_none() && loaded.rest_loader.is_none());
    let resources = loaded.resources;
    let cpu = GameCpuResources::load(&root).unwrap();
    assert!(
        start_game(
            &resources,
            loaded.pack,
            loaded.gui.unwrap(),
            options,
            None,
            Audio::silent(&root),
            root,
            started,
            None,
            Some(settings),
            Some(cpu)
        )
        .unwrap()
        .is_none()
    );
}

fn fresh_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("native-window-{label}-{}", std::process::id()))
}

#[test]
fn native_window_rejects_ordinary_or_authenticated_entry_before_acquisition() {
    let root = fresh_root("refusal");
    assert!(!root.exists());
    for unsupported in [
        "--server",
        "--dedicated",
        "--session-file",
        "--dev-mod",
        "--managed-frozen-recovery",
        "--offscreen",
        "--pregen",
        "--world",
    ] {
        let args = vec![
            "--paired-native-window-close".into(),
            root.to_string_lossy().into_owned(),
            unsupported.into(),
            "unused".into(),
        ];
        assert!(
            Launch::parse(&args).is_err(),
            "unsupported entry {unsupported}"
        );
        assert!(
            !root.exists(),
            "refusal must precede world/control acquisition"
        );
    }
    assert!(Launch::parse(&["--in-frame".into()]).unwrap().is_none());
}

#[test]
fn native_window_rejects_existing_and_relative_roots_without_touching_them() {
    for root in [
        std::env::temp_dir(),
        PathBuf::from("relative-native-world"),
        PathBuf::from("/tmp/../native-world"),
    ] {
        assert!(
            Launch::parse(&[
                "--paired-native-window-close".into(),
                root.to_string_lossy().into_owned()
            ])
            .is_err()
        );
    }
    assert!(std::env::temp_dir().is_dir());
}

#[test]
fn native_window_explicit_launch_selects_fixed_in_frame_profile() {
    let root = fresh_root("parse");
    let args = vec![
        "--paired-native-window-close".into(),
        root.to_string_lossy().into_owned(),
        "--resources".into(),
        "/unused/resources".into(),
        "--frames".into(),
        "2".into(),
    ];
    let parsed = Options::read_from(args).unwrap().unwrap();
    assert!(parsed.native_window.is_some());
    assert!(parsed.frozen.is_none());
    assert!(!parsed.options.server_thread);
    assert!(parsed.options.test_terrain);
    assert_eq!(parsed.options.distance, 2);
    assert_eq!(parsed.options.control, Some(root.join("control.sock")));
    assert!(!root.exists());
}

#[cfg(all(
    feature = "native-writer-test-control",
    debug_assertions,
    target_os = "linux"
))]
fn cold_inputs(
    label: &str,
) -> (
    Loaded,
    Options,
    Settings,
    world_loading::WorldRequest,
    RetainedEntryIdentity,
) {
    let resources =
        PathBuf::from(std::env::var_os("HORIZON_TEST_RESOURCES").expect("full pinned resources"));
    let settings = Settings::default();
    let loaded = load_resources(
        resources.clone(),
        &resources,
        &settings,
        Instant::now(),
        false,
        &Progress::new(),
        &mut |_, _| {},
    )
    .expect("complete actual resource load");
    assert!(loaded.rest_loader.is_none() && loaded.pack_error.is_none() && loaded.gui.is_some());
    let root = fresh_root(label);
    let parsed = Options::read_from(vec![
        "--paired-native-window-close".into(),
        root.to_string_lossy().into_owned(),
        "--resources".into(),
        resources.to_string_lossy().into_owned(),
    ])
    .unwrap()
    .unwrap();
    let mut request = world_loading::WorldRequest::new("cold", Some("Cold"), Some(17), false, None)
        .unwrap()
        .with_identity_intent(world_loading::host_identity::IdentityIntent::CreateNewIdentity);
    request.save_root = root;
    (
        loaded,
        parsed.options,
        settings,
        request,
        RetainedEntryIdentity {
            token: 1,
            owner: Arc::new(()),
        },
    )
}

#[cfg(all(
    feature = "native-writer-test-control",
    debug_assertions,
    target_os = "linux"
))]
fn close_cold(mut cold: ColdPreparation) -> PreparationClosed {
    let deadline = Instant::now() + Duration::from_secs(20);
    cold.begin_close(deadline);
    loop {
        match cold.poll_close() {
            PreparationCloseStatus::Pending => {}
            PreparationCloseStatus::Blocked(error) => panic!("cold native close blocked: {error}"),
            PreparationCloseStatus::Complete => break,
        }
        assert!(
            Instant::now() < deadline,
            "native deadline is not a completion witness"
        );
        std::thread::yield_now();
    }
    match cold.finish_close() {
        Ok(closed) => closed,
        Err(_) => panic!("original close handoff refused"),
    }
}

#[cfg(all(
    feature = "native-writer-test-control",
    debug_assertions,
    target_os = "linux"
))]
#[test]
#[ignore = "isolated complete resources; original cold native workers without any Game"]
fn cold_foreign_entry_retains_original_native_owners_until_checked_close() {
    let (loaded, options, settings, request, entry) = cold_inputs("foreign-entry");
    let root = request.save_root.clone();
    let cold = ColdPreparation::prepare(
        request,
        &loaded.resources,
        &loaded.pack,
        &options,
        &settings,
        &entry,
    );
    assert!(cold.original_error().is_none());
    let foreign = RetainedEntryIdentity {
        token: entry.token,
        owner: Arc::new(()),
    };
    let cold = match cold.into_prepared(&foreign) {
        Err(cold) => cold,
        Ok(_) => panic!("foreign entry consumed original native owners"),
    };
    assert!(horizon_client::save::SessionLock::acquire(&root).is_err());
    let closed = close_cold(cold);
    assert!(closed.matches_entry(&entry) && !closed.matches_entry(&foreign));
    assert!(closed.original_error.is_none());
    let lock = horizon_client::save::SessionLock::acquire(&root)
        .expect("actual original session released after native closure");
    drop(lock);
}

#[cfg(all(
    feature = "native-writer-test-control",
    debug_assertions,
    target_os = "linux"
))]
#[test]
#[ignore = "isolated complete resources; unsupported cold topology refuses before acquisition"]
fn cold_unsupported_topology_preserves_original_error_without_native_acquisition() {
    let (loaded, mut options, settings, request, entry) = cold_inputs("unsupported");
    let root = request.save_root.clone();
    options.server_thread = true;
    let cold = ColdPreparation::prepare(
        request,
        &loaded.resources,
        &loaded.pack,
        &options,
        &settings,
        &entry,
    );
    let original = cold
        .original_error()
        .expect("explicit topology refusal")
        .to_owned();
    assert!(!root.exists());
    let cold = match cold.into_prepared(&entry) {
        Err(cold) => cold,
        Ok(_) => panic!("unsupported cold input adopted"),
    };
    let closed = close_cold(cold);
    assert_eq!(closed.original_error.as_deref(), Some(original.as_str()));
    assert!(closed.matches_entry(&entry) && !root.exists());
}

#[cfg(all(
    feature = "native-writer-test-control",
    debug_assertions,
    target_os = "linux"
))]
fn settle_native_game(game: Box<Game<'_>>) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut owner = CloseOwner::begin(game, CloseProfile::NativeWorld, deadline);
    loop {
        match owner.poll() {
            CloseStatus::Pending(_) => {}
            CloseStatus::Blocked(error) => {
                panic!("original native capability close blocked: {error}")
            }
            CloseStatus::NativeReady => break,
        }
        assert!(
            Instant::now() < deadline,
            "watchdog is not native completion"
        );
        std::thread::yield_now();
    }
    let ready = match owner.try_native_ready() {
        Ok(ready) => ready,
        Err(_) => panic!("native receipt absent"),
    };
    assert!(
        ready.try_dispose_dormant().is_ok(),
        "original checked native owner disposal"
    );
}

#[cfg(all(
    feature = "native-writer-test-control",
    debug_assertions,
    target_os = "linux"
))]
#[test]
#[ignore = "actual cold factory authority, native Level IO, foreign/legacy/mode refusal and checked close"]
fn cold_native_close_authority_binds_original_and_all_three_gates() {
    let (mut loaded, options, settings, request, entry) = cold_inputs("authority-first");
    let root = options.resources.clone();
    // Complete fallible CPU/GUI preparation before acquiring either original
    // native world. No test-only transfer issues a production capability.
    let cpu_first = GameCpuResources::load(&root).unwrap();
    let cpu_second = GameCpuResources::load(&root).unwrap();
    let cpu_legacy = GameCpuResources::load(&root).unwrap();
    let gui_first = loaded.gui.take().unwrap();
    let gui_second = UiResources::load(&root, &root).unwrap();
    let gui_legacy = UiResources::load(&root, &root).unwrap();
    let cold_first = ColdPreparation::prepare(
        request,
        &loaded.resources,
        &loaded.pack,
        &options,
        &settings,
        &entry,
    );
    let first = match cold_first.into_prepared(&entry) {
        Ok(parts) => parts,
        Err(_) => panic!("genuine first cold factory refused"),
    };
    let mut first = Box::new(
        Game::new_with_world(
            &loaded.resources,
            loaded.pack.clone(),
            gui_first,
            options.clone(),
            settings.clone(),
            Audio::silent(&root),
            Some(cpu_first),
            InitialGameWorld::Retained(Box::new(first)),
        )
        .unwrap(),
    );
    let mut second_request =
        world_loading::WorldRequest::new("second", Some("Second"), Some(19), false, None)
            .unwrap()
            .with_identity_intent(world_loading::host_identity::IdentityIntent::CreateNewIdentity);
    second_request.save_root = fresh_root("authority-second");
    let second_entry = RetainedEntryIdentity {
        token: 1,
        owner: Arc::new(()),
    };
    let cold_second = ColdPreparation::prepare(
        second_request,
        &loaded.resources,
        &loaded.pack,
        &options,
        &settings,
        &second_entry,
    );
    let second = match cold_second.into_prepared(&second_entry) {
        Ok(parts) => parts,
        Err(_) => panic!("genuine second cold factory refused"),
    };
    let second = Box::new(
        Game::new_with_world(
            &loaded.resources,
            loaded.pack.clone(),
            gui_second,
            options.clone(),
            settings.clone(),
            Audio::silent(&root),
            Some(cpu_second),
            InitialGameWorld::Retained(Box::new(second)),
        )
        .unwrap(),
    );
    first.load_world_state();
    assert!(first.options.execution_role.is_client());
    // Fresh Level uses an empty Overworld representation. Bind that exact
    // original string instead of accepting arbitrary Dim::from_id fallbacks.
    let original_dimension = first.level.dimension.clone();
    assert!(original_dimension.is_empty());
    assert!(
        first
            .native_close_authority
            .as_ref()
            .unwrap()
            .authorizes(&first)
    );
    // Retain the exact original allocation outside Game while checking absence.
    // These are pure admission queries, not a running no-storage client recipe.
    let original_save = first.save.take().expect("actual cold storage owner");
    let save_identity = Arc::downgrade(&original_save);
    let before_uuid = first.level.player_uuid;
    assert!(!first.native_close_owner_authorized());
    assert!(
        !first
            .native_close_authority
            .as_ref()
            .unwrap()
            .authorizes(&first)
    );
    assert!(first.prepare_close_identity_fields().is_err());
    assert!(first.prepare_retained_level_metadata().is_err());
    assert_eq!(first.level.player_uuid, before_uuid);
    first.save = Some(original_save);
    assert!(Arc::ptr_eq(
        &save_identity.upgrade().unwrap(),
        first.save.as_ref().unwrap()
    ));
    assert!(first.native_close_owner_authorized());
    assert!(
        first
            .native_close_authority
            .as_ref()
            .unwrap()
            .authorizes(&first)
    );
    assert!(
        !first
            .native_close_authority
            .as_ref()
            .unwrap()
            .authorizes(&second)
    );
    assert!(
        !second
            .native_close_authority
            .as_ref()
            .unwrap()
            .authorizes(&first)
    );

    let mut legacy = Game::new(
        &loaded.resources,
        loaded.pack.clone(),
        gui_legacy,
        options,
        settings,
        Audio::silent(&root),
        Some(cpu_legacy),
    )
    .unwrap();
    assert!(legacy.native_close_authority.is_none() && legacy.options.execution_role.is_client());
    // The ordinary server fallback also requires storage; Some keeps its old
    // role behavior. Do not run native IO or accept mutation in the absent state.
    let original_role = legacy.options.execution_role;
    legacy.options.execution_role = execution_role::ExecutionRole::IntegratedServer;
    assert!(legacy.native_close_owner_authorized());
    let original_save = legacy.save.take().expect("actual legacy storage owner");
    let save_identity = Arc::downgrade(&original_save);
    let before_uuid = legacy.level.player_uuid;
    assert!(!legacy.native_close_owner_authorized());
    assert!(legacy.prepare_close_identity_fields().is_err());
    assert!(legacy.prepare_retained_level_metadata().is_err());
    assert_eq!(legacy.level.player_uuid, before_uuid);
    legacy.save = Some(original_save);
    assert!(Arc::ptr_eq(
        &save_identity.upgrade().unwrap(),
        legacy.save.as_ref().unwrap()
    ));
    assert!(legacy.native_close_owner_authorized());
    legacy.options.execution_role = original_role;
    let original_uuid = legacy.level.player_uuid;
    assert!(legacy.prepare_close_identity_fields().is_err());
    assert_eq!(legacy.level.player_uuid, original_uuid);
    legacy.level.player_uuid = Some([1, 2, 3, 4]);
    assert!(
        legacy.prepare_retained_level_metadata().is_err(),
        "UUID presence is not authority"
    );
    drop(legacy);

    // This component check performs genuine native Level IO on the original
    // otherwise-idle cold world, then verifies the exact actual post receipt.
    first.prepare_close_identity_fields().unwrap();
    let (expected, input) = first.prepare_retained_level_metadata().unwrap();
    let io = std::thread::spawn(move || {
        input.capture_native()?;
        Ok::<_, String>(input.write_native())
    })
    .join()
    .expect("actual native Level actor join")
    .expect("actual native Level capture");
    let post = match io.result {
        Ok(post) => post,
        Err(error) => panic!("actual native Level write: {error}"),
    };
    let original_save = first
        .save
        .take()
        .expect("original restored cold storage owner");
    let before_uuid = first.level.player_uuid;
    assert!(
        first
            .preflight_retained_level_metadata(&expected, &post)
            .is_err()
    );
    assert_eq!(first.level.player_uuid, before_uuid);
    first.save = Some(original_save);
    assert!(
        first
            .preflight_retained_level_metadata(&expected, &post)
            .is_ok()
    );
    for mode in 0..4 {
        match mode {
            0 => first.options.server_thread = true,
            1 => first.options.execution_role = execution_role::ExecutionRole::IntegratedServer,
            2 => first.options.test_terrain = false,
            _ => first.level.dimension = "unrecognized:dimension".into(),
        }
        let before = first.level.player_uuid;
        assert!(
            !first
                .native_close_authority
                .as_ref()
                .unwrap()
                .authorizes(&first)
        );
        assert!(first.prepare_close_identity_fields().is_err());
        assert_eq!(first.level.player_uuid, before);
        assert!(first.prepare_retained_level_metadata().is_err());
        assert!(
            first
                .preflight_retained_level_metadata(&expected, &post)
                .is_err()
        );
        first.options.server_thread = false;
        first.options.execution_role = execution_role::ExecutionRole::Client;
        first.options.test_terrain = true;
        first.level.dimension = original_dimension.clone();
    }
    assert!(
        first
            .preflight_retained_level_metadata(&expected, &post)
            .is_ok()
    );
    settle_native_game(first);
    settle_native_game(second);
}

#[cfg(all(
    feature = "native-writer-test-control",
    debug_assertions,
    target_os = "linux"
))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Case {
    UnixIoRetry,
    TcpCleanup,
    SignalRender,
}

#[cfg(all(
    feature = "native-writer-test-control",
    debug_assertions,
    target_os = "linux"
))]
pub(super) struct Control {
    case: Case,
    io: Option<paired_close::WindowIoTestGate>,
    cleanup: Option<horizon_client::control::WindowCleanupHold>,
    release_path: PathBuf,
    restore_path: PathBuf,
    io_reported: bool,
    release_reported: bool,
    ready_reported: bool,
    draw_failure_injected: bool,
    signal_restore_reported: bool,
    signal_draw_entered: bool,
    signal_draw_returned: bool,
    signal_preseal_reported: bool,
    signal_seal_reported: bool,
    signal_recovery_input_reported: bool,
    signal_status_presented: bool,
    pub(super) native_resizes: usize,
    pub(super) actual_presentations: usize,
    world_presentations: usize,
    post_terminal_presentation: bool,
}

#[cfg(all(
    feature = "native-writer-test-control",
    debug_assertions,
    target_os = "linux"
))]
impl Control {
    pub(super) fn from_environment() -> Option<Self> {
        let case = match std::env::var("HORIZON_NATIVE_WINDOW_CASE").ok()?.as_str() {
            "unix-io-retry" => Case::UnixIoRetry,
            "tcp-cleanup" => Case::TcpCleanup,
            "signal-render" => Case::SignalRender,
            other => panic!("unknown native window case {other}"),
        };
        let release_path = PathBuf::from(
            std::env::var_os("HORIZON_NATIVE_WINDOW_RELEASE").expect("owned test release path"),
        );
        assert!(release_path.is_absolute() && !release_path.exists());
        let restore_path = PathBuf::from(
            std::env::var_os("HORIZON_NATIVE_WINDOW_RESTORE")
                .expect("owned test presentation-restore path"),
        );
        assert!(restore_path.is_absolute() && !restore_path.exists());
        Some(Self {
            case,
            io: None,
            cleanup: None,
            release_path,
            restore_path,
            io_reported: false,
            release_reported: false,
            ready_reported: false,
            draw_failure_injected: false,
            signal_restore_reported: false,
            signal_draw_entered: false,
            signal_draw_returned: false,
            signal_preseal_reported: false,
            signal_seal_reported: false,
            signal_recovery_input_reported: false,
            signal_status_presented: false,
            native_resizes: 0,
            actual_presentations: 0,
            world_presentations: 0,
            post_terminal_presentation: false,
        })
    }
    pub(super) fn prepare_control(&mut self, control: &mut RetainedControl) {
        if self.case == Case::TcpCleanup {
            self.cleanup = Some(
                control
                    .arm_window_cleanup_hold()
                    .expect("original prepared cleanup hold"),
            );
        }
    }
    pub(super) fn before_poll(&mut self, application: &mut Application<'_>) {
        if self.io.is_none()
            && self.case != Case::TcpCleanup
            && let Phase::Closing(owner) = &mut application.phase
        {
            self.io = Some(owner.hold_window_native_io_for_test());
        }
        if self.io.as_ref().is_some_and(|gate| gate.entered()) && !self.io_reported {
            self.io_reported = true;
            eprintln!(
                "S1WC1_NATIVE_IO_HELD pid={} presentations={}",
                std::process::id(),
                self.actual_presentations
            );
        }
        if self.case != Case::TcpCleanup
            && self.io_reported
            && !self.release_reported
            && self.release_path.exists()
        {
            let metadata = std::fs::symlink_metadata(&self.release_path).unwrap();
            assert!(metadata.is_file() && metadata.len() == 8);
            assert_eq!(std::fs::read(&self.release_path).unwrap(), b"release\n");
            self.io.as_ref().unwrap().release();
            self.release_reported = true;
            eprintln!("S1WC1_NATIVE_IO_RELEASED pid={}", std::process::id());
        }
        if self.cleanup.as_ref().is_some_and(|gate| gate.entered()) && !self.io_reported {
            self.io_reported = true;
            eprintln!(
                "S1WC1_CONTROL_CLEANUP_HELD pid={} presentations={}",
                std::process::id(),
                self.actual_presentations
            );
        }
        if self.case == Case::TcpCleanup
            && self.io_reported
            && !self.release_reported
            && self.release_path.exists()
        {
            assert_eq!(std::fs::read(&self.release_path).unwrap(), b"release\n");
            self.cleanup.as_ref().unwrap().release();
            self.release_reported = true;
            eprintln!("S1WC1_CONTROL_CLEANUP_RELEASED pid={}", std::process::id());
        }
        if matches!(application.phase, Phase::Ready(_)) && !self.ready_reported {
            self.ready_reported = true;
            eprintln!(
                "S1WC1_WORLD_READY pid={} resizes={} presentations={} attempt={}",
                std::process::id(),
                self.native_resizes,
                self.actual_presentations,
                application.attempt
            );
        }
        if self.case == Case::SignalRender
            && self.signal_draw_returned
            && !self.signal_preseal_reported
        {
            self.signal_preseal_reported = true;
            eprintln!(
                "S1WC1_SIGNAL_PRESEAL_STATE pid={} status_presented={} surface_occluded={} usable={}",
                std::process::id(),
                application.status_presented,
                application.surface_occluded,
                application.view().usable
            );
        }
    }
    pub(super) fn inject_draw_error(&mut self, terminal: bool) -> bool {
        if self.case == Case::TcpCleanup && terminal && !self.draw_failure_injected {
            self.draw_failure_injected = true;
            eprintln!(
                "S1WC1_TERMINAL_FRAME_ERROR_INJECTED pid={}",
                std::process::id()
            );
            return true;
        }
        if self.case == Case::SignalRender && self.io_reported && !self.restore_path.exists() {
            if !self.draw_failure_injected {
                self.draw_failure_injected = true;
                eprintln!("S1WC1_FRAME_ERROR_INJECTED pid={}", std::process::id());
            }
            return true;
        }
        if self.case == Case::SignalRender && self.restore_path.exists() {
            let metadata = std::fs::symlink_metadata(&self.restore_path).unwrap();
            assert!(metadata.is_file() && metadata.len() == 8);
            assert_eq!(std::fs::read(&self.restore_path).unwrap(), b"restore\n");
            if !self.signal_restore_reported {
                self.signal_restore_reported = true;
                eprintln!("S1WC1_SIGNAL_RESTORE_VALIDATED pid={}", std::process::id());
            }
        }
        false
    }
    pub(super) fn before_draw(&mut self) {
        if self.case == Case::SignalRender
            && self.signal_restore_reported
            && !self.signal_draw_entered
        {
            self.signal_draw_entered = true;
            eprintln!(
                "S1WC1_SIGNAL_RESTORE_DRAW_ENTERED pid={}",
                std::process::id()
            );
        }
    }
    pub(super) fn after_draw(&mut self, result: &Result<bool>) {
        if self.case != Case::SignalRender || !self.signal_draw_entered || self.signal_draw_returned
        {
            return;
        }
        self.signal_draw_returned = true;
        match result {
            Ok(presented) => eprintln!(
                "S1WC1_SIGNAL_RESTORE_DRAW_RETURNED pid={} presented={presented}",
                std::process::id()
            ),
            Err(error) => eprintln!(
                "S1WC1_SIGNAL_RESTORE_DRAW_RETURNED pid={} error={error}",
                std::process::id()
            ),
        }
    }
    pub(super) fn control_seal_admitted(&mut self, admission_sealed: bool) {
        if self.case == Case::SignalRender && admission_sealed && !self.signal_seal_reported {
            self.signal_seal_reported = true;
            eprintln!(
                "S1WC1_SIGNAL_CONTROL_SEAL_ADMITTED pid={}",
                std::process::id()
            );
        }
    }
    pub(super) fn status_recovery_input(&mut self) {
        if self.case == Case::SignalRender && !self.signal_recovery_input_reported {
            self.signal_recovery_input_reported = true;
            eprintln!(
                "S1WC1_SIGNAL_STATUS_RECOVERY_INPUT pid={}",
                std::process::id()
            );
        }
    }
    pub(super) fn presented(&mut self, world: bool) {
        self.actual_presentations += 1;
        if self.case == Case::SignalRender
            && !world
            && self.signal_restore_reported
            && !self.signal_status_presented
        {
            self.signal_status_presented = true;
            eprintln!("S1WC1_SIGNAL_STATUS_PRESENTED pid={}", std::process::id());
        }
        if self.case == Case::TcpCleanup
            && !world
            && self.draw_failure_injected
            && !self.post_terminal_presentation
        {
            self.post_terminal_presentation = true;
            eprintln!("S1WC1_TERMINAL_STATUS_PRESENTED pid={}", std::process::id());
        }
        if world {
            self.world_presentations += 1;
            if self.world_presentations == 2 {
                eprintln!(
                    "S1WC1_ORIGINAL_WORLD_PRESENTED pid={} count=2",
                    std::process::id()
                );
            }
        }
    }
    pub(super) fn native_title_tag(&self) -> Option<&'static str> {
        self.post_terminal_presentation
            .then_some("S1WC1-post-terminal-error-presented")
    }
}

#[cfg(all(
    feature = "native-writer-test-control",
    debug_assertions,
    target_os = "linux"
))]
#[test]
#[ignore = "coordinator-owned real display, pinned resources and native IO controls"]
fn native_window_actual_production_window_child() {
    let case = std::env::var("HORIZON_NATIVE_WINDOW_CASE").expect("explicit actual window case");
    assert!(matches!(
        case.as_str(),
        "unix-io-retry" | "tcp-cleanup" | "signal-render"
    ));
    let resources =
        PathBuf::from(std::env::var_os("HORIZON_TEST_RESOURCES").expect("full pinned resources"));
    assert!(resources.join("horizon-resources.json").is_file());
    let root = std::env::current_dir().unwrap().join("native-window-root");
    assert!(!root.exists());
    let socket = root.join(if case == "tcp-cleanup" {
        "control.json"
    } else {
        "control.sock"
    });
    let args = vec![
        "--paired-native-window-close".into(),
        root.to_string_lossy().into_owned(),
        "--resources".into(),
        resources.to_string_lossy().into_owned(),
        "--control-socket".into(),
        socket.to_string_lossy().into_owned(),
        "--timeout-seconds".into(),
        "180".into(),
    ];
    let parsed = Options::read_from(args).unwrap().unwrap();
    assert_eq!(parsed.options.timeout, Duration::from_secs(180));
    eprintln!("S1WC1_WINDOW_START pid={} case={case}", std::process::id());
    let result = run(parsed.options, parsed.native_window.unwrap());
    match case.as_str() {
        "unix-io-retry" => assert!(
            result.is_ok(),
            "original application close failed: {result:?}"
        ),
        "tcp-cleanup" => assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("native control terminal failure")
        ),
        "signal-render" => assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("controlled native frame error")
        ),
        _ => unreachable!(),
    }
    eprintln!(
        "S1WC1_WINDOW_COMPLETE pid={} case={case}",
        std::process::id()
    );
}
