//! Actual Game checkpoints. Each primary needs the coordinator's pinned resources/identity.
use super::super::tests::{expected_advancements, expected_player, expected_stats, setup};
use super::*;
use crate::game::{
    integrated::{
        capture_completion_tests::dispatch_machine_capture, transition_ownership_tests::Fixture,
    },
    lan::{LanSettings, Owner},
};
use horizon_client::{
    chunk::Chunk,
    counter_trial::Snapshot,
    lan::{Joined, NetConfig, Out, PlayerSender, guest::Guest, hub::HubConfig, playerdata},
    motion::PlayerMotion,
    save::Level,
    ticket_levels::Sources,
    world::{
        World,
        counter_restore::{DomainStatus, NativeRetirementReason, RestoreError, RetiredFrame},
    },
};
use horizon_core::random::RandomSource;
use horizon_protocol::packets::configuration::ClientInformation;
use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    sync::{atomic::AtomicI32, mpsc},
    time::Duration,
};
const T: i64 = 126;
type Disk = BTreeMap<PathBuf, Option<Vec<u8>>>;
fn disk(root: &Path) -> Disk {
    fn walk(path: &Path, out: &mut Disk) {
        let Ok(meta) = fs::symlink_metadata(path) else {
            return;
        };
        assert!(!meta.file_type().is_symlink());
        if meta.is_dir() {
            out.insert(path.to_owned(), None);
            for child in fs::read_dir(path).unwrap() {
                walk(&child.unwrap().path(), out);
            }
        } else {
            assert!(meta.is_file());
            out.insert(path.to_owned(), Some(fs::read(path).unwrap()));
        }
    }
    let mut out = BTreeMap::new();
    walk(root, &mut out);
    out
}
pub(super) fn tickets(game: &mut Game<'_>, present: bool) {
    let players = if present {
        game.lan
            .as_ref()
            .unwrap()
            .hub
            .guests
            .iter()
            .map(|g| {
                (
                    (g.position[0].floor() as i32) >> 4,
                    (g.position[2].floor() as i32) >> 4,
                )
            })
            .collect()
    } else {
        Vec::new()
    };
    game.tickets.update(Some(Sources {
        players,
        forced: Vec::new(),
        view_distance: 2,
        simulation_distance: 2,
    }));
    assert_eq!(game.tickets.levels().block_ticking(0, 0), present);
}
pub(super) fn source(
    fixture: &Fixture,
) -> (
    Game<'_>,
    Option<PlayerMotion>,
    Vec<mpsc::Receiver<Out>>,
    Identity,
    NativeBinding,
    RuntimeCounterKey,
    Snapshot,
) {
    let (mut game, body, receivers, identity) = setup(fixture, false);
    let held = game.world.take_counter_trial().unwrap();
    assert_eq!(held.last_retirement().unwrap().record().counter(), 0);
    assert!(game.world.counter_domain_snapshot().is_none());
    for z in -1..=1 {
        for x in -1..=1 {
            if (x, z) != (0, 0) {
                game.world
                    .chunks
                    .insert((x, z), Arc::new(Chunk::empty(x, z)));
            }
            Arc::make_mut(game.world.chunks.get_mut(&(x, z)).unwrap()).lit = true;
            game.world.modified.insert((x, z));
        }
    }
    let binding = fixture_binding(&game).unwrap();
    game.world.set(3, 70, 2, binding.state_id()).unwrap();
    let key = game
        .world
        .initialize_counter_domain(&binding, &target(game.dim).unwrap())
        .unwrap();
    assert_eq!(key.placement().generation, 1);
    assert_ne!(key.placement().world.to_string(), uuid_text(OWNER_RECORD));
    tickets(&mut game, true);
    for time in 124..=T {
        game.world.game_time = time;
        game.tick_counter_trial(true);
        game.tick_counter_trial(true);
        let record = game.world.read_persistent_counter(&key).unwrap();
        assert_eq!(
            (record.counter(), record.last_advanced()),
            ((time - 123) as u64, Some(time))
        );
    }
    game.streaming
        .expect_existing()
        .require_pristine_fixture_capture()
        .unwrap();
    assert_eq!(
        game.capture_profile_checks(CaptureProfile::NativeCounterPatch)
            .unwrap(),
        [Dim::Overworld.id()]
    );
    (game, body, receivers, identity, binding, key, held)
}
fn request(game: &Game<'_>, identity: &Identity, epoch: u64) -> CaptureRequest {
    let archive_root = game.world_root.parent().unwrap().join("machine-archive");
    fs::create_dir_all(&archive_root).unwrap();
    CaptureRequest {
        archive_root,
        epoch,
        identity: identity.clone(),
    }
}
fn published(request: &CaptureRequest) -> PathBuf {
    request
        .archive_root
        .join(format!("epoch-{:020}", request.epoch))
}
fn record(game: &Game<'_>) -> (u64, Option<i64>) {
    let s = game.world.counter_trial_snapshot().unwrap();
    let r = s.record().unwrap();
    (r.counter(), r.last_advanced())
}
fn live(game: &Game<'_>) -> String {
    let mut rng = game.world.random.clone();
    let mut chunks: Vec<_> = game
        .world
        .chunks
        .iter()
        .map(|(pos, chunk)| {
            (
                *pos,
                Arc::as_ptr(chunk) as usize,
                chunk.lit,
                chunk.block(2, 70, 2),
            )
        })
        .collect();
    chunks.sort_by_key(|row| row.0);
    let guests: Vec<_> = game
        .lan
        .as_ref()
        .unwrap()
        .hub
        .guests
        .iter()
        .map(|g| {
            format!(
                "{:?}",
                (
                    g.uuid,
                    g.position,
                    g.selected,
                    &g.inventory,
                    &g.ender_items,
                    &g.saved,
                    g.motion.as_ref().map(|m| &**m as *const _ as usize)
                )
            )
        })
        .collect();
    let mut plays: Vec<_> = game
        .lan
        .as_ref()
        .unwrap()
        .play
        .iter()
        .map(|(conn, p)| (*conn, format!("{:?}", p.stats.values)))
        .collect();
    plays.sort_by_key(|p| p.0);
    format!(
        "{:?}",
        (
            game.world.counter_trial_snapshot(),
            game.world.counter_domain_snapshot(),
            game.world.game_time,
            game.world.day_time,
            game.world.revision,
            chunks,
            guests,
            plays,
            [rng.next_int(), rng.next_int()]
        )
    )
}
fn assert_native_footprint(root: &Path, mut before: Disk) {
    let mut after = disk(root);
    let mut files = Vec::new();
    for x in -1..=0 {
        for z in -1..=0 {
            files.push(
                horizon_client::save::dimension_dir(root, Dim::Overworld.id())
                    .join(format!("region/r.{x}.{z}.mca")),
            );
        }
    }
    files.push(root.join("level.dat"));
    for name in [
        "weather",
        "world_clocks",
        "world_gen_settings",
        "game_rules",
        "random_sequences",
    ] {
        files.push(root.join(format!("data/minecraft/{name}.dat")));
    }
    for (kind, ext) in [("data", "dat"), ("advancements", "json"), ("stats", "json")] {
        files.push(root.join(format!(
            "players/{kind}/ffffffff-ffff-fffe-0000-000300000004.{ext}"
        )));
    }
    for p in files.clone() {
        if p.extension().and_then(|s| s.to_str()) == Some("dat") {
            let name = p.file_name().unwrap().to_str().unwrap();
            files.push(p.with_file_name(format!("{name}_new")));
            files.push(p.with_file_name(format!("{name}_old")));
        }
    }
    let mut parents = BTreeSet::new();
    for p in files {
        let mut parent = p.parent();
        while let Some(d) = parent {
            if !d.starts_with(root) {
                break;
            }
            parents.insert(d.to_owned());
            parent = d.parent();
        }
        let old = before.remove(&p);
        let new = after.remove(&p);
        if old != new {
            assert!(old.as_ref().is_none_or(Option::is_some));
            assert!(new.as_ref().is_some_and(Option::is_some));
        }
    }
    for p in parents {
        if !before.contains_key(&p) && after.get(&p) == Some(&None) {
            after.remove(&p);
        }
    }
    assert_eq!(after, before, "unknown native save write/type/deletion");
}
/// Exact native-source physical footprint, with only six canonical PLAYER overlays
/// and the one detached frame allowed to differ. Unknown files/dirs are forbidden.
fn physical_image(source: &Path, image: &Path, has_frame: bool) {
    let root = image.join("world");
    let mut expected: Disk = disk(source)
        .into_iter()
        .filter(|(p, _)| p != &source.join("session.lock"))
        .map(|(p, v)| (root.join(p.strip_prefix(source).unwrap()), v))
        .collect();
    let mut actual = disk(&root);
    for uuid in [
        "ffffffff-ffff-fffe-0000-000300000004",
        "00000000-0000-0000-0000-00000000000c",
    ] {
        for (kind, ext) in [("data", "dat"), ("advancements", "json"), ("stats", "json")] {
            let path = root.join(format!("players/{kind}/{uuid}.{ext}"));
            assert!(actual.remove(&path).is_some_and(|v| v.is_some()));
            expected.remove(&path);
        }
    }
    let frame = root.join("data/horizon/counter_checkpoint.json");
    if has_frame {
        assert!(actual.remove(&frame).is_some_and(|v| v.is_some()));
        assert_eq!(actual.remove(&root.join("data/horizon")), Some(None));
    } else {
        assert!(!frame.exists());
    }
    assert_eq!(actual, expected, "unknown copy/overlay footprint");
    for x in -1..=0 {
        for z in -1..=0 {
            let relative = format!("region/r.{x}.{z}.mca");
            let source_dimension = horizon_client::save::dimension_dir(source, Dim::Overworld.id());
            let image_dimension = horizon_client::save::dimension_dir(&root, Dim::Overworld.id());
            let a = fs::metadata(source_dimension.join(&relative)).unwrap();
            let b = fs::metadata(image_dimension.join(relative)).unwrap();
            assert_ne!(
                (
                    std::os::unix::fs::MetadataExt::dev(&a),
                    std::os::unix::fs::MetadataExt::ino(&a)
                ),
                (
                    std::os::unix::fs::MetadataExt::dev(&b),
                    std::os::unix::fs::MetadataExt::ino(&b)
                )
            );
        }
    }
}
fn canonical_players(root: &Path) {
    for (first, uuid) in [
        (true, "ffffffff-ffff-fffe-0000-000300000004"),
        (false, "00000000-0000-0000-0000-00000000000c"),
    ] {
        let tag = strict_player(
            &bounded_read(
                &root.join(format!("players/data/{uuid}.dat")),
                2 << 20,
                Phase::Recover,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(tag, expected_player(first, first, false));
        for (kind, expected) in [
            ("advancements", expected_advancements(first)),
            ("stats", expected_stats(first)),
        ] {
            let actual: Value = parse_json(
                &bounded_read(
                    &root.join(format!("players/{kind}/{uuid}.json")),
                    JSON_LIMIT,
                    Phase::Recover,
                )
                .unwrap(),
                JSON_LIMIT,
            )
            .unwrap();
            assert_eq!(actual, expected);
        }
    }
    for kind in ["data", "advancements", "stats"] {
        let ext = if kind == "data" { "dat" } else { "json" };
        assert!(
            !root
                .join(format!(
                    "players/{kind}/00000000-0000-0000-0000-00000000000b.{ext}"
                ))
                .exists()
        );
    }
}
fn channel_guest(conn: u32) -> (Guest, mpsc::Receiver<Out>) {
    let (tx, rx) = mpsc::channel();
    let mut g = Guest::new(
        Joined {
            conn,
            uuid: u128::from(conn),
            name: format!("Restored{conn}"),
            properties: Vec::new(),
            info: ClientInformation::new(2),
            brand: "native checkpoint fixture".into(),
            sender: PlayerSender::new(tx, Arc::default()),
            latency: Arc::new(AtomicI32::new(0)),
            address: "127.0.0.1:25565".parse().unwrap(),
        },
        conn as i32,
        [8.0, 72.0, 8.0],
        2,
    );
    g.placed = true;
    g.loaded = true;
    (g, rx)
}
/// Fresh native Game, no call to setup() and no trial until checked install at the end.
pub(super) fn loaded<'a>(
    fixture: &'a Fixture,
    root: &Path,
) -> (Game<'a>, Vec<mpsc::Receiver<Out>>) {
    let mut game = fixture.game();
    let _discarded_fresh_body = game.player.take();
    assert!(game.inventory.iter().all(Option::is_none));
    assert!(game.armor.iter().all(Option::is_none));
    assert!(game.offhand.is_none());
    game.world_root = root.to_owned();
    game.dim = Dim::Overworld;
    game.level = Level::read(root).expect("native saved Level");
    game.options.test_terrain = true;
    let (streaming, save, locator) = crate::game::world_loading::make_streaming(
        &game.res,
        &game.pack,
        &game.options,
        &game.settings,
        root,
        game.dim,
        game.level.seed,
    )
    .unwrap();
    game.streaming = crate::game::streaming_owner::GameStreaming::from_existing(streaming);
    game.save = save;
    game.end.set_locator(locator);
    let mut world = World::new(game.res.facts().air, game.level.seed);
    world.game_time = game.level.time;
    world.day_time = game.level.day_time;
    world.weather = game.level.weather;
    for z in -1..=1 {
        for x in -1..=1 {
            let chunk = game
                .save
                .load(x, z, game.pack.assets.registry, game.res.biomes())
                .expect("all nine native stored columns");
            assert!(chunk.lit);
            assert!(chunk.saved.is_some(), "retain opaque native extras");
            world.chunks.insert((x, z), Arc::new(chunk));
        }
    }
    game.world = world;
    assert!(game.world.counter_trial_snapshot().is_none());
    assert!(game.world.counter_domain_snapshot().is_none());
    let net = NetConfig {
        listen: false,
        announce: false,
        online: None,
        ..NetConfig::lan(0, "restored checkpoint fixture".into(), 4)
    };
    let hub = HubConfig {
        default_game_mode: 0,
        guest_commands: false,
        force_game_mode: false,
        hardcore: false,
        view_distance: 2,
        simulation_distance: 2,
        dimension: game.dim.id().into(),
        seed: game.level.seed,
        max_players: 4,
        dedicated: false,
        integrated: true,
    };
    game.lan_host(
        LanSettings {
            port: 0,
            guest_commands: false,
            force_game_mode: false,
        },
        net,
        hub,
        Some(11),
    )
    .unwrap();
    let mut host = game.lan.take().unwrap();
    host.owner = Some(Owner {
        uuid: 11,
        saved: None,
    });
    let mut receivers = Vec::new();
    for conn in [11, 12] {
        let (mut guest, rx) = channel_guest(conn);
        receivers.push(rx);
        let uuid = if conn == 11 {
            uuid_text(OWNER_RECORD)
        } else {
            uuid_text([0, 0, 0, 12])
        };
        let tag = strict_player(
            &bounded_read(
                &root.join(format!("players/data/{uuid}.dat")),
                2 << 20,
                Phase::Recover,
            )
            .unwrap(),
        )
        .unwrap();
        let mut level = Level::default();
        level.read_player(&tag);
        level.player_extra = Some(tag);
        playerdata::apply(&mut guest, &level);
        let adv: Value = parse_json(
            &bounded_read(
                &root.join(format!("players/advancements/{uuid}.json")),
                JSON_LIMIT,
                Phase::Recover,
            )
            .unwrap(),
            JSON_LIMIT,
        )
        .unwrap();
        guest.advancements = horizon_client::advancements::Progress::from_json(&adv);
        guest.advancements_loaded = true;
        host.hub.guests.push(guest);
        game.as_guest(&mut host, conn, |g| assert_eq!(g.lan_acting, Some(conn)))
            .unwrap();
        host.play.get_mut(&conn).unwrap().stats =
            crate::game::stats::Stats::read(&root.join(format!("players/stats/{uuid}.json")));
    }
    game.lan = Some(host);
    tickets(&mut game, true);
    assert_eq!(game.world.game_time, T);
    assert!(game.world.counter_domain_snapshot().is_none());
    (game, receivers)
}
fn recover(
    request: &CaptureRequest,
    destination: &Path,
    binding: &NativeBinding,
    world: WorldId,
) -> MachineRecovery {
    recover_machine_fixture_epoch(
        &request.archive_root,
        &request.identity,
        destination,
        binding,
        world,
        &target(Dim::Overworld).unwrap(),
    )
    .unwrap()
}
fn copy_epoch(source: &Path, destination: &Path) {
    let entries = tree(source, &[], Phase::Copy).unwrap();
    fresh_dir(destination, Phase::Copy).unwrap();
    for d in entries.directories {
        fresh_dir(&destination.join(d), Phase::Copy).unwrap();
    }
    for member in entries.files {
        physical_copy(
            &source.join(&member.path),
            &destination.join(&member.path),
            &member,
            Phase::Copy,
        )
        .unwrap();
    }
}
fn rewrite(root: &Path, path: &str, bytes: &[u8]) {
    fs::write(root.join(path), bytes).unwrap();
    let mut manifest: Manifest = parse_json(
        &bounded_read(&root.join("manifest.json"), MANIFEST_LIMIT, Phase::Recover).unwrap(),
        MANIFEST_LIMIT,
    )
    .unwrap();
    let (len, hash) = file_hash(&root.join(path), Phase::Recover).unwrap();
    let row = manifest.files.iter_mut().find(|m| m.path == path).unwrap();
    row.bytes = len;
    row.sha256 = hash;
    fs::write(
        root.join("manifest.json"),
        json_bytes(&manifest, MANIFEST_LIMIT).unwrap(),
    )
    .unwrap();
}
#[test]
#[ignore = "requires isolated actual Game resources and coordinator-bound identities"]
fn machine_restore_nonzero_epoch_installs_and_resumes_native_game() {
    let fixture = Fixture::new("machine-nonzero");
    let (mut game, _body, _receivers, identity, binding, key, _held) = source(&fixture);
    let request = request(&game, &identity, 1);
    let root = published(&request);
    let source_before = disk(&game.world_root);
    let live_before = live(&game);
    let receipt = dispatch_machine_capture(&mut game, request.clone()).unwrap();
    assert_eq!(receipt.epoch, 1);
    assert_native_footprint(&game.world_root, source_before);
    assert_eq!(live(&game), live_before);
    assert!(
        !game
            .world_root
            .join("data/horizon/counter_checkpoint.json")
            .exists(),
        "frame is a detached overlay"
    );
    let frozen = disk(&root);
    let source_disk = disk(&game.world_root);
    canonical_players(&root.join("world"));
    physical_image(&game.world_root, &root, true);
    let destination = game.world_root.parent().unwrap().join("restored");
    let recovered = recover(&request, &destination, &binding, key.placement().world);
    assert_eq!(recovered.receipt.epoch, 1);
    assert_eq!(recovered.receipt.counter.counter, 3);
    assert_eq!(recovered.receipt.counter.last_advanced, Some(T));
    assert_eq!(recovered.frame.allocation_high_water, 1);
    assert_eq!(
        recovered.frame.placement,
        Placement::Active {
            generation: 1,
            counter: 3,
            last_advanced: Some(T)
        }
    );
    canonical_players(&destination);
    // Exactly the selected immutable world's files, plus the fresh native lock.
    let expected = disk(&root.join("world"));
    let mut actual = disk(&destination);
    actual.remove(&destination.join("session.lock"));
    let normalized: Disk = expected
        .iter()
        .map(|(p, v)| {
            (
                destination.join(p.strip_prefix(root.join("world")).unwrap()),
                v.clone(),
            )
        })
        .collect();
    assert_eq!(actual, normalized);
    let (mut restored, _restored_receivers) = loaded(&fixture, &destination);
    let new_key = restored
        .install_machine_checkpoint(&binding, key.placement().world, &recovered.frame)
        .unwrap();
    assert_eq!(new_key.placement(), key.placement());
    assert_ne!(new_key, key);
    assert_ne!(new_key.instance(), key.instance());
    assert_ne!(new_key.session_nonce(), key.session_nonce());
    assert!(restored.world.read_persistent_counter(&key).is_err());
    assert!(game.world.read_persistent_counter(&new_key).is_err());
    assert_eq!(record(&restored), (3, Some(T)));
    restored.tick_counter_trial(true);
    assert_eq!(record(&restored), (3, Some(T)));
    restored.world.game_time = T + 1;
    restored.tick_counter_trial(true);
    restored.tick_counter_trial(true);
    assert_eq!(record(&restored), (4, Some(T + 1)));
    assert_eq!(record(&game), (3, Some(T)));
    assert_eq!(
        restored.world.counter_domain_snapshot().unwrap().high_water,
        1
    );
    assert_eq!(disk(&root), frozen);
    assert_eq!(disk(&game.world_root), source_disk);
    // Native reload extras are preserved and W1 still refuses this separate history.
    assert!(restored.observe_checkpoint_world_owners().is_err());
}

#[test]
#[ignore = "requires isolated actual Game resources and coordinator-bound identities"]
fn machine_restore_rejects_frame_binding_clock_and_occupied_targets() {
    let fixture = Fixture::new("machine-refusals");
    let (mut game, _body, _receivers, identity, binding, key, _held) = source(&fixture);
    let request = request(&game, &identity, 1);
    dispatch_machine_capture(&mut game, request.clone()).unwrap();
    let destination = game.world_root.parent().unwrap().join("refusal-target");
    let recovered = recover(&request, &destination, &binding, key.placement().world);
    let (mut vacant, _rx) = loaded(&fixture, &destination);
    let frame = &recovered.frame;
    let baseline = live(&vacant);
    let files = disk(&destination);
    let source_before = live(&game);
    let mut bad = Vec::new();
    let mut f = frame.clone();
    f.world_uuid = WorldId::parse("11111111-1111-4111-8111-111111111111").unwrap();
    bad.push(f);
    let mut f = frame.clone();
    f.dimension = Dim::Nether.id().into();
    bad.push(f);
    let mut f = frame.clone();
    f.position = [3, 70, 2];
    bad.push(f);
    let mut f = frame.clone();
    f.observed_state.name = "minecraft:stone".into();
    bad.push(f);
    let mut f = frame.clone();
    f.type_id = "capture_counter:wrong".into();
    bad.push(f);
    let mut f = frame.clone();
    f.save_schema = 2;
    bad.push(f);
    let mut f = frame.clone();
    f.package_identity = "0".repeat(64);
    bad.push(f);
    let mut f = frame.clone();
    f.saved_game_time = T + 1;
    bad.push(f);
    let mut f = frame.clone();
    f.placement = Placement::Active {
        generation: 1,
        counter: 0,
        last_advanced: Some(T),
    };
    bad.push(f);
    let mut f = frame.clone();
    f.placement = Placement::Active {
        generation: 1,
        counter: 3,
        last_advanced: None,
    };
    bad.push(f);
    for time in [-1, T + 1] {
        let mut f = frame.clone();
        f.placement = Placement::Active {
            generation: 1,
            counter: 3,
            last_advanced: Some(time),
        };
        bad.push(f);
    }
    let mut f = frame.clone();
    f.allocation_high_water = 0;
    bad.push(f);
    for f in bad {
        assert!(
            vacant
                .install_machine_checkpoint(&binding, key.placement().world, &f)
                .is_err()
        );
        assert_eq!(live(&vacant), baseline);
        assert_eq!(disk(&destination), files);
    }
    vacant.world.game_time = T - 1;
    let before = live(&vacant);
    assert_eq!(
        vacant
            .world
            .restore_counter_frame(
                &binding,
                key.placement().world,
                &target(vacant.dim).unwrap(),
                frame
            )
            .unwrap_err(),
        RestoreError::ClockMismatch
    );
    assert_eq!(live(&vacant), before);
    vacant.world.game_time = T;
    // Strict decoder failures cannot reach owner publication.
    let encoded = frame.encode().unwrap();
    let value: Value = serde_json::from_slice(&encoded).unwrap();
    for key_name in ["last_retired", "world_uuid", "saved_game_time"] {
        let mut v = value.clone();
        v.as_object_mut().unwrap().remove(key_name);
        assert!(CounterFrameV2::decode(&serde_json::to_vec(&v).unwrap()).is_err());
    }
    let mut v = value.clone();
    v["placement"]
        .as_object_mut()
        .unwrap()
        .remove("last_advanced");
    assert!(CounterFrameV2::decode(&serde_json::to_vec(&v).unwrap()).is_err());
    let text = String::from_utf8(encoded).unwrap();
    let duplicate = text.replacen("{", "{\"format\":2,", 1);
    assert!(CounterFrameV2::decode(duplicate.as_bytes()).is_err());
    let overflow = text.replace("\"counter\":3", "\"counter\":18446744073709551616");
    assert_ne!(overflow, text);
    assert!(CounterFrameV2::decode(overflow.as_bytes()).is_err());
    assert_eq!(live(&vacant), baseline);
    for kind in 0..6 {
        let mut invalid = frame.clone();
        invalid.allocation_high_water = 2;
        invalid.placement = Placement::Active {
            generation: 2,
            counter: 3,
            last_advanced: Some(T),
        };
        let mut retired = RetiredFrame {
            generation: 1,
            counter: 3,
            last_advanced: Some(T),
            reason: NativeRetirementReason::BlockChanged,
            game_time: T,
        };
        match kind {
            0 => retired.game_time = T + 1,
            1 => retired.last_advanced = Some(T + 1),
            2 => retired.generation = 2,
            3 => retired.counter = 0,
            4 => retired.last_advanced = None,
            5 => retired.last_advanced = Some(-1),
            _ => unreachable!(),
        }
        invalid.last_retired = Some(retired);
        assert!(
            vacant
                .install_machine_checkpoint(&binding, key.placement().world, &invalid)
                .is_err()
        );
        assert_eq!(live(&vacant), baseline);
    }
    assert_eq!(
        vacant.world.block(3, 70, 2),
        binding.state_id(),
        "matching other physical site cannot supply trusted host target"
    );
    let current = vacant
        .install_machine_checkpoint(&binding, key.placement().world, frame)
        .unwrap();
    let occupied = live(&vacant);
    assert_eq!(
        vacant
            .world
            .restore_counter_frame(
                &binding,
                key.placement().world,
                &target(vacant.dim).unwrap(),
                frame
            )
            .unwrap_err(),
        RestoreError::Occupied
    );
    assert_eq!(live(&vacant), occupied);
    assert_eq!(
        vacant
            .world
            .read_persistent_counter(&current)
            .unwrap()
            .counter(),
        3
    );
    assert_eq!(live(&game), source_before);
    assert_eq!(disk(&destination), files);
}

#[test]
#[ignore = "requires isolated actual Game resources and coordinator-bound identities"]
fn machine_restore_runtime_and_generation_keys_are_not_reused() {
    let fixture = Fixture::new("machine-generations");
    let (mut game, _body, _receivers, identity, binding, key, _held) = source(&fixture);
    let request = request(&game, &identity, 1);
    dispatch_machine_capture(&mut game, request.clone()).unwrap();
    let frozen = disk(&published(&request));
    let destination = game.world_root.parent().unwrap().join("generation-target");
    let recovered = recover(&request, &destination, &binding, key.placement().world);
    let (mut restored, _rx) = loaded(&fixture, &destination);
    let first = restored
        .install_machine_checkpoint(&binding, key.placement().world, &recovered.frame)
        .unwrap();
    assert_eq!(first.placement(), key.placement());
    assert_ne!(first.session_nonce(), key.session_nonce());
    assert_ne!(first.instance(), key.instance());
    assert!(restored.world.read_persistent_counter(&key).is_err());
    restored.world.set(2, 70, 2, restored.world.air).unwrap();
    assert!(restored.world.persistent_counter_key().unwrap().is_none());
    let dormant = restored.world.counter_domain_snapshot().unwrap();
    assert_eq!(dormant.high_water, 1);
    assert_eq!(dormant.retired_key, Some(first.placement().clone()));
    let retirement = restored
        .world
        .counter_trial_snapshot()
        .unwrap()
        .last_retirement()
        .unwrap();
    assert_eq!(
        (
            retirement.record().counter(),
            retirement.record().last_advanced()
        ),
        (3, Some(T))
    );
    restored.world.set(2, 70, 2, binding.state_id()).unwrap();
    let second = restored.world.persistent_counter_key().unwrap().unwrap();
    assert_eq!(second.placement().generation, 2);
    assert_ne!(second.instance(), first.instance());
    assert!(restored.world.read_persistent_counter(&first).is_err());
    assert_eq!(record(&restored), (0, None));
    restored.world.game_time = T + 1;
    restored.tick_counter_trial(true);
    assert_eq!(record(&restored), (1, Some(T + 1)));
    // A saved head may exceed the current key. Re-admission allocates above it.
    let destination2 = game.world_root.parent().unwrap().join("head-target");
    let recovered2 = recover(&request, &destination2, &binding, key.placement().world);
    let (mut high, _rx2) = loaded(&fixture, &destination2);
    let mut frame = recovered2.frame;
    frame.allocation_high_water = 9;
    let old = high
        .install_machine_checkpoint(&binding, key.placement().world, &frame)
        .unwrap();
    high.world.set(2, 70, 2, high.world.air).unwrap();
    high.world.set(2, 70, 2, binding.state_id()).unwrap();
    let tenth = high.world.persistent_counter_key().unwrap().unwrap();
    assert_eq!(tenth.placement().generation, 10);
    assert!(high.world.read_persistent_counter(&old).is_err());
    isolated_recovery(&fixture, &game, &request, &key);
    assert_eq!(disk(&published(&request)), frozen);
    assert_eq!(record(&game), (3, Some(T)));
}

#[test]
#[ignore = "requires isolated actual Game resources and coordinator-bound identities"]
fn machine_restore_take_and_enable_refusal_keep_metadata() {
    let fixture = Fixture::new("machine-take");
    let (mut game, _body, _receivers, identity, binding, key, _held) = source(&fixture);
    let request = request(&game, &identity, 1);
    dispatch_machine_capture(&mut game, request.clone()).unwrap();
    let frozen = disk(&published(&request));
    let configuration = game
        .world
        .counter_trial_snapshot()
        .unwrap()
        .configuration()
        .clone();
    let before = game.world.counter_domain_snapshot().unwrap();
    let held = game.world.take_counter_trial().unwrap();
    assert!(held.record().is_none());
    let retirement = held.last_retirement().unwrap();
    assert_eq!(
        (
            retirement.record().counter(),
            retirement.record().last_advanced()
        ),
        (3, Some(T))
    );
    let detached = game.world.counter_domain_snapshot().unwrap();
    assert_eq!(detached.status, DomainStatus::Detached);
    assert_eq!(detached.world, before.world);
    assert_eq!(detached.high_water, 1);
    assert_eq!(detached.retired_key, Some(key.placement().clone()));
    let state = live(&game);
    assert!(
        game.world
            .enable_counter_trial(configuration.clone())
            .is_err()
    );
    assert_eq!(live(&game), state);
    let frame = recover(
        &request,
        &game.world_root.parent().unwrap().join("take-copy"),
        &binding,
        key.placement().world,
    )
    .frame;
    assert_eq!(
        game.world
            .restore_counter_frame(
                &binding,
                key.placement().world,
                &target(game.dim).unwrap(),
                &frame
            )
            .unwrap_err(),
        RestoreError::Occupied
    );
    assert!(game.world.read_persistent_counter(&key).is_err());
    assert!(game.world.counter_checkpoint_frame(T).is_err());
    let request2 = CaptureRequest {
        epoch: 2,
        ..request.clone()
    };
    let disk_before = disk(&game.world_root);
    assert!(dispatch_machine_capture(&mut game, request2).is_err());
    assert_eq!(disk(&game.world_root), disk_before);
    assert_eq!(live(&game), state);
    assert_eq!(disk(&published(&request)), frozen);
    assert_eq!(held.last_retirement().unwrap(), retirement);
}

#[test]
#[ignore = "requires isolated actual Game resources and coordinator-bound identities"]
fn machine_restore_dual_record_and_whole_epoch_failures_do_not_install() {
    let fixture = Fixture::new("machine-envelopes");
    let (mut game, _body, _receivers, identity, binding, key, _held) = source(&fixture);
    let request = request(&game, &identity, 1);
    dispatch_machine_capture(&mut game, request.clone()).unwrap();
    let epoch = published(&request);
    let frozen = disk(&epoch);
    let source_disk = disk(&game.world_root);
    let source_live = live(&game);
    let frame = game.world.counter_checkpoint_frame(T).unwrap();
    for (index, kind) in [
        "counter",
        "time",
        "position",
        "dimension",
        "world",
        "missing",
        "nullable",
    ]
    .into_iter()
    .enumerate()
    {
        let archive = game
            .world_root
            .parent()
            .unwrap()
            .join(format!("bad-envelope-{index}"));
        fs::create_dir(&archive).unwrap();
        let candidate = archive.join("epoch-00000000000000000001");
        copy_epoch(&epoch, &candidate);
        let mut f = frame.clone();
        let mut v1: CounterArchive = parse_json(
            &fs::read(candidate.join("research/counter.json")).unwrap(),
            JSON_LIMIT,
        )
        .unwrap();
        match kind {
            "counter" => {
                v1.counter = 4;
                rewrite(
                    &candidate,
                    "research/counter.json",
                    &json_bytes(&v1, JSON_LIMIT).unwrap(),
                );
            }
            "time" => {
                f.saved_game_time = T + 1;
                f.placement = Placement::Active {
                    generation: 1,
                    counter: 3,
                    last_advanced: Some(T + 1),
                };
                v1.last_advanced = Some(T + 1);
                rewrite(&candidate, FRAME_PATH, &f.encode().unwrap());
                rewrite(
                    &candidate,
                    "research/counter.json",
                    &json_bytes(&v1, JSON_LIMIT).unwrap(),
                );
            }
            "position" => {
                f.position = [3, 70, 2];
                v1.position = f.position;
                rewrite(&candidate, FRAME_PATH, &f.encode().unwrap());
                rewrite(
                    &candidate,
                    "research/counter.json",
                    &json_bytes(&v1, JSON_LIMIT).unwrap(),
                );
            }
            "dimension" => {
                f.dimension = Dim::Nether.id().into();
                v1.scope.dimension = f.dimension.clone();
                rewrite(&candidate, FRAME_PATH, &f.encode().unwrap());
                rewrite(
                    &candidate,
                    "research/counter.json",
                    &json_bytes(&v1, JSON_LIMIT).unwrap(),
                );
                let mut m: Manifest = parse_json(
                    &fs::read(candidate.join("manifest.json")).unwrap(),
                    MANIFEST_LIMIT,
                )
                .unwrap();
                m.dimensions = vec![f.dimension.clone()];
                for p in &mut m.players {
                    p.dimension = f.dimension.clone();
                }
                fs::write(
                    candidate.join("manifest.json"),
                    json_bytes(&m, MANIFEST_LIMIT).unwrap(),
                )
                .unwrap();
            }
            "world" => {
                f.world_uuid = WorldId::parse("22222222-2222-4222-8222-222222222222").unwrap();
                rewrite(&candidate, FRAME_PATH, &f.encode().unwrap());
            }
            "nullable" => {
                let mut value: Value = serde_json::from_slice(&f.encode().unwrap()).unwrap();
                value["placement"]
                    .as_object_mut()
                    .unwrap()
                    .remove("last_advanced");
                rewrite(&candidate, FRAME_PATH, &serde_json::to_vec(&value).unwrap());
            }
            "missing" => {
                fs::remove_file(candidate.join(FRAME_PATH)).unwrap();
                let mut m: Manifest = parse_json(
                    &fs::read(candidate.join("manifest.json")).unwrap(),
                    MANIFEST_LIMIT,
                )
                .unwrap();
                m.files.retain(|m| m.path != FRAME_PATH);
                fs::write(
                    candidate.join("manifest.json"),
                    json_bytes(&m, MANIFEST_LIMIT).unwrap(),
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        // These are COMPLETE coherent v1 envelopes, not failures hidden by stale hashes.
        verify_image(&candidate, 1, &identity).unwrap();
        let before = disk(&archive);
        let destination = game
            .world_root
            .parent()
            .unwrap()
            .join(format!("refused-copy-{index}"));
        assert!(
            recover_machine_fixture_epoch(
                &archive,
                &identity,
                &destination,
                &binding,
                key.placement().world,
                &target(game.dim).unwrap()
            )
            .is_err()
        );
        assert!(!destination.exists());
        assert_eq!(disk(&archive), before);
        assert_eq!(disk(&game.world_root), source_disk);
        assert_eq!(live(&game), source_live);
    }
    // Highest complete v1 epoch missing V2 is a refusal, not a mixed/older V2 fallback.
    let archive = game.world_root.parent().unwrap().join("missing-newest");
    fs::create_dir(&archive).unwrap();
    copy_epoch(&epoch, &archive.join("epoch-00000000000000000001"));
    let newest = archive.join("epoch-00000000000000000002");
    copy_epoch(&epoch, &newest);
    fs::remove_file(newest.join(FRAME_PATH)).unwrap();
    let mut manifest: Manifest = parse_json(
        &fs::read(newest.join("manifest.json")).unwrap(),
        MANIFEST_LIMIT,
    )
    .unwrap();
    manifest.epoch = 2;
    manifest.files.retain(|m| m.path != FRAME_PATH);
    fs::write(
        newest.join("manifest.json"),
        json_bytes(&manifest, MANIFEST_LIMIT).unwrap(),
    )
    .unwrap();
    let absent = game.world_root.parent().unwrap().join("no-mixed-target");
    assert!(
        recover_machine_fixture_epoch(
            &archive,
            &identity,
            &absent,
            &binding,
            key.placement().world,
            &target(game.dim).unwrap()
        )
        .is_err()
    );
    assert!(!absent.exists());
    // Actual filesystem copy refusal: existing pending target, no partial extra writes.
    let second = CaptureRequest {
        epoch: 2,
        ..request.clone()
    };
    let material = game
        .profile_material(second.clone(), CaptureProfile::NativeCounterPatch)
        .unwrap();
    let pending = second.archive_root.join(".pending-00000000000000000002");
    fs::create_dir(&pending).unwrap();
    fs::write(pending.join("obstruction.bin"), b"pending obstruction").unwrap();
    let native_source = disk(&material.source);
    let stage = disk(&second.archive_root);
    assert!(material.copy().is_err());
    assert_eq!(disk(&game.world_root), native_source);
    assert_eq!(disk(&second.archive_root), stage);
    fs::remove_file(pending.join("obstruction.bin")).unwrap();
    fs::remove_dir(&pending).unwrap();
    // Actual fixed frame overlay cannot replace a directory. The exact physical copy stays.
    let material = game
        .profile_material(second, CaptureProfile::NativeCounterPatch)
        .unwrap();
    let native_source = disk(&material.source);
    let mut image = material.copy().unwrap();
    image.overlay().unwrap();
    physical_image(&game.world_root, &image.root, false);
    let obstruction = make_parents(&image.root, FRAME_PATH, Phase::Overlay).unwrap();
    fs::create_dir(&obstruction).unwrap();
    let stage = disk(&image.root);
    assert!(write_bytes(&obstruction, &frame.encode().unwrap(), true, Phase::Overlay).is_err());
    assert_eq!(disk(&image.root), stage);
    assert_eq!(disk(&game.world_root), native_source);
    // Two actual subprocess stops bracket real rename, with a pipe handshake before ack.
    for phase in ["copy", "before-rename", "after-rename"] {
        fail_stop_publication(&game, &request, &key, phase);
    }
    assert_eq!(disk(&epoch), frozen);
    assert_eq!(live(&game), source_live);
}

#[test]
#[ignore = "requires isolated actual Game resources and coordinator-bound identities"]
fn machine_restore_ineligible_and_faulted_native_phases_do_not_advance() {
    let fixture = Fixture::new("machine-gates");
    let (mut game, _body, _receivers, identity, binding, key, _held) = source(&fixture);
    let request = request(&game, &identity, 1);
    dispatch_machine_capture(&mut game, request.clone()).unwrap();
    let frozen = disk(&published(&request));
    let baseline = game.world.counter_domain_snapshot().unwrap();
    let players: Vec<_> = game
        .lan
        .as_ref()
        .unwrap()
        .hub
        .guests
        .iter()
        .map(|g| (g.uuid, g.position, g.inventory.clone()))
        .collect();
    game.world.game_time = T + 1;
    game.tick_counter_trial(false);
    assert_eq!(record(&game), (3, Some(T)));
    tickets(&mut game, false);
    game.tick_counter_trial(true);
    assert_eq!(record(&game), (3, Some(T)));
    tickets(&mut game, true);
    Arc::make_mut(game.world.chunks.get_mut(&(0, 0)).unwrap()).lit = false;
    game.tick_counter_trial(true);
    assert_eq!(record(&game), (3, Some(T)));
    Arc::make_mut(game.world.chunks.get_mut(&(0, 0)).unwrap()).lit = true;
    assert_eq!(game.world.counter_domain_snapshot().unwrap(), baseline);
    game.tick_counter_trial(true);
    assert_eq!(record(&game), (4, Some(T + 1)));
    game.tick_counter_trial(true);
    assert_eq!(record(&game), (4, Some(T + 1)));
    game.world.game_time = T;
    game.tick_counter_trial(true);
    assert!(
        game.world
            .counter_trial_snapshot()
            .unwrap()
            .fault()
            .is_some()
    );
    assert_eq!(record(&game), (4, Some(T + 1)));
    assert_eq!(
        game.world.counter_domain_snapshot().unwrap().status,
        DomainStatus::Faulted
    );
    let faulted = game.world.counter_trial_snapshot().unwrap();
    game.world.game_time = T + 2;
    game.tick_counter_trial(true);
    assert_eq!(
        game.world.counter_trial_snapshot().unwrap().record(),
        faulted.record()
    );
    assert!(game.world.read_persistent_counter(&key).is_err());
    assert_eq!(game.world.counter_domain_snapshot().unwrap().high_water, 1);
    let now: Vec<_> = game
        .lan
        .as_ref()
        .unwrap()
        .hub
        .guests
        .iter()
        .map(|g| (g.uuid, g.position, g.inventory.clone()))
        .collect();
    assert_eq!(now, players);
    assert_eq!(disk(&published(&request)), frozen);
    // Actual Game phase refuses an incomplete authored storage neighborhood. This is
    // not a fabricated unload; actual private unload/reinsert belongs to lib tests.
    let other = Fixture::new("machine-missing-neighbors");
    let (mut sparse, _body2, _rx2, _identity2) = setup(&other, false);
    let _transient = sparse.world.take_counter_trial().unwrap();
    Arc::make_mut(sparse.world.chunks.get_mut(&(0, 0)).unwrap()).lit = true;
    let b = fixture_binding(&sparse).unwrap();
    let sparse_key = sparse
        .world
        .initialize_counter_domain(&b, &target(sparse.dim).unwrap())
        .unwrap();
    tickets(&mut sparse, true);
    sparse.world.game_time = T + 1;
    sparse.tick_counter_trial(true);
    assert_eq!(
        sparse
            .world
            .read_persistent_counter(&sparse_key)
            .unwrap()
            .counter(),
        0
    );
    // Exact u64 payload restoration, then the REAL native overflow fault.
    let destination = game.world_root.parent().unwrap().join("overflow-target");
    let recovered = recover(&request, &destination, &binding, key.placement().world);
    let (mut overflow, _rx3) = loaded(&fixture, &destination);
    let mut frame = recovered.frame;
    frame.placement = Placement::Active {
        generation: 1,
        counter: u64::MAX,
        last_advanced: Some(T),
    };
    overflow
        .install_machine_checkpoint(&binding, key.placement().world, &frame)
        .unwrap();
    overflow.world.game_time = T + 1;
    overflow.tick_counter_trial(true);
    assert_eq!(record(&overflow), (u64::MAX, Some(T)));
    assert!(
        overflow
            .world
            .counter_trial_snapshot()
            .unwrap()
            .fault()
            .is_some()
    );
    assert_eq!(
        overflow.world.counter_domain_snapshot().unwrap().high_water,
        1
    );
    assert_eq!(disk(&published(&request)), frozen);
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildPlan {
    context: PathBuf,
    archive: PathBuf,
    destination: PathBuf,
    content: PathBuf,
    identity: Identity,
    world: String,
    mode: String,
}
struct OwnedChild {
    child: std::process::Child,
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn child_run(plan: &ChildPlan, kill: bool) -> Value {
    fs::create_dir(&plan.context).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact","game::levels::capture_integration::counter_restore_integration::tests::machine_restore_process_child","--ignored","--nocapture","--test-threads=1"])
        .current_dir(&plan.context).env("HORIZON_MACHINE_CHILD_PLAN",serde_json::to_string(plan).unwrap())
        .env_remove("HORIZON_SESSION_FILE").env("HORIZON_LAUNCHER_NO_SELF_UPDATE","1").env("HORIZON_NO_AUDIO","1")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::from(File::create(plan.context.join("stderr.log")).unwrap()));
    for (name, folder) in [
        ("HOME", "home"),
        ("USERPROFILE", "home"),
        ("HORIZON_HOME", "game"),
        ("HORIZON_LAUNCHER_HOME", "launcher"),
        ("HORIZON_LAUNCHER_TRASH", "trash"),
        ("HORIZON_LAUNCHER_CLIPBOARD", "clipboard"),
        ("HORIZON_LAUNCHER_DESKTOP", "desktop"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_CACHE_HOME", "cache"),
        ("TMPDIR", "tmp"),
        ("TMP", "tmp"),
        ("TEMP", "tmp"),
    ] {
        let path = plan.context.join(folder);
        fs::create_dir_all(&path).unwrap();
        command.env(name, path);
    }
    let mut owned = OwnedChild {
        child: command.spawn().unwrap(),
    };
    let stdout = owned.child.stdout.take().unwrap();
    let log_path = plan.context.join("stdout.log");
    let (send, recv) = mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut input = BufReader::new(stdout);
        let mut log = File::create(log_path).unwrap();
        let mut total = 0usize;
        loop {
            let mut line = Vec::new();
            match (&mut input).take(4097).read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => {
                    total += line.len();
                    log.write_all(&line).unwrap();
                    if line.len() > 4096 || total > 64 << 10 {
                        let _ = send.try_send(Err("bounded stdout exceeded".to_owned()));
                        break;
                    }
                    let text = String::from_utf8_lossy(&line);
                    if let Some(encoded) = text.trim().strip_prefix("H08_MACHINE_CELL ") {
                        let message =
                            parse_json::<Value>(encoded.as_bytes(), 4096).map_err(|e| e.detail);
                        let _ = send.try_send(message);
                    }
                }
                Err(e) => {
                    let _ = send.try_send(Err(e.to_string()));
                    break;
                }
            }
        }
    });
    let signal = recv.recv_timeout(Duration::from_secs(120));
    if kill || signal.is_err() {
        owned.child.kill().unwrap();
    } else {
        owned.child.stdin.as_mut().unwrap().write_all(b"x").unwrap();
    }
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = owned.child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > Duration::from_secs(15) {
            owned.child.kill().unwrap();
            let status = owned.child.wait().unwrap();
            reader.join().unwrap();
            panic!("child did not settle: {status}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    reader.join().unwrap();
    if !kill {
        assert!(
            status.success(),
            "child stderr {}",
            fs::read_to_string(plan.context.join("stderr.log")).unwrap()
        );
    }
    signal
        .unwrap_or_else(|error| {
            panic!(
                "child signal {error}: {}",
                fs::read_to_string(plan.context.join("stderr.log")).unwrap()
            )
        })
        .unwrap()
}
fn isolated_recovery(
    _fixture: &Fixture,
    game: &Game<'_>,
    request: &CaptureRequest,
    key: &RuntimeCounterKey,
) {
    let parent = game.world_root.parent().unwrap();
    let plan = ChildPlan {
        context: parent.join("isolated-recovery-context"),
        archive: request.archive_root.clone(),
        destination: parent.join("isolated-recovery-world"),
        content: parent.join("counter-content"),
        identity: request.identity.clone(),
        world: key.placement().world.to_string(),
        mode: "restore".into(),
    };
    let before = disk(&request.archive_root);
    let output = child_run(&plan, false);
    assert_eq!(output["mode"], "restore");
    assert_eq!(output["world"], key.placement().world.to_string());
    assert_eq!(output["generation"], 1);
    assert_eq!(output["before"], 3);
    assert_eq!(output["after"], 4);
    assert_eq!(output["last_advanced"], T + 1);
    assert_ne!(
        output["session"],
        digest_identity_nonce(key.session_nonce())
    );
    assert_eq!(disk(&request.archive_root), before);
    canonical_players(&plan.destination);
}
fn digest_identity_nonce(bytes: [u8; 16]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn fail_stop_publication(
    game: &Game<'_>,
    request: &CaptureRequest,
    key: &RuntimeCounterKey,
    phase: &str,
) {
    let parent = game.world_root.parent().unwrap();
    let archive = parent.join(format!("fail-stop-{phase}"));
    fs::create_dir(&archive).unwrap();
    let old = archive.join("epoch-00000000000000000001");
    copy_epoch(&published(request), &old);
    let old_bytes = disk(&old);
    let pending = archive.join(".pending-00000000000000000002");
    if phase != "copy" {
        copy_epoch(&old, &pending);
        let mut manifest: Manifest = parse_json(
            &fs::read(pending.join("manifest.json")).unwrap(),
            MANIFEST_LIMIT,
        )
        .unwrap();
        manifest.epoch = 2;
        fs::write(
            pending.join("manifest.json"),
            json_bytes(&manifest, MANIFEST_LIMIT).unwrap(),
        )
        .unwrap();
    }
    let before = disk(&archive);
    let plan = ChildPlan {
        context: parent.join(format!("child-{phase}")),
        archive: archive.clone(),
        destination: parent.join(format!("post-stop-{phase}")),
        content: parent.join("counter-content"),
        identity: request.identity.clone(),
        world: key.placement().world.to_string(),
        mode: phase.into(),
    };
    let signal = child_run(&plan, true);
    assert_eq!(signal["mode"], phase);
    // Kill is a process stop, not empirical power-loss evidence. No durable ack was sent.
    assert_eq!(
        CaptureError::completion_lost("publisher process killed before acknowledgement")
            .publication,
        Publication::Uncertain
    );
    let after = disk(&archive);
    let expected: Disk = match phase {
        "after-rename" => before
            .into_iter()
            .map(|(p, v)| {
                let path = if p.starts_with(&pending) {
                    archive
                        .join("epoch-00000000000000000002")
                        .join(p.strip_prefix(&pending).unwrap())
                } else {
                    p
                };
                (path, v)
            })
            .collect(),
        "copy" => {
            let detached_source = plan.context.join("detached-copy-source");
            let detached_expected: Disk = disk(&old.join("world"))
                .into_iter()
                .map(|(path, value)| {
                    (
                        detached_source.join(path.strip_prefix(old.join("world")).unwrap()),
                        value,
                    )
                })
                .collect();
            assert_eq!(
                disk(&detached_source),
                detached_expected,
                "exact detached input footprint"
            );
            let mut expected = before;
            expected.insert(pending.clone(), None);
            for (p, v) in disk(&old.join("world")) {
                expected.insert(
                    pending
                        .join("world")
                        .join(p.strip_prefix(old.join("world")).unwrap()),
                    v,
                );
            }
            expected
        }
        _ => before,
    };
    assert_eq!(after, expected);
    assert_eq!(disk(&old), old_bytes);
    let binding = fixture_binding(game).unwrap();
    let receipt = recover_machine_fixture_epoch(
        &archive,
        &request.identity,
        &plan.destination,
        &binding,
        key.placement().world,
        &target(game.dim).unwrap(),
    )
    .unwrap();
    assert_eq!(
        receipt.receipt.epoch,
        if phase == "after-rename" { 2 } else { 1 }
    );
    assert_eq!(
        receipt.frame.placement,
        Placement::Active {
            generation: 1,
            counter: 3,
            last_advanced: Some(T)
        }
    );
    assert_eq!(disk(&archive), after);
}

#[test]
#[ignore = "AUXILIARY: explicit bounded parent plan and isolated process only; never a primary"]
fn machine_restore_process_child() {
    let encoded =
        std::env::var("HORIZON_MACHINE_CHILD_PLAN").expect("auxiliary requires parent plan");
    assert!(encoded.len() <= 8192);
    let plan: ChildPlan = parse_json(encoded.as_bytes(), 8192).unwrap();
    assert!(std::env::var_os("HORIZON_SESSION_FILE").is_none());
    assert_eq!(std::env::current_dir().unwrap(), plan.context);
    let world = WorldId::parse(&plan.world).unwrap();
    let fixture = Fixture::new("machine-child");
    let registry = fixture.game().pack.assets.registry;
    let bundle = horizon_mods::paired::PreparedBundle::read_folder(&plan.content).unwrap();
    let binding = NativeBinding::from_prepared(&bundle, "capture_counter:type", registry).unwrap();
    assert_eq!(
        digest_identity(binding.package_identity()),
        plan.identity.counter_content
    );
    let signal = match plan.mode.as_str() {
        "restore" => {
            let recovery = recover_machine_fixture_epoch(
                &plan.archive,
                &plan.identity,
                &plan.destination,
                &binding,
                world,
                &target(Dim::Overworld).unwrap(),
            )
            .unwrap();
            let (mut game, _receivers) = loaded(&fixture, &plan.destination);
            let key = game
                .install_machine_checkpoint(&binding, world, &recovery.frame)
                .unwrap();
            assert_eq!(record(&game), (3, Some(T)));
            game.tick_counter_trial(true);
            assert_eq!(record(&game), (3, Some(T)));
            game.world.game_time = T + 1;
            game.tick_counter_trial(true);
            assert_eq!(record(&game), (4, Some(T + 1)));
            serde_json::json!({"mode":"restore","world":key.placement().world.to_string(),"generation":key.placement().generation,"session":digest_identity_nonce(key.session_nonce()),"before":3,"after":4,"last_advanced":T+1})
        }
        "copy" => {
            let old = plan.archive.join("epoch-00000000000000000001");
            let manifest = verify_image(&old, 1, &plan.identity).unwrap();
            read_machine_image(
                &old,
                &manifest,
                &binding,
                world,
                &target(Dim::Overworld).unwrap(),
            )
            .unwrap();
            let counter: CounterArchive = parse_json(
                &bounded_read(
                    &old.join("research/counter.json"),
                    JSON_LIMIT,
                    Phase::Recover,
                )
                .unwrap(),
                JSON_LIMIT,
            )
            .unwrap();
            // Detached restart input only: this is not a second live Game capture.
            // Material forbids sources inside its target archive. Preserve that guard
            // by making an actual independent physical source in this child's scratch.
            let detached_source = plan.context.join("detached-copy-source");
            copy_epoch(&old.join("world"), &detached_source);
            let source_before = disk(&detached_source);
            let material = Material {
                source: detached_source.clone(),
                request: CaptureRequest {
                    archive_root: plan.archive.clone(),
                    epoch: 2,
                    identity: plan.identity.clone(),
                },
                players: Vec::new(),
                counter,
                dimensions: manifest.dimensions,
            };
            let _incomplete = material.copy().unwrap();
            assert_eq!(disk(&detached_source), source_before);
            serde_json::json!({"mode":"copy"})
        }
        "before-rename" | "after-rename" => {
            let root = plan.archive.join(".pending-00000000000000000002");
            let manifest = verify_image(&root, 2, &plan.identity).unwrap();
            read_machine_image(
                &root,
                &manifest,
                &binding,
                world,
                &target(Dim::Overworld).unwrap(),
            )
            .unwrap();
            let counter: CounterArchive = parse_json(
                &bounded_read(
                    &root.join("research/counter.json"),
                    JSON_LIMIT,
                    Phase::Recover,
                )
                .unwrap(),
                JSON_LIMIT,
            )
            .unwrap();
            let manifest_bytes =
                bounded_read(&root.join("manifest.json"), MANIFEST_LIMIT, Phase::Recover).unwrap();
            let image = PendingImage {
                root,
                request: CaptureRequest {
                    archive_root: plan.archive.clone(),
                    epoch: 2,
                    identity: plan.identity.clone(),
                },
                players: Vec::new(),
                counter,
                dimensions: manifest.dimensions.clone(),
                manifest: Some(manifest),
                manifest_bytes,
            };
            if plan.mode == "after-rename" {
                let _no_ack = image.rename_published().unwrap();
            }
            serde_json::json!({"mode":plan.mode})
        }
        _ => panic!("unknown parent mode"),
    };
    println!(
        "\nH08_MACHINE_CELL {}",
        serde_json::to_string(&signal).unwrap()
    );
    std::io::stdout().flush().unwrap();
    let (send, recv) = mpsc::channel();
    let _stdin = std::thread::spawn(move || {
        let mut byte = [0];
        let result = std::io::stdin()
            .read_exact(&mut byte)
            .map_err(|e| e.to_string());
        let _ = send.send(result);
    });
    recv.recv_timeout(Duration::from_secs(120))
        .expect("parent disappeared")
        .unwrap();
}
