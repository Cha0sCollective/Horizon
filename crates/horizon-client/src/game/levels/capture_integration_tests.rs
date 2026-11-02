//! Actual isolated Game/Task archives. The coordinator binds resources and identities.
use super::*;
use crate::game::{
    integrated::{
        capture_completion_tests::{dispatch_capture, dispatch_capture_after_observer_timeout},
        transition_ownership_tests::Fixture,
    },
    lan::{LanSettings, Owner},
    levels::entity_owner_tests::players,
};
use horizon_client::{
    advancements::{Advancement, Criterion, Rewards, Tree},
    chunk::Chunk,
    counter_trial::Configuration,
    items::ItemStack,
    lan::{Joined, NetConfig, Out, PlayerSender, guest::Guest, hub::HubConfig},
    motion::PlayerMotion,
    mutation_observer::AuthorityScope,
    save::{Level, SaveDir, dimension_dir, read_dat, write_dat},
};
use horizon_core::{geometry::Vec3, position::ChunkPos};
use horizon_protocol::packets::configuration::ClientInformation;
use horizon_world::{chunk::BlockState, region::RegionFile};
use std::{
    io::{BufRead, BufReader},
    os::unix::fs::symlink,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicI32, Ordering},
        mpsc,
    },
    time::Duration,
};
const FIRST: u128 = 11;
const SECOND: u128 = 12;
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
            g.inventory[0] = Some(ItemStack::new("minecraft:stone", 7));
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
            g.ender_items[26] = Some(ItemStack::new("minecraft:cobblestone", 3));
        } else {
            g.inventory[2] = Some(ItemStack::new("minecraft:dirt", 17));
            g.armor[0] = Some(ItemStack::new("minecraft:iron_helmet", 1));
            g.offhand = Some(ItemStack::new("minecraft:stick", 5));
            g.ender_items[1] = Some(ItemStack::new("minecraft:gravel", 9));
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

fn slotted(slot: i8, id: &str, count: i32) -> Tag {
    Tag::compound([
        ("Slot", Tag::Byte(slot)),
        ("id", Tag::String(id.into())),
        ("count", Tag::Int(count)),
    ])
}
pub(super) fn expected_player(first: bool, owner: bool, three: bool) -> Tag {
    let string = |value: &str| Tag::String(value.into());
    let floats =
        |values: &[f32]| Tag::List(values.iter().map(|v| Tag::Float(v.to_bits())).collect());
    let doubles =
        |values: &[f64]| Tag::List(values.iter().map(|v| Tag::Double(v.to_bits())).collect());
    let uuid = if owner {
        vec![-1, -2, 3, 4]
    } else if first {
        vec![0, 0, 0, 11]
    } else {
        vec![0, 0, 0, 12]
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
                &[11.25, 70.0, 2.5]
            } else {
                &[12.5, 81.0, 3.25]
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
            string(if first || !three {
                "minecraft:overworld"
            } else {
                "minecraft:the_nether"
            }),
        ),
        (
            "Inventory",
            Tag::List(if first {
                vec![slotted(0, "minecraft:stone", 7)]
            } else {
                vec![slotted(2, "minecraft:dirt", 17)]
            }),
        ),
        (
            "EnderItems",
            Tag::List(if first {
                vec![slotted(26, "minecraft:cobblestone", 3)]
            } else {
                vec![slotted(1, "minecraft:gravel", 9)]
            }),
        ),
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
    // Both authored players hold a selected hotbar item. Native PlayerEquipment
    // counts that held item when deciding to emit its otherwise empty compound.
    entries.push(("equipment", Tag::compound([] as [(&str, Tag); 0])));
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
pub(super) fn expected_advancements(first: bool) -> serde_json::Value {
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
pub(super) fn expected_stats(first: bool) -> serde_json::Value {
    serde_json::json!({"DataVersion": horizon_world::DATA_VERSION, "stats": {
        "minecraft:custom": {"minecraft:jump": if first { 0 } else { 6 }},
        "minecraft:mined": {"minecraft:stone": if first { -3 } else { 19 }},
    }})
}

pub(super) fn setup(
    fixture: &Fixture,
    three: bool,
) -> (
    Game<'_>,
    Option<PlayerMotion>,
    Vec<mpsc::Receiver<Out>>,
    Identity,
) {
    let (mut game, body, receivers) = ready(fixture, true);
    game.options.test_terrain = true;
    let (streaming, save, locator) = crate::game::world_loading::make_streaming(
        &game.res,
        &game.pack,
        &game.options,
        &game.settings,
        &game.world_root,
        game.dim,
        game.level.seed,
    )
    .unwrap();
    game.streaming = crate::game::streaming_owner::GameStreaming::from_existing(streaming);
    game.save = Some(save);
    game.end.set_locator(locator);
    for guest in &mut game.lan.as_mut().unwrap().hub.guests {
        guest.armor.fill(None);
        guest.offhand = None;
        guest.dimension = if guest.uuid == 12 && three {
            Dim::Nether.id()
        } else {
            Dim::Overworld.id()
        }
        .into();
        guest.position = if guest.uuid == 11 {
            [11.25, 70.0, 2.5]
        } else {
            [12.5, 81.0, 3.25]
        };
        guest.previous_position = guest.position;
    }
    fs::create_dir_all(&game.world_root).unwrap();
    if three {
        for dimension in [Dim::Nether, Dim::End] {
            game.enter_level(None, dimension).unwrap();
            game.world
                .chunks
                .insert((0, 0), Arc::new(Chunk::empty(0, 0)));
            let state = game
                .pack
                .assets
                .registry
                .default_state(if dimension == Dim::Nether {
                    "minecraft:cobblestone"
                } else {
                    "minecraft:gold_block"
                })
                .unwrap();
            game.world.set(1, 70, 1, state).unwrap();
        }
        game.enter_level(None, Dim::Overworld).unwrap();
    }
    let folder = game.world_root.parent().unwrap().join("counter-content");
    for (name, bytes) in [
        ("horizon-paired.toml", b"format=1\nhost-contract='paired-preview-1'\n[content]\nid='capture_counter'\nname='Capture Counter'\nversion='0'\napi='0.2'\nside='both'\ncapabilities=['registries']\n[client]\nentry='client.wasm'\napi='0.1'\ncapabilities=[]\n[server]\nentry='server.wasm'\napi='0.3'\ncapabilities=[]\n".as_slice()),
        ("client.wasm", b"not executed".as_slice()), ("server.wasm", b"not executed".as_slice()),
        ("data/capture_counter/horizon/machine/type.json", br#"{"format":1,"save-schema":1}"#.as_slice()),
        ("data/capture_counter/horizon/block/cube.json", br#"{"template":"minecraft:iron_block","machine-type":"capture_counter:type"}"#.as_slice()),
    ] { let path = folder.join(name); fs::create_dir_all(path.parent().unwrap()).unwrap(); fs::write(path,bytes).unwrap(); }
    let prepared = horizon_mods::paired::PreparedBundle::read_folder(&folder).unwrap();
    let state = game
        .pack
        .assets
        .registry
        .default_state("minecraft:iron_block")
        .unwrap();
    game.world.set(2, 70, 2, state).unwrap();
    let config = Configuration::new(
        AuthorityScope::new(41, Dim::Overworld.id()).unwrap(),
        prepared
            .content()
            .machine_types()
            .get("capture_counter:type")
            .unwrap(),
        BlockState {
            name: "capture_counter:cube".into(),
            properties: Default::default(),
        },
        state,
        [2, 70, 2],
    )
    .unwrap();
    game.world.enable_counter_trial(config).unwrap();
    let binding = |name| {
        let value = std::env::var(name).expect("coordinator must bind capture identity");
        assert!(valid_hash(&value));
        value
    };
    let identity = Identity {
        engine_source: binding("HORIZON_CAPTURE_SOURCE_SHA256"),
        test_binary: binding("HORIZON_CAPTURE_BINARY_SHA256"),
        resource_manifest: binding("HORIZON_CAPTURE_RESOURCE_SHA256"),
        counter_content: prepared
            .identity()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    };
    fs::write(
        game.world_root.join("unrelated-sentinel.bin"),
        b"owned native fixture sentinel",
    )
    .unwrap();
    for guest in &game.lan.as_ref().unwrap().hub.guests {
        assert_eq!(
            (
                guest.position[0].floor() as i32 >> 4,
                guest.position[2].floor() as i32 >> 4
            ),
            (0, 0)
        );
        let dimension = Dim::from_id(&guest.dimension);
        let world = if dimension == game.dim {
            &game.world
        } else {
            &game.levels.parked[&dimension].world
        };
        assert!(world.chunks.contains_key(&(0, 0)));
    }
    players(&game);
    game.capture_cell_checks().unwrap();
    (game, body, receivers, identity)
}
fn request(game: &Game<'_>, identity: &Identity, epoch: u64) -> CaptureRequest {
    let archive_root = game.world_root.parent().unwrap().join("archive");
    fs::create_dir_all(&archive_root).unwrap();
    CaptureRequest {
        archive_root,
        epoch,
        identity: identity.clone(),
    }
}
fn disk(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(path: &Path, out: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        if !path.exists() {
            return;
        }
        if path.is_dir() {
            out.insert(path.to_owned(), None);
            for child in fs::read_dir(path).unwrap() {
                visit(&child.unwrap().path(), out);
            }
        } else {
            out.insert(path.to_owned(), Some(fs::read(path).unwrap()));
        }
    }
    let mut out = BTreeMap::new();
    visit(root, &mut out);
    out
}
fn player_state(game: &Game<'_>) -> Vec<String> {
    let host = game.lan.as_ref().unwrap();
    let mut out: Vec<String> = host
        .hub
        .guests
        .iter()
        .map(|g| {
            let mut level = Level::default();
            horizon_client::lan::playerdata::store(g, &mut level);
            format!(
                "{:?}",
                (
                    g.conn,
                    g.uuid,
                    &g.dimension,
                    level,
                    &g.saved,
                    g.motion.as_deref().map(|p| p as *const _ as usize),
                    &g.advancements.criteria,
                    &g.advancements_changed,
                    &g.recipes_unlocked
                )
            )
        })
        .collect();
    let mut plays: Vec<_> = host.play.iter().collect();
    plays.sort_by_key(|(conn, _)| **conn);
    for (conn, play) in plays {
        out.push(format!(
            "{:?}",
            (
                conn,
                &play.stats.values,
                &play.stats.events,
                play.stats.loaded,
                play.checkpoint_participant_idle(),
                play.left_game,
                play.won_game,
                play.respawn_requested
            )
        ));
    }
    out
}
fn save_footprint(
    game: &Game<'_>,
    before: BTreeMap<PathBuf, Option<Vec<u8>>>,
    ticket: Option<Dim>,
) {
    let mut before = before;
    let mut after = disk(&game.world_root);
    let mut allowed_files = Vec::new();
    for dimension in [Dim::Overworld, Dim::Nether, Dim::End] {
        if dimension != game.dim && !game.levels.parked.contains_key(&dimension) {
            continue;
        }
        let root = dimension_dir(&game.world_root, dimension.id());
        allowed_files.push(root.join("region").join("r.0.0.mca"));
        if ticket == Some(dimension) {
            allowed_files.push(root.join("data/minecraft/chunk_tickets.dat"));
        }
    }
    allowed_files.push(game.world_root.join("level.dat"));
    for name in [
        "weather",
        "world_clocks",
        "world_gen_settings",
        "game_rules",
        "random_sequences",
    ] {
        allowed_files.push(game.world_root.join(format!("data/minecraft/{name}.dat")));
    }
    for (folder, extension) in [("data", "dat"), ("advancements", "json"), ("stats", "json")] {
        allowed_files.push(game.world_root.join(format!(
            "players/{folder}/ffffffff-ffff-fffe-0000-000300000004.{extension}"
        )));
    }
    let native = allowed_files.clone();
    for path in native {
        let filename = path.file_name().unwrap().to_str().unwrap();
        if filename.ends_with(".dat") {
            allowed_files.push(path.with_file_name(format!("{filename}_new")));
            allowed_files.push(path.with_file_name(format!("{filename}_old")));
        }
    }
    let mut parents = BTreeSet::new();
    for path in allowed_files {
        let mut parent = path.parent();
        while let Some(p) = parent {
            if !p.starts_with(&game.world_root) {
                break;
            }
            parents.insert(p.to_owned());
            parent = p.parent();
        }
        let old = before.remove(&path);
        let new = after.remove(&path);
        if old != new {
            assert!(old.as_ref().is_none_or(Option::is_some));
            assert!(new.as_ref().is_some_and(Option::is_some));
        }
    }
    for parent in parents {
        if !before.contains_key(&parent) && after.get(&parent) == Some(&None) {
            after.remove(&parent);
        }
    }
    assert_eq!(after, before);
}
fn default_sequences() -> Tag {
    Tag::compound([
        ("DataVersion", Tag::Int(horizon_world::DATA_VERSION)),
        (
            "data",
            Tag::compound([
                ("salt", Tag::Int(0)),
                ("include_world_seed", Tag::Byte(1)),
                ("include_sequence_id", Tag::Byte(1)),
                ("sequences", Tag::Compound(Default::default())),
            ]),
        ),
    ])
}
fn assert_native_recovery(
    game: &Game<'_>,
    recovered: &Path,
    identity: &Identity,
    archive: &Path,
    epoch: u64,
    runtime_instance: u64,
) {
    let receipt = recover_fixture_epoch(archive, identity, recovered).unwrap();
    assert_eq!(receipt.epoch, epoch);
    assert_eq!(
        receipt.counter,
        CounterArchive {
            schema: 1,
            kind: "item-free-counter-archive".into(),
            scope: ArchiveScope {
                world_instance: 41,
                dimension: Dim::Overworld.id().into()
            },
            r#type: "capture_counter:type".into(),
            save_schema: 1,
            position: [2, 70, 2],
            state: ArchiveState {
                name: "capture_counter:cube".into(),
                properties: BTreeMap::new()
            },
            runtime_instance,
            counter: 0,
            last_advanced: None,
            resident: true,
            live_generation_restore: false
        }
    );
    assert!(Level::read(recovered).is_some());
    for (first, uuid) in [
        (true, "ffffffff-ffff-fffe-0000-000300000004"),
        (false, "00000000-0000-0000-0000-00000000000c"),
    ] {
        let tag = strict_player(
            &bounded_read(
                &recovered.join(format!("players/data/{uuid}.dat")),
                2 << 20,
                Phase::Recover,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            tag,
            expected_player(first, first, game.levels.parked.contains_key(&Dim::Nether))
        );
        let mut level = Level::default();
        level.read_player(&tag);
        level.player_extra = Some(tag);
        let (mut guest, _rx) = guest(if first { 11 } else { 12 });
        horizon_client::lan::playerdata::apply(&mut guest, &level);
        assert_eq!(
            guest.inventory[if first { 0 } else { 2 }]
                .as_ref()
                .unwrap()
                .count,
            if first { 7 } else { 17 }
        );
        let adv: Value = parse_json(
            &bounded_read(
                &recovered.join(format!("players/advancements/{uuid}.json")),
                JSON_LIMIT,
                Phase::Recover,
            )
            .unwrap(),
            JSON_LIMIT,
        )
        .unwrap();
        assert_eq!(adv, expected_advancements(first));
        let stats: Value = parse_json(
            &bounded_read(
                &recovered.join(format!("players/stats/{uuid}.json")),
                JSON_LIMIT,
                Phase::Recover,
            )
            .unwrap(),
            JSON_LIMIT,
        )
        .unwrap();
        assert_eq!(stats, expected_stats(first));
    }
    assert!(
        !recovered
            .join("players/data/00000000-0000-0000-0000-00000000000b.dat")
            .exists()
    );
    assert_eq!(
        read_dat(&recovered.join("data/minecraft/random_sequences.dat")),
        Some(default_sequences())
    );
    assert_eq!(
        fs::read(recovered.join("unrelated-sentinel.bin")).unwrap(),
        b"owned native fixture sentinel"
    );
    for dim in [Dim::Overworld, Dim::Nether, Dim::End] {
        if dim != game.dim && !game.levels.parked.contains_key(&dim) {
            continue;
        }
        let root = dimension_dir(recovered, dim.id());
        let mut region = RegionFile::open(root.join("region/r.0.0.mca")).unwrap();
        let raw = region.read(ChunkPos::new(0, 0)).unwrap().unwrap();
        raw.nbt(nbt_limits()).unwrap();
        let chunk = SaveDir::new(root)
            .load(0, 0, game.pack.assets.registry, game.res.biomes())
            .unwrap();
        let expected = game
            .pack
            .assets
            .registry
            .default_state(match dim {
                Dim::Overworld => "minecraft:stone",
                Dim::Nether => "minecraft:cobblestone",
                Dim::End => "minecraft:gold_block",
            })
            .unwrap();
        assert_eq!(chunk.block(1, 70, 1), expected);
    }
}

#[test]
#[ignore = "requires pinned capture identities/resources and isolated cwd/home/temp"]
fn capture_cell_roundtrips_two_players_native_world_and_archival_counter() {
    for three in [false, true] {
        let fixture = Fixture::new("capture-roundtrip");
        let (mut game, _body, _receivers, identity) = setup(&fixture, three);
        let req = request(&game, &identity, 1);
        let archive = req.archive_root.clone();
        let before = disk(&game.world_root);
        let player_before = player_state(&game);
        let instance = game
            .world
            .counter_trial_snapshot()
            .unwrap()
            .record()
            .unwrap()
            .instance();
        let receipt = dispatch_capture(&mut game, req).unwrap();
        assert_eq!(receipt.epoch, 1);
        assert_eq!(player_state(&game), player_before);
        save_footprint(&game, before, None);
        let epoch = archive.join("epoch-00000000000000000001");
        let frozen = disk(&epoch);
        assert_eq!(
            receipt.manifest_sha256,
            <[u8; 32]>::from(Sha256::digest(
                fs::read(epoch.join("manifest.json")).unwrap()
            ))
        );
        let a = fs::metadata(game.world_root.join("level.dat")).unwrap();
        let b = fs::metadata(epoch.join("world/level.dat")).unwrap();
        assert_ne!((a.dev(), a.ino()), (b.dev(), b.ino()));
        assert_native_recovery(
            &game,
            &game.world_root.parent().unwrap().join("recovered"),
            &identity,
            &archive,
            1,
            instance,
        );
        fs::write(
            game.world_root.join("unrelated-sentinel.bin"),
            b"changed LIVE after capture",
        )
        .unwrap();
        game.lan.as_mut().unwrap().hub.guests[0].inventory[0]
            .as_mut()
            .unwrap()
            .count = 31;
        assert_eq!(disk(&epoch), frozen);
        assert!(game.world.counter_trial_snapshot().is_some());
    }
}

#[test]
#[ignore = "requires pinned capture identities/resources and isolated cwd/home/temp"]
fn capture_cell_refuses_current_owners_and_nonpristine_streamers_without_publication() {
    for case in 0..10 {
        let fixture = Fixture::new("capture-refusal");
        let (mut game, _body, _receivers, identity) = setup(&fixture, false);
        let mut detached = None;
        match case {
            0 => {
                detached = Some(game.lan.as_mut().unwrap().hub.guests.remove(1));
            }
            1 => {
                game.lan.as_mut().unwrap().hub.guests[0].inventory[0]
                    .as_mut()
                    .unwrap()
                    .data_mut()
                    .custom_name = Some("unsupported".into());
            }
            2 => {
                game.lan.as_mut().unwrap().hub.guests[0].inventory[0]
                    .as_mut()
                    .unwrap()
                    .count = 0
            }
            3 => {
                game.lan.as_mut().unwrap().hub.guests[0].inventory[0]
                    .as_mut()
                    .unwrap()
                    .damage = 1
            }
            4 => game.dropped.push(ItemEntity {
                health: 5,
                fire_ticks: -20,
                id: 700,
                stack: ItemStack::new("minecraft:stone", 3),
                position: Vec3::new(2.0, 70.0, 2.0),
                previous: Vec3::new(2.0, 70.0, 2.0),
                velocity: Vec3::new(0.0, 0.0, 0.0),
                thrower: None,
                target: None,
                on_ground: true,
                age: 0,
                pickup_delay: 10,
                bob_offset: 0.25,
                ticks: 0,
            }),
            5 => {
                let state = game
                    .pack
                    .assets
                    .registry
                    .default_state("minecraft:iron_block")
                    .unwrap();
                let config = crate::game::levels::entity_owner_tests::configuration(
                    game.world_root.parent().unwrap(),
                    game.dim,
                    state,
                );
                game.world.enable_world_slot_trial(config).unwrap();
            }
            6 => {
                game.world.block_entities.insert(
                    (3, 70, 3),
                    horizon_client::containers::BlockEntity::Chest(vec![Some(ItemStack::new(
                        "minecraft:dirt",
                        3,
                    ))]),
                );
            }
            7 => game.spill.removed.push((
                [3, 70, 3],
                horizon_client::containers::BlockEntity::Chest(vec![Some(ItemStack::new(
                    "minecraft:dirt",
                    3,
                ))]),
            )),
            8 => {
                let _ = game.streaming.expect_existing_mut().update(&mut game.world);
            }
            9 => game.lan.as_mut().unwrap().hub.guests[1].dimension = Dim::Nether.id().into(),
            _ => unreachable!(),
        }
        let req = request(&game, &identity, 1);
        let source = disk(&game.world_root);
        let archive = disk(&req.archive_root);
        let players_before = player_state(&game);
        let error = dispatch_capture(&mut game, req.clone()).unwrap_err();
        assert_eq!(error.publication, Publication::NotPublished);
        match case {
            0..=3 => assert_eq!(error.code, Failure::Player),
            4 => assert_eq!(error.code, Failure::Context),
            5 => assert_eq!(error.code, Failure::Entity),
            6..=7 => assert_eq!(error.code, Failure::World),
            8 => assert_eq!(error.code, Failure::Streamer),
            9 => assert_eq!(error.code, Failure::Player),
            _ => unreachable!(),
        }
        assert_eq!(disk(&game.world_root), source);
        assert_eq!(disk(&req.archive_root), archive);
        assert_eq!(player_state(&game), players_before);
        assert!(game.world.counter_trial_snapshot().is_some());
        assert!(detached.is_some() == (case == 0));
        if case == 7 {
            assert_eq!(game.spill.removed.len(), 1);
        }
    }
}
fn captured_template(game: &mut Game<'_>, identity: &Identity) -> PathBuf {
    let req = request(game, identity, 1);
    let archive = req.archive_root.clone();
    dispatch_capture(game, req).unwrap();
    archive
}
fn offline_material(
    epoch_root: &Path,
    archive: &Path,
    identity: &Identity,
    epoch: u64,
) -> Material {
    let manifest = verify_image(epoch_root, 1, identity).unwrap();
    let players = manifest
        .players
        .into_iter()
        .map(|descriptor| {
            let nbt = bounded_read(
                &epoch_root.join(player_target(&descriptor, "data")),
                2 << 20,
                Phase::Recover,
            )
            .unwrap();
            let advancements = bounded_read(
                &epoch_root.join(player_target(&descriptor, "advancements")),
                JSON_LIMIT,
                Phase::Recover,
            )
            .unwrap();
            let stats = bounded_read(
                &epoch_root.join(player_target(&descriptor, "stats")),
                JSON_LIMIT,
                Phase::Recover,
            )
            .unwrap();
            PlayerImage {
                descriptor,
                nbt,
                advancements,
                stats,
            }
        })
        .collect();
    let counter = parse_json(
        &bounded_read(
            &epoch_root.join("research/counter.json"),
            JSON_LIMIT,
            Phase::Recover,
        )
        .unwrap(),
        JSON_LIMIT,
    )
    .unwrap();
    static NEXT_INPUT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let source = archive.parent().unwrap().join(format!(
        "offline-input-{}-{}",
        std::process::id(),
        NEXT_INPUT.fetch_add(1, Ordering::Relaxed)
    ));
    clone_files(&epoch_root.join("world"), &source);
    Material {
        source,
        request: CaptureRequest {
            archive_root: archive.to_owned(),
            epoch,
            identity: identity.clone(),
        },
        players,
        counter,
        dimensions: manifest.dimensions,
    }
}
#[test]
#[ignore = "requires pinned capture identities/resources and isolated cwd/home/temp"]
fn capture_cell_native_save_and_staging_io_faults_keep_previous_epoch() {
    let baseline = Fixture::new("capture-fault-baseline");
    let (mut original, _body, _receivers, identity) = setup(&baseline, false);
    let archive = captured_template(&mut original, &identity);
    let old = disk(&archive.join("epoch-00000000000000000001"));
    for ticket in [false, true] {
        let fixture = Fixture::new("capture-native-fault");
        let (mut game, _body, _receivers, current) = setup(&fixture, false);
        assert_eq!(current, identity);
        let source_root = game.save.as_ref().unwrap().root.clone();
        fs::create_dir_all(&source_root).unwrap();
        if ticket {
            let file = source_root.join("data/minecraft/chunk_tickets.dat");
            write_dat(
                &file,
                Tag::compound([
                    ("DataVersion", Tag::Int(horizon_world::DATA_VERSION)),
                    (
                        "data",
                        Tag::compound([
                            ("tickets", Tag::List(Vec::new())),
                            ("fixture:opaque", Tag::Int(19)),
                        ]),
                    ),
                ]),
            )
            .unwrap();
            fs::create_dir(file.with_file_name("chunk_tickets.dat_new")).unwrap();
        } else {
            fs::write(
                source_root.join("region"),
                b"owned region-directory obstruction",
            )
            .unwrap();
        }
        let save = game.save.as_ref().unwrap().clone();
        let counter = game.world.counter_trial_snapshot();
        let players_before = player_state(&game);
        let before = disk(&game.world_root);
        let archive_before = disk(&archive);
        let error = dispatch_capture(
            &mut game,
            CaptureRequest {
                archive_root: archive.clone(),
                epoch: 2,
                identity: identity.clone(),
            },
        )
        .unwrap_err();
        assert_eq!(error.phase, Phase::Save);
        assert_eq!(error.code, Failure::NativeSave);
        assert_eq!(error.publication, Publication::NotPublished);
        assert_eq!(game.dim, Dim::Overworld);
        assert!(Arc::ptr_eq(&save, game.save.as_ref().unwrap()));
        assert_eq!(game.world.counter_trial_snapshot(), counter);
        assert_eq!(player_state(&game), players_before);
        if ticket {
            assert_eq!(disk(&game.world_root), before);
            save.flush().unwrap();
        } else {
            let sticky = save.flush().unwrap_err().to_string();
            assert!(sticky.contains("region"));
            assert_eq!(save.flush().unwrap_err().to_string(), sticky);
            save_footprint(&game, before, None);
        }
        assert_eq!(disk(&archive.join("epoch-00000000000000000001")), old);
        assert_eq!(disk(&archive), archive_before);
        assert!(!archive.join("epoch-00000000000000000002").exists());
    }
    let source = archive.join("epoch-00000000000000000001");
    // Exercise the REAL private publisher phases, not a callback in capture.
    for (index, phase) in ["copy", "overlay", "overlay-parent", "promote"]
        .into_iter()
        .enumerate()
    {
        let epoch = index as u64 + 2;
        let material = offline_material(&source, &archive, &identity, epoch);
        let actual_source = material.source.clone();
        let source_before = disk(&actual_source);
        let mut expected = disk(&archive);
        let pending = archive.join(format!(".pending-{epoch:020}"));
        let overlays: Vec<_> = material
            .players
            .iter()
            .flat_map(|row| {
                let destination = &pending;
                [
                    ("data", &row.nbt),
                    ("advancements", &row.advancements),
                    ("stats", &row.stats),
                ]
                .into_iter()
                .map(move |(kind, bytes)| {
                    (
                        destination.join(format!(
                            "world/players/{kind}/{}.{}",
                            row.descriptor.record_uuid,
                            if kind == "data" { "dat" } else { "json" }
                        )),
                        bytes.to_vec(),
                    )
                })
            })
            .collect();
        if phase == "copy" {
            fs::create_dir(&pending).unwrap();
            expected.insert(pending.clone(), None);
            let error = material
                .copy()
                .err()
                .expect("real existing-directory obstruction");
            assert_eq!(error.phase, Phase::Copy);
            assert_eq!(error.code, Failure::Io);
            assert_eq!(disk(&archive), expected);
        } else {
            let mut image = material.copy().unwrap();
            expected.insert(pending.clone(), None);
            for (path, bytes) in &source_before {
                expected.insert(
                    pending
                        .join("world")
                        .join(path.strip_prefix(&actual_source).unwrap()),
                    bytes.clone(),
                );
            }
            // Full physical-copy footprint, including directories and unrelated bytes.
            assert_eq!(disk(&archive), expected);
            assert_eq!(disk(&actual_source), source_before);
            if phase == "overlay-parent" {
                let parent = image.root.join("world/players/stats");
                fs::remove_dir_all(&parent).unwrap();
                fs::write(&parent, b"owned staging parent obstruction").unwrap();
                expected.retain(|path, _| !path.starts_with(&parent));
                expected.insert(
                    parent.clone(),
                    Some(b"owned staging parent obstruction".to_vec()),
                );
                for (path, bytes) in &overlays {
                    if path.parent() == Some(parent.as_path()) {
                        break;
                    }
                    expected.insert(path.clone(), Some(bytes.to_vec()));
                }
                let error = image.overlay().unwrap_err();
                assert_eq!(error.code, Failure::Io);
                assert_eq!(error.phase, Phase::Overlay);
                assert_eq!(disk(&archive), expected);
            } else if phase == "overlay" {
                let target = image
                    .root
                    .join("world/players/data/ffffffff-ffff-fffe-0000-000300000004.dat");
                fs::remove_file(&target).unwrap();
                fs::create_dir(&target).unwrap();
                expected.insert(target.clone(), None);
                for (path, bytes) in &overlays {
                    if path == &target {
                        break;
                    }
                    expected.insert(path.clone(), Some(bytes.to_vec()));
                }
                let error = image.overlay().unwrap_err();
                assert_eq!(error.phase, Phase::Overlay);
                assert_eq!(error.code, Failure::InvalidImage);
                assert_eq!(disk(&archive), expected);
            } else {
                image.overlay().unwrap();
                for (path, bytes) in overlays {
                    expected.insert(path, Some(bytes));
                }
                expected.insert(pending.join("research"), None);
                expected.insert(
                    pending.join("research/counter.json"),
                    Some(
                        bounded_read(
                            &source.join("research/counter.json"),
                            JSON_LIMIT,
                            Phase::Recover,
                        )
                        .unwrap(),
                    ),
                );
                assert_eq!(disk(&archive), expected);
                image.sync_members().unwrap();
                assert_eq!(disk(&archive), expected);
                image.write_manifest_temp().unwrap();
                expected.insert(
                    pending.join("manifest.json.tmp"),
                    Some(image.manifest_bytes.to_vec()),
                );
                assert_eq!(disk(&archive), expected);
                image.finish_manifest().unwrap();
                let bytes = expected.remove(&pending.join("manifest.json.tmp")).unwrap();
                expected.insert(pending.join("manifest.json"), bytes);
                assert_eq!(disk(&archive), expected);
                let destination = archive.join(format!("epoch-{epoch:020}"));
                fs::create_dir(&destination).unwrap();
                expected.insert(destination, None);
                let error = image.promote().unwrap_err();
                assert_eq!(error.phase, Phase::Promote);
                assert_eq!(error.publication, Publication::NotPublished);
                assert_eq!(disk(&archive), expected);
            }
        }
        assert_eq!(disk(&actual_source), source_before);
        assert_eq!(disk(&source), old);
    }
    assert!(!archive.join("latest").exists());
}
fn overwrite_manifest(root: &Path, manifest: &Manifest) {
    fs::write(
        root.join("manifest.json"),
        json_bytes(manifest, MANIFEST_LIMIT).unwrap(),
    )
    .unwrap();
}
fn duplicate_epoch(source: &Path, archive: &Path, identity: &Identity, epoch: u64) -> PathBuf {
    let mut image = offline_material(source, archive, identity, epoch)
        .copy()
        .unwrap();
    image.overlay().unwrap();
    image.sync_members().unwrap();
    image.write_manifest_temp().unwrap();
    image.finish_manifest().unwrap();
    image.promote().unwrap();
    archive.join(format!("epoch-{epoch:020}"))
}
#[test]
#[ignore = "requires pinned capture identities/resources and isolated cwd/home/temp"]
fn capture_cell_manifest_validation_and_whole_epoch_recovery() {
    let fixture = Fixture::new("capture-verify");
    let (mut game, _body, _receivers, identity) = setup(&fixture, false);
    let archive = captured_template(&mut game, &identity);
    let first = archive.join("epoch-00000000000000000001");
    let old = disk(&first);
    // Each malformed newer epoch must be rejected whole; complete epoch1 remains exact.
    for case in 0..14 {
        let epoch = case + 2;
        let newer = duplicate_epoch(&first, &archive, &identity, epoch);
        let mut manifest = verify_image(&newer, epoch, &identity).unwrap();
        match case {
            0 => fs::write(newer.join("manifest.json"), b"{").unwrap(),
            1 => {
                fs::write(
                    newer.join("world/unrelated-sentinel.bin"),
                    b"wrong generation",
                )
                .unwrap();
            }
            2 => {
                fs::write(newer.join("world/extra.bin"), b"unexpected").unwrap();
            }
            3 => {
                fs::remove_file(newer.join("world/unrelated-sentinel.bin")).unwrap();
            }
            4 => {
                manifest.files[0].path = "../escape".into();
                overwrite_manifest(&newer, &manifest);
            }
            5 => {
                manifest.files.push(manifest.files[0].clone());
                overwrite_manifest(&newer, &manifest);
            }
            6 => {
                manifest.epoch = 1;
                overwrite_manifest(&newer, &manifest);
            }
            7 => {
                let path = newer.join("world/unrelated-sentinel.bin");
                fs::remove_file(&path).unwrap();
                symlink(first.join("world/unrelated-sentinel.bin"), path).unwrap();
            }
            8 => {
                let file = File::create(newer.join("manifest.json")).unwrap();
                file.set_len(MANIFEST_LIMIT as u64 + 1).unwrap();
            }
            9 => {
                fs::write(newer.join("manifest.json"), br#"{"schema":1,"schema":1}"#).unwrap();
            }
            10 => {
                let mut bytes = vec![b'['; 34];
                bytes.extend(std::iter::repeat_n(b']', 34));
                fs::write(newer.join("manifest.json"), bytes).unwrap();
            }
            11 => {
                let path = newer.join("world/unrelated-sentinel.bin");
                fs::remove_file(&path).unwrap();
                fs::create_dir(path).unwrap();
            }
            12 => {
                let path = newer.join("research/counter.json");
                let mut value: Value = parse_json(
                    &bounded_read(&path, JSON_LIMIT, Phase::Recover).unwrap(),
                    JSON_LIMIT,
                )
                .unwrap();
                value.as_object_mut().unwrap().remove("last_advanced");
                let bytes = json_bytes(&value, JSON_LIMIT).unwrap();
                fs::write(&path, &bytes).unwrap();
                let member = manifest
                    .files
                    .iter_mut()
                    .find(|m| m.path == "research/counter.json")
                    .unwrap();
                member.bytes = bytes.len() as u64;
                member.sha256 = hash(&bytes);
                overwrite_manifest(&newer, &manifest);
            }
            13 => {
                manifest.players[0].dimension = Dim::Nether.id().into();
                overwrite_manifest(&newer, &manifest);
            }
            _ => unreachable!(),
        }
        let destination = game
            .world_root
            .parent()
            .unwrap()
            .join(format!("recover-{case}"));
        let receipt = recover_fixture_epoch(&archive, &identity, &destination).unwrap();
        assert_eq!(receipt.epoch, 1);
        assert!(receipt.rejected.iter().any(|(e, _)| *e == epoch));
        assert_eq!(disk(&first), old);
        // Retain no more than the fixed published discovery bound.
        drop(receipt);
        fs::remove_dir_all(&newer).unwrap();
    }
    for field in ["engine", "content"] {
        let newer = duplicate_epoch(&first, &archive, &identity, 2);
        let mut manifest = verify_image(&newer, 2, &identity).unwrap();
        if field == "engine" {
            manifest.identity.engine_source = "0".repeat(64);
        } else {
            manifest.identity.counter_content = "0".repeat(64);
        }
        overwrite_manifest(&newer, &manifest);
        let destination = game
            .world_root
            .parent()
            .unwrap()
            .join(format!("wrong-{field}"));
        let error = recover_fixture_epoch(&archive, &identity, &destination).unwrap_err();
        assert_eq!(error.code, Failure::Identity);
        assert!(!destination.exists());
        fs::remove_dir_all(newer).unwrap();
    }
    let malformed = archive.join("epoch-2");
    fs::create_dir(&malformed).unwrap();
    assert!(
        recover_fixture_epoch(
            &archive,
            &identity,
            &game.world_root.parent().unwrap().join("bad-name")
        )
        .is_err()
    );
    fs::remove_dir(malformed).unwrap();
    let linked = archive.join("epoch-00000000000000000002");
    symlink(&first, &linked).unwrap();
    assert!(
        recover_fixture_epoch(
            &archive,
            &identity,
            &game.world_root.parent().unwrap().join("bad-link")
        )
        .is_err()
    );
    fs::remove_file(linked).unwrap();
    let existing = game.world_root.parent().unwrap().join("existing");
    fs::create_dir(&existing).unwrap();
    let before = disk(&existing);
    assert!(recover_fixture_epoch(&archive, &identity, &existing).is_err());
    assert_eq!(disk(&existing), before);
    let overflow = game.world_root.parent().unwrap().join("discovery-overflow");
    fs::create_dir(&overflow).unwrap();
    clone_files(&first, &overflow.join("epoch-00000000000000000001"));
    for epoch in 2..=17 {
        fs::create_dir(overflow.join(format!("epoch-{epoch:020}"))).unwrap();
    }
    let overflow_before = disk(&overflow);
    let native_before = disk(&game.world_root);
    let destination = game.world_root.parent().unwrap().join("overflow-recovery");
    let error = recover_fixture_epoch(&overflow, &identity, &destination).unwrap_err();
    assert_eq!(error.code, Failure::Limit);
    assert!(!destination.exists());
    assert_eq!(disk(&overflow), overflow_before);
    assert_eq!(disk(&game.world_root), native_before);
    assert_eq!(disk(&first), old);
    assert!(parse_json::<Value>(b"[1]trailing", JSON_LIMIT).is_err());
    assert!(parse_json::<Value>(b"{\"x\":1,\"x\":2}", JSON_LIMIT).is_err());
    // Depth counts edges from the root at zero: 33 nested arrays reach depth32.
    let mut at_limit = vec![b'['; 33];
    at_limit.extend(std::iter::repeat_n(b']', 33));
    assert!(parse_json::<Value>(&at_limit, JSON_LIMIT).is_ok());
    let mut excessive = vec![b'['; 34];
    excessive.extend(std::iter::repeat_n(b']', 34));
    assert!(parse_json::<Value>(&excessive, JSON_LIMIT).is_err());
    let string = serde_json::to_vec(&"x".repeat(4097)).unwrap();
    assert!(parse_json::<Value>(&string, JSON_LIMIT).is_err());
    let nodes = serde_json::to_vec(&vec![0; NODE_LIMIT + 1]).unwrap();
    assert!(parse_json::<Value>(&nodes, JSON_LIMIT).is_err());
    assert!(player_bytes(&Tag::Double(f64::NAN.to_bits())).is_err());
    assert!(player_bytes(&Tag::Float((-0.0f32).to_bits())).is_err());
    assert!(player_bytes(&Tag::String(nbt::JavaString(vec![0xd800]))).is_err());
}
fn clone_files(source: &Path, target: &Path) {
    fresh_dir(target, Phase::Copy).unwrap();
    let entries = tree(source, &[], Phase::Copy).unwrap();
    for dir in entries.directories {
        fresh_dir(&target.join(dir), Phase::Copy).unwrap();
    }
    for member in entries.files {
        physical_copy(
            &source.join(&member.path),
            &target.join(&member.path),
            &member,
            Phase::Copy,
        )
        .unwrap();
    }
    sync_tree(target, Phase::SyncMembers).unwrap();
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildPlan {
    source_epoch: PathBuf,
    archive: PathBuf,
    context: PathBuf,
    phase: String,
    identity: Identity,
}

#[test]
#[ignore = "AUXILIARY: requires bounded parent plan and isolated child context; never execute as a primary"]
fn capture_cell_phase_child() {
    let encoded = std::env::var("HORIZON_CAPTURE_CHILD_PLAN")
        .expect("auxiliary requires explicit parent plan");
    assert!(encoded.len() <= 4096);
    let plan: ChildPlan = parse_json(encoded.as_bytes(), 4096).unwrap();
    assert!(matches!(
        plan.phase.as_str(),
        "copy" | "members" | "manifest-temp" | "manifest-final" | "promote"
    ));
    let mut material = offline_material(&plan.source_epoch, &plan.archive, &plan.identity, 2);
    // Publisher restart test input is a detached variant of a captured native
    // image. It is not a second claim of live machine/transfer integration.
    let source = plan.context.join("offline-source");
    clone_files(&material.source, &source);
    fs::write(
        source.join("new-epoch-marker.bin"),
        b"NEW epoch physical member",
    )
    .unwrap();
    material.source = source;
    let owner = material
        .players
        .iter_mut()
        .find(|row| row.descriptor.integrated_owner)
        .unwrap();
    let mut tag = strict_player(&owner.nbt).unwrap();
    if let Some(Tag::List(inventory)) = tag.get_mut("Inventory")
        && let Tag::Compound(item) = &mut inventory[0]
    {
        item.insert("count".into(), Tag::Int(9));
    }
    owner.nbt = player_bytes(&tag).unwrap();
    let mut image = material.copy().unwrap();
    if plan.phase != "copy" {
        image.overlay().unwrap();
        image.sync_members().unwrap();
        if plan.phase != "members" {
            image.write_manifest_temp().unwrap();
            if plan.phase != "manifest-temp" {
                image.finish_manifest().unwrap();
                if plan.phase == "promote" {
                    let _unacknowledged = image.rename_published().unwrap();
                }
            }
        }
    }
    println!("\nH08_CAPTURE_PHASE {}", plan.phase);
    std::io::stdout().flush().unwrap();
    // Actual parent-owned stdin pipe, with an escape if its parent disappears.
    let (released, release) = mpsc::channel();
    let _stdin = std::thread::spawn(move || {
        let mut byte = [0u8; 1];
        let result = std::io::stdin()
            .read_exact(&mut byte)
            .map_err(|e| e.to_string());
        let _ = released.send(result);
    });
    let result = release.recv_timeout(Duration::from_secs(30));
    panic!("parent did not terminate its auxiliary child: {result:?}");
}
#[test]
#[ignore = "requires pinned capture identities/resources and isolated cwd/home/temp"]
fn capture_cell_real_phase_restart_preserves_complete_epoch_selection() {
    let fixture = Fixture::new("capture-phases");
    let (mut game, _body, _receivers, identity) = setup(&fixture, false);
    let archive = captured_template(&mut game, &identity);
    let source = archive.join("epoch-00000000000000000001");
    let source_before = disk(&source);
    for phase in [
        "copy",
        "members",
        "manifest-temp",
        "manifest-final",
        "promote",
    ] {
        let context = game
            .world_root
            .parent()
            .unwrap()
            .join(format!("child-{phase}"));
        fs::create_dir(&context).unwrap();
        let archive = context.join("archive");
        fs::create_dir(&archive).unwrap();
        clone_files(&source, &archive.join("epoch-00000000000000000001"));
        sync_dir(&archive, Phase::SyncArchive).unwrap();
        let previous = disk(&archive.join("epoch-00000000000000000001"));
        let plan = ChildPlan {
            source_epoch: source.clone(),
            archive: archive.clone(),
            context: context.clone(),
            phase: phase.into(),
            identity: identity.clone(),
        };
        let err = File::create(context.join("child.stderr.log")).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "game::levels::capture_integration::tests::capture_cell_phase_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .current_dir(&context)
            .env(
                "HORIZON_CAPTURE_CHILD_PLAN",
                serde_json::to_string(&plan).unwrap(),
            )
            .env_remove("HORIZON_SESSION_FILE")
            .env("HORIZON_LAUNCHER_NO_SELF_UPDATE", "1")
            .env("HORIZON_NO_AUDIO", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(err));
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
            let path = context.join(folder);
            fs::create_dir_all(&path).unwrap();
            command.env(name, path);
        }
        // Preserve inherited XDG_RUNTIME_DIR; no user/game/session environment is reused.
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stdout_log = context.join("child.stdout.log");
        let expected_signal = format!("H08_CAPTURE_PHASE {phase}");
        let marker = expected_signal.clone();
        let (signaled, signal) = mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let mut input = BufReader::new(stdout);
            let mut log = File::create(stdout_log).unwrap();
            loop {
                let mut line = Vec::new();
                match (&mut input).take(4097).read_until(b'\n', &mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        log.write_all(&line).unwrap();
                        if line.len() > 4096 {
                            let _ =
                                signaled.try_send(Err("oversized child output line".to_owned()));
                            break;
                        }
                        let text = String::from_utf8_lossy(&line);
                        let text = text.trim_end_matches(['\r', '\n']);
                        if text.starts_with("H08_CAPTURE_PHASE ") {
                            let message = if text == marker {
                                Ok(text.to_owned())
                            } else {
                                Err(format!("unexpected phase signal {text}"))
                            };
                            let _ = signaled.try_send(message);
                        }
                    }
                    Err(error) => {
                        let _ = signaled.try_send(Err(error.to_string()));
                        break;
                    }
                }
            }
        });
        let reached = signal.recv_timeout(Duration::from_secs(20));
        let killed = child.kill();
        let status = child.wait().unwrap();
        reader.join().unwrap();
        let correct = matches!(&reached, Ok(Ok(text)) if text == &expected_signal);
        assert!(
            correct,
            "child did not signal {phase}: {reached:?}; stdout: {}; stderr: {}",
            fs::read_to_string(context.join("child.stdout.log")).unwrap(),
            fs::read_to_string(context.join("child.stderr.log")).unwrap()
        );
        killed.unwrap();
        assert!(!status.success());
        let destination = context.join("recovered");
        let receipt = recover_fixture_epoch(&archive, &identity, &destination).unwrap();
        let promoted = phase == "promote";
        assert_eq!(receipt.epoch, if promoted { 2 } else { 1 });
        assert_eq!(destination.join("new-epoch-marker.bin").exists(), promoted);
        if promoted {
            assert_eq!(
                fs::read(destination.join("new-epoch-marker.bin")).unwrap(),
                b"NEW epoch physical member"
            );
        }
        let owner = strict_player(
            &bounded_read(
                &destination.join("players/data/ffffffff-ffff-fffe-0000-000300000004.dat"),
                2 << 20,
                Phase::Recover,
            )
            .unwrap(),
        )
        .unwrap();
        let Some(Tag::List(inventory)) = owner.get("Inventory") else {
            panic!("inventory missing")
        };
        assert_eq!(
            inventory[0].get("count"),
            Some(&Tag::Int(if promoted { 9 } else { 7 }))
        );
        assert_eq!(disk(&archive.join("epoch-00000000000000000001")), previous);
        assert_eq!(disk(&source), source_before);
        // SIGKILL restart selection is not empirical simulated power loss.
    }
}
#[test]
#[ignore = "requires pinned capture identities/resources and isolated cwd/home/temp"]
fn capture_task_observer_timeout_keeps_owner_exclusive_until_definite_completion() {
    let fixture = Fixture::new("capture-timeout");
    let (mut game, _body, _receivers, identity) = setup(&fixture, false);
    let req = request(&game, &identity, 1);
    let archive = req.archive_root.clone();
    let before = disk(&game.world_root);
    let players_before = player_state(&game);
    let instance = game
        .world
        .counter_trial_snapshot()
        .unwrap()
        .record()
        .unwrap()
        .instance();
    let receipt = dispatch_capture_after_observer_timeout(&mut game, req).unwrap();
    assert_eq!(receipt.epoch, 1);
    assert!(game.serving.is_none());
    assert_eq!(player_state(&game), players_before);
    save_footprint(&game, before, None);
    assert_native_recovery(
        &game,
        &game.world_root.parent().unwrap().join("timeout-recovered"),
        &identity,
        &archive,
        1,
        instance,
    );
}
