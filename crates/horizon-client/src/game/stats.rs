//! Player statistics (`ServerStatsCounter`): the custom counters, blocks
//! mined, items used/crafted/broken/picked up/dropped and mobs killed or
//! killed by, saved to `players/stats/<uuid>.json`, plus the phantom
//! spawner that reads the time since rest (`PhantomSpawner`).
use super::{Dim, Game};
use horizon_core::geometry::Vec3;
use horizon_core::random::RandomSource;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

/// `stats[type][key]`, with `type` and `key` namespaced ids.
#[derive(Default)]
pub(super) struct Stats {
    pub(super) values: BTreeMap<String, BTreeMap<String, i32>>,
    pub(super) loaded: bool,
    /// Ticks until the phantom spawner runs again.
    phantom_tick: i32,
    /// Statistic changes (type, key, amount; `None` = reset) for the
    /// scoreboard criteria to follow.
    pub(super) events: Vec<(String, String, Option<i32>)>,
    /// Changed since the client was last sent them (`StatsCounter`'s dirty
    /// set), by type.
    dirty: BTreeMap<String, BTreeSet<String>>,
}

/// `minecraft:` and `name`, given to `f` from the stack when short: some
/// counters go up every tick for every player, and they are found without
/// a string made for each.
fn namespaced<T>(name: &str, f: impl FnOnce(&str) -> T) -> T {
    const NAMESPACE: &str = "minecraft:";
    let mut buf = [0u8; 96];
    let len = NAMESPACE.len() + name.len();
    if len > buf.len() {
        return f(&format!("{NAMESPACE}{name}"));
    }
    buf[..NAMESPACE.len()].copy_from_slice(NAMESPACE.as_bytes());
    buf[NAMESPACE.len()..len].copy_from_slice(name.as_bytes());
    // Two strings one after the other are one.
    f(std::str::from_utf8(&buf[..len]).unwrap_or_default())
}

/// The map under `key`, made when missing, found without an owned key
/// when there.
fn entry<'a, V: Default>(map: &'a mut BTreeMap<String, V>, key: &str) -> &'a mut V {
    if !map.contains_key(key) {
        map.insert(key.to_owned(), V::default());
    }
    map.get_mut(key).expect("just inserted")
}

