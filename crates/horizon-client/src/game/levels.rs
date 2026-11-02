//! A server's levels, one per dimension (`MinecraftServer.levels`).
//!
//! The game's tick code reads one level: the one in place, held in `Game`'s
//! own fields (the world, its streaming and save, block ticks, entities).
//! A server (the dedicated server and single player's server thread) keeps
//! the other dimensions' levels here and ticks each one with players or forced
//! chunks by swapping it into place (`enter_level`), the way `as_guest` swaps a player
//! in, so the tick code runs unchanged, one level at a time. The in-frame
//! game holds one level, which follows its player (`enter_dimension`).
//!
//! Shared by every level, as vanilla's derived levels share the
//! Overworld's level data: the game time, the day time and the weather, the
//! game rules, the scoreboard, the maps and the entities waiting to arrive
//! through a portal.

use super::{
    Dim, ExperienceOrb, FallingBlock, Game, ItemEntity, Mob, boats, commands,
    creaking::CreakingState, dragon, fireworks, lan::LanHost, lightning, machines, minecarts,
    ominous_item_spawner, persist, raids::Raiding, sculk::SculkState, tnt, trident,
};
use horizon_client::{
    Result, block_entity_anim::BlockEntityAnims, random_tick_sections::RandomTickSections,
    save::SaveDir, streaming::Streaming, tick_chunk_membership::TickChunkMembership,
    ticket_levels::ChunkTickets, vanillagen::VanillaTerrain, world::World,
};
use horizon_world::level::{LevelEvent, Ticks};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

#[cfg(feature = "paired-packages")]
pub(in crate::game) mod session_checkpoint;

/// Server ticks a level other than the Overworld stays loaded with no player
/// in it, before it is saved and closed (vanilla unloads a level's chunks
/// once no ticket holds them; the Overworld stays loaded).
const IDLE_TICKS: u32 = 300;

/// A level that is not in place: every per-level field of `Game`.
pub(super) struct Parked {
    world: World,
    streaming: Streaming,
    locator: Option<Arc<VanillaTerrain>>,
    save: Arc<SaveDir>,
    block_ticks: Ticks,
    tick_chunks: TickChunkMembership,
    tickets: ChunkTickets,
    random_tick_sections: RandomTickSections,
    level_events: Vec<LevelEvent>,
    falling: Vec<FallingBlock>,
    paintings: Vec<horizon_client::entities::Painting>,
    item_frames: Vec<horizon_client::entities::ItemFrame>,
    armor_stands: Vec<horizon_client::entities::ArmorStand>,
    cushions: Vec<horizon_client::entities::Cushion>,
    others: Vec<commands::OtherEntity>,
    boats: Vec<boats::Boat>,
    carts: Vec<minecarts::Minecart>,
    jukebox_sounds: HashMap<(i32, i32, i32), u64>,
    bolts: Vec<lightning::Bolt>,
    crystals: Vec<dragon::Crystal>,
    dragon: Option<dragon::Dragon>,
    dragon_fireballs: Vec<dragon::DragonFireball>,
    breath_clouds: Vec<dragon::Breath>,
    tridents: Vec<trident::ThrownTrident>,
    rockets: Vec<fireworks::Rocket>,
    ominous_spawners: Vec<ominous_item_spawner::OminousItemSpawner>,
    tnts: Vec<tnt::PrimedTnt>,
    hoppers: HashMap<(i32, i32, i32), machines::HopperState>,
    mobs: Vec<Mob>,
    /// The entity ids of the level's fence knots (`knot_ids`).
    knot_ids: HashMap<[i32; 3], u32>,
    projectiles: Vec<horizon_client::projectiles::Projectile>,
    dropped: Vec<ItemEntity>,
    orbs: Vec<ExperienceOrb>,
    persist: persist::EntityMemory,
    lectern_pulses: Vec<((i32, i32, i32), i32)>,
    lid_openers: HashMap<(i32, i32, i32), i32>,
    lan_events: Vec<(i32, [i32; 3], i32, Option<u32>)>,
    lan_block_events: Vec<([i32; 3], i32, i32, u32)>,
    lan_entity_events: Vec<(i32, i8, [f64; 3])>,
    lan_critical_hits: Vec<(Option<u32>, i32, bool)>,
    /// Only the per-level parts of these are used (`swap_level`).
    anims: BlockEntityAnims,
    raiding: Raiding,
    sculk: SculkState,
    creaking: CreakingState,
    /// Server ticks since a player was last in the level.
    idle: u32,
}

/// Which passes of the server tick the level in place runs: the first
/// reads the network (`Hub::poll`), the last ends the tick for every player
/// (`Hub::end_tick`). A tick with one level is both.
#[derive(Clone, Copy)]
pub(super) struct Pass {
    pub first: bool,
    pub last: bool,
}

impl Default for Pass {
    fn default() -> Self {
        Self {
            first: true,
            last: true,
        }
    }
}

/// The levels not in place, by dimension.
#[derive(Default)]
pub(super) struct Levels {
    parked: HashMap<Dim, Parked>,
}

impl Levels {
    /// The chunks of each level not in place (capability `world` lends
    /// them to server mods for a call).
    #[cfg_attr(not(feature = "mods"), allow(dead_code))]
    pub(super) fn parked_chunks(
        &mut self,
    ) -> impl Iterator<Item = (Dim, &mut horizon_client::chunk_map::ChunkMap)> {
        self.parked
            .iter_mut()
            .map(|(d, p)| (*d, &mut p.world.chunks))
    }
    /// The levels that are not in place, by dimension.
    pub(super) fn parked_dims(&self) -> Vec<Dim> {
        self.parked.keys().copied().collect()
    }
}
#[cfg(feature = "paired-packages")]
impl Game<'_> {
    pub(in crate::game) fn retained_level_dimensions(
        &self,
    ) -> std::result::Result<Vec<Dim>, String> {
        if self.levels.parked.contains_key(&self.dim) {
            return Err("native close duplicate active/parked dimension".into());
        }
        Ok([Dim::Overworld, Dim::Nether, Dim::End]
            .into_iter()
            .filter(|dim| *dim == self.dim || self.levels.parked.contains_key(dim))
            .collect())
    }
    pub(in crate::game) fn retained_raiding_for_level(&self, dim: Dim) -> Option<&Raiding> {
        if dim == self.dim {
            Some(&self.raiding)
        } else {
            self.levels.parked.get(&dim).map(|p| &p.raiding)
        }
    }
    pub(in crate::game) fn retained_raiding_for_level_mut(
        &mut self,
        dim: Dim,
    ) -> Option<&mut Raiding> {
        if dim == self.dim {
            Some(&mut self.raiding)
        } else {
            self.levels.parked.get_mut(&dim).map(|p| &mut p.raiding)
        }
    }
}

