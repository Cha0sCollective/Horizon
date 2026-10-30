//! Actual owned native startup/lifetime; no machine activation or recovery ingress.
#[cfg(feature = "mods")]
use super::host_identity::IdentityPreflight;
use super::host_identity::{IDENTITY_FILE, IdentityIntent};
use super::*;
use crate::game::integrated::transition_ownership_tests::Fixture;
use horizon_client::{
    chunk::Chunk,
    save::SessionLock,
    world::counter_restore::{
        CounterFrameV2, CounterTarget, FrameBinding, FrameState, NativeBinding, Placement,
        RestoreError, WorldId,
    },
};
use horizon_core::random::RandomSource;
use horizon_world::{
    compression::Compression,
    nbt::{self, Limits, Tag},
};
#[cfg(feature = "mods")]
use std::collections::BTreeSet;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
const WAIT: Duration = Duration::from_secs(120);
type Disk = BTreeMap<PathBuf, Option<Vec<u8>>>;
fn disk(root: &Path) -> Disk {
    fn visit(path: &Path, out: &mut Disk) {
        let meta = fs::symlink_metadata(path).unwrap();
        assert!(!meta.file_type().is_symlink());
        if meta.is_dir() {
            out.insert(path.to_owned(), None);
            for row in fs::read_dir(path).unwrap() {
                visit(&row.unwrap().path(), out);
            }
        } else {
            assert!(meta.is_file());
            out.insert(path.to_owned(), Some(fs::read(path).unwrap()));
        }
    }
    let mut out = BTreeMap::new();
    if root.exists() {
        visit(root, &mut out);
    }
    out
}
fn request(root: &Path, intent: IdentityIntent) -> WorldRequest {
    WorldRequest::new(
        root.to_str().unwrap(),
        Some("Owned Host Identity"),
        Some(17),
        false,
        None,
    )
    .unwrap()
    .with_identity_intent(intent)
}
fn prepare(game: &Game<'_>, root: &Path, intent: IdentityIntent) -> Result<PreparedWorld> {
    request(root, intent).prepare(&game.res, &game.pack, &game.options, &game.settings)
}
fn client<'a>(template: &'a Game<'_>, integrated: bool) -> Game<'a> {
    let mut options = template.options.clone();
    options.test_terrain = true;
    options.server_thread = integrated;
    options.execution_role = execution_role::ExecutionRole::Client;
    options.dedicated = None;
    let mut settings = template.settings.clone();
    settings.render_distance = 2;
    settings.simulation_distance = 2;
    let gui = UiResources {
        atlas: horizon_client::hud::HudAtlas::fonts(&options.resources).unwrap(),
        panorama: Arc::new(horizon_client::gfx::panorama::Faces {
            size: 1,
            pixels: [0u8; 4].repeat(6),
        }),
    };
    let audio = horizon_client::audio::Audio::silent(&options.resources);
    let mut game = Game::new(
        &template.res,
        template.pack.clone(),
        gui,
        options,
        settings,
        audio,
        None,
    )
    .unwrap();
    game.world_root = template.world_root.clone();
    game
}
fn root(game: &Game<'_>, name: &str) -> PathBuf {
    game.world_root.parent().unwrap().join(name)
}
fn identity_on_disk(root: &Path) -> String {
    let value: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(IDENTITY_FILE)).unwrap()).unwrap();
    value["world_uuid"].as_str().unwrap().to_owned()
}
fn unlocked(root: &Path) {
    drop(SessionLock::acquire(root).expect("native session released"));
}
fn close(game: &mut Game<'_>) {
    game.close_world().unwrap();
    let deadline = Instant::now() + WAIT;
    while game.pending_save.is_some() {
        game.poll_closing_save().unwrap();
        assert!(
            Instant::now() < deadline,
            "native closing-save completion deadline"
        );
        std::thread::yield_now();
    }
    assert!(game.host_world_identity().is_none());
    assert!(game.session_lock.is_none());
}
fn open(game: &mut Game<'_>, root: &Path, intent: IdentityIntent) -> Result<()> {
    game.open_world_with_identity(
        root.to_str().unwrap(),
        Some("Owned Host Identity"),
        Some(17),
        false,
        None,
        intent,
    )
}
#[test]
#[ignore = "isolated actual Game resources: native identity preparation and legacy controls"]
fn host_identity_startup_unpaired_prepare_reopen_and_legacy_default() {
    let fixture = Fixture::new("host-id-prepare");
    let template = fixture.game();
    let game = client(&template, false);
    assert!(!game.options.mod_packages.has_pairs());
    let identified = root(&game, "identified");
    let prepared = prepare(&game, &identified, IdentityIntent::CreateNewIdentity).unwrap();
    let world = prepared.session_lock.expected_world().unwrap().to_string();
    assert_eq!(identity_on_disk(&identified), world);
    assert!(prepared.world.counter_domain_snapshot().is_none());
    assert!(SessionLock::acquire(&identified).is_err());
    assert!(!identified.join("horizon-paired-packages.json").exists());
    let before = disk(&identified);
    assert!(prepare(&game, &identified, IdentityIntent::RequireExistingIdentity).is_err());
    assert_eq!(disk(&identified), before);
    drop(prepared);
    unlocked(&identified);
    let before = disk(&identified);
    let reopened = prepare(&game, &identified, IdentityIntent::RequireExistingIdentity).unwrap();
    assert_eq!(
        reopened.session_lock.expected_world().unwrap().to_string(),
        world
    );
    assert_eq!(disk(&identified), before);
    drop(reopened);
    let legacy = prepare(&game, &identified, IdentityIntent::Legacy).unwrap();
    assert!(legacy.session_lock.expected_world().is_none());
    drop(legacy);
    assert_eq!(identity_on_disk(&identified), world);
    let plain = root(&game, "ordinary-legacy");
    let legacy = prepare(&game, &plain, IdentityIntent::Legacy).unwrap();
    assert!(legacy.session_lock.expected_world().is_none());
    assert!(!plain.join(IDENTITY_FILE).exists());
    drop(legacy);
    let before = disk(&plain);
    assert!(prepare(&game, &plain, IdentityIntent::RequireExistingIdentity).is_err());
    assert_eq!(disk(&plain), before);
    for (index, field) in ["DataVersion", "Time", "missing-identity"]
        .into_iter()
        .enumerate()
    {
        let bad = root(&game, &format!("refused-{index}"));
        drop(prepare(&game, &bad, IdentityIntent::CreateNewIdentity).unwrap());
        if field == "missing-identity" {
            fs::remove_file(bad.join(IDENTITY_FILE)).unwrap();
        } else {
            let path = bad.join("level.dat");
            let raw = Compression::Gzip
                .decode(&fs::read(&path).unwrap(), 1 << 20)
                .unwrap();
            let mut named = nbt::read_named(&raw, Limits::default()).unwrap().0;
            let Some(Tag::Compound(data)) = named.tag.get_mut("Data") else {
                panic!()
            };
            data.insert(field.into(), Tag::Int(0));
            let raw = nbt::write_named(&named, Limits::default()).unwrap();
            fs::write(path, Compression::Gzip.encode(&raw).unwrap()).unwrap();
        }
        fs::write(bad.join("session.lock"), b"retained regular lock bytes").unwrap();
        let before = disk(&bad);
        assert!(prepare(&game, &bad, IdentityIntent::RequireExistingIdentity).is_err());
        assert_eq!(
            disk(&bad),
            before,
            "required preflight must precede native lock truncation"
        );
    }
}
#[cfg(feature = "mods")]
#[test]
#[ignore = "isolated actual resources and existing prebuilt HORIZON_COUNTER_PAIR_FIXTURE"]
fn host_identity_startup_paired_package_order_and_failure_artifacts() {
    let fixture = Fixture::new("host-id-paired");
    let template = fixture.game();
    let mut game = client(&template, false);
    let bundle = PathBuf::from(
        std::env::var_os("HORIZON_COUNTER_PAIR_FIXTURE")
            .expect("owned prebuilt Rust pair required; no runtime compiler invocation"),
    );
    game.options.mod_packages = mod_packages::Packages::read(&BTreeSet::new(), &[bundle]).unwrap();
    assert!(game.options.mod_packages.has_pairs());
    let identified = root(&game, "paired-identified");
    let prepared = prepare(&game, &identified, IdentityIntent::CreateNewIdentity).unwrap();
    let world = prepared.session_lock.expected_world().unwrap().to_string();
    let package = fs::read(identified.join("horizon-paired-packages.json")).unwrap();
    assert_eq!(identity_on_disk(&identified), world);
    drop(prepared);
    let before = disk(&identified);
    let reopened = prepare(&game, &identified, IdentityIntent::RequireExistingIdentity).unwrap();
    assert_eq!(
        reopened.session_lock.expected_world().unwrap().to_string(),
        world
    );
    drop(reopened);
    assert_eq!(disk(&identified), before);
    let partial = root(&game, "paired-partial");
    let preflight = IdentityPreflight::read(&partial, IdentityIntent::CreateNewIdentity)
        .unwrap()
        .unwrap();
    let lock = SessionLock::acquire(&partial).unwrap();
    preflight.recheck(&partial).unwrap();
    game.options.mod_packages.record_world(&partial).unwrap();
    assert_eq!(
        fs::read(partial.join("horizon-paired-packages.json")).unwrap(),
        package
    );
    // Controlled exclusive-create collision after the exact package-first handoff.
    fs::write(partial.join(IDENTITY_FILE), b"exclusive collision evidence").unwrap();
    let before = disk(&partial);
    let error = preflight.finish(&partial, lock).err().unwrap();
    assert!(error.to_string().contains("exclusive identity create"));
    assert_eq!(disk(&partial), before);
    assert!(!partial.join("level.dat").exists());
    unlocked(&partial);
    assert!(prepare(&game, &partial, IdentityIntent::CreateNewIdentity).is_err());
    assert!(prepare(&game, &partial, IdentityIntent::RequireExistingIdentity).is_err());
    assert_eq!(disk(&partial), before);
    // An unpaired opener cannot silently drop the unchanged package-v1 record.
    let ordinary = fixture.game();
    let selected_before = disk(&identified);
    assert!(
        prepare(
            &ordinary,
            &identified,
            IdentityIntent::RequireExistingIdentity
        )
        .is_err()
    );
    assert_eq!(disk(&identified), selected_before);
}
#[test]
#[ignore = "isolated actual Game resources: in-frame and genuine deferred/cancelled world entry"]
fn host_identity_startup_in_frame_and_deferred_entry_preserve_intent() {
    let fixture = Fixture::new("host-id-in-frame");
    let template = fixture.game();
    let mut game = client(&template, false);
    let first = root(&game, "in-frame");
    open(&mut game, &first, IdentityIntent::CreateNewIdentity).unwrap();
    assert!(game.integrated.is_none() && game.remote.is_none());
    let world = game.host_world_identity().unwrap().to_string();
    assert_eq!(identity_on_disk(&first), world);
    assert!(SessionLock::acquire(&first).is_err());
    close(&mut game);
    unlocked(&first);
    open(&mut game, &first, IdentityIntent::RequireExistingIdentity).unwrap();
    assert_eq!(game.host_world_identity().unwrap().to_string(), world);
    close(&mut game);
    let cancelled = first.parent().unwrap().join("cancelled-native-loader");
    let loader = WorldLoader::start(
        request(&cancelled, IdentityIntent::CreateNewIdentity),
        game.res.clone(),
        game.pack.clone(),
        game.options.clone(),
        game.settings.clone(),
    )
    .unwrap();
    game.world_loader = Some(loader.cancel());
    let deferred = first.parent().unwrap().join("deferred-identified");
    open(&mut game, &deferred, IdentityIntent::CreateNewIdentity).unwrap();
    assert!(matches!(
        &game.pending_entry,
        Some(PendingWorldEntry::IdentifiedLocal {
            identity_intent: IdentityIntent::CreateNewIdentity,
            ..
        })
    ));
    assert!(!deferred.exists());
    assert!(game.host_world_identity().is_none());
    let deadline = Instant::now() + WAIT;
    while game.pending_entry.is_some() || game.world_loader.is_some() {
        game.resume_world_entry().unwrap();
        assert!(Instant::now() < deadline, "deferred entry deadline");
        std::thread::yield_now();
    }
    let deferred_world = game.host_world_identity().unwrap().to_string();
    assert_eq!(identity_on_disk(&deferred), deferred_world);
    assert_ne!(deferred_world, world);
    assert_ne!(deferred_world, identity_on_disk(&cancelled));
    unlocked(&cancelled);
    assert!(SessionLock::acquire(&deferred).is_err());
    close(&mut game);
    unlocked(&deferred);
}
#[test]
#[ignore = "isolated actual Game resources: background preparation failures and cancellation retain ownership"]
fn host_identity_startup_background_cancellation_errors_and_lock_lifetime() {
    let fixture = Fixture::new("host-id-background");
    let template = fixture.game();
    let game = client(&template, false);
    let background = root(&game, "background-native");
    let loader = WorldLoader::start(
        request(&background, IdentityIntent::CreateNewIdentity),
        game.res.clone(),
        game.pack.clone(),
        game.options.clone(),
        game.settings.clone(),
    )
    .unwrap();
    let prepared = loader.finish().unwrap();
    assert!(prepared.session_lock.expected_world().is_some());
    assert!(SessionLock::acquire(&background).is_err());
    drop(prepared);
    unlocked(&background);
    let reopen = WorldLoader::start(
        request(&background, IdentityIntent::RequireExistingIdentity),
        game.res.clone(),
        game.pack.clone(),
        game.options.clone(),
        game.settings.clone(),
    )
    .unwrap();
    let before = disk(&background);
    let cancelled = reopen.cancel();
    assert!(cancelled.finish().is_err());
    unlocked(&background);
    assert_eq!(disk(&background), before);
    assert!(game.host_world_identity().is_none());
    let failed = root(&game, "failed-preparation");
    let mut options = game.options.clone();
    options.test_terrain = false;
    options.resources = failed.parent().unwrap().join("absent-resources");
    let loader = WorldLoader::start(
        request(&failed, IdentityIntent::CreateNewIdentity),
        game.res.clone(),
        game.pack.clone(),
        options,
        game.settings.clone(),
    )
    .unwrap();
    assert!(
        loader.finish().is_err(),
        "native terrain construction with missing worldgen must fail"
    );
    assert!(failed.join(IDENTITY_FILE).is_file());
    assert!(!failed.join("level.dat").exists());
    unlocked(&failed);
    let before = disk(&failed);
    assert!(prepare(&game, &failed, IdentityIntent::CreateNewIdentity).is_err());
    assert!(prepare(&game, &failed, IdentityIntent::RequireExistingIdentity).is_err());
    assert_eq!(disk(&failed), before);
}
#[test]
#[ignore = "isolated actual Game resources: real integrated Launch identity and native shutdown"]
fn host_identity_startup_actual_integrated_launch_reopen_and_shutdown() {
    let fixture = Fixture::new("host-id-integrated");
    let template = fixture.game();
    let mut game = client(&template, true);
    let identified = root(&game, "integrated-identified");
    open(&mut game, &identified, IdentityIntent::CreateNewIdentity).unwrap();
    assert!(game.remote.is_some() && game.integrated.is_some());
    assert!(game.host_world_identity().is_none());
    let observed = game
        .integrated
        .as_ref()
        .unwrap()
        .call(|server| {
            (
                server.host_world_identity().unwrap().to_string(),
                server.options.execution_role == execution_role::ExecutionRole::IntegratedServer
                    && server.remote.is_none()
                    && server.serving.is_some(),
                server.session_lock.is_some(),
                server.world.counter_domain_snapshot().is_none(),
            )
        })
        .expect("actual server thread services its owned identity observation");
    assert!(observed.1 && observed.2 && observed.3);
    let world = observed.0;
    assert_eq!(identity_on_disk(&identified), world);
    assert!(SessionLock::acquire(&identified).is_err());
    close(&mut game);
    assert!(game.integrated.is_none());
    unlocked(&identified);
    open(
        &mut game,
        &identified,
        IdentityIntent::RequireExistingIdentity,
    )
    .unwrap();
    let reopened = game
        .integrated
        .as_ref()
        .unwrap()
        .call(|server| server.host_world_identity().unwrap().to_string())
        .unwrap();
    assert_eq!(reopened, world);
    assert!(game.host_world_identity().is_none());
    close(&mut game);
    unlocked(&identified);
    assert_eq!(identity_on_disk(&identified), world);
    let missing = identified
        .parent()
        .unwrap()
        .join("missing-required-identity");
    assert!(open(&mut game, &missing, IdentityIntent::RequireExistingIdentity).is_err());
    assert!(game.integrated.is_none() && game.host_world_identity().is_none());
    assert!(!missing.exists());
}
#[test]
#[ignore = "isolated actual Game resources and owned prebuilt counter pair: public UUID refusal only"]
fn host_identity_startup_retained_context_rejects_candidate_world_uuid() {
    let fixture = Fixture::new("host-id-public-counter-refusal");
    let template = fixture.game();
    let mut game = client(&template, false);
    let identified = root(&game, "counter-refusal");
    open(&mut game, &identified, IdentityIntent::CreateNewIdentity).unwrap();
    let bundle = PathBuf::from(
        std::env::var_os("HORIZON_COUNTER_PAIR_FIXTURE")
            .expect("owned prebuilt Rust pair required"),
    );
    let prepared = horizon_mods::paired::PreparedBundle::read_folder(&bundle).unwrap();
    let type_id = prepared
        .content()
        .machine_types()
        .types()
        .first()
        .unwrap()
        .id();
    let binding =
        NativeBinding::from_prepared(&prepared, type_id, game.pack.assets.registry).unwrap();
    let target = CounterTarget::new(game.dim.id(), [2, 70, 2]).unwrap();
    game.world
        .chunks
        .insert((0, 0), Arc::new(Chunk::empty(0, 0)));
    game.world.set(2, 70, 2, binding.state_id()).unwrap();
    let state = game.pack.assets.registry.state(binding.state_id()).unwrap();
    let session = game.session_lock.as_ref().unwrap();
    let expected = session.expected_world().unwrap();
    let a = WorldId::parse("00000000-0000-4000-8000-000000000001").unwrap();
    let b = WorldId::parse("00000000-0000-4000-8000-000000000002").unwrap();
    let candidate = if *expected == a { b } else { a };
    let frame = CounterFrameV2 {
        format: 2,
        kind: "native-counter-checkpoint".into(),
        world_uuid: candidate,
        dimension: target.dimension.clone(),
        position: [2, 70, 2],
        allocation_high_water: 1,
        package_identity: binding
            .package_identity()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        type_id: binding.type_id().into(),
        save_schema: 1,
        binding: FrameBinding {
            block_id: binding.block_id().into(),
            template: "minecraft:iron_block".into(),
            properties: BTreeMap::new(),
        },
        observed_state: FrameState {
            name: state.name.clone(),
            properties: state.properties.clone(),
        },
        saved_game_time: game.world.game_time,
        placement: Placement::Active {
            generation: 1,
            counter: 0,
            last_advanced: None,
        },
        last_retired: None,
    };
    frame.validate().unwrap();
    let before = disk(&identified);
    let pointer = Arc::as_ptr(game.world.chunks.get(&(0, 0)).unwrap());
    let revision = game.world.revision;
    let clock = (game.world.game_time, game.world.day_time);
    let modified = game.world.modified.clone();
    let mut rng = game.world.random.clone();
    let random = [rng.next_int(), rng.next_int()];
    assert_eq!(
        game.world
            .restore_counter_frame(&binding, *expected, &target, &frame),
        Err(RestoreError::IdentityMismatch)
    );
    assert_eq!(game.world.revision, revision);
    assert_eq!(
        Arc::as_ptr(game.world.chunks.get(&(0, 0)).unwrap()),
        pointer
    );
    assert_eq!((game.world.game_time, game.world.day_time), clock);
    assert_eq!(game.world.modified, modified);
    let mut rng = game.world.random.clone();
    assert_eq!([rng.next_int(), rng.next_int()], random);
    assert!(
        game.world.counter_domain_snapshot().is_none()
            && game.world.counter_trial_snapshot().is_none()
    );
    assert_eq!(session.expected_world(), Some(expected));
    assert_eq!(disk(&identified), before);
    // Native initializer/SDK/archive selection are deliberately absent from this proof.
}
