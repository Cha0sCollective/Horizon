//! Defining source-level controls; resource cases use actual native providers,
//! actual Game/guest owners and the coordinator's prebuilt package, never a build.
use super::*;
use crate::game::{
    execution_role::ExecutionRole,
    integrated::transition_ownership_tests::Fixture,
    lan::{LanSettings, Owner},
    slot_player::TransferOutcome,
    world_loading::host_identity::IdentityIntent,
};
use horizon_client::{
    items::ItemStack,
    lan::{Joined, NetConfig, Out, PlayerSender, guest::Guest, hub::HubConfig},
    motion::PlayerMotion,
    slot_transfer::Direction,
    streaming::{NativeWorkCounts, NativeWorkOrigin},
    ticket_levels::Sources,
};
use horizon_core::geometry::Vec3;
use horizon_protocol::packets::configuration::ClientInformation;
use std::{
    sync::{
        atomic::{AtomicI32, AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};
const WAIT: Duration = Duration::from_secs(120);
const POSITION: [i32; 3] = [2, 120, 2];
const OWNER: u128 = 0x55;
const EXTERNAL: u128 = 0x66;
#[path = "session_checkpoint_registered_tests.rs"]
mod registered;

#[test]
#[ignore = "isolated pinned resources + prebuilt pair: managed new Game holds UUID authority"]
fn session_authority_actual_new_game_retains_authority_and_refuses_native_only_reopen() {
    use crate::game::world_loading::host_identity::session_authority::AuthorityNamespace;
    let fixture = Fixture::new("authority-new");
    let _retain = RetainOnPanic::new(&fixture);
    let scratch = Scratch::new("authority-new");
    let ns_root = scratch.0.join("authority");
    fs::create_dir(&ns_root).unwrap();
    let namespace = AuthorityNamespace::new(&ns_root).unwrap();
    let root = scratch.0.join("new-world");
    let mut game = fresh_with_role(&fixture, ExecutionRole::DedicatedServer);
    let mut request = WorldRequest::new("unused", None, Some(17), false, None)
        .unwrap()
        .with_identity_intent(IdentityIntent::CreateNewIdentity)
        .with_session_authority(namespace.clone());
    request.save_root = root.clone();
    let prepared = request
        .prepare(&game.res, &game.pack, &game.options, &game.settings)
        .unwrap();
    assert!(prepared.session_lock.has_authority());
    game.finish_open_world(prepared).unwrap();
    assert!(game.session_lock.as_ref().unwrap().has_authority());
    let anchor = scratch.0.join("copied-anchor");
    fs::create_dir(&anchor).unwrap();
    let identity = fs::read(root.join("horizon-world-identity.json")).unwrap();
    fs::write(anchor.join("horizon-world-identity.json"), &identity).unwrap();
    let archive = scratch.0.join("archive");
    fs::create_dir(&archive).unwrap();
    assert!(
        namespace
            .acquire_recovery(&anchor, &archive, &scratch.0.join("copy"))
            .is_err()
    );
    assert!(!anchor.join("level.dat").exists());
    let mut direct = WorldRequest::new("unused", None, None, false, None)
        .unwrap()
        .with_identity_intent(IdentityIntent::RequireExistingIdentity)
        .with_session_authority(namespace.clone());
    direct.save_root = anchor.clone();
    assert!(
        direct
            .prepare(&game.res, &game.pack, &game.options, &game.settings)
            .is_err()
    );
    assert_eq!(
        fs::read(anchor.join("horizon-world-identity.json")).unwrap(),
        identity
    );
    drop(game);
    let lease = namespace
        .acquire_recovery(&anchor, &archive, &scratch.0.join("copy"))
        .unwrap();
    assert_eq!(
        lease.world().to_string(),
        serde_json::from_slice::<Value>(&identity).unwrap()["world_uuid"]
            .as_str()
            .unwrap()
    );
}

#[test]
#[ignore = "isolated pinned resources + prebuilt pair: managed positive-owner recovery excludes copied worlds and releases on failure"]
fn session_authority_actual_recovered_game_excludes_copies_and_failed_open_releases_lease() {
    use crate::game::world_loading::host_identity::session_authority::AuthorityNamespace;
    let fixture = Fixture::new("authority-recovery");
    let _retain = RetainOnPanic::new(&fixture);
    let mut source = authority_source(&fixture);
    let archive = source.archive.clone();
    source
        .game
        .checkpoint_session(&archive, 1, until())
        .unwrap();
    let scratch = Scratch::new("authority-recovery");
    let ns_root = scratch.0.join("authority");
    fs::create_dir(&ns_root).unwrap();
    let namespace = AuthorityNamespace::new(&ns_root).unwrap();
    let owner_root = source.game.world_root.clone();
    let copied_anchor = scratch.0.join("copied-anchor");
    fs::create_dir(&copied_anchor).unwrap();
    let identity = fs::read(owner_root.join("horizon-world-identity.json")).unwrap();
    fs::write(copied_anchor.join("horizon-world-identity.json"), &identity).unwrap();
    let image_before = tree(&archive, &[]).unwrap();
    let target = source.target.clone();
    let ty = source.type_id.clone();
    let make_request = |owner: &Path, destination: &str, type_id: &str| ManagedRecoveryRequest {
        owner_root: owner.to_owned(),
        expected_target: target.clone(),
        expected_type: type_id.to_owned(),
        archive_root: archive.clone(),
        destination: scratch.0.join(destination),
        deadline: until(),
    };
    let open = |request| authority_open(&fixture, &namespace, request);
    // The selected owner's REAL native lock prevents opening while it is live.
    assert!(
        open(make_request(&owner_root, "live-refusal", &ty))
            .0
            .is_err()
    );
    assert!(!scratch.0.join("live-refusal").exists());
    drop(source);
    // Recovery must use the archive even if the selected native level is damaged.
    fs::write(
        owner_root.join("level.dat"),
        b"damaged native-only successor",
    )
    .unwrap();
    let ((mut recovered, receipt), receivers) = {
        let (result, receivers) = open(make_request(&owner_root, "first", &ty));
        (result.unwrap(), receivers)
    };
    assert_eq!(receipt.epoch, 1);
    assert_eq!(total(&recovered, &receipt.slot), 14);
    assert!(recovered.session_lock.as_ref().unwrap().has_authority());
    assert!(
        open(make_request(&copied_anchor, "concurrent-copy", &ty))
            .0
            .is_err()
    );
    assert!(!scratch.0.join("concurrent-copy").exists());
    assert_eq!(tree(&archive, &[]).unwrap(), image_before);
    recovered.world.game_time += 1;
    recovered.tick_counter_trial(true);
    assert_eq!(
        recovered
            .world
            .read_persistent_counter(&receipt.counter)
            .unwrap()
            .counter(),
        4
    );
    drop(recovered);
    drop(receivers);
    // Full image/type validation failure returns no Game and releases both locks.
    assert!(
        open(make_request(
            &copied_anchor,
            "failed-type",
            "invalid:binding"
        ))
        .0
        .is_err()
    );
    assert!(!scratch.0.join("failed-type").exists());
    let (second, receivers) = open(make_request(&copied_anchor, "second", &ty));
    let (second, receipt) = second.unwrap();
    assert!(second.session_lock.as_ref().unwrap().has_authority());
    assert_eq!(total(&second, &receipt.slot), 14);
    assert_eq!(tree(&archive, &[]).unwrap(), image_before);
    assert_eq!(
        fs::read(owner_root.join("level.dat")).unwrap(),
        b"damaged native-only successor"
    );
    drop(second);
    drop(receivers);
}

// Game is large. Keep independent live owners on the heap in this multi-Game
// fixture instead of reserving many debug return-value slots in one test frame.
// No thread-stack override or production recovery ownership change is involved.
#[inline(never)]
fn authority_source(fixture: &Fixture) -> Box<Session<'_>> {
    Box::new(session(fixture, "authority-recovery", 2, true))
}
#[inline(never)]
fn authority_open<'a>(
    fixture: &'a Fixture,
    namespace: &crate::game::world_loading::host_identity::session_authority::AuthorityNamespace,
    request: ManagedRecoveryRequest,
) -> (
    SResult<(Box<Game<'a>>, RecoveryReceipt)>,
    Vec<mpsc::Receiver<Out>>,
) {
    let mut candidate = fresh_with_role(fixture, ExecutionRole::IntegratedServer);
    let (host, receivers) = make_host(&mut candidate, true, [110, 220], false);
    let result = candidate
        .recover_managed_session_checkpoint(namespace, request, host)
        .map(|(game, receipt)| (Box::new(game), receipt));
    (result, receivers)
}

fn until() -> Instant {
    Instant::now() + WAIT
}
fn retain_log(message: fmt::Arguments<'_>) {
    let _ = std::io::Write::write_fmt(&mut std::io::stderr(), message);
}
struct Scratch(PathBuf);
impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "horizon-session-{}-{}-{label}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        if std::thread::panicking() {
            retain_log(format_args!(
                "SESSION_SCRATCH_RETAINED {}\n",
                self.0.display()
            ));
            return;
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}
/// Paths only: every Game declared after this outer test guard drops first.
/// The old Fixture then cleans its original root, which no longer holds evidence.
struct RetainOnPanic {
    source: PathBuf,
    container: PathBuf,
}
impl RetainOnPanic {
    fn new(fixture: &Fixture) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let context = fixture.game();
        let source = context.world_root.parent().unwrap().to_owned();
        drop(context);
        let name = source.file_name().unwrap().to_str().unwrap();
        assert!(source.is_absolute() && name.starts_with("horizon-transition-"));
        let container = source.with_file_name(format!(
            "{name}-retained-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        assert_eq!(source.parent(), container.parent());
        assert_ne!(source, container);
        Self { source, container }
    }
}
impl Drop for RetainOnPanic {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        match fs::symlink_metadata(&self.source) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                retain_log(format_args!(
                    "SESSION_RETAIN_FAILED non-directory {}\n",
                    self.source.display()
                ));
                return;
            }
            Err(error) => {
                retain_log(format_args!(
                    "SESSION_RETAIN_FAILED {}: {error}\n",
                    self.source.display()
                ));
                return;
            }
        }
        // An exclusive sibling container prevents replacing existing evidence.
        // No Game/SaveDir reference is retained and no save or flush runs here.
        if let Err(error) = fs::create_dir(&self.container) {
            retain_log(format_args!(
                "SESSION_RETAIN_FAILED {}: {error}\n",
                self.container.display()
            ));
            return;
        }
        let destination = self.container.join("fixture");
        match fs::rename(&self.source, &destination) {
            Ok(()) => retain_log(format_args!(
                "SESSION_FIXTURE_RETAINED {}\n",
                destination.display()
            )),
            Err(error) => retain_log(format_args!(
                "SESSION_RETAIN_FAILED {} -> {}: {error}\n",
                self.source.display(),
                destination.display()
            )),
        }
    }
}
fn region() -> Vec<u8> {
    let mut bytes = vec![0; 3 * 4096];
    bytes[..4].copy_from_slice(&0x201u32.to_be_bytes());
    bytes[8192..8196].copy_from_slice(&1u32.to_be_bytes());
    bytes[8196] = 3;
    bytes
}
#[test]
fn session_raw_region_preflight_checks_every_off_resident_location_before_native_open() {
    let good = region();
    assert_eq!(raw_region(&good).unwrap(), vec![0]);
    for (label, bytes) in [
        ("truncated-header", vec![0; 4096]),
        ("header-sector", {
            let mut b = good.clone();
            b[4000..4004].copy_from_slice(&0x101u32.to_be_bytes());
            b
        }),
        ("zero-sector-count", {
            let mut b = good.clone();
            b[4000..4004].copy_from_slice(&0x200u32.to_be_bytes());
            b
        }),
        ("out-of-bounds", {
            let mut b = good.clone();
            b[4000..4004].copy_from_slice(&0x301u32.to_be_bytes());
            b
        }),
        ("overlap", {
            let mut b = good.clone();
            b[4000..4004].copy_from_slice(&0x201u32.to_be_bytes());
            b
        }),
        ("external-flag", {
            let mut b = good.clone();
            b[8196] = 0x83;
            b
        }),
        ("payload-beyond-sector", {
            let mut b = good.clone();
            b[8192..8196].copy_from_slice(&4093u32.to_be_bytes());
            b
        }),
    ] {
        assert!(raw_region(&bytes).is_err(), "{label}");
    }
    // An unused location remains distinct from any malformed nonzero location.
    assert_eq!(raw_region(&vec![0; 8192]).unwrap(), Vec::<usize>::new());
}
#[test]
fn session_structured_decoders_refuse_duplicates_coercions_nonfinite_and_accept_signed_zero() {
    assert!(json::<Value>(br#"{"x":1,"x":2}"#, STRUCTURED_LIMIT).is_err());
    assert!(json::<Value>(b"1 2", STRUCTURED_LIMIT).is_err());
    assert!(json::<Value>(&vec![b' '; STRUCTURED_LIMIT + 1], STRUCTURED_LIMIT).is_err());
    let duplicate = [
        10, 0, 0, 3, 0, 1, b'x', 0, 0, 0, 1, 3, 0, 1, b'x', 0, 0, 0, 2, 0,
    ];
    assert!(raw_nbt(&duplicate).is_err());
    for bits in [
        f64::NAN.to_bits(),
        f64::INFINITY.to_bits(),
        f64::NEG_INFINITY.to_bits(),
    ] {
        assert!(nbt_bytes(&Tag::compound([("v", Tag::Double(bits))])).is_err());
    }
    let tag = Tag::compound([
        ("v", Tag::Double((-0.0f64).to_bits())),
        ("f", Tag::Float((-0.0f32).to_bits())),
    ]);
    let decoded = compressed_nbt(&nbt_bytes(&tag).unwrap()).unwrap();
    assert!(lan::session_player_tags_equal(&tag, &decoded));
    assert_ne!(tag, decoded);
}
#[test]
fn session_inventory_copy_is_physical_bounded_exact_and_preserves_failed_pending_evidence() {
    let scratch = Scratch::new("physical");
    let source = scratch.0.join("source");
    fs::create_dir(&source).unwrap();
    fs::create_dir(source.join("empty")).unwrap();
    fs::write(source.join("a"), b"previous").unwrap();
    let snapshot = tree(&source, &[]).unwrap();
    let copy = scratch.0.join("copy");
    copy_tree(&source, &copy, &snapshot, until()).unwrap();
    assert_eq!(tree(&copy, &[]).unwrap(), snapshot);
    fs::write(source.join("a"), b"changed").unwrap();
    assert_eq!(fs::read(copy.join("a")).unwrap(), b"previous");
    assert!(copy_tree(&source, &scratch.0.join("failed"), &snapshot, until()).is_err());
    assert!(scratch.0.join("failed").is_dir());
    assert_eq!(fs::read(copy.join("a")).unwrap(), b"previous");
    for path in ["../x", "a//b", "a/../b", "/absolute", "a\\b", "a:b"] {
        assert!(relative(path).is_err(), "{path}");
    }
}
#[test]
fn session_epoch_selection_does_not_skip_corrupt_complete_successors_or_reuse_pending_epochs() {
    let scratch = Scratch::new("epochs");
    fs::create_dir(scratch.0.join("epoch-00000000000000000001")).unwrap();
    fs::create_dir(scratch.0.join("epoch-00000000000000000002")).unwrap();
    fs::create_dir(scratch.0.join(".pending-00000000000000000003")).unwrap();
    assert_eq!(epochs(&scratch.0).unwrap().pop().unwrap().0, 2);
    fs::write(
        scratch.0.join("epoch-00000000000000000002/manifest.json"),
        b"{",
    )
    .unwrap();
    assert_eq!(epochs(&scratch.0).unwrap().pop().unwrap().0, 2);
    fs::create_dir(scratch.0.join("epoch-3")).unwrap();
    assert!(epochs(&scratch.0).is_err());
}
fn fresh(fixture: &Fixture) -> Game<'_> {
    fresh_with_role(fixture, ExecutionRole::IntegratedServer)
}
fn fresh_with_role(fixture: &Fixture, role: ExecutionRole) -> Game<'_> {
    // The existing fixture supplies only cloned pinned resources/options here.
    // The candidate is a genuinely fresh headless Game constructor, with no
    // discarded/recreated World or cleared player/item ownership.
    let template = fixture.game();
    static RESOURCES: std::sync::OnceLock<Arc<crate::game::Resources>> = std::sync::OnceLock::new();
    let resources = RESOURCES.get_or_init(|| template.res.clone());
    assert!(Arc::ptr_eq(resources, &template.res));
    let mut options = template.options.clone();
    options.test_terrain = true;
    options.server_thread = false;
    options.execution_role = role;
    if role.is_client() {
        options.dedicated = None;
    }
    let pair = PathBuf::from(
        std::env::var_os("HORIZON_COUNTER_PAIR_FIXTURE")
            .expect("prebuilt pair fixture required; no nested build"),
    );
    options.mod_packages =
        crate::game::mod_packages::Packages::read(&BTreeSet::new(), &[pair]).unwrap();
    assert!(options.mod_packages.has_pairs());
    let mut settings = template.settings.clone();
    settings.render_distance = 2;
    settings.simulation_distance = 2;
    let pack = template.pack.clone();
    drop(template);
    fresh_with_resources(resources, pack, options, settings)
}
fn fresh_with_resources<'a>(
    resources: &'a Arc<crate::game::Resources>,
    pack: crate::game::PackResources,
    options: crate::game::Options,
    settings: horizon_client::settings::Settings,
) -> Game<'a> {
    let gui = crate::game::UiResources {
        atlas: horizon_client::hud::HudAtlas::fonts(&options.resources).unwrap(),
        panorama: Arc::new(horizon_client::gfx::panorama::Faces {
            size: 1,
            pixels: [0u8; 4].repeat(6),
        }),
    };
    let audio = horizon_client::audio::Audio::silent(&options.resources);
    let mut game = Game::new(resources, pack, gui, options, settings, audio, None).unwrap();
    game.start_mods();
    game.session_running_packages().unwrap();
    let _ = game.advancement_tree();
    game
}
fn open_native(game: &mut Game<'_>, root: &Path) {
    let mut request =
        WorldRequest::new("unused", Some("Session checkpoint"), Some(17), false, None)
            .unwrap()
            .with_identity_intent(IdentityIntent::CreateNewIdentity);
    request.save_root = root.to_owned();
    let prepared = request
        .prepare(&game.res, &game.pack, &game.options, &game.settings)
        .unwrap();
    game.finish_open_world(prepared).unwrap();
    assert!(game.native_checkpoint_origin.is_some());
    assert!(
        game.session_lock
            .as_ref()
            .unwrap()
            .expected_world()
            .is_some()
    );
    let _ = game.advancement_tree();
}
fn all_requested(radius: i32, center: (i32, i32)) -> BTreeSet<(i32, i32)> {
    let mut result = BTreeSet::new();
    for dz in -(radius + 2)..=radius + 2 {
        for dx in -(radius + 2)..=radius + 2 {
            if dx * dx + dz * dz <= (radius + 1).pow(2) {
                result.insert((center.0 + dx, center.1 + dz));
            }
        }
    }
    result
}
fn native_counts(game: &Game<'_>, origin: NativeWorkOrigin) -> NativeWorkCounts {
    game.streaming
        .observe_native_work()
        .origins
        .into_iter()
        .find(|(candidate, _)| *candidate == origin)
        .unwrap()
        .1
}
fn update_native(game: &mut Game<'_>) {
    let (_, unloaded) = game.streaming.update(&mut game.world);
    if !unloaded.is_empty() {
        // Match Game's native unload path: retain actual returned columns and
        // attach their current native owners/metadata before releasing them.
        let snapshots = game.with_entities(&unloaded);
        if let Err(error) =
            game.save
                .save_later(&snapshots, game.pack.assets.registry, game.res.biomes())
        {
            panic!(
                "native unloaded snapshot save: {error}; work={:#?}",
                game.streaming.observe_native_work()
            );
        }
        for chunk in &unloaded {
            game.world.modified.remove(&(chunk.x, chunk.z));
        }
    }
}
/// Submit ordinary native Finish work, then change its range before the next
/// result reconciliation. The immutable work origin and fresh payload are real.
fn submit_far_finish(game: &mut Game<'_>, far: (i32, i32), preserve_original: bool) {
    let original_center = game.streaming.center;
    let original_radius = game.streaming.radius;
    let original_extra = game.streaming.extra.clone();
    let before = native_counts(game, NativeWorkOrigin::Finish);
    assert!(
        before.queued == 0
            && before.worker_held == 0
            && before.returned == 0
            && before.reconciling == 0,
        "far excursion starts after prior Finish ownership settled: {before:#?}"
    );
    if preserve_original {
        let coverage = required_coverage(&BTreeSet::from([original_center])).unwrap();
        assert!(
            coverage.iter().all(|at| game.world.chunks.contains_key(at)),
            "original native area must already be resident; work={:#?}",
            game.streaming.observe_native_work()
        );
        game.streaming.extra.push((original_center, 0));
    }
    game.streaming.center = far;
    game.streaming.radius = 0;
    let deadline = until();
    loop {
        update_native(game);
        if native_counts(game, NativeWorkOrigin::Finish).submitted > before.submitted {
            // update drains results before submitting new Finish jobs. Do not
            // call it again until the far completion is outside the kept range.
            game.streaming.center = original_center;
            game.streaming.radius = original_radius;
            game.streaming.extra = original_extra;
            // Retain a genuine returned Finish before a short writer-timeout
            // fence. wait only defers payloads; it neither schedules work nor
            // invokes the callback or activates a chunk in World.
            while native_counts(game, NativeWorkOrigin::Finish).returned == 0 {
                assert!(
                    Instant::now() < deadline,
                    "real far Finish return; work={:#?}",
                    game.streaming.observe_native_work()
                );
                game.streaming.wait(Duration::from_millis(1));
            }
            return;
        }
        assert!(
            Instant::now() < deadline,
            "real far Finish submission; work={:#?}",
            game.streaming.observe_native_work()
        );
        std::thread::yield_now();
    }
}
fn native_fence(game: &mut Game<'_>, deadline: Instant) {
    if let Err(error) = game.session_fence(deadline) {
        panic!(
            "native fixture fence: {error}; work={:#?}",
            game.streaming.observe_native_work()
        );
    }
}
fn residency(game: &mut Game<'_>, radius: i32, center: (i32, i32)) {
    game.streaming.center = center;
    game.streaming.radius = radius;
    if native_counts(game, NativeWorkOrigin::SaveFinishedCallback).accepted_storage_handoffs == 0 {
        assert!(
            game.world.chunks.is_empty(),
            "initial callback excursion precedes residency/owners"
        );
        submit_far_finish(game, (-80, -80), false);
        native_fence(game, until());
        let path = game.save.root.join("region/r.-3.-3.mca");
        let bytes = read(&path, FILE_LIMIT).unwrap_or_else(|error| {
            panic!(
                "actual finished native region: {error}; work={:#?}",
                game.streaming.observe_native_work()
            )
        });
        assert!(
            !raw_region(&bytes).unwrap().is_empty(),
            "completed native callback must write real region records; work={:#?}",
            game.streaming.observe_native_work()
        );
    }
    let required = all_requested(radius, center);
    let deadline = until();
    loop {
        update_native(game);
        if required.iter().all(|at| game.world.chunks.contains_key(at))
            && game.world.chunks.get(&(0, 0)).is_none_or(|chunk| chunk.lit)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "genuine native full residency; work={:#?}",
            game.streaming.observe_native_work()
        );
        std::thread::yield_now();
    }
    native_fence(game, deadline);
    let work = game.streaming.observe_native_work();
    assert!(
        !work.accounting_fault
            && !work.receipt_mismatch
            && !work.worker_initialization_fault
            && !work.worker_exit_fault,
        "native worker ownership fault; work={work:#?}"
    );
    assert!(
        native_counts(game, NativeWorkOrigin::SaveFinishedCallback).accepted_storage_handoffs > 0,
        "actual nonzero native finished storage callbacks; work={work:#?}"
    );
    for (origin, row) in work.origins {
        assert_eq!(
            row.submitted,
            row.queued
                + row.worker_held
                + row.returned
                + row.reconciling
                + row.disposed
                + row.unresolved,
            "native origin conservation {origin:?}: {row:#?}"
        );
        assert_eq!(row.unresolved, 0, "native origin {origin:?}: {row:#?}");
        assert_eq!(row.storage_errors, 0, "native origin {origin:?}: {row:#?}");
    }
}
fn actual_guest(
    game: &Game<'_>,
    conn: u32,
    uuid: u128,
    body: bool,
) -> (Guest, mpsc::Receiver<Out>) {
    let (tx, rx) = mpsc::channel();
    let mut guest = Guest::new(
        Joined {
            conn,
            uuid,
            name: format!("Checkpoint{uuid}"),
            properties: Vec::new(),
            info: ClientInformation::new(2),
            brand: "owned native session fixture".into(),
            sender: PlayerSender::new(tx, Arc::default()),
            latency: Arc::new(AtomicI32::new(0)),
            address: "127.0.0.1:25565".parse().unwrap(),
        },
        conn as i32,
        [8.0, 121.0, 8.0],
        2,
    );
    guest.placed = true;
    guest.loaded = true;
    guest.dimension = Dim::Overworld.id().into();
    if body {
        let mut motion = PlayerMotion::new(game.res.motion(), Vec3::new(8.0, 121.0, 8.0)).unwrap();
        motion.velocity = Vec3::new(0.125, -0.25, 0.375);
        motion.health.food.exhaustion = 2.5;
        motion.health.food.timer = 91;
        motion.server_copy = true;
        guest.on_ground = motion.ground.on_ground;
        guest.motion = Some(Box::new(motion));
    }
    (guest, rx)
}
fn make_host(
    game: &mut Game<'_>,
    owner: bool,
    conns: [u32; 2],
    body: bool,
) -> (LanHost, Vec<mpsc::Receiver<Out>>) {
    let config = HubConfig {
        default_game_mode: 0,
        guest_commands: false,
        force_game_mode: false,
        hardcore: false,
        view_distance: 2,
        simulation_distance: 2,
        dimension: Dim::Overworld.id().into(),
        seed: 17,
        max_players: 4,
        dedicated: !owner,
        integrated: owner,
    };
    let net = NetConfig {
        listen: false,
        announce: false,
        online: None,
        ..NetConfig::lan(0, "native session fixture".into(), 4)
    };
    game.lan_host(
        LanSettings {
            port: 0,
            guest_commands: false,
            force_game_mode: false,
        },
        net,
        config,
        owner.then_some(OWNER),
    )
    .unwrap();
    let mut host = game.lan.take().unwrap();
    if owner {
        host.owner = Some(Owner {
            uuid: OWNER,
            saved: None,
        });
    }
    let (a, ar) = actual_guest(game, conns[0], OWNER, body);
    let (b, br) = actual_guest(game, conns[1], EXTERNAL, body);
    host.hub.guests.extend([a, b]);
    if body {
        for conn in conns {
            host.play.insert(conn, Default::default());
        }
    }
    (host, vec![ar, br])
}
struct Session<'a> {
    game: Game<'a>,
    counter: RuntimeCounterKey,
    slot: SlotKey,
    archive: PathBuf,
    type_id: String,
    target: CounterTarget,
    receivers: Vec<mpsc::Receiver<Out>>,
}

