//! Actual resource-backed active/Parked owners; no process-global state changes.
use super::*;
use crate::game::{
    integrated::transition_ownership_tests::Fixture,
    lan::{LanSettings, ParticipantPreflightError},
};
use EntityOwnerObservationError as E;
use UnsupportedEntityFamily as F;
use horizon_client::{
    chunk::Chunk,
    counter_trial::Configuration,
    entities::{ArmorStand, Cushion, ItemFrame, Painting},
    item_data::BundleSelected,
    items::ItemStack,
    lan::{Joined, NetConfig, Out, PlayerSender, guest::Guest, hub::HubConfig},
    motion::PlayerMotion,
    mutation_observer::AuthorityScope,
    projectiles::{Projectile, ProjectileKind},
    slot_transfer::{Direction, ResolvedLimits, SlotProjection},
    world_slot_trial::Preparation,
};
use horizon_core::{geometry::Vec3, position::BlockPos, random::RandomSource};
use horizon_protocol::packets::configuration::ClientInformation;
use horizon_world::{chunk::BlockState, nbt::Tag};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicI32, Ordering},
        mpsc,
    },
};

pub(in crate::game) fn ready(
    fixture: &Fixture,
) -> (Game<'_>, Option<PlayerMotion>, Vec<mpsc::Receiver<Out>>) {
    let mut game = fixture.game();
    let body = game.player.take();
    assert!(body.is_some());
    game.lan_host(
        LanSettings {
            port: 0,
            guest_commands: false,
            force_game_mode: false,
        },
        NetConfig {
            listen: false,
            announce: false,
            online: None,
            ..NetConfig::lan(0, "owned entity observation".into(), 4)
        },
        HubConfig {
            default_game_mode: 0,
            guest_commands: false,
            force_game_mode: false,
            hardcore: false,
            view_distance: 2,
            simulation_distance: 2,
            dimension: game.dim.id().into(),
            seed: 17,
            max_players: 4,
            dedicated: true,
            integrated: false,
        },
        None,
    )
    .unwrap();
    let mut host = game.lan.take().unwrap();
    let mut receivers = Vec::new();
    for conn in [11, 12] {
        let (tx, rx) = mpsc::channel();
        let mut guest = Guest::new(
            Joined {
                conn,
                uuid: u128::from(conn),
                name: format!("EntityOwnerGuest{conn}"),
                properties: Vec::new(),
                info: ClientInformation::new(2),
                brand: "owned entity fixture".into(),
                sender: PlayerSender::new(tx, Arc::default()),
                latency: Arc::new(AtomicI32::new(0)),
                address: "127.0.0.1:25565".parse().unwrap(),
            },
            conn as i32,
            [8.0, 72.0, 8.0],
            2,
        );
        guest.placed = true;
        guest.loaded = true;
        guest.dimension = Dim::Overworld.id().into();
        guest.saved = Some(Tag::compound([("fixture:opaque", Tag::Int(conn as i32))]));
        guest.inventory[0] = Some(rich(conn));
        host.hub.guests.push(guest);
        receivers.push(rx);
        game.as_guest(&mut host, conn, |_| {}).unwrap();
    }
    game.lan = Some(host);
    players(&game);
    (game, body, receivers)
}
pub(in crate::game) fn players(game: &Game<'_>) {
    let expected = game.observe_checkpoint_participants().unwrap();
    game.preflight_checkpoint_participants(&expected).unwrap();
}
pub(in crate::game) fn rich(count: u32) -> ItemStack {
    let mut stack = ItemStack::new("minecraft:stone", count);
    stack.damage = 3;
    stack.data_mut().bundle_selected = BundleSelected(4);
    stack.data_mut().set_other(
        "fixture:opaque",
        Some(Tag::compound([("marker", Tag::Int(-73))])),
    );
    let mut child = ItemStack::new("minecraft:dirt", 2);
    child.data_mut().bundle_selected = BundleSelected(9);
    stack.data_mut().container.push((2, child.clone()));
    stack.data_mut().charged.push(child.clone());
    stack.data_mut().bundle_contents.push(child.clone());
    stack.data_mut().pot_decorations = Some(Box::new([None, Some(child), None, None]));
    stack
}
fn raw(stack: &ItemStack, out: &mut Vec<(usize, i32)>) {
    if let Some(data) = stack.extra.as_deref() {
        out.push((data as *const _ as usize, data.bundle_selected.0));
        for (_, child) in &data.container {
            raw(child, out);
        }
        for child in data.charged.iter().chain(data.bundle_contents.iter()) {
            raw(child, out);
        }
        if let Some(pots) = &data.pot_decorations {
            for child in pots.iter().flatten() {
                raw(child, out);
            }
        }
    }
}
fn drop(id: u32, position: Vec3, count: u32) -> ItemEntity {
    ItemEntity {
        health: 5,
        fire_ticks: -17,
        id,
        stack: rich(count),
        thrower: Some([1, 2, 3, 4]),
        target: None,
        position,
        previous: position,
        velocity: Vec3::new(0.125, 0.25, -0.125),
        on_ground: false,
        age: 23,
        pickup_delay: 37,
        bob_offset: 0.25,
        ticks: 11,
    }
}
fn stack_view(stack: &ItemStack) -> String {
    let mut metadata = Vec::new();
    raw(stack, &mut metadata);
    format!("{stack:?}:{metadata:?}")
}
fn world_view(world: &World, drops: &[ItemEntity]) -> String {
    let mut keys: Vec<_> = world.chunks.keys().copied().collect();
    keys.sort_unstable();
    let mut raw_values = Vec::new();
    for item in drops {
        raw(&item.stack, &mut raw_values);
    }
    let mut random = world.random.clone();
    let random: [i64; 4] = std::array::from_fn(|_| random.next_long());
    let positions: Vec<_> = drops
        .iter()
        .map(|item| {
            (
                item.id,
                [
                    item.position.x.to_bits(),
                    item.position.y.to_bits(),
                    item.position.z.to_bits(),
                ],
            )
        })
        .collect();
    format!(
        "{:?}",
        (
            keys,
            drops,
            raw_values,
            &world.loaded_entities,
            world.world_slot_snapshot(),
            random,
            world.game_time,
            world.day_time,
            positions
        )
    )
}
fn disk(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    fn visit(path: &Path, entries: &mut Vec<(PathBuf, Option<Vec<u8>>)>) {
        if !path.exists() {
            return;
        }
        if path.is_dir() {
            entries.push((path.to_owned(), None));
            for entry in fs::read_dir(path).unwrap() {
                visit(&entry.unwrap().path(), entries);
            }
        } else {
            entries.push((path.to_owned(), Some(fs::read(path).unwrap())));
        }
    }
    let mut entries = Vec::new();
    visit(root, &mut entries);
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}
#[derive(Debug, PartialEq)]
pub(in crate::game) struct Snapshot {
    native: Vec<String>,
    disk: Vec<(PathBuf, Option<Vec<u8>>)>,
}
pub(in crate::game) fn snapshot(game: &mut Game<'_>) -> Snapshot {
    let mut native = vec![
        format!("{:?}", (game.dim, &game.level)),
        world_view(&game.world, &game.dropped),
    ];
    let mut parked: Vec<_> = game.levels.parked.iter().collect();
    parked.sort_by_key(|(dim, _)| dim.id());
    for (dim, level) in parked {
        native.push(format!(
            "{dim:?}:{}",
            world_view(&level.world, &level.dropped)
        ));
        native.push(format!(
            "{:?}",
            (
                level as *const _ as usize,
                &level.falling,
                &level.paintings,
                &level.item_frames,
                &level.armor_stands,
                &level.cushions,
                &level.others,
                &level.boats,
                &level.carts,
                &level.crystals,
                &level.tnts,
                &level.mobs
            )
        ));
        native.push(format!(
            "{:?}",
            (
                &level.projectiles,
                &level.orbs,
                &level.knot_ids,
                level.bolts.as_ptr(),
                level.dragon.as_ref().map(|d| d as *const _ as usize),
                level.dragon_fireballs.as_ptr(),
                level.breath_clouds.as_ptr(),
                level.ominous_spawners.as_ptr()
            )
        ));
        for trident in &level.tridents {
            native.push(format!(
                "{:?}",
                (stack_view(&trident.stack), trident.position)
            ));
        }
        for rocket in &level.rockets {
            native.push(format!("{:?}", (stack_view(&rocket.item), rocket.position)));
        }
    }
    native.push(format!(
        "{:?}",
        (
            &game.falling,
            &game.paintings,
            &game.item_frames,
            &game.armor_stands,
            &game.cushions,
            &game.others,
            &game.boats,
            &game.carts,
            &game.crystals,
            &game.tnts,
            &game.mobs,
            &game.creatures.projectiles
        )
    ));
    native.push(format!(
        "{:?}",
        (
            &game.orbs,
            &game.knot_ids,
            game.bolts.as_ptr(),
            game.dragon.as_ref().map(|d| d as *const _ as usize),
            game.dragon_fireballs.as_ptr(),
            game.breath_clouds.as_ptr(),
            game.ominous_spawners.as_ptr(),
            game.hook.is_some(),
            game.end_portal_travel
        )
    ));
    for trident in &game.tridents {
        native.push(format!(
            "{:?}",
            (stack_view(&trident.stack), trident.position)
        ));
    }
    for rocket in &game.rockets {
        native.push(format!("{:?}", (stack_view(&rocket.item), rocket.position)));
    }
    let host = game.lan.as_ref().unwrap();
    for guest in &host.hub.guests {
        let mut local = horizon_client::save::Level::default();
        horizon_client::lan::playerdata::store(guest, &mut local);
        let mut metadata = Vec::new();
        for stack in guest
            .inventory
            .iter()
            .chain(guest.armor.iter())
            .chain(std::iter::once(&guest.offhand))
            .chain(guest.ender_items.iter())
            .flatten()
        {
            raw(stack, &mut metadata);
        }
        native.push(format!(
            "{:?}",
            (
                guest.conn,
                guest.uuid,
                local,
                &guest.saved,
                metadata,
                &guest.advancements.criteria,
                &guest.advancements_changed,
                &guest.recipes_unlocked,
                guest.sender.pending.load(Ordering::Relaxed),
                guest
                    .motion
                    .as_deref()
                    .map(horizon_client::lan::guest::SavedBody::of)
            )
        ));
    }
    let mut play: Vec<_> = host.play.iter().collect();
    play.sort_by_key(|(conn, _)| **conn);
    for (conn, play) in play {
        native.push(format!(
            "{:?}",
            (
                conn,
                play as *const _ as usize,
                &play.stats.values,
                &play.stats.events,
                play.stats.loaded,
                play.checkpoint_participant_idle()
            )
        ));
    }
    let mut random = game.particles.random().clone();
    let random: [i64; 4] = std::array::from_fn(|_| random.next_long());
    native.push(format!("{random:?}"));
    Snapshot {
        native,
        disk: disk(&game.world_root),
    }
}
pub(in crate::game) fn assert_refusal(game: &mut Game<'_>, expected: E) {
    players(game);
    let before = snapshot(game);
    for _ in 0..2 {
        players(game);
        assert_eq!(
            game.observe_checkpoint_entity_owners().err(),
            Some(expected)
        );
    }
    assert_eq!(snapshot(game), before);
}
pub(in crate::game) fn memory<'a>(game: &'a Game<'_>, dimension: Dim) -> &'a persist::EntityMemory {
    if game.dim == dimension {
        &game.persist
    } else {
        &game.levels.parked[&dimension].persist
    }
}
fn success(game: &mut Game<'_>, expected: &[(Dim, usize)]) {
    players(game);
    let before = snapshot(game);
    for _ in 0..2 {
        players(game);
        let mut observed = game.observe_checkpoint_entity_owners().unwrap();
        assert_eq!(
            observed
                .levels
                .iter()
                .map(|row| (row.dimension, row.resident_drops))
                .collect::<Vec<_>>(),
            expected
        );
        observed.levels.clear();
    }
    assert_eq!(snapshot(game), before);
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn entity_owners_report_empty_and_distinct_resident_active_and_parked_drops() {
    let fixture = Fixture::new("entity-owner-resident");
    let (mut game, _body, _receivers) = ready(&fixture);
    fs::create_dir_all(game.world_root.join("owned-empty-directory")).unwrap();
    success(&mut game, &[(Dim::Overworld, 0)]);
    game.enter_level(None, Dim::Nether).unwrap();
    game.enter_level(None, Dim::End).unwrap();
    game.enter_level(None, Dim::Overworld).unwrap();
    success(
        &mut game,
        &[(Dim::Overworld, 0), (Dim::End, 0), (Dim::Nether, 0)],
    );
    game.dropped
        .push(drop(101, Vec3::new(15.999, 71.0, 0.1), 7));
    game.enter_level(None, Dim::Nether).unwrap();
    game.world
        .chunks
        .insert((-1, -1), Arc::new(Chunk::empty(-1, -1)));
    game.dropped
        .push(drop(202, Vec3::new(-0.1, 81.0, -0.1), 11));
    game.enter_level(None, Dim::Overworld).unwrap();
    assert!(!game.world.chunks.contains_key(&(-1, -1)));
    success(
        &mut game,
        &[(Dim::Overworld, 1), (Dim::End, 0), (Dim::Nether, 1)],
    );
    assert_eq!(game.dropped[0].stack.count, 7);
    assert_eq!(game.levels.parked[&Dim::Nether].dropped[0].stack.count, 11);
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn entity_owners_require_exact_native_columns_and_reject_invalid_coordinates() {
    for parked in [false, true] {
        let fixture = Fixture::new("entity-owner-columns");
        let (mut game, _body, _receivers) = ready(&fixture);
        if parked {
            game.enter_level(None, Dim::Nether).unwrap();
        }
        let dimension = game.dim;
        for (position, key) in [
            (Vec3::new(-0.1, 71.0, -0.1), (-1, -1)),
            (Vec3::new(16.0, 71.0, 16.0), (1, 1)),
            (Vec3::new(-16.0, 71.0, -16.0), (-1, -1)),
            (Vec3::new(-16.1, 71.0, -16.1), (-2, -2)),
            (
                Vec3::new(
                    f64::from(i32::MIN) + 0.25,
                    f64::MAX,
                    f64::from(i32::MAX) + 0.75,
                ),
                (-134217728, 134217727),
            ),
        ] {
            game.world
                .chunks
                .insert(key, Arc::new(Chunk::empty(key.0, key.1)));
            game.dropped.push(drop(303, position, 13));
            if parked {
                game.enter_level(None, Dim::Overworld).unwrap();
            }
            if parked {
                success(&mut game, &[(Dim::Overworld, 0), (Dim::Nether, 1)]);
            } else {
                success(&mut game, &[(Dim::Overworld, 1)]);
            }
            if parked {
                game.enter_level(None, dimension).unwrap();
            }
            game.dropped.clear();
            game.world.chunks.remove(&key);
        }
        game.world
            .chunks
            .insert((0, 0), Arc::new(Chunk::empty(0, 0)));
        game.dropped.push(drop(304, Vec3::new(-0.1, 71.0, 0.1), 17));
        assert!(!game.world.chunks.contains_key(&(-1, 0)));
        if parked {
            game.enter_level(None, Dim::Overworld).unwrap();
        }
        assert_refusal(&mut game, E::OffResidentDropped { dimension, id: 304 });
        if parked {
            game.enter_level(None, dimension).unwrap();
        }
        game.dropped.clear();
        for axis in 0..3 {
            for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                let mut position = [0.1, 71.0, 0.1];
                position[axis] = value;
                game.dropped.push(drop(
                    305,
                    Vec3::new(position[0], position[1], position[2]),
                    19,
                ));
                if parked {
                    game.enter_level(None, Dim::Overworld).unwrap();
                }
                assert_refusal(&mut game, E::InvalidDroppedPosition { dimension, id: 305 });
                if parked {
                    game.enter_level(None, dimension).unwrap();
                }
                game.dropped.clear();
            }
        }
        for axis in [0, 2] {
            for value in [
                f64::from(i32::MIN) - 0.25,
                f64::from(i32::MAX) + 1.0,
                f64::MAX,
                -f64::MAX,
            ] {
                let mut position = [0.1, 71.0, 0.1];
                position[axis] = value;
                game.dropped.push(drop(
                    306,
                    Vec3::new(position[0], position[1], position[2]),
                    23,
                ));
                if parked {
                    game.enter_level(None, Dim::Overworld).unwrap();
                }
                assert_refusal(&mut game, E::InvalidDroppedPosition { dimension, id: 306 });
                if parked {
                    game.enter_level(None, dimension).unwrap();
                }
                game.dropped.clear();
            }
        }
    }
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
#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn entity_owners_refuse_each_native_family_in_active_and_parked_records() {
    for parked in [false, true] {
        let fixture = Fixture::new("entity-owner-native-families");
        let (mut game, _body, _receivers) = ready(&fixture);
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
            if parked {
                game.enter_level(None, Dim::Nether).unwrap();
            }
            let dimension = game.dim;
            seed_family(&mut game, family);
            if parked {
                game.enter_level(None, Dim::Overworld).unwrap();
            }
            assert_refusal(&mut game, E::UnsupportedEntityFamily { dimension, family });
            if parked {
                game.enter_level(None, dimension).unwrap();
            }
            clear_family(&mut game, family);
            if parked {
                game.enter_level(None, Dim::Overworld).unwrap();
            }
        }
    }
}

