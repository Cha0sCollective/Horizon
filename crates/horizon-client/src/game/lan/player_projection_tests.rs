// Actual native Game owners; resource loading and process isolation belong to the harness.
use super::super::{Dim, integrated::transition_ownership_tests::Fixture};
use super::*;
use ParticipantProjectionError as E;
use horizon_client::{
    advancements::{Advancement, Criterion, Rewards, Tree},
    item_data::BundleSelected,
    lan::{Joined, Out, PlayerSender, guest::Guest},
    motion::PlayerMotion,
};
use horizon_protocol::packets::configuration::ClientInformation;
use horizon_world::nbt::Tag;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicI32, Ordering},
        mpsc,
    },
};

const FIRST: u128 = 0x80000000_ffffffff_00000002_80000001;
const SECOND: u128 = 0x7fffffff_80000000_ffffffff_0000000c;
const DATE: &str = "2026-10-29 10:11:12 +0000";

fn guest(conn: u32) -> (Guest, mpsc::Receiver<Out>) {
    let (tx, rx) = mpsc::channel();
    let mut g = Guest::new(
        Joined {
            conn,
            uuid: if conn == 11 { FIRST } else { SECOND },
            name: format!("ProjectionGuest{conn}"),
            properties: Vec::new(),
            info: ClientInformation::new(2),
            brand: "owned projection fixture".into(),
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
    g.dimension = Dim::Overworld.id().into();
    (g, rx)
}
fn ready(
    fixture: &Fixture,
    owner: bool,
) -> (Game<'_>, Option<PlayerMotion>, Vec<mpsc::Receiver<Out>>) {
    let mut game = fixture.game();
    let body = game.player.take();
    assert!(body.is_some());
    assert!(game.inventory.iter().all(Option::is_none));
    let net = NetConfig {
        listen: false,
        announce: false,
        online: None,
        ..NetConfig::lan(0, "owned projection fixture".into(), 4)
    };
    let config = HubConfig {
        default_game_mode: 0,
        guest_commands: false,
        force_game_mode: false,
        hardcore: false,
        view_distance: 2,
        simulation_distance: 2,
        dimension: game.dim.id().into(),
        seed: 17,
        max_players: 4,
        dedicated: !owner,
        integrated: owner,
    };
    game.lan_host(
        LanSettings {
            port: 0,
            guest_commands: false,
            force_game_mode: false,
        },
        net,
        config,
        owner.then_some(FIRST),
    )
    .unwrap();
    let mut host = game.lan.take().unwrap();
    let (first, first_rx) = guest(11);
    let (second, second_rx) = guest(12);
    host.hub.guests.extend([first, second]);
    for conn in [11, 12] {
        game.as_guest(&mut host, conn, |g| assert_eq!(g.lan_acting, Some(conn)))
            .unwrap();
    }
    if owner {
        game.level.player_uuid = Some([-1, -2, 3, 4]);
        host.owner = Some(Owner {
            uuid: FIRST,
            saved: None,
        });
    }
    // These native Level fields are deliberately NOT overwritten by playerdata::store.
    game.level.motion = [9.0, -8.0, 7.0];
    game.level.food_exhaustion = 2.5;
    game.level.food_tick_timer = 91;
    for g in &mut host.hub.guests {
        let first = g.conn == 11;
        g.dimension = if first {
            "minecraft:overworld"
        } else {
            "minecraft:the_nether"
        }
        .into();
        g.position = if first {
            [11.25, 70.0, -2.5]
        } else {
            [-12.5, 81.0, 3.25]
        };
        g.yaw = if first { 30.0 } else { -45.0 };
        g.pitch = if first { -15.0 } else { 22.0 };
        g.on_ground = first;
        g.health = if first { 13.0 } else { 17.0 };
        g.food = if first { 9 } else { 18 };
        g.saturation = if first { 1.5 } else { 4.5 };
        g.xp_level = if first { 3 } else { 4 };
        g.xp_progress = if first { 0.25 } else { 0.5 };
        g.xp_total = if first { 21 } else { 42 };
        g.xp_seed = if first { -71 } else { 89 };
        g.selected = if first { 0 } else { 2 };
        g.game_mode = 0;
        g.seen_credits = first;
        g.spawn = first.then_some([4.5, 65.0, -6.5]);
        g.respawn_elsewhere =
            (!first).then_some(("minecraft:the_nether".into(), [7, 80, -9], 12.0, false));
        g.last_death = first.then_some(("minecraft:the_end".into(), [-1, 62, 3]));
        g.saved = Some(Tag::compound([
            (
                "fixture:opaque",
                Tag::compound([("marker", Tag::Int(if first { -83 } else { 97 }))]),
            ),
            ("Inventory", Tag::Int(-999)),
            ("UUID", Tag::IntArray(vec![99; 4])),
        ]));
        let motion = g.motion.as_mut().unwrap();
        motion.fire_ticks = if first { 23 } else { -17 };
        motion.air = if first { 211 } else { 277 };
        motion.fall_distance = if first { 3.5 } else { 1.25 };
        g.inventory.fill(None);
        g.armor.fill(None);
        g.offhand = None;
        g.ender_items.fill(None);
        if first {
            g.inventory[0] = Some(rich());
            for (slot, id) in [
                "minecraft:leather_helmet",
                "minecraft:leather_chestplate",
                "minecraft:leather_leggings",
                "minecraft:leather_boots",
            ]
            .into_iter()
            .enumerate()
            {
                g.armor[slot] = Some(ItemStack::new(id, 1));
            }
            g.offhand = Some(ItemStack::new("minecraft:torch", 66));
            g.ender_items[26] = Some(ItemStack::new("minecraft:diamond", 3));
        } else {
            g.inventory[2] = Some(ItemStack::new("minecraft:dirt", 777));
            g.armor[0] = Some(ItemStack::new("minecraft:iron_helmet", 1));
            g.offhand = Some(ItemStack::new("minecraft:stick", 5));
            g.ender_items[1] = Some(ItemStack::new("minecraft:coal", 99));
        }
        g.recipe_book = Default::default();
        g.recipe_book.open[0] = first;
        g.recipe_book.recipes.insert("minecraft:stone".into());
        g.recipe_book
            .to_be_displayed
            .insert("minecraft:stone".into());
        g.recipes_unlocked = vec![format!("fixture:queued_{}", g.conn)];
        g.advancements_loaded = false; // Guest records can legitimately contain in-memory progress.
        g.advancements.criteria.clear();
        g.advancements.grant("fixture:known", "a", DATE);
        if first {
            g.advancements.grant("fixture:known", "b", DATE);
        }
        g.advancements.grant("fixture:unknown", "opaque", DATE);
        g.advancements
            .criteria
            .insert("fixture:empty".into(), Default::default());
        g.advancements_changed.insert(0);
    }
    for (conn, play) in &mut host.play {
        play.stats.loaded = false;
        play.stats.values.clear();
        play.stats.events.clear();
        play.stats.custom("jump", if *conn == 11 { 8 } else { 6 });
        if *conn == 11 {
            play.stats.reset("custom", "minecraft:jump");
        }
        play.stats.add(
            "mined",
            "minecraft:stone",
            if *conn == 11 { -3 } else { 19 },
        );
        play.stats
            .values
            .insert("fixture:empty".into(), Default::default());
    }
    let mut tree = Tree::default();
    tree.by_id.insert("fixture:known".into(), 0);
    tree.list.push(Advancement {
        id: "fixture:known".into(),
        parent: None,
        display: None,
        criteria: ["a", "b"]
            .into_iter()
            .map(|name| Criterion {
                name: name.into(),
                trigger: "minecraft:impossible".into(),
                conditions: serde_json::json!({}),
            })
            .collect(),
        requirements: vec![vec!["a".into()], vec!["b".into()]],
        rewards: Rewards::default(),
    });
    game.adv.tree = Arc::new(tree);
    assert!(game.adv.pending_tree.is_none());
    game.lan = Some(host);
    (game, body, vec![first_rx, second_rx])
}
fn rich() -> ItemStack {
    let mut stack = ItemStack::new("minecraft:stone", 7);
    stack.damage = 3;
    let data = stack.data_mut();
    data.bundle_selected = BundleSelected(4);
    data.map_post_processing = Some(1);
    data.set_other(
        "fixture:component",
        Some(Tag::compound([("marker", Tag::Int(-73))])),
    );
    let mut nested = ItemStack::new("minecraft:dirt", 1);
    nested.data_mut().bundle_selected = BundleSelected(9);
    data.container.push((2, nested));
    stack
}
fn item(id: &str, count: i32) -> Tag {
    Tag::compound([("id", Tag::String(id.into())), ("count", Tag::Int(count))])
}
fn slotted(slot: i8, id: &str, count: i32) -> Tag {
    Tag::compound([
        ("Slot", Tag::Byte(slot)),
        ("id", Tag::String(id.into())),
        ("count", Tag::Int(count)),
    ])
}
fn rich_tag() -> Tag {
    Tag::compound([
        ("Slot", Tag::Byte(0)),
        ("id", Tag::String("minecraft:stone".into())),
        ("count", Tag::Int(7)),
        (
            "components",
            Tag::compound([
                ("minecraft:damage", Tag::Int(3)),
                (
                    "fixture:component",
                    Tag::compound([("marker", Tag::Int(-73))]),
                ),
                (
                    "minecraft:container",
                    Tag::List(vec![Tag::compound([
                        ("slot", Tag::Int(2)),
                        (
                            "item",
                            Tag::compound([("id", Tag::String("minecraft:dirt".into()))]),
                        ),
                    ])]),
                ),
            ]),
        ),
    ])
}
// A hand-authored native save oracle, independent of Level/playerdata/item formatters.
fn expected_player(first: bool, owner: bool) -> Tag {
    let string = |value: &str| Tag::String(value.into());
    let floats =
        |values: &[f32]| Tag::List(values.iter().map(|v| Tag::Float(v.to_bits())).collect());
    let doubles =
        |values: &[f64]| Tag::List(values.iter().map(|v| Tag::Double(v.to_bits())).collect());
    let uuid = if owner {
        vec![-1, -2, 3, 4]
    } else if first {
        vec![i32::MIN, -1, 2, -2147483647]
    } else {
        vec![i32::MAX, i32::MIN, -1, 12]
    };
    let equipment = if first {
        Tag::compound([
            ("head", item("minecraft:leather_helmet", 1)),
            ("chest", item("minecraft:leather_chestplate", 1)),
            ("legs", item("minecraft:leather_leggings", 1)),
            ("feet", item("minecraft:leather_boots", 1)),
            ("offhand", item("minecraft:torch", 66)),
        ])
    } else {
        Tag::compound([
            ("head", item("minecraft:iron_helmet", 1)),
            ("offhand", item("minecraft:stick", 5)),
        ])
    };
    let mut entries = vec![
        (
            "fixture:opaque",
            Tag::compound([("marker", Tag::Int(if first { -83 } else { 97 }))]),
        ),
        ("DataVersion", Tag::Int(horizon_world::DATA_VERSION)),
        ("UUID", Tag::IntArray(uuid)),
        (
            "Pos",
            doubles(if first {
                &[11.25, 70.0, -2.5]
            } else {
                &[-12.5, 81.0, 3.25]
            }),
        ),
        (
            "Rotation",
            floats(if first {
                &[30.0, -15.0]
            } else {
                &[-45.0, 22.0]
            }),
        ),
        (
            "Motion",
            doubles(if owner {
                &[9.0, -8.0, 7.0]
            } else {
                &[0.0, 0.0, 0.0]
            }),
        ),
        ("OnGround", Tag::Byte(i8::from(first))),
        (
            "fall_distance",
            Tag::Double(if first { 3.5f64 } else { 1.25f64 }.to_bits()),
        ),
        (
            "Health",
            Tag::Float(if first { 13.0f32 } else { 17.0f32 }.to_bits()),
        ),
        ("AbsorptionAmount", Tag::Float(0f32.to_bits())),
        ("foodLevel", Tag::Int(if first { 9 } else { 18 })),
        (
            "foodSaturationLevel",
            Tag::Float(if first { 1.5f32 } else { 4.5f32 }.to_bits()),
        ),
        (
            "foodExhaustionLevel",
            Tag::Float(if owner { 2.5f32 } else { 0f32 }.to_bits()),
        ),
        ("foodTickTimer", Tag::Int(if owner { 91 } else { 0 })),
        ("XpLevel", Tag::Int(if first { 3 } else { 4 })),
        (
            "XpP",
            Tag::Float(if first { 0.25f32 } else { 0.5f32 }.to_bits()),
        ),
        ("XpTotal", Tag::Int(if first { 21 } else { 42 })),
        ("XpSeed", Tag::Int(if first { -71 } else { 89 })),
        ("SelectedItemSlot", Tag::Int(if first { 0 } else { 2 })),
        ("playerGameType", Tag::Int(0)),
        (
            "Dimension",
            string(if first {
                "minecraft:overworld"
            } else {
                "minecraft:the_nether"
            }),
        ),
        (
            "Inventory",
            Tag::List(if first {
                vec![rich_tag()]
            } else {
                vec![slotted(2, "minecraft:dirt", 777)]
            }),
        ),
        (
            "EnderItems",
            Tag::List(if first {
                vec![slotted(26, "minecraft:diamond", 3)]
            } else {
                vec![slotted(1, "minecraft:coal", 99)]
            }),
        ),
        ("equipment", equipment),
        (
            "recipeBook",
            Tag::compound([
                ("recipes", Tag::List(vec![string("minecraft:stone")])),
                ("toBeDisplayed", Tag::List(vec![string("minecraft:stone")])),
            ]),
        ),
        (
            "abilities",
            Tag::compound([
                ("invulnerable", Tag::Byte(0)),
                ("mayfly", Tag::Byte(0)),
                ("instabuild", Tag::Byte(0)),
                ("mayBuild", Tag::Byte(1)),
                ("flying", Tag::Byte(0)),
                ("flySpeed", Tag::Float(0.05f32.to_bits())),
                ("walkSpeed", Tag::Float(0.1f32.to_bits())),
            ]),
        ),
        ("seenCredits", Tag::Byte(i8::from(first))),
        ("Fire", Tag::Short(if first { 23 } else { -17 })),
        ("Air", Tag::Short(if first { 211 } else { 277 })),
        ("Invulnerable", Tag::Byte(0)),
        ("PortalCooldown", Tag::Int(0)),
        ("Score", Tag::Int(0)),
        ("SleepTimer", Tag::Short(0)),
        ("HurtTime", Tag::Short(0)),
        ("DeathTime", Tag::Short(0)),
        ("FallFlying", Tag::Byte(0)),
    ];
    if first {
        entries.push((
            "respawn",
            Tag::compound([
                ("dimension", string("minecraft:overworld")),
                ("pos", Tag::IntArray(vec![4, 65, -7])),
                ("yaw", Tag::Float(0f32.to_bits())),
                ("pitch", Tag::Float(0f32.to_bits())),
                ("forced", Tag::Byte(1)),
            ]),
        ));
        entries.push((
            "LastDeathLocation",
            Tag::compound([
                ("dimension", string("minecraft:the_end")),
                ("pos", Tag::IntArray(vec![-1, 62, 3])),
            ]),
        ));
    } else {
        entries.push((
            "respawn",
            Tag::compound([
                ("dimension", string("minecraft:the_nether")),
                ("pos", Tag::IntArray(vec![7, 80, -9])),
                ("yaw", Tag::Float(12f32.to_bits())),
                ("pitch", Tag::Float(0f32.to_bits())),
            ]),
        ));
    }
    let mut result = Tag::compound(entries);
    if first
        && let Tag::Compound(map) = &mut result
        && let Some(Tag::Compound(book)) =
            map.get_mut(&horizon_world::nbt::JavaString::from("recipeBook"))
    {
        book.insert("isGuiOpen".into(), Tag::Byte(1));
    }
    result
}
fn expected_advancements(first: bool) -> serde_json::Value {
    let criteria = if first {
        serde_json::json!({"a": DATE, "b": DATE})
    } else {
        serde_json::json!({"a": DATE})
    };
    serde_json::json!({
        "DataVersion": horizon_world::DATA_VERSION,
        "fixture:known": {"criteria": criteria, "done": first},
        "fixture:unknown": {"criteria": {"opaque": DATE}, "done": false},
    })
}
fn expected_stats(first: bool) -> serde_json::Value {
    serde_json::json!({"DataVersion": horizon_world::DATA_VERSION, "stats": {
        "minecraft:custom": {"minecraft:jump": if first { 0 } else { 6 }},
        "minecraft:mined": {"minecraft:stone": if first { -3 } else { 19 }},
    }})
}
fn files(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    fn visit(root: &Path, out: &mut Vec<(PathBuf, Option<Vec<u8>>)>) {
        if !root.exists() {
            return;
        }
        // Include empty directories and the root itself, distinct from empty files.
        out.push((root.to_owned(), None));
        for entry in fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(&path, out);
            } else {
                out.push((path.clone(), Some(fs::read(path).unwrap())));
            }
        }
    }
    let mut result = Vec::new();
    visit(root, &mut result);
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}
fn metadata(stack: &ItemStack, out: &mut Vec<(usize, i32, Option<u8>)>) {
    let Some(data) = stack.extra.as_deref() else {
        return;
    };
    out.push((
        data as *const _ as usize,
        data.bundle_selected.0,
        data.map_post_processing,
    ));
    for (_, nested) in &data.container {
        metadata(nested, out);
    }
    for nested in data.charged.iter().chain(data.bundle_contents.iter()) {
        metadata(nested, out);
    }
    if let Some(pots) = &data.pot_decorations {
        for nested in pots.iter().flatten() {
            metadata(nested, out);
        }
    }
}
// Comparison snapshots are not expected-format oracles: they retain original native
// allocations, transient selections, notifications, and even malformed saved roots.
fn native(game: &Game<'_>) -> Vec<String> {
    let host = game.lan.as_ref().unwrap();
    let mut rows = vec![
        format!("{:?}", game.level),
        format!(
            "{:?}",
            (
                Arc::as_ptr(&game.adv.tree),
                game.adv.pending_tree.is_some(),
                &game.adv.progress.criteria,
                &game.adv.changed,
                &game.adv.unlocked,
                game.adv.announced.len(),
                game.adv.toasts.len(),
                game.saved_for_exit,
            )
        ),
    ];
    for g in &host.hub.guests {
        let stacks: Vec<_> = g
            .inventory
            .iter()
            .chain(g.armor.iter())
            .chain(std::iter::once(&g.offhand))
            .chain(g.ender_items.iter())
            .chain(std::iter::once(&g.carried))
            .chain(g.craft_grid.iter())
            .chain(std::iter::once(&g.craft_result))
            .collect();
        let mut raw = Vec::new();
        for stack in stacks.iter().filter_map(|s| s.as_ref()) {
            metadata(stack, &mut raw);
        }
        let mut local = horizon_client::save::Level::default();
        playerdata::store(g, &mut local);
        rows.push(format!(
            "{:?}",
            (
                g.conn,
                g.uuid,
                &g.dimension,
                local.player_tag(),
                &g.saved,
                stacks,
                raw,
                g.motion.as_deref().map(|m| m as *const _ as usize),
                g.motion
                    .as_deref()
                    .map(horizon_client::lan::guest::SavedBody::of),
                g.loaded_body
            )
        ));
        rows.push(format!(
            "{:?}",
            (
                g.conn,
                g.loaded,
                g.advancements_loaded,
                &g.advancements.criteria,
                &g.advancements_changed,
                &g.recipes_unlocked,
                Arc::as_ptr(&g.sender.pending),
                g.sender.pending.load(Ordering::Relaxed),
                g.menu.is_some(),
                &g.drag,
                g.inbox.len(),
                &g.left_reason
            )
        ));
    }
    let mut plays: Vec<_> = host.play.iter().map(|(conn, play)| (*conn, play)).collect();
    plays.sort_by_key(|(conn, _)| *conn);
    for (conn, play) in plays {
        rows.push(format!(
            "{:?}",
            (
                conn,
                play as *const _ as usize,
                &play.stats.values,
                &play.stats.events,
                play.stats.loaded,
                play.checkpoint_participant_idle(),
                play.sign_editing,
                play.shield_cooldown,
                play.left_game,
                play.won_game,
                play.respawn_requested
            )
        ));
    }
    rows
}
fn unchanged(game: &Game<'_>, before: &[String], disk: &[(PathBuf, Option<Vec<u8>>)]) {
    assert_eq!(native(game), before);
    assert_eq!(files(&game.world_root), disk);
}
fn root(g: &mut Guest, which: usize) -> &mut Option<ItemStack> {
    match which {
        0 => &mut g.inventory[5],
        1 => &mut g.armor[2],
        2 => &mut g.offhand,
        3 => &mut g.ender_items[9],
        _ => unreachable!(),
    }
}
fn nested(which: usize, count: u32) -> ItemStack {
    let mut parent = ItemStack::new("minecraft:stone", 1);
    let child = ItemStack::new("minecraft:dirt", count);
    match which {
        0 => parent.data_mut().container.push((3, child)),
        1 => parent.data_mut().charged.push(child),
        2 => parent.data_mut().bundle_contents.push(child),
        3 => parent.data_mut().pot_decorations = Some(Box::new([None, Some(child), None, None])),
        _ => unreachable!(),
    }
    parent
}
fn expected_nested(which: usize, count: i32) -> Tag {
    // container/bundle/pot use a template: the default count of one is omitted.
    let template = if count == 1 {
        Tag::compound([("id", Tag::String("minecraft:dirt".into()))])
    } else {
        item("minecraft:dirt", count)
    };
    match which {
        0 => Tag::compound([(
            "minecraft:container",
            Tag::List(vec![Tag::compound([
                ("slot", Tag::Int(3)),
                ("item", template),
            ])]),
        )]),
        1 => Tag::compound([(
            "minecraft:charged_projectiles",
            Tag::List(vec![item("minecraft:dirt", count)]),
        )]),
        2 => Tag::compound([("minecraft:bundle_contents", Tag::List(vec![template]))]),
        3 => Tag::compound([(
            "minecraft:pot_decorations",
            Tag::compound([("left", template)]),
        )]),
        _ => unreachable!(),
    }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn participant_projections_match_independent_rows_and_preserve_native_owners() {
    for owner in [false, true] {
        let fixture = Fixture::new("player-projection-rows");
        let (mut game, _body, _receivers) = ready(&fixture, owner);
        let expected = game.observe_checkpoint_participants().unwrap();
        // The returned order follows the expectation, not the current hub order.
        game.lan.as_mut().unwrap().hub.guests.reverse();
        let before = native(&game);
        let disk = files(&game.world_root);
        for _ in 0..2 {
            let mut projection = game.project_checkpoint_participants(&expected).unwrap();
            assert_eq!(projection.players.len(), 2);
            for (index, first) in [true, false].into_iter().enumerate() {
                let row = &projection.players[index];
                assert_eq!(row.conn, if first { 11 } else { 12 });
                assert_eq!(row.uuid, if first { FIRST } else { SECOND });
                assert_eq!(
                    row.dimension,
                    if first {
                        "minecraft:overworld"
                    } else {
                        "minecraft:the_nether"
                    }
                );
                assert_eq!(
                    row.storage,
                    if owner && first {
                        ParticipantStorage::IntegratedOwner {
                            record_uuid: [-1, -2, 3, 4],
                        }
                    } else {
                        ParticipantStorage::External {
                            record_uuid: if first {
                                [i32::MIN, -1, 2, -2147483647]
                            } else {
                                [i32::MAX, i32::MIN, -1, 12]
                            },
                        }
                    }
                );
                assert_eq!(row.player, expected_player(first, owner && first));
                assert_eq!(row.advancements, expected_advancements(first));
                assert_eq!(row.stats, expected_stats(first));
            }
            // Returned values own their data; dropping/mutating them cannot alter players.
            projection.players[0].player = Tag::Int(-123);
            projection.players[1].advancements = serde_json::json!({"changed": true});
            projection.players[0].stats = serde_json::Value::Null;
            drop(projection);
            unchanged(&game, &before, &disk);
        }
        let raw = game
            .lan
            .as_ref()
            .unwrap()
            .hub
            .guests
            .iter()
            .find(|g| g.conn == 11)
            .unwrap()
            .inventory[0]
            .as_ref()
            .unwrap()
            .data()
            .unwrap();
        assert_eq!(raw.bundle_selected.0, 4);
        assert_eq!(raw.container[0].1.data().unwrap().bundle_selected.0, 9);
        assert_eq!(raw.map_post_processing, Some(1));
    }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn participant_projection_count_boundary_covers_all_roots_and_nested_collections() {
    let fixture = Fixture::new("player-projection-counts");
    let (mut game, _body, _receivers) = ready(&fixture, false);
    let expected = game.observe_checkpoint_participants().unwrap();
    for which in 0..4 {
        for count in [0, i32::MAX as u32 + 1] {
            for child in [None, Some(0), Some(1), Some(2), Some(3)] {
                let g = &mut game.lan.as_mut().unwrap().hub.guests[1];
                let original = root(g, which).replace(match child {
                    None => ItemStack::new("minecraft:stone", count),
                    Some(kind) => nested(kind, count),
                });
                let before = native(&game);
                let disk = files(&game.world_root);
                for _ in 0..2 {
                    assert_eq!(
                        game.project_checkpoint_participants(&expected).err(),
                        Some(E::UnsupportedStack { conn: 12 }),
                        "root {which}, child {child:?}, count {count}"
                    );
                }
                unchanged(&game, &before, &disk);
                *root(&mut game.lan.as_mut().unwrap().hub.guests[1], which) = original;
            }
        }
    }
    // The exact signed-NBT boundary remains supported even above normal item caps.
    for which in 0..4 {
        let g = &mut game.lan.as_mut().unwrap().hub.guests[1];
        let original = root(g, which).replace(ItemStack::new("minecraft:stone", i32::MAX as u32));
        let before = native(&game);
        let disk = files(&game.world_root);
        let projection = game.project_checkpoint_participants(&expected).unwrap();
        let row = &projection.players[1].player;
        let actual = match which {
            0 => {
                let Some(Tag::List(list)) = row.get("Inventory") else {
                    panic!("inventory");
                };
                list.iter()
                    .find(|tag| tag.get("Slot") == Some(&Tag::Byte(5)))
                    .unwrap()
            }
            1 => row.get("equipment").unwrap().get("legs").unwrap(),
            2 => row.get("equipment").unwrap().get("offhand").unwrap(),
            3 => {
                let Some(Tag::List(list)) = row.get("EnderItems") else {
                    panic!("ender");
                };
                list.iter()
                    .find(|tag| tag.get("Slot") == Some(&Tag::Byte(9)))
                    .unwrap()
            }
            _ => unreachable!(),
        };
        assert_eq!(actual.get("count"), Some(&Tag::Int(i32::MAX)));
        unchanged(&game, &before, &disk);
        *root(&mut game.lan.as_mut().unwrap().hub.guests[1], which) = original;
    }
    for which in 0..4 {
        for count in [1, i32::MAX as u32] {
            game.lan.as_mut().unwrap().hub.guests[1].inventory[5] = Some(nested(which, count));
            let before = native(&game);
            let disk = files(&game.world_root);
            let projection = game.project_checkpoint_participants(&expected).unwrap();
            let Some(Tag::List(list)) = projection.players[1].player.get("Inventory") else {
                panic!("inventory");
            };
            let tag = list
                .iter()
                .find(|tag| tag.get("Slot") == Some(&Tag::Byte(5)))
                .unwrap();
            assert_eq!(
                tag.get("components"),
                Some(&expected_nested(which, count as i32))
            );
            assert_eq!(tag.get("count"), Some(&Tag::Int(1)));
            unchanged(&game, &before, &disk);
        }
    }
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn participant_projection_rejects_unknown_air_and_malformed_saved_without_repair() {
    let fixture = Fixture::new("player-projection-shapes");
    let (mut game, _body, _receivers) = ready(&fixture, false);
    let expected = game.observe_checkpoint_participants().unwrap();
    for which in 0..4 {
        for id in ["fixture:unknown", "minecraft:air"] {
            let original = root(&mut game.lan.as_mut().unwrap().hub.guests[1], which)
                .replace(ItemStack::new(id, 1));
            let before = native(&game);
            let disk = files(&game.world_root);
            assert_eq!(
                game.project_checkpoint_participants(&expected).err(),
                Some(E::UnsupportedStack { conn: 12 })
            );
            unchanged(&game, &before, &disk);
            *root(&mut game.lan.as_mut().unwrap().hub.guests[1], which) = original;
        }
    }
    for saved in [Tag::Int(7), Tag::List(vec![Tag::Int(9)])] {
        let original = game.lan.as_mut().unwrap().hub.guests[1]
            .saved
            .replace(saved);
        let before = native(&game);
        let disk = files(&game.world_root);
        assert_eq!(
            game.project_checkpoint_participants(&expected).err(),
            Some(E::UnsupportedPlayerData { conn: 12 })
        );
        unchanged(&game, &before, &disk);
        game.lan.as_mut().unwrap().hub.guests[1].saved = original;
    }
    // Absent saved data stays absent in the native owner; local scratch may encode defaults.
    game.lan.as_mut().unwrap().hub.guests[1].saved = None;
    let before = native(&game);
    let disk = files(&game.world_root);
    assert_eq!(
        game.project_checkpoint_participants(&expected)
            .unwrap()
            .players
            .len(),
        2
    );
    assert!(game.lan.as_ref().unwrap().hub.guests[1].saved.is_none());
    unchanged(&game, &before, &disk);
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn participant_projection_requires_existing_tree_but_accepts_unloaded_record_flags() {
    let fixture = Fixture::new("player-projection-tree");
    let (mut game, _body, _receivers) = ready(&fixture, false);
    let expected = game.observe_checkpoint_participants().unwrap();
    let tree = game.adv.tree.clone();
    game.adv.tree = Arc::default();
    let before = native(&game);
    let disk = files(&game.world_root);
    assert_eq!(
        game.project_checkpoint_participants(&expected).err(),
        Some(E::UnavailableAdvancementTree)
    );
    unchanged(&game, &before, &disk);
    game.adv.tree = tree;
    game.adv.pending_tree = Some(std::thread::spawn(Tree::default));
    let before = native(&game);
    assert_eq!(
        game.project_checkpoint_participants(&expected).err(),
        Some(E::UnavailableAdvancementTree)
    );
    unchanged(&game, &before, &disk);
    game.adv.pending_tree.take().unwrap().join().unwrap();
    let before = native(&game);
    let projection = game.project_checkpoint_participants(&expected).unwrap();
    assert_eq!(
        projection.players[0].advancements,
        expected_advancements(true)
    );
    assert_eq!(projection.players[1].stats, expected_stats(false));
    assert!(
        game.lan
            .as_ref()
            .unwrap()
            .hub
            .guests
            .iter()
            .all(|g| !g.advancements_loaded)
    );
    assert!(
        game.lan
            .as_ref()
            .unwrap()
            .play
            .values()
            .all(|p| !p.stats.loaded)
    );
    unchanged(&game, &before, &disk);
}

#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn participant_projection_calls_real_preflight_again_after_expectation() {
    for case in 0..7 {
        let fixture = Fixture::new("player-projection-preflight");
        let (mut game, _body, mut receivers) = ready(&fixture, false);
        let expected = game.observe_checkpoint_participants().unwrap();
        let mut removed = None;
        let cause = match case {
            0 => {
                game.lan.as_mut().unwrap().hub.guests[0].carried = Some(rich());
                // Preflight has priority even when the metadata tree is also unavailable.
                game.adv.tree = Arc::default();
                ParticipantPreflightError::UnsupportedPlayer { conn: 11 }
            }
            1 => {
                let mut host = game.lan.take().unwrap();
                game.lan_open_merchant(
                    &mut host,
                    11,
                    super::super::trading::MerchantState {
                        session: Box::new(horizon_client::merchant::Session::new(
                            Vec::new(),
                            false,
                        )),
                        scroll: 0,
                        focused: None,
                        mob: None,
                        name: None,
                    },
                );
                game.lan = Some(host);
                ParticipantPreflightError::UnsupportedPlayer { conn: 11 }
            }
            2 => {
                let (tx, rx) = mpsc::channel();
                game.lan.as_mut().unwrap().hub.guests[0].sender =
                    PlayerSender::new(tx, Arc::default());
                receivers.push(rx);
                ParticipantPreflightError::RosterMismatch
            }
            3 => {
                removed = Some(game.lan.as_mut().unwrap().hub.guests.remove(1));
                ParticipantPreflightError::RosterMismatch
            }
            4 => {
                game.lan.as_mut().unwrap().hub.guests[1].uuid = 999;
                ParticipantPreflightError::RosterMismatch
            }
            5 => {
                game.saved_for_exit = true;
                ParticipantPreflightError::UnsupportedServerState
            }
            _ => {
                game.lan.as_mut().unwrap().play.remove(&12);
                ParticipantPreflightError::UnsupportedPlayer { conn: 12 }
            }
        };
        let before = native(&game);
        let disk = files(&game.world_root);
        for _ in 0..2 {
            assert_eq!(
                game.project_checkpoint_participants(&expected).err(),
                Some(E::Preflight(cause)),
                "case {case}"
            );
        }
        unchanged(&game, &before, &disk);
        drop(removed);
    }
}