#[cfg(feature = "paired-packages")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::game) enum NativeLevelsStatus {
    Pending,
    Sealed,
    Blocked(String),
}
#[cfg(feature = "paired-packages")]
struct RetainedNativeLevel {
    dimension: Dim,
    save: Arc<SaveDir>,
    membership: u64,
    sealed: Option<horizon_client::streaming::SealedStream>,
    acknowledged: bool,
    protected: std::collections::HashSet<(i32, i32)>,
    identities: Option<persist::NativeEntityCloseIdentity>,
    modified: Option<std::collections::HashSet<(i32, i32)>>,
}
/// Fixed membership and original producer witnesses; never reconstructible from
/// serialized dimensions, hashes or an observed finished flag.
#[cfg(feature = "paired-packages")]
pub(in crate::game) struct RetainedNativeLevels {
    association: Arc<()>,
    root: std::path::PathBuf,
    active: Dim,
    deadline: Instant,
    cursor: usize,
    error: Option<String>,
    prepared: bool,
    validation: session_checkpoint::SessionValidationContext,
    accepted_uuids: std::collections::BTreeSet<[i32; 4]>,
    initial_reserved: Option<std::collections::HashSet<[i32; 4]>>,
    identity_reservoir: Option<std::collections::HashSet<[i32; 4]>>,
    identity_preparation_error: Option<String>,
    refusals: Vec<(Dim, String)>,
    levels: Vec<RetainedNativeLevel>,
}
#[cfg(feature = "paired-packages")]
impl RetainedNativeLevels {
    pub(in crate::game) fn saves(&self) -> Vec<Arc<SaveDir>> {
        self.levels.iter().map(|level| level.save.clone()).collect()
    }
    pub(in crate::game) fn read_refusals(&self) -> &[(Dim, String)] {
        &self.refusals
    }
    pub(in crate::game) fn acknowledged(&self) -> bool {
        self.levels.iter().all(|level| level.acknowledged)
    }
    fn blocked(&mut self, error: impl Into<String>) -> NativeLevelsStatus {
        let error = error.into();
        self.error.get_or_insert_with(|| error.clone());
        NativeLevelsStatus::Blocked(error)
    }
}
#[cfg(feature = "paired-packages")]
#[derive(Clone)]
pub(in crate::game) struct RetainedNativePayloads {
    association: Arc<()>,
    levels: Vec<RetainedNativeLevelPayload>,
}
#[cfg(feature = "paired-packages")]
#[derive(Clone)]
pub(in crate::game) struct RetainedNativeLevelPayload {
    dimension: Dim,
    save: Arc<SaveDir>,
    chunks: Vec<Arc<horizon_client::chunk::Chunk>>,
    entities: Vec<((i32, i32), Vec<horizon_world::nbt::Tag>)>,
    forced: std::collections::HashSet<(i32, i32)>,
    protected: std::collections::HashSet<(i32, i32)>,
    encoded: Vec<horizon_world::nbt::NamedTag>,
}
#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
impl Game<'_> {
    pub(in crate::game) fn seed_retained_native_level_fixture(
        &mut self,
        dimension: Dim,
        at: (i32, i32),
        ordinal: u32,
        stack: horizon_client::items::ItemStack,
    ) -> std::result::Result<(), String> {
        use horizon_core::geometry::Vec3;
        let kind = self
            .res
            .mob_types()
            .get("minecraft:cow")
            .cloned()
            .ok_or("native fixture cow resource missing")?;
        let block = self
            .pack
            .assets
            .registry
            .default_state("minecraft:stone")
            .ok_or("native fixture stone missing")?;
        let (world, streaming, memory, dropped, orbs, boats, mobs) = if dimension == self.dim {
            (
                &mut self.world,
                self.streaming.expect_existing_mut(),
                &mut self.persist,
                &mut self.dropped,
                &mut self.orbs,
                &mut self.boats,
                &mut self.mobs,
            )
        } else {
            let p = self
                .levels
                .parked
                .get_mut(&dimension)
                .ok_or("native fixture parked level missing")?;
            (
                &mut p.world,
                &mut p.streaming,
                &mut p.persist,
                &mut p.dropped,
                &mut p.orbs,
                &mut p.boats,
                &mut p.mobs,
            )
        };
        world.chunks.insert(
            at,
            Arc::new(horizon_client::chunk::Chunk::empty(at.0, at.1)),
        );
        world
            .set(at.0 * 16 + 1, 70, at.1 * 16 + 1, block)
            .map_err(|error| error.to_string())?;
        let position = Vec3::new(f64::from(at.0 * 16) + 2.5, 71.0, f64::from(at.1 * 16) + 2.5);
        let id = 700 + ordinal * 10;
        dropped.push(ItemEntity {
            id,
            stack: stack.clone(),
            position,
            previous: position,
            velocity: Vec3::new(0.125, 0.25, -0.125),
            thrower: Some([1, 2, 3, 4]),
            target: None,
            health: 5,
            fire_ticks: -17,
            on_ground: false,
            age: 23,
            pickup_delay: 37,
            bob_offset: 0.25,
            ticks: 11,
        });
        memory.seed_retained_native_raw_fixture(id, at, ordinal as i32);
        let mut boat = boats::Boat::new(id + 1, "oak".into(), position, 15.0).with_chest();
        boat.items[2] = Some(stack);
        boats.push(boat);
        let mut passenger = Mob::new(id + 2, kind.clone(), position, 15.0);
        passenger.extra.mount = Some(id + 1);
        passenger.extra.leash = Some(horizon_client::mobs::leash::Holder::Player);
        mobs.push(passenger);
        let mut rider = Mob::new(id + 3, kind, position, 30.0);
        rider.extra.mount = Some(id + 2);
        rider.extra.leash = Some(horizon_client::mobs::leash::Holder::Mob(id + 2));
        mobs.push(rider);
        // Two actual anonymous owners at the same position exercise distinct
        // frozen UUID occurrences, rather than a kind/position deduplication.
        for offset in [4, 5] {
            orbs.push(ExperienceOrb {
                id: id + offset,
                position,
                previous: position,
                velocity: Vec3::ZERO,
                value: 7,
                age: 13,
                on_ground: false,
                count: 2,
                ticks: 17,
            });
        }
        streaming.forced.insert(at);
        Ok(())
    }
    pub(in crate::game) fn seed_retained_saved_falling_fixture(
        &mut self,
        dimension: Dim,
        at: (i32, i32),
        uuid: [i32; 4],
    ) -> std::result::Result<horizon_world::nbt::Tag, String> {
        use horizon_core::{geometry::Vec3, position::BlockPos};
        use horizon_world::nbt::Tag;
        let state = self
            .pack
            .assets
            .registry
            .default_state("minecraft:sand")
            .ok_or("saved falling fixture sand missing")?;
        let position = Vec3::new(
            f64::from(at.0 * 16) + 2.5,
            73.25,
            f64::from(at.1 * 16) + 2.5,
        );
        let raw = Tag::compound([
            ("id", Tag::String("minecraft:falling_block".into())),
            ("UUID", Tag::IntArray(uuid.to_vec())),
            (
                "fixture:saved_falling",
                Tag::compound([
                    ("opaque", Tag::Long(-731)),
                    ("dimension", Tag::String(dimension.id().into())),
                ]),
            ),
        ]);
        let falling = FallingBlock {
            id: 9900,
            state,
            start_pos: BlockPos::new(at.0 * 16 + 2, 74, at.1 * 16 + 2),
            position,
            previous: position,
            velocity: Vec3::new(0.125, -0.25, -0.125),
            age: 29,
            no_gravity: false,
            drop_item: true,
            fall_distance: 3.5,
            stuck: Vec3::new(1.0, 1.0, 1.0),
            damage: horizon_client::falling_block::Damage::default(),
            saved: Some(Box::new((uuid, raw.clone()))),
        };
        if dimension == self.dim {
            self.falling.push(falling);
        } else {
            self.levels
                .parked
                .get_mut(&dimension)
                .ok_or("saved falling fixture parked level missing")?
                .falling
                .push(falling);
        }
        Ok(raw)
    }
}
#[cfg(feature = "paired-packages")]
impl RetainedNativePayloads {
    pub(in crate::game) fn levels(&self) -> &[RetainedNativeLevelPayload] {
        &self.levels
    }
    pub(in crate::game) fn overwritten_entity_rows(&self) -> Vec<(Dim, (i32, i32))> {
        self.levels
            .iter()
            .flat_map(|level| {
                level
                    .entities
                    .iter()
                    .map(move |(at, _)| (level.dimension, *at))
            })
            .collect()
    }
    pub(in crate::game) fn entity_uuid_rows(
        &self,
    ) -> std::result::Result<Vec<(Dim, (i32, i32), [i32; 4])>, String> {
        let mut rows = Vec::new();
        for level in &self.levels {
            for (at, tags) in &level.entities {
                for uuid in
                    persist::native_entity_tag_uuids(tags).map_err(|error| error.to_string())?
                {
                    rows.push((level.dimension, *at, uuid));
                }
            }
        }
        rows.sort_by_key(|(dimension, at, uuid)| (dimension.id(), *at, *uuid));
        Ok(rows)
    }
}
#[cfg(feature = "paired-packages")]
impl RetainedNativeLevelPayload {
    pub(in crate::game) fn dimension(&self) -> Dim {
        self.dimension
    }
    pub(in crate::game) fn save(&self) -> &Arc<SaveDir> {
        &self.save
    }
    pub(in crate::game) fn chunks(&self) -> &[Arc<horizon_client::chunk::Chunk>] {
        &self.chunks
    }
    pub(in crate::game) fn entities(&self) -> &[((i32, i32), Vec<horizon_world::nbt::Tag>)] {
        &self.entities
    }
    pub(in crate::game) fn forced(&self) -> &std::collections::HashSet<(i32, i32)> {
        &self.forced
    }
    pub(in crate::game) fn protected_nonresident(&self) -> &std::collections::HashSet<(i32, i32)> {
        &self.protected
    }
}
#[cfg(feature = "paired-packages")]
impl Parked {
    fn native_entity_view(
        &mut self,
        player_uuid: Option<[i32; 4]>,
        registry: &'static horizon_world::registry::BlockRegistry,
    ) -> persist::NativeEntityView<'_> {
        persist::NativeEntityView {
            persist: persist::NativeEntityMemory::borrow(&mut self.persist),
            player_uuid,
            registry,
            mobs: &self.mobs,
            dropped: &self.dropped,
            orbs: &self.orbs,
            projectiles: &self.projectiles,
            falling: &self.falling,
            boats: &self.boats,
            dragon: self.dragon.as_ref(),
            crystals: &self.crystals,
            ominous_spawners: &self.ominous_spawners,
            carts: &self.carts,
            armor_stands: &self.armor_stands,
            item_frames: &self.item_frames,
            cushions: &self.cushions,
            paintings: &self.paintings,
            others: &self.others,
            fixed_anon: None,
        }
    }
}
#[cfg(feature = "paired-packages")]
impl Game<'_> {
    // Dormant retained factory entry; ordinary level activation remains separate.
    #[allow(dead_code)]
    pub(in crate::game) fn add_retained_parked_level(
        &mut self,
        construction: super::world_loading::RetainedParkedConstruction,
    ) -> std::result::Result<(), super::world_loading::RetainedParkedConstruction> {
        let dimension = construction.dimension();
        if !construction.matches_game(self)
            || dimension == self.dim
            || self.levels.parked.contains_key(&dimension)
        {
            return Err(construction);
        }
        let parts = construction.into_parts()?;
        let parked = Parked {
            world: parts.world,
            streaming: parts.streaming,
            locator: parts.locator,
            save: parts.save,
            block_ticks: Ticks::new(self.level.seed as i32),
            tick_chunks: Default::default(),
            tickets: Default::default(),
            random_tick_sections: Default::default(),
            level_events: Vec::new(),
            falling: Vec::new(),
            paintings: Vec::new(),
            item_frames: Vec::new(),
            armor_stands: Vec::new(),
            cushions: Vec::new(),
            others: Vec::new(),
            boats: Vec::new(),
            carts: Vec::new(),
            jukebox_sounds: HashMap::new(),
            bolts: Vec::new(),
            crystals: Vec::new(),
            dragon: None,
            dragon_fireballs: Vec::new(),
            breath_clouds: Vec::new(),
            tridents: Vec::new(),
            rockets: Vec::new(),
            ominous_spawners: Vec::new(),
            tnts: Vec::new(),
            hoppers: HashMap::new(),
            mobs: Vec::new(),
            knot_ids: HashMap::new(),
            projectiles: Vec::new(),
            dropped: Vec::new(),
            orbs: Vec::new(),
            persist: Default::default(),
            lectern_pulses: Vec::new(),
            lid_openers: HashMap::new(),
            lan_events: Vec::new(),
            lan_block_events: Vec::new(),
            lan_entity_events: Vec::new(),
            lan_critical_hits: Vec::new(),
            anims: Default::default(),
            raiding: Default::default(),
            sculk: Default::default(),
            creaking: Default::default(),
            idle: 0,
        };
        self.levels.parked.insert(dimension, parked);
        Ok(())
    }
    fn native_level(&self, dimension: Dim) -> Option<(&World, &Streaming, &Arc<SaveDir>)> {
        if dimension == self.dim {
            Some((
                &self.world,
                self.streaming.as_existing().ok()?,
                self.save.as_ref()?,
            ))
        } else {
            self.levels
                .parked
                .get(&dimension)
                .map(|p| (&p.world, &p.streaming, &p.save))
        }
    }
    fn native_level_mut(&mut self, dimension: Dim) -> Option<(&mut World, &mut Streaming)> {
        if dimension == self.dim {
            self.save.as_ref()?;
            Some((&mut self.world, self.streaming.as_existing_mut().ok()?))
        } else {
            self.levels
                .parked
                .get_mut(&dimension)
                .map(|p| (&mut p.world, &mut p.streaming))
        }
    }
    fn preflight_native_membership(
        &self,
        levels: &RetainedNativeLevels,
    ) -> std::result::Result<(), String> {
        if self.world_root != levels.root
            || self.dim != levels.active
            || self.retained_level_dimensions()?
                != levels
                    .levels
                    .iter()
                    .map(|level| level.dimension)
                    .collect::<Vec<_>>()
        {
            return Err(
                "retained native fixed level membership or original dimension changed".into(),
            );
        }
        for level in &levels.levels {
            let (world, _, save) = self
                .native_level(level.dimension)
                .ok_or("retained native level disappeared")?;
            if !Arc::ptr_eq(save, &level.save)
                || world.chunks.membership_revision() != level.membership
                || save.root
                    != horizon_client::save::dimension_dir(&self.world_root, level.dimension.id())
            {
                return Err(
                    "retained native World membership or SaveDir allocation changed".into(),
                );
            }
        }
        Ok(())
    }
    pub(in crate::game) fn begin_retained_native_levels(
        &self,
        deadline: Instant,
    ) -> std::result::Result<RetainedNativeLevels, String> {
        let mut fixed = Vec::new();
        for dimension in self.retained_level_dimensions()? {
            let (world, _, save) = self
                .native_level(dimension)
                .ok_or("native close level absent")?;
            fixed.push(RetainedNativeLevel {
                dimension,
                save: save.clone(),
                membership: world.chunks.membership_revision(),
                sealed: None,
                acknowledged: false,
                protected: Default::default(),
                identities: None,
                modified: None,
            });
        }
        Ok(RetainedNativeLevels {
            association: Arc::new(()),
            root: self.world_root.clone(),
            active: self.dim,
            deadline,
            cursor: 0,
            error: None,
            prepared: false,
            validation: self.capture_native_validation_context(),
            accepted_uuids: Default::default(),
            initial_reserved: None,
            identity_reservoir: None,
            identity_preparation_error: None,
            refusals: Vec::new(),
            levels: fixed,
        })
    }
    pub(in crate::game) fn poll_retained_native_levels(
        &mut self,
        levels: &mut RetainedNativeLevels,
    ) -> NativeLevelsStatus {
        use horizon_client::streaming::RetainedStreamStatus as S;
        if let Err(error) = self.preflight_native_membership(levels) {
            return levels.blocked(error);
        }
        if let Some(error) = &levels.error {
            return NativeLevelsStatus::Blocked(error.clone());
        }
        if levels
            .levels
            .iter()
            .all(|level| level.sealed.is_some() || level.acknowledged)
        {
            return NativeLevelsStatus::Sealed;
        }
        let index = levels.cursor % levels.levels.len();
        levels.cursor = (index + 1) % levels.levels.len();
        if levels.levels[index].sealed.is_some() {
            return NativeLevelsStatus::Pending;
        }
        let dimension = levels.levels[index].dimension;
        let status = {
            let (world, streaming) = self.native_level_mut(dimension).unwrap();
            if let Err(error) = streaming.begin_retained_close(Some(world), levels.deadline) {
                return levels.blocked(error);
            }
            streaming.poll_retained_close(Some(world))
        };
        match status {
            S::Pending => NativeLevelsStatus::Pending,
            S::Blocked(error) => levels.blocked(error),
            S::Complete => {
                let (_, streaming) = self.native_level_mut(dimension).unwrap();
                let Some(sealed) = streaming.take_sealed() else {
                    return levels.blocked("retained native producer witness missing");
                };
                levels.refusals.extend(
                    sealed
                        .refused_reads()
                        .iter()
                        .cloned()
                        .map(|error| (dimension, error)),
                );
                if sealed.failed_initialization {
                    levels.refusals.push((
                        dimension,
                        "retained native producer initialization failed".into(),
                    ));
                }
                levels.levels[index].sealed = Some(sealed);
                NativeLevelsStatus::Pending
            }
            S::NeedsRefusedRead => {
                let (_, streaming) = self.native_level_mut(dimension).unwrap();
                let read = streaming.retained_refused_read().unwrap();
                let position = read.position();
                let error = read.error().to_owned();
                let ticket = read.into_ticket();
                match streaming.resolve_retained_refused_read(ticket, error) {
                    Ok(true) => {
                        levels.levels[index].protected.insert(position);
                        NativeLevelsStatus::Pending
                    }
                    Ok(false) => NativeLevelsStatus::Pending,
                    Err(error) => levels.blocked(error),
                }
            }
            S::NeedsFinishedColumn => {
                let (world, streaming, save) = self.native_level(dimension).unwrap();
                let view = streaming.retained_finished_column().unwrap();
                let position = (view.chunk().x, view.chunk().z);
                let read_only = view.evidence().is_some()
                    && !view.chunk().fresh
                    && view.origin() == horizon_client::streaming::NativeWorkOrigin::Generate;
                let mut native_level = self.level.clone();
                native_level.dimension = dimension.id().into();
                let result = super::world_loading::validate_retained_column_context(
                    &levels.validation,
                    &self.pack,
                    &self.res,
                    &self.persist.uuids,
                    &self.world_root,
                    Some(&native_level),
                    Some(world),
                    self.session_lock.as_ref(),
                    save,
                    if dimension == self.dim {
                        self.native_checkpoint_origin.as_ref()
                    } else {
                        None
                    },
                    &levels.accepted_uuids,
                    view.chunk(),
                    view.evidence(),
                );
                let ticket = view.into_ticket();
                let (_, streaming) = self.native_level_mut(dimension).unwrap();
                match result {
                    Ok(None) => NativeLevelsStatus::Pending,
                    Ok(Some((disposition, identities))) => {
                        match streaming.resolve_retained_finished_column(ticket, disposition) {
                            Ok(true) => {
                                levels.levels[index].protected.insert(position);
                                levels.accepted_uuids.extend(identities);
                                NativeLevelsStatus::Pending
                            }
                            Ok(false) => NativeLevelsStatus::Pending,
                            Err(error) => levels.blocked(error),
                        }
                    }
                    Err(error) => {
                        let error = error.to_string();
                        if read_only {
                            match streaming.resolve_retained_refused_read(ticket, error) {
                                Ok(true) => {
                                    levels.levels[index].protected.insert(position);
                                    NativeLevelsStatus::Pending
                                }
                                Ok(false) => NativeLevelsStatus::Pending,
                                Err(error) => levels.blocked(error),
                            }
                        } else {
                            let rejection =
                                streaming.reject_retained_finished_column(ticket, error.clone());
                            levels.blocked(rejection.err().unwrap_or(error))
                        }
                    }
                }
            }
        }
    }
    pub(in crate::game) fn retry_retained_native_levels(
        &mut self,
        levels: &mut RetainedNativeLevels,
        deadline: Instant,
    ) -> std::result::Result<(), String> {
        self.preflight_native_membership(levels)?;
        if deadline <= Instant::now() {
            return Err("native close retry deadline is not future".into());
        }
        for level in &levels.levels {
            if level.sealed.is_none() && !level.acknowledged {
                let (world, streaming) = self.native_level_mut(level.dimension).unwrap();
                streaming.begin_retained_close(Some(world), levels.deadline)?;
                // Observe the actual retained attempt with its original World;
                // renew only a real Blocked attempt, never replay callbacks.
                if matches!(
                    streaming.poll_retained_close(Some(world)),
                    horizon_client::streaming::RetainedStreamStatus::Blocked(_)
                ) {
                    streaming.retry_retained_close(deadline)?;
                }
            }
        }
        levels.deadline = deadline;
        levels.error = None;
        Ok(())
    }
    pub(in crate::game) fn poll_retained_native_writer_ack(
        &mut self,
        levels: &mut RetainedNativeLevels,
        closed: &[horizon_client::save::NativeWriterClosed],
    ) -> NativeLevelsStatus {
        use horizon_client::streaming::RetainedStreamStatus as S;
        if let Err(error) = self.preflight_native_membership(levels) {
            return levels.blocked(error);
        }
        for level in &mut levels.levels {
            if level.acknowledged {
                continue;
            }
            let Some(writer) = closed
                .iter()
                .find(|writer| Arc::ptr_eq(writer.save(), &level.save))
            else {
                return NativeLevelsStatus::Pending;
            };
            let (_, streaming) = self.native_level_mut(level.dimension).unwrap();
            match streaming.poll_retained_writer_ack(writer) {
                S::Pending => return NativeLevelsStatus::Pending,
                S::Complete => {}
                S::Blocked(error) => return levels.blocked(error),
                _ => {
                    return levels.blocked(
                        "unsettled native producer payload at final writer acknowledgement",
                    );
                }
            }
            let Some(sealed) = level.sealed.take() else {
                return levels
                    .blocked("native producer witness absent at final writer acknowledgement");
            };
            if let Err(sealed) = streaming.finish_retained_close(sealed, writer) {
                level.sealed = Some(sealed);
                return levels.blocked("native producer/final writer allocation mismatch");
            }
            level.acknowledged = true;
            return NativeLevelsStatus::Pending;
        }
        NativeLevelsStatus::Sealed
    }
    fn capture_retained_native_level(
        &mut self,
        level: &RetainedNativeLevel,
    ) -> std::result::Result<RetainedNativeLevelPayload, String> {
        let identity = level
            .identities
            .as_ref()
            .ok_or("native close entity identity preparation missing")?;
        let dimension = level.dimension;
        let (mut chunks, loaded, forced) = {
            let (world, streaming, _) = self
                .native_level(dimension)
                .ok_or("native close level absent during projection")?;
            let (ticks, anims) = if dimension == self.dim {
                (&self.block_ticks, &self.be_anims)
            } else {
                let parked = self.levels.parked.get(&dimension).unwrap();
                (&parked.block_ticks, &parked.anims)
            };
            let chunks = horizon_client::save::chunks_with_block_entities(
                world.chunks.values(),
                &world.block_entities,
                &world.block_names,
            )
            .into_iter()
            .map(|mut chunk| {
                persist::native_attach_save_data(
                    &mut chunk,
                    world,
                    ticks,
                    anims,
                    self.world.game_time,
                );
                Arc::new(chunk)
            })
            .collect::<Vec<_>>();
            (
                chunks,
                world.loaded_entities.clone(),
                streaming.forced.clone(),
            )
        };
        chunks.sort_by_key(|chunk| (chunk.x, chunk.z));
        let mut entities = if dimension == self.dim {
            self.native_entity_view().project_close(identity)
        } else {
            let player_uuid = self.level.player_uuid;
            let registry = self.pack.assets.registry;
            self.levels
                .parked
                .get_mut(&dimension)
                .unwrap()
                .native_entity_view(player_uuid, registry)
                .project_close(identity)
        }
        .map_err(|error| error.to_string())?;
        let mut rows: std::collections::BTreeMap<_, _> = entities.drain(..).collect();
        for chunk in &chunks {
            rows.entry((chunk.x, chunk.z)).or_insert_with(Vec::new);
        }
        // Preserve native arrivals as original raw records. No spawn_loaded_entities,
        // native ID allocation, simulation or ownership removal occurs here.
        for (at, tags) in loaded {
            rows.entry(at).or_insert_with(Vec::new).extend(tags);
        }
        entities = rows.into_iter().collect();
        if chunks
            .iter()
            .any(|chunk| level.protected.contains(&(chunk.x, chunk.z)))
            || entities.iter().any(|(at, _)| level.protected.contains(at))
        {
            return Err("native close final resident/entity payload conflicts with protected producer record; original owners retained".into());
        }
        let tables = horizon_client::save::SaveTables::new(&self.pack.assets);
        let shape = horizon_client::save::LevelShape::for_dir(&level.save.root);
        let encoded = chunks
            .iter()
            .map(|chunk| {
                horizon_client::save::encode_with(
                    chunk,
                    self.pack.assets.registry,
                    self.res.biomes(),
                    Some(&tables),
                    shape,
                )
            })
            .collect();
        Ok(RetainedNativeLevelPayload {
            dimension,
            save: level.save.clone(),
            chunks,
            entities,
            forced,
            protected: level.protected.clone(),
            encoded,
        })
    }
    pub(in crate::game) fn prepare_retained_native_payloads(
        &mut self,
        levels: &mut RetainedNativeLevels,
        reserved: &std::collections::HashSet<[i32; 4]>,
    ) -> std::result::Result<RetainedNativePayloads, String> {
        self.preflight_native_membership(levels)?;
        if let Some(error) = &levels.identity_preparation_error {
            return Err(error.clone());
        }
        if levels.prepared
            || levels.error.is_some()
            || levels.levels.iter().any(|level| level.sealed.is_none())
        {
            return Err("native close final projection requires first preparation after actual producer seal".into());
        }
        if levels
            .initial_reserved
            .as_ref()
            .is_some_and(|original| original != reserved)
        {
            return Err("native close old-source UUID reservoir changed".into());
        }
        let result = self.prepare_retained_native_payloads_once(levels, reserved);
        if let Err(error) = &result {
            levels.identity_preparation_error = Some(error.clone());
        }
        result
    }
    fn prepare_retained_native_payloads_once(
        &mut self,
        levels: &mut RetainedNativeLevels,
        reserved: &std::collections::HashSet<[i32; 4]>,
    ) -> std::result::Result<RetainedNativePayloads, String> {
        if levels.identity_reservoir.is_none() {
            let mut used = reserved.clone();
            used.extend(levels.accepted_uuids.iter().copied());
            used.extend(self.level.player_uuid);
            // Inspect every current level BEFORE generating the first UUID, so
            // new identities cannot collide with later parked or raw owners.
            for dimension in self.retained_level_dimensions()? {
                for (_, tags) in &self.native_level(dimension).unwrap().0.loaded_entities {
                    used.extend(
                        persist::native_entity_tag_uuids(tags)
                            .map_err(|error| error.to_string())?,
                    );
                }
                let current = if dimension == self.dim {
                    self.native_entity_view().current_close_uuids()
                } else {
                    let uuid = self.level.player_uuid;
                    let registry = self.pack.assets.registry;
                    self.levels
                        .parked
                        .get_mut(&dimension)
                        .unwrap()
                        .native_entity_view(uuid, registry)
                        .current_close_uuids()
                }
                .map_err(|error| error.to_string())?;
                used.extend(current);
            }
            levels.initial_reserved = Some(reserved.clone());
            levels.identity_reservoir = Some(used);
        }
        let mut payloads = Vec::new();
        for level in &mut levels.levels {
            if level.identities.is_none() {
                let used = levels.identity_reservoir.as_mut().unwrap();
                let identity = if level.dimension == self.dim {
                    self.native_entity_view().prepare_close_identities(used)
                } else {
                    let uuid = self.level.player_uuid;
                    let registry = self.pack.assets.registry;
                    self.levels
                        .parked
                        .get_mut(&level.dimension)
                        .unwrap()
                        .native_entity_view(uuid, registry)
                        .prepare_close_identities(used)
                }
                .map_err(|error| error.to_string())?;
                level.identities = Some(identity);
                level.modified = Some(
                    self.native_level(level.dimension)
                        .unwrap()
                        .0
                        .modified
                        .clone(),
                );
            }
            payloads.push(self.capture_retained_native_level(level)?);
        }
        let mut uuids = levels.accepted_uuids.clone();
        for payload in &payloads {
            for (_, tags) in &payload.entities {
                let mut work: Vec<_> = tags.iter().collect();
                while let Some(tag) = work.pop() {
                    if let Some(horizon_world::nbt::Tag::IntArray(uuid)) = tag.get("UUID") {
                        let uuid: [i32; 4] = uuid
                            .as_slice()
                            .try_into()
                            .map_err(|_| "native close entity UUID size")?;
                        if !uuids.insert(uuid) || self.level.player_uuid == Some(uuid) {
                            return Err("native close duplicate entity or player UUID across original level/producer owners".into());
                        }
                    }
                    if let Some(horizon_world::nbt::Tag::List(passengers)) = tag.get("Passengers") {
                        work.extend(passengers);
                    }
                }
            }
        }
        levels.prepared = true;
        Ok(RetainedNativePayloads {
            association: levels.association.clone(),
            levels: payloads,
        })
    }
    pub(in crate::game) fn preflight_retained_native_payloads(
        &mut self,
        levels: &RetainedNativeLevels,
        payloads: &RetainedNativePayloads,
    ) -> std::result::Result<(), String> {
        self.preflight_native_membership(levels)?;
        if !levels.prepared
            || !Arc::ptr_eq(&levels.association, &payloads.association)
            || levels.levels.len() != payloads.levels.len()
        {
            return Err("native close original payload association absent or changed".into());
        }
        for (level, expected) in levels.levels.iter().zip(&payloads.levels) {
            if self.native_level(level.dimension).unwrap().0.modified
                != *level
                    .modified
                    .as_ref()
                    .ok_or("native close original modified set absent")?
            {
                return Err("native close original modified marker set changed".into());
            }
            let current = self.capture_retained_native_level(level)?;
            if current.dimension != expected.dimension
                || !Arc::ptr_eq(&current.save, &expected.save)
                || current.encoded != expected.encoded
                || current.entities != expected.entities
                || current.forced != expected.forced
                || current.protected != expected.protected
            {
                return Err(
                    "native close original native payload, routes or forced state changed".into(),
                );
            }
        }
        Ok(())
    }
    pub(in crate::game) fn commit_retained_native_payloads(
        &mut self,
        levels: &mut RetainedNativeLevels,
    ) {
        // Caller has checked EVERY native/Level/participant/metadata/image owner
        // before entering any infallible marker commit under the same Game guard.
        for level in &levels.levels {
            if let Some(modified) = &level.modified
                && let Some((world, _)) = self.native_level_mut(level.dimension)
            {
                for at in modified {
                    world.modified.remove(at);
                }
            }
        }
    }
}

