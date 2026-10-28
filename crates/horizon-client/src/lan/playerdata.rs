//! Guests' saved state: `players/data/<uuid>.dat` in the vanilla player
//! compound, through the same code that saves the host.
use super::guest::{Guest, SavedBody};
use crate::{
    items::ItemStack,
    save::{Level, read_dat, write_dat},
};
use horizon_protocol::crypto::uuid_string;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub fn path(world: &Path, uuid: u128) -> PathBuf {
    world
        .join("players")
        .join("data")
        .join(format!("{}.dat", uuid_string(uuid)))
}

/// `UUIDUtil.uuidToIntArray`.
pub fn uuid_ints(uuid: u128) -> [i32; 4] {
    [
        (uuid >> 96) as i32,
        (uuid >> 64) as i32,
        (uuid >> 32) as i32,
        uuid as i32,
    ]
}
/// `UUIDUtil.uuidFromIntArray`.
pub fn ints_uuid(ints: [i32; 4]) -> u128 {
    ints.iter()
        .fold(0, |acc, &i| acc << 32 | u128::from(i as u32))
}

/// Whether a saved file exists for this player.
pub fn exists(world: &Path, uuid: u128) -> bool {
    path(world, uuid).is_file()
}

/// Load a guest's saved state, if they have been here before.
pub fn load(guest: &mut Guest, world: &Path) -> bool {
    let Some(tag) = read_dat(&path(world, guest.uuid)) else {
        return false;
    };
    let mut level = Level {
        health: 20.0,
        food: 20,
        saturation: 5.0,
        ..Level::default()
    };
    level.read_player(&tag);
    level.player_extra = Some(tag);
    apply(guest, &level);
    true
}

/// Give a guest the player state read into `level`.
pub fn apply(guest: &mut Guest, level: &Level) {
    if let Some(p) = level.position {
        guest.position = p;
        guest.previous_position = p;
    }
    guest.yaw = level.rotation[0];
    guest.pitch = level.rotation[1];
    guest.head_yaw = guest.yaw;
    guest.on_ground = level.on_ground;
    guest.health = level.health;
    guest.saved_absorption = Some(level.absorption);
    guest.food = level.food;
    guest.saturation = level.saturation;
    guest.xp_level = level.xp_level;
    guest.xp_progress = level.xp_progress;
    guest.xp_total = level.xp_total;
    guest.xp_seed = level.xp_seed;
    guest.effects = crate::effects::PlayerEffects::from_saved(&level.active_effects);
    guest.game_mode = level.mode();
    guest.flying = level.flying;
    guest.selected = (level.selected_slot.max(0) as usize).min(8);
    // A point that is the world spawn is none of the player's own: it
    // respawns wherever the world spawn is then (`setworldspawn`).
    guest.spawn = level
        .spawn
        .filter(|s| Some(s.map(|v| v.floor() as i32)) != level.world_spawn)
        .map(|s| [s[0] + 0.5, s[1], s[2] + 0.5]);
    guest.respawn_elsewhere = level.respawn_elsewhere.clone();
    guest.saved = level.player_extra.clone();
    guest.seen_credits = level.seen_credits;
    guest.last_death = level.last_death.clone();
    let extra = level.player_extra.as_ref();
    let short = |key: &str, default: i64| {
        extra
            .and_then(|t| crate::save::number(t.get(key)))
            .unwrap_or(default) as i32
    };
    guest.loaded_body = Some(SavedBody {
        fire_ticks: short("Fire", -20),
        air: short("Air", 300),
        fall_distance: level.fall_distance,
    });
    guest.recipe_book = level.recipe_book.clone();
    guest.dimension = match level.dimension.as_str() {
        d @ ("minecraft:the_nether" | "minecraft:the_end") => d.to_owned(),
        _ => "minecraft:overworld".to_owned(),
    };
    if level.ender_items.len() == 27 {
        guest.ender_items = level.ender_items.clone();
    }
    for (slot, item, count, damage) in &level.inventory {
        let mut stack = ItemStack {
            item: Arc::from(item.as_str()),
            count: *count,
            damage: *damage,
            extra: level.inventory_components.get(slot).cloned(),
        };
        if stack.count == 0 {
            stack.count = 1;
        }
        match *slot {
            0..=35 => guest.inventory[usize::from(*slot)] = Some(stack),
            100 => guest.armor[3] = Some(stack),
            101 => guest.armor[2] = Some(stack),
            102 => guest.armor[1] = Some(stack),
            103 => guest.armor[0] = Some(stack),
            150 => guest.offhand = Some(stack),
            _ => {}
        }
    }
}