/// Genuine healthy source for the dormant owned close tests. These helpers keep
/// every populated participant and its actual receiver alive across the handoff.
#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
pub(in crate::game) struct CoherentCloseFixture<'a> {
    pub game: Box<Game<'a>>,
    pub counter: RuntimeCounterKey,
    pub slot: SlotKey,
    pub archive: PathBuf,
    pub type_id: String,
    pub target: CounterTarget,
    pub receivers: Vec<mpsc::Receiver<Out>>,
}
#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
impl CoherentCloseFixture<'_> {
    pub(in crate::game) fn recovery_request(&self, destination: PathBuf) -> RecoveryRequest {
        RecoveryRequest {
            expected_world: self.counter.placement().world,
            expected_target: self.target.clone(),
            expected_type: self.type_id.clone(),
            archive_root: self.archive.clone(),
            destination,
            deadline: until(),
        }
    }
}
#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
pub(in crate::game) fn coherent_close_fixture<'a>(
    fixture: &'a Fixture,
    label: &str,
) -> CoherentCloseFixture<'a> {
    let mut source = session(fixture, label, 2, true);
    source.game.lan.as_mut().unwrap().hub.guests[0].inventory[1] = Some(session_items_rich());
    source.game.spawn_item(
        session_items_rich(),
        [3.5, 121.0, 3.5],
        [0.125, 0.0, 0.0],
        17,
    );
    source.game.save_game_with(false).unwrap();
    source.game.session_fence(until()).unwrap();
    assert_eq!(
        source
            .game
            .world
            .read_persistent_counter(&source.counter)
            .unwrap()
            .counter(),
        3
    );
    assert!(!source.game.dropped.is_empty());
    assert!(!source.game.lan.as_ref().unwrap().hub.guests.is_empty());
    CoherentCloseFixture {
        game: Box::new(source.game),
        counter: source.counter,
        slot: source.slot,
        archive: source.archive,
        type_id: source.type_id,
        target: source.target,
        receivers: source.receivers,
    }
}
#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
pub(in crate::game) fn recover_coherent_close_fixture<'a>(
    fixture: &'a Fixture,
    request: RecoveryRequest,
) -> (Game<'a>, RecoveryReceipt, Vec<mpsc::Receiver<Out>>) {
    let mut candidate = fresh_with_role(fixture, ExecutionRole::IntegratedServer);
    let (host, receivers) = make_host(&mut candidate, true, [110, 220], false);
    let (game, receipt) = candidate.recover_session_checkpoint(request, host).unwrap();
    (game, receipt, receivers)
}
#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
pub(in crate::game) fn coherent_close_item_total(game: &Game<'_>, slot: &SlotKey) -> u64 {
    session_items_total(game, slot)
}

#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
fn cold_retained_game<'a>(fixture: &'a Fixture, label: &str) -> Game<'a> {
    use crate::game::world_loading::{
        PreparationCloseStatus, PreparedOutcome, RetainedWorldLoader,
    };
    let mut game = fresh_with_role(fixture, ExecutionRole::IntegratedServer);
    game.fresh_session_candidate()
        .expect("genuinely cold title-idle fixture Game");
    let root = fixture
        .game()
        .world_root
        .parent()
        .unwrap()
        .join(format!("retained-native-{label}"));
    let mut request = WorldRequest::new(
        "unused",
        Some("Retained close fixture"),
        Some(17),
        false,
        None,
    )
    .unwrap()
    .with_identity_intent(IdentityIntent::CreateNewIdentity);
    request.save_root = root;
    let mut loader = RetainedWorldLoader::start(
        request,
        game.res.clone(),
        game.pack.clone(),
        game.options.clone(),
        game.settings.clone(),
    )
    .unwrap();
    let outcome = loop {
        if let Some(outcome) = loader.poll_retained_outcome() {
            break outcome.expect("actual retained fixture worker returned");
        }
        std::thread::yield_now();
    };
    drop(loader); // Its actual handle was harvested, never detached.
    let mut refused = match outcome {
        PreparedOutcome::Ready(ready) => match ready.into_cold_prepared_for_test() {
            Ok(prepared) => {
                let save = prepared.save.clone();
                let world = prepared.session_lock.expected_world().copied();
                game.finish_open_world(prepared).unwrap();
                assert!(Arc::ptr_eq(&save, &game.save));
                assert_eq!(
                    game.session_lock
                        .as_ref()
                        .and_then(|s| s.expected_world())
                        .copied(),
                    world
                );
                let _ = game.advancement_tree();
                return game;
            }
            Err(ready) => PreparedOutcome::Ready(ready),
        },
        failed => failed,
    };
    // Any fixture construction refusal follows its original explicit close path
    // before reporting failure. No armed preparation is discarded to pass setup.
    refused.begin_close(until());
    loop {
        match refused.poll_close(&game) {
            PreparationCloseStatus::Pending => std::thread::yield_now(),
            PreparationCloseStatus::Complete => break,
            PreparationCloseStatus::Blocked(error) => {
                panic!("retained fixture preparation close blocked: {error}")
            }
        }
    }
    assert!(refused.finish_close().is_ok());
    panic!("genuine retained fixture preparation refused")
}

#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
pub(in crate::game) fn retained_native_close_game<'a>(
    fixture: &'a Fixture,
    label: &str,
) -> Box<Game<'a>> {
    Box::new(cold_retained_game(fixture, label))
}
#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
#[derive(Clone, Copy, Debug)]
pub(in crate::game) enum RetainedCloseFixtureSetup {
    Healthy,
    MalformedOldLevelBackup,
}
#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
pub(in crate::game) struct RetainedCloseFixtureObservation {
    pub counter: Result<u64, String>,
    pub items: Result<u64, String>,
    pub players: Result<Vec<SessionPlayerRow>, String>,
    pub dimension: Dim,
    pub level_uuid: Option<[i32; 4]>,
    pub uuids: BTreeMap<u32, [i32; 4]>,
    pub raw: BTreeMap<u32, Tag>,
    pub clocks: (i64, i64),
    pub random: u64,
}
#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
pub(in crate::game) struct RetainedCloseFixtureToken {
    save: Arc<save::SaveDir>,
    shared: Arc<horizon_client::lan::Shared>,
    world: WorldId,
    root: PathBuf,
    counter: RuntimeCounterKey,
    slot: SlotKey,
    archive: PathBuf,
    target: CounterTarget,
    type_id: String,
}
#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
impl RetainedCloseFixtureToken {
    pub(in crate::game) fn matches(&self, game: &Game<'_>) -> bool {
        Arc::ptr_eq(&self.save, &game.save)
            && self.root == game.world_root
            && game.session_lock.as_ref().and_then(|s| s.expected_world()) == Some(&self.world)
            && game.lan.as_ref().is_some_and(|host| {
                Arc::ptr_eq(&self.shared, &host.hub.lan.shared)
                    && !host.hub.lan.shared.config.listen
                    && !host.hub.lan.shared.config.announce
                    && host.hub.lan.shared.config.online.is_none()
            })
    }
    pub(in crate::game) fn observe(&self, game: &Game<'_>) -> RetainedCloseFixtureObservation {
        let counter = game
            .world
            .read_persistent_counter(&self.counter)
            .map(|row| row.counter())
            .map_err(|e| e.to_string());
        let items = game
            .world
            .read_world_slot(&self.slot)
            .map(|row| row.slot.stack().map_or(0, session_items_stack_count))
            .map_err(|e| format!("{e:?}"))
            .and_then(|machine| {
                game.lan
                    .as_ref()
                    .map(|host| {
                        machine
                            + game
                                .dropped
                                .iter()
                                .map(|e| session_items_stack_count(&e.stack))
                                .sum::<u64>()
                            + host
                                .hub
                                .guests
                                .iter()
                                .flat_map(|g| {
                                    g.inventory
                                        .iter()
                                        .chain(&g.armor)
                                        .chain(std::iter::once(&g.offhand))
                                        .chain(&g.ender_items)
                                        .flatten()
                                })
                                .map(session_items_stack_count)
                                .sum::<u64>()
                    })
                    .ok_or_else(|| "original fixture participants absent".into())
            });
        let players = game
            .observe_checkpoint_participants()
            .map_err(|e| format!("{e:?}"))
            .and_then(|roster| {
                game.project_session_participant_rows(&roster)
                    .map_err(|e| e.to_string())
            });
        RetainedCloseFixtureObservation {
            counter,
            items,
            players,
            dimension: game.dim,
            level_uuid: game.level.player_uuid,
            uuids: game
                .persist
                .uuids
                .iter()
                .map(|(id, uuid)| (*id, *uuid))
                .collect(),
            raw: game
                .persist
                .raw
                .iter()
                .map(|(id, tag)| (*id, tag.clone()))
                .collect(),
            clocks: (game.world.game_time, game.world.day_time),
            random: game.world.random.state(),
        }
    }
    pub(in crate::game) fn recovery_request(
        &self,
        destination: PathBuf,
        deadline: Instant,
    ) -> RecoveryRequest {
        RecoveryRequest {
            expected_world: self.world,
            expected_target: self.target.clone(),
            expected_type: self.type_id.clone(),
            archive_root: self.archive.clone(),
            destination,
            deadline,
        }
    }
}
#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
pub(in crate::game) fn retained_coherent_close_fixture<'a>(
    fixture: &'a Fixture,
    label: &str,
    setup: RetainedCloseFixtureSetup,
    deadline: Instant,
) -> crate::game::paired_close::TestCloseCustody<'a> {
    let game = cold_retained_game(fixture, label);
    let root = game.world_root.clone();
    let mut source = session_opened_game(game, root, label, 2, true, false);
    source.game.lan.as_mut().unwrap().hub.guests[0].inventory[1] = Some(session_items_rich());
    source.game.spawn_item(
        session_items_rich(),
        [3.5, 121.0, 3.5],
        [0.125, 0.0, 0.0],
        17,
    );
    source.game.save_game_with(false).unwrap();
    source.game.session_fence(until()).unwrap();
    if matches!(setup, RetainedCloseFixtureSetup::MalformedOldLevelBackup) {
        fs::write(
            source.game.world_root.join("level.dat_old"),
            b"malformed retained old Level backup",
        )
        .unwrap();
    }
    let game = Box::new(source.game);
    let profile = crate::game::paired_close::CloseProfile::CoherentSession {
        archive: source.archive.clone(),
    };
    // Mint only at uninterrupted final handoff. No Game/Lan/Shared reference or
    // connector-capable handle escapes after this private construction point.
    let token = RetainedCloseFixtureToken {
        save: game.save.clone(),
        shared: game.lan.as_ref().unwrap().hub.lan.shared.clone(),
        world: *game
            .session_lock
            .as_ref()
            .unwrap()
            .expected_world()
            .unwrap(),
        root: game.world_root.clone(),
        counter: source.counter,
        slot: source.slot,
        archive: source.archive,
        target: source.target,
        type_id: source.type_id,
    };
    crate::game::paired_close::TestCloseCustody::from_constructor(
        game,
        profile,
        deadline,
        token,
        source.receivers,
    )
}
#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
#[test]
#[ignore = "isolated pinned resources + prebuilt pair: complete owned read refuses malformed old native backup before identities or final writes"]
fn coherent_image_old_level_backup_refuses_before_final_preparation() {
    let fixture = Fixture::new("coherent-old-level-backup");
    let _retain = RetainOnPanic::new(&fixture);
    let mut source = coherent_close_fixture(&fixture, "coherent-old-level-backup");
    source
        .game
        .spawn_item(session_items_rich(), [4.5, 121.0, 4.5], [0.0; 3], 17);
    let unassigned = source.game.dropped.last().unwrap().id;
    assert!(!source.game.persist.uuids.contains_key(&unassigned));
    fs::write(
        source.game.world_root.join("level.dat_old"),
        b"malformed old native level backup",
    )
    .unwrap();
    let files = tree(&source.game.world_root, &["session.lock"]).unwrap();
    let uuids = source.game.persist.uuids.clone();
    let raw = source.game.persist.raw.clone();
    let entity_head = source.game.next_entity;
    let dropped = source.game.dropped.len();
    let context = source
        .game
        .prepare_coherent_image_context(&source.archive)
        .unwrap();
    let error = match context.read_preflight_input(until()).run() {
        Ok(_) => panic!("malformed original native backup produced a read witness"),
        Err(error) => error,
    };
    assert_eq!(error.publication, Publication::NotPublished);
    assert_eq!(error.item_identities_assigned, 0);
    assert_eq!(source.game.persist.uuids, uuids);
    assert_eq!(source.game.persist.raw, raw);
    assert!(!source.game.persist.uuids.contains_key(&unassigned));
    assert_eq!(source.game.next_entity, entity_head);
    assert_eq!(source.game.dropped.len(), dropped);
    assert_eq!(
        tree(&source.game.world_root, &["session.lock"]).unwrap(),
        files
    );
    assert!(tree(&source.archive, &[]).unwrap().files.is_empty());
    assert!(tree(&source.archive, &[]).unwrap().directories.is_empty());
}

#[test]
fn native_read_opaque_profile_inspects_without_rewriting_bytes_or_weakening_d() {
    let opaque = Tag::compound([
        ("UUID", Tag::IntArray(vec![7, 8, 9, 10])),
        ("id", Tag::String("example:opaque_entity".into())),
        ("test:float", Tag::Float(0x7fc0_0073)),
        ("test:double", Tag::Double((-0.0f64).to_bits())),
        (
            "test:java",
            Tag::String(nbt::JavaString(vec![0xd800, 0x0061])),
        ),
        (
            "Passengers",
            Tag::List(vec![Tag::compound([
                ("id", Tag::String("example:opaque_passenger".into())),
                ("UUID", Tag::IntArray(vec![11, 12, 13, 14])),
            ])]),
        ),
    ]);
    let named = NamedTag {
        name: "native named root".into(),
        tag: opaque.clone(),
    };
    let raw = nbt::write_named(&named, limits()).unwrap();
    let original_bytes = raw.clone();
    let view = raw_native_nbt(&raw).unwrap();
    assert_eq!(view.name, named.name);
    assert_eq!(view.tag.get("UUID"), opaque.get("UUID"));
    assert_eq!(view.tag.get("test:java"), opaque.get("test:java"));
    // This temporary native inspection view is never installed or serialized:
    // the native reader normalizes zero, and its writer canonicalizes NaNs.
    assert_eq!(view.tag.get("test:double"), Some(&Tag::Double(0)));
    assert_eq!(view.tag.get("test:float"), Some(&Tag::Float(0x7fc0_0000)));
    assert_eq!(raw, original_bytes);
    assert!(raw_nbt(&raw).is_err());
    assert!(raw_item_nbt(&raw).is_err());
    let mut current = BTreeMap::new();
    native_entity_uuids(
        std::slice::from_ref(&opaque),
        (Dim::Nether, (3, -4)),
        &mut current,
    )
    .unwrap();
    assert_eq!(current.len(), 2);
    assert!(native_entity_uuids(&[opaque], (Dim::End, (4, 5)), &mut current).is_err());
    let mut trailing = raw;
    trailing.push(0);
    assert!(raw_native_nbt(&trailing).is_err());
    let duplicate = [
        10, 0, 0, 3, 0, 1, b'x', 0, 0, 0, 1, 3, 0, 1, b'x', 0, 0, 0, 2, 0,
    ];
    assert!(raw_native_nbt(&duplicate).is_err());
}

#[test]
fn native_read_record_nbt_suffixes_and_zlib_lz4_suffixes_refuse() {
    let named = NamedTag {
        name: "".into(),
        tag: Tag::compound([
            (
                "Entities",
                Tag::List(vec![Tag::compound([("test:raw", Tag::Float(0x7fc0_0000))])]),
            ),
            ("Position", Tag::IntArray(vec![0, 0])),
        ]),
    };
    for codec in [
        Compression::Gzip,
        Compression::Zlib,
        Compression::None,
        Compression::Lz4,
    ] {
        let record =
            horizon_world::region::ChunkRecord::from_nbt(&named, codec, 7, limits()).unwrap();
        assert_eq!(
            raw_native_nbt(&items::native_record_bytes(&record).unwrap()).unwrap(),
            named
        );
        let mut raw = nbt::write_named(&named, limits()).unwrap();
        raw.push(0);
        let changed = horizon_world::region::ChunkRecord {
            compression: codec,
            compressed: codec.encode(&raw).unwrap(),
            timestamp: 7,
        };
        assert!(raw_native_nbt(&items::native_record_bytes(&changed).unwrap()).is_err());
        if matches!(codec, Compression::Zlib | Compression::Lz4) {
            let mut suffix = record.clone();
            suffix.compressed.extend([1, 2, 3]);
            assert!(items::native_record_bytes(&suffix).is_err());
        }
    }
}

#[test]
fn native_read_dimension_and_opaque_entity_envelopes_are_routed_without_decoding() {
    let column = |y| {
        Tag::compound([
            ("xPos", Tag::Int(2)),
            ("zPos", Tag::Int(-3)),
            ("yPos", Tag::Int(i32::from(y))),
            ("Status", Tag::String("minecraft:full".into())),
            (
                "sections",
                Tag::List(vec![Tag::compound([("Y", Tag::Byte(y))])]),
            ),
            ("test:unknown", Tag::ByteArray(vec![9, 1, 7])),
        ])
    };
    native_column_envelope(&column(0), (2, -3), LevelShape::NETHER).unwrap();
    native_column_envelope(&column(-4), (2, -3), LevelShape::OVERWORLD).unwrap();
    assert!(native_column_envelope(&column(-4), (2, -3), LevelShape::NETHER).is_err());
    assert!(native_column_envelope(&column(0), (3, -3), LevelShape::END).is_err());
    let poi = |section: &str, pos: [i32; 3]| {
        Tag::compound([(
            "Sections",
            Tag::compound([(
                section,
                Tag::compound([(
                    "Records",
                    Tag::List(vec![Tag::compound([
                        ("pos", Tag::IntArray(pos.to_vec())),
                        ("type", Tag::String("example:unknown_poi".into())),
                        ("free_tickets", Tag::Int(2)),
                        ("test:opaque", Tag::Double(0x7ff8_0000_0000_0073)),
                    ])]),
                )]),
            )]),
        )])
    };
    native_poi_envelope(&poi("4", [33, 70, -47]), (2, -3), LevelShape::NETHER).unwrap();
    native_poi_envelope(&poi("-4", [33, -64, -47]), (2, -3), LevelShape::OVERWORLD).unwrap();
    assert!(native_poi_envelope(&poi("4", [49, 70, -47]), (2, -3), LevelShape::NETHER).is_err());
    assert!(native_poi_envelope(&poi("4", [33, 70, -31]), (2, -3), LevelShape::NETHER).is_err());
    assert!(native_poi_envelope(&poi("3", [33, 70, -47]), (2, -3), LevelShape::NETHER).is_err());
    assert!(native_poi_envelope(&poi("4", [33, -1, -47]), (2, -3), LevelShape::NETHER).is_err());
    assert!(native_poi_envelope(&poi("4", [33, 256, -47]), (2, -3), LevelShape::NETHER).is_err());
    assert!(native_poi_envelope(&poi("16", [33, 256, -47]), (2, -3), LevelShape::NETHER).is_err());
    assert!(
        native_poi_envelope(
            &Tag::compound([("Sections", Tag::List(Vec::new()))]),
            (2, -3),
            LevelShape::END
        )
        .is_err()
    );
}

