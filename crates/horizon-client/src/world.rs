//! Resident client world: streamed chunk columns plus the bookkeeping the game
//! loop needs to relight and remesh after edits.
use crate::chunk::{Chunk, MIN_Y, SECTIONS};
use horizon_core::random::LegacyRandom;
use horizon_world::registry::BlockStateId;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

/// Sections (x, section index, z) waiting to be remeshed, kept by column
/// (a bit per section index), so the mesh scheduler looks at each column once
/// rather than at every section of it.
#[derive(Clone, Debug, Default)]
pub struct DirtySections {
    columns: crate::fast_hash::FxHashMap<(i32, i32), u32>,
}
const _: () = assert!(SECTIONS <= 32);
impl DirtySections {
    pub fn insert(&mut self, (x, sy, z): (i32, i32, i32)) {
        debug_assert!((0..SECTIONS as i32).contains(&sy));
        *self.columns.entry((x, z)).or_default() |= 1 << sy;
    }
    pub fn contains(&self, &(x, sy, z): &(i32, i32, i32)) -> bool {
        (0..SECTIONS as i32).contains(&sy)
            && self.columns.get(&(x, z)).is_some_and(|m| m & 1 << sy != 0)
    }
    /// The dirty section indexes of column (x, z), as bits, removed.
    pub fn take_column(&mut self, x: i32, z: i32) -> u32 {
        self.columns.remove(&(x, z)).unwrap_or(0)
    }
    /// Keep the columns `keep` accepts.
    pub fn retain_columns(&mut self, mut keep: impl FnMut(i32, i32) -> bool) {
        self.columns.retain(|&(x, z), _| keep(x, z));
    }
    /// The columns holding dirty sections, with their section bits.
    pub fn columns(&self) -> impl Iterator<Item = ((i32, i32), u32)> + '_ {
        self.columns.iter().map(|(&c, &m)| (c, m))
    }
    pub fn clear(&mut self) {
        self.columns.clear();
    }
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }
    /// How many sections are dirty.
    pub fn len(&self) -> usize {
        self.columns.values().map(|m| m.count_ones() as usize).sum()
    }
}
pub struct World {
    pub fast_lava: bool,
    pub chunks: crate::chunk_map::ChunkMap,
    pub random: LegacyRandom,
    pub revision: u64,
    pub health_rules: HealthRules,
    pub air: BlockStateId,
    /// Sections whose geometry changed and need remeshing.
    pub dirty_sections: DirtySections,
    /// Chunks whose light must be recomputed.
    pub dirty_light: crate::fast_hash::FxHashSet<(i32, i32)>,
    /// Edits on lit chunks (position, old state, new state), relit
    /// incrementally by streaming.
    pub light_edits: Vec<((i32, i32, i32), u32, u32)>,
    /// Saved entities of chunks that just loaded, spawned by the game.
    pub loaded_entities: Vec<((i32, i32), Vec<horizon_world::nbt::Tag>)>,
    /// Chunks changed since the last save.
    pub modified: HashSet<(i32, i32)>,
    /// Block entities, and the order they tick in.
    pub block_entities: crate::block_entities::BlockEntities,
    /// Each chest's `ContainerOpenersCounter`: opener count and largest
    /// interaction range.
    pub chest_openers: HashMap<(i32, i32, i32), (i32, f64)>,
    /// `BaseContainerBlockEntity.name`: custom names of container block entities.
    pub block_names: HashMap<(i32, i32, i32), String>,
    /// States (by id) whose writes are recorded in `watched_writes` (sculk
    /// blocks, which need a block entity).
    pub watched: Vec<bool>,
    /// Writes of watched states: position, previous and new state.
    pub watched_writes: Vec<((i32, i32, i32), u32, u32)>,
    /// Separate passive host observation; never enabled for a default world.
    #[cfg(feature = "paired-packages")]
    mutation_observer: Option<Box<crate::mutation_observer::Observer>>,
    #[cfg(feature = "paired-packages")]
    residency_observer: Option<Box<crate::residency_observer::Observer>>,
    #[cfg(feature = "paired-packages")]
    counter_trial: Option<Box<crate::counter_trial::Trial>>,
    #[cfg(feature = "paired-packages")]
    pub(crate) world_slot_trial: Option<Box<crate::world_slot_trial::Trial>>,
    /// `ChunkAccess.inhabitedTime` of resident chunks: ticks a player spent
    /// within spawning range (local difficulty).
    pub inhabited: HashMap<(i32, i32), i64>,
    pub game_time: i64,
    pub day_time: i64,
    pub weather: Weather,
    /// Every block position written since the log was last drained, kept
    /// only while the world is open to the network (see `lan`).
    pub block_log: Option<Vec<(i32, i32, i32)>>,
    /// The sections whose light changed since the log was last drained, as
    /// a bit per section of each column, kept while `block_log` is
    /// (`ChunkHolder.sectionLightChanged`).
    pub light_log: Option<crate::fast_hash::FxHashMap<(i32, i32), u32>>,
    /// The villager POIs of each section (`PoiManager`'s sections).
    pub pois: crate::poi_sections::PoiSections<crate::poi_sections::Poi>,
    /// Moving-piston states removed during the last tick; retained until the
    /// final block mesh is visible.
    pub moving_piston_fallback: Vec<([i32; 3], horizon_world::piston_shape::MovingPistonState)>,
    /// The moving-piston block entities of the level, by position, copied
    /// from the block ticks after each tick: collision and rendering read them.
    pub moving_pistons: Vec<([i32; 3], horizon_world::piston_shape::MovingPistonState)>,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Weather {
    pub clear_time: i32,
    pub rain_time: i32,
    pub thunder_time: i32,
    pub raining: bool,
    pub thundering: bool,
    pub rain_level: (f32, f32),
    pub thunder_level: (f32, f32),
}
impl Weather {
    /// The timers of `advanceWeatherCycle`, which run only while
    /// `advance_weather` is on.
    pub fn tick_timers(&mut self, random: &mut horizon_core::random::LegacyRandom) {
        use horizon_core::random::RandomSource;
        let mut uniform =
            |min: i32, max: i32| min + random.next_int_bound(max - min + 1).unwrap_or(0);
        if self.clear_time > 0 {
            self.clear_time -= 1;
            self.thunder_time = if self.thundering { 0 } else { 1 };
            self.rain_time = if self.raining { 0 } else { 1 };
            self.thundering = false;
            self.raining = false;
        } else {
            if self.thunder_time > 0 {
                self.thunder_time -= 1;
                if self.thunder_time == 0 {
                    self.thundering = !self.thundering;
                }
            } else if self.thundering {
                self.thunder_time = uniform(3600, 15600);
            } else {
                self.thunder_time = uniform(12000, 180000);
            }
            if self.rain_time > 0 {
                self.rain_time -= 1;
                if self.rain_time == 0 {
                    self.raining = !self.raining;
                }
            } else if self.raining {
                self.rain_time = uniform(12000, 24000);
            } else {
                self.rain_time = uniform(12000, 180000);
            }
        }
    }
    /// The rain and thunder level fade at the end of `advanceWeatherCycle`,
    /// which runs whatever `advance_weather` says, so `/weather` still shows.
    pub fn tick_levels(&mut self) {
        self.thunder_level.0 = self.thunder_level.1;
        self.thunder_level.1 =
            (self.thunder_level.1 + if self.thundering { 0.01 } else { -0.01 }).clamp(0.0, 1.0);
        self.rain_level.0 = self.rain_level.1;
        self.rain_level.1 =
            (self.rain_level.1 + if self.raining { 0.01 } else { -0.01 }).clamp(0.0, 1.0);
    }
    /// Level.getRainLevel / getThunderLevel (thunder scales with rain).
    pub fn levels(&self, partial: f32) -> (f32, f32) {
        let rain = self.rain_level.0 + (self.rain_level.1 - self.rain_level.0) * partial;
        let thunder =
            self.thunder_level.0 + (self.thunder_level.1 - self.thunder_level.0) * partial;
        (rain, thunder * rain)
    }
}
impl World {
    pub fn new(air: BlockStateId, seed: i64) -> Self {
        Self {
            chunks: Default::default(),
            random: LegacyRandom::new(seed),
            revision: 0,
            health_rules: HealthRules::default(),
            air,
            fast_lava: false,
            dirty_sections: Default::default(),
            dirty_light: Default::default(),
            light_edits: Vec::new(),
            loaded_entities: Vec::new(),
            modified: HashSet::new(),
            block_entities: Default::default(),
            chest_openers: HashMap::new(),
            block_names: HashMap::new(),
            watched: Vec::new(),
            watched_writes: Vec::new(),
            #[cfg(feature = "paired-packages")]
            mutation_observer: None,
            #[cfg(feature = "paired-packages")]
            residency_observer: None,
            #[cfg(feature = "paired-packages")]
            counter_trial: None,
            #[cfg(feature = "paired-packages")]
            world_slot_trial: None,
            inhabited: HashMap::new(),
            game_time: 0,
            day_time: 1000,
            weather: Weather::default(),
            block_log: None,
            light_log: None,
            moving_piston_fallback: Vec::new(),
            moving_pistons: Vec::new(),
            pois: Default::default(),
        }
    }
    /// Opt in after the host establishes authority, dimension and registry IDs.
    /// This does not infer authority from a World or expose it to a guest.
    #[cfg(feature = "paired-packages")]
    pub fn enable_mutation_observer(
        &mut self,
        scope: crate::mutation_observer::AuthorityScope,
        watched: impl IntoIterator<Item = BlockStateId>,
        capacity: usize,
    ) -> Result<(), crate::mutation_observer::ConfigError> {
        if self.mutation_observer.is_some() {
            return Err(crate::mutation_observer::ConfigError::AlreadyEnabled);
        }
        self.mutation_observer = Some(Box::new(crate::mutation_observer::Observer::new(
            scope, watched, capacity,
        )?));
        Ok(())
    }
    #[cfg(feature = "paired-packages")]
    pub fn mutation_observer(&self) -> Option<&crate::mutation_observer::Observer> {
        self.mutation_observer.as_deref()
    }
    /// Disabled is distinct from an enabled empty batch or a sticky fault.
    #[cfg(feature = "paired-packages")]
    pub fn drain_mutations(
        &mut self,
    ) -> Option<
        Result<Vec<crate::mutation_observer::Mutation>, crate::mutation_observer::ObserverFault>,
    > {
        self.mutation_observer
            .as_mut()
            .map(|observer| observer.drain())
    }
    /// Explicit teardown returns the observer, including its pending/fault state.
    #[cfg(feature = "paired-packages")]
    pub fn take_mutation_observer(&mut self) -> Option<crate::mutation_observer::Observer> {
        self.mutation_observer.take().map(|observer| *observer)
    }
    /// Host opt-in, after establishing authoritative scope. This reports current
    /// membership separately and does not load chunks or fabricate load history.
    #[cfg(feature = "paired-packages")]
    pub fn enable_residency_observer(
        &mut self,
        scope: crate::mutation_observer::AuthorityScope,
        watched: impl IntoIterator<Item = (i32, i32)>,
        capacity: usize,
    ) -> Result<crate::residency_observer::Snapshot, crate::residency_observer::ConfigError> {
        if self.residency_observer.is_some() {
            return Err(crate::residency_observer::ConfigError::AlreadyEnabled);
        }
        let observer = crate::residency_observer::Observer::new(scope, watched, capacity)?;
        let snapshot = observer.snapshot(self.game_time, |c| self.chunks.contains_key(&c));
        self.residency_observer = Some(Box::new(observer));
        Ok(snapshot)
    }
    #[cfg(feature = "paired-packages")]
    pub fn residency_observer(&self) -> Option<&crate::residency_observer::Observer> {
        self.residency_observer.as_deref()
    }
    /// Current watched membership, without draining or fabricating history.
    /// A queue fault does not make this a replacement for the lost history.
    #[cfg(feature = "paired-packages")]
    pub fn residency_snapshot(&self) -> Option<crate::residency_observer::Snapshot> {
        self.residency_observer
            .as_ref()
            .map(|o| o.snapshot(self.game_time, |c| self.chunks.contains_key(&c)))
    }
    #[cfg(feature = "paired-packages")]
    pub fn drain_residency(
        &mut self,
    ) -> Option<Result<Vec<crate::residency_observer::Event>, crate::residency_observer::Fault>>
    {
        self.residency_observer.as_mut().map(|o| o.drain())
    }
    /// Explicit teardown retains pending history/faults in the returned observer.
    #[cfg(feature = "paired-packages")]
    pub fn take_residency_observer(&mut self) -> Option<crate::residency_observer::Observer> {
        self.residency_observer.take().map(|o| *o)
    }
    #[cfg(feature = "paired-packages")]
    pub fn enable_counter_trial(
        &mut self,
        configuration: crate::counter_trial::Configuration,
    ) -> Result<crate::counter_trial::Snapshot, crate::counter_trial::ConfigError> {
        use crate::counter_trial::{ConfigError, Trial};
        if self.counter_trial.is_some() {
            return Err(ConfigError::AlreadyEnabled);
        }
        if configuration.state_id() == self.air {
            return Err(ConfigError::InvalidMetadata);
        }
        let [x, y, z] = configuration.position();
        let current = self
            .chunks
            .get(&configuration.chunk())
            .map(|c| c.block(x & 15, y, z & 15))
            .ok_or(ConfigError::NotResident)?;
        let trial = Trial::new(configuration, Some(current));
        let snapshot = trial.snapshot();
        self.counter_trial = Some(Box::new(trial));
        Ok(snapshot)
    }
    #[cfg(feature = "paired-packages")]
    pub fn counter_trial_snapshot(&self) -> Option<crate::counter_trial::Snapshot> {
        self.counter_trial.as_ref().map(|t| t.snapshot())
    }
    #[cfg(feature = "paired-packages")]
    pub fn read_counter_trial(
        &self,
        scope: &crate::mutation_observer::AuthorityScope,
        instance: u64,
    ) -> Result<crate::counter_trial::Record, crate::counter_trial::ReadError> {
        self.counter_trial
            .as_ref()
            .ok_or(crate::counter_trial::ReadError::Disabled)?
            .read(scope, instance)
    }
    /// The authoritative phase supplies running and current block-tick eligibility;
    /// the trial still rechecks resident storage/state, not unloaded-as-air.
    #[cfg(feature = "paired-packages")]
    pub fn advance_counter_trial(
        &mut self,
        running: bool,
        eligible: bool,
    ) -> Option<Result<bool, crate::counter_trial::Fault>> {
        let trial = self.counter_trial.as_mut()?;
        let configuration = trial.configuration();
        if !matches!(
            configuration.backend(),
            crate::counter_trial::Backend::Native
        ) {
            return Some(Err(crate::counter_trial::Fault::DispatchMismatch));
        }
        let [x, y, z] = configuration.position();
        let current = self
            .chunks
            .get(&configuration.chunk())
            .map(|c| c.block(x & 15, y, z & 15));
        Some(trial.advance(running, eligible, current, self.game_time))
    }
    #[cfg(feature = "paired-packages")]
    fn guest_counter_current(&self) -> Option<BlockStateId> {
        let configuration = self.counter_trial.as_ref()?.configuration();
        let [x, y, z] = configuration.position();
        self.chunks
            .get(&configuration.chunk())
            .map(|c| c.block(x & 15, y, z & 15))
    }
    /// The caller supplies current authoritative running/ticket eligibility.
    #[cfg(feature = "paired-packages")]
    pub fn prepare_guest_counter_phase(
        &mut self,
        running: bool,
        eligible: bool,
    ) -> Result<Option<crate::counter_trial::PhaseToken>, crate::counter_trial::PhaseError> {
        let current = self.guest_counter_current();
        match &mut self.counter_trial {
            Some(trial) => trial.prepare_guest(running, eligible, current, self.game_time),
            None => Ok(None),
        }
    }
    #[cfg(feature = "paired-packages")]
    fn complete_guest_counter_phase(
        &mut self,
        token: crate::counter_trial::PhaseToken,
        result: Result<crate::counter_trial::Proposal, crate::counter_trial::GuestFailure>,
        running: bool,
        eligible: bool,
    ) -> Result<crate::counter_trial::Completion, crate::counter_trial::PhaseError> {
        let current = self.guest_counter_current();
        match &mut self.counter_trial {
            Some(trial) => {
                trial.complete_guest(token, result, running, eligible, current, self.game_time)
            }
            None => Ok(crate::counter_trial::Completion::Stale),
        }
    }
    /// Rechecks the token and fresh authority/storage before publishing both fields.
    #[cfg(feature = "paired-packages")]
    pub fn commit_guest_counter_phase(
        &mut self,
        token: crate::counter_trial::PhaseToken,
        proposal: crate::counter_trial::Proposal,
        running: bool,
        eligible: bool,
    ) -> Result<crate::counter_trial::Completion, crate::counter_trial::PhaseError> {
        self.complete_guest_counter_phase(token, Ok(proposal), running, eligible)
    }
    /// A stale failure cannot fault a different/replaced trial.
    #[cfg(feature = "paired-packages")]
    pub fn fail_guest_counter_phase(
        &mut self,
        token: crate::counter_trial::PhaseToken,
        failure: crate::counter_trial::GuestFailure,
        running: bool,
        eligible: bool,
    ) -> Result<crate::counter_trial::Completion, crate::counter_trial::PhaseError> {
        self.complete_guest_counter_phase(token, Err(failure), running, eligible)
    }
    /// Explicit lifetime retirement before the owning World is discarded.
    #[cfg(feature = "paired-packages")]
    pub fn take_counter_trial(&mut self) -> Option<crate::counter_trial::Snapshot> {
        self.counter_trial.take().map(|mut t| {
            t.retire(
                crate::counter_trial::RetirementReason::Explicit,
                self.game_time,
            );
            t.snapshot()
        })
    }
    /// Ordinary authoritative streaming admission, distinct from replica writes
    /// and temporary lending of the map to the SDK.
    pub(crate) fn insert_streamed_chunk(&mut self, chunk: Chunk) -> Option<Arc<Chunk>> {
        let key = (chunk.x, chunk.z);
        #[cfg(feature = "paired-packages")]
        let counter_state = self
            .counter_trial
            .as_ref()
            .filter(|t| t.configuration().chunk() == key)
            .map(|t| {
                let [x, y, z] = t.configuration().position();
                chunk.block(x & 15, y, z & 15)
            });
        #[cfg(feature = "paired-packages")]
        let slot_state = self
            .world_slot_trial
            .as_ref()
            .filter(|t| t.watches_chunk(key))
            .map(|t| {
                let [x, y, z] = t.position();
                chunk.block(x & 15, y, z & 15)
            });
        let previous = self.chunks.insert(key, Arc::new(chunk));
        #[cfg(feature = "paired-packages")]
        if let (Some(trial), Some(state)) = (&mut self.world_slot_trial, slot_state) {
            trial.inserted(previous.is_some(), state, self.game_time);
        }
        #[cfg(feature = "paired-packages")]
        if let (Some(trial), Some(state)) = (&mut self.counter_trial, counter_state) {
            trial.inserted(previous.is_some(), state, self.game_time);
        }
        #[cfg(feature = "paired-packages")]
        if let Some(observer) = &mut self.residency_observer {
            use crate::residency_observer::Kind;
            observer.record(
                key,
                if previous.is_some() {
                    Kind::Replaced
                } else {
                    Kind::Loaded
                },
                self.game_time,
            );
        }
        previous
    }
    pub(crate) fn remove_streamed_chunk(&mut self, key: &(i32, i32)) -> Option<Arc<Chunk>> {
        let previous = self.chunks.remove(key);
        #[cfg(feature = "paired-packages")]
        if previous.is_some()
            && let Some(trial) = self
                .world_slot_trial
                .as_mut()
                .filter(|t| t.watches_chunk(*key))
        {
            trial.unloaded();
        }
        #[cfg(feature = "paired-packages")]
        if previous.is_some()
            && let Some(trial) = self
                .counter_trial
                .as_mut()
                .filter(|t| t.configuration().chunk() == *key)
        {
            trial.unloaded();
        }
        #[cfg(feature = "paired-packages")]
        if previous.is_some()
            && let Some(observer) = &mut self.residency_observer
        {
            observer.record(
                *key,
                crate::residency_observer::Kind::Unloaded,
                self.game_time,
            );
        }
        previous
    }
    /// A controlled blockstate scene for the native scene probes: material
    /// layers, a stepped pyramid and the model gallery. Not generated terrain.
    pub fn test_scene(
        assets: &crate::assets::Assets,
        render: &crate::assets::RenderAssets,
        radius: i32,
    ) -> crate::Result<Self> {
        let air = assets
            .registry
            .default_state("minecraft:air")
            .ok_or("missing air state")?;
        let mut world = Self::new(air, 0);
        for cz in -radius..=radius {
            for cx in -radius..=radius {
                world
                    .chunks
                    .insert((cx, cz), Arc::new(Chunk::empty(cx, cz)));
                for z in 0..16 {
                    for x in 0..16 {
                        for y in 0..4 {
                            let layer = match y {
                                0 => 0,
                                1 | 2 => 1,
                                _ if (cx + cz) & 1 == 0 => 2,
                                _ => 3,
                            };
                            world.set(
                                cx * 16 + x,
                                y,
                                cz * 16 + z,
                                assets.material_states[layer],
                            )?;
                        }
                    }
                }
            }
        }
        for y in 4..11 {
            for z in -10_i32..=10 {
                for x in -10_i32..=10 {
                    if x.abs().max(z.abs()) <= 10 - (y - 4) {
                        world.set(
                            x,
                            y,
                            z,
                            assets.material_states[if y & 1 == 0 { 4 } else { 5 }],
                        )?;
                    }
                }
            }
        }
        for (position, state) in &render.model_placements {
            world.set(position.x, position.y, position.z, *state)?;
        }
        world.revision = 0;
        Ok(world)
    }
    /// Resident storage write; marks the touched sections (and neighbours on
    /// section borders) for remeshing and the surrounding chunks for relighting.
    /// Neighbour updates, drops and block entities belong to the gameplay layer.
    pub fn set(&mut self, x: i32, y: i32, z: i32, state: BlockStateId) -> crate::Result<()> {
        let Some(chunk) = self.chunks.get_mut(&(x >> 4, z >> 4)) else {
            return Err("write outside resident chunks".into());
        };
        if y < MIN_Y || y >= MIN_Y + (SECTIONS as i32) * 16 {
            return Err("write outside world height".into());
        }
        let previous = Arc::make_mut(chunk).set(x & 15, y, z & 15, state.0);
        if previous != state.0 {
            #[cfg(feature = "paired-packages")]
            if let Some(trial) = &mut self.world_slot_trial {
                trial.changed([x, y, z], state, self.game_time);
            }
            #[cfg(feature = "paired-packages")]
            if let Some(trial) = &mut self.counter_trial {
                trial.changed([x, y, z], state, self.game_time);
            }
            self.block_entities
                .state_changed((x, y, z), previous, state.0);
            let watches = |id: u32| self.watched.get(id as usize).copied().unwrap_or(false);
            if watches(previous) || watches(state.0) {
                self.watched_writes.push(((x, y, z), previous, state.0));
            }
            #[cfg(feature = "paired-packages")]
            if let Some(observer) = &mut self.mutation_observer {
                observer.record([x, y, z], BlockStateId(previous), state, self.game_time);
            }
            self.revision = self.revision.wrapping_add(1);
            if let Some(log) = &mut self.block_log {
                log.push((x, y, z));
            }
            self.modified.insert((x >> 4, z >> 4));
            let sy = (y - MIN_Y) >> 4;
            for dz in -1..=1 {
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        let (bx, by, bz) = (x + dx, y + dy, z + dz);
                        let s = (by - MIN_Y) >> 4;
                        if !(0..SECTIONS as i32).contains(&s) {
                            continue;
                        }
                        self.dirty_sections.insert((bx >> 4, s, bz >> 4));
                    }
                }
            }
            let _ = sy;
            if self.chunks.get(&(x >> 4, z >> 4)).is_some_and(|c| c.lit) {
                self.light_edits.push(((x, y, z), previous, state.0));
            } else {
                for dz in -1..=1 {
                    for dx in -1..=1 {
                        self.dirty_light.insert(((x >> 4) + dx, (z >> 4) + dz));
                    }
                }
            }
        }
        Ok(())
    }
    /// `getHeight(Heightmap.Types.WORLD_SURFACE, x, z)`: one above the highest
    /// block that isn't `air`, or `MIN_Y` (also where the chunk isn't loaded).
    pub fn world_surface(&self, x: i32, z: i32, air: impl Fn(BlockStateId) -> bool) -> i32 {
        self.chunks.get(&(x >> 4, z >> 4)).map_or(MIN_Y, |chunk| {
            chunk.world_surface(x, z, |v| air(BlockStateId(v)))
        })
    }
    /// `getHeight(Heightmap.Types.MOTION_BLOCKING, x, z)`: one above the
    /// highest block where `blocks` (motion blocking or holding a fluid),
    /// or `MIN_Y` (also where the chunk isn't loaded).
    pub fn motion_blocking(&self, x: i32, z: i32, blocks: impl Fn(BlockStateId) -> bool) -> i32 {
        self.chunks.get(&(x >> 4, z >> 4)).map_or(MIN_Y, |chunk| {
            chunk.motion_blocking(x & 15, z & 15, |v| blocks(BlockStateId(v)))
        })
    }
    pub fn block(&self, x: i32, y: i32, z: i32) -> BlockStateId {
        self.chunks
            .get(&(x >> 4, z >> 4))
            .map_or(self.air, |chunk| chunk.block(x & 15, y, z & 15))
    }
    /// Whether a block of the box `lo..=hi` may match, from the palettes of
    /// the sections it reaches (a palette of the whole registry may hold
    /// anything): `false` means [`Self::block`] matches in none of its cells,
    /// so a scan over them can stop before reading one.
    pub fn may_hold(
        &self,
        lo: [i32; 3],
        hi: [i32; 3],
        mut matches: impl FnMut(BlockStateId) -> bool,
    ) -> bool {
        let top = MIN_Y + SECTIONS as i32 * 16 - 1;
        // Heights outside the sections read as state 0.
        if (lo[1] < MIN_Y || hi[1] > top) && matches(BlockStateId(0)) {
            return true;
        }
        let sections = (lo[1].max(MIN_Y) - MIN_Y) >> 4..=(hi[1].min(top) - MIN_Y) >> 4;
        for cx in lo[0] >> 4..=hi[0] >> 4 {
            for cz in lo[2] >> 4..=hi[2] >> 4 {
                let Some(chunk) = self.chunks.get(&(cx, cz)) else {
                    if matches(self.air) {
                        return true;
                    }
                    continue;
                };
                for s in sections.clone() {
                    let blocks = &chunk.sections[s as usize].blocks;
                    if blocks.is_global() || blocks.maybe_has(|v| matches(BlockStateId(v))) {
                        return true;
                    }
                }
            }
        }
        false
    }
    /// The block entities to tick this tick, in order (`BlockEntities::tickers`).
    pub fn block_entity_tickers(&mut self) -> Vec<(crate::block_entities::Pos, u32)> {
        let (chunks, air) = (&self.chunks, self.air);
        self.block_entities.tickers(|(x, y, z)| {
            chunks
                .get(&(x >> 4, z >> 4))
                .map_or(air, |chunk| chunk.block(x & 15, y, z & 15))
                .0
        })
    }
    pub fn light(&self, x: i32, y: i32, z: i32) -> u8 {
        self.chunks
            .get(&(x >> 4, z >> 4))
            .map_or(0xF0, |chunk| chunk.light(x & 15, y, z & 15))
    }
    pub fn is_air(&self, state: BlockStateId) -> bool {
        state == self.air
    }
    pub fn loaded(&self, x: i32, z: i32) -> bool {
        self.chunks.contains_key(&(x >> 4, z >> 4))
    }
}

