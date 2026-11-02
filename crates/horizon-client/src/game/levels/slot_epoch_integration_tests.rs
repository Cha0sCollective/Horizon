//! Fixed active slot epoch proof through the actual owner Task. No ordinary admission.
use super::super::tests::{loaded, source};
use super::*;
use crate::game::{
    integrated::{
        capture_completion_tests::{
            dispatch_slot_capture, dispatch_slot_capture_after_observer_timeout,
        },
        transition_ownership_tests::Fixture,
    },
    slot_player::TransferOutcome,
};
use EntityOwnerObservationError as E;
use UnsupportedEntityFamily as F;
use horizon_client::{
    chunk::{ChunkExtras, RolledLoot},
    entities::{ArmorStand, Cushion, ItemFrame, Painting},
    items::ItemStack,
    projectiles::{Projectile, ProjectileKind},
    slot_transfer::{Direction, ResolvedLimits, SlotProjection},
    world_slot_trial::Preparation,
};
use horizon_core::{
    geometry::Vec3,
    position::BlockPos,
    random::RandomSource,
    ticks::{SavedTick, TickPriority, TickTypeId},
};
use horizon_world::level::LevelEvent;
use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};
const T: i64 = 126;
const WORLD_SPAWN: [i32; 3] = [0, 64, 0];
const PERSONAL_RESPAWN: [i32; 3] = [4, 65, -7];
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
fn request(game: &Game<'_>, identity: &Identity, epoch: u64) -> CaptureRequest {
    let archive_root = game.world_root.parent().unwrap().join("slot-archive");
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
fn seed_slot(game: &mut Game<'_>, binding: &NativeBinding) -> SlotKey {
    // Author the global spawn as ordinary world bootstrap would, before the
    // ten-item baseline and actual player-cell move. The owner's existing
    // personal spawn remains a distinct native PLAYER field.
    assert!(game.level.world_spawn.is_none());
    let owner = game
        .lan
        .as_ref()
        .unwrap()
        .hub
        .guests
        .iter()
        .find(|guest| guest.uuid == 11)
        .unwrap();
    assert_eq!(owner.spawn, Some([4.5, 65.0, -6.5]));
    assert!(owner.respawn_elsewhere.is_none());
    assert_ne!(WORLD_SPAWN, PERSONAL_RESPAWN);
    game.level.world_spawn = Some(WORLD_SPAWN);
    // Author the ten-item baseline before the qualified actual player-cell move.
    game.lan
        .as_mut()
        .unwrap()
        .hub
        .guests
        .iter_mut()
        .find(|g| g.uuid == 11)
        .unwrap()
        .inventory[0] = Some(ItemStack::new("minecraft:stone", 10));
    let key = game
        .world
        .initialize_persistent_world_slot(binding, &target(game.dim).unwrap())
        .unwrap()
        .unwrap();
    let mut host = game.lan.take().unwrap();
    let moved = game
        .as_guest(&mut host, 11, |g| {
            g.transfer_world_slot_inventory_cell(&key, 0, Direction::IntoMachine, 7)
        })
        .unwrap()
        .unwrap();
    assert!(matches!(moved,TransferOutcome::Committed(value) if value.moved==7));
    game.lan = Some(host);
    assert!(game.lan_acting.is_none() && game.player.is_none());
    assert_eq!(game.level.world_spawn, Some(WORLD_SPAWN));
    assert!(plain_stone(
        game.lan
            .as_ref()
            .unwrap()
            .hub
            .guests
            .iter()
            .find(|g| g.uuid == 11)
            .unwrap()
            .inventory[0]
            .as_ref(),
        3
    ));
    let snapshot = game.world.read_world_slot(&key).unwrap();
    assert_eq!(snapshot.slot.revision(), 1);
    assert!(plain_stone(snapshot.slot.stack(), 7));
    key
}
type Live = (
    CounterFrameV2,
    Vec<u8>,
    Vec<Vec<u8>>,
    i64,
    i64,
    u64,
    [i32; 2],
    Vec<((i32, i32), usize)>,
);
fn live(game: &Game<'_>) -> Live {
    let (counter, slot) = game.slot_epoch_projection().unwrap();
    let players = game
        .slot_player_images()
        .unwrap()
        .into_iter()
        .flat_map(|p| [p.nbt, p.advancements, p.stats])
        .collect();
    let mut random = game.world.random.clone();
    let mut chunks: Vec<_> = game
        .world
        .chunks
        .iter()
        .map(|(p, c)| (*p, Arc::as_ptr(c) as usize))
        .collect();
    chunks.sort_unstable();
    (
        counter,
        slot,
        players,
        game.world.game_time,
        game.world.day_time,
        game.world.revision,
        [random.next_int(), random.next_int()],
        chunks,
    )
}
fn record(game: &Game<'_>) -> (u64, Option<i64>) {
    let s = game.world.counter_trial_snapshot().unwrap();
    let r = s.record().unwrap();
    (r.counter(), r.last_advanced())
}
fn recover(
    request: &CaptureRequest,
    destination: &Path,
    game: &Game<'_>,
    binding: &NativeBinding,
    world: WorldId,
) -> SlotRecovery {
    recover_slot_fixture_epoch(
        &request.archive_root,
        &request.identity,
        destination,
        binding,
        game.res.items(),
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
fn slot_manifest(root: &Path) -> SlotManifest {
    parse_json(
        &fs::read(root.join("manifest.json")).unwrap(),
        MANIFEST_LIMIT,
    )
    .unwrap()
}
fn put_manifest(root: &Path, manifest: &SlotManifest) {
    fs::write(
        root.join("manifest.json"),
        json_bytes(manifest, MANIFEST_LIMIT).unwrap(),
    )
    .unwrap();
}
fn rewrite(root: &Path, path: &str, bytes: &[u8]) {
    fs::write(root.join(path), bytes).unwrap();
    let mut m = slot_manifest(root);
    let (len, hash) = file_hash(&root.join(path), Phase::Recover).unwrap();
    let row = m.files.iter_mut().find(|m| m.path == path).unwrap();
    row.bytes = len;
    row.sha256 = hash;
    put_manifest(root, &m);
}
fn canonical_players(root: &Path) {
    let mut expected =
        crate::game::levels::capture_integration::tests::expected_player(true, true, false);
    let Some(Tag::List(inventory)) = expected.get_mut("Inventory") else {
        panic!("fixture inventory");
    };
    let stone = inventory
        .iter_mut()
        .find(|t| t.get("Slot") == Some(&Tag::Byte(0)))
        .unwrap();
    stone.insert("count".into(), Tag::Int(3));
    let owner = uuid_text(OWNER_RECORD);
    let external = uuid_text([0, 0, 0, 12]);
    for (uuid, player, is_owner) in [
        (owner, expected, true),
        (
            external,
            crate::game::levels::capture_integration::tests::expected_player(false, false, false),
            false,
        ),
    ] {
        assert_eq!(
            strict_player(&fs::read(root.join(format!("players/data/{uuid}.dat"))).unwrap())
                .unwrap(),
            player
        );
        let adv: Value = parse_json(
            &fs::read(root.join(format!("players/advancements/{uuid}.json"))).unwrap(),
            JSON_LIMIT,
        )
        .unwrap();
        assert_eq!(
            adv,
            crate::game::levels::capture_integration::tests::expected_advancements(is_owner)
        );
        let stats: Value = parse_json(
            &fs::read(root.join(format!("players/stats/{uuid}.json"))).unwrap(),
            JSON_LIMIT,
        )
        .unwrap();
        assert_eq!(
            stats,
            crate::game::levels::capture_integration::tests::expected_stats(is_owner)
        );
    }
    assert!(
        !root
            .join(format!("players/data/{}.dat", uuid_text([0, 0, 0, 11])))
            .exists()
    );
}
fn native_spawn_fields(root: &Path) {
    let level = horizon_client::save::Level::read(root).expect("native saved Level");
    assert_eq!(level.world_spawn, Some(WORLD_SPAWN));
    let saved = strict_player(&fs::read(root.join("level.dat")).unwrap()).unwrap();
    let spawn = saved.get("Data").unwrap().get("spawn").unwrap();
    assert_eq!(spawn.get("pos"), Some(&Tag::IntArray(WORLD_SPAWN.to_vec())));
    assert_eq!(
        spawn.get("dimension"),
        Some(&Tag::String(Dim::Overworld.id().into()))
    );
    let owner = strict_player(
        &fs::read(root.join(format!("players/data/{}.dat", uuid_text(OWNER_RECORD)))).unwrap(),
    )
    .unwrap();
    let respawn = owner
        .get("respawn")
        .expect("personal respawn retained after native save");
    assert_eq!(
        respawn.get("pos"),
        Some(&Tag::IntArray(PERSONAL_RESPAWN.to_vec()))
    );
    assert_eq!(
        respawn.get("dimension"),
        Some(&Tag::String(Dim::Overworld.id().into()))
    );
    assert_eq!(respawn.get("forced"), Some(&Tag::Byte(1)));
}
fn assert_same_selected_world(image: &Path, destination: &Path) {
    let expected = disk(&image.join("world"));
    let mut actual = disk(destination);
    actual.remove(&destination.join("session.lock"));
    let expected: Disk = expected
        .into_iter()
        .map(|(p, b)| {
            (
                destination.join(p.strip_prefix(image.join("world")).unwrap()),
                b,
            )
        })
        .collect();
    assert_eq!(actual, expected);
}
#[test]
#[ignore = "requires isolated actual Game resources and coordinator-bound identities"]
fn slot_epoch_native_task_conserves_ten_as_player_three_and_slot_seven() {
    let fixture = Fixture::new("slot-epoch-positive");
    let (mut game, _body, _rx, identity, binding, old_counter, _held) = source(&fixture);
    let old_slot = seed_slot(&mut game, &binding);
    assert_eq!(game.level.world_spawn, Some(WORLD_SPAWN));
    let source_owner = game
        .slot_player_images()
        .unwrap()
        .into_iter()
        .find(|row| row.descriptor.integrated_owner)
        .unwrap();
    let source_owner = strict_player(&source_owner.nbt).unwrap();
    assert_eq!(
        source_owner.get("respawn").unwrap().get("pos"),
        Some(&Tag::IntArray(PERSONAL_RESPAWN.to_vec()))
    );
    let before = live(&game);
    let request = request(&game, &identity, 1);
    assert!(matches!(
        game.observe_checkpoint_entity_owners(),
        Err(E::WorldSlotTrial { .. })
    ));
    let report = game.observe_slot_epoch_entity_owners().unwrap();
    assert_eq!(report.levels.len(), 1);
    assert_eq!(report.levels[0].accounted_slots, 1);
    assert_eq!(report.levels[0].resident_drops, 0);
    let receipt = dispatch_slot_capture(&mut game, request.clone()).unwrap();
    assert_eq!(receipt.epoch, 1);
    assert_eq!(live(&game), before);
    assert_eq!(game.level.world_spawn, Some(WORLD_SPAWN));
    native_spawn_fields(&game.world_root);
    let image = published(&request);
    let frozen = disk(&image);
    let source_disk = disk(&game.world_root);
    let envelope = verify_complete_envelope(&image, 1, &identity).unwrap();
    assert!(envelope.slot_manifest.is_some());
    assert_eq!(
        envelope
            .manifest
            .files
            .iter()
            .filter(|m| m.path == SLOT_MEMBER)
            .count(),
        1
    );
    assert!(verify_image(&image, 1, &identity).is_err());
    canonical_players(&image.join("world"));
    native_spawn_fields(&image.join("world"));
    assert!(
        !game
            .world_root
            .join("data/horizon/slot_checkpoint.nbt")
            .exists()
    );
    let destination = game.world_root.parent().unwrap().join("positive-restored");
    let recovery = recover(
        &request,
        &destination,
        &game,
        &binding,
        old_counter.placement().world,
    );
    assert_eq!(recovery.receipt.epoch, 1);
    assert_same_selected_world(&image, &destination);
    canonical_players(&destination);
    native_spawn_fields(&destination);
    let (mut restored, _rx2) = loaded(&fixture, &destination);
    assert_eq!(restored.level.world_spawn, Some(WORLD_SPAWN));
    let (new_counter, new_slot) = restored
        .install_slot_epoch(
            &binding,
            old_counter.placement().world,
            &recovery.counter,
            recovery.slot,
        )
        .unwrap();
    assert_eq!(new_counter.placement(), old_counter.placement());
    assert_ne!(new_counter.session_nonce(), old_counter.session_nonce());
    assert_ne!(new_slot, old_slot);
    assert!(
        restored
            .world
            .read_persistent_counter(&old_counter)
            .is_err()
    );
    assert!(restored.world.read_world_slot(&old_slot).is_err());
    assert!(plain_stone(
        restored
            .world
            .read_world_slot(&new_slot)
            .unwrap()
            .slot
            .stack(),
        7
    ));
    assert!(plain_stone(
        restored
            .lan
            .as_ref()
            .unwrap()
            .hub
            .guests
            .iter()
            .find(|g| g.uuid == 11)
            .unwrap()
            .inventory[0]
            .as_ref(),
        3
    ));
    assert_eq!(record(&restored), (3, Some(T)));
    // At the eligible counter value/time, retained native extras are the actual
    // refusal. Prove the other fixed premises before any advancing tick.
    restored.slot_epoch_projection().unwrap();
    let roster = restored.observe_checkpoint_participants().unwrap();
    restored.preflight_checkpoint_participants(&roster).unwrap();
    restored.observe_slot_epoch_entity_owners().unwrap();
    restored
        .streaming
        .expect_existing()
        .require_pristine_fixture_capture()
        .unwrap();
    assert_eq!(restored.world.chunks.len(), 9);
    assert!(restored.world.chunks.values().all(|chunk| chunk.lit));
    assert!(restored.world.chunks.values().any(|chunk| {
        chunk
            .saved
            .as_ref()
            .is_some_and(|saved| !saved.root.is_empty())
    }));
    let reloaded_before = live(&restored);
    let error = restored
        .capture_profile_checks(CaptureProfile::NativeSlotPatch)
        .unwrap_err();
    assert_eq!(error.code, Failure::World);
    assert_eq!(live(&restored), reloaded_before);
    restored.tick_counter_trial(true);
    assert_eq!(record(&restored), (3, Some(T)));
    restored.world.game_time = T + 1;
    restored.tick_counter_trial(true);
    restored.tick_counter_trial(true);
    assert_eq!(record(&restored), (4, Some(T + 1)));
    assert_eq!(disk(&image), frozen);
    assert_eq!(disk(&game.world_root), source_disk);
    assert_eq!(live(&game), before);
    isolated_child_restore(&game, &request, &old_counter);
}

fn seed_family(game: &mut Game<'_>, family: F) {
    let at = Vec3::new(2.5, 70.0, 2.5);
    match family {
        F::Falling => game.falling.push(FallingBlock {
            id: 401,
            state: game.world.block(1, 70, 1),
            start_pos: BlockPos::new(2, 70, 2),
            position: at,
            previous: at,
            velocity: Vec3::ZERO,
            age: 7,
            no_gravity: false,
            drop_item: true,
            fall_distance: 1.25,
            stuck: Vec3::new(1.0, 1.0, 1.0),
            damage: Default::default(),
            saved: Some(Box::new((
                [1, 2, 3, 4],
                Tag::compound([("fixture:raw", Tag::Int(-7))]),
            ))),
        }),
        F::Paintings => game.paintings.push(Painting {
            id: 402,
            pos: [2, 70, 2],
            facing: 0,
            variant: "minecraft:kebab".into(),
            width: 1,
            height: 1,
            check_interval: 7,
        }),
        F::ItemFrames => game.item_frames.push(ItemFrame {
            id: 403,
            pos: [2, 70, 2],
            facing: 2,
            glow: true,
            item: Some(rich(7)),
            rotation: 3,
            drop_chance: 0.75,
            fixed: false,
            invisible: false,
            check_interval: 7,
        }),
        F::ArmorStands => {
            let mut stand = ArmorStand::new(404, at, 15.0);
            stand.equipment[5] = Some(rich(1));
            game.armor_stands.push(stand);
        }
        F::Cushions => game.cushions.push(Cushion {
            id: 405,
            position: at,
            pos: [2, 70, 2],
            yaw: 15.0,
            color: 3,
            check_interval: 7,
        }),
        F::Others => game.others.push(commands::OtherEntity {
            id: 406,
            kind: "fixture:unknown".into(),
            position: at,
            yaw: 15.0,
            pitch: -7.0,
        }),
        F::Boats => {
            let mut boat = boats::Boat::new(407, "oak".into(), at, 15.0).with_chest();
            boat.items[0] = Some(rich(7));
            game.boats.push(boat);
        }
        F::Carts => {
            let mut cart = minecarts::Minecart::new(408, minecarts::CartKind::Chest, at);
            cart.items[0] = Some(rich(7));
            game.carts.push(cart);
        }
        F::Bolts => game.spawn_remote_bolt(409, [2.5, 70.0, 2.5]),
        F::Crystals => game.crystals.push(dragon::Crystal {
            id: 410,
            position: at,
            show_bottom: true,
            time: 17,
            beam: Some([3, 81, 4]),
            invulnerable: false,
        }),
        F::Dragon => {
            let tag = Tag::compound([(
                "Pos",
                Tag::List(vec![
                    Tag::Double(2.5f64.to_bits()),
                    Tag::Double(70f64.to_bits()),
                    Tag::Double(2.5f64.to_bits()),
                ]),
            )]);
            game.dragon = Some(dragon::Dragon::from_nbt(&tag, 73).unwrap());
        }
        F::DragonFireballs => game.dragon_fireballs.push(dragon::DragonFireball::remote(
            412,
            at,
            Vec3::new(0.1, 0.2, 0.3),
        )),
        F::BreathClouds => game.breath_clouds.push(dragon::Breath {
            id: 413,
            at: [2.5, 70.0, 2.5],
            radius: 3.5,
            age: 7,
            duration: 91,
            from_dragon: true,
            potion: None,
        }),
        F::Tridents => {
            let mut trident = trident::ThrownTrident::remote(414, at, 15.0, -7.0);
            trident.stack = rich(1);
            trident.stack.item = "minecraft:trident".into();
            game.tridents.push(trident);
        }
        F::Rockets => {
            let mut rocket = fireworks::Rocket::remote(415, at, Vec3::new(0.1, 0.2, 0.3));
            rocket.item = rich(1);
            rocket.item.item = "minecraft:firework_rocket".into();
            game.rockets.push(rocket);
        }
        F::OminousSpawners => game.add_ominous_item_spawner(rich(7), [2.5, 70.0, 2.5], 91),
        F::Tnts => game.tnts.push(tnt::PrimedTnt {
            id: 417,
            position: at,
            previous: at,
            velocity: Vec3::ZERO,
            fuse: 37,
            on_ground: true,
        }),
        F::Mobs => game.mobs.push(Mob::new(
            418,
            game.res
                .mob_types()
                .get("minecraft:pillager")
                .unwrap()
                .clone(),
            at,
            15.0,
        )),
        F::Projectiles => {
            let projectile = Projectile::new(ProjectileKind::Snowball, at, None);
            assert!(!projectile.is_arrow() && !projectile.is_fireball());
            game.creatures.projectiles.push(projectile);
        }
        F::Orbs => game.orbs.push(ExperienceOrb {
            id: 420,
            position: at,
            previous: at,
            velocity: Vec3::ZERO,
            value: 7,
            age: 13,
            on_ground: false,
            count: 2,
            ticks: 17,
        }),
        F::LeashKnots => {
            game.knot_ids.insert([2, 70, 2], 421);
        }
        F::FishingHook => unreachable!(),
    }
}
fn clear_family(game: &mut Game<'_>, family: F) {
    match family {
        F::Falling => game.falling.clear(),
        F::Paintings => game.paintings.clear(),
        F::ItemFrames => game.item_frames.clear(),
        F::ArmorStands => game.armor_stands.clear(),
        F::Cushions => game.cushions.clear(),
        F::Others => game.others.clear(),
        F::Boats => game.boats.clear(),
        F::Carts => game.carts.clear(),
        F::Bolts => game.bolts.clear(),
        F::Crystals => game.crystals.clear(),
        F::Dragon => game.dragon = None,
        F::DragonFireballs => game.dragon_fireballs.clear(),
        F::BreathClouds => game.breath_clouds.clear(),
        F::Tridents => game.tridents.clear(),
        F::Rockets => game.rockets.clear(),
        F::OminousSpawners => game.ominous_spawners.clear(),
        F::Tnts => game.tnts.clear(),
        F::Mobs => game.mobs.clear(),
        F::Projectiles => game.creatures.projectiles.clear(),
        F::Orbs => game.orbs.clear(),
        F::LeashKnots => game.knot_ids.clear(),
        F::FishingHook => unreachable!(),
    }
}

fn rich(count: u32) -> ItemStack {
    crate::game::levels::entity_owner_tests::rich(count)
}
fn drop_item(id: u32, position: Vec3) -> ItemEntity {
    ItemEntity {
        id,
        stack: ItemStack::new("minecraft:stone", 1),
        position,
        previous: position,
        velocity: Vec3::ZERO,
        on_ground: false,
        age: 1,
        pickup_delay: 9,
        bob_offset: 0.25,
        ticks: 1,
        health: 5,
        fire_ticks: -17,
        thrower: None,
        target: None,
    }
}
fn assert_entity_refusal(game: &mut Game<'_>, request: &CaptureRequest, expected: E) {
    let before = live(game);
    let world_disk = disk(&game.world_root);
    let archive = disk(&request.archive_root);
    assert_eq!(
        game.observe_slot_epoch_entity_owners().err(),
        Some(expected)
    );
    let error = dispatch_slot_capture(game, request.clone()).unwrap_err();
    assert_eq!(error.code, Failure::Entity);
    assert_eq!(live(game), before);
    assert_eq!(disk(&game.world_root), world_disk);
    assert_eq!(disk(&request.archive_root), archive);
}
#[test]
#[ignore = "requires isolated actual Game resources and coordinator-bound identities"]
fn slot_epoch_accounted_owner_keeps_every_other_refusal() {
    let fixture = Fixture::new("slot-epoch-owner-refusals");
    let (mut game, body, _rx, identity, binding, _counter, _held) = source(&fixture);
    let slot = seed_slot(&mut game, &binding);
    let request = request(&game, &identity, 1);
    // PLAYER refusals are reached with the actual counter/slot pair still valid.
    for different_dimension in [false, true] {
        {
            let guest = game
                .lan
                .as_mut()
                .unwrap()
                .hub
                .guests
                .iter_mut()
                .find(|guest| guest.uuid == 12)
                .unwrap();
            if different_dimension {
                assert_eq!(guest.dimension, Dim::Overworld.id());
                guest.dimension = Dim::Nether.id().into();
            } else {
                assert!(guest.inventory[1].is_none());
                guest.inventory[1] = Some(ItemStack::new("minecraft:diamond", 1));
            }
        }
        game.slot_epoch_projection().unwrap();
        game.observe_slot_epoch_entity_owners().unwrap();
        game.observe_checkpoint_world_owners().unwrap();
        game.streaming
            .expect_existing()
            .require_pristine_fixture_capture()
            .unwrap();
        let before = live(&game);
        let source_disk = disk(&game.world_root);
        let archive = disk(&request.archive_root);
        let error = dispatch_slot_capture(&mut game, request.clone()).unwrap_err();
        assert_eq!(error.code, Failure::Player);
        assert_eq!(error.publication, Publication::NotPublished);
        assert_eq!(live(&game), before);
        assert_eq!(disk(&game.world_root), source_disk);
        assert_eq!(disk(&request.archive_root), archive);
        let guest = game
            .lan
            .as_mut()
            .unwrap()
            .hub
            .guests
            .iter_mut()
            .find(|guest| guest.uuid == 12)
            .unwrap();
        if different_dimension {
            guest.dimension = Dim::Overworld.id().into();
        } else {
            guest.inventory[1] = None;
        }
    }
    // Actual qualified update submits native work. Its order is never reset, and
    // the independent fixture keeps all ownership in place throughout refusal.
    let streamer_fixture = Fixture::new("slot-epoch-streamer-refusal");
    let (
        mut streaming_game,
        _stream_body,
        _stream_rx,
        stream_identity,
        stream_binding,
        _stream_counter,
        _stream_held,
    ) = source(&streamer_fixture);
    let _stream_slot = seed_slot(&mut streaming_game, &stream_binding);
    let stream_request = self::request(&streaming_game, &stream_identity, 1);
    assert!(
        streaming_game
            .streaming
            .expect_existing_mut()
            .update(&mut streaming_game.world)
            .1
            .is_empty()
    );
    assert_eq!(
        streaming_game
            .streaming
            .expect_existing()
            .require_pristine_fixture_capture(),
        Err(horizon_client::streaming::CapturePristineError::PreviouslySubmitted)
    );
    streaming_game.slot_epoch_projection().unwrap();
    streaming_game.observe_slot_epoch_entity_owners().unwrap();
    streaming_game.observe_checkpoint_world_owners().unwrap();
    assert_eq!(streaming_game.world.chunks.len(), 9);
    assert!(streaming_game.world.chunks.values().all(|chunk| chunk.lit));
    let before = live(&streaming_game);
    let source_disk = disk(&streaming_game.world_root);
    let archive = disk(&stream_request.archive_root);
    let error = dispatch_slot_capture(&mut streaming_game, stream_request.clone()).unwrap_err();
    assert_eq!(error.code, Failure::Streamer);
    assert_eq!(error.publication, Publication::NotPublished);
    assert_eq!(live(&streaming_game), before);
    assert_eq!(disk(&streaming_game.world_root), source_disk);
    assert_eq!(disk(&stream_request.archive_root), archive);
    for family in [
        F::Falling,
        F::Paintings,
        F::ItemFrames,
        F::ArmorStands,
        F::Cushions,
        F::Others,
        F::Boats,
        F::Carts,
        F::Bolts,
        F::Crystals,
        F::Dragon,
        F::DragonFireballs,
        F::BreathClouds,
        F::Tridents,
        F::Rockets,
        F::OminousSpawners,
        F::Tnts,
        F::Mobs,
        F::Projectiles,
        F::Orbs,
        F::LeashKnots,
    ] {
        seed_family(&mut game, family);
        assert_entity_refusal(
            &mut game,
            &request,
            E::UnsupportedEntityFamily {
                dimension: Dim::Overworld,
                family,
            },
        );
        clear_family(&mut game, family);
    }
    game.world.loaded_entities.push(((0, 0), Vec::new()));
    assert_entity_refusal(
        &mut game,
        &request,
        E::LoadedEntities {
            dimension: Dim::Overworld,
        },
    );
    game.world.loaded_entities.clear();
    for kind in [
        persist::EntityMemoryKind::Uuids,
        persist::EntityMemoryKind::Raw,
        persist::EntityMemoryKind::Unknown,
        persist::EntityMemoryKind::Anonymous,
    ] {
        let mut native_payload = None;
        match kind {
            persist::EntityMemoryKind::Uuids => {
                game.persist.uuids.insert(713, [1, 2, 3, 4]);
            }
            persist::EntityMemoryKind::Raw => {
                game.persist
                    .raw
                    .insert(713, Tag::compound([("retained", Tag::Int(-73))]));
            }
            persist::EntityMemoryKind::Unknown | persist::EntityMemoryKind::Anonymous => {
                let id = if kind == persist::EntityMemoryKind::Unknown {
                    "fixture:opaque"
                } else {
                    "minecraft:experience_orb"
                };
                let mut payload = Tag::compound([
                    ("id", Tag::String(id.into())),
                    (
                        "fixture:opaque",
                        Tag::compound([
                            ("nested", Tag::List(vec![Tag::Int(-73), Tag::Long(97)])),
                            ("binary", Tag::ByteArray(vec![0, 128, 255])),
                        ]),
                    ),
                ]);
                if kind == persist::EntityMemoryKind::Anonymous {
                    payload.insert(
                        "Pos".into(),
                        Tag::List(vec![
                            Tag::Double(2.5f64.to_bits()),
                            Tag::Double(70f64.to_bits()),
                            Tag::Double(2.5f64.to_bits()),
                        ]),
                    );
                    payload.insert("Value".into(), Tag::Short(7));
                    payload.insert("Count".into(), Tag::Int(2));
                }
                // Unknown deliberately has no Pos, just like the qualified
                // defining fixture: a positioned custom id becomes a simulated
                // OtherEntity with Raw memory, rather than an unknown row.
                native_payload = Some(payload.clone());
                game.world.loaded_entities.push(((0, 0), vec![payload]));
                game.spawn_loaded_entities();
            }
        }
        assert_eq!(game.persist.checkpoint_entity_memory(), Some(kind));
        let keys = std::collections::HashSet::from([(0, 0)]);
        let unknown_before = if kind == persist::EntityMemoryKind::Unknown {
            assert!(game.persist.uuids.is_empty() && game.persist.raw.is_empty());
            assert!(game.others.is_empty() && game.orbs.is_empty());
            let rows = game.entity_chunks(&keys, false);
            assert_eq!(
                rows,
                vec![((0, 0), vec![native_payload.as_ref().unwrap().clone()])]
            );
            Some(rows)
        } else {
            None
        };
        let anonymous_pointer = if kind == persist::EntityMemoryKind::Anonymous {
            assert!(game.persist.uuids.is_empty() && game.persist.raw.is_empty());
            let (_, retained) =
                game.persist_anon("minecraft:experience_orb", Vec3::new(2.5, 70.0, 2.5));
            assert_eq!(retained, native_payload.as_ref());
            assert_eq!(game.orbs.len(), 1);
            assert_eq!((game.orbs[0].value, game.orbs[0].count), (7, 2));
            retained.map(|tag| tag as *const Tag as usize)
        } else {
            None
        };
        assert!(plain_stone(
            game.world.read_world_slot(&slot).unwrap().slot.stack(),
            7
        ));
        let heads = (
            game.world.counter_domain_snapshot(),
            game.world.persistent_world_slot_metadata(),
        );
        assert_entity_refusal(
            &mut game,
            &request,
            E::EntityMemory {
                dimension: Dim::Overworld,
                kind,
            },
        );
        assert_eq!(
            (
                game.world.counter_domain_snapshot(),
                game.world.persistent_world_slot_metadata()
            ),
            heads
        );
        assert_eq!(game.persist.checkpoint_entity_memory(), Some(kind));
        if let Some(before) = unknown_before {
            assert_eq!(game.entity_chunks(&keys, false), before);
            assert!(game.persist.uuids.is_empty() && game.persist.raw.is_empty());
        }
        if let Some(pointer) = anonymous_pointer {
            let (_, retained) =
                game.persist_anon("minecraft:experience_orb", Vec3::new(2.5, 70.0, 2.5));
            assert_eq!(retained, native_payload.as_ref());
            assert_eq!(
                retained.map(|tag| tag as *const Tag as usize),
                Some(pointer)
            );
            assert_eq!((game.orbs[0].value, game.orbs[0].count), (7, 2));
        }
        assert!(plain_stone(
            game.world.read_world_slot(&slot).unwrap().slot.stack(),
            7
        ));
        // Test fixture cleanup follows preservation assertions; slot owner stays in place.
        game.persist = Default::default();
        game.orbs.clear();
    }
    for (at, expected) in [
        (
            Vec3::new(f64::NAN, 70.0, 2.0),
            E::InvalidDroppedPosition {
                dimension: Dim::Overworld,
                id: 701,
            },
        ),
        (
            Vec3::new(200.0, 70.0, 200.0),
            E::OffResidentDropped {
                dimension: Dim::Overworld,
                id: 701,
            },
        ),
    ] {
        game.dropped.push(drop_item(701, at));
        assert_entity_refusal(&mut game, &request, expected);
        game.dropped.clear();
    }
    game.player = body;
    game.use_fishing_rod(&ItemStack::new("minecraft:fishing_rod", 1));
    assert!(game.hook.is_some());
    let _body = game.player.take();
    assert_entity_refusal(
        &mut game,
        &request,
        E::UnsupportedEntityFamily {
            dimension: Dim::Overworld,
            family: F::FishingHook,
        },
    );
    game.hook = None;
    game.end_portal_travel = true;
    assert_entity_refusal(&mut game, &request, E::PortalTravel);
    game.end_portal_travel = false;
    game.lan_acting = Some(11);
    assert_eq!(
        game.observe_slot_epoch_entity_owners().err(),
        Some(E::ActingGuest)
    );
    assert_eq!(
        dispatch_slot_capture(&mut game, request.clone())
            .unwrap_err()
            .code,
        Failure::Player
    );
    game.lan_acting = None;
    // All unchanged W1 staged/saved chunk categories still run after slot accounting.
    use horizon_client::{containers::BlockEntity, world::WorldOwnerKind as K};
    for kind in [
        K::StagedBlockEntities,
        K::StagedEntities,
        K::SavedBlockEntities,
        K::SavedBlockEntitySources,
        K::RolledLoot,
        K::OpaqueChunkRoot,
        K::Postprocess,
        K::SavedBlockTicks,
        K::SavedFluidTicks,
    ] {
        let original = game.world.chunks.get(&(-1, -1)).unwrap().clone();
        let mut chunk = (**game.world.chunks.get(&(-1, -1)).unwrap()).clone();
        let mut extras = ChunkExtras::default();
        let opaque = || Tag::compound([("retained", Tag::Int(-73))]);
        let tick = SavedTick {
            kind: TickTypeId(3),
            pos: BlockPos::new(-1, 71, -1),
            delay: -7,
            priority: TickPriority::High,
        };
        match kind {
            K::StagedBlockEntities => chunk
                .block_entities
                .push(((-1, 71, -1), BlockEntity::Chest(vec![Some(rich(7))]))),
            K::StagedEntities => chunk.entities.push(opaque()),
            K::SavedBlockEntities => extras.block_entities.push(opaque()),
            K::SavedBlockEntitySources => {
                extras.block_entity_sources.push(((-1, 71, -1), opaque()))
            }
            K::RolledLoot => extras.rolled_loot.push(RolledLoot {
                pos: (-1, 71, -1),
                table: "fixture:loot".into(),
                seed: Some(-3),
                items: opaque(),
            }),
            K::OpaqueChunkRoot => {
                extras.root.insert("fixture:opaque".into(), opaque());
            }
            K::Postprocess => chunk.postprocess.push([-1, 71, -1]),
            K::SavedBlockTicks => extras.block_ticks.push(tick),
            K::SavedFluidTicks => extras.fluid_ticks.push(tick),
            _ => unreachable!(),
        }
        chunk.saved = Some(Arc::new(extras));
        game.world.chunks.insert((-1, -1), Arc::new(chunk));
        let before = live(&game);
        let disk_before = disk(&game.world_root);
        assert!(game.observe_slot_epoch_entity_owners().is_ok());
        assert!(game.observe_checkpoint_world_owners().is_err());
        assert_eq!(
            dispatch_slot_capture(&mut game, request.clone())
                .unwrap_err()
                .code,
            Failure::World
        );
        assert_eq!(live(&game), before);
        assert_eq!(disk(&game.world_root), disk_before);
        game.world.chunks.insert((-1, -1), original);
    }
    game.level_events.push(LevelEvent::LevelEvent {
        id: 2001,
        pos: BlockPos::new(1, 70, 1),
        data: 1,
    });
    assert_eq!(
        dispatch_slot_capture(&mut game, request.clone())
            .unwrap_err()
            .code,
        Failure::World
    );
    assert_eq!(game.level_events.len(), 1);
    game.level_events.clear();
    assert!(plain_stone(
        game.world.read_world_slot(&slot).unwrap().slot.stack(),
        7
    ));
    world_surface_refusals(&mut game, &request);
    // Native arrivals loader proves the final E1 branch is not skipped.
    let arrival = Tag::compound([
        ("dimension", Tag::String(Dim::Nether.id().into())),
        ("x", Tag::Int(1)),
        ("y", Tag::Int(70)),
        ("z", Tag::Int(1)),
        (
            "entity",
            Tag::compound([("id", Tag::String("fixture:opaque".into()))]),
        ),
    ]);
    let path = game.world_root.join("data/horizon_portal_arrivals.dat");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    horizon_client::save::write_dat(
        &path,
        Tag::compound([("arrivals", Tag::List(vec![arrival]))]),
    )
    .unwrap();
    // Reset only the initially empty portal fixture before native loading creates an owner.
    game.portals = Default::default();
    game.load_portal_arrivals();
    assert_entity_refusal(&mut game, &request, E::PortalArrivals);
    // A separate actual Game retains arrivals above while native travel is tested.
    let travel_fixture = Fixture::new("slot-epoch-native-travel");
    let (
        mut travel,
        travel_body,
        _travel_rx,
        travel_identity,
        travel_binding,
        _travel_counter,
        _travel_held,
    ) = source(&travel_fixture);
    let travel_slot = seed_slot(&mut travel, &travel_binding);
    let travel_request = self::request(&travel, &travel_identity, 1);
    // Retained native-plan predicate fixture, as in the qualified defining E1
    // test. Completing the in-frame transition retires the counter and replaces
    // its World; this fixture makes no completed-transition or history claim.
    let body = travel_body.as_ref().unwrap();
    let plan = travel.portal_plan(
        BlockPos::new(2, 70, 2),
        body.position,
        body.dimensions.width,
        body.dimensions.height,
        true,
    );
    assert_eq!(plan.dim, Dim::Nether);
    assert!(travel.may_enter(plan.dim));
    assert!(travel.portals.travel.is_none());
    travel.portals.travel = Some(plan);
    let plan_before = format!("{:?}", travel.portals.travel);
    let plan_pointer = travel
        .portals
        .travel
        .as_ref()
        .map(|value| value as *const _ as usize);
    let heads = (
        travel.world.counter_domain_snapshot(),
        travel.world.persistent_world_slot_metadata(),
    );
    assert_eq!(record(&travel), (3, Some(T)));
    assert!(plain_stone(
        travel
            .world
            .read_world_slot(&travel_slot)
            .unwrap()
            .slot
            .stack(),
        7
    ));
    assert_entity_refusal(&mut travel, &travel_request, E::PortalTravel);
    assert_eq!(format!("{:?}", travel.portals.travel), plan_before);
    assert_eq!(
        travel
            .portals
            .travel
            .as_ref()
            .map(|value| value as *const _ as usize),
        plan_pointer
    );
    assert_eq!(
        (
            travel.world.counter_domain_snapshot(),
            travel.world.persistent_world_slot_metadata()
        ),
        heads
    );
    assert_eq!(record(&travel), (3, Some(T)));
    assert!(plain_stone(
        travel
            .world
            .read_world_slot(&travel_slot)
            .unwrap()
            .slot
            .stack(),
        7
    ));
    // Produce an actual extra parked level via the server's preserving swap;
    // the predicate fixture above did not perform a dimension transition.
    travel.enter_level(None, Dim::Nether).unwrap();
    travel.enter_level(None, Dim::Overworld).unwrap();
    let parked = travel.levels.parked.remove(&Dim::Nether).unwrap();
    travel.levels.parked.insert(Dim::Overworld, parked);
    assert_eq!(
        travel.observe_slot_epoch_entity_owners().err(),
        Some(E::DuplicateActiveDimension)
    );
}
#[test]
#[ignore = "requires isolated actual Game resources and coordinator-bound identities"]
fn slot_epoch_rejects_unsupported_and_detached_slot_modes() {
    for mode in [
        "none",
        "empty",
        "unsupported",
        "pending",
        "detached",
        "dormant",
        "faulted",
        "transient",
    ] {
        let fixture = Fixture::new("slot-epoch-modes");
        let (mut game, _body, _rx, identity, binding, counter, _held) = source(&fixture);
        let request = request(&game, &identity, 1);
        let mut retained = None;
        let mut teardown = None;
        match mode {
            "none" => {}
            "empty" => {
                game.world
                    .initialize_persistent_world_slot(&binding, &target(game.dim).unwrap())
                    .unwrap();
            }
            "dormant" => {
                game.world.set(2, 70, 2, game.res.facts().air).unwrap();
                game.world
                    .initialize_persistent_world_slot(&binding, &target(game.dim).unwrap())
                    .unwrap();
            }
            "transient" => {
                let config = game
                    .world
                    .counter_trial_snapshot()
                    .unwrap()
                    .configuration()
                    .clone();
                game.world.enable_world_slot_trial(config).unwrap();
            }
            _ => {
                let key = seed_slot(&mut game, &binding);
                match mode {
                    "unsupported" => {
                        let mut host = game.lan.take().unwrap();
                        game.as_guest(&mut host, 11, |g| {
                            g.transfer_world_slot_inventory_cell(&key, 0, Direction::IntoMachine, 1)
                        })
                        .unwrap()
                        .unwrap();
                        game.lan = Some(host);
                    }
                    "pending" | "detached" => {
                        game.world.set(2, 70, 2, game.res.facts().air).unwrap();
                        assert!(game.world.world_slot_snapshot().unwrap().pending.is_some());
                        if mode == "detached" {
                            retained = game.world.take_world_slot_retirement();
                            teardown = game.world.take_world_slot_trial();
                        }
                    }
                    "faulted" => {
                        game.world.game_time = T - 1;
                        game.tick_counter_trial(true);
                        assert!(game.world.read_persistent_counter(&counter).is_err());
                    }
                    _ => unreachable!(),
                }
            }
        }
        let metadata = game.world.persistent_world_slot_metadata();
        let state = format!("{:?}", game.world.world_slot_snapshot());
        let source_disk = disk(&game.world_root);
        let archive = disk(&request.archive_root);
        assert!(game.observe_slot_epoch_entity_owners().is_err());
        assert!(dispatch_slot_capture(&mut game, request.clone()).is_err());
        assert_eq!(game.world.persistent_world_slot_metadata(), metadata);
        assert_eq!(format!("{:?}", game.world.world_slot_snapshot()), state);
        assert_eq!(disk(&game.world_root), source_disk);
        assert_eq!(disk(&request.archive_root), archive);
        if mode == "detached" {
            assert!(game.world.world_slot_snapshot().is_none());
            assert!(game.world.has_world_slot_trial());
            assert_eq!(metadata.unwrap().status, SlotStatus::Detached);
            assert!(plain_stone(
                retained
                    .as_ref()
                    .unwrap()
                    .snapshot()
                    .allocation
                    .slot
                    .stack(),
                7
            ));
            assert!(teardown.as_ref().unwrap().active.is_none());
            assert!(matches!(
                game.observe_checkpoint_entity_owners(),
                Err(E::WorldSlotTrial { .. })
            ));
        }
    }
}

fn edit_slot(root: &Path, edit: impl FnOnce(&mut Tag)) {
    let bytes = fs::read(root.join(SLOT_MEMBER)).unwrap();
    let (mut named, used) = nbt::read_named(
        &bytes,
        Limits {
            max_depth: 32,
            max_nodes: 16_384,
            max_bytes: 1 << 20,
        },
    )
    .unwrap();
    assert_eq!(used, bytes.len());
    edit(&mut named.tag);
    let bytes = nbt::write_named(
        &named,
        Limits {
            max_depth: 32,
            max_nodes: 16_384,
            max_bytes: 1 << 20,
        },
    )
    .unwrap();
    rewrite(root, SLOT_MEMBER, &bytes);
}
fn two_epochs(game: &Game<'_>, request: &CaptureRequest, label: &str) -> (PathBuf, PathBuf) {
    let archive = game.world_root.parent().unwrap().join(label);
    fs::create_dir(&archive).unwrap();
    let old = archive.join("epoch-00000000000000000001");
    let newer = archive.join("epoch-00000000000000000002");
    copy_epoch(&published(request), &old);
    copy_epoch(&old, &newer);
    let mut manifest = slot_manifest(&newer);
    manifest.epoch = 2;
    put_manifest(&newer, &manifest);
    (archive, newer)
}
fn refused_before_copy(
    game: &Game<'_>,
    archive: &Path,
    identity: &Identity,
    binding: &NativeBinding,
    world: WorldId,
    label: &str,
) -> CaptureError {
    let destination = game.world_root.parent().unwrap().join(label);
    let before = disk(archive);
    let live_before = live(game);
    let source_before = disk(&game.world_root);
    let error = recover_slot_fixture_epoch(
        archive,
        identity,
        &destination,
        binding,
        game.res.items(),
        world,
        &target(Dim::Overworld).unwrap(),
    )
    .unwrap_err();
    assert!(!destination.exists());
    assert_eq!(disk(archive), before);
    assert_eq!(live(game), live_before);
    assert_eq!(disk(&game.world_root), source_before);
    error
}
#[test]
#[ignore = "requires isolated actual Game resources and coordinator-bound identities"]
fn slot_epoch_profile_matrix_and_semantic_corruption_refuse_before_copy() {
    let fixture = Fixture::new("slot-epoch-profile-matrix");
    let (mut game, _body, _rx, identity, binding, key, _held) = source(&fixture);
    let _slot = seed_slot(&mut game, &binding);
    let request = request(&game, &identity, 1);
    dispatch_slot_capture(&mut game, request.clone()).unwrap();
    let image = published(&request);
    let frozen = disk(&image);
    let world = key.placement().world;
    for (index, case) in [
        "path-missing",
        "path-type",
        "path-int",
        "path-value",
        "member-unlisted",
        "member-and-file-missing",
        "frame-unlisted",
        "frame-and-file-missing",
        "player-descriptor-uuid",
        "unknown-field",
        "duplicate-path",
        "schema",
        "identity",
    ]
    .into_iter()
    .enumerate()
    {
        let (archive, newer) = two_epochs(&game, &request, &format!("profile-{index}"));
        let mut value: Value = parse_json(
            &fs::read(newer.join("manifest.json")).unwrap(),
            MANIFEST_LIMIT,
        )
        .unwrap();
        match case {
            "path-missing" => {
                value.as_object_mut().unwrap().remove("slot_path");
            }
            "path-type" => value["slot_path"] = Value::Bool(true),
            "path-int" => value["slot_path"] = Value::Number(7.into()),
            "path-value" => value["slot_path"] = Value::String(FRAME_PATH.into()),
            "member-unlisted" | "member-and-file-missing" => {
                value["files"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|m| m["path"] != SLOT_MEMBER);
                if case == "member-and-file-missing" {
                    fs::remove_file(newer.join(SLOT_MEMBER)).unwrap();
                }
            }
            "frame-unlisted" | "frame-and-file-missing" => {
                value["files"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|m| m["path"] != FRAME_PATH);
                if case == "frame-and-file-missing" {
                    fs::remove_file(newer.join(FRAME_PATH)).unwrap();
                }
            }
            "player-descriptor-uuid" => value["players"][0]["record_uuid"] = "invalid-uuid".into(),
            "unknown-field" => value["surprise"] = true.into(),
            "schema" => value["schema"] = 3.into(),
            "identity" => value["identity"]["test_binary"] = Value::String("e".repeat(64)),
            "duplicate-path" => {}
            _ => unreachable!(),
        }
        let mut bytes = serde_json::to_vec(&value).unwrap();
        if case == "duplicate-path" {
            bytes.pop();
            bytes.extend_from_slice(format!(",\"slot_path\":\"{SLOT_MEMBER}\"}}").as_bytes());
        }
        fs::write(newer.join("manifest.json"), bytes).unwrap();
        let error = refused_before_copy(
            &game,
            &archive,
            &identity,
            &binding,
            world,
            &format!("profile-dest-{index}"),
        );
        assert_eq!(
            error.code,
            if case == "identity" {
                Failure::Identity
            } else {
                Failure::Profile
            },
            "{case}"
        );
        // An older semantic refusal is not promoted over a newer valid whole image.
        if case == "path-missing" {
            let third = archive.join("epoch-00000000000000000003");
            copy_epoch(&image, &third);
            let mut m = slot_manifest(&third);
            m.epoch = 3;
            put_manifest(&third, &m);
            let dest = game
                .world_root
                .parent()
                .unwrap()
                .join("newer-valid-profile");
            let selected = recover_slot_fixture_epoch(
                &archive,
                &identity,
                &dest,
                &binding,
                game.res.items(),
                world,
                &target(game.dim).unwrap(),
            )
            .unwrap();
            assert_eq!(selected.receipt.epoch, 3);
            assert_same_selected_world(&third, &dest);
        }
    }
    for (index, case) in [
        "invalid-nbt",
        "world",
        "dimension",
        "position",
        "package",
        "type",
        "generation",
        "slot-generation",
        "time",
        "machine-head",
        "slot-head",
        "revision",
        "count",
        "counter-clock",
        "archive-counter",
        "archive-save-schema",
        "archive-kind",
        "archive-resident",
        "player-record-uuid",
        "native-clock",
        "player-share",
    ]
    .into_iter()
    .enumerate()
    {
        let (archive, newer) = two_epochs(&game, &request, &format!("semantic-{index}"));
        match case {
            "invalid-nbt" => rewrite(&newer, SLOT_MEMBER, b"invalid slot NBT"),
            "world" => edit_slot(&newer, |tag| {
                tag.insert(
                    "world_uuid".into(),
                    Tag::String("00000000-0000-4000-8000-000000000123".into()),
                )
            }),
            "dimension" => edit_slot(&newer, |tag| {
                tag.insert("dimension".into(), Tag::String(Dim::Nether.id().into()))
            }),
            "position" => edit_slot(&newer, |tag| {
                tag.insert("position".into(), Tag::IntArray(vec![3, 70, 2]))
            }),
            "package" => edit_slot(&newer, |tag| {
                tag.insert(
                    "package_identity".into(),
                    Tag::String("0".repeat(64).as_str().into()),
                )
            }),
            "type" => edit_slot(&newer, |tag| {
                tag.insert(
                    "type_id".into(),
                    Tag::String("capture_counter:other".into()),
                )
            }),
            "generation" => edit_slot(&newer, |tag| {
                tag.get_mut("ownership").unwrap().insert(
                    "machine_generation".into(),
                    Tag::ByteArray(0u64.to_be_bytes().to_vec()),
                )
            }),
            "slot-generation" => edit_slot(&newer, |tag| {
                tag.insert(
                    "slot_high_water".into(),
                    Tag::ByteArray(2u64.to_be_bytes().to_vec()),
                );
                tag.get_mut("ownership").unwrap().insert(
                    "slot_generation".into(),
                    Tag::ByteArray(2u64.to_be_bytes().to_vec()),
                )
            }),
            "time" => edit_slot(&newer, |tag| {
                tag.insert("saved_game_time".into(), Tag::Long(T - 1))
            }),
            "machine-head" => edit_slot(&newer, |tag| {
                tag.insert(
                    "machine_high_water".into(),
                    Tag::ByteArray(2u64.to_be_bytes().to_vec()),
                )
            }),
            "slot-head" => edit_slot(&newer, |tag| {
                tag.insert(
                    "slot_high_water".into(),
                    Tag::ByteArray(2u64.to_be_bytes().to_vec()),
                )
            }),
            "revision" => edit_slot(&newer, |tag| {
                tag.get_mut("ownership").unwrap().insert(
                    "revision".into(),
                    Tag::ByteArray(2u64.to_be_bytes().to_vec()),
                )
            }),
            "count" => edit_slot(&newer, |tag| {
                tag.get_mut("ownership")
                    .unwrap()
                    .get_mut("stack")
                    .unwrap()
                    .get_mut("value")
                    .unwrap()
                    .insert("count".into(), Tag::Int(6))
            }),
            "counter-clock" => {
                let mut frame: Value =
                    parse_json(&fs::read(newer.join(FRAME_PATH)).unwrap(), 16 << 10).unwrap();
                frame["saved_game_time"] = (T - 1).into();
                rewrite(&newer, FRAME_PATH, &serde_json::to_vec(&frame).unwrap());
            }
            "archive-counter" => {
                let mut archive: Value = parse_json(
                    &fs::read(newer.join("research/counter.json")).unwrap(),
                    JSON_LIMIT,
                )
                .unwrap();
                archive["counter"] = 4.into();
                rewrite(
                    &newer,
                    "research/counter.json",
                    &serde_json::to_vec(&archive).unwrap(),
                );
            }
            "archive-save-schema" | "archive-kind" | "archive-resident" => {
                let mut archive: Value = parse_json(
                    &fs::read(newer.join("research/counter.json")).unwrap(),
                    JSON_LIMIT,
                )
                .unwrap();
                match case {
                    "archive-save-schema" => archive["save_schema"] = 2.into(),
                    "archive-kind" => archive["kind"] = "unsupported-archive".into(),
                    "archive-resident" => archive["resident"] = false.into(),
                    _ => unreachable!(),
                }
                rewrite(
                    &newer,
                    "research/counter.json",
                    &serde_json::to_vec(&archive).unwrap(),
                );
            }
            "player-record-uuid" => {
                let relative = format!("world/players/data/{}.dat", uuid_text(OWNER_RECORD));
                let mut player = strict_player(&fs::read(newer.join(&relative)).unwrap()).unwrap();
                player.insert("UUID".into(), Tag::IntArray(vec![0, 0, 0, 19]));
                rewrite(&newer, &relative, &player_bytes(&player).unwrap());
            }
            "native-clock" => {
                let mut level =
                    strict_player(&fs::read(newer.join("world/level.dat")).unwrap()).unwrap();
                level
                    .get_mut("Data")
                    .unwrap()
                    .insert("Time".into(), Tag::Long(T - 1));
                rewrite(&newer, "world/level.dat", &player_bytes(&level).unwrap());
            }
            "player-share" => {
                let relative = format!("world/players/data/{}.dat", uuid_text(OWNER_RECORD));
                let mut player = strict_player(&fs::read(newer.join(&relative)).unwrap()).unwrap();
                let Some(Tag::List(items)) = player.get_mut("Inventory") else {
                    panic!();
                };
                items
                    .iter_mut()
                    .find(|t| t.get("Slot") == Some(&Tag::Byte(0)))
                    .unwrap()
                    .insert("count".into(), Tag::Int(4));
                rewrite(&newer, &relative, &player_bytes(&player).unwrap());
            }
            _ => unreachable!(),
        }
        let ranked_semantic = matches!(
            case,
            "archive-save-schema" | "archive-kind" | "archive-resident" | "player-record-uuid"
        );
        if ranked_semantic {
            assert_eq!(
                verify_complete_envelope(&newer, 2, &identity)
                    .err()
                    .unwrap()
                    .code,
                Failure::Profile
            );
        } else {
            verify_complete_envelope(&newer, 2, &identity).unwrap();
        }
        let error = refused_before_copy(
            &game,
            &archive,
            &identity,
            &binding,
            world,
            &format!("semantic-dest-{index}"),
        );
        if ranked_semantic {
            assert_eq!(error.code, Failure::Profile);
        }
    }
    // Declared physical corruption is different: choose one complete older WHOLE epoch.
    for (index, (relative, missing)) in [
        (SLOT_MEMBER, false),
        (SLOT_MEMBER, true),
        (FRAME_PATH, false),
        (FRAME_PATH, true),
    ]
    .into_iter()
    .enumerate()
    {
        let (archive, newer) = two_epochs(&game, &request, &format!("physical-{index}"));
        if missing {
            fs::remove_file(newer.join(relative)).unwrap();
        } else {
            fs::write(newer.join(relative), b"hash corruption").unwrap();
        }
        let dest = game
            .world_root
            .parent()
            .unwrap()
            .join(format!("physical-dest-{index}"));
        let selected = recover_slot_fixture_epoch(
            &archive,
            &identity,
            &dest,
            &binding,
            game.res.items(),
            world,
            &target(game.dim).unwrap(),
        )
        .unwrap();
        assert_eq!(selected.receipt.epoch, 1);
        assert_eq!(selected.receipt.rejected.len(), 1);
        assert_same_selected_world(&image, &dest);
        canonical_players(&dest);
    }
    // Old wrappers cannot copy an image while dropping the selected slot owner.
    for (index, profile) in [RecoveryProfile::ArchiveOnly, RecoveryProfile::CounterOnly]
        .into_iter()
        .enumerate()
    {
        let dest = game
            .world_root
            .parent()
            .unwrap()
            .join(format!("legacy-wrapper-{index}"));
        let before = disk(&request.archive_root);
        assert_eq!(
            select_fixture_epoch_for(&request.archive_root, &identity, &dest, profile)
                .err()
                .unwrap()
                .code,
            Failure::Profile
        );
        assert!(!dest.exists());
        assert_eq!(disk(&request.archive_root), before);
    }
    let archive = game
        .world_root
        .parent()
        .unwrap()
        .join("legacy-counter-only");
    fs::create_dir(&archive).unwrap();
    let old = archive.join("epoch-00000000000000000001");
    copy_epoch(&image, &old);
    let mut m = slot_manifest(&old);
    m.files.retain(|m| m.path != SLOT_MEMBER);
    fs::remove_file(old.join(SLOT_MEMBER)).unwrap();
    let v1 = Manifest {
        schema: 1,
        kind: "fresh-native-capture-cell".into(),
        epoch: 1,
        identity: m.identity,
        players: m.players,
        dimensions: m.dimensions,
        counter: m.counter,
        directories: m.directories,
        files: m.files,
    };
    fs::write(
        old.join("manifest.json"),
        json_bytes(&v1, MANIFEST_LIMIT).unwrap(),
    )
    .unwrap();
    verify_image(&old, 1, &identity).unwrap();
    refused_before_copy(
        &game,
        &archive,
        &identity,
        &binding,
        world,
        "legacy-no-slot-dest",
    );
    let native_dest = game
        .world_root
        .parent()
        .unwrap()
        .join("legacy-counter-compatible");
    assert!(
        super::super::recover_machine_fixture_epoch(
            &archive,
            &identity,
            &native_dest,
            &binding,
            world,
            &target(game.dim).unwrap()
        )
        .is_ok()
    );
    // A v1 envelope may physically describe the reserved bytes, but none of the
    // recovery wrappers may use that legacy header to ignore their slot owner.
    let reserved_archive = game
        .world_root
        .parent()
        .unwrap()
        .join("legacy-reserved-slot");
    fs::create_dir(&reserved_archive).unwrap();
    let reserved = reserved_archive.join("epoch-00000000000000000001");
    copy_epoch(&image, &reserved);
    let mut reserved_v1 = v1.clone();
    reserved_v1.files = slot_manifest(&reserved).files;
    fs::write(
        reserved.join("manifest.json"),
        json_bytes(&reserved_v1, MANIFEST_LIMIT).unwrap(),
    )
    .unwrap();
    verify_image(&reserved, 1, &identity).unwrap(); // v1 decoder semantics stay exact.
    for (index, profile) in [
        RecoveryProfile::ArchiveOnly,
        RecoveryProfile::CounterOnly,
        RecoveryProfile::ActiveSlot,
    ]
    .into_iter()
    .enumerate()
    {
        let destination = game
            .world_root
            .parent()
            .unwrap()
            .join(format!("reserved-v1-destination-{index}"));
        let before = disk(&reserved_archive);
        assert_eq!(
            select_fixture_epoch_for(&reserved_archive, &identity, &destination, profile)
                .err()
                .unwrap()
                .code,
            Failure::Profile
        );
        assert!(!destination.exists());
        assert_eq!(disk(&reserved_archive), before);
    }
    // Genuine archive-only v1 needs no V2 checkpoint. Archive recovery is allowed;
    // both native owner restore profiles refuse before a destination is created.
    let archival_archive = game
        .world_root
        .parent()
        .unwrap()
        .join("legacy-archive-only");
    fs::create_dir(&archival_archive).unwrap();
    let archival = archival_archive.join("epoch-00000000000000000001");
    copy_epoch(&old, &archival);
    let mut archival_v1 = v1.clone();
    archival_v1.files.retain(|member| member.path != FRAME_PATH);
    fs::remove_file(archival.join(FRAME_PATH)).unwrap();
    fs::write(
        archival.join("manifest.json"),
        json_bytes(&archival_v1, MANIFEST_LIMIT).unwrap(),
    )
    .unwrap();
    verify_image(&archival, 1, &identity).unwrap();
    let archive_destination = game
        .world_root
        .parent()
        .unwrap()
        .join("legacy-archive-compatible");
    let archival_receipt =
        recover_fixture_epoch(&archival_archive, &identity, &archive_destination).unwrap();
    assert_eq!(archival_receipt.epoch, 1);
    assert_same_selected_world(&archival, &archive_destination);
    for (index, profile) in [RecoveryProfile::CounterOnly, RecoveryProfile::ActiveSlot]
        .into_iter()
        .enumerate()
    {
        let destination = game
            .world_root
            .parent()
            .unwrap()
            .join(format!("archive-only-native-refusal-{index}"));
        let before = disk(&archival_archive);
        assert_eq!(
            select_fixture_epoch_for(&archival_archive, &identity, &destination, profile)
                .err()
                .unwrap()
                .code,
            Failure::Profile
        );
        assert!(!destination.exists());
        assert_eq!(disk(&archival_archive), before);
    }
    assert_eq!(disk(&image), frozen);
}
// Test-only edits name exact native NBT fields; no alternate decoder or wire format.
trait TagFields {
    fn insert(&mut self, key: horizon_world::nbt::JavaString, value: Tag);
}
impl TagFields for Tag {
    fn insert(&mut self, key: horizon_world::nbt::JavaString, value: Tag) {
        let Tag::Compound(map) = self else {
            panic!("compound field edit")
        };
        map.insert(key, value);
    }
}
fn pending_slot(game: &mut Game<'_>, request: CaptureRequest) -> PendingImage {
    let (counter, slot) = game.slot_epoch_projection().unwrap();
    let material = game
        .profile_material(request, CaptureProfile::NativeSlotPatch)
        .unwrap();
    let mut image = material.copy().unwrap();
    image.overlay().unwrap();
    write_bytes(
        &make_parents(&image.root, FRAME_PATH, Phase::Overlay).unwrap(),
        &counter.encode().unwrap(),
        true,
        Phase::Overlay,
    )
    .unwrap();
    write_bytes(
        &make_parents(&image.root, SLOT_MEMBER, Phase::Overlay).unwrap(),
        &slot,
        true,
        Phase::Overlay,
    )
    .unwrap();
    image
}
fn finish_slot(image: &mut PendingImage) {
    image.sync_members().unwrap();
    image
        .write_manifest_temp_for(RecoveryProfile::ActiveSlot)
        .unwrap();
    image.finish_manifest().unwrap();
}
#[test]
#[ignore = "requires isolated actual Game resources and coordinator-bound identities"]
fn slot_epoch_whole_image_interruptions_and_definite_completion() {
    let fixture = Fixture::new("slot-epoch-interruptions");
    let (mut game, _body, _rx, identity, binding, key, _held) = source(&fixture);
    let _slot = seed_slot(&mut game, &binding);
    let request = request(&game, &identity, 1);
    let source_before = live(&game);
    // Real ten-second observer timeout still receives the Task's definite completion.
    let started = std::time::Instant::now();
    let receipt = dispatch_slot_capture_after_observer_timeout(&mut game, request.clone()).unwrap();
    assert!(started.elapsed() >= Duration::from_secs(10));
    assert_eq!(receipt.epoch, 1);
    assert_eq!(live(&game), source_before);
    let frozen = disk(&published(&request));
    // Failed pending-directory creation cannot replace prior contents or publish.
    let next = CaptureRequest {
        epoch: 2,
        ..request.clone()
    };
    let material = game
        .profile_material(next.clone(), CaptureProfile::NativeSlotPatch)
        .unwrap();
    let pending = next.archive_root.join(".pending-00000000000000000002");
    fs::create_dir(&pending).unwrap();
    fs::write(pending.join("retained"), b"retained").unwrap();
    let stage = disk(&request.archive_root);
    assert!(material.copy().is_err());
    assert_eq!(disk(&request.archive_root), stage);
    fs::remove_file(pending.join("retained")).unwrap();
    fs::remove_dir(&pending).unwrap();
    // Real slot overlay obstruction: retain the copied world and all existing bytes.
    let material = game
        .profile_material(next, CaptureProfile::NativeSlotPatch)
        .unwrap();
    let mut image = material.copy().unwrap();
    image.overlay().unwrap();
    let obstruction = make_parents(&image.root, SLOT_MEMBER, Phase::Overlay).unwrap();
    fs::create_dir(&obstruction).unwrap();
    let stage = disk(&image.root);
    let bytes = game.slot_epoch_projection().unwrap().1;
    assert!(write_bytes(&obstruction, &bytes, true, Phase::Overlay).is_err());
    assert_eq!(disk(&image.root), stage);
    // Manifest-temp and final-rename obstructions use real filesystem operations.
    let mut image = pending_slot(
        &mut game,
        CaptureRequest {
            epoch: 3,
            ..request.clone()
        },
    );
    fs::create_dir(image.root.join("manifest.json.tmp")).unwrap();
    let stage = disk(&image.root);
    assert!(
        image
            .write_manifest_temp_for(RecoveryProfile::ActiveSlot)
            .is_err()
    );
    assert_eq!(disk(&image.root), stage);
    let mut image = pending_slot(
        &mut game,
        CaptureRequest {
            epoch: 4,
            ..request.clone()
        },
    );
    finish_slot(&mut image);
    let target = request.archive_root.join("epoch-00000000000000000004");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("retained"), b"unrelated obstruction").unwrap();
    let stage = disk(&request.archive_root);
    let error = image
        .rename_published_for(RecoveryProfile::ActiveSlot)
        .err()
        .unwrap();
    assert_eq!(error.publication, Publication::NotPublished);
    assert_eq!(disk(&request.archive_root), stage);
    // Real rename succeeds; a subsequent archive-directory sync failure is uncertain.
    let mut image = pending_slot(
        &mut game,
        CaptureRequest {
            epoch: 5,
            ..request.clone()
        },
    );
    finish_slot(&mut image);
    let promoted = image
        .rename_published_for(RecoveryProfile::ActiveSlot)
        .unwrap();
    let moved = request.archive_root.with_file_name("slot-archive-held");
    fs::rename(&request.archive_root, &moved).unwrap();
    fs::write(&request.archive_root, b"directory sync obstruction").unwrap();
    let error = promoted.acknowledge().unwrap_err();
    assert_eq!(error.publication, Publication::Uncertain);
    fs::remove_file(&request.archive_root).unwrap();
    fs::rename(&moved, &request.archive_root).unwrap();
    verify_complete_envelope(
        &request.archive_root.join("epoch-00000000000000000005"),
        5,
        &identity,
    )
    .unwrap();
    for phase in ["copy", "before-rename", "after-rename"] {
        fail_stop_slot_publication(&game, &request, &key, phase);
    }
    assert_eq!(disk(&published(&request)), frozen);
    assert_eq!(live(&game), source_before);
}
#[test]
#[ignore = "requires isolated actual Game resources and coordinator-bound identities"]
fn slot_epoch_private_install_failure_and_fresh_authority() {
    let fixture = Fixture::new("slot-epoch-private-install");
    let (mut game, _body, _rx, identity, binding, key, _held) = source(&fixture);
    let slot = seed_slot(&mut game, &binding);
    let request = request(&game, &identity, 1);
    dispatch_slot_capture(&mut game, request.clone()).unwrap();
    let before = live(&game);
    let frozen = disk(&published(&request));
    let limits = ResolvedLimits::new(64, 64).unwrap();
    let mut recipient = SlotProjection::new(None);
    let old_plan = match game
        .world
        .prepare_world_slot_transfer(&slot, &recipient, Direction::IntoPlayer, 1, limits)
        .unwrap()
    {
        Preparation::Prepared(plan) => *plan,
        other => panic!("{other:?}"),
    };
    let destination = game.world_root.parent().unwrap().join("private-partial");
    let recovery = recover(
        &request,
        &destination,
        &game,
        &binding,
        key.placement().world,
    );
    let header = recovery.slot.view().package_identity.as_ptr() as usize;
    let (mut partial, _rx2) = loaded(&fixture, &destination);
    // Counter install can succeed before an occupied native slot refuses. Keep both
    // the partial Game and the exact move-only candidate private and owned here.
    partial
        .world
        .enable_world_slot_trial(
            game.world
                .counter_trial_snapshot()
                .unwrap()
                .configuration()
                .clone(),
        )
        .unwrap();
    let rejected = partial
        .install_slot_epoch(
            &binding,
            key.placement().world,
            &recovery.counter,
            recovery.slot,
        )
        .unwrap_err();
    assert_eq!(record(&partial), (3, Some(T)));
    assert_eq!(
        rejected.slot.view().package_identity.as_ptr() as usize,
        header
    );
    assert!(
        matches!(rejected.slot.view().allocation,SlotFrameAllocation::Active {stack:Some(stack),revision:1,..} if plain_stone(Some(stack),7))
    );
    assert_eq!(rejected.error.publication, Publication::NotPublished);
    assert!(partial.world.persistent_world_slot_metadata().is_none());
    assert!(
        partial
            .world
            .world_slot_snapshot()
            .unwrap()
            .active
            .unwrap()
            .slot
            .stack()
            .is_none()
    );
    let destination2 = game.world_root.parent().unwrap().join("fresh-authority");
    let recovery2 = recover(
        &request,
        &destination2,
        &game,
        &binding,
        key.placement().world,
    );
    let (mut restored, _rx3) = loaded(&fixture, &destination2);
    let (fresh_counter, fresh_slot) = restored
        .install_slot_epoch(
            &binding,
            key.placement().world,
            &recovery2.counter,
            recovery2.slot,
        )
        .unwrap();
    assert_ne!(fresh_counter.session_nonce(), key.session_nonce());
    assert_ne!(fresh_slot, slot);
    assert_eq!(fresh_slot.incarnation(), 1);
    assert_eq!(fresh_counter.placement().generation, 1);
    assert_ne!(fresh_slot.scope(), slot.scope());
    assert!(
        restored
            .world
            .commit_world_slot_transfer(old_plan, &mut recipient, limits)
            .is_err()
    );
    assert!(recipient.snapshot().stack().is_none());
    assert!(plain_stone(
        restored
            .world
            .read_world_slot(&fresh_slot)
            .unwrap()
            .slot
            .stack(),
        7
    ));
    let destination3 = game
        .world_root
        .parent()
        .unwrap()
        .join("wrong-native-clock-install");
    let recovery3 = recover(
        &request,
        &destination3,
        &game,
        &binding,
        key.placement().world,
    );
    let address = recovery3.slot.view().package_identity.as_ptr() as usize;
    let (mut wrong_clock, _rx4) = loaded(&fixture, &destination3);
    wrong_clock.world.game_time = T - 1;
    let clock_rejected = wrong_clock
        .install_slot_epoch(
            &binding,
            key.placement().world,
            &recovery3.counter,
            recovery3.slot,
        )
        .unwrap_err();
    assert_eq!(
        clock_rejected.slot.view().package_identity.as_ptr() as usize,
        address
    );
    assert!(wrong_clock.world.counter_domain_snapshot().is_none());
    assert!(wrong_clock.world.world_slot_snapshot().is_none());
    assert!(
        matches!(clock_rejected.slot.view().allocation,SlotFrameAllocation::Active {stack:Some(stack),..} if stack.count==7)
    );
    let independent = WorldId::parse("00000000-0000-4000-8000-000000000123").unwrap();
    refused_before_copy(
        &game,
        &request.archive_root,
        &identity,
        &binding,
        independent,
        "wrong-independent-world",
    );
    let wrong_host = CounterTarget::new(Dim::Overworld.id(), [3, 70, 2]).unwrap();
    let absent = game
        .world_root
        .parent()
        .unwrap()
        .join("wrong-independent-target");
    assert!(
        recover_slot_fixture_epoch(
            &request.archive_root,
            &identity,
            &absent,
            &binding,
            game.res.items(),
            key.placement().world,
            &wrong_host
        )
        .is_err()
    );
    assert!(!absent.exists());
    assert_eq!(disk(&published(&request)), frozen);
    assert_eq!(live(&game), before);
    assert!(
        matches!(rejected.slot.view().allocation,SlotFrameAllocation::Active {stack:Some(stack),..} if stack.count==7)
    );
}

fn isolated_child_restore(game: &Game<'_>, request: &CaptureRequest, key: &RuntimeCounterKey) {
    let parent = game.world_root.parent().unwrap();
    let plan = ChildPlan {
        context: parent.join("slot-child-restore"),
        archive: request.archive_root.clone(),
        destination: parent.join("slot-child-world"),
        content: parent.join("counter-content"),
        identity: request.identity.clone(),
        world: key.placement().world.to_string(),
        mode: "restore".into(),
    };
    let before = disk(&request.archive_root);
    let output = child_run(&plan, false);
    assert_eq!(output["world"], plan.world);
    assert_eq!(output["slot_count"], 7);
    assert_eq!(output["player_count"], 3);
    assert_eq!(output["before"], 3);
    assert_eq!(output["after"], 4);
    assert_eq!(output["last_advanced"], T + 1);
    assert_eq!(output["generation"], 1);
    let old_session: String = key
        .session_nonce()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_ne!(output["session"], old_session);
    canonical_players(&plan.destination);
    assert_eq!(disk(&request.archive_root), before);
}
fn fail_stop_slot_publication(
    game: &Game<'_>,
    request: &CaptureRequest,
    key: &RuntimeCounterKey,
    phase: &str,
) {
    let parent = game.world_root.parent().unwrap();
    let archive = parent.join(format!("slot-stop-{phase}"));
    fs::create_dir(&archive).unwrap();
    let old = archive.join("epoch-00000000000000000001");
    copy_epoch(&published(request), &old);
    let frozen = disk(&old);
    let pending = archive.join(".pending-00000000000000000002");
    if phase != "copy" {
        copy_epoch(&old, &pending);
        let mut manifest = slot_manifest(&pending);
        manifest.epoch = 2;
        put_manifest(&pending, &manifest);
    }
    let before = disk(&archive);
    let plan = ChildPlan {
        context: parent.join(format!("slot-child-{phase}")),
        archive: archive.clone(),
        destination: parent.join(format!("slot-post-stop-{phase}")),
        content: parent.join("counter-content"),
        identity: request.identity.clone(),
        world: key.placement().world.to_string(),
        mode: phase.into(),
    };
    let signal = child_run(&plan, true);
    assert_eq!(signal["mode"], phase);
    let after = disk(&archive);
    let expected = match phase {
        "after-rename" => before
            .into_iter()
            .map(|(p, v)| {
                let p = if p.starts_with(&pending) {
                    archive
                        .join("epoch-00000000000000000002")
                        .join(p.strip_prefix(&pending).unwrap())
                } else {
                    p
                };
                (p, v)
            })
            .collect(),
        "copy" => {
            let detached = plan.context.join("detached-copy-source");
            let expected: Disk = disk(&old.join("world"))
                .into_iter()
                .map(|(p, v)| (detached.join(p.strip_prefix(old.join("world")).unwrap()), v))
                .collect();
            assert_eq!(disk(&detached), expected);
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
    assert_eq!(disk(&old), frozen);
    let binding = fixture_binding(game).unwrap();
    let recovery = recover_slot_fixture_epoch(
        &archive,
        &request.identity,
        &plan.destination,
        &binding,
        game.res.items(),
        key.placement().world,
        &target(game.dim).unwrap(),
    )
    .unwrap();
    assert_eq!(
        recovery.receipt.epoch,
        if phase == "after-rename" { 2 } else { 1 }
    );
    assert!(
        matches!(recovery.slot.view().allocation,SlotFrameAllocation::Active {stack:Some(stack),..} if stack.count==7)
    );
    canonical_players(&plan.destination);
    assert_eq!(disk(&archive), after);
}
#[test]
#[ignore = "AUXILIARY: explicit bounded parent plan and isolated process only; never a primary"]
fn slot_epoch_process_child() {
    let encoded = std::env::var("HORIZON_SLOT_EPOCH_CHILD_PLAN")
        .expect("auxiliary requires explicit parent plan");
    assert!(encoded.len() <= 8192);
    let plan: ChildPlan = parse_json(encoded.as_bytes(), 8192).unwrap();
    assert!(std::env::var_os("HORIZON_SESSION_FILE").is_none());
    assert_eq!(std::env::current_dir().unwrap(), plan.context);
    let world = WorldId::parse(&plan.world).unwrap();
    let fixture = Fixture::new("slot-epoch-child");
    let game = fixture.game();
    let bundle = horizon_mods::paired::PreparedBundle::read_folder(&plan.content).unwrap();
    let binding =
        NativeBinding::from_prepared(&bundle, "capture_counter:type", game.pack.assets.registry)
            .unwrap();
    assert_eq!(
        digest_identity(binding.package_identity()),
        plan.identity.counter_content
    );
    let signal = match plan.mode.as_str() {
        "restore" => {
            let recovery = recover_slot_fixture_epoch(
                &plan.archive,
                &plan.identity,
                &plan.destination,
                &binding,
                game.res.items(),
                world,
                &target(Dim::Overworld).unwrap(),
            )
            .unwrap();
            let (mut restored, _rx) = loaded(&fixture, &plan.destination);
            let (counter, slot) = restored
                .install_slot_epoch(&binding, world, &recovery.counter, recovery.slot)
                .unwrap();
            assert_eq!(record(&restored), (3, Some(T)));
            restored.tick_counter_trial(true);
            assert_eq!(record(&restored), (3, Some(T)));
            restored.world.game_time = T + 1;
            restored.tick_counter_trial(true);
            assert_eq!(record(&restored), (4, Some(T + 1)));
            let count = restored
                .world
                .read_world_slot(&slot)
                .unwrap()
                .slot
                .stack()
                .unwrap()
                .count;
            let player = restored
                .lan
                .as_ref()
                .unwrap()
                .hub
                .guests
                .iter()
                .find(|g| g.uuid == 11)
                .unwrap()
                .inventory[0]
                .as_ref()
                .unwrap()
                .count;
            let session: String = counter
                .session_nonce()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            serde_json::json!({"mode":"restore","world":counter.placement().world.to_string(),"generation":counter.placement().generation,"slot_count":count,"player_count":player,"session":session,"before":3,"after":4,"last_advanced":T+1})
        }
        "copy" => {
            let old = plan.archive.join("epoch-00000000000000000001");
            let envelope = verify_complete_envelope(&old, 1, &plan.identity).unwrap();
            check_recovery_profile(
                &envelope.manifest,
                envelope.slot_manifest.as_ref(),
                RecoveryProfile::ActiveSlot,
            )
            .unwrap();
            read_slot_image(
                &old,
                &envelope.manifest,
                &binding,
                game.res.items(),
                world,
                &target(Dim::Overworld).unwrap(),
            )
            .unwrap();
            let counter: CounterArchive = parse_json(
                &fs::read(old.join("research/counter.json")).unwrap(),
                JSON_LIMIT,
            )
            .unwrap();
            let source = plan.context.join("detached-copy-source");
            copy_epoch(&old.join("world"), &source);
            let before = disk(&source);
            let material = Material {
                source: source.clone(),
                request: CaptureRequest {
                    archive_root: plan.archive.clone(),
                    epoch: 2,
                    identity: plan.identity.clone(),
                },
                players: Vec::new(),
                counter,
                dimensions: envelope.manifest.dimensions,
            };
            let _incomplete = material.copy().unwrap();
            assert_eq!(disk(&source), before);
            serde_json::json!({"mode":"copy"})
        }
        "before-rename" | "after-rename" => {
            let root = plan.archive.join(".pending-00000000000000000002");
            let envelope = verify_complete_envelope(&root, 2, &plan.identity).unwrap();
            read_slot_image(
                &root,
                &envelope.manifest,
                &binding,
                game.res.items(),
                world,
                &target(Dim::Overworld).unwrap(),
            )
            .unwrap();
            let counter: CounterArchive = parse_json(
                &fs::read(root.join("research/counter.json")).unwrap(),
                JSON_LIMIT,
            )
            .unwrap();
            let manifest_bytes = fs::read(root.join("manifest.json")).unwrap();
            let image = PendingImage {
                root,
                request: CaptureRequest {
                    archive_root: plan.archive.clone(),
                    epoch: 2,
                    identity: plan.identity.clone(),
                },
                players: Vec::new(),
                counter,
                dimensions: envelope.manifest.dimensions.clone(),
                manifest: Some(envelope.manifest),
                manifest_bytes,
            };
            if plan.mode == "after-rename" {
                let _no_ack = image
                    .rename_published_for(RecoveryProfile::ActiveSlot)
                    .unwrap();
            }
            serde_json::json!({"mode":plan.mode})
        }
        _ => panic!("unknown bounded child mode"),
    };
    println!(
        "\nH08_SLOT_EPOCH_CELL {}",
        serde_json::to_string(&signal).unwrap()
    );
    std::io::stdout().flush().unwrap();
    let (send, recv) = mpsc::channel();
    let _input = std::thread::spawn(move || {
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
    command.args(["--exact","game::levels::capture_integration::counter_restore_integration::slot_epoch_integration::tests::slot_epoch_process_child","--ignored","--nocapture","--test-threads=1"])
        .current_dir(&plan.context).env("HORIZON_SLOT_EPOCH_CHILD_PLAN",serde_json::to_string(plan).unwrap())
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
                    if let Some(encoded) = text.trim().strip_prefix("H08_SLOT_EPOCH_CELL ") {
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

#[track_caller]
fn assert_world_refusal(game: &mut Game<'_>, request: &CaptureRequest, case: &str) {
    eprintln!("SLOT_EPOCH_W1_CASE {case}");
    let before = live(game);
    let source = disk(&game.world_root);
    let archive = disk(&request.archive_root);
    assert!(
        game.observe_slot_epoch_entity_owners().is_ok(),
        "{case}: accounted entity owner"
    );
    let observed = game.observe_checkpoint_world_owners();
    assert!(
        observed.is_err(),
        "{case}: expected W1 refusal, got {observed:?}"
    );
    assert_eq!(
        dispatch_slot_capture(game, request.clone())
            .unwrap_err()
            .code,
        Failure::World,
        "{case}"
    );
    assert_eq!(live(game), before);
    assert_eq!(disk(&game.world_root), source);
    assert_eq!(disk(&request.archive_root), archive);
}
fn world_surface_refusals(game: &mut Game<'_>, request: &CaptureRequest) {
    use horizon_client::containers::{Banner, BlockEntity};
    use horizon_client::world::WorldOwnerKind as K;
    let p = [1, 71, 1];
    let key = (1, 71, 1);
    // Native block entity and its pending/added/removed ticker queues are distinct.
    game.world
        .block_entities
        .insert(key, BlockEntity::Chest(vec![Some(rich(7))]));
    assert_eq!(
        game.world.checkpoint_world_owner().unwrap().kind,
        K::PendingBlockEntityTicker
    );
    assert_world_refusal(game, request, "block-entity-pending-ticker");
    // Match the native lifecycle used by the qualified W1 fixture: settle the
    // initial ticker before borrowing, then return the owner before ticking again.
    game.world.block_entities.tickers(|_| 0);
    assert_eq!(
        game.world.checkpoint_world_owner().unwrap().kind,
        K::NativeBlockEntity
    );
    assert_world_refusal(game, request, "block-entity-native-owner");
    let held = game.world.block_entities.take(&key).unwrap();
    assert_eq!(
        game.world.checkpoint_world_owner().unwrap().kind,
        K::BorrowedBlockEntity
    );
    fn payload_allocations(entity: &BlockEntity) -> Vec<(usize, i32)> {
        fn visit(stack: &ItemStack, rows: &mut Vec<(usize, i32)>) {
            if let Some(data) = stack.extra.as_deref() {
                rows.push((data as *const _ as usize, data.bundle_selected.0));
                for (_, child) in &data.container {
                    visit(child, rows);
                }
                for child in data.charged.iter().chain(&data.bundle_contents) {
                    visit(child, rows);
                }
                if let Some(pots) = &data.pot_decorations {
                    for child in pots.iter().flatten() {
                        visit(child, rows);
                    }
                }
            }
        }
        let mut rows = Vec::new();
        for stack in entity.slots().iter().flatten() {
            visit(stack, &mut rows);
        }
        rows
    }
    let expected_payload = [Some(rich(7))];
    assert_eq!(held.slots(), &expected_payload);
    let held_slots = held.slots().as_ptr();
    let allocations = payload_allocations(&held);
    assert_eq!(
        allocations.iter().map(|row| row.1).collect::<Vec<_>>(),
        [4, 9, 9, 9, 9]
    );
    assert_world_refusal(game, request, "block-entity-borrowed-owner");
    assert_eq!(held.slots(), &expected_payload);
    assert_eq!(held.slots().as_ptr(), held_slots);
    assert_eq!(payload_allocations(&held), allocations);
    game.world.block_entities.put(key, held);
    assert_eq!(
        game.world.checkpoint_world_owner().unwrap().kind,
        K::PendingBlockEntityTicker
    );
    assert_world_refusal(game, request, "block-entity-returned-pending-ticker");
    let removed = game.world.block_entities.remove(&key).unwrap();
    assert_eq!(
        game.world.checkpoint_world_owner().unwrap().kind,
        K::AddedBlockEntityTicker
    );
    assert_eq!(removed.slots(), &expected_payload);
    assert_world_refusal(game, request, "block-entity-removed-added-ticker");
    assert_eq!(removed.slots(), &expected_payload);
    assert_eq!(payload_allocations(&removed), allocations);
    game.world.block_entities.tickers(|_| 0);
    let piston = || horizon_world::piston_shape::MovingPistonState {
        moved_state: game.res.facts().air,
        direction: horizon_core::geometry::Direction::Up,
        extending: true,
        source: false,
        progress: 0.25,
        previous_progress: 0.0,
    };
    let moving = piston();
    game.world.watched_writes.push((key, 13, 0));
    assert_world_refusal(game, request, "watched-writes");
    game.world.watched_writes.clear();
    game.world.moving_pistons.push((p, moving));
    assert_world_refusal(game, request, "moving-pistons");
    game.world.moving_pistons.clear();
    game.world.moving_piston_fallback.push((p, moving));
    assert_world_refusal(game, request, "moving-piston-fallback");
    game.world.moving_piston_fallback.clear();
    for kind in [
        PendingKind::Shulker,
        PendingKind::Banner,
        PendingKind::Names,
        PendingKind::Pot,
        PendingKind::Copied,
    ] {
        match kind {
            PendingKind::Shulker => {
                game.pending.shulker.insert(p, vec![(2, rich(7))]);
            }
            PendingKind::Banner => {
                game.pending.banner.insert(p, Banner::default());
            }
            PendingKind::Names => {
                game.pending
                    .names
                    .insert(p, ("minecraft:chest".into(), "retained".into()));
            }
            PendingKind::Pot => {
                game.pending
                    .pot
                    .insert(p, [None, Some(rich(7)), None, None]);
            }
            PendingKind::Copied => {
                game.pending
                    .copied
                    .insert(p, ("minecraft:player_head".into(), Default::default()));
            }
        }
        assert_world_refusal(game, request, &format!("pending-{kind:?}"));
        match kind {
            PendingKind::Shulker => {
                game.pending.shulker.remove(&p);
            }
            PendingKind::Banner => {
                game.pending.banner.remove(&p);
            }
            PendingKind::Names => {
                game.pending.names.remove(&p);
            }
            PendingKind::Pot => {
                game.pending.pot.remove(&p);
            }
            PendingKind::Copied => {
                game.pending.copied.remove(&p);
            }
        }
    }
    game.spill.spawns.push(crate::game::spill::Spawn {
        stack: rich(7),
        pos: [1.5, 71.0, 1.5],
        motion: None,
        delay: 37,
    });
    assert_world_refusal(game, request, "spill-spawns");
    game.spill.spawns.pop();
    game.spill
        .removed
        .push((p, BlockEntity::Chest(vec![Some(rich(7))])));
    assert_world_refusal(game, request, "spill-removed");
    game.spill.removed.pop();
    game.spill.orbs.push(([1.5, 71.0, 1.5], 7, -7));
    assert_world_refusal(game, request, "spill-orbs");
    game.spill.orbs.pop();
    game.brushing
        .insert(p, crate::game::brush::world_owner_test_brush_state());
    let brushing = crate::game::brush::world_owner_test_brushing_snapshot(&game.brushing);
    assert_world_refusal(game, request, "brushing");
    assert_eq!(
        crate::game::brush::world_owner_test_brushing_snapshot(&game.brushing),
        brushing
    );
    game.brushing.remove(&p);
    game.item_columns = Some(crate::game::machines::ItemColumns::default());
    assert_world_refusal(game, request, "hopper-pass");
    game.item_columns = None;
    // Pregen's private native task is deliberately not constructed or pumped.
    assert!(game.cmds.pregen.is_none());
}