pub(in crate::game) fn configuration(
    root: &Path,
    dimension: Dim,
    state: horizon_world::registry::BlockStateId,
) -> Configuration {
    let folder = root.join("owned-entity-slot-metadata");
    for (name, bytes) in [
        ("horizon-paired.toml", b"format=1\nhost-contract='paired-preview-1'\n[content]\nid='entity_slot'\nname='Slot'\nversion='0'\napi='0.2'\nside='both'\ncapabilities=['registries']\n[client]\nentry='client.wasm'\napi='0.1'\ncapabilities=[]\n[server]\nentry='server.wasm'\napi='0.3'\ncapabilities=[]\n".as_slice()),
        ("client.wasm", b"not executed".as_slice()), ("server.wasm", b"not executed".as_slice()),
        ("data/entity_slot/horizon/machine/type.json", br#"{"format":1,"save-schema":1}"#.as_slice()),
        ("data/entity_slot/horizon/block/cube.json", br#"{"template":"minecraft:iron_block","machine-type":"entity_slot:type"}"#.as_slice()),
    ] { let path = folder.join(name); fs::create_dir_all(path.parent().unwrap()).unwrap(); fs::write(path, bytes).unwrap(); }
    let prepared = horizon_mods::paired::PreparedBundle::read_folder(&folder).unwrap();
    Configuration::new(
        AuthorityScope::new(91, dimension.id()).unwrap(),
        prepared
            .content()
            .machine_types()
            .get("entity_slot:type")
            .unwrap(),
        BlockState {
            name: "entity_slot:cube".into(),
            properties: Default::default(),
        },
        state,
        [2, 70, 2],
    )
    .unwrap()
}
fn seed_trial(game: &mut Game<'_>, mode: usize) {
    let state = game
        .pack
        .assets
        .registry
        .default_state("minecraft:iron_block")
        .unwrap();
    if !game.world.chunks.contains_key(&(0, 0)) {
        game.world
            .chunks
            .insert((0, 0), Arc::new(Chunk::empty(0, 0)));
    }
    game.world
        .set(
            2,
            70,
            2,
            if mode == 0 {
                game.res.facts().air
            } else {
                state
            },
        )
        .unwrap();
    let config = configuration(&game.world_root, game.dim, state);
    let enabled = game.world.enable_world_slot_trial(config).unwrap();
    if mode >= 2 {
        let key = enabled.active.unwrap().key;
        let limits = ResolvedLimits::new(64, 64).unwrap();
        let mut source = SlotProjection::new(Some(rich(7)));
        let prepared = match game
            .world
            .prepare_world_slot_transfer(&key, &source, Direction::IntoMachine, 7, limits)
            .unwrap()
        {
            Preparation::Prepared(plan) => *plan,
            other => panic!("real positive transfer required: {other:?}"),
        };
        game.world
            .commit_world_slot_transfer(prepared, &mut source, limits)
            .unwrap();
        assert!(source.snapshot().stack().is_none());
    }
    if mode == 3 {
        game.world.set(2, 70, 2, game.res.facts().air).unwrap();
        assert!(game.world.world_slot_snapshot().unwrap().pending.is_some());
    }
    assert!(game.world.has_world_slot_trial());
}
#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn entity_owners_refuse_pending_loaded_rows_and_all_whole_trial_states() {
    for parked in [false, true] {
        for empty in [false, true] {
            let fixture = Fixture::new("entity-owner-loaded-rows");
            let (mut game, _body, _receivers) = ready(&fixture);
            if parked {
                game.enter_level(None, Dim::Nether).unwrap();
            }
            let dimension = game.dim;
            game.world.loaded_entities.push((
                (0, 0),
                if empty {
                    Vec::new()
                } else {
                    vec![Tag::compound([
                        ("id", Tag::String("fixture:opaque".into())),
                        ("retained", Tag::Long(-73)),
                    ])]
                },
            ));
            if parked {
                game.enter_level(None, Dim::Overworld).unwrap();
            }
            assert_refusal(&mut game, E::LoadedEntities { dimension });
        }
        for mode in 0..4 {
            let fixture = Fixture::new("entity-owner-whole-trial");
            let (mut game, _body, _receivers) = ready(&fixture);
            if parked {
                game.enter_level(None, Dim::Nether).unwrap();
            }
            let dimension = game.dim;
            seed_trial(&mut game, mode);
            if parked {
                game.enter_level(None, Dim::Overworld).unwrap();
            }
            assert_refusal(&mut game, E::WorldSlotTrial { dimension });
        }
    }
}
#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn entity_owners_refuse_authority_guest_turn_duplicate_dimension_and_local_hook() {
    let fixture = Fixture::new("entity-owner-authority");
    let (mut game, mut body, _receivers) = ready(&fixture);
    let expected = game.observe_checkpoint_participants().unwrap();
    game.options.execution_role = crate::game::execution_role::ExecutionRole::Client;
    assert_eq!(
        game.preflight_checkpoint_participants(&expected),
        Err(ParticipantPreflightError::NotAuthoritative)
    );
    assert_eq!(
        game.observe_checkpoint_entity_owners().err(),
        Some(E::NotAuthoritative)
    );
    game.options.execution_role = crate::game::execution_role::ExecutionRole::IntegratedServer;
    let mut host = game.lan.take().unwrap();
    game.as_guest(&mut host, 11, |acting| {
        assert_eq!(
            acting.preflight_checkpoint_participants(&expected),
            Err(ParticipantPreflightError::ActingGuest)
        );
        assert_eq!(
            acting.observe_checkpoint_entity_owners().err(),
            Some(E::ActingGuest)
        );
    })
    .unwrap();
    game.lan = Some(host);
    game.player = body.take();
    assert!(game.use_fishing_rod(&ItemStack::new("minecraft:fishing_rod", 1)));
    body = game.player.take();
    assert!(body.is_some());
    assert!(game.hook.is_some());
    assert_refusal(
        &mut game,
        E::UnsupportedEntityFamily {
            dimension: Dim::Overworld,
            family: F::FishingHook,
        },
    );
    game.hook = None;
    game.end_portal_travel = true;
    assert_refusal(&mut game, E::PortalTravel);
    game.end_portal_travel = false;
    game.enter_level(None, Dim::Nether).unwrap();
    game.enter_level(None, Dim::Overworld).unwrap();
    let parked = game.levels.parked.remove(&Dim::Nether).unwrap();
    game.levels.parked.insert(Dim::Overworld, parked);
    assert_refusal(&mut game, E::DuplicateActiveDimension);
}
