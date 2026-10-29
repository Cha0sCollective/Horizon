//! Actual borrowed W1 observations. Producer completion and history are not tested here.
use super::*;
use crate::game::{
    brush::{world_owner_test_brush_state, world_owner_test_brushing_snapshot},
    integrated::transition_ownership_tests::Fixture,
    levels::entity_owner_tests::{players, ready, rich, snapshot},
};
use WorldOwnerObservationError as E;
use horizon_client::{
    chunk::{Chunk, ChunkExtras, RolledLoot},
    containers::{Banner, BlockEntity},
    items::ItemStack,
    world::{WorldOwnerKind as K, WorldOwnerLocation as L, WorldOwnerPresence},
};
use horizon_core::{
    geometry::Direction,
    position::BlockPos,
    ticks::{SavedTick, TickPriority, TickTypeId},
};
use horizon_world::{
    level::{LevelEvent, PlayerEffect},
    nbt::{JavaString, Tag},
    piston_shape::MovingPistonState,
    registry::BlockStateId,
};
use std::fs;

const P: [i32; 3] = [-1, 71, 33];
const C: (i32, i32) = (-1, 2);
fn opaque() -> Tag {
    Tag::compound([(
        "fixture:opaque",
        Tag::List(vec![
            Tag::Float(0x7fc01234),
            Tag::Double(0x7ff8000000001234),
            Tag::Double(0x8000000000000000),
            Tag::String(JavaString(vec![0xd800, 0, 0xdc00])),
        ]),
    )])
}
fn piston() -> MovingPistonState {
    MovingPistonState {
        moved_state: BlockStateId(13),
        direction: Direction::Up,
        extending: true,
        source: false,
        progress: f32::from_bits(0x7fc01234),
        previous_progress: -0.0,
    }
}
fn raw_stack(stack: &ItemStack) -> String {
    fn visit(stack: &ItemStack, rows: &mut Vec<(usize, i32)>) {
        if let Some(d) = stack.extra.as_deref() {
            rows.push((d as *const _ as usize, d.bundle_selected.0));
            for (_, s) in &d.container {
                visit(s, rows);
            }
            for s in d.charged.iter().chain(d.bundle_contents.iter()) {
                visit(s, rows);
            }
            if let Some(pots) = &d.pot_decorations {
                for s in pots.iter().flatten() {
                    visit(s, rows);
                }
            }
        }
    }
    let mut rows = Vec::new();
    visit(stack, &mut rows);
    format!("{stack:?}:{rows:?}")
}
fn be_view(entity: &BlockEntity) -> String {
    let mut payloads = Vec::new();
    for s in entity.slots().iter().flatten() {
        payloads.push(raw_stack(s));
    }
    match entity {
        BlockEntity::Jukebox(j) => payloads.extend(j.record.iter().map(raw_stack)),
        BlockEntity::Lectern(l) => payloads.extend(l.book.iter().map(raw_stack)),
        BlockEntity::Campfire(c) => payloads.extend(c.items.iter().flatten().map(raw_stack)),
        BlockEntity::Vault(v) => {
            payloads.extend(v.items_to_eject.iter().map(raw_stack));
            payloads.extend(v.display_item.iter().map(raw_stack));
            payloads.extend(v.config.key_item.iter().map(raw_stack));
        }
        BlockEntity::TrialSpawner(t) => {
            if let Some(items) = &t.dispensing {
                payloads.extend(items.iter().map(raw_stack));
            }
        }
        _ => {}
    }
    format!(
        "{entity:?}:{payloads:?}:{:?}",
        (entity.slots().as_ptr(), entity.slots().len())
    )
}
fn world_view(world: &World) -> Vec<String> {
    let mut rows = Vec::new();
    let mut entries: Vec<_> = world.block_entities.iter().collect();
    entries.sort_by_key(|(p, _)| **p);
    for (p, e) in entries {
        rows.push(format!("{p:?}:{:p}:{}", e, be_view(e)));
    }
    let mut chunks: Vec<_> = world.chunks.iter().collect();
    chunks.sort_by_key(|(p, _)| **p);
    for (key, c) in chunks {
        rows.push(format!(
            "{:?}:{:?}:{:?}",
            (
                key,
                Arc::as_ptr(c),
                c.block_entities.as_ptr(),
                c.entities.as_ptr(),
                c.postprocess.as_ptr(),
                c.block_entities.capacity(),
                c.entities.capacity(),
                c.postprocess.capacity()
            ),
            (&c.entities, &c.postprocess),
            c.saved.as_ref().map(|s| (Arc::as_ptr(s), &**s))
        ));
        for (p, e) in &c.block_entities {
            rows.push(format!("{p:?}:{:p}:{}", e, be_view(e)));
        }
    }
    let bits = |values: &[([i32; 3], MovingPistonState)]| {
        values
            .iter()
            .map(|(p, s)| {
                (
                    *p,
                    s.moved_state,
                    s.direction,
                    s.source,
                    s.extending,
                    s.progress.to_bits(),
                    s.previous_progress.to_bits(),
                )
            })
            .collect::<Vec<_>>()
    };
    rows.push(format!(
        "{:?}:{:?}:{:?}",
        world.watched_writes,
        (
            world.watched_writes.as_ptr(),
            world.watched_writes.capacity(),
            bits(&world.moving_pistons),
            bits(&world.moving_piston_fallback)
        ),
        (
            world.moving_pistons.as_ptr(),
            world.moving_pistons.capacity(),
            world.moving_piston_fallback.as_ptr(),
            world.moving_piston_fallback.capacity(),
            world.chunks.membership_revision()
        )
    ));
    rows.push(format!(
        "{:?}:{:?}:{:?}",
        world.block_names,
        world
            .chest_openers
            .iter()
            .map(|(p, (n, r))| (*p, *n, r.to_bits()))
            .collect::<Vec<_>>(),
        (
            world.block_names.capacity(),
            world.chest_openers.capacity(),
            world.block_entities.capacity()
        )
    ));
    rows
}
fn event_view(events: &[LevelEvent]) -> String {
    let floats: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            LevelEvent::Sound { volume, pitch, .. } => {
                Some((u64::from(volume.to_bits()), u64::from(pitch.to_bits())))
            }
            LevelEvent::PrimeTnt { rot, .. } => Some((rot.to_bits(), 0)),
            _ => None,
        })
        .collect();
    format!("{events:?}:{floats:?}")
}
fn extension(game: &Game<'_>) -> Vec<String> {
    let mut rows = world_view(&game.world);
    rows.push(format!(
        "{:p}:{}:{:?}",
        game.level_events.as_ptr(),
        event_view(&game.level_events),
        game.level_events.capacity()
    ));
    for dim in [Dim::Overworld, Dim::Nether, Dim::End] {
        if let Some(p) = game.levels.parked.get(&dim) {
            rows.push(format!("{dim:?}:{:p}:{:?}", p, world_view(&p.world)));
            rows.push(format!(
                "{:p}:{}:{}",
                p.level_events.as_ptr(),
                event_view(&p.level_events),
                p.level_events.capacity()
            ));
        }
    }
    rows.push(format!(
        "{:?}:{:?}:{:?}:{:?}:{:?}",
        game.pending
            .shulker
            .iter()
            .map(|(p, v)| (*p, v as *const _))
            .collect::<Vec<_>>(),
        game.pending
            .banner
            .iter()
            .map(|(p, v)| (*p, v as *const _))
            .collect::<Vec<_>>(),
        game.pending
            .names
            .iter()
            .map(|(p, v)| (*p, v as *const _))
            .collect::<Vec<_>>(),
        game.pending
            .pot
            .iter()
            .map(|(p, v)| (*p, v as *const _))
            .collect::<Vec<_>>(),
        game.pending
            .copied
            .iter()
            .map(|(p, v)| (*p, v as *const _))
            .collect::<Vec<_>>()
    ));
    for (&p, items) in &game.pending.shulker {
        rows.push(format!(
            "shulker:{p:?}:{:p}:{}:{:?}",
            items.as_ptr(),
            items.capacity(),
            items
                .iter()
                .map(|(i, s)| (*i, raw_stack(s)))
                .collect::<Vec<_>>()
        ));
    }
    rows.push(format!(
        "{:?}:{:?}:{:?}:{:?}",
        game.pending.banner, game.pending.names, game.pending.pot, game.pending.copied
    ));
    for (&p, items) in &game.pending.pot {
        for s in items.iter().flatten() {
            rows.push(format!("pot:{p:?}:{}", raw_stack(s)));
        }
    }
    for s in &game.spill.spawns {
        rows.push(format!(
            "spawn:{}:{:?}:{:?}:{}",
            raw_stack(&s.stack),
            s.pos.map(f64::to_bits),
            s.motion.map(|v| v.map(f64::to_bits)),
            s.delay
        ));
    }
    for (p, e) in &game.spill.removed {
        rows.push(format!("removed:{p:?}:{}", be_view(e)));
    }
    rows.push(format!(
        "{:?}",
        game.spill
            .orbs
            .iter()
            .map(|(p, n, id)| (p.map(f64::to_bits), n, id))
            .collect::<Vec<_>>()
    ));
    rows.push(format!(
        "{:?}",
        world_owner_test_brushing_snapshot(&game.brushing)
    ));
    rows.push(format!(
        "{:?}:{:?}",
        (
            game.pending.shulker.capacity(),
            game.pending.banner.capacity(),
            game.pending.names.capacity(),
            game.pending.pot.capacity(),
            game.pending.copied.capacity()
        ),
        (
            game.spill.spawns.as_ptr(),
            game.spill.spawns.capacity(),
            game.spill.removed.as_ptr(),
            game.spill.removed.capacity(),
            game.spill.orbs.as_ptr(),
            game.spill.orbs.capacity(),
            game.brushing.capacity(),
            game.item_columns.as_ref().map(|c| c as *const _),
            game.cmds.pregen.is_some()
        )
    ));
    rows
}
fn stable(
    game: &mut Game<'_>,
    expected: std::result::Result<WorldOwnerObservation, E>,
    eligible: bool,
) {
    if eligible {
        players(game);
        assert!(game.observe_checkpoint_entity_owners().is_ok());
    }
    let before = snapshot(game);
    let ext = extension(game);
    for _ in 0..2 {
        assert_eq!(game.observe_checkpoint_world_owners(), expected);
        if eligible {
            players(game);
            assert!(game.observe_checkpoint_entity_owners().is_ok());
        }
    }
    assert_eq!(snapshot(game), before);
    assert_eq!(extension(game), ext);
}
fn world_error(dim: Dim, kind: K, location: L) -> E {
    E::World {
        dimension: dim,
        cause: WorldOwnerPresence { kind, location },
    }
}
fn open_levels(game: &mut Game<'_>) {
    game.enter_level(None, Dim::Nether).unwrap();
    game.enter_level(None, Dim::End).unwrap();
    game.enter_level(None, Dim::Overworld).unwrap();
}
fn target<'a>(game: &'a mut Game<'_>, parked: bool) -> &'a mut World {
    if parked {
        &mut game.levels.parked.get_mut(&Dim::Nether).unwrap().world
    } else {
        &mut game.world
    }
}
fn clean_report(game: &Game<'_>) -> WorldOwnerObservation {
    // Expected rows come from native owners, never from candidate output.
    let mut levels = [None; 3];
    for (index, dimension) in [Dim::Overworld, Dim::Nether, Dim::End]
        .into_iter()
        .enumerate()
    {
        let world = if dimension == game.dim {
            Some(&game.world)
        } else {
            game.levels.parked.get(&dimension).map(|p| &p.world)
        };
        levels[index] = world.map(|w| WorldOwnerLevel {
            dimension,
            resident_columns: w.chunks.len(),
        });
    }
    WorldOwnerObservation { levels }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn world_owners_report_empty_active_and_all_parked_levels() {
    let fixture = Fixture::new("world-owner-empty");
    let (mut game, _body, _receivers) = ready(&fixture);
    fs::create_dir_all(game.world_root.join("owned-empty-directory")).unwrap();
    let initial = WorldOwnerObservation {
        levels: [
            Some(WorldOwnerLevel {
                dimension: Dim::Overworld,
                resident_columns: 1,
            }),
            None,
            None,
        ],
    };
    stable(&mut game, Ok(initial), true);
    open_levels(&mut game);
    let expected = clean_report(&game);
    assert_eq!(
        expected.levels.map(|r| r.map(|r| r.dimension)),
        [Some(Dim::Overworld), Some(Dim::Nether), Some(Dim::End)]
    );
    stable(&mut game, Ok(expected), true);
    game.enter_level(None, Dim::End).unwrap();
    stable(&mut game, Ok(expected), true);
    let mut detached = game.observe_checkpoint_world_owners().unwrap();
    detached.levels = [None; 3];
    assert_ne!(detached, expected);
    stable(&mut game, Ok(expected), true);
}

fn populate(entity: &mut BlockEntity) {
    for slot in entity.slots_mut() {
        *slot = Some(rich(7));
    }
    match entity {
        BlockEntity::Jukebox(j) => j.record = Some(rich(11)),
        BlockEntity::Lectern(l) => l.book = Some(rich(13)),
        BlockEntity::Campfire(c) => c.items[0] = Some(rich(17)),
        BlockEntity::Vault(v) => {
            v.items_to_eject.push(rich(19));
            v.display_item = Some(rich(23));
        }
        BlockEntity::TrialSpawner(t) => t.dispensing = Some(vec![rich(29)]),
        BlockEntity::Banner(b) => {
            b.components.insert("fixture:opaque".into(), opaque());
        }
        BlockEntity::Skull(s) => s.profile = Some(opaque()),
        BlockEntity::Beehive(h) => h.bees.push(horizon_client::beehive::Occupant {
            entity_data: opaque(),
            ticks_in_hive: 71,
            min_ticks_in_hive: 93,
        }),
        BlockEntity::CommandBlock(c) => {
            c.command = "give @a minecraft:stone 7".into();
            c.auto = true;
        }
        _ => {}
    }
}
#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn world_owners_refuse_all_native_block_entities_and_lent_states() {
    let blocks = [
        "chest",
        "furnace",
        "brewing_stand",
        "oak_sign",
        "jukebox",
        "beacon",
        "lectern",
        "sculk_sensor",
        "creaking_heart",
        "white_banner",
        "crafter",
        "spawner",
        "trial_spawner",
        "vault",
        "beehive",
        "command_block",
        "campfire",
        "player_head",
        "end_gateway",
        "daylight_detector",
        "chiseled_bookshelf",
    ];
    for parked in [false, true] {
        let fixture = Fixture::new("world-owner-native");
        let (mut game, _body, _receivers) = ready(&fixture);
        open_levels(&mut game);
        let dimension = if parked { Dim::Nether } else { Dim::Overworld };
        let base = clean_report(&game);
        for block in blocks {
            let mut entity = if block == "jukebox" {
                BlockEntity::Jukebox(Default::default())
            } else {
                BlockEntity::for_block(&format!("minecraft:{block}")).unwrap()
            };
            populate(&mut entity);
            let key = (i32::MAX - 15, -73, 41); // synthetic/off-resident is still owned
            target(&mut game, parked).block_entities.insert(key, entity);
            stable(
                &mut game,
                Err(world_error(
                    dimension,
                    K::PendingBlockEntityTicker,
                    L::Block([key.0, key.1, key.2]),
                )),
                true,
            );
            target(&mut game, parked).block_entities.tickers(|_| 0); // setup only, before the next frozen observation
            let live = world_error(
                dimension,
                K::NativeBlockEntity,
                L::Block([key.0, key.1, key.2]),
            );
            stable(&mut game, Err(live), true);
            let held = target(&mut game, parked).block_entities.take(&key).unwrap();
            let held_before = be_view(&held);
            let address = &held as *const _;
            stable(
                &mut game,
                Err(world_error(
                    dimension,
                    K::BorrowedBlockEntity,
                    L::Block([key.0, key.1, key.2]),
                )),
                true,
            );
            assert_eq!(be_view(&held), held_before);
            assert_eq!(&held as *const _, address);
            target(&mut game, parked).block_entities.put(key, held);
            // put may schedule a new ticker when the setup state was non-ticking.
            let restored = world_error(
                dimension,
                K::PendingBlockEntityTicker,
                L::Block([key.0, key.1, key.2]),
            );
            stable(&mut game, Err(restored), true);
            target(&mut game, parked).block_entities.remove(&key);
            stable(
                &mut game,
                Err(world_error(
                    dimension,
                    K::AddedBlockEntityTicker,
                    L::Block([key.0, key.1, key.2]),
                )),
                true,
            );
            target(&mut game, parked).block_entities.tickers(|_| 0); // cleanup after preservation assertions
            stable(&mut game, Ok(base), true);
        }
    }
}