impl Game<'_> {
    /// End every owned trial without swapping levels or changing save results.
    #[cfg(feature = "paired-packages")]
    pub(super) fn retire_all_counter_trials(&mut self, boundary: &str) {
        self.retire_counter_trial(boundary);
        for parked in self.levels.parked.values_mut() {
            super::counter_control::retire_world_counter(&mut parked.world, boundary);
        }
    }

    /// A new level for `dim`, opened from the save (`MinecraftServer.createLevels`).
    fn open_level(&self, dim: Dim) -> Result<Parked> {
        let (streaming, save, locator) = super::world_loading::make_streaming(
            &self.res,
            &self.pack,
            &self.options,
            &self.settings,
            &self.world_root,
            dim,
            self.level.seed,
        )?;
        let mut world = World::new(self.res.facts().air, self.level.seed);
        world.fast_lava = dim == Dim::Nether;
        Ok(Parked {
            world,
            streaming,
            locator,
            save,
            block_ticks: Ticks::new(self.level.seed as i32),
            tick_chunks: Default::default(),
            tickets: Default::default(),
            random_tick_sections: Default::default(),
            level_events: Vec::new(),
            falling: Vec::new(),
            paintings: Vec::new(),
            item_frames: Vec::new(),
            armor_stands: Vec::new(),
            cushions: Vec::new(),
            others: Vec::new(),
            boats: Vec::new(),
            carts: Vec::new(),
            jukebox_sounds: HashMap::new(),
            bolts: Vec::new(),
            crystals: Vec::new(),
            dragon: None,
            dragon_fireballs: Vec::new(),
            breath_clouds: Vec::new(),
            tridents: Vec::new(),
            rockets: Vec::new(),
            ominous_spawners: Vec::new(),
            tnts: Vec::new(),
            hoppers: HashMap::new(),
            mobs: Vec::new(),
            knot_ids: HashMap::new(),
            projectiles: Vec::new(),
            dropped: Vec::new(),
            orbs: Vec::new(),
            persist: Default::default(),
            lectern_pulses: Vec::new(),
            lid_openers: HashMap::new(),
            lan_events: Vec::new(),
            lan_block_events: Vec::new(),
            lan_entity_events: Vec::new(),
            lan_critical_hits: Vec::new(),
            anims: Default::default(),
            raiding: Default::default(),
            sculk: Default::default(),
            creaking: Default::default(),
            idle: 0,
        })
    }

    /// Exchange the level in place with `p`.
    fn swap_level(&mut self, p: &mut Parked) {
        // Check the audited current Game factory invariant before changing any
        // provenance or moving any other level owner. This is not remote dispatch.
        self.streaming.expect_existing();
        self.save
            .as_ref()
            .expect("current Game factory owns storage");
        #[cfg(feature = "paired-packages")]
        {
            // Swapping away and back must never revive initial-open provenance.
            self.native_checkpoint_origin = None;
        }
        use std::mem::swap;
        swap(&mut self.world, &mut p.world);
        self.streaming
            .try_swap_existing(&mut p.streaming)
            .expect("active Existing owner was checked before the level swap");
        let mut locator = self.end.locator();
        swap(&mut locator, &mut p.locator);
        self.end.set_locator(locator);
        self.share_spawn_conditions();
        swap(
            self.save
                .as_mut()
                .expect("active save was checked before the level swap"),
            &mut p.save,
        );
        swap(&mut self.block_ticks, &mut p.block_ticks);
        swap(&mut self.tick_chunks, &mut p.tick_chunks);
        swap(&mut self.tickets, &mut p.tickets);
        swap(&mut self.random_tick_sections, &mut p.random_tick_sections);
        swap(&mut self.level_events, &mut p.level_events);
        swap(&mut self.falling, &mut p.falling);
        swap(&mut self.paintings, &mut p.paintings);
        swap(&mut self.item_frames, &mut p.item_frames);
        swap(&mut self.armor_stands, &mut p.armor_stands);
        swap(&mut self.cushions, &mut p.cushions);
        swap(&mut self.others, &mut p.others);
        swap(&mut self.boats, &mut p.boats);
        swap(&mut self.carts, &mut p.carts);
        swap(&mut self.jukebox_sounds, &mut p.jukebox_sounds);
        swap(&mut self.bolts, &mut p.bolts);
        swap(&mut self.crystals, &mut p.crystals);
        swap(&mut self.dragon, &mut p.dragon);
        swap(&mut self.dragon_fireballs, &mut p.dragon_fireballs);
        swap(&mut self.breath_clouds, &mut p.breath_clouds);
        swap(&mut self.tridents, &mut p.tridents);
        swap(&mut self.rockets, &mut p.rockets);
        swap(&mut self.ominous_spawners, &mut p.ominous_spawners);
        swap(&mut self.tnts, &mut p.tnts);
        swap(&mut self.hoppers, &mut p.hoppers);
        swap(&mut self.mobs, &mut p.mobs);
        swap(&mut self.knot_ids, &mut p.knot_ids);
        swap(&mut self.creatures.projectiles, &mut p.projectiles);
        swap(&mut self.dropped, &mut p.dropped);
        swap(&mut self.orbs, &mut p.orbs);
        swap(&mut self.persist, &mut p.persist);
        swap(&mut self.lectern_pulses, &mut p.lectern_pulses);
        swap(&mut self.lid_openers, &mut p.lid_openers);
        swap(&mut self.lan_events, &mut p.lan_events);
        swap(&mut self.lan_block_events, &mut p.lan_block_events);
        swap(&mut self.lan_entity_events, &mut p.lan_entity_events);
        swap(&mut self.lan_critical_hits, &mut p.lan_critical_hits);
        let (a, b) = (&mut self.be_anims, &mut p.anims);
        swap(&mut a.chests, &mut b.chests);
        swap(&mut a.books, &mut b.books);
        swap(&mut a.shulkers, &mut b.shulkers);
        swap(&mut a.bells, &mut b.bells);
        swap(&mut a.conduits, &mut b.conduits);
        swap(&mut a.skulls, &mut b.skulls);
        swap(&mut a.pots, &mut b.pots);
        swap(&mut a.gateways, &mut b.gateways);
        swap(&mut a.gateway_servers, &mut b.gateway_servers);
        swap(&mut a.conduit_servers, &mut b.conduit_servers);
        self.raiding.swap_level(&mut p.raiding);
        self.sculk.swap_level(&mut p.sculk);
        self.creaking.swap_level(&mut p.creaking);
        // What only the client draws starts over.
        self.particles.clear();
        self.pickups.clear();
    }

    /// Put `target`'s level in place, opening it first if it is not loaded.
    /// The level that was in place is kept with the others; the shared
    /// clock and weather go with the swap. `host` is the hub when the caller
    /// holds it (`self.lan` is taken during the network tick).
    pub(super) fn enter_level(&mut self, host: Option<&mut LanHost>, target: Dim) -> Result<()> {
        // The in-frame game keeps its one level (`enter_dimension`).
        if self.options.execution_role.is_client() {
            return Ok(());
        }
        if target != self.dim {
            let (mut p, opened) = match self.levels.parked.remove(&target) {
                Some(p) => (p, false),
                None => (self.open_level(target)?, true),
            };
            p.world.game_time = self.world.game_time;
            p.world.day_time = self.world.day_time;
            p.world.weather = self.world.weather;
            // The game rules and the difficulty are the server's, not a
            // level's: natural regeneration holds in every dimension.
            p.world.health_rules = self.world.health_rules;
            // The sculk tables' watched blocks are built once per game.
            p.world.watched = std::mem::take(&mut self.world.watched);
            self.swap_level(&mut p);
            p.idle = 0;
            let left = std::mem::replace(&mut self.dim, target);
            self.levels.parked.insert(left, p);
            if opened {
                // The new streaming's radius and meshing options.
                self.apply_settings();
            }
        }
        let host = match host {
            Some(h) => Some(h),
            None => self.lan.as_mut(),
        };
        if let Some(h) = host {
            h.hub.set_dimension(target.id());
        }
        Ok(())
    }

    /// Put `dim`'s level in place for a command whose source is there
    /// (`execute in`, `execute at` a player in another level), opening it if
    /// nobody is in it; returns the level to put back afterwards. Only a
    /// server holds other levels.
    pub(super) fn enter_command_level(&mut self, dim: Dim) -> Option<Dim> {
        if self.options.execution_role.is_client() || dim == self.dim {
            return None;
        }
        let back = self.dim;
        match self.enter_level(None, dim) {
            Ok(()) => Some(back),
            Err(e) => {
                log::error!("could not open {}: {e}", dim.id());
                None
            }
        }
    }

    /// One server tick (`MinecraftServer.tickServer`): each level with
    /// players or forced chunks in turn (`tickChildren`), its chunks streamed
    /// and loaded and its world ticked. The first pass reads the network and runs the
    /// tick's shared parts (the clock, the weather, functions, the autosave);
    /// the next ones only their level. While `paused`, only the connections
    /// run.
    pub(super) fn tick_server(&mut self, paused: bool) -> Result<()> {
        let dims = self.levels_to_tick();
        let n = dims.len();
        let mut result = Ok(());
        for (k, dim) in dims.into_iter().enumerate() {
            if let Err(e) = self.enter_level(None, dim) {
                result = Err(e);
                break;
            }
            self.level_pass = Pass {
                first: k == 0,
                last: k + 1 == n,
            };
            self.update_dedicated_streaming();
            self.update_world();
            // What server mods set in this level since its last tick.
            self.apply_mod_block_writes();
            result = if paused {
                self.tick_lan(true);
                Ok(())
            } else if k == 0 {
                self.tick()
            } else {
                let running = self.clock.world_runs();
                if running {
                    self.tick_weather_effects();
                }
                self.tick_world(running)
            };
            if result.is_err() {
                break;
            }
        }
        self.level_pass = Pass::default();
        result?;
        self.close_idle_levels()
    }

    /// `tick_server` as a server thread runs it. With `HORIZON_SPIKES=MS`, a
    /// tick longer than MS is logged with its thread CPU time and spans.
    pub(super) fn tick_server_timed(&mut self, paused: bool) -> Result<()> {
        let Some(threshold) = self.options.spikes else {
            return self.tick_server(paused);
        };
        let (started, cpu) = (Instant::now(), horizon_client::spans::thread_cpu());
        let result = self.tick_server(paused);
        let wall = started.elapsed().as_secs_f32() * 1000.0;
        // Taken every tick: nothing else takes this thread's spans.
        let spans = horizon_client::spans::take();
        if wall > threshold {
            let cpu = horizon_client::spans::thread_cpu()
                .zip(cpu)
                .map_or_else(String::new, |(end, start)| {
                    format!(", cpu {:.1} ms", (end - start).as_secs_f32() * 1000.0)
                });
            log::info!(
                "Horizon: slow tick {wall:.1} ms{cpu} ({} mobs): {}",
                self.mobs.len(),
                horizon_client::spans::describe(&spans)
            );
        }
        result
    }

    /// The end of `MinecraftServer.runServer`'s loop for a server thread: a
    /// finished `/tick sprint` reports to the operators as the server, then
    /// the next tick starts one tick length after this one, or straight away
    /// while the sprint runs (`checkShouldSprintThisTick`).
    pub(super) fn wait_for_next_server_tick(&mut self, next: &mut Instant, paused: bool) {
        if let Some(report) = self.clock.take_sprint_report() {
            // ServerTickRateManager.finishTickSprint: the server's sendSuccess.
            self.console_broadcast = Some(true);
            self.sprint_report(&report);
            self.console_broadcast = None;
            if let Some(mut host) = self.lan.take() {
                self.send_admin_feedback(&mut host, None);
                self.lan = Some(host);
            }
            self.admin_feedback.clear();
        }
        let now = Instant::now();
        if !paused && self.clock.sprinting() {
            *next = now;
            return;
        }
        *next += self.clock.tick_duration();
        if *next > now {
            // A `/pregen` task or force-loaded chunks keep the workers fed
            // until the next tick.
            self.pump_pregen(*next);
            let now = Instant::now();
            if *next > now {
                std::thread::sleep(*next - now);
            }
        } else if now - *next > Duration::from_secs(2) {
            *next = now;
        }
    }

    /// The levels this server tick runs, in `MinecraftServer.levels` order
    /// (the Overworld, the Nether, the End): every level with a player in
    /// it, forced chunks or a `/pregen` task, or the Overworld while idle.
    pub(super) fn levels_to_tick(&self) -> Vec<Dim> {
        let guests = self
            .lan
            .as_ref()
            .map_or(&[][..], |h| h.hub.guests.as_slice());
        let mut dims: Vec<Dim> = [Dim::Overworld, Dim::Nether, Dim::End]
            .into_iter()
            .filter(|d| {
                let forced = if *d == self.dim {
                    !self.streaming.expect_existing().forced.is_empty()
                } else {
                    self.levels
                        .parked
                        .get(d)
                        .is_some_and(|p| !p.streaming.forced.is_empty())
                };
                guests.iter().any(|g| g.dimension == d.id())
                    || forced
                    || self.pregen_level() == Some(*d)
            })
            .collect();
        if dims.is_empty() {
            dims.push(Dim::Overworld);
        }
        dims
    }

    /// Count the parked levels' empty ticks, and save and close the ones
    /// (other than the Overworld) nobody has been in for a while.
    pub(super) fn close_idle_levels(&mut self) -> Result<()> {
        let occupied = self.levels_to_tick();
        let mut closing = Vec::new();
        for (dim, p) in &mut self.levels.parked {
            if occupied.contains(dim) {
                p.idle = 0;
                continue;
            }
            p.idle += 1;
            if *dim != Dim::Overworld && p.idle >= IDLE_TICKS {
                closing.push(*dim);
            }
        }
        if closing.is_empty() {
            return Ok(());
        }
        let back = self.dim;
        let loop_result = (|| -> Result<()> {
            for dim in closing {
                self.enter_level(None, dim)?;
                self.save_level(true)?;
                self.enter_level(None, back)?;
                #[cfg(feature = "paired-packages")]
                if let Some(parked) = self.levels.parked.get_mut(&dim) {
                    super::counter_control::retire_world_counter(
                        &mut parked.world,
                        "idle-level-close",
                    );
                }
                self.levels.parked.remove(&dim);
            }
            Ok(())
        })();
        // Completed closes stay closed; a failed save retains its native owner.
        let restore_result = self.enter_level(None, back);
        if let Err(error) = &restore_result {
            log::error!(
                "idle-close restoration failed: requested {}, active {}: {error}",
                back.id(),
                self.dim.id(),
            );
        }
        loop_result.and(restore_result)
    }

    /// Save every parked level (`MinecraftServer.saveAllChunks`); the level
    /// in place is the caller's to save.
    pub(super) fn save_parked_levels(&mut self, wait: bool) -> Result<()> {
        if self.levels.parked.is_empty() {
            return Ok(());
        }
        let back = self.dim;
        let dims: Vec<Dim> = self.levels.parked.keys().copied().collect();
        let loop_result = (|| -> Result<()> {
            for dim in dims {
                self.enter_level(None, dim)?;
                self.save_level(wait)?;
            }
            Ok(())
        })();
        // A save error must not leave a different native level and hub in place.
        let restore_result = self.enter_level(None, back);
        if let Err(error) = &restore_result {
            log::error!(
                "parked-save restoration failed: requested {}, active {}: {error}",
                back.id(),
                self.dim.id(),
            );
        }
        loop_result.and(restore_result)
    }
}