impl Stats {
    pub(super) fn add(&mut self, ty: &str, key: &str, n: i32) {
        namespaced(ty, |ty| {
            let v = entry(entry(&mut self.values, ty), key);
            *v = v.saturating_add(n);
            self.events.push((ty.to_owned(), key.to_owned(), Some(n)));
            self.mark_dirty(ty, key);
        });
    }
    fn mark_dirty(&mut self, ty: &str, key: &str) {
        let keys = entry(&mut self.dirty, ty);
        if !keys.contains(key) {
            keys.insert(key.to_owned());
        }
    }
    pub(super) fn get(&self, ty: &str, key: &str) -> i32 {
        self.values
            .get(&format!("minecraft:{ty}"))
            .and_then(|m| m.get(key))
            .copied()
            .unwrap_or(0)
    }
    pub(super) fn reset(&mut self, ty: &str, key: &str) {
        if let Some(m) = self.values.get_mut(&format!("minecraft:{ty}")) {
            m.insert(key.to_owned(), 0);
        }
        self.mark_dirty(&format!("minecraft:{ty}"), key);
        self.events
            .push((format!("minecraft:{ty}"), key.to_owned(), None));
    }
    pub(super) fn custom(&mut self, key: &str, n: i32) {
        namespaced(key, |key| self.add("custom", key, n));
    }
    /// `ServerStatsCounter.markAllDirty`: everything goes out on the next request.
    pub(super) fn mark_all_dirty(&mut self) {
        self.dirty = self
            .values
            .iter()
            .map(|(ty, m)| (ty.clone(), m.keys().cloned().collect()))
            .collect();
    }
    /// `ServerStatsCounter.sendStats`' entries: the dirty statistics and
    /// their values, the set cleared.
    pub(super) fn take_dirty(&mut self) -> Vec<(String, String, i32)> {
        std::mem::take(&mut self.dirty)
            .into_iter()
            .flat_map(|(ty, keys)| keys.into_iter().map(move |key| (ty.clone(), key)))
            .map(|(ty, key)| {
                let v = self.values.get(&ty).and_then(|m| m.get(&key)).copied();
                (ty, key, v.unwrap_or(0))
            })
            .collect()
    }
    /// `ServerStatsCounter.parseLocal`: the counters of a stats file.
    pub(super) fn read(path: &Path) -> Self {
        let mut stats = Self {
            loaded: true,
            ..Self::default()
        };
        let Some(json) = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        else {
            return stats;
        };
        if let Some(all) = json.get("stats").and_then(|s| s.as_object()) {
            for (ty, entries) in all {
                let Some(entries) = entries.as_object() else {
                    continue;
                };
                let map = stats.values.entry(ty.clone()).or_default();
                for (key, v) in entries {
                    if let Some(n) = v.as_i64() {
                        map.insert(key.clone(), n as i32);
                    }
                }
            }
        }
        stats
    }
    /// The existing saved JSON value, borrowed without consuming notifications.
    pub(super) fn to_json(&self) -> serde_json::Value {
        let stats: serde_json::Map<String, serde_json::Value> = self
            .values
            .iter()
            .filter(|(_, m)| !m.is_empty())
            .map(|(ty, m)| {
                let entries: serde_json::Map<String, serde_json::Value> =
                    m.iter().map(|(k, v)| (k.clone(), (*v).into())).collect();
                (ty.clone(), serde_json::Value::Object(entries))
            })
            .collect();
        serde_json::json!({
            "stats": stats,
            "DataVersion": horizon_world::DATA_VERSION,
        })
    }
    /// `ServerStatsCounter.save`.
    pub(super) fn write(&self, path: &Path) -> horizon_client::Result<()> {
        let json = self.to_json();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", path.display()))?;
        }
        let text = serde_json::to_string_pretty(&json)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        std::fs::write(path, text).map_err(|error| format!("{}: {error}", path.display()).into())
    }
    fn save_local_to(
        &self,
        world: &Path,
        uuid: Option<[i32; 4]>,
        remote: bool,
        no_player: bool,
    ) -> horizon_client::Result<()> {
        if !self.loaded || remote || no_player {
            return Ok(());
        }
        let Some(uuid) = uuid else { return Ok(()) };
        self.write(&player_json(world, "stats", uuid))
    }
}
/// `players/<dir>/<uuid>.json` of a world (advancements, stats).
pub(super) fn player_json(world: &Path, dir: &str, uuid: [i32; 4]) -> PathBuf {
    let hex: String = uuid.iter().map(|v| format!("{:08x}", *v as u32)).collect();
    let text = format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    );
    world.join("players").join(dir).join(format!("{text}.json"))
}
impl Game<'_> {
    fn stats_path(&self) -> Option<PathBuf> {
        Some(player_json(
            &self.world_root,
            "stats",
            self.level.player_uuid?,
        ))
    }
    /// `ServerStatsCounter.parseLocal`.
    pub(super) fn load_stats(&mut self) {
        if self.stats.loaded || self.remote.is_some() || self.no_player_here() {
            return;
        }
        self.stats.loaded = true;
        if let Some(path) = self.stats_path() {
            self.stats.values = Stats::read(&path).values;
        }
    }
    /// `ServerStatsCounter.save`.
    pub(super) fn save_stats(&self) -> horizon_client::Result<()> {
        self.stats.save_local_to(
            &self.world_root,
            self.level.player_uuid,
            self.remote.is_some(),
            self.no_player_here(),
        )
    }
    /// The per-tick counters (`Player.tick`/`ServerPlayer.tick` awardStat).
    pub(super) fn tick_stats(&mut self) {
        self.load_stats();
        self.count_tick_stats(self.sleeping.is_some());
        self.tick_movement_stats();
    }
    /// `Player.tick`'s counters for the player in `self.player`.
    pub(super) fn count_tick_stats(&mut self, sleeping: bool) {
        let Some(p) = self.player.as_ref() else {
            return;
        };
        let alive = !p.health.dead_or_dying();
        let crouching = p.pose == horizon_client::pose::Pose::Crouching;
        let s = &mut self.stats;
        s.custom("play_time", 1);
        s.custom("total_world_time", 1);
        if alive {
            s.custom("time_since_death", 1);
            if !sleeping {
                s.custom("time_since_rest", 1);
            }
        }
        if crouching {
            s.custom("sneak_time", 1);
        }
    }
    fn tick_movement_stats(&mut self) {
        let Some(p) = self.player.as_ref() else {
            return;
        };
        let d = p.position.subtract(self.previous_position);
        self.movement_stats(d);
    }
    /// `Player.checkMovementStatistics` / `checkRidingStatistics` for one move.
    pub(super) fn movement_stats(&mut self, d: Vec3) {
        let Some(p) = self.player.as_ref() else {
            return;
        };
        let horizontal = ((d.x * d.x + d.z * d.z).sqrt() * 100.0).round() as i32;
        let total = ((d.x * d.x + d.y * d.y + d.z * d.z).sqrt() * 100.0).round() as i32;
        // Teleports are not travel.
        if total <= 0 || total > 1000 {
            return;
        }
        let crouching = p.pose == horizon_client::pose::Pose::Crouching;
        let feet = [
            p.position.x.floor() as i32,
            p.position.y.floor() as i32,
            p.position.z.floor() as i32,
        ];
        let eye_in_water = self.res.facts().get(self.world.block(
            feet[0],
            (p.position.y + f64::from(p.dimensions.eye_height)).floor() as i32,
            feet[2],
        ));
        let in_water = self
            .res
            .facts()
            .get(self.world.block(feet[0], feet[1], feet[2]))
            .fluid
            == horizon_client::blocks::Fluid::Water;
        let on_ground = p.ground.on_ground;
        let (sprinting, gliding, swimming) = (
            p.sprinting,
            p.fall_flying,
            p.pose == horizon_client::pose::Pose::Swimming,
        );
        let climbing = p.last_climbable.is_some() && d.y > 0.0;
        let riding = self.riding.map(|id| {
            self.mobs
                .iter()
                .find(|m| m.id == id)
                .map(|m| m.species())
                .ok_or(())
        });
        let s = &mut self.stats;
        match riding {
            Some(Ok(sp)) => {
                use horizon_client::mobs::Species;
                // `checkRidingStatistics`: only these mounts count, the horse
                // family by the full 3D distance; any other mount nothing.
                match sp {
                    Species::Pig => s.custom("pig_one_cm", total),
                    Species::Strider => s.custom("strider_one_cm", total),
                    Species::HappyGhast => s.custom("happy_ghast_one_cm", total),
                    Species::Nautilus | Species::ZombieNautilus => {
                        s.custom("nautilus_one_cm", total)
                    }
                    sp if horizon_client::mobs::equine::is_equine(sp) => {
                        s.custom("horse_one_cm", total)
                    }
                    _ => {}
                }
            }
            Some(Err(())) => {
                if self.boats.iter().any(|b| Some(b.id) == self.riding) {
                    s.custom("boat_one_cm", total)
                } else if self.carts.iter().any(|c| Some(c.id) == self.riding) {
                    s.custom("minecart_one_cm", total)
                }
            }
            None if swimming => s.custom("swim_one_cm", total),
            None if eye_in_water.fluid == horizon_client::blocks::Fluid::Water => {
                s.custom("walk_under_water_one_cm", total)
            }
            None if in_water => s.custom("walk_on_water_one_cm", horizontal),
            None if climbing => s.custom("climb_one_cm", (d.y * 100.0).round() as i32),
            None if on_ground => {
                if sprinting {
                    s.custom("sprint_one_cm", horizontal)
                } else if crouching {
                    s.custom("crouch_one_cm", horizontal)
                } else {
                    s.custom("walk_one_cm", horizontal)
                }
            }
            None if gliding => s.custom("aviate_one_cm", total),
            // Airborne and not gliding: more than 25 cm sideways per move
            // (creative flight, or a long jump), not only flying.
            None if horizontal > 25 => s.custom("fly_one_cm", horizontal),
            None => {}
        }
    }
    /// `PhantomSpawner.tick`: tired players under an open night sky draw
    /// phantoms. A custom spawner ticks only while `spawn_mobs` is on
    /// (`ServerChunkCache.tickChunks`), and this one only spawns enemies
    /// (`ServerLevel.isSpawningMonsters`: `spawn_monsters` too). Every
    /// player of the level counts: the game's own and the hosted ones (a
    /// server thread's players are all hosted).
    pub(super) fn spawn_phantoms(&mut self) {
        let rules = &self.ui.rules;
        if !rules.bool("spawn_mobs")
            || !rules.bool("spawn_monsters")
            || !rules.bool("spawn_phantoms")
            || self.dim != Dim::Overworld
        {
            return;
        }
        self.stats.phantom_tick -= 1;
        if self.stats.phantom_tick > 0 {
            return;
        }
        let r = &mut self.world.random;
        self.stats.phantom_tick += (60 + r.next_int_bound(60).unwrap_or(0)) * 20;
        if self.sky_darken() < 5 {
            return;
        }
        let rest = |s: &Stats| s.get("custom", "minecraft:time_since_rest");
        // `level.players()`: (block position, time since rest) of each player
        // who isn't a spectator.
        let mut players = Vec::new();
        if let Some(p) = self.player.as_ref()
            && !self.spectator()
        {
            players.push((p.position, rest(&self.stats)));
        }
        if let Some(host) = self.lan.as_ref() {
            let level = self.dim.id();
            for g in host.hub.guests.iter().filter(|g| {
                g.placed
                    && g.dimension == level
                    && g.game_mode != horizon_client::lan::guest::SPECTATOR
            }) {
                let since = host.play.get(&g.conn).map_or(0, |p| rest(&p.stats));
                players.push((
                    Vec3::new(g.position[0], g.position[1], g.position[2]),
                    since,
                ));
            }
        }
        for (position, since) in players {
            self.spawn_phantoms_near(position, since);
        }
    }
    /// One player's turn of `PhantomSpawner.tick`.
    fn spawn_phantoms_near(&mut self, position: Vec3, since_rest: i32) {
        let pos = [
            position.x.floor() as i32,
            position.y.floor() as i32,
            position.z.floor() as i32,
        ];
        if pos[1] < 63 || !self.can_see_sky(pos) {
            return;
        }
        // `getCurrentDifficultyAt(playerPos).isHarderThan(random * 3)`.
        let difficulty = self.local_difficulty([position.x, position.y, position.z]);
        let r = &mut self.world.random;
        if difficulty.effective <= r.next_float() * 3.0 {
            return;
        }
        if r.next_int_bound(since_rest.max(1)).unwrap_or(0) < 72000 {
            return;
        }
        // `playerPos.above(20 + nextInt(15)).east(-10 + nextInt(21)).south(...)`:
        // the height is drawn first.
        let up = 20 + r.next_int_bound(15).unwrap_or(0);
        let east = -10 + r.next_int_bound(21).unwrap_or(10);
        let south = -10 + r.next_int_bound(21).unwrap_or(10);
        let at = [pos[0] + east, pos[1] + up, pos[2] + south];
        if !self
            .res
            .facts()
            .is_air(self.world.block(at[0], at[1], at[2]))
        {
            return;
        }
        let group = 1 + self.world.random.next_int_bound(3).unwrap_or(0);
        for _ in 0..group {
            let _ = self.spawn_mob(
                "minecraft:phantom",
                [
                    f64::from(at[0]) + 0.5,
                    f64::from(at[1]),
                    f64::from(at[2]) + 0.5,
                ],
                false,
            );
        }
    }
    fn can_see_sky(&self, pos: [i32; 3]) -> bool {
        (pos[1] + 1..pos[1] + 256).all(|y| {
            let f = self.res.facts().get(self.world.block(pos[0], y, pos[2]));
            !f.full_collision && f.fluid == horizon_client::blocks::Fluid::None
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_scratch(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "horizon-stats-json-{name}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }
    fn json_read(path: &Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn borrowed_statistics_json_preserves_notifications_and_legacy_pretty_text() {
        let root = json_scratch("borrowed-projection");
        let path = root.join("nested/stats.json");
        let mut stats = Stats {
            phantom_tick: 17,
            ..Stats::default()
        };
        stats.custom("jump", 8);
        stats.reset("custom", "minecraft:jump");
        stats.add("mined", "minecraft:stone", -3);
        stats.values.insert("example:empty".into(), BTreeMap::new());
        let before = (
            stats.values.clone(),
            stats.events.clone(),
            stats.dirty.clone(),
            stats.loaded,
            stats.phantom_tick,
        );
        let expected = serde_json::json!({"DataVersion":horizon_world::DATA_VERSION,"stats":{"minecraft:custom":{"minecraft:jump":0},"minecraft:mined":{"minecraft:stone":-3}}});
        assert_eq!(stats.to_json(), expected);
        assert_eq!(stats.to_json(), expected);
        assert!(!path.exists());
        assert_eq!(
            (
                stats.values.clone(),
                stats.events.clone(),
                stats.dirty.clone(),
                stats.loaded,
                stats.phantom_tick
            ),
            before
        );
        stats.write(&path).unwrap();
        // Authored independently of either formatter call, including old whitespace/order.
        let text = r#"{
  "DataVersion": DATA_VERSION,
  "stats": {
    "minecraft:custom": {
      "minecraft:jump": 0
    },
    "minecraft:mined": {
      "minecraft:stone": -3
    }
  }
}"#
        .replace("DATA_VERSION", &horizon_world::DATA_VERSION.to_string());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert_eq!(
            (
                stats.values.clone(),
                stats.events.clone(),
                stats.dirty.clone(),
                stats.loaded,
                stats.phantom_tick
            ),
            before
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checked_stats_file_failures_retry_and_zero_reset_preserves_network_dirty_and_events() {
        for leaf in [false, true] {
            let root = json_scratch(if leaf { "leaf" } else { "parent" });
            let path = root.join("nested/stats.json");
            if leaf {
                std::fs::create_dir_all(&path).unwrap();
            } else {
                std::fs::write(root.join("nested"), b"blocked parent").unwrap();
            }
            let mut stats = Stats {
                loaded: true,
                phantom_tick: 31,
                ..Stats::default()
            };
            stats.add("mined", "minecraft:stone", 9);
            let before = (
                stats.values.clone(),
                stats.events.clone(),
                stats.dirty.clone(),
                stats.loaded,
                stats.phantom_tick,
            );
            assert!(
                stats
                    .write(&path)
                    .unwrap_err()
                    .to_string()
                    .contains(path.to_str().unwrap())
            );
            assert_eq!(
                (
                    stats.values.clone(),
                    stats.events.clone(),
                    stats.dirty.clone(),
                    stats.loaded,
                    stats.phantom_tick
                ),
                before
            );
            if leaf {
                std::fs::remove_dir(&path).unwrap();
            } else {
                std::fs::remove_file(root.join("nested")).unwrap();
            }
            stats.write(&path).unwrap();
            assert_eq!(
                json_read(&path),
                serde_json::json!({"stats":{"minecraft:mined":{"minecraft:stone":9}},"DataVersion":horizon_world::DATA_VERSION})
            );
            assert_eq!(
                (
                    stats.values.clone(),
                    stats.events.clone(),
                    stats.dirty.clone(),
                    stats.loaded,
                    stats.phantom_tick
                ),
                before
            );
            stats.reset("mined", "minecraft:stone");
            let events = stats.events.clone();
            let dirty = stats.dirty.clone();
            stats.write(&path).unwrap();
            assert_eq!(
                json_read(&path),
                serde_json::json!({"stats":{"minecraft:mined":{"minecraft:stone":0}},"DataVersion":horizon_world::DATA_VERSION})
            );
            assert_eq!(Stats::read(&path).get("mined", "minecraft:stone"), 0);
            assert_eq!(stats.events, events);
            assert_eq!(stats.dirty, dirty);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn local_stats_json_guards_keep_sentinel_and_missing_identity_creates_no_filename() {
        let uuid = [-1, 0, i32::MIN, i32::MAX];
        for (loaded, remote, no_player) in [
            (false, false, false),
            (true, true, false),
            (true, false, true),
        ] {
            let root = json_scratch("guard");
            let path = player_json(&root, "stats", uuid);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"sentinel").unwrap();
            let mut stats = Stats {
                loaded,
                ..Stats::default()
            };
            stats.custom("play_time", 5);
            let before = (
                stats.values.clone(),
                stats.events.clone(),
                stats.dirty.clone(),
            );
            stats
                .save_local_to(&root, Some(uuid), remote, no_player)
                .unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), b"sentinel");
            assert_eq!(stats.loaded, loaded);
            assert_eq!(
                (
                    stats.values.clone(),
                    stats.events.clone(),
                    stats.dirty.clone()
                ),
                before
            );
            std::fs::remove_dir_all(root).unwrap();
        }
        let root = json_scratch("identity");
        let stats = Stats {
            loaded: true,
            ..Stats::default()
        };
        stats.save_local_to(&root, None, false, false).unwrap();
        assert!(std::fs::read_dir(&root).unwrap().next().is_none());
        assert_eq!(
            player_json(&root, "stats", uuid),
            root.join("players/stats/ffffffff-0000-0000-8000-00007fffffff.json")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn counters_events_and_dirty_entries() {
        let mut s = Stats::default();
        s.custom("play_time", 1);
        s.custom("play_time", 1);
        s.add("mined", "minecraft:stone", 2);
        s.custom(&"x".repeat(100), 3);
        assert_eq!(s.get("custom", "minecraft:play_time"), 2);
        assert_eq!(s.get("mined", "minecraft:stone"), 2);
        assert_eq!(
            s.get("custom", &format!("minecraft:{}", "x".repeat(100))),
            3
        );
        assert_eq!(s.events.len(), 4);
        assert_eq!(
            s.events[1],
            (
                "minecraft:custom".into(),
                "minecraft:play_time".into(),
                Some(1)
            )
        );
        let long = format!("minecraft:{}", "x".repeat(100));
        let sent = |v: &[(&str, &str, i32)]| -> Vec<(String, String, i32)> {
            v.iter()
                .map(|&(t, k, n)| (t.to_owned(), k.to_owned(), n))
                .collect()
        };
        assert_eq!(
            s.take_dirty(),
            sent(&[
                ("minecraft:custom", "minecraft:play_time", 2),
                ("minecraft:custom", &long, 3),
                ("minecraft:mined", "minecraft:stone", 2),
            ])
        );
        assert!(s.take_dirty().is_empty());
        s.reset("mined", "minecraft:stone");
        assert_eq!(
            s.take_dirty(),
            sent(&[("minecraft:mined", "minecraft:stone", 0)])
        );
        s.mark_all_dirty();
        assert_eq!(s.take_dirty().len(), 3);
    }
}
