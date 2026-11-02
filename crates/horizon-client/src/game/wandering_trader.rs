//! `WanderingTraderSpawner`: every 1200 ticks the spawn delay counts down;
//! when it runs out a trader may appear near the player (or the nearest
//! bell), bringing two llamas on leads, and wanders off after two days.
use super::{Dim, Game, creatures::Creatures};
use horizon_client::{
    mobs::{Species, leash::Holder, spawning, trader},
    save::{read_dat, write_dat},
};
use horizon_core::{geometry::Vec3, random::RandomSource};
use horizon_world::nbt::Tag;

const TICK_DELAY: i32 = 1200;
const SPAWN_DELAY: i32 = 24000;
const MIN_CHANCE: i32 = 25;
const MAX_CHANCE: i32 = 75;
const ATTEMPTS: i32 = 10;

/// The spawner's clock and the `WanderingTraderData` saved with the world.
pub(super) struct Spawner {
    tick_delay: i32,
    spawn_delay: i32,
    chance: i32,
    loaded: bool,
    dirty: bool,
}
impl Default for Spawner {
    fn default() -> Self {
        Self {
            tick_delay: TICK_DELAY,
            spawn_delay: SPAWN_DELAY,
            chance: MIN_CHANCE,
            loaded: false,
            dirty: false,
        }
    }
}
impl Spawner {
    fn native_data(&self) -> Tag {
        Tag::compound([
            (
                "data",
                Tag::compound([
                    ("spawn_delay", Tag::Int(self.spawn_delay)),
                    ("spawn_chance", Tag::Int(self.chance)),
                ]),
            ),
            ("DataVersion", Tag::Int(horizon_world::DATA_VERSION)),
        ])
    }
    fn save_to(&mut self, path: &std::path::Path) -> horizon_client::Result<()> {
        if !self.loaded || !self.dirty {
            return Ok(());
        }
        write_dat(path, self.native_data())
            .map_err(|error| format!("{}: {error}", path.display()))?;
        self.dirty = false;
        Ok(())
    }
}

#[cfg(feature = "paired-packages")]
fn retained_trader_state(spawner: &Spawner) -> [i32; 5] {
    [
        spawner.tick_delay,
        spawner.spawn_delay,
        spawner.chance,
        i32::from(spawner.loaded),
        i32::from(spawner.dirty),
    ]
}

#[cfg(feature = "paired-packages")]
pub(in crate::game) struct TraderMetadataExpected {
    projection: std::sync::Arc<()>,
    root: std::path::PathBuf,
    state: [i32; 5],
}
#[cfg(feature = "paired-packages")]
#[derive(Clone)]
pub(in crate::game) struct TraderMetadataInput {
    projection: std::sync::Arc<()>,
    path: std::path::PathBuf,
    active: bool,
    data: Tag,
}
#[cfg(feature = "paired-packages")]
pub(in crate::game) struct TraderMetadataPost {
    projection: std::sync::Arc<()>,
    wrote: bool,
}
#[cfg(feature = "paired-packages")]
pub(in crate::game) struct TraderMetadataIo {
    pub effect: super::paired_close::MetadataEffect,
    pub result: std::result::Result<TraderMetadataPost, String>,
}
#[cfg(feature = "paired-packages")]
impl TraderMetadataInput {
    pub(in crate::game) fn write_native(&self) -> TraderMetadataIo {
        use super::paired_close::MetadataEffect as E;
        if !self.active {
            return TraderMetadataIo {
                effect: E::Skipped,
                result: Ok(TraderMetadataPost {
                    projection: self.projection.clone(),
                    wrote: false,
                }),
            };
        }
        match write_dat(&self.path, self.data.clone()) {
            Ok(()) => TraderMetadataIo {
                effect: E::Completed,
                result: Ok(TraderMetadataPost {
                    projection: self.projection.clone(),
                    wrote: true,
                }),
            },
            Err(error) => TraderMetadataIo {
                effect: E::Unknown,
                result: Err(format!("{}: {error}", self.path.display())),
            },
        }
    }
}
#[cfg(feature = "paired-packages")]
impl Game<'_> {
    pub(in crate::game) fn prepare_retained_trader_metadata(
        &self,
    ) -> (TraderMetadataExpected, TraderMetadataInput) {
        let projection = std::sync::Arc::new(());
        let trader = &self.creatures.trader;
        (
            TraderMetadataExpected {
                projection: projection.clone(),
                root: self.world_root.clone(),
                state: retained_trader_state(trader),
            },
            TraderMetadataInput {
                projection,
                path: self.trader_path(),
                active: trader.loaded && trader.dirty,
                data: trader.native_data(),
            },
        )
    }
    pub(in crate::game) fn preflight_retained_trader_metadata(
        &self,
        expected: &TraderMetadataExpected,
        post: &TraderMetadataPost,
    ) -> std::result::Result<(), String> {
        if !std::sync::Arc::ptr_eq(&expected.projection, &post.projection)
            || self.world_root != expected.root
            || retained_trader_state(&self.creatures.trader) != expected.state
            || post.wrote != (expected.state[3] != 0 && expected.state[4] != 0)
        {
            return Err("retained trader original owner state or projection changed".into());
        }
        Ok(())
    }
    pub(in crate::game) fn commit_retained_trader_metadata(&mut self, post: TraderMetadataPost) {
        if post.wrote {
            self.creatures.trader.dirty = false;
        }
    }
}