#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::game) enum UnsupportedEntityFamily {
    Falling,
    Paintings,
    ItemFrames,
    ArmorStands,
    Cushions,
    Others,
    Boats,
    Carts,
    Bolts,
    Crystals,
    Dragon,
    DragonFireballs,
    BreathClouds,
    Tridents,
    Rockets,
    OminousSpawners,
    Tnts,
    Mobs,
    Projectiles,
    Orbs,
    LeashKnots,
    FishingHook,
}
#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::game) enum EntityOwnerObservationError {
    NotAuthoritative,
    ActingGuest,
    DuplicateActiveDimension,
    WorldSlotTrial {
        dimension: Dim,
    },
    LoadedEntities {
        dimension: Dim,
    },
    EntityMemory {
        dimension: Dim,
        kind: persist::EntityMemoryKind,
    },
    UnsupportedEntityFamily {
        dimension: Dim,
        family: UnsupportedEntityFamily,
    },
    InvalidDroppedPosition {
        dimension: Dim,
        id: u32,
    },
    OffResidentDropped {
        dimension: Dim,
        id: u32,
    },
    PortalTravel,
    PortalArrivals,
    Reservation,
}
#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
pub(in crate::game) struct EntityOwnerObservation {
    levels: Vec<EntityOwnerLevel>,
}
#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
struct EntityOwnerLevel {
    dimension: Dim,
    resident_drops: usize,
    #[cfg(all(test, target_os = "linux"))]
    accounted_slots: u8,
}
#[cfg(feature = "paired-packages")]
#[derive(Clone, Copy)]
enum EntitySlotProfile {
    Ordinary,
    #[cfg(all(test, target_os = "linux"))]
    SlotFixture,
    #[cfg(all(test, target_os = "linux"))]
    SlotAdvance,
}

