//! Actual native lazy replacements; the harness supplies pinned resources and owned homes.
use super::*;
use crate::game::integrated::transition_ownership_tests::Fixture;
use horizon_client::{
    effects::PlayerEffects,
    items::ItemStack,
    save::{read_dat, write_dat},
};
use std::{fs, path::PathBuf, sync::Arc};

const CENTER: [i32; 3] = [8, 72, 8];
const OMEN: [i32; 3] = [9, 72, 11];
const HERO: [i32; 4] = [11, 22, 33, 44];
const PLAYER: [i32; 4] = [-11, 22, -33, 44];

fn wrapper(raids: &Raids) -> Tag {
    Tag::compound([
        ("data", raids.to_nbt()),
        ("DataVersion", Tag::Int(horizon_world::DATA_VERSION)),
    ])
}
fn samples(game: &Game<'_>) -> [i64; 4] {
    let mut random = game.world.random.clone();
    std::array::from_fn(|_| random.next_long())
}
fn particle_samples(game: &mut Game<'_>) -> [i64; 4] {
    let mut random = game.particles.random().clone();
    std::array::from_fn(|_| random.next_long())
}
fn source(game: &mut Game<'_>, dim: Dim) -> i32 {
    if game.dim != dim {
        let old_save = game.save.clone();
        game.switch_level(dim, Vec3::new(8.0, 72.0, 8.0)).unwrap();
        old_save.flush().unwrap();
    }
    game.load_raids().unwrap();
    let (id, _) = game.raiding.raids.create_or_extend(CENTER, 2, Some(1), 73);
    assert_eq!(id, 2);
    game.raiding.raids.get_mut(id).unwrap().ticks_active = 23;
    game.raiding.omen_loaded = true;
    game.raiding.omen_position = Some(OMEN);
    game.save_raids().unwrap();
    assert!(!game.raiding.raids.dirty);
    {
        let raid = game.raiding.raids.get_mut(id).unwrap();
        raid.started = true;
        raid.active = true;
        raid.ticks_active = 1234;
        raid.groups_spawned = 2;
        raid.omen_level = 2;
        raid.total_health = 47.5;
        raid.cooldown_ticks = 37;
        raid.celebration_ticks = 17;
        raid.wave_spawn_pos = Some([19, 71, 23]);
        raid.add_hero(HERO);
        raid.set_leader(2, 700);
        raid.bar.name = BarName::Remaining(2);
        raid.bar.progress = 0.625;
        raid.bar.visible = true;
        raid.bar.player = true;
    }
    game.raiding.raids.tick = 91;
    game.raiding.raids.dirty = true;
    game.raiding.bar_hurts.insert(id, 7);
    game.raiding
        .homes
        .insert(id, (444, Arc::new(vec![[4, 65, 6], [7, 66, 9]])));
    game.level.player_uuid = Some(PLAYER);
    // Failed saves intentionally update this projection; it is not whole-Level rollback.
    game.level.player_extra = Some(Tag::compound([
        ("fixture:opaque", Tag::Int(-83)),
        ("raid_omen_position", Tag::IntArray(vec![-1, -2, -3])),
    ]));
    id
}
fn assert_source(game: &Game<'_>, id: i32) {
    let raid = game.raiding.raids.get(id).unwrap();
    assert_eq!(raid.center, CENTER);
    assert!(raid.started && raid.active && raid.bar.visible && raid.bar.player);
    assert_eq!(raid.ticks_active, 1234);
    assert_eq!(raid.groups_spawned, 2);
    assert_eq!(raid.omen_level, 2);
    assert_eq!(raid.total_health, 47.5);
    assert_eq!(raid.cooldown_ticks, 37);
    assert_eq!(raid.celebration_ticks, 17);
    assert_eq!(raid.wave_spawn_pos, Some([19, 71, 23]));
    assert_eq!(raid.leader(2), Some(700));
    assert_eq!(raid.heroes(), vec![HERO]);
    assert_eq!(raid.bar.name, BarName::Remaining(2));
    assert_eq!(raid.bar.progress, 0.625);
    assert_eq!(game.raiding.raids.tick, 91);
    assert!(game.raiding.raids.dirty);
    assert_eq!(game.raiding.bar_hurts.get(&id), Some(&7));
    let (tick, homes) = &game.raiding.homes[&id];
    assert_eq!(*tick, 444);
    assert_eq!(homes.as_slice(), &[[4, 65, 6], [7, 66, 9]]);
}
#[derive(Debug, PartialEq)]
struct RaidOwner {
    tag: Tag,
    runtime: String,
    loaded: Option<Dim>,
    bars: HashMap<i32, u64>,
    homes: Vec<(i32, i64, usize, Vec<[i32; 3]>)>,
    omen: (bool, Option<[i32; 3]>),
    attributes: Option<usize>,
}
fn owner(game: &Game<'_>) -> RaidOwner {
    let mut homes: Vec<_> = game
        .raiding
        .homes
        .iter()
        .map(|(id, (tick, positions))| {
            (
                *id,
                *tick,
                Arc::as_ptr(positions) as usize,
                positions.as_ref().clone(),
            )
        })
        .collect();
    homes.sort_by_key(|(id, ..)| *id);
    RaidOwner {
        tag: game.raiding.raids.to_nbt(),
        // Includes leaders/bar and private raid RNG, which NBT alone does not retain.
        runtime: format!("{:?}", game.raiding.raids),
        loaded: game.raiding.loaded,
        bars: game.raiding.bar_hurts.clone(),
        homes,
        omen: (game.raiding.omen_loaded, game.raiding.omen_position),
        attributes: game
            .raiding
            .attributes
            .as_ref()
            .map(|a| Arc::as_ptr(a) as usize),
    }
}
struct Boundary {
    source: Dim,
    target: Dim,
    id: i32,
    source_path: PathBuf,
    target_path: PathBuf,
    obstruction: PathBuf,
    old_bytes: Vec<u8>,
    target_bytes: Vec<u8>,
    live: Tag,
    target_tag: Tag,
}
fn mismatch(game: &mut Game<'_>, from: Dim, to: Dim) -> Boundary {
    let id = source(game, from);
    let source_path = game.raids_path(from);
    let target_path = game.raids_path(to);
    let old_bytes = fs::read(&source_path).unwrap();
    let live = wrapper(&game.raiding.raids);
    assert_ne!(read_dat(&source_path), Some(live.clone()));
    let mut target = Raids::default();
    let (target_id, _) = target.create_or_extend([12, 72, 12], 2, Some(3), 89);
    assert_eq!(target_id, id, "numeric collision is required");
    let raid = target.get_mut(id).unwrap();
    raid.started = true;
    raid.groups_spawned = 4;
    raid.ticks_active = 4567;
    raid.total_health = 99.0;
    target.tick = 203;
    let target_tag = wrapper(&target);
    write_dat(&target_path, target_tag.clone()).unwrap();
    let target_bytes = fs::read(&target_path).unwrap();
    let obstruction = source_path.with_file_name("raids.dat_new");
    fs::create_dir(&obstruction).unwrap();
    let old_save = game.save.clone();
    game.switch_level(to, Vec3::new(8.0, 72.0, 8.0)).unwrap();
    old_save.flush().unwrap(); // The real departing chunk/entity saves must succeed.
    assert_eq!(game.dim, to);
    assert_eq!(game.raiding.loaded, Some(from));
    assert!(
        game.player.is_some(),
        "switch_level retains the native omen caller's body"
    );
    assert_source(game, id);
    Boundary {
        source: from,
        target: to,
        id,
        source_path,
        target_path,
        obstruction,
        old_bytes,
        target_bytes,
        live,
        target_tag,
    }
}
fn unchanged_files(boundary: &Boundary) {
    assert_eq!(fs::read(&boundary.source_path).unwrap(), boundary.old_bytes);
    assert_eq!(
        fs::read(&boundary.target_path).unwrap(),
        boundary.target_bytes
    );
    assert!(boundary.obstruction.is_dir());
}
fn failed_load(game: &mut Game<'_>, boundary: &Boundary) {
    let before = owner(game);
    let random = samples(game); // Only the failed lazy operation, after actual transition.
    let error = game.load_raids().unwrap_err().to_string();
    assert!(
        error.contains(&boundary.source_path.display().to_string()),
        "{error}"
    );
    assert_eq!(owner(game), before);
    assert_eq!(samples(game), random);
    assert_eq!(game.raiding.loaded, Some(boundary.source));
    assert_source(game, boundary.id);
    unchanged_files(boundary);
    let extra = game.level.player_extra.as_ref().unwrap();
    assert_eq!(extra.get("fixture:opaque"), Some(&Tag::Int(-83)));
    assert_eq!(
        extra.get("raid_omen_position"),
        Some(&Tag::IntArray(OMEN.to_vec()))
    );
}
fn target_loaded(game: &Game<'_>, boundary: &Boundary) {
    assert_eq!(game.dim, boundary.target);
    assert_eq!(game.raiding.loaded, Some(boundary.target));
    let raid = game.raiding.raids.get(boundary.id).unwrap();
    assert_eq!(raid.center, [12, 72, 12]);
    assert_eq!(raid.groups_spawned, 4);
    assert_eq!(read_dat(&boundary.source_path), Some(boundary.live.clone()));
    assert_eq!(
        fs::read(&boundary.target_path).unwrap(),
        boundary.target_bytes
    );
}
fn native_mob(game: &Game<'_>, id: u32, kind: &str, position: Vec3) -> Mob {
    Mob::new(
        id,
        game.res.mob_types().get(kind).unwrap().clone(),
        position,
        0.0,
    )
}
fn collision_owners(game: &mut Game<'_>, id: i32) {
    game.world
        .chunks
        .insert((0, 0), Arc::new(horizon_client::chunk::Chunk::empty(0, 0)));
    let at = Vec3::new(8.5, 72.0, 8.5);
    let mut leader = native_mob(game, 700, "minecraft:pillager", at);
    leader.health = 7.5;
    let state = raider::state_mut(&mut leader);
    state.raid_id = Some(id);
    state.wave = 2;
    state.patrol_leader = true;
    state.active_raid = true;
    state.can_join_raid = true;
    state.registered = false;
    state.hurt_seq = 99;
    state.pre_hurt_health = 24.0;
    let mut recruit = native_mob(game, 701, "minecraft:pillager", at);
    let state = raider::state_mut(&mut recruit);
    state.raid_id = Some(id);
    state.wave = 3;
    state.active_raid = true;
    state.can_join_raid = true;
    state.registered = true;
    state.recruit_due = true;
    state.banner_at = Some([17.0, 72.0, 19.0]);
    state.banner_item = 810;
    let mut free = native_mob(game, 702, "minecraft:vindicator", at);
    raider::state_mut(&mut free).can_join_raid = true;
    let mut villager = native_mob(game, 703, "minecraft:villager", at);
    villager.extra.home = Some(CENTER);
    mobs_villager::ext_mut(&mut villager).raid = mobs_villager::RAID_NONE;
    game.mobs.extend([leader, recruit, free, villager]);
    let mut banner = ItemStack::new(raider::BANNER, 1);
    banner.data_mut().ominous_banner = true;
    game.spawn_item(banner, [8.5, 72.0, 8.5], [0.0; 3], 0);
    game.world.game_time = 140; // Recruitment/join bookkeeping would be eligible.
    game.stats.custom("raid_trigger", 5);
    game.adv.unlocked.push("fixture:pending_recipe".into());
    game.adv.changed.insert(37);
}
fn activity(game: &Game<'_>) -> Vec<String> {
    let mut result = Vec::new();
    for mob in &game.mobs {
        result.push(format!(
            "{:?}",
            (
                mob.id,
                mob.health,
                &mob.extra,
                &mob.outbox,
                mob.glowing,
                mob.ai.no_action_time,
                mob.position
            )
        ));
    }
    result.push(format!(
        "{:?}",
        (
            &game.dropped,
            &game.stats.values,
            &game.stats.events,
            &game.adv.progress.criteria,
            &game.adv.changed,
            &game.adv.unlocked
        )
    ));
    result.push(format!(
        "{:?}",
        (
            &game.lan_events,
            &game.lan_block_events,
            &game.lan_entity_events,
            &game.lan_critical_hits
        )
    ));
    result
}
fn direct_refusals(game: &mut Game<'_>, id: i32) {
    let before = owner(game);
    let mobs = activity(game);
    let random = samples(game);
    let particles = particle_samples(game);
    assert_eq!(game.raiding_nearby(CENTER), None);
    assert!(!game.raid_started(id));
    assert!(game.raid_status_text(id).is_empty());
    game.raid_command_stop(id);
    assert_eq!(game.raid_set_omen(id, 5), 0);
    game.raid_glow(id);
    let dead = game.mobs.iter().find(|m| m.id == 700).unwrap().clone();
    game.raider_died(&dead, true);
    assert_eq!(game.raid_boss_at([8.5, 72.0, 8.5], true), None);
    assert_eq!(owner(game), before);
    assert_eq!(activity(game), mobs);
    assert_eq!(samples(game), random);
    assert_eq!(particle_samples(game), particles);
    assert_source(game, id);
    let raider = game.mobs.iter().find(|m| m.id == 700).unwrap();
    assert_eq!(raider::state(raider).unwrap().raid_id, Some(id));
    assert!(!raider.has_effect("glowing"));
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn raid_lazy_failure_retains_source_and_loader_repair_targets_original_file() {
    for (from, to) in [(Dim::Overworld, Dim::Nether), (Dim::Nether, Dim::Overworld)] {
        let fixture = Fixture::new("raid-lazy-repair");
        let mut game = fixture.game();
        let boundary = mismatch(&mut game, from, to);
        failed_load(&mut game, &boundary);
        failed_load(&mut game, &boundary);
        fs::remove_dir(&boundary.obstruction).unwrap();
        let mut random = game.world.random.clone();
        let seed = random.next_long();
        let expected_random: [i64; 4] = std::array::from_fn(|_| random.next_long());
        let expected_native = Raids::from_nbt(boundary.target_tag.get("data").unwrap(), seed);
        game.load_raids().unwrap();
        target_loaded(&game, &boundary);
        assert_eq!(wrapper(&game.raiding.raids), boundary.target_tag);
        assert_eq!(
            format!("{:?}", game.raiding.raids),
            format!("{expected_native:?}")
        );
        assert_eq!(
            samples(&game),
            expected_random,
            "one native destination seed draw"
        );
        assert!(game.raiding.bar_hurts.is_empty());
        assert!(game.raiding.homes.is_empty());
        assert!(!game.raiding.raids.dirty);
        assert_eq!(game.raiding_nearby([12, 72, 12]), Some(boundary.id));
        assert_eq!(game.raid_set_omen(boundary.id, 3), 4);
        assert_eq!(game.raiding.raids.get(boundary.id).unwrap().omen_level, 3);
        // Preserve the ordinary command helper's pre-existing dirty semantics.
        assert!(!game.raiding.raids.dirty);
        collision_owners(&mut game, boundary.id);
        game.raiding
            .raids
            .get_mut(boundary.id)
            .unwrap()
            .set_leader(2, 700);
        game.raid_glow(boundary.id);
        assert!(
            game.mobs
                .iter()
                .find(|m| m.id == 700)
                .unwrap()
                .has_effect("glowing")
        );
        let dead = game.mobs.iter().find(|m| m.id == 700).unwrap().clone();
        game.raider_died(&dead, true);
        let raid = game.raiding.raids.get(boundary.id).unwrap();
        assert_eq!(raid.leader(2), None);
        assert_eq!(raid.heroes(), vec![PLAYER]);
        assert_eq!(game.raiding.bar_hurts.get(&boundary.id), Some(&99));
        assert_eq!(
            raider::state(game.mobs.iter().find(|m| m.id == 700).unwrap())
                .unwrap()
                .raid_id,
            None
        );
        assert!(game.raiding.raids.dirty);
        game.raid_command_stop(boundary.id);
        assert!(game.raiding.raids.get(boundary.id).unwrap().is_stopped());
        assert_eq!(read_dat(&boundary.source_path), Some(boundary.live.clone()));
    }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn raid_direct_helpers_refuse_colliding_dimension_and_unloaded_identity() {
    let fixture = Fixture::new("raid-direct-collision");
    let mut game = fixture.game();
    let boundary = mismatch(&mut game, Dim::Nether, Dim::Overworld);
    collision_owners(&mut game, boundary.id);
    failed_load(&mut game, &boundary);
    // Everything except dimension identity would qualify the visible source boss bar.
    assert_eq!(game.dim, Dim::Overworld);
    assert!(game.player.is_some());
    assert_eq!(
        game.raiding.raids.nearby(CENTER).map(|(id, _)| id),
        Some(boundary.id)
    );
    direct_refusals(&mut game, boundary.id);
    unchanged_files(&boundary);
    // Also protect the explicit unloaded state; no lazy IO or live defaults appear.
    game.raiding.loaded = None;
    direct_refusals(&mut game, boundary.id);
    assert_eq!(game.raiding.loaded, None);
    unchanged_files(&boundary);
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn raid_mismatched_callers_refuse_and_original_callers_resume_after_repair() {
    // The Overworld target also satisfies the native creation attribute and bossbar
    // conditions; a Nether-only test could return before reaching lazy loading.
    for caller in 0..5 {
        let fixture = Fixture::new("raid-caller-repair");
        let mut game = fixture.game();
        let boundary = mismatch(&mut game, Dim::Nether, Dim::Overworld);
        collision_owners(&mut game, boundary.id);
        assert!(!game.spectator());
        assert!(game.player.is_some() && game.player_block().is_some());
        assert!(game.remote.is_none());
        assert!(game.ui.rules.bool("raids"));
        assert_ne!(game.difficulty_id(), 0);
        assert!(game.gameplay_attribute(horizon_client::patrol::CAN_START_RAID, CENTER));
        assert!(sections_to_village(&game.village_centers(), section(CENTER)) <= 1);
        assert!(game.raiding.raids.get(boundary.id).unwrap().omen_level < raid::MAX_OMEN_LEVEL);
        if caller == 0 {
            game.raiding.omen_loaded = false;
        }
        let bad = EffectInstance::new("bad_omen", 21, 1);
        let omen = EffectInstance::new("raid_omen", 1, 1);
        let before = owner(&game);
        let mobs = activity(&game);
        let random = samples(&game);
        let particles = particle_samples(&mut game);
        for _ in 0..2 {
            match caller {
                0 => assert_eq!(game.omen_tick(&bad), OmenTick::Continue),
                1 => assert_eq!(game.omen_tick(&omen), OmenTick::Continue),
                2 => assert_eq!(game.create_or_extend_raid(CENTER, Some(1)), None),
                3 => assert!(!game.raid_command_create(CENTER, 3)),
                _ => game.tick_raids(),
            }
            assert_eq!(owner(&game), before, "caller {caller}");
            assert_eq!(activity(&game), mobs, "caller {caller}");
            assert_eq!(samples(&game), random);
            assert_eq!(particle_samples(&mut game), particles);
            assert_source(&game, boundary.id);
            unchanged_files(&boundary);
            assert_eq!(game.raiding.omen_position, Some(OMEN));
        }
        fs::remove_dir(&boundary.obstruction).unwrap();
        let mut expected_rng = game.world.random.clone();
        expected_rng.next_long(); // The destination lazy-load seed.
        if matches!(caller, 1..=3) {
            expected_rng.next_long();
        } // Native creation seed.
        match caller {
            0 => {
                assert_eq!(
                    game.omen_tick(&bad),
                    OmenTick::Add(EffectInstance::new("raid_omen", 600, 1))
                );
                assert_eq!(game.raiding.omen_position, Some(CENTER));
            }
            1 => {
                assert_eq!(game.omen_tick(&omen), OmenTick::Remove);
                assert_eq!(game.raiding.omen_position, None);
                assert_eq!(game.raiding.raids.get(boundary.id).unwrap().omen_level, 5);
            }
            2 => {
                assert_eq!(
                    game.create_or_extend_raid(CENTER, Some(1)),
                    Some(boundary.id)
                );
                assert_eq!(game.raiding.raids.get(boundary.id).unwrap().omen_level, 5);
            }
            3 => {
                assert!(game.raid_command_create(CENTER, 3));
                assert_eq!(game.raiding.raids.get(boundary.id).unwrap().omen_level, 3);
            }
            _ => {
                game.tick_raids();
                let leader = game.mobs.iter().find(|m| m.id == 700).unwrap();
                assert!(raider::state(leader).unwrap().registered);
                assert_eq!(game.raiding.bar_hurts.get(&boundary.id), Some(&99));
                assert_eq!(game.raiding.raids.tick, 204);
                let villager = game.mobs.iter().find(|m| m.id == 703).unwrap();
                assert_eq!(mobs_villager::raid_status(villager), mobs_villager::RAID);
            }
        }
        target_loaded(&game, &boundary);
        assert!(game.raid_started(boundary.id));
        assert!(!game.raid_status_text(boundary.id).is_empty());
        assert_eq!(
            game.raid_boss_at([8.5, 72.0, 8.5], true)
                .map(|(id, _, _)| id),
            Some(boundary.id)
        );
        if caller < 4 {
            let expected: [i64; 4] = std::array::from_fn(|_| expected_rng.next_long());
            assert_eq!(
                samples(&game),
                expected,
                "loader and creation RNG counted separately"
            );
        }
    }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn raid_obstructed_return_to_source_keeps_same_dimension_access() {
    for (from, to) in [(Dim::Overworld, Dim::Nether), (Dim::Nether, Dim::Overworld)] {
        let fixture = Fixture::new("raid-return-retained-source");
        let mut game = fixture.game();
        let boundary = mismatch(&mut game, from, to);
        failed_load(&mut game, &boundary);
        let old_save = game.save.clone();
        game.switch_level(from, Vec3::new(8.0, 72.0, 8.0)).unwrap();
        old_save.flush().unwrap();
        assert_eq!(game.dim, from);
        assert_eq!(game.raiding.loaded, Some(from));
        let before = owner(&game);
        let random = samples(&game);
        game.load_raids().unwrap(); // Must not attempt the still-obstructed old file.
        assert_eq!(owner(&game), before);
        assert_eq!(samples(&game), random);
        assert_source(&game, boundary.id);
        assert_eq!(game.raiding_nearby(CENTER), Some(boundary.id));
        assert!(game.raid_started(boundary.id));
        assert!(!game.raid_status_text(boundary.id).is_empty());
        if from == Dim::Overworld {
            assert_eq!(
                game.raid_boss_at([8.5, 72.0, 8.5], true),
                Some((boundary.id, raid_name_tag(BarName::Remaining(2)), 0.625,))
            );
        }
        unchanged_files(&boundary);
    }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn raid_refused_omen_ticks_still_count_down_and_expire_native_effects() {
    let fixture = Fixture::new("raid-effect-expiry-boundary");
    let mut game = fixture.game();
    let boundary = mismatch(&mut game, Dim::Nether, Dim::Overworld);
    collision_owners(&mut game, boundary.id);
    let before = owner(&game);
    let mobs = activity(&game);
    let random = samples(&game);
    let mut effects = PlayerEffects::default();
    effects.add(EffectInstance::new("bad_omen", 2, 1));
    effects.add(EffectInstance::new("raid_omen", 1, 1));
    let mut called = Vec::new();
    let (actions, added) = effects.tick_with(
        &mut |effect| {
            called.push(effect.id);
            game.omen_tick(effect)
        },
        false,
    );
    assert_eq!(called, vec!["bad_omen", "raid_omen"]);
    assert!(actions.is_empty() && !added);
    assert_eq!(effects.get("bad_omen").unwrap().duration, 1);
    assert!(
        effects.get("raid_omen").is_none(),
        "Continue is not an effect-time pause or replay queue"
    );
    assert_eq!(effects.tick_count, 1);
    assert_eq!(game.raiding.omen_position, Some(OMEN));
    assert_eq!(owner(&game), before);
    assert_eq!(activity(&game), mobs);
    assert_eq!(samples(&game), random);
    unchanged_files(&boundary);
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn raid_initial_and_clean_lazy_replacement_preserve_defaults_and_seed_draw() {
    for input in 0..3 {
        let fixture = Fixture::new("raid-default-load");
        let mut game = fixture.game();
        let path = game.raids_path(game.dim);
        match input {
            0 => assert!(!path.exists()),
            1 => write_dat(&path, Tag::compound([("data", Tag::Int(-73))])).unwrap(),
            _ => {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, b"owned invalid compressed raid file").unwrap();
            }
        }
        assert_eq!(game.raiding.loaded, None);
        game.raiding.bar_hurts.insert(2, 71);
        game.raiding
            .homes
            .insert(2, (13, Arc::new(vec![[1, 2, 3]])));
        let mut rng = game.world.random.clone();
        rng.next_long();
        let expected: [i64; 4] = std::array::from_fn(|_| rng.next_long());
        game.load_raids().unwrap();
        assert_eq!(game.raiding.loaded, Some(Dim::Overworld));
        assert_eq!(
            game.raiding.raids.to_nbt(),
            Tag::compound([("next_id", Tag::Int(1)), ("tick", Tag::Int(0)),])
        );
        assert!(game.raiding.raids.dirty);
        assert!(game.raiding.bar_hurts.is_empty() && game.raiding.homes.is_empty());
        assert_eq!(samples(&game), expected);
        game.save_raids().unwrap();
        assert_eq!(
            read_dat(&path),
            Some(Tag::compound([
                (
                    "data",
                    Tag::compound([("next_id", Tag::Int(1)), ("tick", Tag::Int(0))])
                ),
                ("DataVersion", Tag::Int(horizon_world::DATA_VERSION)),
            ]))
        );
        let old_bytes = fs::read(&path).unwrap();
        // A clean previous owner must keep its old save guard, even with an obstruction.
        let obstruction = path.with_file_name("raids.dat_new");
        fs::create_dir(&obstruction).unwrap();
        game.raiding.bar_hurts.insert(2, 71);
        game.raiding
            .homes
            .insert(2, (13, Arc::new(vec![[1, 2, 3]])));
        let old_save = game.save.clone();
        game.switch_level(Dim::Nether, Vec3::new(8.0, 72.0, 8.0))
            .unwrap();
        old_save.flush().unwrap();
        let mut rng = game.world.random.clone();
        rng.next_long();
        let expected: [i64; 4] = std::array::from_fn(|_| rng.next_long());
        game.load_raids().unwrap();
        assert_eq!(game.raiding.loaded, Some(Dim::Nether));
        assert!(game.raiding.raids.map.is_empty() && game.raiding.raids.dirty);
        assert_eq!(samples(&game), expected);
        assert!(game.raiding.bar_hurts.is_empty() && game.raiding.homes.is_empty());
        assert_eq!(fs::read(&path).unwrap(), old_bytes);
        assert!(obstruction.is_dir());
        assert!(!game.raids_path(Dim::Nether).exists());
    }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn raid_server_level_round_trip_preserves_parked_native_owners() {
    let fixture = Fixture::new("raid-parked-owner-round-trip");
    let mut game = fixture.game();
    let id = source(&mut game, Dim::Overworld);
    let original = owner(&game);
    let source_rng = samples(&game);
    let path = game.raids_path(Dim::Overworld);
    let bytes = fs::read(&path).unwrap();
    game.enter_level(None, Dim::Nether).unwrap();
    assert_eq!(game.raiding.loaded, None);
    assert!(game.raiding.raids.map.is_empty());
    game.load_raids().unwrap();
    let (nether_id, _) = game
        .raiding
        .raids
        .create_or_extend([33, 81, 35], 3, Some(3), 97);
    assert_eq!(nether_id, id);
    game.raiding.raids.get_mut(id).unwrap().set_leader(4, 901);
    game.raiding.raids.get_mut(id).unwrap().bar.progress = 0.25;
    game.raiding.bar_hurts.insert(id, 113);
    game.raiding
        .homes
        .insert(id, (555, Arc::new(vec![[31, 82, 37]])));
    let target = owner(&game);
    let target_rng = samples(&game);
    game.enter_level(None, Dim::Overworld).unwrap();
    assert_eq!(game.dim, Dim::Overworld);
    assert_eq!(owner(&game), original);
    assert_eq!(samples(&game), source_rng);
    assert_source(&game, id);
    game.load_raids().unwrap();
    assert_eq!(owner(&game), original);
    assert_eq!(
        fs::read(&path).unwrap(),
        bytes,
        "parking is not lazy save/replacement"
    );
    game.enter_level(None, Dim::Nether).unwrap();
    assert_eq!(owner(&game), target);
    assert_eq!(samples(&game), target_rng);
    assert_eq!(game.raiding.raids.get(id).unwrap().center, [33, 81, 35]);
    assert_eq!(game.raiding.raids.get(id).unwrap().leader(4), Some(901));
    assert_eq!(game.raiding.bar_hurts.get(&id), Some(&113));
    assert_eq!(game.raiding.homes[&id].0, 555);
}