/// Save a guest (PlayerDataStorage.save).
pub fn save(guest: &Guest, world: &Path) -> crate::Result<()> {
    let mut level = Level {
        player_uuid: Some(uuid_ints(guest.uuid)),
        ..Level::default()
    };
    store(guest, &mut level);
    let file = path(world, guest.uuid);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", file.display()))?;
    }
    write_dat(&file, level.player_tag())
        .map_err(|error| format!("{}: {error}", file.display()).into())
}

/// Put a guest's state in `level`'s player fields.
pub fn store(guest: &Guest, level: &mut Level) {
    level.position = Some(guest.position);
    level.dimension = guest.dimension.clone();
    level.rotation = [guest.yaw, guest.pitch];
    level.health = guest.health;
    level.absorption = guest
        .motion
        .as_ref()
        .map_or(guest.saved_absorption.unwrap_or(0.0), |m| {
            m.health.absorption()
        });
    level.food = guest.food;
    level.saturation = guest.saturation;
    level.xp_level = guest.xp_level;
    level.xp_progress = guest.xp_progress;
    level.xp_total = guest.xp_total;
    level.xp_seed = guest.xp_seed;
    level.active_effects = guest.effects.saved();
    level.game_mode = guest.game_mode;
    level.creative = guest.game_mode == 1;
    level.flying = guest.flying;
    level.selected_slot = guest.selected as i32;
    level.spawn = guest.spawn.map(|s| [s[0] - 0.5, s[1], s[2] - 0.5]);
    level.respawn_elsewhere = guest.respawn_elsewhere.clone();
    level.on_ground = guest.on_ground;
    level.seen_credits = guest.seen_credits;
    level.last_death = guest.last_death.clone();
    level.player_extra = guest.saved.clone();
    // The live body's fire, air and fall distance (Entity.saveWithoutId),
    // or what the file had while the player has no body yet.
    let body = guest
        .motion
        .as_deref()
        .map(SavedBody::of)
        .or(guest.loaded_body);
    if let Some(body) = body {
        level.fall_distance = body.fall_distance;
        let extra = level
            .player_extra
            .get_or_insert_with(|| horizon_world::nbt::Tag::Compound(Default::default()));
        if let horizon_world::nbt::Tag::Compound(map) = extra {
            let short = |v: i32| horizon_world::nbt::Tag::Short(v.clamp(-32768, 32767) as i16);
            map.insert("Fire".into(), short(body.fire_ticks));
            map.insert("Air".into(), short(body.air));
        }
    }
    level.recipe_book = guest.recipe_book.clone();
    level.ender_items = guest.ender_items.clone();
    level.inventory.clear();
    level.inventory_components.clear();
    let mut put = |slot: u8, stack: &Option<ItemStack>| {
        if let Some(s) = stack {
            level
                .inventory
                .push((slot, s.item.to_string(), s.count, s.damage));
            if let Some(extra) = &s.extra {
                level.inventory_components.insert(slot, extra.clone());
            }
        }
    };
    for (i, s) in guest.inventory.iter().enumerate() {
        put(i as u8, s);
    }
    put(103, &guest.armor[0]);
    put(102, &guest.armor[1]);
    put(101, &guest.armor[2]);
    put(100, &guest.armor[3]);
    put(150, &guest.offhand);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lan::{Joined, PlayerSender};
    use horizon_protocol::packets::configuration::ClientInformation;
    use horizon_world::nbt::Tag;
    use std::sync::{Arc, atomic::AtomicI32, mpsc};

    fn guest() -> Guest {
        let (tx, _rx) = mpsc::channel();
        Guest::new(
            Joined {
                conn: 2,
                uuid: 2,
                name: "Alex".into(),
                properties: Vec::new(),
                info: ClientInformation::new(8),
                brand: "test".into(),
                sender: PlayerSender::new(tx, Arc::default()),
                latency: Arc::new(AtomicI32::new(0)),
                address: "127.0.0.1:25565".parse().unwrap(),
            },
            2,
            [0.0, 64.0, 0.0],
            8,
        )
    }

    fn scratch(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "horizon-playerdata-{name}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn populated_guest() -> Guest {
        let mut g = guest();
        g.uuid = 0x8000_0000_ffff_ffff_0123_4567_89ab_cdef;
        g.position = [-2.1, -65.9, 3.9];
        g.dimension = "minecraft:the_nether".into();
        g.yaw = -42.5;
        g.pitch = 17.25;
        g.selected = 4;
        g.loaded_body = Some(SavedBody {
            fire_ticks: 96,
            air: 250,
            fall_distance: 9.5,
        });
        let mut sword = ItemStack::new("minecraft:diamond_sword", 1);
        sword.damage = 5;
        sword.data_mut().custom_name = Some("Test blade".into());
        sword.data_mut().other.push((
            "test:opaque_component".into(),
            Tag::compound([("value", Tag::Long(73))]),
        ));
        g.inventory[0] = Some(sword);
        for (slot, id) in g.armor.iter_mut().zip([
            "minecraft:diamond_helmet",
            "minecraft:diamond_chestplate",
            "minecraft:diamond_leggings",
            "minecraft:diamond_boots",
        ]) {
            *slot = Some(ItemStack::new(id, 1));
        }
        g.offhand = Some(ItemStack::new("minecraft:torch", 12));
        g.ender_items[5] = Some(ItemStack::new("minecraft:ender_pearl", 12));
        g.carried = Some(ItemStack::new("minecraft:stone", 3));
        g.craft_grid[0] = Some(ItemStack::new("minecraft:oak_planks", 2));
        g.craft_result = Some(ItemStack::new("minecraft:stick", 4));
        g.saved = Some(Tag::compound([
            (
                "test:opaque",
                Tag::compound([("nested", Tag::List(vec![Tag::Int(11), Tag::Int(-7)]))]),
            ),
            ("UUID", Tag::IntArray(vec![9; 4])),
        ]));
        g.ack_up_to.set(23);
        g
    }

    #[derive(Debug, PartialEq)]
    struct BorrowedState {
        record: Tag,
        inventory: Vec<Option<ItemStack>>,
        armor: [Option<ItemStack>; 4],
        offhand: Option<ItemStack>,
        ender_items: Vec<Option<ItemStack>>,
        carried: Option<ItemStack>,
        craft_grid: [Option<ItemStack>; 4],
        craft_result: Option<ItemStack>,
        saved: Option<Tag>,
        placed: bool,
        ack_up_to: i32,
    }
    fn borrowed_state(g: &Guest) -> BorrowedState {
        let mut level = Level {
            player_uuid: Some(uuid_ints(g.uuid)),
            ..Level::default()
        };
        store(g, &mut level);
        BorrowedState {
            record: level.player_tag(),
            inventory: g.inventory.clone(),
            armor: g.armor.clone(),
            offhand: g.offhand.clone(),
            ender_items: g.ender_items.clone(),
            carried: g.carried.clone(),
            craft_grid: g.craft_grid.clone(),
            craft_result: g.craft_result.clone(),
            saved: g.saved.clone(),
            placed: g.placed,
            ack_up_to: g.ack_up_to.get(),
        }
    }

    fn read_written_gzip(file: &Path) -> Tag {
        let bytes = std::fs::read(file).unwrap();
        assert!(bytes.starts_with(&[0x1f, 0x8b]));
        let raw = horizon_world::compression::Compression::Gzip
            .decode(&bytes, 64 << 20)
            .unwrap();
        let (named, consumed) =
            horizon_world::nbt::read_named(&raw, horizon_world::nbt::Limits::default()).unwrap();
        assert_eq!(consumed, raw.len());
        named.tag
    }

    fn assert_populated_record(tag: &Tag) {
        assert_eq!(
            tag.get("DataVersion"),
            Some(&Tag::Int(horizon_world::DATA_VERSION))
        );
        assert_eq!(
            tag.get("UUID"),
            Some(&Tag::IntArray(vec![
                i32::MIN,
                -1,
                0x0123_4567,
                0x89ab_cdef_u32 as i32
            ]))
        );
        assert_eq!(
            tag.get("Pos"),
            Some(&Tag::List(vec![
                Tag::Double((-2.1_f64).to_bits()),
                Tag::Double((-65.9_f64).to_bits()),
                Tag::Double(3.9_f64.to_bits())
            ]))
        );
        assert_eq!(
            tag.get("Dimension"),
            Some(&Tag::String("minecraft:the_nether".into()))
        );
        assert_eq!(
            tag.get("Rotation"),
            Some(&Tag::List(vec![
                Tag::Float((-42.5_f32).to_bits()),
                Tag::Float(17.25_f32.to_bits())
            ]))
        );
        assert_eq!(tag.get("SelectedItemSlot"), Some(&Tag::Int(4)));
        assert_eq!(tag.get("Fire"), Some(&Tag::Short(96)));
        assert_eq!(tag.get("Air"), Some(&Tag::Short(250)));
        assert_eq!(
            tag.get("fall_distance"),
            Some(&Tag::Double(9.5_f64.to_bits()))
        );
        assert_eq!(
            tag.get("Inventory"),
            Some(&Tag::List(vec![Tag::compound([
                ("Slot", Tag::Byte(0)),
                ("id", Tag::String("minecraft:diamond_sword".into())),
                ("count", Tag::Int(1)),
                (
                    "components",
                    Tag::compound([
                        ("minecraft:damage", Tag::Int(5)),
                        ("minecraft:custom_name", Tag::String("Test blade".into())),
                        (
                            "test:opaque_component",
                            Tag::compound([("value", Tag::Long(73))])
                        ),
                    ])
                ),
            ])]))
        );
        assert_eq!(
            tag.get("equipment"),
            Some(&Tag::compound([
                (
                    "head",
                    Tag::compound([
                        ("id", Tag::String("minecraft:diamond_helmet".into())),
                        ("count", Tag::Int(1))
                    ])
                ),
                (
                    "chest",
                    Tag::compound([
                        ("id", Tag::String("minecraft:diamond_chestplate".into())),
                        ("count", Tag::Int(1))
                    ])
                ),
                (
                    "legs",
                    Tag::compound([
                        ("id", Tag::String("minecraft:diamond_leggings".into())),
                        ("count", Tag::Int(1))
                    ])
                ),
                (
                    "feet",
                    Tag::compound([
                        ("id", Tag::String("minecraft:diamond_boots".into())),
                        ("count", Tag::Int(1))
                    ])
                ),
                (
                    "offhand",
                    Tag::compound([
                        ("id", Tag::String("minecraft:torch".into())),
                        ("count", Tag::Int(12))
                    ])
                ),
            ]))
        );
        assert_eq!(
            tag.get("EnderItems"),
            Some(&Tag::List(vec![Tag::compound([
                ("Slot", Tag::Byte(5)),
                ("id", Tag::String("minecraft:ender_pearl".into())),
                ("count", Tag::Int(12)),
            ])]))
        );
        assert_eq!(
            tag.get("test:opaque"),
            Some(&Tag::compound([(
                "nested",
                Tag::List(vec![Tag::Int(11), Tag::Int(-7)])
            )]))
        );
    }

    #[test]
    fn checked_playerdata_parent_failure_repairs_with_exact_nbt_and_borrowed_state() {
        let root = scratch("parent");
        let g = populated_guest();
        let before = borrowed_state(&g);
        let file = path(&root, g.uuid);
        assert_eq!(
            file,
            root.join("players/data/80000000-ffff-ffff-0123-456789abcdef.dat")
        );
        std::fs::create_dir_all(root.join("players")).unwrap();
        std::fs::write(root.join("players/data"), b"blocked parent").unwrap();
        assert!(
            save(&g, &root)
                .unwrap_err()
                .to_string()
                .contains(file.to_str().unwrap())
        );
        assert_eq!(
            std::fs::read(root.join("players/data")).unwrap(),
            b"blocked parent"
        );
        assert_eq!(borrowed_state(&g), before);
        std::fs::remove_file(root.join("players/data")).unwrap();
        save(&g, &root).unwrap();
        assert_populated_record(&read_written_gzip(&file));
        assert_eq!(borrowed_state(&g), before);
        // The standalone writer retains its existing absence of a placed guard.
        assert!(!g.placed);
        let mut back = guest();
        back.uuid = g.uuid;
        assert!(load(&mut back, &root));
        assert_eq!(back.inventory, g.inventory);
        assert_eq!(back.armor, g.armor);
        assert_eq!(back.offhand, g.offhand);
        assert_eq!(back.ender_items, g.ender_items);
        assert_eq!(back.position, g.position);
        assert_eq!(back.dimension, g.dimension);
        assert_eq!(back.loaded_body, g.loaded_body);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checked_playerdata_new_file_failure_preserves_baseline_then_retries_and_resets() {
        let root = scratch("new-file");
        let mut g = populated_guest();
        let file = path(&root, g.uuid);
        save(&g, &root).unwrap();
        let baseline = std::fs::read(&file).unwrap();
        let blocked = file.with_file_name(format!(
            "{}_new",
            file.file_name().unwrap().to_str().unwrap()
        ));
        std::fs::create_dir(&blocked).unwrap();
        g.position = [7.0, 81.5, -9.25];
        g.inventory[0].as_mut().unwrap().damage = 17;
        let changed = borrowed_state(&g);
        assert!(
            save(&g, &root)
                .unwrap_err()
                .to_string()
                .contains(file.to_str().unwrap())
        );
        assert_eq!(std::fs::read(&file).unwrap(), baseline);
        assert_eq!(borrowed_state(&g), changed);
        std::fs::remove_dir(blocked).unwrap();
        save(&g, &root).unwrap();
        let updated = read_written_gzip(&file);
        assert_eq!(
            updated.get("Pos"),
            Some(&Tag::List(vec![
                Tag::Double(7.0_f64.to_bits()),
                Tag::Double(81.5_f64.to_bits()),
                Tag::Double((-9.25_f64).to_bits())
            ]))
        );
        let Some(Tag::List(inventory)) = updated.get("Inventory") else {
            panic!("inventory list")
        };
        assert_eq!(
            inventory[0]
                .get("components")
                .unwrap()
                .get("minecraft:damage"),
            Some(&Tag::Int(17))
        );
        assert_eq!(borrowed_state(&g), changed);
        let backup = file.with_file_name(format!(
            "{}_old",
            file.file_name().unwrap().to_str().unwrap()
        ));
        assert_eq!(std::fs::read(&backup).unwrap(), baseline);
        let previous = std::fs::read(&file).unwrap();
        // No backup removal: exercise the ordinary replacement path a third time.
        g.inventory.fill(None);
        g.armor.fill(None);
        g.offhand = None;
        g.ender_items.fill(None);
        let reset = borrowed_state(&g);
        save(&g, &root).unwrap();
        let empty = read_written_gzip(&file);
        assert_eq!(empty.get("Inventory"), Some(&Tag::List(Vec::new())));
        assert_eq!(empty.get("EnderItems"), Some(&Tag::List(Vec::new())));
        assert!(empty.get("equipment").is_none());
        assert_eq!(empty.get("test:opaque"), updated.get("test:opaque"));
        assert_eq!(borrowed_state(&g), reset);
        assert_eq!(std::fs::read(backup).unwrap(), previous);
        let mut back = guest();
        back.uuid = g.uuid;
        assert!(load(&mut back, &root));
        assert!(back.inventory.iter().all(Option::is_none));
        assert!(back.armor.iter().all(Option::is_none));
        assert!(back.offhand.is_none());
        assert!(back.ender_items.iter().all(Option::is_none));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_saved_burn_and_fall_come_back() {
        // A player saved burning in mid-fall (#1323, #1327): Entity.saveWithoutId's
        // Fire, Air and fall_distance, read back for the next body.
        let mut g = guest();
        g.loaded_body = Some(SavedBody {
            fire_ticks: 96,
            air: 250,
            fall_distance: 9.5,
        });
        let mut level = Level::default();
        store(&g, &mut level);
        let tag = level.player_tag();
        let mut read = Level::default();
        read.read_player(&tag);
        read.player_extra = Some(tag);
        let mut back = guest();
        apply(&mut back, &read);
        assert_eq!(back.loaded_body, g.loaded_body);
    }
}