/// The game rules and difficulty a player's health reads (`Player.isInvulnerableTo`,
/// natural regeneration), kept with the world they belong to.
#[derive(Clone, Copy, Debug)]
pub struct HealthRules {
    pub difficulty: horizon_world::food::Difficulty,
    pub natural_regeneration: bool,
    /// The `drowning_damage`, `fall_damage`, `fire_damage` and
    /// `freeze_damage` rules `Player.isInvulnerableTo` reads.
    pub drowning_damage: bool,
    pub fall_damage: bool,
    pub fire_damage: bool,
    pub freeze_damage: bool,
}
impl Default for HealthRules {
    fn default() -> Self {
        Self {
            difficulty: horizon_world::food::Difficulty::Normal,
            natural_regeneration: true,
            drowning_damage: true,
            fall_damage: true,
            fire_damage: true,
            freeze_damage: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::World;
    use crate::chunk::Chunk;
    use horizon_world::registry::BlockStateId;
    use std::sync::Arc;

    /// A box misses a state only where no section it reaches lists it, and
    /// unloaded columns or heights past the world count as their air.
    #[test]
    fn may_hold_reads_the_palettes_of_the_reached_sections() {
        let mut world = World::new(BlockStateId(0), 0);
        let mut chunk = Chunk::empty(0, 0);
        chunk.set(3, 70, 3, 9);
        world.chunks.insert((0, 0), Arc::new(chunk));
        let nine = |s: BlockStateId| s.0 == 9;
        // The section of y 64..80 holds 9, the one below does not.
        assert!(world.may_hold([0, 64, 0], [1, 65, 1], nine));
        assert!(world.may_hold([10, 79, 10], [12, 80, 12], nine));
        assert!(!world.may_hold([0, 50, 0], [15, 63, 15], nine));
        // A box over the unloaded column next door sees only air.
        assert!(!world.may_hold([16, 70, 0], [17, 71, 1], nine));
        assert!(world.may_hold([16, 70, 0], [17, 71, 1], |s| s.0 == 0));
        // Past the top, and the palette of air below it.
        assert!(!world.may_hold([0, 400, 0], [0, 401, 0], nine));
        assert!(world.may_hold([0, 400, 0], [0, 401, 0], |s| s.0 == 0));
    }
}

#[cfg(all(test, feature = "paired-packages"))]
mod residency_tests {
    use super::*;
    use crate::{
        mutation_observer::AuthorityScope,
        residency_observer::{ConfigError, Fault, Kind},
    };
    fn scope(dimension: &str) -> AuthorityScope {
        AuthorityScope::new(7, dimension).unwrap()
    }
    #[test]
    fn residency_snapshot_is_not_history_and_transitions_preserve_order() {
        let mut world = World::new(BlockStateId(0), 0);
        world.chunks.insert((2, 0), Arc::new(Chunk::empty(2, 0)));
        world.game_time = 10;
        let snapshot = world
            .enable_residency_observer(scope("minecraft:overworld"), [(2, 0), (-1, 0), (2, 0)], 8)
            .unwrap();
        assert_eq!(snapshot.entries(), &[((-1, 0), false), ((2, 0), true)]);
        assert_eq!(snapshot.game_time(), 10);
        assert_eq!(snapshot.scope().dimension(), "minecraft:overworld");
        assert!(world.drain_residency().unwrap().unwrap().is_empty());
        let revision = world.chunks.membership_revision();
        let old = world.insert_streamed_chunk(Chunk::empty(2, 0)).unwrap();
        let old_weak = Arc::downgrade(&old);
        drop(old);
        assert!(
            old_weak.upgrade().is_none(),
            "observer retained a whole old chunk"
        );
        assert_eq!(world.chunks.membership_revision(), revision);
        world.insert_streamed_chunk(Chunk::empty(50, 0)); // Unwatched.
        assert!(world.remove_streamed_chunk(&(-1, 0)).is_none());
        world.game_time = 11;
        world.remove_streamed_chunk(&(2, 0)).unwrap();
        world.insert_streamed_chunk(Chunk::empty(2, 0));
        let events = world.drain_residency().unwrap().unwrap();
        assert_eq!(
            events
                .iter()
                .map(|e| (e.coordinate(), e.kind(), e.game_time(), e.sequence()))
                .collect::<Vec<_>>(),
            [
                ((2, 0), Kind::Replaced, 10, 1),
                ((2, 0), Kind::Unloaded, 11, 2),
                ((2, 0), Kind::Loaded, 11, 3)
            ]
        );
        for event in events {
            assert_eq!(event.scope(), snapshot.scope());
        }
        world.remove_streamed_chunk(&(2, 0));
        let current = world.residency_snapshot().unwrap();
        assert_eq!(current.entries(), &[((-1, 0), false), ((2, 0), false)]);
        assert_eq!(current.game_time(), 11);
        assert_eq!(current.scope(), snapshot.scope());
        let pending = world.drain_residency().unwrap().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].sequence(), 4);
        assert_eq!(snapshot.entries()[1], ((2, 0), true)); // Immutable enable snapshot.
    }
    #[test]
    fn residency_faults_are_sticky_and_changes_still_commit() {
        let mut world = World::new(BlockStateId(0), 0);
        world.insert_streamed_chunk(Chunk::empty(0, 0));
        assert!(world.residency_observer().is_none());
        assert!(world.residency_snapshot().is_none());
        assert!(world.drain_residency().is_none());
        assert_eq!(
            world.enable_residency_observer(scope("minecraft:overworld"), [], 1),
            Err(ConfigError::InvalidWatch)
        );
        assert!(world.residency_observer().is_none());
        world
            .enable_residency_observer(scope("minecraft:overworld"), [(0, 0)], 1)
            .unwrap();
        assert_eq!(
            world.enable_residency_observer(scope("minecraft:the_nether"), [(1, 0)], 2),
            Err(ConfigError::AlreadyEnabled)
        );
        world.insert_streamed_chunk(Chunk::empty(0, 0));
        assert!(world.remove_streamed_chunk(&(0, 0)).is_some());
        let fault = Fault::Overflow {
            capacity: 1,
            sequence: 2,
        };
        assert_eq!(world.drain_residency(), Some(Err(fault)));
        world.insert_streamed_chunk(Chunk::empty(0, 0));
        assert!(world.chunks.contains_key(&(0, 0)));
        assert_eq!(
            world.residency_snapshot().unwrap().entries(),
            &[((0, 0), true)]
        );
        assert_eq!(world.drain_residency(), Some(Err(fault)));
        let mut observer = world.take_residency_observer().unwrap();
        assert_eq!(observer.pending_len(), 1);
        assert_eq!(observer.fault(), Some(fault));
        assert_eq!(observer.drain(), Err(fault));
        world.remove_streamed_chunk(&(0, 0));
        assert!(world.drain_residency().is_none());
    }
    #[test]
    fn residency_scope_moves_with_world_and_sdk_lending_is_not_unload() {
        let mut first = World::new(BlockStateId(0), 0);
        let mut second = World::new(BlockStateId(0), 0);
        for (world, dimension, time) in [
            (&mut first, "minecraft:overworld", 10),
            (&mut second, "minecraft:the_nether", 20),
        ] {
            world
                .enable_residency_observer(scope(dimension), [(0, 0)], 8)
                .unwrap();
            world.game_time = time;
            world.insert_streamed_chunk(Chunk::empty(0, 0));
        }
        std::mem::swap(&mut first, &mut second);
        let lent = std::mem::take(&mut first.chunks);
        first.chunks = lent;
        first.remove_streamed_chunk(&(0, 0));
        let moved = first.drain_residency().unwrap().unwrap();
        assert_eq!(moved.len(), 2);
        for event in moved {
            assert_eq!(event.scope().dimension(), "minecraft:the_nether");
            assert_eq!(event.game_time(), 20);
        }
        let parked = second.drain_residency().unwrap().unwrap();
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].scope().dimension(), "minecraft:overworld");
        assert_eq!(parked[0].game_time(), 10);
    }
}