#[cfg(feature = "paired-packages")]
macro_rules! unsupported_entities {
    ($owner:expr, $projectiles:expr) => {{
        use UnsupportedEntityFamily as F;
        [
            (F::Falling, !$owner.falling.is_empty()),
            (F::Paintings, !$owner.paintings.is_empty()),
            (F::ItemFrames, !$owner.item_frames.is_empty()),
            (F::ArmorStands, !$owner.armor_stands.is_empty()),
            (F::Cushions, !$owner.cushions.is_empty()),
            (F::Others, !$owner.others.is_empty()),
            (F::Boats, !$owner.boats.is_empty()),
            (F::Carts, !$owner.carts.is_empty()),
            (F::Bolts, !$owner.bolts.is_empty()),
            (F::Crystals, !$owner.crystals.is_empty()),
            (F::Dragon, $owner.dragon.is_some()),
            (F::DragonFireballs, !$owner.dragon_fireballs.is_empty()),
            (F::BreathClouds, !$owner.breath_clouds.is_empty()),
            (F::Tridents, !$owner.tridents.is_empty()),
            (F::Rockets, !$owner.rockets.is_empty()),
            (F::OminousSpawners, !$owner.ominous_spawners.is_empty()),
            (F::Tnts, !$owner.tnts.is_empty()),
            (F::Mobs, !$owner.mobs.is_empty()),
            (F::Projectiles, !$projectiles.is_empty()),
            (F::Orbs, !$owner.orbs.is_empty()),
            (F::LeashKnots, !$owner.knot_ids.is_empty()),
        ]
        .into_iter()
        .find_map(|(family, present)| present.then_some(family))
    }};
}
#[cfg(feature = "paired-packages")]
fn inspect_entity_level(
    dimension: Dim,
    world: &World,
    memory: &persist::EntityMemory,
    dropped: &[ItemEntity],
    unsupported: Option<UnsupportedEntityFamily>,
) -> std::result::Result<EntityOwnerLevel, EntityOwnerObservationError> {
    use EntityOwnerObservationError as E;
    if world.has_world_slot_trial() {
        return Err(E::WorldSlotTrial { dimension });
    }
    inspect_entity_level_owners(dimension, world, memory, dropped, unsupported)
}
#[cfg(feature = "paired-packages")]
fn inspect_entity_level_owners(
    dimension: Dim,
    world: &World,
    memory: &persist::EntityMemory,
    dropped: &[ItemEntity],
    unsupported: Option<UnsupportedEntityFamily>,
) -> std::result::Result<EntityOwnerLevel, EntityOwnerObservationError> {
    use EntityOwnerObservationError as E;
    if !world.loaded_entities.is_empty() {
        return Err(E::LoadedEntities { dimension });
    }
    if let Some(kind) = memory.checkpoint_entity_memory() {
        return Err(E::EntityMemory { dimension, kind });
    }
    if let Some(family) = unsupported {
        return Err(E::UnsupportedEntityFamily { dimension, family });
    }
    for item in dropped {
        let p = item.position;
        if !p.x.is_finite()
            || !p.y.is_finite()
            || !p.z.is_finite()
            || p.x.floor() < f64::from(i32::MIN)
            || p.x.floor() > f64::from(i32::MAX)
            || p.z.floor() < f64::from(i32::MIN)
            || p.z.floor() > f64::from(i32::MAX)
        {
            return Err(E::InvalidDroppedPosition {
                dimension,
                id: item.id,
            });
        }
        let column = ((p.x.floor() as i32) >> 4, (p.z.floor() as i32) >> 4);
        if !world.chunks.contains_key(&column) {
            return Err(E::OffResidentDropped {
                dimension,
                id: item.id,
            });
        }
    }
    Ok(EntityOwnerLevel {
        dimension,
        resident_drops: dropped.len(),
        #[cfg(all(test, target_os = "linux"))]
        accounted_slots: 0,
    })
}
#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
impl Game<'_> {
    /// Current named ENTITY owners only; no player check, lasting barrier or readiness token.
    pub(in crate::game) fn observe_checkpoint_entity_owners(
        &self,
    ) -> std::result::Result<EntityOwnerObservation, EntityOwnerObservationError> {
        self.observe_entity_owners_profile(EntitySlotProfile::Ordinary)
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(in crate::game) fn observe_slot_epoch_entity_owners(
        &self,
    ) -> std::result::Result<EntityOwnerObservation, EntityOwnerObservationError> {
        self.observe_entity_owners_profile(EntitySlotProfile::SlotFixture)
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(in crate::game) fn observe_slot_advance_entity_owners(
        &self,
    ) -> std::result::Result<EntityOwnerObservation, EntityOwnerObservationError> {
        self.observe_entity_owners_profile(EntitySlotProfile::SlotAdvance)
    }
    fn observe_entity_owners_profile(
        &self,
        profile: EntitySlotProfile,
    ) -> std::result::Result<EntityOwnerObservation, EntityOwnerObservationError> {
        use super::nether_portal::PortalEntityOwner;
        use EntityOwnerObservationError as E;
        if self.remote.is_some()
            || self.integrated.is_some()
            || self.options.execution_role.is_client()
        {
            return Err(E::NotAuthoritative);
        }
        if self.lan_acting.is_some() {
            return Err(E::ActingGuest);
        }
        if self.levels.parked.contains_key(&self.dim) {
            return Err(E::DuplicateActiveDimension);
        }
        let count = self
            .levels
            .parked
            .len()
            .checked_add(1)
            .ok_or(E::Reservation)?;
        let mut levels = Vec::new();
        levels.try_reserve(count).map_err(|_| E::Reservation)?;
        let active = match profile {
            EntitySlotProfile::Ordinary => inspect_entity_level(
                self.dim,
                &self.world,
                &self.persist,
                &self.dropped,
                unsupported_entities!(self, self.creatures.projectiles),
            ),
            #[cfg(all(test, target_os = "linux"))]
            EntitySlotProfile::SlotFixture | EntitySlotProfile::SlotAdvance => {
                match profile {
                    EntitySlotProfile::SlotAdvance => self.slot_advance_projection(),
                    _ => self.slot_epoch_projection(),
                }
                .map_err(|_| E::WorldSlotTrial {
                    dimension: self.dim,
                })?;
                let mut row = inspect_entity_level_owners(
                    self.dim,
                    &self.world,
                    &self.persist,
                    &self.dropped,
                    unsupported_entities!(self, self.creatures.projectiles),
                )?;
                row.accounted_slots = 1;
                Ok(row)
            }
        }?;
        levels.push(active);
        for (&dimension, parked) in &self.levels.parked {
            levels.push(inspect_entity_level(
                dimension,
                &parked.world,
                &parked.persist,
                &parked.dropped,
                unsupported_entities!(parked, parked.projectiles),
            )?);
        }
        if self.hook.is_some() {
            return Err(E::UnsupportedEntityFamily {
                dimension: self.dim,
                family: UnsupportedEntityFamily::FishingHook,
            });
        }
        if self.end_portal_travel {
            return Err(E::PortalTravel);
        }
        match self.portals.checkpoint_entity_owner() {
            Some(PortalEntityOwner::Travel) => return Err(E::PortalTravel),
            Some(PortalEntityOwner::Arrivals) => return Err(E::PortalArrivals),
            None => {}
        }
        levels.sort_unstable_by_key(|row| row.dimension.id());
        Ok(EntityOwnerObservation { levels })
    }
}

#[cfg(all(test, feature = "paired-packages"))]
pub(super) mod entity_owner_tests;

#[cfg(all(test, feature = "paired-packages"))]
mod parked_save_tests;

#[cfg(all(test, feature = "paired-packages"))]
mod idle_close_tests;

#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::game) enum PendingKind {
    Shulker,
    Banner,
    Names,
    Pot,
    Copied,
}
#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::game) enum SpillKind {
    Spawns,
    Removed,
    Orbs,
}
#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::game) enum WorldOwnerObservationError {
    NotAuthoritative,
    ActingGuest,
    DuplicateActiveDimension,
    World {
        dimension: Dim,
        cause: horizon_client::world::WorldOwnerPresence,
    },
    LevelEvents {
        dimension: Dim,
    },
    Pending {
        kind: PendingKind,
        position: [i32; 3],
    },
    Spill {
        kind: SpillKind,
    },
    Brushing {
        position: [i32; 3],
    },
    HopperPass,
    Pregen,
}
#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::game) struct WorldOwnerLevel {
    dimension: Dim,
    resident_columns: usize,
}
#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::game) struct WorldOwnerObservation {
    levels: [Option<WorldOwnerLevel>; 3],
}