#[test]
fn native_read_uuid_reservoir_and_exact_row_coverage_preserve_current_ownership() {
    use std::collections::HashSet;
    let player = [1, 2, 3, 4];
    let entity = [5, 6, 7, 8];
    let historical = [9, 10, 11, 12];
    let mut players = HashSet::new();
    let mut records = HashSet::new();
    let filename = format!("{}.dat", save::uuid_text(player));
    native_player_record(
        &filename,
        &Tag::compound([("UUID", Tag::IntArray(player.to_vec()))]),
        true,
        &mut players,
        &mut records,
    )
    .unwrap();
    native_player_record(
        &filename,
        &Tag::compound([("UUID", Tag::IntArray(player.to_vec()))]),
        false,
        &mut players,
        &mut records,
    )
    .unwrap();
    native_player_record(
        &format!("{}.dat", save::uuid_text(historical)),
        &Tag::compound([("UUID", Tag::IntArray(historical.to_vec()))]),
        false,
        &mut players,
        &mut records,
    )
    .unwrap();
    assert_eq!(records.len(), 1);
    assert!(!players.contains(&historical));
    assert!(
        native_player_record(
            &format!("{}.dat", save::uuid_text(entity)),
            &Tag::compound([("UUID", Tag::IntArray(player.to_vec()))]),
            true,
            &mut players,
            &mut records
        )
        .is_err()
    );
    // Use the clean expected current set after the intentionally rejected row.
    let players = HashSet::from([player]);
    let old = (Dim::Nether, (3, -4));
    let current = BTreeMap::from([(entity, old)]);
    assert_eq!(
        native_uuid_reservoir(&current, &players),
        HashSet::from([entity, player])
    );
    let final_rows = [(Dim::End, (7, 8), entity)];
    assert!(
        native_final_uuid_coverage(
            &current,
            &HashSet::new(),
            &players,
            &HashSet::new(),
            &HashSet::new(),
            &final_rows
        )
        .is_err()
    );
    native_final_uuid_coverage(
        &current,
        &HashSet::new(),
        &players,
        &HashSet::from([old]),
        &HashSet::new(),
        &final_rows,
    )
    .unwrap();
    // A separate entity sidecar overwrite cannot erase embedded legacy data.
    assert!(
        native_final_uuid_coverage(
            &current,
            &HashSet::from([entity]),
            &players,
            &HashSet::from([old]),
            &HashSet::new(),
            &final_rows
        )
        .is_err()
    );
    native_final_uuid_coverage(
        &current,
        &HashSet::from([entity]),
        &players,
        &HashSet::new(),
        &HashSet::from([old]),
        &final_rows,
    )
    .unwrap();
    assert!(
        native_final_uuid_coverage(
            &current,
            &HashSet::new(),
            &players,
            &HashSet::from([old]),
            &HashSet::new(),
            &[(Dim::End, (7, 8), player)]
        )
        .is_err()
    );
    assert!(
        native_final_uuid_coverage(
            &current,
            &HashSet::new(),
            &players,
            &HashSet::from([old]),
            &HashSet::new(),
            &[final_rows[0], final_rows[0]]
        )
        .is_err()
    );
}
fn session<'a>(fixture: &'a Fixture, label: &str, radius: i32, owner: bool) -> Session<'a> {
    let game = fresh_with_role(
        fixture,
        if owner {
            ExecutionRole::IntegratedServer
        } else {
            ExecutionRole::DedicatedServer
        },
    );
    let root = fixture
        .game()
        .world_root
        .parent()
        .unwrap()
        .join(format!("native-{label}"));
    session_with_game(game, root, label, radius, owner, false)
}
fn session_with_game<'a>(
    mut game: Game<'a>,
    root: PathBuf,
    label: &str,
    radius: i32,
    owner: bool,
    registered: bool,
) -> Session<'a> {
    open_native(&mut game, &root);
    session_opened_game(game, root, label, radius, owner, registered)
}
fn session_opened_game<'a>(
    mut game: Game<'a>,
    root: PathBuf,
    label: &str,
    radius: i32,
    owner: bool,
    registered: bool,
) -> Session<'a> {
    residency(&mut game, radius, (0, 0));
    let catalog = game.options.mod_packages.catalog().unwrap();
    let (prepared, ty) = catalog
        .prepared()
        .values()
        .flat_map(|p| {
            p.content()
                .machine_types()
                .types()
                .iter()
                .map(move |ty| (p, ty.id()))
        })
        .next()
        .unwrap();
    let type_id = ty.to_owned();
    let binding = if registered {
        NativeBinding::from_registered_prepared(prepared, &type_id, game.pack.assets.registry)
            .unwrap()
    } else {
        NativeBinding::from_prepared(prepared, &type_id, game.pack.assets.registry).unwrap()
    };
    game.world
        .set(POSITION[0], POSITION[1], POSITION[2], binding.state_id())
        .unwrap();
    let counter = if registered {
        game.bootstrap_registered_host_counter(&type_id, POSITION)
            .unwrap()
    } else {
        game.bootstrap_host_counter(&type_id, POSITION).unwrap()
    };
    let target = CounterTarget::new(Dim::Overworld.id(), POSITION).unwrap();
    let slot = game
        .world
        .initialize_persistent_world_slot(&binding, &target)
        .unwrap()
        .unwrap();
    let (mut host, receivers) = make_host(&mut game, owner, [10, 20], true);
    host.hub.guests[0].inventory[0] = Some(ItemStack::new("minecraft:stone", 10));
    host.hub.guests[1].inventory[0] = Some(ItemStack::new("minecraft:stone", 4));
    game.lan = Some(host);
    transfer(&mut game, &slot, 10, Direction::IntoMachine, 7);
    game.tickets.update(Some(Sources {
        players: vec![(0, 0)],
        forced: Vec::new(),
        view_distance: 2,
        simulation_distance: 2,
    }));
    for _ in 0..3 {
        game.world.game_time += 1;
        game.tick_counter_trial(true);
    }
    assert_eq!(
        game.world
            .read_persistent_counter(&counter)
            .unwrap()
            .counter(),
        3
    );
    // Native save AFTER real participant transfer gives genuine history/I/O.
    game.save_game_with(false).unwrap();
    game.session_fence(until()).unwrap();
    let archive = root.parent().unwrap().join(format!("archive-{label}"));
    fs::create_dir(&archive).unwrap();
    assert!(game.world.chunks.len() > 9);
    Session {
        game,
        counter,
        slot,
        archive,
        type_id,
        target,
        receivers,
    }
}
fn transfer(game: &mut Game<'_>, slot: &SlotKey, conn: u32, direction: Direction, count: u32) {
    let mut host = game.lan.take().unwrap();
    let result = game
        .as_guest(&mut host, conn, |game| {
            game.transfer_world_slot_inventory_cell(slot, 0, direction, count)
        })
        .unwrap()
        .unwrap();
    assert!(matches!(result,TransferOutcome::Committed(row) if row.moved==count));
    game.lan = Some(host);
}
fn total(game: &Game<'_>, slot: &SlotKey) -> u32 {
    game.world
        .read_world_slot(slot)
        .unwrap()
        .slot
        .stack()
        .map_or(0, |stack| stack.count)
        + game
            .lan
            .as_ref()
            .unwrap()
            .hub
            .guests
            .iter()
            .flat_map(|guest| guest.inventory.iter().flatten())
            .filter(|stack| &*stack.item == "minecraft:stone")
            .map(|stack| stack.count)
            .sum::<u32>()
}
fn request(session: &Session<'_>, destination: PathBuf) -> RecoveryRequest {
    RecoveryRequest {
        expected_world: session.counter.placement().world,
        expected_target: session.target.clone(),
        expected_type: session.type_id.clone(),
        archive_root: session.archive.clone(),
        destination,
        deadline: until(),
    }
}
fn restored<'a>(
    fixture: &'a Fixture,
    session: &Session<'_>,
    name: &str,
    owner: bool,
) -> (Game<'a>, RecoveryReceipt, Vec<mpsc::Receiver<Out>>) {
    let mut candidate = fresh_with_role(
        fixture,
        if owner {
            ExecutionRole::IntegratedServer
        } else {
            ExecutionRole::DedicatedServer
        },
    );
    let (host, receivers) = make_host(&mut candidate, owner, [110, 220], false);
    let destination = session.archive.parent().unwrap().join(name);
    let (game, receipt) = candidate
        .recover_session_checkpoint(request(session, destination), host)
        .unwrap();
    (game, receipt, receivers)
}
#[test]
#[ignore = "isolated pinned resources + prebuilt pair: actual native Game two epochs, both roster roles, variable residency and body preservation"]
fn session_actual_native_game_two_epochs_variable_shapes_fresh_keys_and_body_conservation() {
    let fixture = Fixture::new("session-two-epochs");
    let _retain = RetainOnPanic::new(&fixture);
    let mut shapes = Vec::new();
    for (index, owner, radius) in [(0, true, 2), (1, false, 3)] {
        let mut session = session(&fixture, &format!("shape{index}"), radius, owner);
        let _receivers = &session.receivers;
        let native_local = session.game.level.player_uuid;
        let before = session.game.session_snapshot().unwrap();
        shapes.push(before.residents.len());
        assert_eq!(total(&session.game, &session.slot), 14);
        session
            .game
            .checkpoint_session(&session.archive, 1, until())
            .unwrap();
        let first_image = tree(&published(&session, 1), &[]).unwrap();
        let (mut recovered, receipt, _receivers) =
            restored(&fixture, &session, &format!("recovered{index}"), owner);
        assert_eq!(recovered.level.player_uuid, native_local);
        assert_eq!(total(&recovered, &receipt.slot), 14);
        assert!(
            recovered
                .world
                .read_persistent_counter(&session.counter)
                .is_err()
        );
        assert!(recovered.world.read_world_slot(&session.slot).is_err());
        assert_ne!(
            receipt.counter.session_nonce(),
            session.counter.session_nonce()
        );
        assert_ne!(receipt.slot, session.slot);
        assert!(same_players(
            &before.players,
            &recovered.session_snapshot().unwrap().players
        ));
        for guest in &recovered.lan.as_ref().unwrap().hub.guests {
            let body = guest.motion.as_ref().unwrap();
            assert_eq!(
                [body.velocity.x, body.velocity.y, body.velocity.z],
                [0.125, -0.25, 0.375]
            );
            assert_eq!(body.health.food.exhaustion, 2.5);
            assert_eq!(body.health.food.timer, 91);
        }
        transfer(&mut recovered, &receipt.slot, 110, Direction::IntoPlayer, 2);
        assert_eq!(total(&recovered, &receipt.slot), 14);
        recovered.tickets.update(Some(Sources {
            players: vec![(0, 0)],
            forced: Vec::new(),
            view_distance: 2,
            simulation_distance: 2,
        }));
        assert!(
            recovered
                .ui
                .rules
                .set("spawn_mobs", horizon_client::gamerules::Value::Bool(false))
        );
        assert!(recovered.ui.rules.set(
            "spawn_monsters",
            horizon_client::gamerules::Value::Bool(false)
        ));
        assert!(recovered.ui.rules.set(
            "random_tick_speed",
            horizon_client::gamerules::Value::Int(0)
        ));
        let inhabited = recovered.world.inhabited.get(&(0, 0)).copied().unwrap_or(0);
        let clean = !recovered.world.modified.contains(&(0, 0));
        assert!(recovered.no_player_here() && recovered.player.is_none());
        assert_eq!(
            recovered.options.execution_role,
            if owner {
                ExecutionRole::IntegratedServer
            } else {
                ExecutionRole::DedicatedServer
            }
        );
        assert!(recovered.lan_acting.is_none());
        assert!(!recovered.sculk.tracker_loaded);
        let tracker = recovered.sculk.tracker;
        recovered.tick().unwrap();
        assert!(
            !recovered.sculk.tracker_loaded,
            "no local tracker is loaded in either authoritative roster role"
        );
        assert_eq!(
            recovered.sculk.tracker.ticks_since_last_warning,
            tracker.ticks_since_last_warning + 1,
            "the actual shared runtime tracker still ticks without a local actor"
        );
        assert_eq!(recovered.sculk.tracker.warning_level, 0);
        assert_eq!(recovered.sculk.tracker.cooldown_ticks, 0);
        let local = recovered.level.player_extra.clone();
        recovered.save_warden_tracker();
        assert_eq!(recovered.level.player_extra, local);
        assert!(
            recovered
                .level
                .player_tag()
                .get("warden_spawn_tracker")
                .is_none()
        );
        assert!(
            recovered.world.inhabited.get(&(0, 0)).copied().unwrap_or(0) > inhabited,
            "actual Game tick increments native inhabited metadata"
        );
        assert!(
            clean && !recovered.world.modified.contains(&(0, 0)),
            "metadata changes on a clean saved native column"
        );
        assert_eq!(
            recovered
                .world
                .read_persistent_counter(&receipt.counter)
                .unwrap()
                .counter(),
            4
        );
        let second_before = recovered.session_snapshot().unwrap();
        recovered
            .checkpoint_session(&session.archive, 2, until())
            .unwrap();
        let (mut again, again_receipt, _receivers) =
            restored(&fixture, &session, &format!("again{index}"), owner);
        assert_eq!(again_receipt.epoch, 2);
        assert_eq!(again.level.player_uuid, native_local);
        assert_eq!(total(&again, &again_receipt.slot), 14);
        assert!(same_players(
            &second_before.players,
            &again.session_snapshot().unwrap().players
        ));
        assert!(
            again
                .world
                .read_persistent_counter(&receipt.counter)
                .is_err()
        );
        assert!(again.world.read_world_slot(&receipt.slot).is_err());
        assert_eq!(
            again.world.counter_domain_snapshot().unwrap().high_water,
            recovered
                .world
                .counter_domain_snapshot()
                .unwrap()
                .high_water
        );
        assert_eq!(
            again.world.persistent_world_slot_metadata(),
            recovered.world.persistent_world_slot_metadata()
        );
        assert_eq!(again.world.game_time, recovered.world.game_time);
        assert_eq!(again.world.day_time, recovered.world.day_time);
        assert_eq!(
            again.session_checkpoint_raid_observation(),
            (false, second_before.raids.clone()),
            "recovery installs exact native raid metadata without a simulation tick"
        );
        let persisted = columns(&again, &again.world_root, again.world.game_time).unwrap();
        for (at, inhabited) in &recovered.world.inhabited {
            if recovered.world.chunks.contains_key(at) {
                assert_eq!(
                    persisted
                        .get(at)
                        .unwrap()
                        .saved
                        .as_ref()
                        .unwrap()
                        .root
                        .get(&"InhabitedTime".into()),
                    Some(&Tag::Long(*inhabited))
                );
                assert_eq!(
                    persisted
                        .get(at)
                        .unwrap()
                        .saved
                        .as_ref()
                        .unwrap()
                        .last_update,
                    recovered.world.game_time
                );
            }
        }
        // A further genuine tick advances the already-loaded empty raid clock
        // while it remains clean. Ordinary save_raids would skip this metadata.
        let (dirty, old_raids) = again.session_checkpoint_raid_observation();
        assert!(!dirty);
        let old_raids = old_raids.unwrap();
        let Some(Tag::Int(old_tick)) = old_raids.get("data").unwrap().get("tick") else {
            panic!("native raid clock has its exact Int type")
        };
        again.tick().unwrap();
        let (dirty, current_raids) = again.session_checkpoint_raid_observation();
        assert!(
            !dirty,
            "ordinary empty raid tick stays clean before tick200"
        );
        let current_raids = current_raids.unwrap();
        assert_eq!(
            current_raids.get("data").unwrap().get("tick"),
            Some(&Tag::Int(old_tick + 1))
        );
        assert_eq!(
            current_raids.get("data").unwrap().get("next_id"),
            old_raids.get("data").unwrap().get("next_id")
        );
        assert_ne!(
            raid_metadata(&again.world_root).unwrap(),
            Some(current_raids.clone()),
            "the native primary is genuinely stale before the checkpoint write"
        );
        let third_before = again.session_snapshot().unwrap();
        let previous = tree(&published(&session, 2), &[]).unwrap();
        let obstruction = again.world_root.join(RAIDS).with_file_name("raids.dat_new");
        assert!(obstruction.starts_with(&again.world_root));
        fs::create_dir(&obstruction).unwrap();
        let random = again.world.random.state();
        let error = again
            .checkpoint_session(&session.archive, 3, until())
            .unwrap_err();
        assert_eq!(error.publication, Publication::NotPublished);
        assert!(
            error
                .detail
                .contains("could not save session empty raids at"),
            "{error}"
        );
        assert_eq!(again.world.random.state(), random);
        assert_eq!(
            again.session_checkpoint_raid_observation(),
            (false, Some(current_raids.clone()))
        );
        assert_eq!(total(&again, &again_receipt.slot), 14);
        let failed = again.session_snapshot().unwrap();
        assert_eq!(failed.counter, third_before.counter);
        assert_eq!(failed.slot, third_before.slot);
        assert_eq!(failed.columns, third_before.columns);
        assert_eq!(failed.raids, third_before.raids);
        assert!(same_players(&failed.players, &third_before.players));
        assert_eq!(tree(&published(&session, 2), &[]).unwrap(), previous);
        assert!(!published(&session, 3).exists());
        // Remove only the empty directory this test exclusively created. No
        // native primary, historical record or pending image is erased.
        assert!(fs::read_dir(&obstruction).unwrap().next().is_none());
        fs::remove_dir(&obstruction).unwrap();
        again
            .checkpoint_session(&session.archive, 3, until())
            .unwrap();
        assert_eq!(again.world.random.state(), random);
        assert_eq!(
            again.session_checkpoint_raid_observation(),
            (false, Some(current_raids.clone()))
        );
        assert_eq!(
            raid_metadata(&again.world_root).unwrap(),
            Some(current_raids.clone())
        );
        let old = compressed_nbt(
            &read(
                &again.world_root.join(format!("{RAIDS}_old")),
                STRUCTURED_LIMIT,
            )
            .unwrap(),
        )
        .unwrap();
        validate_session_empty_raids(&old).unwrap();
        assert_eq!(
            old, old_raids,
            "native rotation preserves the independently valid previous clock"
        );
        assert_eq!(tree(&published(&session, 2), &[]).unwrap(), previous);
        let (mut third, third_receipt, _receivers) =
            restored(&fixture, &session, &format!("third{index}"), owner);
        assert_eq!(third_receipt.epoch, 3);
        assert_eq!(
            third.session_checkpoint_raid_observation(),
            (false, Some(current_raids))
        );
        assert_eq!(third.world.game_time, again.world.game_time);
        assert_eq!(third.world.day_time, again.world.day_time);
        assert_eq!(third.level.player_uuid, native_local);
        assert_eq!(total(&third, &third_receipt.slot), 14);
        assert!(same_players(
            &third_before.players,
            &third.session_snapshot().unwrap().players
        ));
        assert_eq!(
            third
                .world
                .read_persistent_counter(&third_receipt.counter)
                .unwrap()
                .counter(),
            5
        );
        assert!(
            third
                .world
                .read_persistent_counter(&again_receipt.counter)
                .is_err()
        );
        assert!(third.world.read_world_slot(&again_receipt.slot).is_err());
        let image = published(&session, 3);
        let inventory = tree(&image, &[]).unwrap();
        let copy = session
            .archive
            .parent()
            .unwrap()
            .join(format!("raid-controls{index}"));
        copy_tree(&image, &copy, &inventory, until()).unwrap();
        // Independently check the primary and historical backup after resealing,
        // so refusal proves the closed native schema, rather than a stale hash.
        for name in [RAIDS.to_owned(), format!("{RAIDS}_old")] {
            let path = copy.join("world").join(&name);
            let original = read(&path, STRUCTURED_LIMIT).unwrap();
            let mut corrupt = compressed_nbt(&original).unwrap();
            let Tag::Compound(root) = &mut corrupt else {
                unreachable!()
            };
            let Tag::Compound(data) = root.get_mut(&"data".into()).unwrap() else {
                unreachable!()
            };
            data.insert("raids".into(), Tag::List(Vec::new()));
            fs::write(&path, nbt_bytes(&corrupt).unwrap()).unwrap();
            reseal(&copy, 3);
            let error = verify_image(
                &third,
                &copy,
                3,
                session.counter.placement().world,
                &session.target,
                &session.type_id,
            )
            .err()
            .expect("opaque primary or backup is refused after a valid manifest");
            assert!(error.detail.contains("session raids"), "{error}");
            fs::write(&path, original).unwrap();
        }
        reseal(&copy, 3);
        assert_eq!(tree(&image, &[]).unwrap(), inventory);
        // Ordinary native opening leaves raids lazy. Its effective projection
        // must preserve the checked disk counters instead of native defaults.
        let mut unopened = fresh(&fixture);
        let mut request = WorldRequest::new("unused", None, None, false, None)
            .unwrap()
            .with_expected_checkpoint_world(session.counter.placement().world);
        request.save_root = copy.join("world");
        let prepared = request
            .prepare(
                &unopened.res,
                &unopened.pack,
                &unopened.options,
                &unopened.settings,
            )
            .unwrap();
        unopened.finish_open_world(prepared).unwrap();
        assert!(unopened.session_checkpoint_raids().unwrap().is_none());
        let before = read(&unopened.world_root.join(RAIDS), STRUCTURED_LIMIT).unwrap();
        let random = unopened.world.random.state();
        assert_eq!(
            unopened.effective_session_raids().unwrap(),
            third_before.raids
        );
        unopened.write_session_checkpoint_raids().unwrap();
        assert_eq!(unopened.world.random.state(), random);
        assert!(unopened.session_checkpoint_raids().unwrap().is_none());
        assert_eq!(
            read(&unopened.world_root.join(RAIDS), STRUCTURED_LIMIT).unwrap(),
            before
        );
        // Unsupported old local metadata is retained verbatim; absent-local
        // load/save neither interprets it nor normalizes it into an inert tag.
        let old_tracker = horizon_client::sculk::WardenSpawnTracker {
            ticks_since_last_warning: 19,
            warning_level: 2,
            cooldown_ticks: 7,
        };
        let extra = unopened
            .level
            .player_extra
            .get_or_insert_with(|| Tag::Compound(Default::default()));
        let Tag::Compound(fields) = extra else {
            unreachable!()
        };
        fields.insert("warden_spawn_tracker".into(), old_tracker.to_nbt());
        let local = unopened.level.player_extra.clone();
        assert!(unopened.no_player_here() && unopened.player.is_none());
        assert!(!unopened.sculk.tracker_loaded);
        let tracker = unopened.sculk.tracker;
        unopened.tick_sculk_entities();
        unopened.save_warden_tracker();
        assert!(!unopened.sculk.tracker_loaded);
        assert_eq!(unopened.sculk.tracker.warning_level, tracker.warning_level);
        assert_eq!(
            unopened.sculk.tracker.cooldown_ticks,
            tracker.cooldown_ticks
        );
        assert_eq!(
            unopened.sculk.tracker.ticks_since_last_warning,
            tracker.ticks_since_last_warning + 1
        );
        assert_eq!(unopened.level.player_extra, local);
        assert!(!lan::session_empty_native_player(
            &unopened.level.player_tag()
        ));

        // Each live nonzero owner is tested in an otherwise successful actual
        // recovered session, then retained on refusal without clearing it.
        if owner {
            third.sculk.tracker.set_warning_level(1);
        } else {
            third.sculk.tracker.cooldown_ticks = 1;
        }
        let tracker = third.sculk.tracker;
        let error = third
            .checkpoint_session(&session.archive, 4, until())
            .unwrap_err();
        assert_eq!(error.publication, Publication::NotPublished);
        assert!(
            error.detail.contains("unsupported native warden"),
            "{error}"
        );
        assert_eq!(third.sculk.tracker, tracker);
        assert_eq!(total(&third, &third_receipt.slot), 14);
        assert_eq!(tree(&image, &[]).unwrap(), inventory);
        assert_eq!(tree(&published(&session, 2), &[]).unwrap(), previous);
        assert_eq!(tree(&published(&session, 1), &[]).unwrap(), first_image);
        assert!(!published(&session, 4).exists());
    }
    assert_ne!(shapes[0], shapes[1]);
}