#[cfg(all(test, feature = "paired-packages"))]
mod mutation_tests {
    use super::*;
    use crate::mutation_observer::{AuthorityScope, Cause, ConfigError, ObserverFault};

    fn resident() -> World {
        let mut world = World::new(BlockStateId(0), 0);
        world.chunks.insert((0, 0), Arc::new(Chunk::empty(0, 0)));
        world
    }
    fn enable(world: &mut World, dimension: &str, capacity: usize) {
        world
            .enable_mutation_observer(
                AuthorityScope::new(7, dimension).unwrap(),
                [BlockStateId(1), BlockStateId(2)],
                capacity,
            )
            .unwrap();
    }

    #[test]
    fn committed_transitions_stay_ordered_without_coalescing_or_clock_invention() {
        let mut world = resident();
        enable(&mut world, "minecraft:overworld", 8);
        world.game_time = 42;
        world.set(2, 70, 2, BlockStateId(9)).unwrap(); // Both states unobserved.
        for state in [1, 2, 0] {
            world.set(2, 70, 2, BlockStateId(state)).unwrap();
        }
        let batch = world.drain_mutations().unwrap().unwrap();
        let observed: Vec<_> = batch
            .iter()
            .map(|event| {
                (
                    event.position(),
                    event.previous().0,
                    event.current().0,
                    event.game_time(),
                    event.sequence(),
                    event.cause(),
                )
            })
            .collect();
        assert_eq!(
            observed,
            [
                ([2, 70, 2], 9, 1, 42, 1, Cause::Unclassified),
                ([2, 70, 2], 1, 2, 42, 2, Cause::Unclassified),
                ([2, 70, 2], 2, 0, 42, 3, Cause::Unclassified),
            ]
        );
        assert_eq!(batch[0].scope().world_instance(), 7);
        assert_eq!(batch[0].scope().dimension(), "minecraft:overworld");
        world.game_time = 43;
        world.set(3, 70, 3, BlockStateId(1)).unwrap();
        let next = world.drain_mutations().unwrap().unwrap();
        assert_eq!((next[0].game_time(), next[0].sequence()), (43, 4));
        assert!(world.drain_mutations().unwrap().unwrap().is_empty());
    }

