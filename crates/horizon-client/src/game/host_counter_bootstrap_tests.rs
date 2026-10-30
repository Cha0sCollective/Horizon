//! Explicit new-host bootstrap only; no ordinary command or capture admission.
#![cfg(feature = "mods")]
use super::*;
use crate::game::{
    integrated::transition_ownership_tests::Fixture, world_loading::host_identity::IdentityIntent,
};
use horizon_client::{
    audio::Audio,
    hud::HudAtlas,
    world::counter_restore::{DomainSnapshot, DomainStatus, Placement},
};
use horizon_core::{geometry::Vec3, random::RandomSource};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
const POSITION: [i32; 3] = [2, 120, 2];
const IDENTITY_FILE: &str = "horizon-world-identity.json";
const WAIT: Duration = Duration::from_secs(120);
fn control_files(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    [
        IDENTITY_FILE,
        "horizon-paired-packages.json",
        "level.dat",
        "data/horizon/counter_checkpoint.json",
        "data/horizon/slot_checkpoint.nbt",
    ]
    .into_iter()
    .map(|name| {
        let path = root.join(name);
        let bytes = match fs::symlink_metadata(&path) {
            Ok(meta) => {
                assert!(meta.is_file() && !meta.file_type().is_symlink());
                Some(fs::read(path).unwrap())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("control metadata {name}: {error}"),
        };
        (name.to_owned(), bytes)
    })
    .collect()
}
fn client<'a>(template: &'a Game<'_>, threaded: bool, paired: bool) -> Game<'a> {
    let mut options = template.options.clone();
    options.execution_role = super::super::execution_role::ExecutionRole::Client;
    options.server_thread = threaded;
    options.dedicated = None;
    options.test_terrain = true;
    if paired {
        let path = PathBuf::from(
            std::env::var_os("HORIZON_COUNTER_PAIR_FIXTURE")
                .expect("existing read-only prebuilt counter pair required"),
        );
        options.mod_packages =
            super::super::mod_packages::Packages::read(&BTreeSet::new(), &[path]).unwrap();
        assert!(options.mod_packages.has_pairs());
    }
    let mut settings = template.settings.clone();
    settings.render_distance = 2;
    settings.simulation_distance = 2;
    let gui = super::super::UiResources {
        atlas: HudAtlas::fonts(&options.resources).unwrap(),
        panorama: Arc::new(horizon_client::gfx::panorama::Faces {
            size: 1,
            pixels: [0u8; 4].repeat(6),
        }),
    };
    let audio = Audio::silent(&options.resources);
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
    // The normal captured startup loader, before opening an editable world.
    if paired {
        game.start_mods();
        game.options
            .mod_packages
            .check_running(&game.mods.mods)
            .unwrap();
    }
    game
}
fn root(game: &Game<'_>, name: &str) -> PathBuf {
    game.world_root.parent().unwrap().join(name)
}
fn open(game: &mut Game<'_>, root: &Path, intent: IdentityIntent) {
    game.open_world_with_identity(
        root.to_str().unwrap(),
        Some("Host Counter Bootstrap"),
        Some(17),
        false,
        None,
        intent,
    )
    .unwrap();
}
fn close(game: &mut Game<'_>) {
    game.close_world().unwrap();
    let deadline = Instant::now() + WAIT;
    while game.pending_save.is_some() {
        game.poll_closing_save().unwrap();
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert!(game.session_lock.is_none() && game.host_world_identity().is_none());
}
fn on_server<T: Send + 'static>(
    game: &Game<'_>,
    action: impl FnOnce(&mut Game<'_>) -> T + Send + 'static,
) -> T {
    let (completed, completion) = std::sync::mpsc::sync_channel(1);
    let observer = game.integrated.as_ref().unwrap().call(move |server| {
        let result = action(server);
        let _ = completed.send(result);
    });
    let result = completion.recv_timeout(WAIT).expect(
        "definite actual-server task completion; observer timeout never cancels or retries",
    );
    eprintln!(
        "HOST_BOOTSTRAP_TASK observer_some={} completed=true",
        observer.is_some()
    );
    result
}
fn selected(game: &Game<'_>) -> (String, NativeBinding) {
    let catalog = game.options.mod_packages.catalog().unwrap();
    let mut matches = catalog.prepared().values().flat_map(|prepared| {
        prepared
            .content()
            .machine_types()
            .types()
            .iter()
            .map(move |ty| (prepared, ty.id()))
    });
    let (prepared, type_id) = matches.next().unwrap();
    assert!(matches.next().is_none());
    let binding =
        NativeBinding::from_prepared(prepared, type_id, game.pack.assets.registry).unwrap();
    assert_ne!(binding.block_id(), "minecraft:iron_block");
    assert_eq!(
        game.pack
            .assets
            .registry
            .state(binding.state_id())
            .unwrap()
            .name,
        "minecraft:iron_block"
    );
    (type_id.to_owned(), binding)
}
fn physical_input(game: &mut Game<'_>, state: BlockStateId) {
    let deadline = Instant::now() + WAIT;
    while !game.world.loaded(0, 0) {
        game.update_world();
        if !game.world.loaded(0, 0) {
            game.streaming.wait(Duration::from_millis(2));
        }
        assert!(
            Instant::now() < deadline,
            "actual native target-column residency"
        );
    }
    assert!(
        !game
            .world
            .block_entities
            .contains_key(&(POSITION[0], POSITION[1], POSITION[2]))
    );
    game.world
        .set(POSITION[0], POSITION[1], POSITION[2], state)
        .unwrap();
}
#[derive(Debug, PartialEq)]
struct State {
    world: (i64, i64, u64, [i32; 4]),
    domain: Option<DomainSnapshot>,
    trial: Option<Snapshot>,
    modified: BTreeSet<(i32, i32)>,
    loaded: Vec<((i32, i32), Vec<horizon_world::nbt::Tag>)>,
    chunks: Vec<((i32, i32), usize, Vec<(usize, Vec<u32>, Vec<u8>)>)>,
}
fn state(game: &Game<'_>) -> State {
    let mut random = game.world.random.clone();
    let mut chunks: Vec<_> = game
        .world
        .chunks
        .iter()
        .map(|(at, chunk)| {
            (
                *at,
                Arc::as_ptr(chunk) as usize,
                chunk
                    .sections
                    .iter()
                    .map(|section| {
                        (
                            Arc::as_ptr(section) as usize,
                            (0..4096).map(|i| section.get(i)).collect(),
                            (0..4096).map(|i| section.light(i)).collect(),
                        )
                    })
                    .collect(),
            )
        })
        .collect();
    chunks.sort_by_key(|row| row.0);
    State {
        world: (
            game.world.game_time,
            game.world.day_time,
            game.world.revision,
            [
                random.next_int(),
                random.next_int(),
                random.next_int(),
                random.next_int(),
            ],
        ),
        domain: game.world.counter_domain_snapshot(),
        trial: game.world.counter_trial_snapshot(),
        modified: game.world.modified.iter().copied().collect(),
        loaded: game.world.loaded_entities.clone(),
        chunks,
    }
}
fn refuse(game: &mut Game<'_>, type_id: &str) {
    let before = state(game);
    let files = control_files(&game.world_root);
    assert!(game.bootstrap_host_counter(type_id, POSITION).is_err());
    assert_eq!(state(game), before);
    assert_eq!(control_files(&game.world_root), files);
}
fn initialize(game: &mut Game<'_>, type_id: &str) -> RuntimeCounterKey {
    let world = *game.host_world_identity().unwrap();
    let before = state(game);
    let files = control_files(&game.world_root);
    let key = game.bootstrap_host_counter(type_id, POSITION).unwrap();
    let after = state(game);
    assert_eq!(after.world, before.world);
    assert_eq!(after.chunks, before.chunks);
    assert_eq!(after.loaded, before.loaded);
    assert_eq!(after.modified, before.modified);
    assert_eq!(
        control_files(&game.world_root),
        files,
        "bootstrap does not write identity/level/machine records; native producer writes are separate"
    );
    assert_eq!(key.placement().world, world);
    assert_eq!(key.placement().position, POSITION);
    assert_eq!(key.placement().dimension, game.dim.id());
    assert_eq!(key.placement().generation, 1);
    let record = game.world.read_persistent_counter(&key).unwrap();
    assert_eq!(
        (record.counter(), record.last_advanced(), record.resident()),
        (0, None, true)
    );
    let metadata = game.world.counter_domain_snapshot().unwrap();
    assert_eq!(
        (metadata.world, metadata.high_water, metadata.status),
        (world, 1, DomainStatus::Healthy)
    );
    assert!(matches!(
        game.world
            .counter_trial_snapshot()
            .unwrap()
            .configuration()
            .backend(),
        Backend::Native
    ));
    let frame = game
        .world
        .counter_checkpoint_frame(game.world.game_time)
        .unwrap();
    assert_eq!(frame.world_uuid, world);
    assert_eq!(
        frame.placement,
        Placement::Active {
            generation: 1,
            counter: 0,
            last_advanced: None
        }
    );
    key
}
#[test]
#[ignore = "isolated actual resources and prebuilt pair: initial in-frame host bootstrap"]
fn host_counter_bootstrap_in_frame_retained_uuid_one_use_and_native_binding() {
    let fixture = Fixture::new("host-bootstrap-in-frame");
    let template = fixture.game();
    let mut game = client(&template, false, true);
    let path = root(&game, "initial-identified");
    open(&mut game, &path, IdentityIntent::CreateNewIdentity);
    let (type_id, binding) = selected(&game);
    assert!(game.world.counter_domain_snapshot().is_none());
    physical_input(&mut game, binding.state_id());
    let key = initialize(&mut game, &type_id);
    refuse(&mut game, &type_id);
    assert_eq!(
        game.world.read_persistent_counter(&key).unwrap().counter(),
        0
    );
    let anchor = fs::read(path.join(IDENTITY_FILE)).unwrap();
    close(&mut game);
    open(&mut game, &path, IdentityIntent::RequireExistingIdentity);
    assert_eq!(fs::read(path.join(IDENTITY_FILE)).unwrap(), anchor);
    physical_input(&mut game, binding.state_id());
    refuse(&mut game, &type_id);
    assert!(game.world.read_persistent_counter(&key).is_err());
    assert!(game.world.counter_domain_snapshot().is_none());
    close(&mut game);
    let second = path.parent().unwrap().join("another-new-identity");
    open(&mut game, &second, IdentityIntent::CreateNewIdentity);
    physical_input(&mut game, binding.state_id());
    let fresh = initialize(&mut game, &type_id);
    assert_ne!(fresh.instance(), key.instance());
    assert_ne!(fresh.session_nonce(), key.session_nonce());
    assert_ne!(fresh.placement().world, key.placement().world);
    assert!(game.world.read_persistent_counter(&key).is_err());
    close(&mut game);
}
#[test]
#[ignore = "isolated actual resources and prebuilt pair: Legacy ineligibility and refused-input preservation"]
fn host_counter_bootstrap_legacy_invalid_inputs_and_missing_catalog_preserve_state() {
    let fixture = Fixture::new("host-bootstrap-refusals");
    let template = fixture.game();
    let mut game = client(&template, false, true);
    let legacy = root(&game, "legacy");
    open(&mut game, &legacy, IdentityIntent::Legacy);
    let (type_id, binding) = selected(&game);
    physical_input(&mut game, binding.state_id());
    assert!(game.host_world_identity().is_none());
    refuse(&mut game, &type_id);
    close(&mut game);
    let fresh = legacy.parent().unwrap().join("new-identified");
    open(&mut game, &fresh, IdentityIntent::CreateNewIdentity);
    // No target residency yet: refusal does not bind at first use or spend.
    assert!(!game.world.loaded(0, 0));
    refuse(&mut game, &type_id);
    let stone = game
        .pack
        .assets
        .registry
        .default_state("minecraft:stone")
        .unwrap();
    physical_input(&mut game, stone);
    refuse(&mut game, &type_id);
    let before = state(&game);
    let files = control_files(&game.world_root);
    assert!(
        game.bootstrap_host_counter("missing:type", POSITION)
            .is_err()
    );
    assert!(game.bootstrap_host_counter(&type_id, [2, 500, 2]).is_err());
    assert_eq!(state(&game), before);
    assert_eq!(control_files(&game.world_root), files);
    if let Some(alias) = game.pack.assets.registry.default_state(binding.block_id()) {
        assert_ne!(alias, binding.state_id());
        physical_input(&mut game, alias);
        refuse(&mut game, &type_id);
    }
    physical_input(&mut game, binding.state_id());
    let _key = initialize(&mut game, &type_id);
    close(&mut game);
    let mut unpaired = client(&template, false, false);
    let path = root(&unpaired, "no-catalog");
    open(&mut unpaired, &path, IdentityIntent::CreateNewIdentity);
    physical_input(&mut unpaired, binding.state_id());
    refuse(&mut unpaired, &type_id);
    assert!(unpaired.world.counter_domain_snapshot().is_none());
    close(&mut unpaired);
}
#[test]
#[ignore = "isolated actual resources and prebuilt pair: in-frame replacement cannot rebind a live grant"]
fn host_counter_bootstrap_in_frame_dimension_return_replacement_refuses_unused_grant() {
    let fixture = Fixture::new("host-bootstrap-replacement");
    let template = fixture.game();
    let mut game = client(&template, false, true);
    let path = root(&game, "replacement-identified");
    open(&mut game, &path, IdentityIntent::CreateNewIdentity);
    let (type_id, binding) = selected(&game);
    let world = *game.host_world_identity().unwrap();
    physical_input(&mut game, binding.state_id());
    game.switch_level(super::super::Dim::Nether, Vec3::new(2.0, 120.0, 2.0))
        .unwrap();
    refuse(&mut game, &type_id);
    game.switch_level(super::super::Dim::Overworld, Vec3::new(2.0, 120.0, 2.0))
        .unwrap();
    assert_eq!(*game.host_world_identity().unwrap(), world);
    physical_input(&mut game, binding.state_id());
    refuse(&mut game, &type_id);
    assert!(game.world.counter_domain_snapshot().is_none());
    close(&mut game);
}
#[test]
#[ignore = "isolated actual resources and prebuilt pair: initially non-Overworld identity stays ineligible"]
fn host_counter_bootstrap_initial_non_overworld_cannot_gain_eligibility() {
    let fixture = Fixture::new("host-bootstrap-initial-nether");
    let template = fixture.game();
    let mut game = client(&template, false, true);
    let path = root(&game, "initial-nether");
    game.options.dimension = Some(super::super::Dim::Nether.id().to_owned());
    open(&mut game, &path, IdentityIntent::CreateNewIdentity);
    let (type_id, binding) = selected(&game);
    let world = *game.host_world_identity().unwrap();
    assert_eq!(game.dim, super::super::Dim::Nether);
    refuse(&mut game, &type_id);
    game.switch_level(super::super::Dim::Overworld, Vec3::new(2.0, 120.0, 2.0))
        .unwrap();
    assert_eq!(*game.host_world_identity().unwrap(), world);
    physical_input(&mut game, binding.state_id());
    refuse(&mut game, &type_id);
    assert!(game.world.counter_domain_snapshot().is_none());
    close(&mut game);
}
#[test]
#[ignore = "isolated actual resources and prebuilt pair: real integrated Launch and same-World parking"]
fn host_counter_bootstrap_integrated_launch_parking_preserves_initial_world_and_one_use() {
    let fixture = Fixture::new("host-bootstrap-integrated");
    let template = fixture.game();
    let mut game = client(&template, true, true);
    let path = root(&game, "integrated-identified");
    open(&mut game, &path, IdentityIntent::CreateNewIdentity);
    assert!(
        game.remote.is_some() && game.integrated.is_some() && game.host_world_identity().is_none()
    );
    let result = on_server(&game, |server| {
        assert_eq!(
            server.options.execution_role,
            super::super::execution_role::ExecutionRole::IntegratedServer
        );
        let (type_id, binding) = selected(server);
        let world = *server.host_world_identity().unwrap();
        physical_input(server, binding.state_id());
        server.enter_level(None, super::super::Dim::Nether).unwrap();
        refuse(server, &type_id);
        server
            .enter_level(None, super::super::Dim::Overworld)
            .unwrap();
        assert_eq!(*server.host_world_identity().unwrap(), world);
        let key = initialize(server, &type_id);
        server.enter_level(None, super::super::Dim::Nether).unwrap();
        server
            .enter_level(None, super::super::Dim::Overworld)
            .unwrap();
        assert_eq!(
            server
                .world
                .read_persistent_counter(&key)
                .unwrap()
                .counter(),
            0
        );
        refuse(server, &type_id);
        (world.to_string(), key.instance(), key.session_nonce())
    });
    let anchor: serde_json::Value =
        serde_json::from_slice(&fs::read(path.join(IDENTITY_FILE)).unwrap()).unwrap();
    assert_eq!(anchor["world_uuid"].as_str().unwrap(), result.0);
    close(&mut game);
    open(&mut game, &path, IdentityIntent::RequireExistingIdentity);
    on_server(&game, |server| {
        let (type_id, binding) = selected(server);
        physical_input(server, binding.state_id());
        refuse(server, &type_id);
        assert!(server.world.counter_domain_snapshot().is_none());
    });
    close(&mut game);
}