fn seed_chunk(world: &mut World, kind: K) {
    let mut chunk = Chunk::empty(C.0, C.1);
    let mut extras = ChunkExtras::default();
    let tick = SavedTick {
        kind: TickTypeId(3),
        pos: BlockPos::new(P[0], P[1], P[2]),
        delay: -7,
        priority: TickPriority::High,
    };
    match kind {
        K::StagedBlockEntities => chunk
            .block_entities
            .push(((P[0], P[1], P[2]), BlockEntity::Chest(vec![Some(rich(7))]))),
        K::StagedEntities => chunk.entities.push(opaque()),
        K::SavedBlockEntities => extras.block_entities.push(opaque()),
        K::SavedBlockEntitySources => extras
            .block_entity_sources
            .push(((P[0], P[1], P[2]), opaque())),
        K::RolledLoot => extras.rolled_loot.push(RolledLoot {
            pos: (P[0], P[1], P[2]),
            table: "fixture:loot".into(),
            seed: Some(-3),
            items: opaque(),
        }),
        K::OpaqueChunkRoot => {
            extras.root.insert("fixture:opaque".into(), opaque());
        }
        K::Postprocess => chunk.postprocess.push(P),
        K::SavedBlockTicks => extras.block_ticks.push(tick),
        K::SavedFluidTicks => extras.fluid_ticks.push(tick),
        _ => panic!("not a chunk category"),
    }
    chunk.saved = Some(Arc::new(extras));
    world.chunks.insert(C, Arc::new(chunk));
}
#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn world_owners_refuse_every_staged_saved_and_opaque_chunk_category() {
    for parked in [false, true] {
        let fixture = Fixture::new("world-owner-chunks");
        let (mut game, _body, _receivers) = ready(&fixture);
        open_levels(&mut game);
        let dimension = if parked { Dim::Nether } else { Dim::Overworld };
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
            let base = clean_report(&game);
            seed_chunk(target(&mut game, parked), kind);
            stable(
                &mut game,
                Err(world_error(dimension, kind, L::Column([C.0, C.1]))),
                true,
            );
            target(&mut game, parked).chunks.remove(&C);
            stable(&mut game, Ok(base), true);
        }
        let mut chunk = Chunk::empty(C.0, C.1);
        let mut extras = ChunkExtras::default();
        extras.block_entity_order.push((1, 2, 3));
        chunk.saved = Some(Arc::new(extras));
        let world = target(&mut game, parked);
        world.chunks.insert(C, Arc::new(chunk));
        world.block_names.insert((1, 2, 3), "benign name".into());
        world.chest_openers.insert((1, 2, 3), (3, -0.0));
        let expected = clean_report(&game);
        stable(&mut game, Ok(expected), true);
    }
}