    #[test]
    fn failed_or_same_state_writes_produce_no_observations() {
        let mut world = resident();
        enable(&mut world, "minecraft:overworld", 8);
        world.set(2, 70, 2, BlockStateId(1)).unwrap();
        world.drain_mutations().unwrap().unwrap();
        let revision = world.revision;
        world.set(2, 70, 2, BlockStateId(1)).unwrap();
        assert!(world.set(16, 70, 0, BlockStateId(1)).is_err());
        assert!(world.set(2, MIN_Y - 1, 2, BlockStateId(1)).is_err());
        assert!(
            world
                .set(2, MIN_Y + SECTIONS as i32 * 16, 2, BlockStateId(1))
                .is_err()
        );
        assert_eq!(world.revision, revision);
        assert!(world.drain_mutations().unwrap().unwrap().is_empty());
        world.set(2, 70, 2, BlockStateId(0)).unwrap();
        assert_eq!(world.drain_mutations().unwrap().unwrap()[0].sequence(), 2);
    }

    #[test]
    fn existing_watch_and_network_logs_remain_independent() {
        let mut world = resident();
        world.watched = vec![false, true];
        world.watched_writes.push(((-1, 0, 0), 5, 6));
        world.block_log = Some(vec![(-1, 0, 0)]);
        enable(&mut world, "minecraft:overworld", 8);
        world.set(2, 70, 2, BlockStateId(1)).unwrap();
        assert_eq!(
            world.watched_writes,
            [((-1, 0, 0), 5, 6), ((2, 70, 2), 0, 1)]
        );
        assert_eq!(world.block_log.as_ref().unwrap(), &[(-1, 0, 0), (2, 70, 2)]);
        assert_eq!(world.drain_mutations().unwrap().unwrap().len(), 1);
        assert_eq!(world.watched_writes.len(), 2);
        assert_eq!(world.block_log.as_ref().unwrap().len(), 2);
        world.watched_writes.clear();
        world.block_log.as_mut().unwrap().clear();
        world.set(2, 70, 2, BlockStateId(0)).unwrap();
        assert_eq!(world.drain_mutations().unwrap().unwrap().len(), 1);
        assert_eq!(world.watched_writes, [((2, 70, 2), 1, 0)]);
    }