#[cfg(feature = "paired-packages")]
#[derive(Clone, Copy)]
enum WorldObservationProfile {
    Ordinary,
    #[cfg(all(test, target_os = "linux"))]
    NativeSlotRecapture,
    #[cfg(all(test, target_os = "linux"))]
    NativeSlotAdvance,
}

#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
impl Game<'_> {
    /// Only named current W1 owners; never global readiness or producer completion.
    pub(in crate::game) fn observe_checkpoint_world_owners(
        &self,
    ) -> std::result::Result<WorldOwnerObservation, WorldOwnerObservationError> {
        self.observe_world_owners_profile(WorldObservationProfile::Ordinary)
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(in crate::game) fn observe_slot_recapture_world_owners(
        &self,
    ) -> std::result::Result<WorldOwnerObservation, WorldOwnerObservationError> {
        self.observe_world_owners_profile(WorldObservationProfile::NativeSlotRecapture)
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(in crate::game) fn observe_slot_advance_world_owners(
        &self,
    ) -> std::result::Result<WorldOwnerObservation, WorldOwnerObservationError> {
        self.observe_world_owners_profile(WorldObservationProfile::NativeSlotAdvance)
    }
    fn observe_world_owners_profile(
        &self,
        profile: WorldObservationProfile,
    ) -> std::result::Result<WorldOwnerObservation, WorldOwnerObservationError> {
        use WorldOwnerObservationError as E;
        use horizon_client::world::WorldOwnerLocation;
        if self.remote.is_some()
            || self.integrated.is_some()
            || self.options.execution_role.is_client()
        {
            return Err(E::NotAuthoritative);
        }
        if self.lan_acting.is_some() {
            return Err(E::ActingGuest);
        }
        if self.levels.parked.contains_key(&self.dim) {
            return Err(E::DuplicateActiveDimension);
        }
        let mut first: Option<(u8, usize, Option<WorldOwnerLocation>, E)> = None;
        let mut consider = |rank, dimension, location, error| {
            let key = (rank, dimension, location);
            if first.as_ref().is_none_or(|old| key < (old.0, old.1, old.2)) {
                first = Some((rank, dimension, location, error));
            }
        };
        let mut levels = [None; 3];
        for (index, dimension) in [Dim::Overworld, Dim::Nether, Dim::End]
            .into_iter()
            .enumerate()
        {
            let (world, events) = if dimension == self.dim {
                (&self.world, &self.level_events)
            } else if let Some(parked) = self.levels.parked.get(&dimension) {
                (&parked.world, &parked.level_events)
            } else {
                continue;
            };
            levels[index] = Some(WorldOwnerLevel {
                dimension,
                resident_columns: world.chunks.len(),
            });
            let cause = match profile {
                WorldObservationProfile::Ordinary => world.checkpoint_world_owner(),
                #[cfg(all(test, target_os = "linux"))]
                WorldObservationProfile::NativeSlotRecapture => {
                    if dimension == self.dim {
                        world.checkpoint_world_owner_for_native_empty_roots()
                    } else {
                        world.checkpoint_world_owner()
                    }
                }
                #[cfg(all(test, target_os = "linux"))]
                WorldObservationProfile::NativeSlotAdvance => {
                    if dimension == self.dim {
                        world.checkpoint_world_owner_for_native_previous_tick_roots()
                    } else {
                        world.checkpoint_world_owner()
                    }
                }
            };
            if let Some(cause) = cause {
                consider(
                    cause.kind as u8,
                    index,
                    Some(cause.location),
                    E::World { dimension, cause },
                );
            }
            if !events.is_empty() {
                consider(16, index, None, E::LevelEvents { dimension });
            }
        }
        for (kind, position) in [
            (
                PendingKind::Shulker,
                self.pending.shulker.keys().min().copied(),
            ),
            (
                PendingKind::Banner,
                self.pending.banner.keys().min().copied(),
            ),
            (PendingKind::Names, self.pending.names.keys().min().copied()),
            (PendingKind::Pot, self.pending.pot.keys().min().copied()),
            (
                PendingKind::Copied,
                self.pending.copied.keys().min().copied(),
            ),
        ] {
            if let Some(position) = position {
                consider(
                    17 + kind as u8,
                    3,
                    Some(WorldOwnerLocation::Block(position)),
                    E::Pending { kind, position },
                );
            }
        }
        for (kind, present) in [
            (SpillKind::Spawns, !self.spill.spawns.is_empty()),
            (SpillKind::Removed, !self.spill.removed.is_empty()),
            (SpillKind::Orbs, !self.spill.orbs.is_empty()),
        ] {
            if present {
                consider(22 + kind as u8, 3, None, E::Spill { kind });
            }
        }
        if let Some(&position) = self.brushing.keys().min() {
            consider(
                25,
                3,
                Some(WorldOwnerLocation::Block(position)),
                E::Brushing { position },
            );
        }
        if self.item_columns.is_some() {
            consider(26, 3, None, E::HopperPass);
        }
        // Presence deliberately includes paused tasks; do not pump or construct one here.
        if self.cmds.pregen.is_some() {
            consider(27, 3, None, E::Pregen);
        }
        match first {
            Some((_, _, _, error)) => Err(error),
            None => Ok(WorldOwnerObservation { levels }),
        }
    }
}

#[cfg(all(test, feature = "paired-packages"))]
mod world_owner_tests;

/// The session profile admits native history, rather than the research fixture's
/// exact-tick empty roots. Validate every opaque field before accepting it.
#[cfg(feature = "paired-packages")]
fn session_native_metadata(extras: &horizon_client::chunk::ChunkExtras, clock: i64) -> bool {
    use horizon_world::nbt::{Compound, Tag};
    fn field<'a>(map: &'a Compound, key: &str) -> Option<&'a Tag> {
        map.get(&key.into())
    }
    if extras.last_update < 0 || extras.last_update > clock || !extras.block_entity_order.is_empty()
    {
        return false;
    }
    // Fresh full native chunks, including snapshots made before a reload, may
    // have no preserved root fields. The encoder supplies its empty defaults.
    if extras.root.is_empty() {
        return true;
    }
    if extras.root.len() != 3
        || !matches!(field(&extras.root, "InhabitedTime"), Some(Tag::Long(value)) if *value >= 0)
    {
        return false;
    }
    let Some(Tag::Compound(structures)) = field(&extras.root, "structures") else {
        return false;
    };
    if structures.len() != 2
        || !["References", "starts"].into_iter().all(|key| {
            matches!(field(structures, key), Some(Tag::Compound(values)) if values.is_empty())
        })
    {
        return false;
    }
    matches!(field(&extras.root, "PostProcessing"), Some(Tag::List(sections))
        if sections.len() == horizon_client::chunk::SECTIONS
            && sections.iter().all(|section| matches!(section, Tag::List(values) if values.is_empty())))
}