fn events() -> Vec<LevelEvent> {
    let pos = BlockPos::new(P[0], P[1], P[2]);
    let state = BlockStateId(13);
    let mut events = vec![
        LevelEvent::LevelEvent {
            id: 2001,
            pos,
            data: 13,
        },
        LevelEvent::Sound {
            pos,
            sound: Arc::from("fixture:sound"),
            volume: f32::from_bits(0x7fc01234),
            pitch: -0.0,
        },
        LevelEvent::DropResources { pos, state },
        LevelEvent::FallingBlock { pos, state },
        LevelEvent::GrowTree {
            pos,
            state,
            grower: Arc::from("fixture:tree"),
        },
        LevelEvent::PlaceFeature {
            pos,
            state,
            feature: Arc::from("fixture:feature"),
        },
        LevelEvent::PrimeTnt {
            pos,
            rot: f64::from_bits(0x7ff8000000001234),
        },
        LevelEvent::BlockEvent {
            pos,
            state,
            kind: "dispense",
        },
        LevelEvent::BlockEntityEvent {
            pos,
            state,
            a: 1,
            b: -3,
        },
        LevelEvent::GameEvent {
            pos,
            event: "minecraft:block_destroy",
            state: Some(state),
        },
    ];
    for effect in [
        PlayerEffect::OpenMenu,
        PlayerEffect::EatCake,
        PlayerEffect::GiveItem(Arc::from("minecraft:stone")),
        PlayerEffect::DropItem(Arc::from("minecraft:bone_meal")),
        PlayerEffect::InteractLoot("fixture:loot"),
        PlayerEffect::Sleep,
        PlayerEffect::RespawnAnchor,
    ] {
        events.push(LevelEvent::Player { pos, state, effect });
    }
    events
}
#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn world_owners_refuse_watched_shape_and_every_level_event() {
    for parked in [false, true] {
        let fixture = Fixture::new("world-owner-effects");
        let (mut game, _body, _receivers) = ready(&fixture);
        open_levels(&mut game);
        let dimension = if parked { Dim::Nether } else { Dim::Overworld };
        let base = clean_report(&game);
        for kind in [K::WatchedWrites, K::MovingPistons, K::MovingPistonFallback] {
            let world = target(&mut game, parked);
            match kind {
                K::WatchedWrites => world.watched_writes.push(((P[0], P[1], P[2]), 13, 0)),
                K::MovingPistons => world.moving_pistons.push((P, piston())),
                K::MovingPistonFallback => world.moving_piston_fallback.push((P, piston())),
                _ => unreachable!(),
            }
            stable(
                &mut game,
                Err(world_error(dimension, kind, L::Block(P))),
                true,
            );
            let world = target(&mut game, parked);
            match kind {
                K::WatchedWrites => world.watched_writes.clear(),
                K::MovingPistons => world.moving_pistons.clear(),
                K::MovingPistonFallback => world.moving_piston_fallback.clear(),
                _ => unreachable!(),
            }
            stable(&mut game, Ok(base), true);
        }
        for event in events() {
            if parked {
                game.levels
                    .parked
                    .get_mut(&dimension)
                    .unwrap()
                    .level_events
                    .push(event);
            } else {
                game.level_events.push(event);
            }
            stable(&mut game, Err(E::LevelEvents { dimension }), true);
            if parked {
                game.levels
                    .parked
                    .get_mut(&dimension)
                    .unwrap()
                    .level_events
                    .clear();
            } else {
                game.level_events.clear();
            }
            stable(&mut game, Ok(base), true);
        }
    }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn world_owners_refuse_all_pending_and_spill_categories() {
    let fixture = Fixture::new("world-owner-global");
    let (mut game, _body, _receivers) = ready(&fixture);
    open_levels(&mut game);
    let base = clean_report(&game);
    for populated in [false, true] {
        for kind in [
            PendingKind::Shulker,
            PendingKind::Banner,
            PendingKind::Names,
            PendingKind::Pot,
            PendingKind::Copied,
        ] {
            match kind {
                PendingKind::Shulker => {
                    game.pending.shulker.insert(
                        P,
                        if populated {
                            vec![(2, rich(7))]
                        } else {
                            vec![]
                        },
                    );
                }
                PendingKind::Banner => {
                    let mut b = Banner::default();
                    if populated {
                        b.components.insert("fixture:opaque".into(), opaque());
                    }
                    game.pending.banner.insert(P, b);
                }
                PendingKind::Names => {
                    game.pending.names.insert(
                        P,
                        if populated {
                            ("minecraft:chest".into(), "kept name".into())
                        } else {
                            (String::new(), String::new())
                        },
                    );
                }
                PendingKind::Pot => {
                    game.pending.pot.insert(
                        P,
                        if populated {
                            [None, Some(rich(11)), None, None]
                        } else {
                            [None, None, None, None]
                        },
                    );
                }
                PendingKind::Copied => {
                    let mut c = horizon_world::nbt::Compound::default();
                    if populated {
                        c.insert("fixture:opaque".into(), opaque());
                    }
                    game.pending
                        .copied
                        .insert(P, ("minecraft:player_head".into(), c));
                }
            }
            stable(&mut game, Err(E::Pending { kind, position: P }), true);
            match kind {
                PendingKind::Shulker => {
                    game.pending.shulker.remove(&P);
                }
                PendingKind::Banner => {
                    game.pending.banner.remove(&P);
                }
                PendingKind::Names => {
                    game.pending.names.remove(&P);
                }
                PendingKind::Pot => {
                    game.pending.pot.remove(&P);
                }
                PendingKind::Copied => {
                    game.pending.copied.remove(&P);
                }
            }
            stable(&mut game, Ok(base), true);
        }
        for kind in [SpillKind::Spawns, SpillKind::Removed, SpillKind::Orbs] {
            match kind {
                SpillKind::Spawns => game.spill.spawns.push(crate::game::spill::Spawn {
                    stack: rich(if populated { 13 } else { 0 }),
                    pos: [f64::from_bits(0x7ff8000000001234), -0.0, f64::INFINITY],
                    motion: Some([-0.0, 0.25, f64::NEG_INFINITY]),
                    delay: 37,
                }),
                SpillKind::Removed => game.spill.removed.push((
                    P,
                    BlockEntity::Chest(if populated {
                        vec![Some(rich(17))]
                    } else {
                        vec![]
                    }),
                )),
                SpillKind::Orbs => game.spill.orbs.push((
                    [f64::from_bits(0x7ff8000000005678), -0.0, f64::INFINITY],
                    if populated { 19 } else { 0 },
                    -7,
                )),
            }
            stable(&mut game, Err(E::Spill { kind }), true);
            match kind {
                SpillKind::Spawns => {
                    game.spill.spawns.pop();
                }
                SpillKind::Removed => {
                    game.spill.removed.pop();
                }
                SpillKind::Orbs => {
                    game.spill.orbs.pop();
                }
            }
            stable(&mut game, Ok(base), true);
        }
    }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn world_owners_refuse_brush_and_hopper_bookkeeping_without_advancing() {
    let fixture = Fixture::new("world-owner-brush");
    let (mut game, _body, _receivers) = ready(&fixture);
    let base = clean_report(&game);
    for p in [[8, 70, 1], [-9, 70, 1], [-9, -7, 1]] {
        game.brushing.insert(p, world_owner_test_brush_state());
    }
    let before = world_owner_test_brushing_snapshot(&game.brushing);
    for row in &before {
        assert_eq!(
            (row.2, row.3, row.4, row.5),
            (7, 123456, 123499, Some([-1, 0, 0]))
        );
    }
    stable(
        &mut game,
        Err(E::Brushing {
            position: [-9, -7, 1],
        }),
        true,
    );
    assert_eq!(world_owner_test_brushing_snapshot(&game.brushing), before);
    for p in [[8, 70, 1], [-9, 70, 1], [-9, -7, 1]] {
        game.brushing.remove(&p);
    }
    stable(&mut game, Ok(base), true);
    game.item_columns = Some(crate::game::machines::ItemColumns::default());
    stable(&mut game, Err(E::HopperPass), true);
    game.item_columns = None;
    stable(&mut game, Ok(base), true);
    // The pregen presence guard is source-only: no private task or global knob.
    assert!(game.cmds.pregen.is_none());
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn world_owners_have_deterministic_priority_and_authority_guards() {
    let fixture = Fixture::new("world-owner-priority");
    let (mut game, _body, _receivers) = ready(&fixture);
    open_levels(&mut game);
    let base = clean_report(&game);
    game.pending.shulker.insert([8, 1, 0], vec![]);
    game.pending.shulker.insert([-8, 1, 0], vec![]);
    game.spill.orbs.push(([0.0; 3], 0, 0));
    stable(
        &mut game,
        Err(E::Pending {
            kind: PendingKind::Shulker,
            position: [-8, 1, 0],
        }),
        true,
    );
    game.levels
        .parked
        .get_mut(&Dim::End)
        .unwrap()
        .level_events
        .push(events().remove(0));
    stable(
        &mut game,
        Err(E::LevelEvents {
            dimension: Dim::End,
        }),
        true,
    );
    game.world.moving_pistons.push((P, piston()));
    stable(
        &mut game,
        Err(world_error(Dim::Overworld, K::MovingPistons, L::Block(P))),
        true,
    );
    seed_chunk(
        &mut game.levels.parked.get_mut(&Dim::Nether).unwrap().world,
        K::StagedEntities,
    );
    stable(
        &mut game,
        Err(world_error(
            Dim::Nether,
            K::StagedEntities,
            L::Column([C.0, C.1]),
        )),
        true,
    );
    for key in [(8, 0), (-9, 5), (-9, -5)] {
        let mut c = Chunk::empty(key.0, key.1);
        c.entities.push(opaque());
        game.world.chunks.insert(key, Arc::new(c));
    }
    stable(
        &mut game,
        Err(world_error(
            Dim::Overworld,
            K::StagedEntities,
            L::Column([-9, -5]),
        )),
        true,
    );
    for key in [(-9, -5), (-9, 5), (8, 0)] {
        let c = game.world.chunks.remove(&key).unwrap();
        game.world.chunks.insert(key, c);
    }
    stable(
        &mut game,
        Err(world_error(
            Dim::Overworld,
            K::StagedEntities,
            L::Column([-9, -5]),
        )),
        true,
    );
    for key in [(8, 0), (-9, 5), (-9, -5)] {
        game.world.chunks.remove(&key);
    }
    game.levels
        .parked
        .get_mut(&Dim::Nether)
        .unwrap()
        .world
        .chunks
        .remove(&C);
    game.world.moving_pistons.pop();
    game.levels
        .parked
        .get_mut(&Dim::End)
        .unwrap()
        .level_events
        .pop();
    game.pending.shulker.remove(&[8, 1, 0]);
    game.pending.shulker.remove(&[-8, 1, 0]);
    game.spill.orbs.pop();
    stable(&mut game, Ok(base), true);
    game.options.execution_role = crate::game::execution_role::ExecutionRole::Client;
    stable(&mut game, Err(E::NotAuthoritative), false);
    game.options.execution_role = crate::game::execution_role::ExecutionRole::IntegratedServer;
    let before_turn = snapshot(&mut game);
    let extension_before_turn = extension(&game);
    let mut host = game.lan.take().unwrap();
    game.as_guest(&mut host, 11, |acting| {
        // The unchanged participant snapshot needs Game.lan; as_guest owns the host locally.
        let before = extension(acting);
        assert_eq!(
            acting.observe_checkpoint_world_owners(),
            Err(E::ActingGuest)
        );
        assert_eq!(
            acting.observe_checkpoint_world_owners(),
            Err(E::ActingGuest)
        );
        assert_eq!(extension(acting), before);
    })
    .unwrap();
    game.lan = Some(host);
    assert_eq!(snapshot(&mut game), before_turn);
    assert_eq!(extension(&game), extension_before_turn);
    stable(&mut game, Ok(base), true);
    let parked = game.levels.parked.remove(&Dim::Nether).unwrap();
    game.levels.parked.insert(Dim::Overworld, parked);
    stable(&mut game, Err(E::DuplicateActiveDimension), false);
    let parked = game.levels.parked.remove(&Dim::Overworld).unwrap();
    game.levels.parked.insert(Dim::Nether, parked);
    stable(&mut game, Ok(base), true);
}