#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
impl Game<'_> {
    pub(in crate::game) fn set_retained_trader_fixture(&mut self, loaded: bool, dirty: bool) {
        self.creatures.trader = Spawner {
            tick_delay: 17,
            spawn_delay: 12345,
            chance: 63,
            loaded,
            dirty,
        };
    }
    pub(in crate::game) fn retained_trader_fixture_state(&self) -> [i32; 5] {
        retained_trader_state(&self.creatures.trader)
    }
}
impl Creatures {
    /// A fresh spawner for a newly opened world.
    pub(super) fn reset_trader(&mut self) {
        self.trader = Spawner::default();
    }
}
impl Game<'_> {
    fn trader_path(&self) -> std::path::PathBuf {
        self.world_root
            .join("data")
            .join("minecraft")
            .join("wandering_trader.dat")
    }
    fn load_trader_data(&mut self) {
        if self.creatures.trader.loaded {
            return;
        }
        let tag = read_dat(&self.trader_path());
        let data = tag.as_ref().and_then(|t| t.get("data"));
        let get = |k: &str, d: i32| match data.and_then(|d| d.get(k)) {
            Some(Tag::Int(v)) => *v,
            _ => d,
        };
        let t = &mut self.creatures.trader;
        t.spawn_delay = get("spawn_delay", SPAWN_DELAY);
        t.chance = get("spawn_chance", MIN_CHANCE);
        t.loaded = true;
    }
    pub(super) fn save_wandering_trader(&mut self) -> horizon_client::Result<()> {
        let path = self.trader_path();
        self.creatures.trader.save_to(&path)
    }
    /// `WanderingTraderSpawner.tick`.
    pub(super) fn tick_wandering_trader(&mut self) {
        if self.dim != Dim::Overworld || !self.ui.rules.bool("spawn_wandering_traders") {
            return;
        }
        self.load_trader_data();
        self.creatures.trader.tick_delay -= 1;
        if self.creatures.trader.tick_delay > 0 {
            return;
        }
        self.creatures.trader.tick_delay = TICK_DELAY;
        let t = &mut self.creatures.trader;
        t.spawn_delay -= TICK_DELAY;
        t.dirty = true;
        if t.spawn_delay > 0 {
            return;
        }
        t.spawn_delay = SPAWN_DELAY;
        let chance = t.chance;
        t.chance = (chance + 25).clamp(MIN_CHANCE, MAX_CHANCE);
        if self.world.random.next_int_bound(100).unwrap_or(100) <= chance && self.spawn_trader() {
            self.creatures.trader.chance = MIN_CHANCE;
        }
    }
    /// `WanderingTraderSpawner.spawn`.
    fn spawn_trader(&mut self) -> bool {
        // ServerLevel.getRandomPlayer: one of the living players, drawn
        // from the level random.
        let alive: Vec<Vec3> = self
            .level_players()
            .into_iter()
            .filter(|p| p.alive)
            .map(|p| p.position)
            .collect();
        if alive.is_empty() {
            return true;
        }
        let pick = self
            .world
            .random
            .next_int_bound(alive.len() as i32)
            .unwrap_or(0);
        let p = alive[pick as usize];
        if self.world.random.next_int_bound(10).unwrap_or(1) != 0 {
            return false;
        }
        let player_pos = [p.x.floor() as i32, p.y.floor() as i32, p.z.floor() as i32];
        let reference = self.nearest_bell(player_pos, 48).unwrap_or(player_pos);
        let Some(pos) = self.find_trader_spawn_near(reference, 48) else {
            return false;
        };
        if !self.trader_has_space(pos) {
            return false;
        }
        let biome = self.world.chunks.get(&(pos[0] >> 4, pos[2] >> 4)).map(|c| {
            self.res
                .biomes()
                .get(c.biome(pos[0], pos[1], pos[2]))
                .name
                .clone()
        });
        if biome.is_some_and(|b| {
            self.res
                .mob_data()
                .biome_has("minecraft:without_wandering_trader_spawns", &b)
        }) {
            return false;
        }
        let at = [
            f64::from(pos[0]) + 0.5,
            f64::from(pos[1]),
            f64::from(pos[2]) + 0.5,
        ];
        let Some(id) = self.spawn_mob("minecraft:wandering_trader", at, true) else {
            return false;
        };
        for _ in 0..2 {
            self.spawn_trader_llama(id, 4);
        }
        if let Some(m) = self.mobs.iter_mut().find(|m| m.id == id) {
            m.extra.ints[5] = trader::DESPAWN_DELAY;
            m.extra.point = Some([
                f64::from(reference[0]),
                f64::from(reference[1]),
                f64::from(reference[2]),
            ]);
            m.extra.home = Some(reference);
            m.extra.ints[4] = 16;
        }
        true
    }
    /// `tryToSpawnLlamaFor`: a trader llama near the trader, on its lead.
    fn spawn_trader_llama(&mut self, trader: u32, radius: i32) {
        let Some(at) = self.mobs.iter().find(|m| m.id == trader).map(|m| {
            [
                m.position.x.floor() as i32,
                m.position.y.floor() as i32,
                m.position.z.floor() as i32,
            ]
        }) else {
            return;
        };
        let Some(pos) = self.find_trader_spawn_near(at, radius) else {
            return;
        };
        let spot = [
            f64::from(pos[0]) + 0.5,
            f64::from(pos[1]),
            f64::from(pos[2]) + 0.5,
        ];
        if let Some(id) = self.spawn_mob("minecraft:trader_llama", spot, false)
            && let Some(l) = self.mobs.iter_mut().find(|m| m.id == id)
        {
            // EntitySpawnReason.EVENT: grown up.
            l.extra.age = 0;
            l.extra.leash = Some(Holder::Mob(trader));
        }
    }
    /// `findSpawnPositionNear`: ten tries at a column near the reference
    /// where the trader could stand.
    fn find_trader_spawn_near(&mut self, reference: [i32; 3], radius: i32) -> Option<[i32; 3]> {
        for _ in 0..ATTEMPTS {
            let x =
                reference[0] + self.world.random.next_int_bound(radius * 2).unwrap_or(0) - radius;
            let z =
                reference[2] + self.world.random.next_int_bound(radius * 2).unwrap_or(0) - radius;
            if !self.world.loaded(x, z) {
                continue;
            }
            let y = self.motion_top(x, z);
            let w = self.mob_world_base(&[]);
            if spawning::placement_ok(Species::WanderingTrader, &w, x, y, z) {
                return Some([x, y, z]);
            }
        }
        None
    }
    /// `hasEnoughSpace`: no collision in the 2 by 3 by 2 box.
    fn trader_has_space(&self, pos: [i32; 3]) -> bool {
        (0..=1).all(|dx| {
            (0..=2).all(|dy| {
                (0..=1).all(|dz| {
                    !self
                        .res
                        .facts()
                        .get(self.world.block(pos[0] + dx, pos[1] + dy, pos[2] + dz))
                        .full_collision
                })
            })
        })
    }
    /// The closest bell (`PoiTypes.MEETING`) within `range` blocks.
    fn nearest_bell(&self, from: [i32; 3], range: i32) -> Option<[i32; 3]> {
        let mut best: Option<([i32; 3], i32)> = None;
        for x in from[0] - range..=from[0] + range {
            for z in from[2] - range..=from[2] + range {
                if !self.world.loaded(x, z) {
                    continue;
                }
                for y in (from[1] - range).max(horizon_client::chunk::MIN_Y)..=from[1] + range {
                    if &*self.res.facts().get(self.world.block(x, y, z)).name != "minecraft:bell" {
                        continue;
                    }
                    let d = (x - from[0]).pow(2) + (y - from[1]).pow(2) + (z - from[2]).pow(2);
                    if best.is_none_or(|b| d < b.1) {
                        best = Some(([x, y, z], d));
                    }
                }
            }
        }
        best.map(|b| b.0)
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "horizon-trader-save-{name}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }
    fn state(spawner: &Spawner) -> (i32, i32, i32, bool, bool) {
        (
            spawner.tick_delay,
            spawner.spawn_delay,
            spawner.chance,
            spawner.loaded,
            spawner.dirty,
        )
    }
    fn populated() -> Spawner {
        Spawner {
            tick_delay: 17,
            spawn_delay: 12000,
            chance: 50,
            loaded: true,
            dirty: true,
        }
    }

    #[test]
    fn failed_trader_save_keeps_dirty_and_timers_for_exact_retry() {
        let root = scratch("retry");
        let path = root.join("wandering_trader.dat");
        let blocked = root.join("wandering_trader.dat_new");
        let mut spawner = populated();
        let before = state(&spawner);
        std::fs::create_dir(&blocked).unwrap();
        assert!(
            spawner
                .save_to(&path)
                .unwrap_err()
                .to_string()
                .contains("wandering_trader.dat")
        );
        assert_eq!(state(&spawner), before);
        assert!(!path.exists());
        std::fs::remove_dir(blocked).unwrap();
        spawner.save_to(&path).unwrap();
        assert_eq!(state(&spawner), (17, 12000, 50, true, false));
        assert_eq!(
            read_dat(&path).unwrap(),
            Tag::compound([
                (
                    "data",
                    Tag::compound([
                        ("spawn_delay", Tag::Int(12000)),
                        ("spawn_chance", Tag::Int(50))
                    ])
                ),
                ("DataVersion", Tag::Int(horizon_world::DATA_VERSION)),
            ])
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unloaded_or_clean_trader_save_writes_nothing_and_keeps_all_state() {
        for (loaded, dirty) in [(false, true), (true, false), (false, false)] {
            let root = scratch("guard");
            let mut spawner = populated();
            spawner.loaded = loaded;
            spawner.dirty = dirty;
            let before = state(&spawner);
            spawner
                .save_to(&root.join("data/minecraft/wandering_trader.dat"))
                .unwrap();
            assert!(std::fs::read_dir(&root).unwrap().next().is_none());
            assert_eq!(state(&spawner), before);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}