fn published(session: &Session<'_>, epoch: u64) -> PathBuf {
    session.archive.join(format!("epoch-{epoch:020}"))
}
fn reseal(root: &Path, epoch: u64) {
    let mut manifest: Manifest = json(
        &fs::read(root.join("manifest.json")).unwrap(),
        MANIFEST_LIMIT,
    )
    .unwrap();
    manifest.epoch = epoch;
    let inventory = tree(root, &["manifest.json"]).unwrap();
    manifest.directories = inventory.directories;
    manifest.files = inventory.files;
    fs::write(
        root.join("manifest.json"),
        json_bytes(&manifest, MANIFEST_LIMIT).unwrap(),
    )
    .unwrap();
}
#[test]
#[ignore = "isolated pinned resources + prebuilt pair: actual body-only observations, provenance retirement, roster/install and native schema refusals"]
fn session_actual_body_only_changes_and_failed_second_install_preserve_all_live_owners() {
    let fixture = Fixture::new("session-body-refusals");
    let _retain = RetainOnPanic::new(&fixture);
    let mut session = session(&fixture, "body", 2, true);
    session
        .game
        .checkpoint_session(&session.archive, 1, until())
        .unwrap();
    let image = published(&session, 1);
    let (mut recovered, receipt, _receivers) = restored(&fixture, &session, "body-recovered", true);
    let before = recovered.session_snapshot().unwrap();
    let cached = recovered.level.player_tag();
    for change in 0..3 {
        let body = recovered.lan.as_mut().unwrap().hub.guests[0]
            .motion
            .as_mut()
            .unwrap();
        match change {
            0 => body.velocity.x += 0.5,
            1 => body.health.food.exhaustion += 1.0,
            _ => body.health.food.timer += 1,
        };
        assert_eq!(
            recovered.level.player_tag(),
            cached,
            "changing only real motion does not touch cached Level"
        );
        let after = recovered.session_snapshot().unwrap();
        assert!(
            !same_players(&before.players, &after.players),
            "body field {change} cannot hide in cached Level"
        );
        let body = recovered.lan.as_mut().unwrap().hub.guests[0]
            .motion
            .as_mut()
            .unwrap();
        match change {
            0 => body.velocity.x -= 0.5,
            1 => body.health.food.exhaustion -= 1.0,
            _ => body.health.food.timer -= 1,
        };
    }
    let checked = verify_image(
        &recovered,
        &image,
        1,
        session.counter.placement().world,
        &session.target,
        &session.type_id,
    )
    .unwrap();
    let mut donor = fresh(&fixture);
    let (host, _receivers) = make_host(&mut donor, true, [333, 444], false);
    assert!(
        recovered
            .install_session_checkpoint_participants(host, &checked.players)
            .is_err()
    );
    let after = recovered.session_snapshot().unwrap();
    assert_eq!(before.counter, after.counter);
    assert_eq!(before.slot, after.slot);
    assert!(same_players(&before.players, &after.players));
    assert_eq!(total(&recovered, &receipt.slot), 14);
    let previous = tree(&image, &[]).unwrap();
    recovered.lan.as_mut().unwrap().hub.guests[0]
        .motion
        .as_mut()
        .unwrap()
        .velocity
        .x = f64::NAN;
    let error = recovered
        .checkpoint_session(&session.archive, 2, until())
        .unwrap_err();
    assert_eq!(error.publication, Publication::NotPublished);
    assert_eq!(tree(&image, &[]).unwrap(), previous);
    recovered.lan.as_mut().unwrap().hub.guests[0]
        .motion
        .as_mut()
        .unwrap()
        .velocity
        .x = 0.125;
    recovered.retire_counter_trial("session-provenance-control");
    assert!(recovered.native_checkpoint_origin.is_none());
    assert!(
        recovered
            .checkpoint_session(&session.archive, 2, until())
            .is_err()
    );
}
#[test]
#[ignore = "isolated pinned resources + prebuilt pair: all members, native raw roots/headers/proto, package/schema/roster and newest-complete no-fallback"]
fn session_actual_complete_image_corruption_and_native_proto_refuse_before_regeneration() {
    let fixture = Fixture::new("session-corruption");
    let _retain = RetainOnPanic::new(&fixture);
    let mut session = session(&fixture, "corrupt", 2, true);
    session
        .game
        .checkpoint_session(&session.archive, 1, until())
        .unwrap();
    let baseline = published(&session, 1);
    let baseline_tree = tree(&baseline, &[]).unwrap();
    for case in 0..14 {
        let archive = session
            .archive
            .parent()
            .unwrap()
            .join(format!("corrupt-archive-{case}"));
        fs::create_dir(&archive).unwrap();
        let old = archive.join("epoch-00000000000000000001");
        copy_tree(&baseline, &old, &baseline_tree, until()).unwrap();
        let newer = archive.join("epoch-00000000000000000002");
        copy_tree(&baseline, &newer, &baseline_tree, until()).unwrap();
        reseal(&newer, 2);
        let mut manifest: Manifest = json(
            &fs::read(newer.join("manifest.json")).unwrap(),
            MANIFEST_LIMIT,
        )
        .unwrap();
        match case {
            0 => {
                fs::remove_file(newer.join(SLOT)).unwrap();
            }
            1 => {
                fs::write(newer.join(COUNTER), b"{}").unwrap();
                reseal(&newer, 2);
            }
            2 => {
                fs::write(newer.join(format!("world/{PIN}")), b"{}").unwrap();
                reseal(&newer, 2);
            }
            3 => {
                manifest.type_id.push_str("_wrong");
                fs::write(
                    newer.join("manifest.json"),
                    json_bytes(&manifest, MANIFEST_LIMIT).unwrap(),
                )
                .unwrap();
            }
            4 => {
                manifest.players[0].record_uuid[0] ^= 1;
                fs::write(
                    newer.join("manifest.json"),
                    json_bytes(&manifest, MANIFEST_LIMIT).unwrap(),
                )
                .unwrap();
            }
            5 => {
                let path = newer.join(player_path(&manifest.players[0], "stats"));
                fs::write(path,b"{\"stats\":{\"minecraft:custom\":{\"minecraft:jump\":1.5}},\"DataVersion\":0}").unwrap();
                reseal(&newer, 2);
            }
            6..=11 => {
                let name = manifest
                    .files
                    .iter()
                    .find(|row| row.path.ends_with(".mca"))
                    .unwrap()
                    .path
                    .clone();
                let path = newer.join(&name);
                let mut bytes = fs::read(&path).unwrap();
                let used = raw_region(&bytes).unwrap();
                let unused = (0..1024).find(|i| !used.contains(i)).unwrap();
                let location = bytes[used[0] * 4..used[0] * 4 + 4].to_vec();
                match case {
                    6 => bytes.truncate(4096),
                    7 => bytes[unused * 4..unused * 4 + 4].copy_from_slice(&0x200u32.to_be_bytes()),
                    8 => bytes[unused * 4..unused * 4 + 4].copy_from_slice(&location),
                    9 => bytes[unused * 4..unused * 4 + 4]
                        .copy_from_slice(&0xffff_ff01u32.to_be_bytes()),
                    10 => {
                        let location = u32::from_be_bytes(location.try_into().unwrap());
                        bytes[(location >> 8) as usize * 4096 + 4] |= 128;
                    }
                    _ => {
                        fs::write(
                            newer.join(
                                "world/dimensions/minecraft/overworld/region/c.4096.4096.mcc",
                            ),
                            b"unsupported external owner",
                        )
                        .unwrap();
                    }
                }
                fs::write(&path, bytes).unwrap();
                reseal(&newer, 2);
            }
            12..=13 => {
                let name = manifest
                    .files
                    .iter()
                    .find(|row| row.path.ends_with(".mca"))
                    .unwrap()
                    .path
                    .clone();
                let path = newer.join(&name);
                let mut region = RegionFile::open_writable(&path, false).unwrap();
                let coordinate = region.chunks().next().unwrap().0;
                let record = region.read(coordinate).unwrap().unwrap();
                let mut named = raw_nbt(
                    &record
                        .compression
                        .decode(&record.compressed, STRUCTURED_LIMIT)
                        .unwrap(),
                )
                .unwrap();
                if let Tag::Compound(fields) = &mut named.tag {
                    if case == 12 {
                        fields.insert("Status".into(), Tag::String("minecraft:features".into()));
                    } else {
                        fields.insert("opaque_owner".into(), Tag::Int(1));
                    }
                }
                let record = horizon_world::region::ChunkRecord::from_nbt(
                    &named,
                    Compression::Zlib,
                    0,
                    limits(),
                )
                .unwrap();
                region.write(coordinate, &record).unwrap();
                drop(region);
                reseal(&newer, 2);
            }
            _ => unreachable!(),
        }
        let mut candidate = fresh(&fixture);
        let (host, _receivers) = make_host(&mut candidate, true, [310, 320], false);
        let destination = archive
            .parent()
            .unwrap()
            .join(format!("corrupt-destination-{case}"));
        let mut request = request(&session, destination.clone());
        request.archive_root = archive;
        assert!(
            candidate.recover_session_checkpoint(request, host).is_err(),
            "corrupt case {case} cannot return a Game"
        );
        assert!(
            !destination.exists(),
            "case {case} refuses before native preparation/regeneration"
        );
        assert_eq!(tree(&baseline, &[]).unwrap(), baseline_tree);
    }
}
#[test]
#[ignore = "isolated pinned resources + prebuilt pair: publication phase interruption classifications and previous complete preservation"]
fn session_actual_copy_manifest_promotion_and_parent_sync_interruptions_preserve_previous_epoch() {
    let fixture = Fixture::new("session-interruptions");
    let _retain = RetainOnPanic::new(&fixture);
    let mut session = session(&fixture, "interrupt", 2, true);
    session
        .game
        .checkpoint_session(&session.archive, 1, until())
        .unwrap();
    let baseline = tree(&published(&session, 1), &[]).unwrap();
    for (index, at) in [
        Step::Copied,
        Step::Overlaid,
        Step::MembersSynced,
        Step::ManifestCompleted,
        Step::BeforePromotion,
        Step::Promoted,
        Step::ParentSynced,
    ]
    .into_iter()
    .enumerate()
    {
        let epoch = index as u64 + 2;
        let error = session
            .game
            .checkpoint_session_steps(&session.archive, epoch, until(), |step| {
                if step == at {
                    Err(fail("paired test interruption"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
        let uncertain = matches!(at, Step::Promoted | Step::ParentSynced);
        assert_eq!(
            error.publication,
            if uncertain {
                Publication::Uncertain
            } else {
                Publication::NotPublished
            }
        );
        assert_eq!(tree(&published(&session, 1), &[]).unwrap(), baseline);
        assert_eq!(published(&session, epoch).exists(), uncertain);
        assert_eq!(
            session
                .archive
                .join(format!(".pending-{epoch:020}"))
                .exists(),
            !uncertain
        );
        assert_eq!(total(&session.game, &session.slot), 14);
    }
    // A failing rename attempt is also uncertain, regardless of the observed
    // destination afterwards. An occupied destination is created at the seam.
    let epoch = 20;
    let destination = published(&session, epoch);
    let error = session
        .game
        .checkpoint_session_steps(&session.archive, epoch, until(), |step| {
            if step == Step::BeforePromotion {
                fs::create_dir(&destination).unwrap();
                fs::write(destination.join("occupied"), b"held").unwrap();
            }
            Ok(())
        })
        .unwrap_err();
    assert_eq!(error.publication, Publication::Uncertain);
    assert_eq!(fs::read(destination.join("occupied")).unwrap(), b"held");
    assert_eq!(tree(&published(&session, 1), &[]).unwrap(), baseline);
}
#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
#[test]
#[ignore = "isolated pinned resources + prebuilt pair: actual Game writer gate timeout/release/retry and real writer panic; no Integrated wrapper"]
fn session_actual_writer_block_timeout_release_retry_and_panic_retains_unpublished_ownership() {
    use horizon_client::save::NativeWriterTestAction;
    let fixture = Fixture::new("session-writer-control");
    let _retain = RetainOnPanic::new(&fixture);
    let mut session = session(&fixture, "blocked", 2, true);
    assert!(session.game.integrated.is_none());
    let mut gate = session
        .game
        .save
        .arm_native_writer_test_control(NativeWriterTestAction::Block)
        .unwrap();
    let stone = session
        .game
        .pack
        .assets
        .registry
        .default_state("minecraft:stone")
        .unwrap();
    session.game.world.set(3, 120, 2, stone).unwrap();
    session.game.save_game_with(false).unwrap();
    gate.wait_entered(WAIT).unwrap();
    let before = session.game.save.observe_native_writer_for_test().unwrap();
    assert!(before.outstanding_batches() > 0 && before.pending_records() > 0);
    let error = session
        .game
        .checkpoint_session(
            &session.archive,
            1,
            Instant::now() + Duration::from_millis(20),
        )
        .unwrap_err();
    assert_eq!(error.publication, Publication::NotPublished);
    assert!(!published(&session, 1).exists());
    assert_eq!(total(&session.game, &session.slot), 14);
    gate.trigger().unwrap();
    drop(gate);
    session
        .game
        .checkpoint_session(&session.archive, 1, until())
        .unwrap();
    let previous = tree(&published(&session, 1), &[]).unwrap();
    let mut gate = session
        .game
        .save
        .arm_native_writer_test_control(NativeWriterTestAction::Panic)
        .unwrap();
    session.game.world.set(4, 120, 2, stone).unwrap();
    session.game.save_game_with(false).unwrap();
    gate.wait_entered(WAIT).unwrap();
    gate.trigger().unwrap();
    let deadline = until();
    loop {
        let observed = session.game.save.observe_native_writer_for_test().unwrap();
        if observed.thread_finished() == Some(true) {
            assert!(observed.outstanding_batches() > 0 && observed.pending_records() > 0);
            assert!(observed.sticky_error().is_none());
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    let error = session
        .game
        .checkpoint_session(
            &session.archive,
            2,
            Instant::now() + Duration::from_millis(20),
        )
        .unwrap_err();
    assert_eq!(error.publication, Publication::NotPublished);
    assert_eq!(tree(&published(&session, 1), &[]).unwrap(), previous);
    assert!(!published(&session, 2).exists());
    assert!(!session.game.save.writes_finished().unwrap());
    drop(gate);
    // No live Integrated owner may run its flush-on-shutdown path. Native fence
    // settled producers before the writer wait; direct Game/SaveDir Drop joins
    // the panicked thread without repairing pending state or calling flush.
    assert!(session.game.integrated.is_none());
    drop(session);
}
#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
#[test]
#[ignore = "isolated pinned resources + prebuilt pair: actual filesystem sticky writer error, no false publication"]
fn session_actual_filesystem_writer_error_is_not_published_and_previous_image_survives() {
    let fixture = Fixture::new("session-filesystem-error");
    let _retain = RetainOnPanic::new(&fixture);
    let mut session = session(&fixture, "filesystem", 2, true);
    session
        .game
        .checkpoint_session(&session.archive, 1, until())
        .unwrap();
    let previous = tree(&published(&session, 1), &[]).unwrap();
    let healthy = session.game.save.observe_native_writer_for_test().unwrap();
    assert!(
        healthy.sticky_error().is_none(),
        "initially healthy writer: {healthy:?}"
    );
    assert!(
        session.game.save.writes_finished().unwrap(),
        "initial native writer completion: {healthy:?}"
    );
    let region = session.game.save.root.join("region");
    // Existing writer files are not corrupted: a fresh destination region for a
    // genuinely generated farther area is blocked by an actual directory.
    let blocked = region.join("r.2.2.mca");
    fs::create_dir(&blocked).unwrap();
    let before = native_counts(&session.game, NativeWorkOrigin::SaveFinishedCallback);
    submit_far_finish(&mut session.game, (80, 80), true);
    let fence = session.game.session_fence(until());
    let work = session.game.streaming.observe_native_work();
    assert!(
        fence.is_err(),
        "blocked actual native callback writer; work={work:#?}"
    );
    let after = native_counts(&session.game, NativeWorkOrigin::SaveFinishedCallback);
    // The real writer can fail between the callback's region and entity
    // enqueues. Record that genuine partial failure rather than relabel it
    // as a fully accepted callback.
    assert!(
        after.submitted > before.submitted
            && (after.accepted_storage_handoffs > before.accepted_storage_handoffs
                || after.storage_errors > before.storage_errors),
        "actual far Finish storage outcome; work={work:#?}"
    );
    let observed = session.game.save.observe_native_writer_for_test().unwrap();
    retain_log(format_args!(
        "SESSION_FILESYSTEM_CALLBACK accepted_before={} accepted_after={} errors_before={} errors_after={} writer={observed:?}\n",
        before.accepted_storage_handoffs,
        after.accepted_storage_handoffs,
        before.storage_errors,
        after.storage_errors
    ));
    assert!(
        observed.sticky_error().is_some(),
        "actual filesystem writer error: {observed:?}; work={work:#?}"
    );
    let error = session
        .game
        .checkpoint_session(&session.archive, 2, until())
        .unwrap_err();
    assert_eq!(error.publication, Publication::NotPublished);
    assert_eq!(tree(&published(&session, 1), &[]).unwrap(), previous);
    assert!(!published(&session, 2).exists());
    // Stop producers with the real sticky error visible; Drop joins the writer,
    // never claims successful durability and never patches pending accounting.
    drop(session);
}

#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
#[test]
#[ignore = "isolated pinned resources + prebuilt pair: real late built-in Finish callback during exclusive checkpoint fence"]
fn session_actual_late_finish_storage_handoff_is_closed_before_snapshot_and_retry() {
    use horizon_client::save::NativeWriterTestAction;
    let fixture = Fixture::new("session-late-finish");
    let _retain = RetainOnPanic::new(&fixture);
    let mut session = session(&fixture, "late-finish", 2, true);
    let count = |game: &Game<'_>, origin| {
        game.streaming
            .observe_native_work()
            .origins
            .iter()
            .find(|(candidate, _)| *candidate == origin)
            .unwrap()
            .1
    };
    let handoff = count(&session.game, NativeWorkOrigin::SaveFinishedCallback);
    let mut gate = session
        .game
        .save
        .arm_native_writer_test_control(NativeWriterTestAction::Block)
        .unwrap();
    submit_far_finish(&mut session.game, (-112, -112), true);
    let error = session
        .game
        .checkpoint_session(
            &session.archive,
            1,
            Instant::now() + Duration::from_millis(30),
        )
        .unwrap_err();
    assert_eq!(error.publication, Publication::NotPublished);
    assert!(!published(&session, 1).exists());
    gate.wait_entered(WAIT).unwrap();
    let after = count(&session.game, NativeWorkOrigin::SaveFinishedCallback);
    assert!(
        after.accepted_storage_handoffs > handoff.accepted_storage_handoffs,
        "late actual Finish storage handoff precedes final writer poll; work={:#?}",
        session.game.streaming.observe_native_work()
    );
    assert!(
        session
            .game
            .save
            .observe_native_writer_for_test()
            .unwrap()
            .outstanding_batches()
            > 0
    );
    gate.trigger().unwrap();
    drop(gate);
    native_fence(&mut session.game, until());
    residency(&mut session.game, 2, (0, 0));
    session
        .game
        .checkpoint_session(&session.archive, 1, until())
        .unwrap();
    assert_eq!(total(&session.game, &session.slot), 14);
}
#[test]
#[ignore = "isolated pinned resources + prebuilt pair: real zero dedicated roster/inert native cache plus no orphan item/body authority"]
fn session_actual_empty_dedicated_roster_preserves_inert_local_uuid_and_refuses_orphan_authority() {
    let fixture = Fixture::new("session-zero-roster");
    let _retain = RetainOnPanic::new(&fixture);
    let mut game = fresh(&fixture);
    let root = fixture
        .game()
        .world_root
        .parent()
        .unwrap()
        .join("native-zero");
    open_native(&mut game, &root);
    residency(&mut game, 2, (0, 0));
    let catalog = game.options.mod_packages.catalog().unwrap();
    let (prepared, type_id) = catalog
        .prepared()
        .values()
        .flat_map(|p| {
            p.content()
                .machine_types()
                .types()
                .iter()
                .map(move |ty| (p, ty.id()))
        })
        .next()
        .unwrap();
    let type_id = type_id.to_owned();
    let native =
        NativeBinding::from_prepared(prepared, &type_id, game.pack.assets.registry).unwrap();
    game.world
        .set(POSITION[0], POSITION[1], POSITION[2], native.state_id())
        .unwrap();
    let counter = game.bootstrap_host_counter(&type_id, POSITION).unwrap();
    let target = CounterTarget::new(Dim::Overworld.id(), POSITION).unwrap();
    let slot = game
        .world
        .initialize_persistent_world_slot(&native, &target)
        .unwrap()
        .unwrap();
    let (mut host, receivers) = make_host(&mut game, false, [1000, 2000], false);
    host.hub.guests.clear();
    game.lan = Some(host);
    assert!(game.observe_checkpoint_participants().is_ok());
    assert!(
        game.project_session_participant_rows(&game.observe_checkpoint_participants().unwrap())
            .unwrap()
            .is_empty()
    );
    let archive = root.parent().unwrap().join("zero-archive");
    fs::create_dir(&archive).unwrap();
    let native_uuid = game.level.player_uuid;
    game.checkpoint_session(&archive, 1, until()).unwrap();
    let session = Session {
        game,
        counter,
        slot,
        archive,
        type_id,
        target,
        receivers,
    };
    let mut candidate = fresh(&fixture);
    let (mut host, _receivers) = make_host(&mut candidate, false, [1001, 2001], false);
    host.hub.guests.clear();
    let destination = root.parent().unwrap().join("zero-recovered");
    let (mut recovered, receipt) = candidate
        .recover_session_checkpoint(request(&session, destination), host)
        .unwrap();
    assert!(recovered.lan.as_ref().unwrap().owner.is_none());
    assert!(recovered.lan.as_ref().unwrap().hub.guests.is_empty());
    assert_eq!(recovered.level.player_uuid, native_uuid);
    assert_eq!(
        recovered
            .world
            .read_persistent_counter(&receipt.counter)
            .unwrap()
            .counter(),
        0
    );
    assert!(
        recovered
            .world
            .read_world_slot(&receipt.slot)
            .unwrap()
            .slot
            .stack()
            .is_none()
    );
    let previous = tree(&published(&session, 1), &[]).unwrap();
    recovered
        .level
        .inventory
        .push((0, "minecraft:stone".into(), 1, 0));
    assert!(
        recovered
            .checkpoint_session(&session.archive, 2, until())
            .is_err()
    );
    assert_eq!(tree(&published(&session, 1), &[]).unwrap(), previous);
}
#[test]
#[ignore = "isolated pinned resources + prebuilt pair: actual raw native columns closed schema, sections, old metadata and native body profile"]
fn session_actual_native_record_and_player_shape_controls() {
    let fixture = Fixture::new("session-native-shapes");
    let _retain = RetainOnPanic::new(&fixture);
    let mut session = session(&fixture, "shapes", 2, true);
    session
        .game
        .checkpoint_session(&session.archive, 1, until())
        .unwrap();
    let image = verify_image(
        &session.game,
        &published(&session, 1),
        1,
        session.counter.placement().world,
        &session.target,
        &session.type_id,
    )
    .unwrap();
    let at = (POSITION[0] >> 4, POSITION[2] >> 4);
    let chunk = image.columns.get(&at).unwrap();
    let named = save::encode_with(
        chunk,
        session.game.pack.assets.registry,
        session.game.res.biomes(),
        session.game.save.tables.get().map(Arc::as_ref),
        LevelShape::OVERWORLD,
    );
    assert!(validate_column(&session.game, &named, at, session.game.world.game_time).is_ok());
    let live_lit = session
        .game
        .world
        .chunks
        .values()
        .find(|chunk| chunk.lit)
        .expect("actual current native lit column");
    let lit_snapshot = session.game.with_entities([live_lit]).remove(0);
    let lit_named = save::encode_with(
        &lit_snapshot,
        session.game.pack.assets.registry,
        session.game.res.biomes(),
        session.game.save.tables.get().map(Arc::as_ref),
        LevelShape::OVERWORLD,
    );
    assert_eq!(lit_named.tag.get("isLightOn"), Some(&Tag::Byte(1)));
    assert!(
        list(lit_named.tag.get("sections"))
            .unwrap()
            .iter()
            .any(|section| section.get("SkyLight").is_some())
    );
    // The initial far Finish callback really wrote this off-resident unlit
    // record. Read that archived native record rather than inventing a chunk.
    let far_path =
        published(&session, 1).join("world/dimensions/minecraft/overworld/region/r.-3.-3.mca");
    assert!(
        !raw_region(&read(&far_path, FILE_LIMIT).unwrap())
            .unwrap()
            .is_empty()
    );
    let mut far_region = RegionFile::open(&far_path).unwrap();
    let far_coordinate = far_region.chunks().next().unwrap().0;
    let record = far_region.read(far_coordinate).unwrap().unwrap();
    let unlit_named = raw_nbt(
        &record
            .compression
            .decode(&record.compressed, STRUCTURED_LIMIT)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(unlit_named.tag.get("isLightOn"), Some(&Tag::Byte(0)));
    assert!(list(unlit_named.tag.get("sections")).unwrap().iter()
        .all(|section| section.get("SkyLight").is_none()
            && section.get("BlockLight").is_none()));
    let native_unlit = Arc::new(
        save::decode(
            &unlit_named,
            session.game.pack.assets.registry,
            session.game.res.biomes(),
        )
        .unwrap(),
    );
    for (label, saved, expected, lit) in [
        ("current lit", lit_named, lit_snapshot, true),
        ("archived unlit Finish", unlit_named, native_unlit, false),
    ] {
        let restored = validate_column(
            &session.game,
            &saved,
            (expected.x, expected.z),
            session.game.world.game_time,
        )
        .unwrap();
        assert_eq!((restored.lit, restored.light_loaded), (lit, lit), "{label}");
        assert_eq!(restored.sections.len(), expected.sections.len());
        for (actual, expected) in restored.sections.iter().zip(&expected.sections) {
            for index in 0..4096 {
                assert_eq!(
                    actual.get(index),
                    expected.get(index),
                    "{label} block {index}"
                );
                assert_eq!(
                    actual.light(index),
                    expected.light(index),
                    "{label} light {index}"
                );
            }
            for index in 0..64 {
                assert_eq!(
                    actual.biomes.get(index),
                    expected.biomes.get(index),
                    "{label} biome {index}"
                );
            }
        }
        let encoded = save::encode_with(
            &restored,
            session.game.pack.assets.registry,
            session.game.res.biomes(),
            session.game.save.tables.get().map(Arc::as_ref),
            LevelShape::OVERWORLD,
        );
        for (before, after) in list(saved.tag.get("sections"))
            .unwrap()
            .iter()
            .zip(list(encoded.tag.get("sections")).unwrap())
        {
            for key in ["SkyLight", "BlockLight"] {
                assert_eq!(
                    before.get(key),
                    after.get(key),
                    "{label} exact native {key}"
                );
            }
        }
    }
    // Use an actual generated nondefault state, emitted through the pinned
    // native writer. Default-state strings alone cannot exercise this branch.
    let (palette_at, palette_column, section_index, palette_index) = image
        .columns
        .iter()
        .find_map(|(at, chunk)| {
            let column = save::encode_with(
                chunk,
                session.game.pack.assets.registry,
                session.game.res.biomes(),
                session.game.save.tables.get().map(Arc::as_ref),
                LevelShape::OVERWORLD,
            );
            let found = list(column.tag.get("sections"))
                .unwrap()
                .iter()
                .enumerate()
                .find_map(|(section_index, section)| {
                    list(
                        section
                            .get("block_states")
                            .and_then(|states| states.get("palette")),
                    )
                    .unwrap()
                    .iter()
                    .position(|state| matches!(state, Tag::Compound(_)))
                    .map(|palette_index| (section_index, palette_index))
                });
            found.map(|(section_index, palette_index)| (*at, column, section_index, palette_index))
        })
        .expect("actual TestTerrain nondefault compound palette state");
    let emitted = &list(
        list(palette_column.tag.get("sections")).unwrap()[section_index]
            .get("block_states")
            .and_then(|states| states.get("palette")),
    )
    .unwrap()[palette_index];
    assert!(matches!(emitted.get("id"), Some(Tag::String(_))));
    assert!(matches!(emitted.get("properties"), Some(Tag::Compound(values)) if !values.is_empty()));
    assert_eq!(
        horizon_world::chunk::BlockState::from_nbt(emitted)
            .unwrap()
            .to_nbt(),
        *emitted
    );
    assert!(
        validate_column(
            &session.game,
            &palette_column,
            palette_at,
            session.game.world.game_time
        )
        .is_ok()
    );
    for case in 0..5 {
        let mut modified = palette_column.clone();
        let Some(Tag::List(sections)) = modified.tag.get_mut("sections") else {
            unreachable!()
        };
        let Some(Tag::List(palette)) = sections[section_index]
            .get_mut("block_states")
            .and_then(|states| states.get_mut("palette"))
        else {
            unreachable!()
        };
        let Tag::Compound(fields) = &mut palette[palette_index] else {
            unreachable!()
        };
        match case {
            0 => {
                let id = fields.remove(&"id".into()).unwrap();
                let properties = fields.remove(&"properties".into()).unwrap();
                fields.insert("Name".into(), id);
                fields.insert("Properties".into(), properties);
            }
            1 => {
                fields.insert("id".into(), Tag::Int(0));
            }
            2 => {
                fields.insert("properties".into(), Tag::List(Vec::new()));
            }
            _ => {
                let Some(Tag::Compound(properties)) = fields.get_mut(&"properties".into()) else {
                    unreachable!()
                };
                if case == 3 {
                    properties.insert("unsupported_property".into(), Tag::String("true".into()));
                } else {
                    let key = properties.keys().next().unwrap().clone();
                    properties.insert(key, Tag::Int(1));
                }
            }
        }
        assert!(
            validate_column(
                &session.game,
                &modified,
                palette_at,
                session.game.world.game_time
            )
            .is_err(),
            "native palette shape case {case}"
        );
    }
    for case in 0..8 {
        let mut modified = named.clone();
        let Tag::Compound(root) = &mut modified.tag else {
            unreachable!()
        };
        match case {
            0 => {
                root.insert("LastUpdate".into(), Tag::Int(0));
            }
            1 => {
                root.insert(
                    "LastUpdate".into(),
                    Tag::Long(session.game.world.game_time + 1),
                );
            }
            2 => {
                root.insert(
                    "block_entities".into(),
                    Tag::List(vec![Tag::Compound(Default::default())]),
                );
            }
            3 => {
                root.insert("Status".into(), Tag::String("minecraft:features".into()));
            }
            4 => {
                root.insert("opaque".into(), Tag::Int(0));
            }
            5 => {
                root.insert("sections".into(), Tag::List(Vec::new()));
            }
            6 => {
                root.insert("Heightmaps".into(), Tag::Compound(Default::default()));
            }
            _ => {
                let Some(Tag::List(sections)) = root.get_mut(&"sections".into()) else {
                    unreachable!()
                };
                if let Tag::Compound(section) = &mut sections[0] {
                    section.insert("Y".into(), Tag::Int(-4));
                }
            }
        }
        assert!(
            validate_column(&session.game, &modified, at, session.game.world.game_time).is_err(),
            "native shape case {case}"
        );
    }
    let mut historical = named.clone();
    if let Tag::Compound(root) = &mut historical.tag {
        root.insert("LastUpdate".into(), Tag::Long(0));
        root.insert("InhabitedTime".into(), Tag::Long(900));
    }
    assert!(
        validate_column(&session.game, &historical, at, session.game.world.game_time).is_ok(),
        "old same-version history is not restricted to current/previous tick"
    );
    let row = &image.players.rows()[0];
    for case in 0..4 {
        let mut tag = row.player.clone();
        if let Tag::Compound(fields) = &mut tag {
            match case {
                0 => {
                    fields.insert(
                        "active_effects".into(),
                        Tag::List(vec![Tag::Compound(Default::default())]),
                    );
                }
                1 => {
                    fields.insert("unknown_owner".into(), Tag::Int(1));
                }
                2 => {
                    fields.insert("foodTickTimer".into(), Tag::Long(91));
                }
                _ => {
                    if let Some(Tag::Compound(abilities)) = fields.get_mut(&"abilities".into()) {
                        abilities.insert("flySpeed".into(), Tag::Float(1.0f32.to_bits()));
                    }
                }
            }
        }
        assert!(
            lan::validate_session_player_tag(&tag).is_err(),
            "player shape case {case}"
        );
    }
}

#[test]
#[ignore = "isolated pinned resources + prebuilt pair: actual constructor provenance invalidation, no same-SaveDir or dimension-return reconstruction"]
fn session_actual_native_origin_retirement_and_level_swap_cannot_recreate_eligibility() {
    let fixture = Fixture::new("session-origin");
    let _retain = RetainOnPanic::new(&fixture);
    let mut game = fresh(&fixture);
    let root = fixture
        .game()
        .world_root
        .parent()
        .unwrap()
        .join("native-origin");
    open_native(&mut game, &root);
    let save = game.save.clone();
    let uuid = *game
        .session_lock
        .as_ref()
        .unwrap()
        .expected_world()
        .unwrap();
    assert!(game.world.counter_trial_snapshot().is_none());
    game.retire_counter_trial("world-only-reset-without-counter");
    assert!(game.native_checkpoint_origin.is_none());
    game.save = save;
    assert_eq!(
        game.session_lock.as_ref().unwrap().expected_world(),
        Some(&uuid)
    );
    assert!(game.session_context().is_err());
    let mut request = WorldRequest::new("unused", None, None, false, None)
        .unwrap()
        .with_expected_checkpoint_world(uuid);
    request.save_root = root.clone();
    let prepared = request.prepare(&game.res, &game.pack, &game.options, &game.settings);
    assert!(
        prepared.is_err(),
        "live retained native session lock prevents a concurrent reopen"
    );
    // Release the owning Game, then genuinely reopen under a new native lock.
    drop(game);
    let mut game = fresh(&fixture);
    let mut request = WorldRequest::new("unused", None, None, false, None)
        .unwrap()
        .with_expected_checkpoint_world(uuid);
    request.save_root = root.clone();
    let prepared = request
        .prepare(&game.res, &game.pack, &game.options, &game.settings)
        .unwrap();
    game.finish_open_world(prepared).unwrap();
    assert!(game.native_checkpoint_origin.is_some());
    game.switch_level(Dim::Nether, Vec3::new(8.0, 120.0, 8.0))
        .unwrap();
    assert!(game.native_checkpoint_origin.is_none());
    game.switch_level(Dim::Overworld, Vec3::new(8.0, 120.0, 8.0))
        .unwrap();
    assert!(game.native_checkpoint_origin.is_none());
    assert!(game.session_context().is_err());
}
#[test]
#[ignore = "isolated pinned resources + prebuilt pair: post-counter participant installation errors consume whole Game, no independent host/Game escape"]
fn session_actual_recovery_host_roster_and_mapping_failures_return_no_game() {
    let fixture = Fixture::new("session-roster-install");
    let _retain = RetainOnPanic::new(&fixture);
    let mut session = session(&fixture, "roster", 2, true);
    session
        .game
        .checkpoint_session(&session.archive, 1, until())
        .unwrap();
    let image = tree(&published(&session, 1), &[]).unwrap();
    for case in 0..4 {
        let mut candidate = fresh(&fixture);
        let (mut host, _receivers) = make_host(&mut candidate, true, [77, 88], false);
        match case {
            0 => {
                host.hub.guests.pop();
            }
            1 => {
                host.owner.as_mut().unwrap().uuid = EXTERNAL;
            }
            2 => {
                host.hub.guests[1].uuid = 0x777;
            }
            _ => {
                host.hub.guests[1].inventory[0] = Some(ItemStack::new("minecraft:stone", 1));
            }
        }
        let destination = session
            .archive
            .parent()
            .unwrap()
            .join(format!("roster-install-{case}"));
        let result =
            candidate.recover_session_checkpoint(request(&session, destination.clone()), host);
        assert!(
            result.is_err(),
            "roster case {case} cannot return a partially installed Game"
        );
        assert!(
            destination.exists(),
            "case {case} reaches the locked unpublished recovery/install path"
        );
        assert_eq!(tree(&published(&session, 1), &[]).unwrap(), image);
        let lock = save::SessionLock::acquire(&destination).unwrap();
        drop(lock);
    }
}
#[test]
#[ignore = "isolated pinned resources + prebuilt pair: actual native default player fields, modes, signed-zero body success and malformed body refusal"]
fn session_actual_player_defaults_modes_and_signed_zero_are_checked_without_cached_body_authority()
{
    let fixture = Fixture::new("session-player-defaults");
    let _retain = RetainOnPanic::new(&fixture);
    let mut session = session(&fixture, "player-defaults", 2, true);
    let initial = session.game.session_snapshot().unwrap();
    let canonical = &initial.players[0].player;
    assert!(lan::validate_session_player_tag(canonical).is_ok());
    for key in [
        "Invulnerable",
        "PortalCooldown",
        "Score",
        "SleepTimer",
        "HurtTime",
        "DeathTime",
        "FallFlying",
    ] {
        let original = canonical.get(key).unwrap();
        for kind in 0..3 {
            let mut tag = canonical.clone();
            if let Tag::Compound(fields) = &mut tag {
                match kind {
                    0 => {
                        fields.remove(&key.into());
                    }
                    1 => {
                        fields.insert(
                            key.into(),
                            match original {
                                Tag::Byte(_) => Tag::Byte(1),
                                Tag::Short(_) => Tag::Short(1),
                                _ => Tag::Int(1),
                            },
                        );
                    }
                    _ => {
                        fields.insert(key.into(), Tag::Long(0));
                    }
                }
            }
            assert!(
                lan::validate_session_player_tag(&tag).is_err(),
                "default {key} variation {kind}"
            );
        }
    }
    for (mode, flying, valid) in [
        (-1, false, false),
        (4, false, false),
        (99, true, false),
        (0, false, true),
        (1, false, true),
        (2, false, true),
        (3, true, true),
    ] {
        let guest = &mut session.game.lan.as_mut().unwrap().hub.guests[0];
        guest.game_mode = mode;
        guest.flying = flying;
        let body = guest.motion.as_mut().unwrap();
        body.flying = flying;
        body.may_fly = matches!(mode, 1 | 3);
        body.invulnerable = body.may_fly;
        body.spectator = mode == 3;
        let roster = session.game.observe_checkpoint_participants().unwrap();
        let result = session.game.project_session_participant_rows(&roster);
        assert_eq!(result.is_ok(), valid, "actual game mode {mode}");
    }
    let guest = &mut session.game.lan.as_mut().unwrap().hub.guests[0];
    guest.game_mode = 0;
    guest.flying = false;
    let body = guest.motion.as_mut().unwrap();
    body.flying = false;
    body.may_fly = false;
    body.invulnerable = false;
    body.spectator = false;
    body.velocity.x = -0.0;
    body.health.food.exhaustion = -0.0;
    for field in 0..3 {
        let guest = &mut session.game.lan.as_mut().unwrap().hub.guests[0];
        let body = guest.motion.as_mut().unwrap();
        match field {
            0 => body.may_fly = true,
            1 => body.invulnerable = true,
            _ => body.spectator = true,
        };
        let roster = session.game.observe_checkpoint_participants().unwrap();
        assert!(
            session
                .game
                .project_session_participant_rows(&roster)
                .is_err(),
            "actual mode/body flag mismatch {field}"
        );
        let body = session.game.lan.as_mut().unwrap().hub.guests[0]
            .motion
            .as_mut()
            .unwrap();
        match field {
            0 => body.may_fly = false,
            1 => body.invulnerable = false,
            _ => body.spectator = false,
        };
    }
    session
        .game
        .checkpoint_session(&session.archive, 1, until())
        .unwrap();
    let (mut game, receipt, _receivers) =
        restored(&fixture, &session, "signed-zero-recovered", true);
    assert_eq!(total(&game, &receipt.slot), 14);
    let guest = game
        .lan
        .as_ref()
        .unwrap()
        .hub
        .guests
        .iter()
        .find(|guest| guest.uuid == OWNER)
        .unwrap();
    assert_eq!(guest.motion.as_ref().unwrap().velocity.x, 0.0);
    assert_eq!(guest.motion.as_ref().unwrap().health.food.exhaustion, 0.0);

    // A genuine in-frame constructor role and actual native body still load
    // and save their own tracker. No server role is relabelled after creation.
    let mut local = fresh_with_role(&fixture, ExecutionRole::Client);
    assert!(local.options.execution_role.is_client());
    assert!(!local.options.server_thread && local.options.dedicated.is_none());
    local.player = Some(PlayerMotion::new(local.res.motion(), Vec3::new(8.0, 121.0, 8.0)).unwrap());
    let saved = horizon_client::sculk::WardenSpawnTracker {
        ticks_since_last_warning: 31,
        warning_level: 2,
        cooldown_ticks: 7,
    };
    local.level.player_extra = Some(Tag::compound([("warden_spawn_tracker", saved.to_nbt())]));
    assert!(!local.no_player_here() && local.player.is_some());
    assert!(!local.sculk.tracker_loaded);
    local.tick_sculk_entities();
    let mut expected = saved;
    expected.tick();
    assert!(local.sculk.tracker_loaded);
    assert_eq!(local.sculk.tracker, expected);
    local.save_warden_tracker();
    assert_eq!(
        local
            .level
            .player_extra
            .as_ref()
            .unwrap()
            .get("warden_spawn_tracker"),
        Some(&expected.to_nbt())
    );

    // Each invalid counter belongs to a separate actual fresh candidate. Its
    // recovery is consumed and refused, without resetting it to gain eligibility.
    for field in 0..3 {
        let mut candidate = fresh(&fixture);
        assert!(candidate.fresh_session_candidate().is_ok());
        match field {
            0 => candidate.sculk.tracker.ticks_since_last_warning = -1,
            1 => candidate.sculk.tracker.warning_level = -1,
            _ => candidate.sculk.tracker.cooldown_ticks = -1,
        }
        let (host, _receivers) = make_host(&mut candidate, true, [333, 444], false);
        let destination = session
            .archive
            .parent()
            .unwrap()
            .join(format!("invalid-tracker-{field}"));
        let error = candidate
            .recover_session_checkpoint(request(&session, destination.clone()), host)
            .err()
            .expect("negative actual tracker counters refuse a consumed candidate");
        assert!(
            error.detail.contains("unsupported native warden"),
            "{error}"
        );
        assert!(!destination.exists());
    }

    // A real acting guest swaps in its native body and saved metadata. The
    // shared tracker remains the current implementation; only local persistence
    // eligibility changes, and the absent server cache stays untouched afterwards.
    assert!(game.no_player_here() && game.player.is_none());
    assert!(!game.sculk.tracker_loaded);
    let local_cache = game.level.player_extra.clone();
    let previous = tree(&published(&session, 1), &[]).unwrap();
    let mut host = game.lan.take().unwrap();
    let guest = host
        .hub
        .guests
        .iter_mut()
        .find(|guest| guest.uuid == OWNER)
        .unwrap();
    let conn = guest.conn;
    assert!(guest.motion.is_some());
    let saved = horizon_client::sculk::WardenSpawnTracker {
        ticks_since_last_warning: 41,
        warning_level: 1,
        cooldown_ticks: 5,
    };
    let extra = guest
        .saved
        .get_or_insert_with(|| Tag::Compound(Default::default()));
    let Tag::Compound(fields) = extra else {
        unreachable!()
    };
    fields.insert("warden_spawn_tracker".into(), saved.to_nbt());
    let mut expected = saved;
    expected.tick();
    game.as_guest(&mut host, conn, |acting| {
        assert_eq!(acting.lan_acting, Some(conn));
        assert!(acting.player.is_some() && !acting.no_player_here());
        acting.tick_sculk_entities();
        assert!(acting.sculk.tracker_loaded);
        assert_eq!(acting.sculk.tracker, expected);
        acting.save_warden_tracker();
        assert_eq!(
            acting
                .level
                .player_extra
                .as_ref()
                .unwrap()
                .get("warden_spawn_tracker"),
            Some(&expected.to_nbt())
        );
    })
    .expect("actual guest body is installed for its turn");
    assert!(game.no_player_here() && game.player.is_none());
    assert_eq!(game.sculk.tracker, expected);
    assert!(game.sculk.tracker_loaded);
    assert_eq!(game.level.player_extra, local_cache);
    game.save_warden_tracker();
    assert_eq!(
        game.level.player_extra, local_cache,
        "a loaded shared tracker must not contaminate the absent local record"
    );
    assert_eq!(
        host.hub
            .guests
            .iter()
            .find(|guest| guest.conn == conn)
            .unwrap()
            .saved
            .as_ref()
            .unwrap()
            .get("warden_spawn_tracker"),
        Some(&expected.to_nbt())
    );
    game.lan = Some(host);
    let error = game
        .checkpoint_session(&session.archive, 2, until())
        .unwrap_err();
    assert!(
        error.detail.contains("unsupported native warden"),
        "{error}"
    );
    assert_eq!(game.sculk.tracker, expected);
    assert_eq!(total(&game, &receipt.slot), 14);
    assert_eq!(tree(&published(&session, 1), &[]).unwrap(), previous);
}

#[test]
#[ignore = "isolated pinned resources + prebuilt pair: each exact image member hash/absence and trusted host request identity/type correlation"]
fn session_actual_every_member_and_trusted_recovery_context_is_mandatory() {
    let fixture = Fixture::new("session-member-coverage");
    let _retain = RetainOnPanic::new(&fixture);
    let mut session = session(&fixture, "members", 2, true);
    session
        .game
        .checkpoint_session(&session.archive, 1, until())
        .unwrap();
    let baseline = published(&session, 1);
    let inventory = tree(&baseline, &[]).unwrap();
    let copy = session
        .archive
        .parent()
        .unwrap()
        .join("member-control-image");
    copy_tree(&baseline, &copy, &inventory, until()).unwrap();
    let manifest: Manifest = json(
        &fs::read(copy.join("manifest.json")).unwrap(),
        MANIFEST_LIMIT,
    )
    .unwrap();
    for row in &manifest.files {
        let path = copy.join(&row.path);
        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(
            verify_image(
                &session.game,
                &copy,
                1,
                session.counter.placement().world,
                &session.target,
                &session.type_id
            )
            .is_err(),
            "missing {}",
            row.path
        );
        fs::write(&path, &bytes).unwrap();
        let mut changed = bytes.clone();
        if let Some(first) = changed.first_mut() {
            *first ^= 1;
        } else {
            changed.push(1);
        }
        fs::write(&path, changed).unwrap();
        assert!(
            verify_image(
                &session.game,
                &copy,
                1,
                session.counter.placement().world,
                &session.target,
                &session.type_id
            )
            .is_err(),
            "hash changed {}",
            row.path
        );
        fs::write(path, bytes).unwrap();
    }
    assert_eq!(tree(&copy, &[]).unwrap(), inventory);
    let wrong = WorldId::parse("00000000-0000-4000-8000-000000000001").unwrap();
    assert_ne!(wrong, session.counter.placement().world);
    assert!(
        verify_image(
            &session.game,
            &copy,
            1,
            wrong,
            &session.target,
            &session.type_id
        )
        .is_err()
    );
    assert!(
        verify_image(
            &session.game,
            &copy,
            1,
            session.counter.placement().world,
            &CounterTarget::new(Dim::Overworld.id(), [3, 120, 2]).unwrap(),
            &session.type_id
        )
        .is_err()
    );
    assert!(
        verify_image(
            &session.game,
            &copy,
            1,
            session.counter.placement().world,
            &session.target,
            "unknown:machine"
        )
        .is_err()
    );
    let counter_path = copy.join(COUNTER);
    let mut frame: Value = json(&fs::read(&counter_path).unwrap(), STRUCTURED_LIMIT).unwrap();
    frame["format"] = Value::from(99);
    fs::write(counter_path, json_bytes(&frame, STRUCTURED_LIMIT).unwrap()).unwrap();
    reseal(&copy, 1);
    assert!(
        verify_image(
            &session.game,
            &copy,
            1,
            session.counter.placement().world,
            &session.target,
            &session.type_id
        )
        .is_err(),
        "current counter schema only"
    );
    assert_eq!(tree(&baseline, &[]).unwrap(), inventory);
}

#[test]
fn session_tree_budget_reserves_eager_directory_entries_and_unprocessed_siblings_before_reads() {
    let scratch = Scratch::new("entry-budget");
    let flat = scratch.0.join("flat");
    fs::create_dir(&flat).unwrap();
    let bad = File::create(flat.join("a-oversized")).unwrap();
    bad.set_len(FILE_LIMIT as u64 + 1).unwrap();
    drop(bad);
    for i in 0..ENTRY_LIMIT {
        fs::write(flat.join(format!("z-{i:03}")), b"").unwrap();
    }
    let error = tree(&flat, &[]).unwrap_err();
    assert_eq!(
        error.detail, "tree entry bound",
        "reject the 257th enumerated entry before sorting or reading an earlier oversized member"
    );
    let nested = scratch.0.join("nested");
    fs::create_dir(&nested).unwrap();
    fs::create_dir(nested.join("a-child")).unwrap();
    fs::write(nested.join("z-reserved-sibling"), b"").unwrap();
    let bad = File::create(nested.join("a-child/a-oversized")).unwrap();
    bad.set_len(FILE_LIMIT as u64 + 1).unwrap();
    drop(bad);
    for i in 0..254 {
        fs::write(nested.join(format!("a-child/z-{i:03}")), b"").unwrap();
    }
    let error = tree(&nested, &[]).unwrap_err();
    assert_eq!(
        error.detail, "tree entry bound",
        "both root siblings consume the shared budget before child collection"
    );
}
#[test]
#[ignore = "isolated pinned resources + prebuilt pair: dedicated UUID collision refuses before native save and before checked player brand"]
fn session_actual_dedicated_native_uuid_collision_is_never_an_external_participant_owner() {
    let fixture = Fixture::new("session-dedicated-collision");
    let _retain = RetainOnPanic::new(&fixture);
    let mut session = session(&fixture, "dedicated-collision", 2, false);
    let uuid = session.game.level.player_uuid.unwrap();
    let profile = u128::from_be_bytes(
        uuid.into_iter()
            .flat_map(i32::to_be_bytes)
            .collect::<Vec<_>>()
            .try_into()
            .unwrap(),
    );
    let old = session.game.lan.as_ref().unwrap().hub.guests[0].uuid;
    assert_ne!(profile, old);
    session.game.lan.as_mut().unwrap().hub.guests[0].uuid = profile;
    let source = tree(&session.game.world_root, &["session.lock"]).unwrap();
    let error = session
        .game
        .checkpoint_session(&session.archive, 1, until())
        .unwrap_err();
    assert_eq!(error.publication, Publication::NotPublished);
    assert!(error.detail.contains("collides"));
    assert_eq!(
        tree(&session.game.world_root, &["session.lock"]).unwrap(),
        source
    );
    assert!(!published(&session, 1).exists());
    session.game.lan.as_mut().unwrap().hub.guests[0].uuid = old;
    session
        .game
        .checkpoint_session(&session.archive, 1, until())
        .unwrap();
    let original = published(&session, 1);
    let image = session
        .archive
        .parent()
        .unwrap()
        .join("dedicated-collision-image");
    copy_tree(&original, &image, &tree(&original, &[]).unwrap(), until()).unwrap();
    let mut manifest: Manifest = json(
        &fs::read(image.join("manifest.json")).unwrap(),
        MANIFEST_LIMIT,
    )
    .unwrap();
    let before = manifest.players[0].clone();
    manifest.players[0].record_uuid = uuid;
    let after = &manifest.players[0];
    for kind in ["data", "stats", "advancements"] {
        let old = image.join(player_path(&before, kind));
        let new = image.join(player_path(after, kind));
        let mut bytes = fs::read(&old).unwrap();
        if kind == "data" {
            let mut player = compressed_nbt(&bytes).unwrap();
            if let Tag::Compound(fields) = &mut player {
                fields.insert("UUID".into(), Tag::IntArray(uuid.to_vec()));
            }
            bytes = nbt_bytes(&player).unwrap();
        }
        fs::write(new, bytes).unwrap();
        fs::remove_file(old).unwrap();
    }
    fs::write(
        image.join("manifest.json"),
        json_bytes(&manifest, MANIFEST_LIMIT).unwrap(),
    )
    .unwrap();
    reseal(&image, 1);
    let error = match verify_image(
        &session.game,
        &image,
        1,
        session.counter.placement().world,
        &session.target,
        &session.type_id,
    ) {
        Ok(_) => panic!("dedicated local collision granted checked players"),
        Err(error) => error,
    };
    assert!(error.detail.contains("collides"));
}
#[test]
#[ignore = "isolated pinned resources + prebuilt pair: corrupt off-resident source location refuses before checkpoint's native writers can normalize it"]
fn session_actual_corrupt_source_off_resident_header_is_not_hidden_by_native_save() {
    let fixture = Fixture::new("session-source-header");
    let _retain = RetainOnPanic::new(&fixture);
    let mut session = session(&fixture, "source-header", 2, true);
    let path = session.game.save.root.join("region/r.0.0.mca");
    let original = fs::read(&path).unwrap();
    let mut corrupt = original.clone();
    assert_eq!(&corrupt[1023 * 4..1023 * 4 + 4], &[0; 4]);
    corrupt[1023 * 4..1023 * 4 + 4].copy_from_slice(&0x200u32.to_be_bytes());
    fs::write(&path, &corrupt).unwrap();
    let stone = session
        .game
        .pack
        .assets
        .registry
        .default_state("minecraft:stone")
        .unwrap();
    session.game.world.set(3, 121, 2, stone).unwrap();
    let error = session
        .game
        .checkpoint_session(&session.archive, 1, until())
        .unwrap_err();
    assert_eq!(error.publication, Publication::NotPublished);
    assert!(error.detail.contains("location/sector"));
    assert_eq!(
        fs::read(&path).unwrap(),
        corrupt,
        "existing native region writer cannot hide the malformed off-resident location"
    );
    assert!(!published(&session, 1).exists());
    fs::write(path, original).unwrap();
    session
        .game
        .checkpoint_session(&session.archive, 1, until())
        .unwrap();
    assert_eq!(total(&session.game, &session.slot), 14);
}
#[test]
#[ignore = "isolated pinned resources + prebuilt pair: unsupported region backups refuse working capture and resealed complete image validation"]
fn session_actual_region_backups_refuse_capture_and_resealed_complete_images() {
    let fixture = Fixture::new("session-region-backups");
    let _retain = RetainOnPanic::new(&fixture);
    let mut session = session(&fixture, "region-backups", 2, true);
    session
        .game
        .checkpoint_session(&session.archive, 1, until())
        .unwrap();
    let previous = published(&session, 1);
    let previous_tree = tree(&previous, &[]).unwrap();
    let native_region = fs::read(session.game.save.root.join("region/r.0.0.mca")).unwrap();
    for (index, bytes) in [
        b"arbitrary unsupported region backup".to_vec(),
        native_region,
    ]
    .into_iter()
    .enumerate()
    {
        let epoch = index as u64 + 2;
        let backup_name = format!("garbage-{index}.mca_old");
        let working_backup = session.game.save.root.join("region").join(&backup_name);
        fs::write(&working_backup, &bytes).unwrap();
        let error = session
            .game
            .checkpoint_session(&session.archive, epoch, until())
            .unwrap_err();
        assert_eq!(error.publication, Publication::NotPublished);
        assert!(error.detail.contains("unsupported native member"));
        assert_eq!(fs::read(&working_backup).unwrap(), bytes);
        assert!(!published(&session, epoch).exists());
        assert_eq!(tree(&previous, &[]).unwrap(), previous_tree);
        fs::remove_file(working_backup).unwrap();

        let image = session
            .archive
            .parent()
            .unwrap()
            .join(format!("region-backup-image-{index}"));
        copy_tree(&previous, &image, &previous_tree, until()).unwrap();
        let member_path = format!("world/dimensions/minecraft/overworld/region/{backup_name}");
        fs::write(image.join(&member_path), &bytes).unwrap();
        reseal(&image, epoch);
        let manifest: Manifest = json(
            &fs::read(image.join("manifest.json")).unwrap(),
            MANIFEST_LIMIT,
        )
        .unwrap();
        let member = manifest
            .files
            .iter()
            .find(|member| member.path == member_path)
            .unwrap();
        assert_eq!(member.bytes, bytes.len() as u64);
        assert_eq!(member.sha256, digest(&bytes));
        let error = match verify_image(
            &session.game,
            &image,
            epoch,
            session.counter.placement().world,
            &session.target,
            &session.type_id,
        ) {
            Ok(_) => panic!("unsupported region backup granted a checked image"),
            Err(error) => error,
        };
        assert!(error.detail.contains("unsupported native member"));
        assert_eq!(tree(&previous, &[]).unwrap(), previous_tree);
    }
}

// These tests exercise the actual Game receiver. Pointer preservation of the
// original pending payload is separately checked through World's borrowed tests;
// public diagnostic snapshots here intentionally clone their item data.
#[inline(never)]
fn retirement_session<'a>(fixture: &'a Fixture, label: &str, owner: bool) -> Box<Session<'a>> {
    Box::new(session(fixture, label, 2, owner))
}

#[inline(never)]
fn retirement_restored<'a>(
    fixture: &'a Fixture,
    session: &Session<'_>,
    label: &str,
    owner: bool,
) -> (Box<Game<'a>>, RecoveryReceipt, Vec<mpsc::Receiver<Out>>) {
    let (game, receipt, receivers) = restored(fixture, session, label, owner);
    (Box::new(game), receipt, receivers)
}

fn retirement_rich(count: u32) -> ItemStack {
    let mut value = ItemStack::new("minecraft:stone", count);
    value.damage = 7;
    let data = value.data_mut();
    data.custom_name = Some("persistent original owner".into());
    data.bundle_selected.0 = 4;
    data.set_other(
        "example:opaque",
        Some(Tag::compound([("value", Tag::Int(-73))])),
    );
    let mut child = ItemStack::new("minecraft:dirt", 2);
    child.data_mut().bundle_selected.0 = 8;
    data.container.push((2, child.clone()));
    data.charged.push(child.clone());
    data.bundle_contents.push(child.clone());
    data.pot_decorations = Some(Box::new([Some(child), None, None, None]));
    value
}

fn retirement_raw_equal(actual: &ItemStack, expected: &ItemStack) {
    assert_eq!(actual, expected);
    match (actual.data(), expected.data()) {
        (Some(a), Some(b)) => {
            assert_eq!(a.bundle_selected.0, b.bundle_selected.0);
            for ((_, a), (_, b)) in a.container.iter().zip(&b.container) {
                retirement_raw_equal(a, b);
            }
            for (a, b) in a.charged.iter().zip(&b.charged) {
                retirement_raw_equal(a, b);
            }
            for (a, b) in a.bundle_contents.iter().zip(&b.bundle_contents) {
                retirement_raw_equal(a, b);
            }
            if let (Some(a), Some(b)) = (&a.pot_decorations, &b.pot_decorations) {
                for (a, b) in a.iter().flatten().zip(b.iter().flatten()) {
                    retirement_raw_equal(a, b);
                }
            }
        }
        (None, None) => {}
        _ => panic!("raw retirement metadata presence changed"),
    }
}

fn retirement_random(game: &mut Game<'_>) -> [u32; 3] {
    use horizon_core::random::RandomSource;
    let mut random = game.particles.random().clone();
    std::array::from_fn(|_| random.next_float().to_bits())
}

fn retirement_total(game: &Game<'_>) -> u32 {
    let snapshot = game.world.world_slot_snapshot().unwrap();
    snapshot
        .active
        .as_ref()
        .or_else(|| snapshot.pending.as_ref().map(|p| &p.allocation))
        .and_then(|a| a.slot.stack())
        .map_or(0, |s| s.count)
        + game
            .dropped
            .iter()
            .map(|entity| entity.stack.count)
            .sum::<u32>()
        + game
            .lan
            .as_ref()
            .unwrap()
            .hub
            .guests
            .iter()
            .flat_map(|guest| guest.inventory.iter().flatten())
            .map(|s| s.count)
            .sum::<u32>()
}

struct RetirementBefore {
    pending: horizon_client::world_slot_trial::RetirementSnapshot,
    metadata: horizon_client::world::slot_restore::SlotMetadataSnapshot,
    counter: horizon_client::world::counter_restore::DomainSnapshot,
    id: u32,
    random: [u32; 3],
    clock: i64,
    total: u32,
}
impl RetirementBefore {
    fn capture(game: &mut Game<'_>) -> Self {
        Self {
            pending: game.world.world_slot_snapshot().unwrap().pending.unwrap(),
            metadata: game.world.persistent_world_slot_metadata().unwrap(),
            counter: game.world.counter_domain_snapshot().unwrap(),
            id: game.next_entity,
            random: retirement_random(game),
            clock: game.world.game_time,
            total: retirement_total(game),
        }
    }
    fn assert_retained(&self, game: &mut Game<'_>) {
        let pending = game.world.world_slot_snapshot().unwrap().pending.unwrap();
        assert_eq!(pending.allocation.key, self.pending.allocation.key);
        assert_eq!(
            pending.allocation.slot.revision(),
            self.pending.allocation.slot.revision()
        );
        assert_eq!(
            pending.allocation.resident,
            self.pending.allocation.resident
        );
        assert_eq!(
            (pending.reason, pending.game_time),
            (self.pending.reason, self.pending.game_time)
        );
        retirement_raw_equal(
            pending.allocation.slot.stack().unwrap(),
            self.pending.allocation.slot.stack().unwrap(),
        );
        assert_eq!(
            game.world.persistent_world_slot_metadata(),
            Some(self.metadata)
        );
        assert_eq!(
            game.world.counter_domain_snapshot(),
            Some(self.counter.clone())
        );
        assert_eq!(game.next_entity, self.id);
        assert_eq!(retirement_random(game), self.random);
        assert_eq!(game.world.game_time, self.clock);
        assert_eq!(retirement_total(game), self.total);
        assert!(game.dropped.is_empty());
    }
}

fn retirement_replace(game: &mut Game<'_>, rounds: usize) {
    let iron = game
        .pack
        .assets
        .registry
        .default_state("minecraft:iron_block")
        .unwrap();
    for _ in 0..rounds {
        game.world
            .set(POSITION[0], POSITION[1], POSITION[2], game.world.air)
            .unwrap();
        game.world
            .set(POSITION[0], POSITION[1], POSITION[2], iron)
            .unwrap();
    }
}

#[test]
#[ignore = "isolated pinned resources + prebuilt pair: actual persistent Game retirement, rich nested ownership, sink failures and retained dropped owner"]
fn session_actual_persistent_retirement_rich_delivery_and_sink_refusals() {
    use crate::game::slot_retirement::{Delivery, DeliveryError};
    use horizon_client::world::slot_restore::{SlotLifecycleError, SlotStatus};
    let fixture = Fixture::new("persistent-retirement-rich");
    let _retain = RetainOnPanic::new(&fixture);
    for owner in [true, false] {
        let mut boxed = retirement_session(
            &fixture,
            if owner {
                "rich-integrated"
            } else {
                "rich-dedicated"
            },
            owner,
        );
        let session = boxed.as_mut();
        // The fixture's fourteen plain items stay represented. Add one owned
        // seven-item rich stack, then move that WHOLE cell into the machine:
        // no unsupported rich remainder may mask the later ENTITY refusal.
        let slot = session.slot.clone();
        transfer(&mut session.game, &slot, 10, Direction::IntoPlayer, 7);
        assert!(session.game.lan.as_ref().unwrap().hub.guests[0].inventory[1].is_none());
        session.game.lan.as_mut().unwrap().hub.guests[0].inventory[1] = Some(retirement_rich(7));
        let mut host = session.game.lan.take().unwrap();
        let moved = session
            .game
            .as_guest(&mut host, 10, |game| {
                game.transfer_world_slot_inventory_cell(&slot, 1, Direction::IntoMachine, 7)
            })
            .unwrap()
            .unwrap();
        assert!(matches!(moved, TransferOutcome::Committed(row) if row.moved == 7));
        session.game.lan = Some(host);
        assert!(session.game.lan.as_ref().unwrap().hub.guests[0].inventory[1].is_none());
        let expected = session
            .game
            .world
            .read_world_slot(&slot)
            .unwrap()
            .slot
            .stack()
            .unwrap()
            .clone();
        assert_eq!(retirement_total(&session.game), 21);
        retirement_replace(&mut session.game, 3);
        assert_eq!(
            session
                .game
                .world
                .counter_domain_snapshot()
                .unwrap()
                .high_water,
            4
        );
        assert_eq!(
            session
                .game
                .world
                .persistent_world_slot_metadata()
                .unwrap()
                .high_water,
            1
        );
        let before = RetirementBefore::capture(&mut session.game);
        assert_eq!(
            session
                .game
                .deliver_pending_persistent_world_slot_with_reservation(&slot, usize::MAX),
            Err(DeliveryError::Reservation)
        );
        before.assert_retained(&mut session.game);
        let id = session.game.next_entity;
        session.game.next_entity = u32::MAX;
        let exhausted = RetirementBefore::capture(&mut session.game);
        assert_eq!(
            session.game.deliver_pending_persistent_world_slot(&slot),
            Err(DeliveryError::EntityIdExhausted)
        );
        exhausted.assert_retained(&mut session.game);
        session.game.next_entity = id;
        session.game.dim = Dim::Nether;
        assert_eq!(
            session.game.deliver_pending_persistent_world_slot(&slot),
            Err(DeliveryError::WrongContext)
        );
        session.game.dim = Dim::Overworld;
        before.assert_retained(&mut session.game);
        let time = session.game.world.game_time;
        session.game.world.game_time = -1;
        let clock = RetirementBefore::capture(&mut session.game);
        assert_eq!(
            session.game.deliver_pending_persistent_world_slot(&slot),
            Err(DeliveryError::Persistent(SlotLifecycleError::ClockMismatch))
        );
        clock.assert_retained(&mut session.game);
        session.game.world.game_time = time;
        before.assert_retained(&mut session.game);

        assert_eq!(
            session.game.deliver_pending_persistent_world_slot(&slot),
            Ok(Delivery::Delivered { entity_id: id + 1 })
        );
        assert_eq!(session.game.dropped.len(), 1);
        let entity = &session.game.dropped[0];
        assert_eq!(entity.id, id + 1);
        retirement_raw_equal(&entity.stack, &expected);
        assert_eq!(entity.position, Vec3::new(2.5, 120.5, 2.5));
        assert_eq!(retirement_total(&session.game), 21);
        assert_eq!(
            session
                .game
                .world
                .persistent_world_slot_metadata()
                .unwrap()
                .status,
            SlotStatus::Healthy
        );
        assert_eq!(
            session.game.deliver_pending_persistent_world_slot(&slot),
            Err(DeliveryError::NoPending)
        );
        let fresh = session
            .game
            .world
            .reconcile_persistent_world_slot()
            .unwrap()
            .unwrap();
        assert_eq!(fresh.incarnation(), 2);
        let error = session
            .game
            .checkpoint_session(&session.archive, 1, until())
            .unwrap_err();
        assert!(error.detail.contains("ENTITY"), "{error}");
        assert!(!published(session, 1).exists());
        assert_eq!(session.game.dropped.len(), 1);
        retirement_raw_equal(&session.game.dropped[0].stack, &expected);
        assert_eq!(retirement_total(&session.game), 21);
        // Keep the genuine ItemEntity in its owning Game through test teardown.
    }
}

#[test]
#[ignore = "isolated pinned resources + prebuilt pair: empty persistent retirement, independent heads, real checkpoint restore recapture and corrupt history refusals"]
fn session_actual_persistent_retirement_empty_reconcile_checkpoint_roundtrip() {
    use crate::game::slot_retirement::Delivery;
    let fixture = Fixture::new("persistent-retirement-empty");
    let _retain = RetainOnPanic::new(&fixture);
    for owner in [true, false] {
        let mut boxed = retirement_session(
            &fixture,
            if owner {
                "empty-integrated"
            } else {
                "empty-dedicated"
            },
            owner,
        );
        let session = boxed.as_mut();
        let old_slot = session.slot.clone();
        let old_counter = session.counter.clone();
        transfer(&mut session.game, &old_slot, 10, Direction::IntoPlayer, 7);
        retirement_replace(&mut session.game, 3);
        assert!(
            session
                .game
                .checkpoint_session(&session.archive, 1, until())
                .is_err()
        );
        let id = session.game.next_entity;
        let random = retirement_random(&mut session.game);
        assert_eq!(
            session
                .game
                .deliver_pending_persistent_world_slot(&old_slot),
            Ok(Delivery::EmptyAcknowledged)
        );
        assert_eq!(session.game.next_entity, id);
        assert_eq!(retirement_random(&mut session.game), random);
        assert!(session.game.dropped.is_empty());
        assert!(
            session
                .game
                .checkpoint_session(&session.archive, 1, until())
                .is_err()
        );
        session.slot = session
            .game
            .world
            .reconcile_persistent_world_slot()
            .unwrap()
            .unwrap();
        session.counter = session
            .game
            .world
            .persistent_counter_key()
            .unwrap()
            .unwrap();
        assert_eq!(session.slot.incarnation(), 2);
        assert!(session.game.world.read_world_slot(&old_slot).is_err());
        assert!(
            session
                .game
                .world
                .read_persistent_counter(&old_counter)
                .is_err()
        );
        let frame = session
            .game
            .world
            .counter_checkpoint_frame(session.game.world.game_time)
            .unwrap();
        assert_eq!(frame.allocation_high_water, 4);
        assert_eq!(frame.last_retired.as_ref().unwrap().generation, 3);
        assert_eq!(
            frame.last_retired.as_ref().unwrap().reason,
            NativeRetirementReason::BlockChanged
        );
        assert_eq!(retirement_total(&session.game), 14);
        session
            .game
            .checkpoint_session(&session.archive, 1, until())
            .unwrap();
        let image = published(session, 1);
        let original_tree = tree(&image, &[]).unwrap();
        for invalid in 0..6 {
            let copy = session
                .archive
                .parent()
                .unwrap()
                .join(format!("invalid-retirement-{owner}-{invalid}"));
            copy_tree(&image, &copy, &original_tree, until()).unwrap();
            let mut changed = frame.clone();
            let retired = changed.last_retired.as_mut().unwrap();
            match invalid {
                0 => retired.reason = NativeRetirementReason::Explicit,
                1 => retired.reason = NativeRetirementReason::ChunkReplaced,
                2 => retired.generation = changed.allocation_high_water,
                3 => retired.game_time = changed.saved_game_time + 1,
                4 => retired.generation = 0,
                _ => {
                    retired.counter = 1;
                    retired.last_advanced = None;
                }
            }
            fs::write(copy.join(COUNTER), serde_json::to_vec(&changed).unwrap()).unwrap();
            reseal(&copy, 1);
            assert!(
                verify_image(
                    &session.game,
                    &copy,
                    1,
                    session.counter.placement().world,
                    &session.target,
                    &session.type_id
                )
                .is_err()
            );
            assert_eq!(tree(&image, &[]).unwrap(), original_tree);
        }
        let (mut recovered, receipt, _receivers) = retirement_restored(
            &fixture,
            session,
            if owner {
                "recovered-empty-integrated"
            } else {
                "recovered-empty-dedicated"
            },
            owner,
        );
        assert_ne!(receipt.slot, session.slot);
        assert_eq!(receipt.slot.incarnation(), 2);
        assert_eq!(
            recovered
                .world
                .counter_domain_snapshot()
                .unwrap()
                .high_water,
            4
        );
        assert_eq!(
            recovered
                .world
                .persistent_world_slot_metadata()
                .unwrap()
                .high_water,
            2
        );
        assert_eq!(
            recovered
                .world
                .counter_checkpoint_frame(recovered.world.game_time)
                .unwrap(),
            frame
        );
        assert!(recovered.world.read_world_slot(&old_slot).is_err());
        assert!(
            recovered
                .world
                .read_persistent_counter(&old_counter)
                .is_err()
        );
        assert_eq!(retirement_total(&recovered), 14);
        recovered.world.game_time += 1;
        recovered.tick_counter_trial(true);
        assert_eq!(
            recovered
                .world
                .read_persistent_counter(&receipt.counter)
                .unwrap()
                .counter(),
            1
        );
        recovered
            .checkpoint_session(&session.archive, 2, until())
            .unwrap();
        assert!(published(session, 2).exists());
        assert_eq!(retirement_total(&recovered), 14);
    }
}

fn session_items_rich() -> ItemStack {
    let mut stack = ItemStack::new("minecraft:stone", 7);
    stack.damage = 7;
    let data = stack.data_mut();
    data.custom_name = Some("native durable dropped owner".into());
    data.set_other(
        "example:opaque",
        Some(Tag::compound([("value", Tag::Long(-73))])),
    );
    let mut child = ItemStack::new("minecraft:dirt", 2);
    child.data_mut().custom_name = Some("nested original".into());
    data.container.push((2, child.clone()));
    data.charged.push(child.clone());
    data.bundle_contents.push(child.clone());
    let mut decoration = child;
    decoration.count = 1; // The native ItemStackTemplate omits this default.
    data.pot_decorations = Some(Box::new([Some(decoration), None, None, None]));
    stack
}
#[inline(never)]
fn session_items_delivered<'a>(
    fixture: &'a Fixture,
    label: &str,
    owner: bool,
    rich: bool,
) -> Box<Session<'a>> {
    use crate::game::slot_retirement::Delivery;
    let mut session = retirement_session(fixture, label, owner);
    session
        .game
        .checkpoint_session(&session.archive, 1, until())
        .unwrap();
    if rich {
        let slot = session.slot.clone();
        transfer(&mut session.game, &slot, 10, Direction::IntoPlayer, 7);
        session.game.lan.as_mut().unwrap().hub.guests[0].inventory[1] = Some(session_items_rich());
        let mut host = session.game.lan.take().unwrap();
        let moved = session
            .game
            .as_guest(&mut host, 10, |game| {
                game.transfer_world_slot_inventory_cell(&slot, 1, Direction::IntoMachine, 7)
            })
            .unwrap()
            .unwrap();
        assert!(matches!(moved, TransferOutcome::Committed(row) if row.moved == 7));
        session.game.lan = Some(host);
    }
    retirement_replace(&mut session.game, 3);
    let original = session.slot.clone();
    assert!(matches!(
        session
            .game
            .deliver_pending_persistent_world_slot(&original),
        Ok(Delivery::Delivered { .. })
    ));
    session.slot = session
        .game
        .world
        .reconcile_persistent_world_slot()
        .unwrap()
        .unwrap();
    session.counter = session
        .game
        .world
        .persistent_counter_key()
        .unwrap()
        .unwrap();
    let entity = &mut session.game.dropped[0];
    entity.health = 4;
    entity.fire_ticks = -7;
    entity.age = 201;
    entity.pickup_delay = 17;
    entity.on_ground = true;
    entity.velocity = Vec3::new(0.125, -0.25, 0.0);
    entity.thrower = Some([7, 6, 5, 4]);
    entity.target = Some([3, 2, 1, 0]);
    assert!(session.game.persist.uuids.is_empty());
    assert!(session.game.persist.raw.is_empty());
    session
}
fn session_items_stack_count(stack: &ItemStack) -> u64 {
    u64::from(stack.count)
        + stack.data().map_or(0, |d| {
            d.container
                .iter()
                .map(|(_, s)| session_items_stack_count(s))
                .sum::<u64>()
                + d.charged
                    .iter()
                    .chain(&d.bundle_contents)
                    .map(session_items_stack_count)
                    .sum::<u64>()
                + d.pot_decorations.as_ref().map_or(0, |p| {
                    p.iter()
                        .flatten()
                        .map(session_items_stack_count)
                        .sum::<u64>()
                })
        })
}
fn session_items_total(game: &Game<'_>, slot: &SlotKey) -> u64 {
    let machine = game.world.read_world_slot(slot).unwrap();
    machine.slot.stack().map_or(0, session_items_stack_count)
        + game
            .dropped
            .iter()
            .map(|e| session_items_stack_count(&e.stack))
            .sum::<u64>()
        + game
            .lan
            .as_ref()
            .unwrap()
            .hub
            .guests
            .iter()
            .flat_map(|g| {
                g.inventory
                    .iter()
                    .chain(&g.armor)
                    .chain(std::iter::once(&g.offhand))
                    .chain(&g.ender_items)
                    .flatten()
            })
            .map(session_items_stack_count)
            .sum::<u64>()
}
fn session_items_assert_refusal_retains(session: &mut Session<'_>, epoch: u64) {
    let items = session.game.dropped.clone();
    let uuids = session.game.persist.uuids.clone();
    let raw = session.game.persist.raw.clone();
    let unmodelled = session.game.persist.session_unmodelled_memory_for_test();
    let portals = session.game.session_item_portal_bytes_for_test();
    let id = session.game.next_entity;
    let clock = (session.game.world.game_time, session.game.world.day_time);
    let random = session.game.world.random.state();
    let files = tree(&session.game.world_root, &["session.lock"]).unwrap();
    let error = session
        .game
        .checkpoint_session(&session.archive, epoch, until())
        .unwrap_err();
    assert_eq!(error.item_identities_assigned, 0, "{error}");
    assert_eq!(error.publication, Publication::NotPublished);
    assert_eq!(session.game.dropped.len(), items.len());
    for (a, b) in session.game.dropped.iter().zip(items) {
        retirement_raw_equal(&a.stack, &b.stack);
        let bits = |p: Vec3| [p.x.to_bits(), p.y.to_bits(), p.z.to_bits()];
        assert_eq!(
            (
                a.id,
                bits(a.position),
                bits(a.previous),
                bits(a.velocity),
                a.health,
                a.fire_ticks,
                a.age,
                a.pickup_delay,
                a.ticks,
                a.bob_offset.to_bits()
            ),
            (
                b.id,
                bits(b.position),
                bits(b.previous),
                bits(b.velocity),
                b.health,
                b.fire_ticks,
                b.age,
                b.pickup_delay,
                b.ticks,
                b.bob_offset.to_bits()
            )
        );
    }
    assert_eq!(session.game.persist.uuids, uuids);
    assert_eq!(session.game.persist.raw, raw);
    assert_eq!(
        session.game.persist.session_unmodelled_memory_for_test(),
        unmodelled
    );
    assert_eq!(session.game.session_item_portal_bytes_for_test(), portals);
    assert_eq!(session.game.next_entity, id);
    assert_eq!(
        (session.game.world.game_time, session.game.world.day_time),
        clock
    );
    assert_eq!(session.game.world.random.state(), random);
    assert_eq!(
        tree(&session.game.world_root, &["session.lock"]).unwrap(),
        files
    );
    assert!(!published(session, epoch).exists());
}

#[test]
#[ignore = "isolated pinned resources + prebuilt pair: genuine retirement drop, native entity writer, both owner roles, rich/plain roundtrip and distant item arrival"]
fn session_items_actual_retirement_native_capture_restore_recapture_both_roles() {
    let fixture = Fixture::new("session-items-native");
    let _retain = RetainOnPanic::new(&fixture);
    for (index, owner, rich) in [
        (0, true, false),
        (1, false, false),
        (2, true, true),
        (3, false, true),
    ] {
        let mut s = session_items_delivered(&fixture, &format!("items-{index}"), owner, rich);
        let total = session_items_total(&s.game, &s.slot);
        assert_eq!(total, if rich { 28 } else { 14 });
        if index == 3 {
            let far = (40, 40);
            s.game.streaming.extra = s
                .game
                .world
                .chunks
                .keys()
                .copied()
                .map(|at| (at, 0))
                .chain([(far, 0)])
                .collect();
            residency(&mut s.game, 0, far);
            s.game.dropped[0].position = Vec3::new(640.5, 120.5, 640.5);
            assert!(s.game.world.chunks.contains_key(&far));
        }
        let id = s.game.dropped[0].id;
        let stack = s.game.dropped[0].stack.clone();
        let random = s.game.world.random.state();
        let clocks = (s.game.world.game_time, s.game.world.day_time);
        let previous = tree(&published(&s, 1), &[]).unwrap();
        let error = s
            .game
            .checkpoint_session_with_test_fault(&s.archive, 2, until(), CheckpointTestFault::Copied)
            .unwrap_err();
        assert_eq!(
            (error.item_identities_assigned, error.publication),
            (1, Publication::NotPublished)
        );
        assert!(error.to_string().contains("1 item identities assigned"));
        let before = s.game.project_session_checkpoint_items().unwrap();
        let memory = s.game.session_checkpoint_item_memory().unwrap();
        let uuid = s.game.persist.uuids[&id];
        assert_eq!(tree(&published(&s, 1), &[]).unwrap(), previous);
        assert_eq!(s.game.dropped[0].id, id);
        retirement_raw_equal(&s.game.dropped[0].stack, &stack);
        s.game.checkpoint_session(&s.archive, 3, until()).unwrap();
        assert_eq!(s.game.project_session_checkpoint_items().unwrap(), before);
        assert_eq!(s.game.session_checkpoint_item_memory().unwrap(), memory);
        assert_eq!(s.game.world.random.state(), random);
        assert_eq!((s.game.world.game_time, s.game.world.day_time), clocks);
        let image = published(&s, 3);
        let manifest: Manifest = json(
            &read(&image.join("manifest.json"), MANIFEST_LIMIT).unwrap(),
            MANIFEST_LIMIT,
        )
        .unwrap();
        assert_eq!(manifest.format, 1);
        assert!(manifest.files.iter().any(|r| {
            r.path
                .starts_with("world/dimensions/minecraft/overworld/entities/")
                && r.path.ends_with(".mca")
        }));
        assert!(
            !manifest
                .files
                .iter()
                .any(|r| r.path.contains("drops") || r.path.contains("journal"))
        );
        let (mut recovered, receipt, receivers) =
            retirement_restored(&fixture, &s, &format!("items-restored-{index}"), owner);
        assert_eq!(
            recovered.project_session_checkpoint_items().unwrap(),
            before
        );
        assert_eq!(session_items_total(&recovered, &receipt.slot), total);
        assert_eq!(
            recovered
                .persist
                .uuids
                .values()
                .filter(|u| **u == uuid)
                .count(),
            1
        );
        assert!(recovered.world.loaded_entities.is_empty());
        let e = &recovered.dropped[0];
        assert_eq!(
            (e.previous, e.ticks, e.bob_offset.to_bits()),
            (e.position, 0, 0)
        );
        retirement_raw_equal(&e.stack, &stack);
        recovered
            .checkpoint_session(&s.archive, 4, until())
            .unwrap();
        assert_eq!(
            recovered.project_session_checkpoint_items().unwrap(),
            before
        );
        drop(recovered);
        drop(receivers);
        let (again, receipt, _receivers) =
            retirement_restored(&fixture, &s, &format!("items-again-{index}"), owner);
        assert_eq!(again.project_session_checkpoint_items().unwrap(), before);
        assert_eq!(session_items_total(&again, &receipt.slot), total);
        assert_eq!(s.game.project_session_checkpoint_items().unwrap(), before);
    }
}

#[test]
#[ignore = "isolated pinned resources + prebuilt pair: actual private portal cooldown/process/orphan/sample refusal and matched-default roundtrip without cleanup ticks"]
fn session_items_actual_private_portal_owners_before_uuid_and_after_native_save() {
    use horizon_client::nether_portal::{PortalState, Process};
    use horizon_core::position::BlockPos;
    let fixture = Fixture::new("session-items-portals");
    let _retain = RetainOnPanic::new(&fixture);
    let mut s = session_items_delivered(&fixture, "item-portals", true, true);
    let id = s.game.dropped[0].id;
    // This writes the real private map. The native Base projection still emits 0.
    s.game.set_portal_cooldown(id);
    let native =
        horizon_client::entity_nbt::item_entity_to_nbt(&s.game.dropped[0], [1, 2, 3, 4], None);
    assert_eq!(native.get("PortalCooldown"), Some(&Tag::Int(0)));
    session_items_assert_refusal_retains(&mut s, 2);
    s.game.set_session_item_portal_for_test(id, None, None);
    let mut states = vec![PortalState {
        cooldown: -1,
        process: None,
    }];
    for inside in [false, true] {
        for time in [0, 7] {
            states.push(PortalState {
                cooldown: 0,
                process: Some(Process {
                    entry: BlockPos::new(2, 120, 2),
                    time,
                    inside,
                }),
            });
        }
    }
    for state in states {
        s.game
            .set_session_item_portal_for_test(id, Some(state), None);
        session_items_assert_refusal_retains(&mut s, 2);
    }
    s.game.set_session_item_portal_for_test(id, None, None);
    for state in [
        PortalState::default(),
        PortalState {
            cooldown: 1,
            process: None,
        },
    ] {
        s.game
            .set_session_item_portal_for_test(id + 99, Some(state), None);
        session_items_assert_refusal_retains(&mut s, 2);
        s.game.set_session_item_portal_for_test(id + 99, None, None);
    }
    for (key, position) in [(id + 99, Vec3::ZERO), (id, Vec3::new(f64::NAN, 0.0, 0.0))] {
        s.game
            .set_session_item_portal_for_test(key, None, Some(position));
        session_items_assert_refusal_retains(&mut s, 2);
        s.game.set_session_item_portal_for_test(key, None, None);
    }
    s.game.set_session_item_portal_for_test(
        id,
        Some(PortalState::default()),
        Some(Vec3::new(-0.0, 119.25, 2.0)),
    );
    let before = s.game.session_item_portal_bytes_for_test();
    s.game.checkpoint_session(&s.archive, 2, until()).unwrap();
    assert_eq!(s.game.session_item_portal_bytes_for_test(), before);
    let projection = s.game.project_session_checkpoint_items().unwrap();
    let (mut recovered, _, _receivers) =
        retirement_restored(&fixture, &s, "item-portal-recovery", true);
    assert_eq!(
        recovered.project_session_checkpoint_items().unwrap(),
        projection
    );
    assert!(
        recovered
            .portals
            .project_session_item_portal_owners(&recovered.dropped)
            .is_ok()
    );
    recovered
        .checkpoint_session(&s.archive, 3, until())
        .unwrap();
    assert_eq!(s.game.session_item_portal_bytes_for_test(), before);
}

#[test]
#[ignore = "isolated pinned resources + prebuilt pair: recursive drop and each participant owner selection refusal without item/map/file mutation"]
fn session_items_actual_runtime_bundle_selection_refuses_each_owner_graph() {
    let fixture = Fixture::new("session-items-selection");
    let _retain = RetainOnPanic::new(&fixture);
    let mut s = session_items_delivered(&fixture, "item-selection", true, true);
    let original = s.game.dropped[0].stack.clone();
    for nested in [false, true] {
        if nested {
            s.game.dropped[0].stack.data_mut().container[0]
                .1
                .data_mut()
                .bundle_selected
                .0 = 3;
        } else {
            s.game.dropped[0].stack.data_mut().bundle_selected.0 = 3;
        }
        session_items_assert_refusal_retains(&mut s, 2);
        s.game.dropped[0].stack = original.clone();
    }
    for player in 0..2 {
        for location in 0..4 {
            for nested in [false, true] {
                let guest = &mut s.game.lan.as_mut().unwrap().hub.guests[player];
                let cell = match location {
                    0 => &mut guest.inventory[8],
                    1 => &mut guest.armor[0],
                    2 => &mut guest.offhand,
                    _ => &mut guest.ender_items[0],
                };
                let mut stack = session_items_rich();
                if nested {
                    stack.data_mut().bundle_contents[0]
                        .data_mut()
                        .bundle_selected
                        .0 = 9;
                } else {
                    stack.data_mut().bundle_selected.0 = 9;
                }
                *cell = Some(stack.clone());
                session_items_assert_refusal_retains(&mut s, 2);
                let guest = &mut s.game.lan.as_mut().unwrap().hub.guests[player];
                let cell = match location {
                    0 => &mut guest.inventory[8],
                    1 => &mut guest.armor[0],
                    2 => &mut guest.offhand,
                    _ => &mut guest.ender_items[0],
                };
                retirement_raw_equal(cell.as_ref().unwrap(), &stack);
                *cell = None;
            }
        }
    }
    s.game.checkpoint_session(&s.archive, 2, until()).unwrap();
}

#[test]
#[ignore = "isolated pinned resources + prebuilt pair: canonical nested template count1 survives all actual player owner locations and native dropped graph"]
fn session_items_actual_template_one_defaults_survive_player_and_drop_roundtrip() {
    let fixture = Fixture::new("session-items-template-one");
    let _retain = RetainOnPanic::new(&fixture);
    let mut s = session_items_delivered(&fixture, "item-template-one", true, true);
    let mut stack = ItemStack::new("minecraft:stone", 2);
    let child = ItemStack::new("minecraft:dirt", 1);
    let data = stack.data_mut();
    data.container.push((4, child.clone()));
    data.charged.push(child.clone());
    data.bundle_contents.push(child.clone());
    data.pot_decorations = Some(Box::new([Some(child), None, None, None]));
    for guest in &mut s.game.lan.as_mut().unwrap().hub.guests {
        guest.inventory[8] = Some(stack.clone());
        guest.armor[0] = Some(stack.clone());
        guest.offhand = Some(stack.clone());
        guest.ender_items[0] = Some(stack.clone());
    }
    let total = session_items_total(&s.game, &s.slot);
    assert_eq!(total, 28 + 8 * 6);
    s.game.checkpoint_session(&s.archive, 2, until()).unwrap();
    let (game, receipt, _receivers) =
        retirement_restored(&fixture, &s, "item-template-one-recovery", true);
    assert_eq!(session_items_total(&game, &receipt.slot), total);
    for guest in &game.lan.as_ref().unwrap().hub.guests {
        for actual in [
            &guest.inventory[8],
            &guest.armor[0],
            &guest.offhand,
            &guest.ender_items[0],
        ] {
            retirement_raw_equal(actual.as_ref().unwrap(), &stack);
        }
    }
    assert_eq!(
        game.project_session_checkpoint_items().unwrap(),
        s.game.project_session_checkpoint_items().unwrap()
    );
}

fn session_items_second_delivery(s: &mut Session<'_>) {
    use crate::game::slot_retirement::Delivery;
    let slot = s.slot.clone();
    transfer(&mut s.game, &slot, 10, Direction::IntoMachine, 1);
    retirement_replace(&mut s.game, 1);
    assert!(matches!(
        s.game.deliver_pending_persistent_world_slot(&slot),
        Ok(Delivery::Delivered { .. })
    ));
    s.slot = s
        .game
        .world
        .reconcile_persistent_world_slot()
        .unwrap()
        .unwrap();
    s.counter = s.game.world.persistent_counter_key().unwrap().unwrap();
    assert_eq!(s.game.dropped.len(), 2);
}

#[test]
#[ignore = "isolated pinned resources + prebuilt pair: staged UUID collision exhaustion, duplicate/orphan/raw/unknown/anonymous ownership and exact refusal retention"]
fn session_items_actual_identity_and_memory_preflight_is_all_or_nothing() {
    let fixture = Fixture::new("session-items-identities");
    let _retain = RetainOnPanic::new(&fixture);
    let mut s = session_items_delivered(&fixture, "item-identities", true, false);
    session_items_second_delivery(&mut s);
    let reserved = s.game.session_checkpoint_identity_preflight().unwrap();
    let random = s.game.world.random.state();
    let mut calls = 0;
    let error = s
        .game
        .prepare_session_item_identities_with_test_entropy(|| {
            calls += 1;
            [31, 32, 33, 34]
        })
        .unwrap_err();
    assert!(error.detail.contains("collision exhaustion"));
    assert_eq!(calls, 33);
    assert!(s.game.persist.uuids.is_empty());
    calls = 0;
    assert!(
        s.game
            .prepare_session_item_identities_with_test_entropy(|| {
                calls += 1;
                reserved[0]
            })
            .is_err()
    );
    assert_eq!(calls, 32);
    assert!(s.game.persist.uuids.is_empty());
    assert_eq!(s.game.world.random.state(), random);
    let ids = [s.game.dropped[0].id, s.game.dropped[1].id];
    s.game.dropped[1].id = ids[0];
    session_items_assert_refusal_retains(&mut s, 2);
    s.game.dropped[1].id = ids[1];
    for anonymous in [false, true] {
        s.game
            .persist
            .set_session_unmodelled_memory_for_test(anonymous, true);
        session_items_assert_refusal_retains(&mut s, 2);
        s.game
            .persist
            .set_session_unmodelled_memory_for_test(anonymous, false);
    }
    s.game.persist.uuids.insert(ids[0] + 100, [1, 2, 3, 4]);
    session_items_assert_refusal_retains(&mut s, 2);
    s.game.persist.uuids.remove(&(ids[0] + 100));
    s.game.persist.raw.insert(
        ids[0],
        Tag::compound([("id", Tag::String("minecraft:item".into()))]),
    );
    session_items_assert_refusal_retains(&mut s, 2);
    s.game.persist.raw.clear();
    assert_eq!(
        s.game.prepare_session_checkpoint_item_identities().unwrap(),
        2
    );
    let uuids = [s.game.persist.uuids[&ids[0]], s.game.persist.uuids[&ids[1]]];
    for collision in [uuids[0], reserved[0]] {
        s.game.persist.uuids.insert(ids[1], collision);
        session_items_assert_refusal_retains(&mut s, 2);
    }
    s.game.persist.uuids.insert(ids[1], uuids[1]);
    let mut raw =
        horizon_client::entity_nbt::item_entity_to_nbt(&s.game.dropped[0], uuids[0], None);
    if let Tag::Compound(fields) = &mut raw {
        fields.insert("UUID".into(), Tag::IntArray(vec![0, 0, 0, 0]));
    }
    s.game.persist.raw.insert(ids[0], raw);
    session_items_assert_refusal_retains(&mut s, 2);
    s.game.persist.raw.clear();
    // Supported raw modelled fields may legitimately be stale after live edits.
    let stale = horizon_client::entity_nbt::item_entity_to_nbt(&s.game.dropped[0], uuids[0], None);
    s.game.persist.raw.insert(ids[0], stale);
    s.game.dropped[0].position.x += 0.25;
    s.game.dropped[0].stack.count += 1;
    s.game.checkpoint_session(&s.archive, 2, until()).unwrap();
    assert_eq!(s.game.persist.uuids[&ids[0]], uuids[0]);
    assert_eq!(s.game.persist.uuids[&ids[1]], uuids[1]);
}

#[test]
#[ignore = "isolated pinned resources + prebuilt pair: live item scalar/graph/component/count/entity limits refuse before identity preparation"]
fn session_items_actual_live_profile_bounds_and_codec_coercions_refuse_without_writes() {
    let fixture = Fixture::new("session-items-bounds");
    let _retain = RetainOnPanic::new(&fixture);
    let mut s = session_items_delivered(&fixture, "item-bounds", true, true);
    let original = s.game.dropped[0].clone();
    for case in 0..18 {
        match case {
            0 => s.game.dropped[0].health = i32::from(i16::MAX) + 1,
            1 => s.game.dropped[0].fire_ticks = i32::from(i16::MIN) - 1,
            2 => s.game.dropped[0].age = 32768,
            3 => s.game.dropped[0].pickup_delay = 32768,
            4 => s.game.dropped[0].velocity.x = f64::INFINITY,
            5 => s.game.dropped[0].position.z = f64::NAN,
            6 => s.game.dropped[0].stack.count = 0,
            7 => s.game.dropped[0].stack.damage = u32::MAX,
            8 => s.game.dropped[0].stack.item = Arc::from("fixture:missing"),
            9 => {
                let d = s.game.dropped[0].stack.data_mut();
                d.container.push(d.container[0].clone());
            }
            10 => {
                let d = s.game.dropped[0].stack.data_mut();
                d.other.push(d.other[0].clone());
            }
            11 => s.game.dropped[0]
                .stack
                .data_mut()
                .set_other("minecraft:attribute_modifiers", Some(Tag::List(vec![]))),
            12 => s.game.dropped[0].stack.data_mut().custom_name = Some("x".repeat(4097)),
            13 => {
                let mut stack = ItemStack::new("minecraft:stone", 1);
                for _ in 0..33 {
                    let mut parent = ItemStack::new("minecraft:stone", 1);
                    parent.data_mut().bundle_contents.push(stack);
                    stack = parent;
                }
                s.game.dropped[0].stack = stack;
            }
            14 => {
                s.game.dropped[0].stack.data_mut().bundle_contents =
                    vec![ItemStack::new("minecraft:dirt", 1); 16_385]
            }
            15 => s.game.dropped.extend(vec![original.clone(); 1024]),
            16 => s.game.dropped[0].velocity.z = -0.0,
            _ => s.game.dropped[0]
                .stack
                .data_mut()
                .set_other("example:zero", Some(Tag::Double((-0.0f64).to_bits()))),
        }
        session_items_assert_refusal_retains(&mut s, 2);
        s.game.dropped.truncate(1);
        s.game.dropped[0] = original.clone();
    }
    s.game.checkpoint_session(&s.archive, 2, until()).unwrap();
}

#[test]
#[ignore = "isolated pinned resources + prebuilt pair: actual recovery queue missing/extra/duplicate/reordered owners, missing resident and checked ID exhaustion before publication"]
fn session_items_actual_fresh_installer_refusals_retain_queue_ids_and_random() {
    let fixture = Fixture::new("session-items-installer");
    let _retain = RetainOnPanic::new(&fixture);
    let mut s = session_items_delivered(&fixture, "item-installer", true, true);
    session_items_second_delivery(&mut s);
    s.game.checkpoint_session(&s.archive, 2, until()).unwrap();
    let expected = s.game.project_session_checkpoint_items().unwrap();
    let candidate = Box::new(fresh(&fixture));
    let destination = s.archive.parent().unwrap().join("item-installer-recovery");
    let mut parts = candidate
        .prepare_session_checkpoint_before_items(request(&s, destination), |game, image| {
            let original = game.world.loaded_entities.clone();
            assert_eq!(original.iter().map(|(_, v)| v.len()).sum::<usize>(), 2);
            let head = game.next_entity;
            let random = game.world.random.state();
            assert!(
                game.check_session_item_install_reservation_for_test(image, usize::MAX)
                    .is_err()
            );
            assert_eq!(game.world.loaded_entities, original);
            assert_eq!(game.next_entity, head);
            assert!(
                game.dropped.is_empty()
                    && game.persist.uuids.is_empty()
                    && game.persist.raw.is_empty()
            );
            for case in 0..6 {
                game.world.loaded_entities = original.clone();
                match case {
                    0 => {
                        game.world.loaded_entities[0].1.pop();
                    }
                    1 => {
                        let extra = game.world.loaded_entities[0].1[0].clone();
                        game.world.loaded_entities[0].1.push(extra);
                    }
                    2 => {
                        let mut extra = game.world.loaded_entities[0].1[0].clone();
                        if let Tag::Compound(fields) = &mut extra {
                            fields.insert("UUID".into(), Tag::IntArray(vec![9, 8, 7, 6]));
                        }
                        game.world.loaded_entities[0].1.push(extra);
                    }
                    3 => game.next_entity = u32::MAX,
                    4 => {
                        let extra = game.world.loaded_entities[0].clone();
                        game.world.loaded_entities.push(extra);
                    }
                    _ => {
                        let item = game.world.loaded_entities[0].1[0].clone();
                        game.persist.raw.insert(77, item);
                    }
                }
                let queue = game.world.loaded_entities.clone();
                let id = game.next_entity;
                let raw = game.persist.raw.clone();
                assert!(
                    game.install_session_checkpoint_items(image).is_err(),
                    "case {case}"
                );
                assert!(game.dropped.is_empty());
                assert!(game.persist.uuids.is_empty());
                assert_eq!(game.persist.raw, raw);
                assert_eq!(game.world.loaded_entities, queue);
                assert_eq!(game.next_entity, id);
                assert_eq!(game.world.random.state(), random);
                game.next_entity = head;
                game.persist.raw.clear();
            }
            game.world.loaded_entities = original;
            let key = *image.columns().iter().next().unwrap();
            let chunk = game.world.chunks.remove(&key).unwrap();
            let queue = game.world.loaded_entities.clone();
            assert!(game.install_session_checkpoint_items(image).is_err());
            assert!(game.dropped.is_empty());
            assert_eq!(game.world.loaded_entities, queue);
            assert_eq!(game.next_entity, head);
            game.world.chunks.insert(key, chunk);
            game.world.loaded_entities.reverse();
            for (_, tags) in &mut game.world.loaded_entities {
                tags.reverse();
            }
            image.check_arrivals(
                game,
                &game.world.chunks.keys().copied().collect(),
                &game.world.loaded_entities,
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        parts.game.project_session_checkpoint_items().unwrap(),
        expected
    );
    assert!(parts.game.world.loaded_entities.is_empty());
    let (host, _receivers) = make_host(&mut parts.game, true, [110, 220], false);
    parts
        .game
        .install_session_checkpoint_participants(host, &parts.verified.players)
        .unwrap();
    let (mut game, _) = parts.finish().unwrap();
    let before = game.project_session_checkpoint_items().unwrap();
    // A second attempt cannot install the image into an already-owning Game.
    let image = verify_image(
        &game,
        &published(&s, 2),
        2,
        s.counter.placement().world,
        &s.target,
        &s.type_id,
    )
    .unwrap();
    assert!(game.install_session_checkpoint_items(&image.items).is_err());
    assert_eq!(game.project_session_checkpoint_items().unwrap(), before);
}

fn session_items_edit_native_record(path: &Path, change: impl FnOnce(&mut NamedTag, ChunkPos)) {
    let mut region = RegionFile::open_writable(path, false).unwrap();
    let coordinate = region.chunks().next().unwrap().0;
    let original = region.read(coordinate).unwrap().unwrap();
    let mut named = raw_nbt(
        &original
            .compression
            .decode(&original.compressed, STRUCTURED_LIMIT)
            .unwrap(),
    )
    .unwrap();
    change(&mut named, coordinate);
    let record =
        horizon_world::region::ChunkRecord::from_nbt(&named, Compression::Zlib, 0, limits())
            .unwrap();
    region.write(coordinate, &record).unwrap();
}
fn session_items_mutate_record(named: &mut NamedTag, at: ChunkPos, case: usize) {
    let Tag::Compound(root) = &mut named.tag else {
        panic!("record compound")
    };
    match case {
        0 => {
            root.insert("DataVersion".into(), Tag::Int(DATA_VERSION - 1));
        }
        1 => {
            root.insert("Position".into(), Tag::IntArray(vec![at.x + 1, at.z]));
        }
        2 => {
            root.insert("unmodelled_owner".into(), Tag::Byte(0));
        }
        _ => {
            let Some(Tag::List(entities)) = root.get_mut(&"Entities".into()) else {
                panic!("record list")
            };
            if case == 3 {
                entities.push(entities[0].clone());
                return;
            }
            let Tag::Compound(item) = &mut entities[0] else {
                panic!("item compound")
            };
            match case {
                4 => {
                    item.insert("id".into(), Tag::String("minecraft:pig".into()));
                }
                5 => {
                    item.insert("Passengers".into(), Tag::List(vec![]));
                }
                6 => {
                    item.insert("PortalCooldown".into(), Tag::Int(1));
                }
                7 => {
                    item.insert("Health".into(), Tag::Int(5));
                }
                8 => {
                    item.insert("UUID".into(), Tag::IntArray(vec![1, 2, 3]));
                }
                9 => {
                    item.insert(
                        "Pos".into(),
                        Tag::List(vec![
                            Tag::Double(1024f64.to_bits()),
                            Tag::Double(120f64.to_bits()),
                            Tag::Double(0),
                        ]),
                    );
                }
                10 => {
                    item.remove(&"Motion".into());
                }
                11 => {
                    item.insert("fall_distance".into(), Tag::Double((-0.0f64).to_bits()));
                }
                _ => {
                    let child = item
                        .get_mut(&"Item".into())
                        .and_then(|v| v.get_mut("components"))
                        .and_then(|v| v.get_mut("minecraft:pot_decorations"))
                        .and_then(|v| v.get_mut("back"))
                        .unwrap();
                    let Tag::Compound(child) = child else {
                        unreachable!()
                    };
                    child.insert(
                        "count".into(),
                        if case == 12 {
                            Tag::Byte(1)
                        } else {
                            Tag::Int(0)
                        },
                    );
                }
            }
        }
    }
}

#[test]
#[ignore = "isolated pinned resources + prebuilt pair: every source entity record and resealed image strictly validated before writer normalization or native opening"]
fn session_items_actual_source_and_resealed_entity_schema_refuse_before_normalization() {
    let fixture = Fixture::new("session-items-records");
    let _retain = RetainOnPanic::new(&fixture);
    let mut s = session_items_delivered(&fixture, "item-records", true, true);
    s.game.checkpoint_session(&s.archive, 2, until()).unwrap();
    let native = s.game.save.root.join("entities/r.0.0.mca");
    let bytes = fs::read(&native).unwrap();
    let image = published(&s, 2);
    let inventory = tree(&image, &[]).unwrap();
    for case in 0..14 {
        session_items_edit_native_record(&native, |tag, at| {
            session_items_mutate_record(tag, at, case)
        });
        session_items_assert_refusal_retains(&mut s, 3);
        fs::write(&native, &bytes).unwrap();
        let copied = s
            .archive
            .parent()
            .unwrap()
            .join(format!("items-invalid-image-{case}"));
        copy_tree(&image, &copied, &inventory, until()).unwrap();
        let path = copied.join("world/dimensions/minecraft/overworld/entities/r.0.0.mca");
        session_items_edit_native_record(&path, |tag, at| {
            session_items_mutate_record(tag, at, case)
        });
        reseal(&copied, 2);
        assert!(
            verify_image(
                &s.game,
                &copied,
                2,
                s.counter.placement().world,
                &s.target,
                &s.type_id
            )
            .is_err(),
            "resealed {case}"
        );
    }
    let live = s.game.project_session_checkpoint_items().unwrap();
    // Use the actual native writer for valid stale disk history, including a
    // resident column with no current live owner. Capture must overwrite/delete
    // these records through that same writer rather than filtering its copy.
    let mut stale = live.rows.values().next().unwrap().tag.clone();
    let Some(Tag::Compound(item)) = stale.get_mut("Item") else {
        unreachable!()
    };
    item.insert("count".into(), Tag::Int(99));
    let mut orphan = stale.clone();
    if let Tag::Compound(fields) = &mut orphan {
        fields.insert("UUID".into(), Tag::IntArray(vec![97, 98, 99, 100]));
        fields.insert(
            "Pos".into(),
            Tag::List(vec![
                Tag::Double(16.5f64.to_bits()),
                Tag::Double(120.5f64.to_bits()),
                Tag::Double(2.5f64.to_bits()),
            ]),
        );
    }
    assert!(s.game.world.chunks.contains_key(&(1, 0)));
    s.game
        .save
        .save_entities(vec![((0, 0), vec![stale]), ((1, 0), vec![orphan])])
        .unwrap();
    s.game.checkpoint_session(&s.archive, 3, until()).unwrap();
    assert_eq!(s.game.project_session_checkpoint_items().unwrap(), live);
    assert_eq!(
        verify_image(
            &s.game,
            &published(&s, 3),
            3,
            s.counter.placement().world,
            &s.target,
            &s.type_id
        )
        .unwrap()
        .items
        .projection,
        live
    );
    assert!(s.game.save.load_entities(1, 0).is_empty());
    assert_eq!(tree(&image, &[]).unwrap(), inventory);
}

#[test]
#[ignore = "isolated pinned resources + prebuilt pair: raw entity sectors/compression/length/trailing data and off-resident records refuse in source and resealed images"]
fn session_items_actual_raw_regions_and_off_resident_records_are_never_skipped() {
    let fixture = Fixture::new("session-items-physical");
    let _retain = RetainOnPanic::new(&fixture);
    let mut s = session_items_delivered(&fixture, "item-physical", true, false);
    s.game.checkpoint_session(&s.archive, 2, until()).unwrap();
    let native = s.game.save.root.join("entities/r.0.0.mca");
    let original = fs::read(&native).unwrap();
    let location = u32::from_be_bytes(original[..4].try_into().unwrap());
    let offset = (location >> 8) as usize * 4096;
    let image = published(&s, 2);
    let inventory = tree(&image, &[]).unwrap();
    for case in 0..8 {
        let mut bytes = original.clone();
        match case {
            0 => bytes.truncate(4096),
            1 => bytes[..4].copy_from_slice(&0x101u32.to_be_bytes()),
            2 => bytes[..4].copy_from_slice(&(location & !255).to_be_bytes()),
            3 => bytes[4000..4004].copy_from_slice(&location.to_be_bytes()),
            4 => bytes[offset + 4] |= 128,
            5 => bytes[offset + 4] = 99,
            6 => bytes[offset..offset + 4].copy_from_slice(&u32::MAX.to_be_bytes()),
            _ => bytes[offset..offset + 4].copy_from_slice(&0u32.to_be_bytes()),
        }
        fs::write(&native, &bytes).unwrap();
        session_items_assert_refusal_retains(&mut s, 3);
        fs::write(&native, &original).unwrap();
        let copy = s
            .archive
            .parent()
            .unwrap()
            .join(format!("items-physical-image-{case}"));
        copy_tree(&image, &copy, &inventory, until()).unwrap();
        fs::write(
            copy.join("world/dimensions/minecraft/overworld/entities/r.0.0.mca"),
            &bytes,
        )
        .unwrap();
        reseal(&copy, 2);
        assert!(
            verify_image(
                &s.game,
                &copy,
                2,
                s.counter.placement().world,
                &s.target,
                &s.type_id
            )
            .is_err()
        );
    }
    // A bounded but trailing native NBT payload is not repaired by a decoder.
    {
        let mut region = RegionFile::open_writable(&native, false).unwrap();
        let at = region.chunks().next().unwrap().0;
        let record = region.read(at).unwrap().unwrap();
        let mut raw = record
            .compression
            .decode(&record.compressed, STRUCTURED_LIMIT)
            .unwrap();
        raw.push(0);
        region
            .write(
                at,
                &horizon_world::region::ChunkRecord {
                    compression: Compression::None,
                    compressed: raw,
                    timestamp: 0,
                },
            )
            .unwrap();
    }
    session_items_assert_refusal_retains(&mut s, 3);
    fs::write(&native, &original).unwrap();
    let projection = s.game.project_session_checkpoint_items().unwrap();
    for malformed in [false, true] {
        let mut tag = projection.rows.values().next().unwrap().tag.clone();
        if let Tag::Compound(fields) = &mut tag {
            fields.insert(
                "Pos".into(),
                Tag::List(vec![
                    Tag::Double(3200.5f64.to_bits()),
                    Tag::Double(120.5f64.to_bits()),
                    Tag::Double(3200.5f64.to_bits()),
                ]),
            );
            fields.insert("UUID".into(), Tag::IntArray(vec![91, 92, 93, 94]));
            if malformed {
                fields.insert("Health".into(), Tag::Int(5));
            }
        }
        s.game
            .save
            .save_entities(vec![((200, 200), vec![tag])])
            .unwrap();
        session_items_assert_refusal_retains(&mut s, 3);
        let copy = s
            .archive
            .parent()
            .unwrap()
            .join(format!("items-offresident-image-{malformed}"));
        copy_tree(&image, &copy, &inventory, until()).unwrap();
        fs::copy(
            s.game.save.root.join("entities/r.6.6.mca"),
            copy.join("world/dimensions/minecraft/overworld/entities/r.6.6.mca"),
        )
        .unwrap();
        reseal(&copy, 2);
        assert!(
            verify_image(
                &s.game,
                &copy,
                2,
                s.counter.placement().world,
                &s.target,
                &s.type_id
            )
            .is_err()
        );
        s.game
            .save
            .save_entities(vec![((200, 200), Vec::new())])
            .unwrap();
    }
    s.game.checkpoint_session(&s.archive, 3, until()).unwrap();
    assert_eq!(
        s.game.project_session_checkpoint_items().unwrap(),
        projection
    );
}

#[test]
#[ignore = "isolated pinned resources + prebuilt pair: promoted uncertainty retains genuine original owner and UUID; fresh-epoch retry restores one owner"]
fn session_items_actual_promoted_uncertainty_and_new_epoch_retry_preserve_uuid() {
    let fixture = Fixture::new("session-items-promoted");
    let _retain = RetainOnPanic::new(&fixture);
    let mut s = session_items_delivered(&fixture, "item-promoted", false, true);
    let previous = tree(&published(&s, 1), &[]).unwrap();
    let error = s
        .game
        .checkpoint_session_with_test_fault(&s.archive, 2, until(), CheckpointTestFault::Promoted)
        .unwrap_err();
    assert_eq!(
        (error.publication, error.item_identities_assigned),
        (Publication::Uncertain, 1)
    );
    let projection = s.game.project_session_checkpoint_items().unwrap();
    let memory = s.game.session_checkpoint_item_memory().unwrap();
    assert!(published(&s, 2).exists());
    assert_eq!(tree(&published(&s, 1), &[]).unwrap(), previous);
    s.game.checkpoint_session(&s.archive, 3, until()).unwrap();
    assert_eq!(
        s.game.project_session_checkpoint_items().unwrap(),
        projection
    );
    assert_eq!(s.game.session_checkpoint_item_memory().unwrap(), memory);
    let (game, receipt, _receivers) =
        retirement_restored(&fixture, &s, "item-promoted-recovery", false);
    assert_eq!(receipt.epoch, 3);
    assert_eq!(game.project_session_checkpoint_items().unwrap(), projection);
    assert_eq!(game.dropped.len(), 1);
}

#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
#[test]
#[ignore = "isolated pinned resources + prebuilt pair: actual writer block after UUID preparation, total deadline, release and fresh-epoch retry retain rich dropped owner"]
fn session_items_actual_writer_deadline_release_and_retry_retain_original_owner() {
    use horizon_client::save::NativeWriterTestAction;
    let fixture = Fixture::new("session-items-writer");
    let _retain = RetainOnPanic::new(&fixture);
    let mut s = session_items_delivered(&fixture, "item-writer", true, true);
    let stack = s.game.dropped[0].stack.clone();
    let id = s.game.dropped[0].id;
    let mut gate = s
        .game
        .save
        .arm_native_writer_test_control(NativeWriterTestAction::Block)
        .unwrap();
    let error = s
        .game
        .checkpoint_session(&s.archive, 2, Instant::now() + Duration::from_secs(10))
        .unwrap_err();
    gate.wait_entered(WAIT).unwrap();
    assert_eq!(
        (error.publication, error.item_identities_assigned),
        (Publication::NotPublished, 1)
    );
    assert!(!published(&s, 2).exists());
    let projection = s.game.project_session_checkpoint_items().unwrap();
    let memory = s.game.session_checkpoint_item_memory().unwrap();
    assert_eq!(s.game.dropped[0].id, id);
    retirement_raw_equal(&s.game.dropped[0].stack, &stack);
    gate.trigger().unwrap();
    drop(gate);
    native_fence(&mut s.game, until());
    s.game.checkpoint_session(&s.archive, 3, until()).unwrap();
    assert_eq!(
        s.game.project_session_checkpoint_items().unwrap(),
        projection
    );
    assert_eq!(s.game.session_checkpoint_item_memory().unwrap(), memory);
    let (game, _, _receivers) = retirement_restored(&fixture, &s, "item-writer-recovery", true);
    assert_eq!(game.project_session_checkpoint_items().unwrap(), projection);
}

#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
#[test]
#[ignore = "isolated pinned resources + prebuilt pair: real sticky native entity-region writer error preserves original delivered owner and previous accepted epoch"]
fn session_items_actual_entity_writer_filesystem_error_retains_drop_and_prior_image() {
    let fixture = Fixture::new("session-items-writer-error");
    let _retain = RetainOnPanic::new(&fixture);
    let mut s = session_items_delivered(&fixture, "item-writer-error", false, true);
    let far = (40, 40);
    s.game.streaming.extra = s
        .game
        .world
        .chunks
        .keys()
        .copied()
        .map(|at| (at, 0))
        .chain([(far, 0)])
        .collect();
    residency(&mut s.game, 0, far);
    s.game.dropped[0].position = Vec3::new(640.5, 120.5, 640.5);
    let blocked = s.game.save.root.join("entities/r.1.1.mca");
    assert!(!blocked.exists());
    fs::create_dir_all(blocked.parent().unwrap()).unwrap();
    fs::create_dir(&blocked).unwrap();
    let prior = tree(&published(&s, 1), &[]).unwrap();
    let stack = s.game.dropped[0].stack.clone();
    let id = s.game.dropped[0].id;
    let random = s.game.world.random.state();
    let error = s
        .game
        .checkpoint_session(&s.archive, 2, until())
        .unwrap_err();
    assert_eq!(
        (error.publication, error.item_identities_assigned),
        (Publication::NotPublished, 1)
    );
    assert!(
        s.game
            .save
            .observe_native_writer_for_test()
            .unwrap()
            .sticky_error()
            .is_some()
    );
    assert!(!published(&s, 2).exists());
    assert_eq!(tree(&published(&s, 1), &[]).unwrap(), prior);
    assert_eq!(s.game.dropped[0].id, id);
    retirement_raw_equal(&s.game.dropped[0].stack, &stack);
    assert!(s.game.persist.uuids.contains_key(&id));
    assert_eq!(s.game.world.random.state(), random);
    // The sticky writer cannot be relabeled healthy or reset here. Its actual
    // owning SaveDir is dropped after source ownership assertions and joins it.
}

#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
#[test]
#[ignore = "isolated pinned resources + prebuilt pair: verified distant item paused in actual native worker, original recovery deadline, owned release and real recapture"]
fn session_items_actual_delayed_native_item_column_deadline_release_and_cancellation() {
    let fixture = Fixture::new("session-items-delayed-column");
    let _retain = RetainOnPanic::new(&fixture);
    let mut s = session_items_delivered(&fixture, "item-delayed-column", true, true);
    let far = (40, 40);
    s.game.streaming.extra = s
        .game
        .world
        .chunks
        .keys()
        .copied()
        .map(|at| (at, 0))
        .chain([(far, 0)])
        .collect();
    residency(&mut s.game, 0, far);
    s.game.dropped[0].position = Vec3::new(640.5, 120.5, 640.5);
    s.game.checkpoint_session(&s.archive, 2, until()).unwrap();
    let projection = s.game.project_session_checkpoint_items().unwrap();
    let memory = s.game.session_checkpoint_item_memory().unwrap();
    let archive = tree(&s.archive, &[]).unwrap();
    let source = tree(&s.game.world_root, &["session.lock"]).unwrap();
    let staging = std::cell::Cell::new(false);
    let mut control = SessionItemLoadTest {
        column: far,
        mode: SessionItemLoadTestMode::HoldToDeadline,
        observer: None,
    };
    let candidate = Box::new(fresh(&fixture));
    let mut recovery = request(
        &s,
        s.archive.parent().unwrap().join("item-delayed-deadline"),
    );
    let original_deadline = Instant::now() + Duration::from_secs(20);
    recovery.deadline = original_deadline;
    let result = candidate.prepare_session_checkpoint_steps(
        recovery,
        |_, _| {
            staging.set(true);
            Ok(())
        },
        Some(&mut control),
    );
    let error = match result {
        Err(error) => error,
        Ok(parts) => {
            drop(parts);
            panic!("paused item column returned a recovery owner")
        }
    };
    // On Err, the locally owned receipt already released before PreparedWorld
    // destruction, and Streaming already joined every real worker. Assert only
    // after that cleanup; no detached helper thread or fake provider exists.
    let observer = control.observer.as_ref().unwrap();
    assert!(
        observer.entered() && observer.resumed(),
        "actual target Generate worker must have entered and been reaped"
    );
    assert!(error.detail.contains("deadline"), "{error}");
    assert!(Instant::now() >= original_deadline);
    assert!(!staging.get());
    assert_eq!(error.item_identities_assigned, 0);
    assert_eq!(
        s.game.project_session_checkpoint_items().unwrap(),
        projection
    );
    assert_eq!(s.game.session_checkpoint_item_memory().unwrap(), memory);
    assert_eq!(tree(&s.archive, &[]).unwrap(), archive);
    assert_eq!(tree(&s.game.world_root, &["session.lock"]).unwrap(), source);
    for (name, mode) in [
        ("released", SessionItemLoadTestMode::ReleaseAfterEntry),
        ("cancelled", SessionItemLoadTestMode::DropBeforeEntry),
    ] {
        let candidate = Box::new(fresh(&fixture));
        let mut control = SessionItemLoadTest {
            column: far,
            mode,
            observer: None,
        };
        let recovery = request(
            &s,
            s.archive
                .parent()
                .unwrap()
                .join(format!("item-delayed-{name}")),
        );
        let mut parts = candidate
            .prepare_session_checkpoint_steps(recovery, |_, _| Ok(()), Some(&mut control))
            .unwrap();
        let observer = control.observer.as_ref().unwrap();
        if name == "released" {
            assert!(observer.entered() && observer.resumed());
        } else {
            assert!(!observer.entered());
        }
        assert!(parts.game.world.chunks.contains_key(&far));
        assert!(parts.game.world.loaded_entities.is_empty());
        assert_eq!(
            parts.game.project_session_checkpoint_items().unwrap(),
            projection
        );
        let (host, _receivers) = make_host(&mut parts.game, true, [110, 220], false);
        parts
            .game
            .install_session_checkpoint_participants(host, &parts.verified.players)
            .unwrap();
        let (mut game, _) = parts.finish().unwrap();
        let epoch = if name == "released" { 3 } else { 4 };
        game.checkpoint_session(&s.archive, epoch, until()).unwrap();
        assert_eq!(game.project_session_checkpoint_items().unwrap(), projection);
        assert_eq!(
            s.game.project_session_checkpoint_items().unwrap(),
            projection
        );
    }
}