    #[test]
    fn overflow_faults_the_batch_but_does_not_reject_committed_world_writes() {
        let mut world = resident();
        enable(&mut world, "minecraft:overworld", 1);
        world.set(2, 70, 2, BlockStateId(1)).unwrap();
        world.set(2, 70, 2, BlockStateId(2)).unwrap();
        let expected = ObserverFault::Overflow {
            capacity: 1,
            sequence: 2,
        };
        assert_eq!(world.block(2, 70, 2), BlockStateId(2));
        assert_eq!(world.drain_mutations(), Some(Err(expected)));
        world.set(2, 70, 2, BlockStateId(0)).unwrap();
        assert_eq!(world.block(2, 70, 2), BlockStateId(0));
        assert_eq!(world.drain_mutations(), Some(Err(expected)));
        let observer = world.mutation_observer().unwrap();
        assert_eq!(observer.pending_len(), 1);
        assert_eq!(observer.fault(), Some(expected));
    }

    #[test]
    fn dimension_scope_and_pending_history_travel_with_world_swaps() {
        let mut overworld = resident();
        let mut nether = resident();
        enable(&mut overworld, "minecraft:overworld", 8);
        enable(&mut nether, "minecraft:the_nether", 8);
        overworld.game_time = 10;
        nether.game_time = 20;
        overworld.set(2, 70, 2, BlockStateId(1)).unwrap();
        nether.set(2, 70, 2, BlockStateId(1)).unwrap();
        std::mem::swap(&mut overworld, &mut nether);
        overworld.set(2, 70, 2, BlockStateId(0)).unwrap();
        let moved = overworld.drain_mutations().unwrap().unwrap();
        assert_eq!(moved.len(), 2);
        for event in &moved {
            assert_eq!(event.scope().world_instance(), 7);
            assert_eq!(event.scope().dimension(), "minecraft:the_nether");
            assert_eq!(event.game_time(), 20);
        }
        assert_eq!((moved[0].sequence(), moved[1].sequence()), (1, 2));
        let parked = nether.drain_mutations().unwrap().unwrap();
        assert_eq!(parked[0].scope().dimension(), "minecraft:overworld");
        assert_eq!(parked[0].game_time(), 10);
        assert_eq!(parked[0].sequence(), 1);
    }

    #[test]
    fn default_is_disabled_and_replacement_requires_explicit_teardown() {
        let mut world = resident();
        world.set(2, 70, 2, BlockStateId(1)).unwrap();
        assert!(world.mutation_observer().is_none());
        assert!(world.drain_mutations().is_none());
        assert_eq!(
            world.enable_mutation_observer(
                AuthorityScope::new(7, "minecraft:overworld").unwrap(),
                [],
                2,
            ),
            Err(ConfigError::EmptyWatchedStates)
        );
        assert!(world.mutation_observer().is_none());
        enable(&mut world, "minecraft:overworld", 8);
        world.set(2, 70, 2, BlockStateId(2)).unwrap();
        assert_eq!(
            world.enable_mutation_observer(
                AuthorityScope::new(8, "minecraft:the_end").unwrap(),
                [BlockStateId(1)],
                2,
            ),
            Err(ConfigError::AlreadyEnabled)
        );
        let mut retained = world.take_mutation_observer().unwrap();
        assert_eq!(retained.scope().world_instance(), 7);
        assert_eq!(retained.drain().unwrap().len(), 1);
        world.set(2, 70, 2, BlockStateId(0)).unwrap();
        assert!(world.drain_mutations().is_none());
    }
}