#[cfg(feature = "paired-packages")]
impl Game<'_> {
    /// The fresh recovery route retains a verified native arrival queue while
    /// staging items; all existing simulated entity families must still be empty.
    pub(in crate::game) fn inspect_session_checkpoint_item_install_owners(&self) -> Result<()> {
        if unsupported_entities!(self, self.creatures.projectiles).is_some()
            || !self.dropped.is_empty()
            || self.hook.is_some()
            || self.end_portal_travel
            || self.portals.checkpoint_entity_owner().is_some()
            || self.persist.checkpoint_entity_memory().is_some()
        {
            return Err("fresh item installer already owns ENTITY state".into());
        }
        self.portals
            .project_session_item_portal_owners(&[])
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    /// Full named WORLD/ENTITY census for one native session. The caller must
    /// independently project the checked persistent counter and slot before
    /// accounting the sole supported slot. No research profile is enabled here.
    pub(in crate::game) fn inspect_session_checkpoint_owners(&self) -> Result<()> {
        use horizon_client::world::WorldOwnerKind as K;
        if self.remote.is_some()
            || self.integrated.is_some()
            || self.options.execution_role.is_client()
            || self.lan_acting.is_some()
            || self.dim != Dim::Overworld
            || !self.levels.parked.is_empty()
        {
            return Err("session checkpoint requires one authoritative initial Overworld".into());
        }
        if self.world.game_time < 0 {
            return Err("session checkpoint requires a nonnegative native clock".into());
        }
        // The library owns private borrowed/pending/added block-entity queues.
        // Its ordering guarantees all those categories precede opaque roots.
        // Do not accept a minimum OpaqueChunkRoot as a complete owner census:
        // every chunk and every later named family is checked below as well.
        if let Some(cause) = self.world.checkpoint_world_owner()
            && cause.kind < K::OpaqueChunkRoot
        {
            return Err(format!("session checkpoint unsupported WORLD owner: {cause:?}").into());
        }
        if self.world.block_entities.keys().next().is_some()
            || !self.world.chest_openers.is_empty()
            || !self.world.block_names.is_empty()
        {
            return Err("session checkpoint native block-entity ownership".into());
        }
        for (&(x, z), chunk) in &self.world.chunks {
            if chunk.x != x
                || chunk.z != z
                || !chunk.block_entities.is_empty()
                || !chunk.block_names.is_empty()
                || !chunk.entities.is_empty()
                || !chunk.structure_refs.is_empty()
                || !chunk.structure_starts.is_empty()
                || !chunk.postprocess.is_empty()
            {
                return Err(
                    format!("session checkpoint unsupported chunk owner at {x},{z}").into(),
                );
            }
            if let Some(saved) = &chunk.saved
                && (!saved.block_entities.is_empty()
                    || !saved.block_entity_sources.is_empty()
                    || !saved.rolled_loot.is_empty()
                    || !saved.block_ticks.is_empty()
                    || !saved.fluid_ticks.is_empty()
                    || !session_native_metadata(saved, self.world.game_time))
            {
                return Err(
                    format!("session checkpoint unsupported saved owner at {x},{z}").into(),
                );
            }
        }
        if !self.world.watched_writes.is_empty()
            || !self.world.moving_pistons.is_empty()
            || !self.world.moving_piston_fallback.is_empty()
            || self.world.inhabited.values().any(|ticks| *ticks < 0)
            || self.block_ticks.blocks.count() != 0
            || self.block_ticks.fluids.count() != 0
            || !self.block_ticks.comparator_outputs.is_empty()
            || !self.block_ticks.torch_toggles.is_empty()
            || !self.block_ticks.moving_pistons.is_empty()
            || !self.block_ticks.piston_pushes.is_empty()
            || !self.block_ticks.block_events.is_empty()
            || !self.level_events.is_empty()
        {
            return Err("session checkpoint deferred WORLD ownership".into());
        }
        if !self.pending.shulker.is_empty()
            || !self.pending.banner.is_empty()
            || !self.pending.names.is_empty()
            || !self.pending.pot.is_empty()
            || !self.pending.copied.is_empty()
            || !self.spill.spawns.is_empty()
            || !self.spill.removed.is_empty()
            || !self.spill.orbs.is_empty()
            || !self.brushing.is_empty()
            || self.item_columns.is_some()
            || self.cmds.pregen.is_some()
            || !self.hoppers.is_empty()
            || !self.lectern_pulses.is_empty()
            || !self.lid_openers.is_empty()
        {
            return Err("session checkpoint pending WORLD owner or producer".into());
        }
        // Session-only item accounting; the ordinary conservative census above
        // continues to refuse all native memory categories.
        if !self.world.loaded_entities.is_empty()
            || unsupported_entities!(self, self.creatures.projectiles).is_some()
        {
            return Err("session checkpoint unsupported ENTITY owner or arrival".into());
        }
        self.preflight_session_checkpoint_item_memory()
            .map_err(|error| format!("session checkpoint unsupported ENTITY owner: {error}"))?;
        self.portals
            .project_session_item_portal_owners(&self.dropped)
            .map_err(|error| error.to_string())?;
        if self.hook.is_some()
            || self.end_portal_travel
            || self.portals.checkpoint_entity_owner().is_some()
        {
            return Err("session checkpoint fishing or portal ENTITY owner".into());
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "paired-packages", target_os = "linux"))]
pub(in crate::game) mod capture_integration;
