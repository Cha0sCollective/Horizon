//! Bounded opt-in native counter placement restore. No ordinary loader enables this cell.
use super::World;
use crate::{
    chunk::{MIN_Y, SECTIONS},
    counter_trial::{Configuration, Record, RetirementReason, Trial},
    mutation_observer::AuthorityScope,
};
use horizon_mods::{content::machines::MachineType, paired::PreparedBundle};
use horizon_world::{
    chunk::BlockState,
    registry::{BlockRegistry, BlockStateId},
};
use serde::{
    Deserialize, Serialize,
    de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use serde_json::Value;
use std::{collections::BTreeMap, fmt, sync::Arc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreError {
    InvalidFrame,
    IdentityMismatch,
    BindingMismatch,
    ClockMismatch,
    Occupied,
    NotResident,
    StateMismatch,
    GenerationExhausted,
    RuntimeExhausted,
    Faulted,
    Detached,
    Entropy,
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "native counter restore: {self:?}")
    }
}
impl std::error::Error for RestoreError {}
type Result<T> = std::result::Result<T, RestoreError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorldId([u8; 16]);
/// Opaque initial-World identity. This is neither a save identity nor readiness.
pub(super) struct HostCounterMarker;
/// One native host bootstrap, owned with its new-world session lock. The host
/// must establish a genuinely new logical identity and exclude alternate record
/// installation under that UUID. No serialized data can reconstruct this grant.
pub struct HostCounterBootstrap {
    world: WorldId,
    marker: Arc<HostCounterMarker>,
    bound: bool,
    spent: bool,
}
impl HostCounterBootstrap {
    pub fn for_new_host_world(world: WorldId) -> Self {
        Self {
            world,
            marker: Arc::new(HostCounterMarker),
            bound: false,
            spent: false,
        }
    }
}
impl WorldId {
    pub fn parse(text: &str) -> Result<Self> {
        if text.len() != 36 {
            return Err(RestoreError::InvalidFrame);
        }
        let mut bytes = [0; 16];
        let mut digit = 0;
        for (i, byte) in text.bytes().enumerate() {
            if [8, 13, 18, 23].contains(&i) {
                if byte != b'-' {
                    return Err(RestoreError::InvalidFrame);
                }
            } else {
                let nibble = match byte {
                    b'0'..=b'9' => byte - b'0',
                    b'a'..=b'f' => byte - b'a' + 10,
                    _ => return Err(RestoreError::InvalidFrame),
                };
                bytes[digit / 2] |= nibble << if digit % 2 == 0 { 4 } else { 0 };
                digit += 1;
            }
        }
        if bytes[6] >> 4 != 4 || bytes[8] >> 6 != 2 {
            return Err(RestoreError::InvalidFrame);
        }
        Ok(Self(bytes))
    }
    fn fresh() -> Result<Self> {
        let mut bytes = [0; 16];
        horizon_protocol::crypto::random_bytes(&mut bytes).map_err(|_| RestoreError::Entropy)?;
        bytes[6] = (bytes[6] & 15) | 0x40;
        bytes[8] = (bytes[8] & 63) | 0x80;
        Ok(Self(bytes))
    }
}
impl fmt::Display for WorldId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, byte) in self.0.iter().enumerate() {
            if [4, 6, 8, 10].contains(&i) {
                f.write_str("-")?;
            }
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}
impl Serialize for WorldId {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for WorldId {
    fn deserialize<D: de::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CounterTarget {
    pub dimension: String,
    pub position: [i32; 3],
}
impl CounterTarget {
    pub fn new(dimension: &str, position: [i32; 3]) -> Result<Self> {
        if !canonical(dimension) || !(MIN_Y..MIN_Y + SECTIONS as i32 * 16).contains(&position[1]) {
            return Err(RestoreError::InvalidFrame);
        }
        Ok(Self {
            dimension: dimension.into(),
            position,
        })
    }
}
fn canonical(value: &str) -> bool {
    if value.len() > 256 {
        return false;
    }
    let Some((namespace, path)) = value.split_once(':') else {
        return false;
    };
    let token = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_.-".contains(&c);
    !namespace.is_empty()
        && namespace.bytes().all(token)
        && !path.is_empty()
        && path.bytes().all(|c| token(c) || c == b'/')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}
fn digest(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn valid_digest(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// Immutable captured package and native registry descriptors, never scalar payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeBindingProfile {
    LegacyIron,
    RegisteredCube,
}
impl NativeBindingProfile {
    pub fn counter_kind(self) -> &'static str {
        match self {
            Self::LegacyIron => "native-counter-checkpoint",
            Self::RegisteredCube => "native-registered-counter-checkpoint",
        }
    }
    pub(super) fn slot_kind(self) -> &'static str {
        match self {
            Self::LegacyIron => "native-slot-checkpoint",
            Self::RegisteredCube => "native-registered-slot-checkpoint",
        }
    }
    pub(super) fn from_slot_kind(kind: &str) -> Result<Self> {
        match kind {
            "native-slot-checkpoint" => Ok(Self::LegacyIron),
            "native-registered-slot-checkpoint" => Ok(Self::RegisteredCube),
            _ => Err(RestoreError::InvalidFrame),
        }
    }
    pub(super) fn physical_name(self, alias: &str) -> &str {
        match self {
            Self::LegacyIron => "minecraft:iron_block",
            Self::RegisteredCube => alias,
        }
    }
}
#[derive(Clone)]
pub struct NativeBinding {
    profile: NativeBindingProfile,
    guest: Option<Arc<PreparedBundle>>,
    package: [u8; 32],
    machine_type: MachineType,
    block_id: String,
    state_id: BlockStateId,
    registry: &'static BlockRegistry,
}
impl NativeBinding {
    pub fn from_prepared(
        prepared: &PreparedBundle,
        type_id: &str,
        registry: &'static BlockRegistry,
    ) -> Result<Self> {
        Self::from_profile(
            prepared,
            type_id,
            registry,
            NativeBindingProfile::LegacyIron,
        )
    }
    /// Captured declaration plus the actual materialized host registry. This is
    /// a checked binding, not resource provenance or a bootstrap capability.
    pub fn from_registered_prepared(
        prepared: &PreparedBundle,
        type_id: &str,
        registry: &'static BlockRegistry,
    ) -> Result<Self> {
        Self::from_profile(
            prepared,
            type_id,
            registry,
            NativeBindingProfile::RegisteredCube,
        )
    }
    fn from_profile(
        prepared: &PreparedBundle,
        type_id: &str,
        registry: &'static BlockRegistry,
        profile: NativeBindingProfile,
    ) -> Result<Self> {
        let owner = &prepared.bundle().content_owner().id;
        let content = prepared.content();
        let machine_type = content
            .machine_types()
            .get(type_id)
            .ok_or(RestoreError::BindingMismatch)?;
        let mut blocks = content
            .blocks
            .iter()
            .filter(|block| block.machine_type() == Some(type_id));
        let block = blocks.next().ok_or(RestoreError::BindingMismatch)?;
        if machine_type.save_schema() != 1
            || !canonical(type_id)
            || !canonical(&block.id)
            || type_id.split_once(':').map(|(id, _)| id) != Some(owner.as_str())
            || block.id.split_once(':').map(|(id, _)| id) != Some(owner.as_str())
            || blocks.next().is_some()
            || block.template != "minecraft:iron_block"
            || block.state_plan.is_some()
            || block.properties != horizon_mods::content::BlockProperties::default()
        {
            return Err(RestoreError::BindingMismatch);
        }
        let physical = BlockState {
            name: profile.physical_name(&block.id).into(),
            properties: BTreeMap::new(),
        };
        let state_id = registry
            .id(&physical)
            .ok_or(RestoreError::BindingMismatch)?;
        if profile == NativeBindingProfile::RegisteredCube
            && (registry.default_state(&block.id) != Some(state_id)
                || registry.state(state_id) != Some(&physical)
                || registry.default_state("minecraft:air") == Some(state_id)
                || registry.default_state("minecraft:iron_block") == Some(state_id)
                || (0..registry.len())
                    .filter(|&id| {
                        registry
                            .state(BlockStateId(id as u32))
                            .is_some_and(|s| s.name == block.id)
                    })
                    .count()
                    != 1)
        {
            return Err(RestoreError::BindingMismatch);
        }
        Ok(Self {
            profile,
            guest: None,
            package: *prepared.identity(),
            machine_type: machine_type.clone(),
            block_id: block.id.clone(),
            state_id,
            registry,
        })
    }
    pub fn package_identity(&self) -> [u8; 32] {
        self.package
    }
    pub fn profile(&self) -> NativeBindingProfile {
        self.profile
    }
    pub fn physical_descriptor(&self) -> &BlockState {
        self.registry
            .state(self.state_id)
            .expect("checked immutable binding state")
    }
    pub fn type_id(&self) -> &str {
        self.machine_type.id()
    }
    pub fn block_id(&self) -> &str {
        &self.block_id
    }
    pub fn state_id(&self) -> BlockStateId {
        self.state_id
    }
    pub(super) fn physical_state(&self, state: BlockStateId) -> Option<&BlockState> {
        self.registry.state(state)
    }
    fn symbolic_state(&self) -> BlockState {
        BlockState {
            name: self.block_id.clone(),
            properties: BTreeMap::new(),
        }
    }
    pub(super) fn configuration(
        &self,
        target: &CounterTarget,
        scope: AuthorityScope,
    ) -> Result<Configuration> {
        let configuration = Configuration::new(
            scope,
            &self.machine_type,
            self.symbolic_state(),
            self.state_id,
            target.position,
        )
        .map_err(|_| RestoreError::BindingMismatch)?;
        match &self.guest {
            None => Ok(configuration),
            Some(pair) => configuration
                .with_guest_package(pair)
                .map_err(|_| RestoreError::BindingMismatch),
        }
    }
}

fn nullable<'de, D: de::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> std::result::Result<Option<T>, D::Error> {
    Option::deserialize(d)
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameBinding {
    pub block_id: String,
    pub template: String,
    pub properties: BTreeMap<String, String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameState {
    pub name: String,
    pub properties: BTreeMap<String, String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase", deny_unknown_fields)]
pub enum Placement {
    Active {
        generation: u64,
        counter: u64,
        #[serde(deserialize_with = "nullable")]
        last_advanced: Option<i64>,
    },
    Dormant,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeRetirementReason {
    BlockChanged,
    ChunkReplaced,
    Explicit,
}
impl NativeRetirementReason {
    fn native(self) -> RetirementReason {
        match self {
            Self::BlockChanged => RetirementReason::BlockChanged,
            Self::ChunkReplaced => RetirementReason::ChunkReplaced,
            Self::Explicit => RetirementReason::Explicit,
        }
    }
    fn from_native(reason: RetirementReason) -> Self {
        match reason {
            RetirementReason::BlockChanged => Self::BlockChanged,
            RetirementReason::ChunkReplaced => Self::ChunkReplaced,
            RetirementReason::Explicit => Self::Explicit,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetiredFrame {
    pub generation: u64,
    pub counter: u64,
    #[serde(deserialize_with = "nullable")]
    pub last_advanced: Option<i64>,
    pub reason: NativeRetirementReason,
    pub game_time: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CounterFrameV2 {
    pub format: u32,
    pub kind: String,
    pub world_uuid: WorldId,
    pub dimension: String,
    pub position: [i32; 3],
    pub allocation_high_water: u64,
    pub package_identity: String,
    pub type_id: String,
    pub save_schema: u32,
    pub binding: FrameBinding,
    pub observed_state: FrameState,
    pub saved_game_time: i64,
    pub placement: Placement,
    #[serde(deserialize_with = "nullable")]
    pub last_retired: Option<RetiredFrame>,
}
impl CounterFrameV2 {
    pub fn binding_profile(&self) -> Result<NativeBindingProfile> {
        match self.kind.as_str() {
            "native-counter-checkpoint" => Ok(NativeBindingProfile::LegacyIron),
            "native-registered-counter-checkpoint" => Ok(NativeBindingProfile::RegisteredCube),
            _ => Err(RestoreError::InvalidFrame),
        }
    }
    pub fn validate(&self) -> Result<()> {
        let profile = self.binding_profile()?;
        if self.format != 2 {
            return Err(RestoreError::InvalidFrame);
        }
        self.validate_fields(profile)
    }
    // Common scalar validation only: callers must separately validate their exact header.
    fn validate_fields(&self, profile: NativeBindingProfile) -> Result<()> {
        CounterTarget::new(&self.dimension, self.position)?;
        let generation =
            |generation: u64| generation > 0 && generation <= self.allocation_high_water;
        let payload = |counter: u64, last: Option<i64>, end: i64| {
            (counter == 0) == last.is_none() && last.is_none_or(|time| time >= 0 && time <= end)
        };
        if self.save_schema != 1
            || self.saved_game_time < 0
            || !valid_digest(&self.package_identity)
            || !canonical(&self.type_id)
            || !canonical(&self.binding.block_id)
            || self.type_id.split_once(':').map(|(id, _)| id)
                != self.binding.block_id.split_once(':').map(|(id, _)| id)
            || self.binding.template != "minecraft:iron_block"
            || !self.binding.properties.is_empty()
            || !canonical(&self.observed_state.name)
            || !self.observed_state.properties.is_empty()
        {
            return Err(RestoreError::InvalidFrame);
        }
        if let Placement::Active {
            generation: active,
            counter,
            last_advanced,
        } = self.placement
        {
            if !generation(active)
                || !payload(counter, last_advanced, self.saved_game_time)
                || self.observed_state.name != profile.physical_name(&self.binding.block_id)
                || self
                    .last_retired
                    .as_ref()
                    .is_some_and(|retired| retired.generation >= active)
            {
                return Err(RestoreError::InvalidFrame);
            }
        } else if self.observed_state.name == profile.physical_name(&self.binding.block_id) {
            return Err(RestoreError::InvalidFrame);
        }
        if self.last_retired.as_ref().is_some_and(|retired| {
            !generation(retired.generation)
                || retired.game_time < 0
                || retired.game_time > self.saved_game_time
                || !payload(retired.counter, retired.last_advanced, retired.game_time)
        }) {
            return Err(RestoreError::InvalidFrame);
        }
        Ok(())
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > 16 << 10 {
            return Err(RestoreError::InvalidFrame);
        }
        let mut nodes = 0;
        let mut decoder = serde_json::Deserializer::from_slice(bytes);
        let value = Seed {
            nodes: &mut nodes,
            depth: 0,
        }
        .deserialize(&mut decoder)
        .map_err(|_| RestoreError::InvalidFrame)?;
        decoder.end().map_err(|_| RestoreError::InvalidFrame)?;
        let frame: Self = serde_json::from_value(value).map_err(|_| RestoreError::InvalidFrame)?;
        frame.validate()?;
        Ok(frame)
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| RestoreError::InvalidFrame)?;
        Self::decode(&bytes)?;
        Ok(bytes)
    }
}

struct Seed<'a> {
    nodes: &'a mut usize,
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for Seed<'_> {
    type Value = Value;
    fn deserialize<D: de::Deserializer<'de>>(
        self,
        decoder: D,
    ) -> std::result::Result<Value, D::Error> {
        if self.depth > 8 || *self.nodes >= 512 {
            return Err(de::Error::custom("frame structural bound"));
        }
        *self.nodes += 1;
        decoder.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Seed<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bounded integer frame")
    }
    fn visit_unit<E: de::Error>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Value, E> {
        if v.len() > 256 {
            Err(E::custom("frame string bound"))
        } else {
            Ok(Value::String(v.into()))
        }
    }
    fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Value, E> {
        self.visit_str(&v)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(Seed {
            nodes: &mut *self.nodes,
            depth: self.depth + 1,
        })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Value, A::Error> {
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if key.len() > 256 || values.contains_key(&key) || *self.nodes >= 512 {
                return Err(de::Error::custom("frame key bound"));
            }
            *self.nodes += 1;
            let value = map.next_value_seed(Seed {
                nodes: &mut *self.nodes,
                depth: self.depth + 1,
            })?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacementKey {
    pub world: WorldId,
    pub dimension: String,
    pub position: [i32; 3],
    pub generation: u64,
}
#[derive(Debug)]
struct Session([u8; 16]);
#[derive(Clone, Debug)]
pub struct RuntimeCounterKey {
    session: Arc<Session>,
    placement: PlacementKey,
    instance: u64,
}
impl PartialEq for RuntimeCounterKey {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.session, &other.session)
            && self.placement == other.placement
            && self.instance == other.instance
    }
}
impl Eq for RuntimeCounterKey {}
impl RuntimeCounterKey {
    pub fn placement(&self) -> &PlacementKey {
        &self.placement
    }
    pub fn instance(&self) -> u64 {
        self.instance
    }
    /// Observation only; these bytes cannot construct a runtime key.
    pub fn session_nonce(&self) -> [u8; 16] {
        self.session.0
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DomainStatus {
    Healthy,
    Faulted,
    Detached,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DomainSnapshot {
    pub world: WorldId,
    pub dimension: String,
    pub high_water: u64,
    pub active_key: Option<PlacementKey>,
    pub retired_key: Option<PlacementKey>,
    pub status: DomainStatus,
}
pub(super) struct Domain {
    world: WorldId,
    target: CounterTarget,
    high_water: u64,
    active_key: Option<PlacementKey>,
    retired_key: Option<PlacementKey>,
    session: Arc<Session>,
    status: DomainStatus,
    guest_fault: Option<GuestFaultWitness>,
    binding: NativeBinding,
    pub(super) slot: Option<super::slot_restore::SlotDomain>,
}
pub(super) struct SlotAnchor<'a> {
    pub world: WorldId,
    pub target: &'a CounterTarget,
    pub high_water: u64,
    pub generation: Option<u64>,
    pub binding: &'a NativeBinding,
}
impl Domain {
    pub(super) fn slot_anchor(&self) -> Result<SlotAnchor<'_>> {
        self.healthy()?;
        Ok(SlotAnchor {
            world: self.world,
            target: &self.target,
            high_water: self.high_water,
            generation: self.active_key.as_ref().map(|key| key.generation),
            binding: &self.binding,
        })
    }
    pub(super) fn detach(&mut self) {
        if self.active_key.is_some() {
            self.retired_key = self.active_key.take();
        }
        self.status = DomainStatus::Detached;
    }
    fn healthy(&self) -> Result<()> {
        if self.guest_fault.is_some() {
            return Err(RestoreError::Faulted);
        }
        match self.status {
            DomainStatus::Healthy => Ok(()),
            DomainStatus::Faulted => Err(RestoreError::Faulted),
            DomainStatus::Detached => Err(RestoreError::Detached),
        }
    }
    fn key(&self, generation: u64) -> PlacementKey {
        PlacementKey {
            world: self.world,
            dimension: self.target.dimension.clone(),
            position: self.target.position,
            generation,
        }
    }
    pub(super) fn configuration_matches(&self, configuration: &Configuration) -> bool {
        self.binding.matches_backend(configuration)
            && configuration.scope().dimension() == self.target.dimension
            && configuration.position() == self.target.position
            && configuration.machine_type() == &self.binding.machine_type
            && configuration.state_id() == self.binding.state_id
            && configuration.state() == &self.binding.symbolic_state()
    }
}

impl World {
    /// Bind once during preparation, before publishing the actual initial World.
    /// Supplying the actual dimension is a native-host responsibility; chunks do
    /// not identify their World/dimension. Replacement never rebinds this grant.
    pub fn bind_host_counter_bootstrap(
        &mut self,
        grant: &mut HostCounterBootstrap,
        actual_dimension: &str,
    ) -> Result<()> {
        if grant.bound || grant.spent || self.host_counter_bootstrap.is_some() {
            return Err(RestoreError::Occupied);
        }
        self.vacant_counter_domain()?;
        if self.world_slot_trial.is_some() {
            return Err(RestoreError::Occupied);
        }
        if actual_dimension != "minecraft:overworld" {
            return Err(RestoreError::IdentityMismatch);
        }
        self.host_counter_bootstrap = Some(Arc::downgrade(&grant.marker));
        grant.bound = true;
        Ok(())
    }

    /// Explicit one-use bootstrap only, distinct from random-world/transient
    /// initialization. All fallible work precedes owner publication and spending.
    pub fn initialize_counter_domain_from_host(
        &mut self,
        binding: &NativeBinding,
        target: &CounterTarget,
        grant: &mut HostCounterBootstrap,
    ) -> Result<RuntimeCounterKey> {
        if grant.spent || self.registered_guest_placement.is_some() {
            return Err(RestoreError::Occupied);
        }
        let marker = self
            .host_counter_bootstrap
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .ok_or(RestoreError::IdentityMismatch)?;
        if !grant.bound
            || !Arc::ptr_eq(&marker, &grant.marker)
            || target.dimension != "minecraft:overworld"
        {
            return Err(RestoreError::IdentityMismatch);
        }
        self.vacant_counter_domain()?;
        if self.world_slot_trial.is_some() {
            return Err(RestoreError::Occupied);
        }
        if self.game_time < 0 {
            return Err(RestoreError::ClockMismatch);
        }
        if binding.state_id == self.air || self.restore_current(target)? != binding.state_id {
            return Err(RestoreError::StateMismatch);
        }
        let (session, scope) = Self::fresh_counter_authority(target)?;
        let configuration = binding.configuration(target, scope)?;
        let placement = PlacementKey {
            world: grant.world,
            dimension: target.dimension.clone(),
            position: target.position,
            generation: 1,
        };
        let domain = Box::new(Domain {
            world: grant.world,
            target: target.clone(),
            high_water: 1,
            active_key: Some(placement.clone()),
            retired_key: None,
            session: session.clone(),
            status: DomainStatus::Healthy,
            guest_fault: None,
            binding: binding.clone(),
            slot: None,
        });
        let trial = Box::new(
            Trial::restore_native(configuration, Some((0, None)), None, self.game_time)
                .map_err(|_| RestoreError::RuntimeExhausted)?,
        );
        let runtime = RuntimeCounterKey {
            session,
            placement,
            instance: trial.snapshot().record().unwrap().instance(),
        };
        self.counter_domain = Some(domain);
        self.counter_trial = Some(trial);
        grant.spent = true;
        Ok(runtime)
    }

    pub(super) fn persistent_counter_phase_guard(&mut self) {
        let Some(domain) = self.counter_domain.as_mut() else {
            return;
        };
        let Some(trial) = self.counter_trial.as_mut() else {
            return;
        };
        if domain.status != DomainStatus::Healthy
            || self.game_time < 0
            || trial
                .snapshot()
                .record()
                .and_then(|record| record.last_advanced())
                .is_some_and(|time| time > self.game_time)
        {
            domain.status = DomainStatus::Faulted;
            trial.persistent_fault();
        }
    }
    fn restore_current(&self, target: &CounterTarget) -> Result<BlockStateId> {
        CounterTarget::new(&target.dimension, target.position)?;
        let [x, y, z] = target.position;
        self.chunks
            .get(&(x >> 4, z >> 4))
            .map(|chunk| chunk.block(x & 15, y, z & 15))
            .ok_or(RestoreError::NotResident)
    }
    fn vacant_counter_domain(&self) -> Result<()> {
        if self.counter_trial.is_some() || self.counter_domain.is_some() {
            Err(RestoreError::Occupied)
        } else {
            Ok(())
        }
    }
    fn fresh_counter_authority(target: &CounterTarget) -> Result<(Arc<Session>, AuthorityScope)> {
        let mut nonce = [0; 16];
        horizon_protocol::crypto::random_bytes(&mut nonce).map_err(|_| RestoreError::Entropy)?;
        let mut scalar = [0; 8];
        horizon_protocol::crypto::random_bytes(&mut scalar).map_err(|_| RestoreError::Entropy)?;
        let scalar = u64::from_ne_bytes(scalar);
        if scalar == 0 {
            return Err(RestoreError::Entropy);
        }
        let scope = AuthorityScope::new(scalar, &target.dimension)
            .map_err(|_| RestoreError::InvalidFrame)?;
        Ok((Arc::new(Session(nonce)), scope))
    }
    pub fn initialize_counter_domain(
        &mut self,
        binding: &NativeBinding,
        target: &CounterTarget,
    ) -> Result<RuntimeCounterKey> {
        self.vacant_counter_domain()?;
        if self.game_time < 0 {
            return Err(RestoreError::ClockMismatch);
        }
        if binding.state_id == self.air || self.restore_current(target)? != binding.state_id {
            return Err(RestoreError::StateMismatch);
        }
        let world = WorldId::fresh()?;
        let (session, scope) = Self::fresh_counter_authority(target)?;
        let configuration = binding.configuration(target, scope)?;
        let key = PlacementKey {
            world,
            dimension: target.dimension.clone(),
            position: target.position,
            generation: 1,
        };
        let domain = Box::new(Domain {
            world,
            target: target.clone(),
            high_water: 1,
            active_key: Some(key.clone()),
            retired_key: None,
            session: session.clone(),
            status: DomainStatus::Healthy,
            guest_fault: None,
            binding: binding.clone(),
            slot: None,
        });
        let trial = Box::new(
            Trial::restore_native(configuration, Some((0, None)), None, self.game_time)
                .map_err(|_| RestoreError::RuntimeExhausted)?,
        );
        let runtime = RuntimeCounterKey {
            session,
            placement: key,
            instance: trial.snapshot().record().unwrap().instance(),
        };
        self.counter_domain = Some(domain);
        self.counter_trial = Some(trial);
        Ok(runtime)
    }
    pub fn restore_counter_frame(
        &mut self,
        binding: &NativeBinding,
        expected_world: WorldId,
        target: &CounterTarget,
        frame: &CounterFrameV2,
    ) -> Result<Option<RuntimeCounterKey>> {
        self.vacant_counter_domain()?;
        frame.validate()?;
        if frame.world_uuid != expected_world
            || frame.dimension != target.dimension
            || frame.position != target.position
        {
            return Err(RestoreError::IdentityMismatch);
        }
        if frame.package_identity != digest(&binding.package)
            || frame.binding_profile()? != binding.profile
            || frame.type_id != binding.type_id()
            || frame.binding.block_id != binding.block_id
            || frame.save_schema != binding.machine_type.save_schema()
        {
            return Err(RestoreError::BindingMismatch);
        }
        if self.game_time != frame.saved_game_time {
            return Err(RestoreError::ClockMismatch);
        }
        let current = self.restore_current(target)?;
        let state = binding
            .registry
            .state(current)
            .ok_or(RestoreError::StateMismatch)?;
        if state.name != frame.observed_state.name
            || state.properties != frame.observed_state.properties
            || binding.state_id == self.air
            || matches!(frame.placement, Placement::Active { .. }) && current != binding.state_id
        {
            return Err(RestoreError::StateMismatch);
        }
        let (session, scope) = Self::fresh_counter_authority(target)?;
        let configuration = binding.configuration(target, scope)?;
        let mut domain = Box::new(Domain {
            world: expected_world,
            target: target.clone(),
            high_water: frame.allocation_high_water,
            active_key: None,
            retired_key: None,
            session: session.clone(),
            status: DomainStatus::Healthy,
            guest_fault: None,
            binding: binding.clone(),
            slot: None,
        });
        let active = match frame.placement {
            Placement::Active {
                generation,
                counter,
                last_advanced,
            } => {
                domain.active_key = Some(domain.key(generation));
                Some((counter, last_advanced))
            }
            Placement::Dormant => None,
        };
        let retired = frame.last_retired.as_ref().map(|retired| {
            domain.retired_key = Some(domain.key(retired.generation));
            (
                retired.counter,
                retired.last_advanced,
                retired.reason.native(),
                retired.game_time,
            )
        });
        let trial = Box::new(
            Trial::restore_native(configuration, active, retired, frame.saved_game_time)
                .map_err(|_| RestoreError::RuntimeExhausted)?,
        );
        let runtime = trial.snapshot().record().map(|record| RuntimeCounterKey {
            session,
            placement: domain.active_key.clone().unwrap(),
            instance: record.instance(),
        });
        self.counter_domain = Some(domain);
        self.counter_trial = Some(trial);
        Ok(runtime)
    }
    pub fn counter_domain_snapshot(&self) -> Option<DomainSnapshot> {
        self.counter_domain.as_ref().map(|domain| DomainSnapshot {
            world: domain.world,
            dimension: domain.target.dimension.clone(),
            high_water: domain.high_water,
            active_key: domain.active_key.clone(),
            retired_key: domain.retired_key.clone(),
            status: if domain.guest_fault.is_some() {
                DomainStatus::Faulted
            } else {
                domain.status
            },
        })
    }
    pub fn read_persistent_counter(&self, key: &RuntimeCounterKey) -> Result<Record> {
        let domain = self
            .counter_domain
            .as_ref()
            .ok_or(RestoreError::IdentityMismatch)?;
        domain.healthy()?;
        if !Arc::ptr_eq(&domain.session, &key.session)
            || domain.active_key.as_ref() != Some(&key.placement)
        {
            return Err(RestoreError::IdentityMismatch);
        }
        let snapshot = self
            .counter_trial
            .as_ref()
            .ok_or(RestoreError::Detached)?
            .snapshot();
        if snapshot.fault().is_some() {
            return Err(RestoreError::Faulted);
        }
        if !domain.configuration_matches(snapshot.configuration()) {
            return Err(RestoreError::BindingMismatch);
        }
        let record = snapshot
            .record()
            .filter(|record| record.instance() == key.instance)
            .ok_or(RestoreError::IdentityMismatch)?;
        if !record.resident() {
            return Err(RestoreError::NotResident);
        }
        if self.restore_current(&domain.target)? != domain.binding.state_id {
            return Err(RestoreError::StateMismatch);
        }
        Ok(record)
    }
    pub fn persistent_counter_key(&self) -> Result<Option<RuntimeCounterKey>> {
        let domain = self
            .counter_domain
            .as_ref()
            .ok_or(RestoreError::IdentityMismatch)?;
        domain.healthy()?;
        let snapshot = self
            .counter_trial
            .as_ref()
            .ok_or(RestoreError::Detached)?
            .snapshot();
        if snapshot.fault().is_some() {
            return Err(RestoreError::Faulted);
        }
        if !domain.configuration_matches(snapshot.configuration()) {
            return Err(RestoreError::BindingMismatch);
        }
        let Some(placement) = &domain.active_key else {
            if snapshot.record().is_some() {
                return Err(RestoreError::InvalidFrame);
            }
            return Ok(None);
        };
        let record = snapshot.record().ok_or(RestoreError::InvalidFrame)?;
        let key = RuntimeCounterKey {
            session: domain.session.clone(),
            placement: placement.clone(),
            instance: record.instance(),
        };
        self.read_persistent_counter(&key)?;
        Ok(Some(key))
    }
    pub fn counter_checkpoint_frame(&self, saved_game_time: i64) -> Result<CounterFrameV2> {
        if self
            .counter_domain
            .as_ref()
            .is_some_and(|d| d.binding.is_guest())
        {
            return Err(RestoreError::BindingMismatch);
        }
        let domain = self
            .counter_domain
            .as_ref()
            .ok_or(RestoreError::IdentityMismatch)?;
        domain.healthy()?;
        if saved_game_time != self.game_time {
            return Err(RestoreError::ClockMismatch);
        }
        let snapshot = self
            .counter_trial
            .as_ref()
            .ok_or(RestoreError::Detached)?
            .snapshot();
        if snapshot.fault().is_some() {
            return Err(RestoreError::Faulted);
        }
        if !domain.configuration_matches(snapshot.configuration()) {
            return Err(RestoreError::BindingMismatch);
        }
        let current = self.restore_current(&domain.target)?;
        let state = domain
            .binding
            .registry
            .state(current)
            .ok_or(RestoreError::StateMismatch)?;
        let placement = match (snapshot.record(), &domain.active_key) {
            (Some(record), Some(key))
                if record.resident() && current == domain.binding.state_id =>
            {
                Placement::Active {
                    generation: key.generation,
                    counter: record.counter(),
                    last_advanced: record.last_advanced(),
                }
            }
            (None, None) => Placement::Dormant,
            (Some(record), _) if !record.resident() => return Err(RestoreError::NotResident),
            _ => return Err(RestoreError::InvalidFrame),
        };
        let last_retired = match (snapshot.last_retirement(), &domain.retired_key) {
            (Some(retired), Some(key)) => Some(RetiredFrame {
                generation: key.generation,
                counter: retired.record().counter(),
                last_advanced: retired.record().last_advanced(),
                reason: NativeRetirementReason::from_native(retired.reason()),
                game_time: retired.game_time(),
            }),
            (None, None) => None,
            _ => return Err(RestoreError::InvalidFrame),
        };
        let frame = CounterFrameV2 {
            format: 2,
            kind: domain.binding.profile.counter_kind().into(),
            world_uuid: domain.world,
            dimension: domain.target.dimension.clone(),
            position: domain.target.position,
            allocation_high_water: domain.high_water,
            package_identity: digest(&domain.binding.package),
            type_id: domain.binding.type_id().into(),
            save_schema: 1,
            binding: FrameBinding {
                block_id: domain.binding.block_id.clone(),
                template: "minecraft:iron_block".into(),
                properties: BTreeMap::new(),
            },
            observed_state: FrameState {
                name: state.name.clone(),
                properties: state.properties.clone(),
            },
            saved_game_time,
            placement,
            last_retired,
        };
        frame.validate()?;
        Ok(frame)
    }
    pub(super) fn persistent_counter_changed(&mut self, position: [i32; 3], state: BlockStateId) {
        let Some(domain) = self.counter_domain.as_mut() else {
            return;
        };
        if domain.target.position != position || domain.status != DomainStatus::Healthy {
            return;
        }
        let Some(trial) = self.counter_trial.as_mut() else {
            return;
        };
        if trial.snapshot().fault().is_some() {
            domain.status = DomainStatus::Faulted;
            return;
        }
        if domain.active_key.is_some() {
            trial.retire(RetirementReason::BlockChanged, self.game_time);
            domain.retired_key = domain.active_key.take();
        }
        // Armed Guest placement publishes counter and slot together through its
        // immediate stage. A changed write must not readmit only the counter.
        if self.registered_guest_placement.is_some() {
            return;
        }
        if state == domain.binding.state_id {
            let Some(next) = domain.high_water.checked_add(1) else {
                domain.status = DomainStatus::Faulted;
                trial.persistent_fault();
                return;
            };
            let key = domain.key(next);
            match trial.persistent_admit() {
                Ok(_) => {
                    domain.high_water = next;
                    domain.active_key = Some(key);
                }
                Err(_) => {
                    domain.status = DomainStatus::Faulted;
                    trial.persistent_fault();
                }
            }
        }
    }
    pub(super) fn persistent_counter_inserted(&mut self, replaced: bool, state: BlockStateId) {
        let Some(domain) = self.counter_domain.as_mut() else {
            return;
        };
        let Some(trial) = self.counter_trial.as_mut() else {
            return;
        };
        if domain.status != DomainStatus::Healthy {
            return;
        }
        if replaced
            || (domain.active_key.is_some() && state != domain.binding.state_id)
            || (domain.active_key.is_none() && state == domain.binding.state_id)
        {
            trial.unloaded();
            trial.persistent_fault();
            domain.status = DomainStatus::Faulted;
        } else if domain.active_key.is_some() {
            trial.inserted(false, state, self.game_time);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::Chunk;
    use std::{fs, sync::OnceLock};
    const POSITION: [i32; 3] = [2, 70, 2];
    fn binding() -> NativeBinding {
        static BINDING: OnceLock<NativeBinding> = OnceLock::new();
        BINDING.get_or_init(|| {
            static REGISTRY:OnceLock<BlockRegistry>=OnceLock::new();
            let registry=REGISTRY.get_or_init(||BlockRegistry::from_facts_json(&br#"{"defaults":{"minecraft:air":0,"minecraft:iron_block":1,"minecraft:stone":2},"states":[["air",true,false,"","minecraft:air"],["iron",false,false,"","minecraft:iron_block"],["stone",false,false,"","minecraft:stone"]]}"#[..]).unwrap());
            let prepared=prepared_with_block(br#"{"template":"minecraft:iron_block","machine-type":"counter_demo:counter_type"}"#);
            assert!(NativeBinding::from_prepared(&prepared,"other:counter_type",registry).is_err());
            NativeBinding::from_prepared(&prepared,"counter_demo:counter_type",registry).unwrap()
        }).clone()
    }
    fn prepared_with_block(block: &[u8]) -> PreparedBundle {
        let mut nonce = [0; 16];
        horizon_protocol::crypto::random_bytes(&mut nonce).unwrap();
        let dir = std::env::temp_dir().join(format!(
            "horizon-native-counter-{}-{:x}",
            std::process::id(),
            u128::from_be_bytes(nonce)
        ));
        fs::create_dir(&dir).unwrap();
        for (name,bytes) in [
                ("horizon-paired.toml", &b"format=1\nhost-contract='paired-preview-1'\n[content]\nid='counter_demo'\nname='Counter'\nversion='0'\napi='0.2'\nside='both'\ncapabilities=['registries']\n[client]\nentry='client.wasm'\napi='0.1'\ncapabilities=[]\n[server]\nentry='server.wasm'\napi='0.3'\ncapabilities=[]\n"[..]),
                ("client.wasm",&b"not executed"[..]),("server.wasm",&b"not executed"[..]),
                ("data/counter_demo/horizon/machine/counter_type.json",&br#"{"format":1,"save-schema":1}"#[..]),
                ("data/counter_demo/horizon/block/counter.json",block),
            ] {
                let path=dir.join(name);fs::create_dir_all(path.parent().unwrap()).unwrap();fs::write(path,bytes).unwrap();
            }
        let prepared = PreparedBundle::read_folder(&dir).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        prepared
    }
    fn target() -> CounterTarget {
        CounterTarget::new("minecraft:overworld", POSITION).unwrap()
    }
    fn chunk(state: u32) -> Chunk {
        let mut chunk = Chunk::empty(0, 0);
        chunk.set(2, 70, 2, state);
        chunk
    }
    fn bare(state: u32, time: i64) -> World {
        let mut world = World::new(BlockStateId(0), 17);
        world.game_time = time;
        world.chunks.insert((0, 0), Arc::new(chunk(state)));
        world
    }
    fn active() -> (World, RuntimeCounterKey) {
        let mut world = bare(1, 0);
        let key = world
            .initialize_counter_domain(&binding(), &target())
            .unwrap();
        for time in 1..=3 {
            world.game_time = time;
            assert_eq!(world.advance_counter_trial(true, true), Some(Ok(true)));
        }
        (world, key)
    }
    fn write(world: &mut World, state: u32) {
        world.set(2, 70, 2, BlockStateId(state)).unwrap();
    }

    #[test]
    fn registered_counter_closed_raw_kinds_and_physical_descriptors() {
        let raw = serde_json::json!({
            "format":2,"kind":"native-registered-counter-checkpoint",
            "world_uuid":"12345678-1234-4234-8234-123456789abc",
            "dimension":"minecraft:overworld","position":[2,70,2],
            "allocation_high_water":7,"package_identity":"ab".repeat(32),
            "type_id":"counter_demo:counter_type","save_schema":1,
            "binding":{"block_id":"counter_demo:counter","template":"minecraft:iron_block","properties":{}},
            "observed_state":{"name":"counter_demo:counter","properties":{}},
            "saved_game_time":12,"placement":{"state":"active","generation":7,"counter":0,"last_advanced":null},
            "last_retired":null
        });
        let decode = |value: &Value| CounterFrameV2::decode(&serde_json::to_vec(value).unwrap());
        assert_eq!(
            decode(&raw).unwrap().binding_profile().unwrap(),
            NativeBindingProfile::RegisteredCube
        );
        for mode in 0..7 {
            let mut bad = raw.clone();
            match mode {
                0 => bad["kind"] = "native-registered-counter-checkpoint-vnext".into(),
                1 => bad["kind"] = "native-counter-checkpoint".into(),
                2 => bad["format"] = 3.into(),
                3 => bad["observed_state"]["name"] = "minecraft:iron_block".into(),
                4 => bad["binding"]["template"] = "counter_demo:counter".into(),
                5 => bad["binding"]["properties"] = serde_json::json!({"axis":"x"}),
                _ => bad["placement"]["generation"] = 8.into(),
            }
            assert!(decode(&bad).is_err(), "mode {mode}");
        }
        for observed in ["minecraft:air", "minecraft:iron_block"] {
            let mut dormant = raw.clone();
            dormant["placement"] = serde_json::json!({"state":"dormant"});
            dormant["observed_state"]["name"] = observed.into();
            assert!(matches!(
                decode(&dormant).unwrap().placement,
                Placement::Dormant
            ));
        }
    }

    #[test]
    fn registered_counter_factory_checks_default_propertyless_unique_actual_alias() {
        let prepared = prepared_with_block(
            br#"{"template":"minecraft:iron_block","machine-type":"counter_demo:counter_type"}"#,
        );
        let make = |states: Vec<serde_json::Value>, default: u32| -> &'static BlockRegistry {
            let value = serde_json::json!({"defaults":{"minecraft:air":0,"minecraft:iron_block":1,"counter_demo:counter":default},"states":states});
            Box::leak(Box::new(
                BlockRegistry::from_facts_json(&serde_json::to_vec(&value).unwrap()[..]).unwrap(),
            ))
        };
        let base = vec![
            serde_json::json!(["air", true, false, "", "minecraft:air"]),
            serde_json::json!(["iron", false, false, "", "minecraft:iron_block"]),
        ];
        let mut states = base.clone();
        states.push(serde_json::json!([
            "alias",
            false,
            false,
            "",
            "counter_demo:counter"
        ]));
        let registry = make(states.clone(), 2);
        let actual = NativeBinding::from_registered_prepared(
            &prepared,
            "counter_demo:counter_type",
            registry,
        )
        .unwrap();
        assert_eq!(actual.state_id(), BlockStateId(2));
        assert_eq!(actual.physical_descriptor().name, "counter_demo:counter");
        assert_eq!(
            NativeBinding::from_prepared(&prepared, "counter_demo:counter_type", registry)
                .unwrap()
                .state_id(),
            BlockStateId(1)
        );
        assert!(
            NativeBinding::from_registered_prepared(&prepared, "other:counter_type", registry)
                .is_err()
        );
        assert!(
            NativeBinding::from_registered_prepared(
                &prepared,
                "counter_demo:counter_type",
                binding().registry
            )
            .is_err()
        );
        states.push(serde_json::json!(["extra",false,false,"",{"id":"counter_demo:counter","properties":{"axis":"x"}}]));
        assert!(
            NativeBinding::from_registered_prepared(
                &prepared,
                "counter_demo:counter_type",
                make(states, 2)
            )
            .is_err()
        );
        let mut states = base;
        states.push(serde_json::json!(["property",false,false,"",{"id":"counter_demo:counter","properties":{"axis":"x"}}]));
        assert!(
            NativeBinding::from_registered_prepared(
                &prepared,
                "counter_demo:counter_type",
                make(states, 2)
            )
            .is_err()
        );
        for decl in [br#"{"template":"minecraft:iron_block","machine-type":"counter_demo:counter_type","properties":{"light":15}}"#.as_slice(), br#"{"template":"minecraft:iron_block","machine-type":"counter_demo:counter_type","properties":{"hardness":5.0}}"#.as_slice()] {
            let changed = prepared_with_block(decl);
            assert!(NativeBinding::from_registered_prepared(&changed, "counter_demo:counter_type", registry).is_err());
        }
    }

    #[test]
    fn native_counter_binding_rejects_prepared_declared_property_overrides() {
        let registry = binding().registry;
        for properties in [
            r#"{"light":15}"#,
            r#"{"hardness":5.0}"#,
            r#"{"requires_tool":false}"#,
        ] {
            let block = format!(
                r#"{{"template":"minecraft:iron_block","machine-type":"counter_demo:counter_type","properties":{properties}}}"#
            );
            let prepared = prepared_with_block(block.as_bytes());
            assert_eq!(prepared.content().blocks.len(), 1);
            assert_ne!(
                prepared.content().blocks[0].properties,
                horizon_mods::content::BlockProperties::default()
            );
            assert!(matches!(
                NativeBinding::from_prepared(&prepared, "counter_demo:counter_type", registry),
                Err(RestoreError::BindingMismatch)
            ));
        }
        let prepared=prepared_with_block(br#"{"template":"minecraft:iron_block","machine-type":"counter_demo:counter_type","properties":{}}"#);
        assert!(
            NativeBinding::from_prepared(&prepared, "counter_demo:counter_type", registry).is_ok()
        );
    }

    #[test]
    fn native_counter_frame_roundtrip_restores_nonzero_and_fresh_runtime_authority() {
        let (source, old) = active();
        let frame = source.counter_checkpoint_frame(3).unwrap();
        let frame = CounterFrameV2::decode(&frame.encode().unwrap()).unwrap();
        let mut restored = bare(1, 3);
        let key = restored
            .restore_counter_frame(&binding(), frame.world_uuid, &target(), &frame)
            .unwrap()
            .unwrap();
        assert_eq!(key.placement(), old.placement());
        assert_ne!(key.instance(), old.instance());
        assert_ne!(key.session_nonce(), old.session_nonce());
        assert_ne!(key, old);
        assert_eq!(
            restored.read_persistent_counter(&old),
            Err(RestoreError::IdentityMismatch)
        );
        assert_eq!(
            source.read_persistent_counter(&key),
            Err(RestoreError::IdentityMismatch)
        );
        assert_eq!(restored.read_persistent_counter(&key).unwrap().counter(), 3);
        assert_eq!(restored.advance_counter_trial(true, true), Some(Ok(false)));
        restored.game_time = 4;
        assert_eq!(restored.advance_counter_trial(true, true), Some(Ok(true)));
        assert_eq!(restored.read_persistent_counter(&key).unwrap().counter(), 4);
        assert_eq!(source.read_persistent_counter(&old).unwrap().counter(), 3);
    }
    #[test]
    fn native_counter_frame_codec_requires_nullable_closed_bounded_integer_schema() {
        let (world, _) = active();
        let frame = world.counter_checkpoint_frame(3).unwrap();
        let text = String::from_utf8(frame.encode().unwrap()).unwrap();
        for bad in [
            text.replace("\"last_advanced\":3,", ""),
            text.replace(",\"last_advanced\":3", ""),
            text.replace(",\"last_retired\":null", ""),
            text.replace("\"format\":2", "\"format\":2,\"format\":2"),
            text.replace("\"counter\":3", "\"counter\":18446744073709551616"),
            text.replace("\"counter\":3", "\"counter\":3.0"),
            text.replace("\"format\":2", "\"unknown\":0,\"format\":2"),
            text.replace("native-counter-checkpoint", &"a".repeat(257)),
        ] {
            if bad != text {
                assert!(CounterFrameV2::decode(bad.as_bytes()).is_err(), "{bad}");
            }
        }
        let mut bytes = frame.encode().unwrap();
        bytes.resize((16 << 10) + 1, b' ');
        assert_eq!(
            CounterFrameV2::decode(&bytes),
            Err(RestoreError::InvalidFrame)
        );
        for id in [
            "00000000-0000-0000-0000-000000000000",
            "00000000-0000-4000-7000-000000000000",
            "00000000-0000-4000-8000-00000000000A",
        ] {
            assert!(WorldId::parse(id).is_err());
        }
        assert!(CounterTarget::new("minecraft:../overworld", POSITION).is_err());
    }
    #[test]
    fn native_counter_frame_rejects_each_payload_chronology_and_generation_mismatch() {
        let (world, _) = active();
        let base = world.counter_checkpoint_frame(3).unwrap();
        for (counter, last) in [(0, Some(0)), (1, None), (1, Some(-1)), (1, Some(4))] {
            let mut frame = base.clone();
            frame.placement = Placement::Active {
                generation: 1,
                counter,
                last_advanced: last,
            };
            assert_eq!(frame.validate(), Err(RestoreError::InvalidFrame));
        }
        let mut base = base;
        base.allocation_high_water = 7;
        base.placement = Placement::Active {
            generation: 7,
            counter: 3,
            last_advanced: Some(3),
        };
        for (generation, counter, last, time) in [
            (0, 1, Some(1), 1),
            (7, 1, Some(1), 1),
            (8, 1, Some(1), 1),
            (6, 0, Some(0), 0),
            (6, 1, None, 1),
            (6, 1, Some(-1), 1),
            (6, 1, Some(3), 2),
            (6, 1, Some(1), 4),
            (6, 0, None, -1),
        ] {
            let mut frame = base.clone();
            frame.last_retired = Some(RetiredFrame {
                generation,
                counter,
                last_advanced: last,
                reason: NativeRetirementReason::BlockChanged,
                game_time: time,
            });
            assert_eq!(frame.validate(), Err(RestoreError::InvalidFrame));
        }
        for generation in [0, 8] {
            let mut frame = base.clone();
            frame.placement = Placement::Active {
                generation,
                counter: 3,
                last_advanced: Some(3),
            };
            assert!(frame.validate().is_err());
        }
    }
    #[test]
    fn native_counter_restore_refusals_preserve_owner_storage_clock_and_rng() {
        use horizon_core::random::RandomSource;
        let (source, _) = active();
        let frame = source.counter_checkpoint_frame(3).unwrap();
        for mutation in 0..8 {
            let mut candidate = frame.clone();
            let mut host = target();
            let mut world = bare(1, 3);
            match mutation {
                0 => {
                    candidate.world_uuid =
                        WorldId::parse("00000000-0000-4000-8000-000000000001").unwrap()
                }
                1 => host.dimension = "minecraft:the_nether".into(),
                2 => host.position = [3, 70, 2],
                3 => candidate.package_identity = "0".repeat(64),
                4 => candidate.type_id = "counter_demo:other".into(),
                5 => candidate.binding.block_id = "counter_demo:other".into(),
                6 => world.game_time = 4,
                _ => world = bare(2, 3),
            }
            let chunks = world.chunks.get(&(0, 0)).unwrap().clone();
            let before_time = world.game_time;
            let mut expected_random = bare(1, 3).random;
            let result =
                world.restore_counter_frame(&binding(), frame.world_uuid, &host, &candidate);
            assert!(result.is_err(), "mutation {mutation}");
            assert!(world.counter_domain_snapshot().is_none());
            assert!(world.counter_trial_snapshot().is_none());
            assert_eq!(world.game_time, before_time);
            assert!(Arc::ptr_eq(world.chunks.get(&(0, 0)).unwrap(), &chunks));
            assert_eq!(world.random.next_int(), expected_random.next_int());
        }
        let (mut occupied, key) = active();
        let metadata = occupied.counter_domain_snapshot();
        let snapshot = occupied.counter_trial_snapshot();
        assert_eq!(
            occupied.restore_counter_frame(&binding(), frame.world_uuid, &target(), &frame),
            Err(RestoreError::Occupied)
        );
        assert_eq!(
            occupied.initialize_counter_domain(&binding(), &target()),
            Err(RestoreError::Occupied)
        );
        assert_eq!(occupied.counter_domain_snapshot(), metadata);
        assert_eq!(occupied.counter_trial_snapshot(), snapshot);
        assert_eq!(occupied.read_persistent_counter(&key).unwrap().counter(), 3);
        let mut transient = bare(1, 3);
        let configuration = binding()
            .configuration(
                &target(),
                AuthorityScope::new(9, "minecraft:overworld").unwrap(),
            )
            .unwrap();
        transient.enable_counter_trial(configuration).unwrap();
        let snapshot = transient.counter_trial_snapshot();
        assert_eq!(
            transient.restore_counter_frame(&binding(), frame.world_uuid, &target(), &frame),
            Err(RestoreError::Occupied)
        );
        assert_eq!(transient.counter_trial_snapshot(), snapshot);
    }
    #[test]
    fn native_counter_lifecycle_unload_reload_retains_payload_and_replacement_freezes() {
        let (mut world, key) = active();
        let metadata = world.counter_domain_snapshot();
        world.remove_streamed_chunk(&(0, 0)).unwrap();
        assert_eq!(
            world.read_persistent_counter(&key),
            Err(RestoreError::NotResident)
        );
        assert_eq!(world.advance_counter_trial(true, true), Some(Ok(false)));
        world.insert_streamed_chunk(chunk(1));
        assert_eq!(world.read_persistent_counter(&key).unwrap().counter(), 3);
        assert_eq!(world.counter_domain_snapshot(), metadata);
        world.insert_streamed_chunk(chunk(1));
        let snapshot = world.counter_trial_snapshot().unwrap();
        assert_eq!(snapshot.record().unwrap().counter(), 3);
        assert!(!snapshot.record().unwrap().resident());
        assert_eq!(
            world.read_persistent_counter(&key),
            Err(RestoreError::Faulted)
        );
        assert_eq!(
            world.advance_counter_trial(true, true),
            Some(Err(crate::counter_trial::Fault::PersistentDomain))
        );
        assert_eq!(
            world.counter_checkpoint_frame(3),
            Err(RestoreError::Faulted)
        );
        assert_eq!(
            world.counter_domain_snapshot().unwrap().high_water,
            metadata.unwrap().high_water
        );
        write(&mut world, 0);
        write(&mut world, 1);
        assert_eq!(
            world
                .counter_trial_snapshot()
                .unwrap()
                .record()
                .unwrap()
                .counter(),
            3
        );
        let (mut mismatch, key) = active();
        mismatch.remove_streamed_chunk(&(0, 0));
        mismatch.insert_streamed_chunk(chunk(2));
        assert_eq!(
            mismatch.read_persistent_counter(&key),
            Err(RestoreError::Faulted)
        );
        assert_eq!(
            mismatch
                .counter_trial_snapshot()
                .unwrap()
                .record()
                .unwrap()
                .counter(),
            3
        );
    }
    #[test]
    fn native_counter_retirement_dormant_restore_preserves_head_and_reallocates_above_it() {
        let (mut source, old) = active();
        write(&mut source, 2);
        let dormant = source.counter_checkpoint_frame(3).unwrap();
        assert_eq!(dormant.placement, Placement::Dormant);
        assert_eq!(dormant.last_retired.as_ref().unwrap().counter, 3);
        write(&mut source, 0);
        assert_eq!(
            source.counter_checkpoint_frame(3).unwrap().last_retired,
            dormant.last_retired
        );
        let mut dormant = CounterFrameV2::decode(&dormant.encode().unwrap()).unwrap();
        dormant.allocation_high_water = 12;
        let mut restored = bare(2, 3);
        assert!(
            restored
                .restore_counter_frame(&binding(), dormant.world_uuid, &target(), &dormant)
                .unwrap()
                .is_none()
        );
        assert_eq!(restored.counter_domain_snapshot().unwrap().high_water, 12);
        write(&mut restored, 1);
        let fresh = restored.persistent_counter_key().unwrap().unwrap();
        assert_eq!(fresh.placement().generation, 13);
        assert_ne!(fresh.instance(), old.instance());
        assert_eq!(
            restored.read_persistent_counter(&fresh).unwrap().counter(),
            0
        );
        assert_eq!(
            restored.read_persistent_counter(&old),
            Err(RestoreError::IdentityMismatch)
        );
        assert_eq!(
            restored
                .counter_checkpoint_frame(3)
                .unwrap()
                .last_retired
                .unwrap()
                .counter,
            3
        );
        let mut active = source
            .counter_trial_snapshot()
            .unwrap()
            .configuration()
            .clone();
        assert_eq!(
            restored.enable_counter_trial(active.clone()),
            Err(crate::counter_trial::ConfigError::AlreadyEnabled)
        );
        // The legacy configuration remains a transient type; it cannot bypass the domain guard.
        active = restored
            .counter_trial_snapshot()
            .unwrap()
            .configuration()
            .clone();
        assert_eq!(
            restored.enable_counter_trial(active),
            Err(crate::counter_trial::ConfigError::AlreadyEnabled)
        );
    }
    #[test]
    fn native_counter_generation_exhaustion_keeps_retirement_without_unstamped_zero() {
        let (mut source, _) = active();
        write(&mut source, 0);
        let mut frame = source.counter_checkpoint_frame(3).unwrap();
        frame.allocation_high_water = u64::MAX;
        let mut restored = bare(0, 3);
        restored
            .restore_counter_frame(&binding(), frame.world_uuid, &target(), &frame)
            .unwrap();
        let retired = restored.counter_trial_snapshot().unwrap().last_retirement();
        write(&mut restored, 1);
        assert_eq!(restored.block(2, 70, 2), BlockStateId(1));
        assert!(
            restored
                .counter_trial_snapshot()
                .unwrap()
                .record()
                .is_none()
        );
        assert_eq!(
            restored.counter_trial_snapshot().unwrap().last_retirement(),
            retired
        );
        assert_eq!(
            restored.counter_domain_snapshot().unwrap().high_water,
            u64::MAX
        );
        assert_eq!(
            restored.counter_domain_snapshot().unwrap().status,
            DomainStatus::Faulted
        );
        assert_eq!(
            restored.advance_counter_trial(true, true),
            Some(Err(crate::counter_trial::Fault::PersistentDomain))
        );
    }
    #[test]
    fn native_counter_take_detaches_metadata_and_retains_owned_snapshot() {
        let (mut world, key) = active();
        let frame = world.counter_checkpoint_frame(3).unwrap();
        let configuration = world
            .counter_trial_snapshot()
            .unwrap()
            .configuration()
            .clone();
        let owned = world.take_counter_trial().unwrap();
        assert_eq!(owned.last_retirement().unwrap().record().counter(), 3);
        assert_eq!(
            world.counter_domain_snapshot().unwrap().status,
            DomainStatus::Detached
        );
        assert_eq!(world.counter_domain_snapshot().unwrap().high_water, 1);
        assert_eq!(
            world.read_persistent_counter(&key),
            Err(RestoreError::Detached)
        );
        assert_eq!(
            world.counter_checkpoint_frame(3),
            Err(RestoreError::Detached)
        );
        assert_eq!(
            world.enable_counter_trial(configuration),
            Err(crate::counter_trial::ConfigError::AlreadyEnabled)
        );
        assert_eq!(
            world.restore_counter_frame(&binding(), frame.world_uuid, &target(), &frame),
            Err(RestoreError::Occupied)
        );
        write(&mut world, 0);
        write(&mut world, 1);
        assert!(world.counter_trial_snapshot().is_none());
        assert_eq!(owned.last_retirement().unwrap().record().counter(), 3);
    }
    #[test]
    fn native_counter_restored_maximum_skips_then_faults_without_normalization() {
        let (source, _) = active();
        let mut frame = source.counter_checkpoint_frame(3).unwrap();
        frame.placement = Placement::Active {
            generation: 1,
            counter: u64::MAX,
            last_advanced: Some(3),
        };
        let mut restored = bare(1, 3);
        let key = restored
            .restore_counter_frame(&binding(), frame.world_uuid, &target(), &frame)
            .unwrap()
            .unwrap();
        assert_eq!(restored.advance_counter_trial(true, true), Some(Ok(false)));
        restored.game_time = 4;
        assert_eq!(
            restored.advance_counter_trial(true, true),
            Some(Err(crate::counter_trial::Fault::CounterExhausted))
        );
        assert_eq!(
            restored
                .counter_trial_snapshot()
                .unwrap()
                .record()
                .unwrap()
                .counter(),
            u64::MAX
        );
        assert_eq!(
            restored
                .counter_trial_snapshot()
                .unwrap()
                .record()
                .unwrap()
                .last_advanced(),
            Some(3)
        );
        assert_eq!(
            restored.read_persistent_counter(&key),
            Err(RestoreError::Faulted)
        );
    }
    #[test]
    fn native_counter_active_head_noops_authority_gates_and_empty_dormant_restore() {
        let (source, _) = active();
        let mut frame = source.counter_checkpoint_frame(3).unwrap();
        frame.allocation_high_water = 20;
        let mut restored = bare(1, 3);
        let old = restored
            .restore_counter_frame(&binding(), frame.world_uuid, &target(), &frame)
            .unwrap()
            .unwrap();
        assert_eq!(old.placement().generation, 1);
        let baseline = restored.counter_trial_snapshot();
        let metadata = restored.counter_domain_snapshot();
        write(&mut restored, 1);
        restored.set(3, 70, 2, BlockStateId(2)).unwrap();
        assert_eq!(restored.counter_trial_snapshot(), baseline);
        assert_eq!(restored.counter_domain_snapshot(), metadata);
        restored.game_time = 4;
        assert_eq!(restored.advance_counter_trial(false, true), Some(Ok(false)));
        assert_eq!(restored.advance_counter_trial(true, false), Some(Ok(false)));
        assert_eq!(restored.counter_trial_snapshot(), baseline);
        write(&mut restored, 0);
        write(&mut restored, 1);
        let fresh = restored.persistent_counter_key().unwrap().unwrap();
        assert_eq!(fresh.placement().generation, 21);
        assert_eq!(
            restored.read_persistent_counter(&old),
            Err(RestoreError::IdentityMismatch)
        );
        assert_eq!(
            restored.read_persistent_counter(&fresh).unwrap().counter(),
            0
        );
        let mut empty = frame;
        empty.placement = Placement::Dormant;
        empty.last_retired = None;
        empty.observed_state.name = "minecraft:stone".into();
        empty.allocation_high_water = 0;
        let empty = CounterFrameV2::decode(&empty.encode().unwrap()).unwrap();
        let mut dormant = bare(2, 3);
        assert!(
            dormant
                .restore_counter_frame(&binding(), empty.world_uuid, &target(), &empty)
                .unwrap()
                .is_none()
        );
        assert_eq!(dormant.persistent_counter_key(), Ok(None));
        assert_eq!(dormant.counter_checkpoint_frame(3).unwrap(), empty);
        write(&mut dormant, 1);
        assert_eq!(
            dormant
                .persistent_counter_key()
                .unwrap()
                .unwrap()
                .placement()
                .generation,
            1
        );
    }

    #[derive(Debug, PartialEq)]
    struct HostState {
        domain: Option<DomainSnapshot>,
        trial: Option<crate::counter_trial::Snapshot>,
        clock: (i64, i64),
        revision: u64,
        random: [i32; 2],
        marker: Option<usize>,
        chunks: Vec<((i32, i32), usize, Vec<(usize, Vec<u32>, Vec<u8>)>)>,
        modified: Vec<(i32, i32)>,
    }
    fn host_state(world: &World) -> HostState {
        use horizon_core::random::RandomSource;
        let mut rng = world.random.clone();
        let mut chunks: Vec<_> = world
            .chunks
            .iter()
            .map(|(at, chunk)| {
                (
                    *at,
                    Arc::as_ptr(chunk) as usize,
                    chunk
                        .sections
                        .iter()
                        .map(|section| {
                            (
                                Arc::as_ptr(section) as usize,
                                (0..4096).map(|i| section.get(i)).collect(),
                                (0..4096).map(|i| section.light(i)).collect(),
                            )
                        })
                        .collect(),
                )
            })
            .collect();
        chunks.sort_by_key(|row| row.0);
        let mut modified: Vec<_> = world.modified.iter().copied().collect();
        modified.sort_unstable();
        HostState {
            domain: world.counter_domain_snapshot(),
            trial: world.counter_trial_snapshot(),
            clock: (world.game_time, world.day_time),
            revision: world.revision,
            random: [rng.next_int(), rng.next_int()],
            marker: world
                .host_counter_bootstrap
                .as_ref()
                .map(|weak| weak.as_ptr() as usize),
            chunks,
            modified,
        }
    }
    fn host_grant() -> HostCounterBootstrap {
        HostCounterBootstrap::for_new_host_world(
            WorldId::parse("00000000-0000-4000-8000-000000000101").unwrap(),
        )
    }
    #[test]
    fn host_counter_bootstrap_retained_uuid_one_use_and_fresh_runtime_keys() {
        let mut world = bare(1, 3);
        let mut grant = host_grant();
        world
            .bind_host_counter_bootstrap(&mut grant, "minecraft:overworld")
            .unwrap();
        let key = world
            .initialize_counter_domain_from_host(&binding(), &target(), &mut grant)
            .unwrap();
        assert!(grant.bound && grant.spent);
        assert_eq!(key.placement().world, grant.world);
        assert_eq!(key.placement().generation, 1);
        let record = world.read_persistent_counter(&key).unwrap();
        assert_eq!(
            (record.counter(), record.last_advanced(), record.resident()),
            (0, None, true)
        );
        let metadata = world.counter_domain_snapshot().unwrap();
        assert_eq!(
            (metadata.world, metadata.high_water, metadata.status),
            (grant.world, 1, DomainStatus::Healthy)
        );
        assert_eq!(
            world.counter_checkpoint_frame(3).unwrap().world_uuid,
            grant.world
        );
        let before = host_state(&world);
        assert_eq!(
            world.initialize_counter_domain_from_host(&binding(), &target(), &mut grant),
            Err(RestoreError::Occupied)
        );
        assert_eq!(host_state(&world), before);
        let mut replacement = bare(1, 3);
        assert_eq!(
            replacement.initialize_counter_domain_from_host(&binding(), &target(), &mut grant),
            Err(RestoreError::Occupied)
        );
        assert!(replacement.counter_domain_snapshot().is_none());
        let mut other = bare(1, 3);
        let mut second = HostCounterBootstrap::for_new_host_world(
            WorldId::parse("00000000-0000-4000-8000-000000000102").unwrap(),
        );
        other
            .bind_host_counter_bootstrap(&mut second, "minecraft:overworld")
            .unwrap();
        let fresh = other
            .initialize_counter_domain_from_host(&binding(), &target(), &mut second)
            .unwrap();
        assert_ne!(key.instance(), fresh.instance());
        assert_ne!(key.session_nonce(), fresh.session_nonce());
        assert_eq!(
            other.read_persistent_counter(&key),
            Err(RestoreError::IdentityMismatch)
        );
        assert_eq!(
            world.read_persistent_counter(&fresh),
            Err(RestoreError::IdentityMismatch)
        );
    }
    #[test]
    fn host_counter_bootstrap_live_marker_move_and_replacement_refusals() {
        let mut initial = bare(1, 3);
        let mut grant = host_grant();
        let before = host_state(&initial);
        assert_eq!(
            initial.bind_host_counter_bootstrap(&mut grant, "minecraft:the_nether"),
            Err(RestoreError::IdentityMismatch)
        );
        assert!(!grant.bound && !grant.spent);
        assert_eq!(host_state(&initial), before);
        assert_eq!(
            initial.initialize_counter_domain_from_host(&binding(), &target(), &mut grant),
            Err(RestoreError::IdentityMismatch)
        );
        initial
            .bind_host_counter_bootstrap(&mut grant, "minecraft:overworld")
            .unwrap();
        let marker = grant.marker.clone();
        assert!(Arc::ptr_eq(
            &initial
                .host_counter_bootstrap
                .as_ref()
                .unwrap()
                .upgrade()
                .unwrap(),
            &marker
        ));
        let mut moved = initial;
        let mut replacement = bare(1, 3);
        let before = host_state(&replacement);
        assert_eq!(
            replacement.initialize_counter_domain_from_host(&binding(), &target(), &mut grant),
            Err(RestoreError::IdentityMismatch)
        );
        assert_eq!(
            replacement.bind_host_counter_bootstrap(&mut grant, "minecraft:overworld"),
            Err(RestoreError::Occupied)
        );
        assert!(!grant.spent);
        assert_eq!(host_state(&replacement), before);
        let mut parked = bare(2, 3);
        std::mem::swap(&mut moved, &mut parked);
        assert_eq!(
            moved.initialize_counter_domain_from_host(&binding(), &target(), &mut grant),
            Err(RestoreError::IdentityMismatch)
        );
        std::mem::swap(&mut moved, &mut parked);
        let key = moved
            .initialize_counter_domain_from_host(&binding(), &target(), &mut grant)
            .unwrap();
        assert_eq!(key.placement().world, grant.world);
        let mut expired = bare(1, 3);
        let mut old = host_grant();
        expired
            .bind_host_counter_bootstrap(&mut old, "minecraft:overworld")
            .unwrap();
        drop(old);
        assert!(
            expired
                .host_counter_bootstrap
                .as_ref()
                .unwrap()
                .upgrade()
                .is_none()
        );
        let mut new = host_grant();
        let before = host_state(&expired);
        assert_eq!(
            expired.bind_host_counter_bootstrap(&mut new, "minecraft:overworld"),
            Err(RestoreError::Occupied)
        );
        assert_eq!(
            expired.initialize_counter_domain_from_host(&binding(), &target(), &mut new),
            Err(RestoreError::IdentityMismatch)
        );
        assert_eq!(host_state(&expired), before);
        assert!(!new.bound && !new.spent);
    }
    #[test]
    fn host_counter_bootstrap_invalid_inputs_preserve_unused_grant_and_world() {
        for case in [
            "clock",
            "state",
            "absent",
            "position",
            "dimension",
            "air-binding",
            "invalid-binding",
        ] {
            let mut world = bare(1, 3);
            let mut grant = host_grant();
            world
                .bind_host_counter_bootstrap(&mut grant, "minecraft:overworld")
                .unwrap();
            let mut binding = binding();
            let mut target = target();
            let expected = match case {
                "clock" => {
                    world.game_time = -1;
                    RestoreError::ClockMismatch
                }
                "state" => {
                    write(&mut world, 2);
                    RestoreError::StateMismatch
                }
                "absent" => {
                    world.chunks.remove(&(0, 0));
                    RestoreError::NotResident
                }
                "position" => {
                    target.position[1] = 500;
                    RestoreError::InvalidFrame
                }
                "dimension" => {
                    target.dimension = "minecraft:the_nether".into();
                    RestoreError::IdentityMismatch
                }
                "air-binding" => {
                    binding.state_id = world.air;
                    RestoreError::StateMismatch
                }
                "invalid-binding" => {
                    binding.block_id = "bad".into();
                    RestoreError::BindingMismatch
                }
                _ => unreachable!(),
            };
            let before = host_state(&world);
            let strong = Arc::strong_count(&grant.marker);
            assert_eq!(
                world.initialize_counter_domain_from_host(&binding, &target, &mut grant),
                Err(expected),
                "{case}"
            );
            assert_eq!(host_state(&world), before, "{case}");
            assert!(!grant.spent && grant.bound);
            assert_eq!(Arc::strong_count(&grant.marker), strong);
        }
    }
    #[test]
    fn host_counter_bootstrap_existing_owners_and_heads_refuse_unchanged() {
        for mode in [
            "matching",
            "different",
            "dormant",
            "faulted",
            "detached",
            "transient",
        ] {
            let mut world = bare(if mode == "dormant" { 2 } else { 1 }, 3);
            let mut grant = host_grant();
            world
                .bind_host_counter_bootstrap(&mut grant, "minecraft:overworld")
                .unwrap();
            let mut held = None;
            if mode == "transient" {
                let config = binding()
                    .configuration(
                        &target(),
                        AuthorityScope::new(17, "minecraft:overworld").unwrap(),
                    )
                    .unwrap();
                world.enable_counter_trial(config).unwrap();
            } else {
                let (source, _) = active();
                let mut frame = source.counter_checkpoint_frame(3).unwrap();
                if mode != "different" {
                    frame.world_uuid = grant.world;
                }
                if mode == "dormant" {
                    frame.placement = Placement::Dormant;
                    frame.observed_state.name = "minecraft:stone".into();
                }
                world
                    .restore_counter_frame(&binding(), frame.world_uuid, &target(), &frame)
                    .unwrap();
                if mode == "faulted" {
                    world.game_time = 2;
                    world.advance_counter_trial(true, true);
                }
                if mode == "detached" {
                    held = world.take_counter_trial();
                }
            }
            let before = host_state(&world);
            let held_before = held.clone();
            assert_eq!(
                world.initialize_counter_domain_from_host(&binding(), &target(), &mut grant),
                Err(RestoreError::Occupied),
                "{mode}"
            );
            assert_eq!(host_state(&world), before, "{mode}");
            assert_eq!(held, held_before);
            assert!(!grant.spent);
        }
    }
    #[test]
    fn host_counter_bootstrap_random_and_transient_defaults_stay_distinct() {
        let mut random = bare(1, 3);
        assert!(random.host_counter_bootstrap.is_none());
        let key = random
            .initialize_counter_domain(&binding(), &target())
            .unwrap();
        assert!(random.host_counter_bootstrap.is_none());
        assert_eq!(
            random.counter_checkpoint_frame(3).unwrap().world_uuid,
            key.placement().world
        );
        let mut transient = bare(1, 3);
        let config = binding()
            .configuration(
                &target(),
                AuthorityScope::new(27, "minecraft:overworld").unwrap(),
            )
            .unwrap();
        let snapshot = transient.enable_counter_trial(config).unwrap();
        assert_eq!(snapshot.record().unwrap().counter(), 0);
        assert!(
            transient.counter_domain_snapshot().is_none()
                && transient.host_counter_bootstrap.is_none()
        );
        let mut grant = host_grant();
        let before = host_state(&transient);
        assert_eq!(
            transient.bind_host_counter_bootstrap(&mut grant, "minecraft:overworld"),
            Err(RestoreError::Occupied)
        );
        assert_eq!(host_state(&transient), before);
        assert!(!grant.bound && !grant.spent);
    }
    // Host-only defenses: metadata fixtures deliberately never execute their
    // placeholder component bytes. Actual Wasm families live in Game tests.
    #[cfg(feature = "mods")]
    mod persistent_guest_host_tests {
        use super::*;
        use crate::counter_trial::{Completion, Fault, GuestFailure, PhaseError, Proposal};
        fn guest_binding() -> NativeBinding {
            let prepared = Arc::new(prepared_with_block(br#"{"template":"minecraft:iron_block","machine-type":"counter_demo:counter_type"}"#));
            let registry = Box::leak(Box::new(BlockRegistry::from_facts_json(&br#"{"defaults":{"minecraft:air":0,"minecraft:iron_block":1,"counter_demo:counter":2},"states":[["air",true,false,"","minecraft:air"],["iron",false,false,"","minecraft:iron_block"],["alias",false,false,"","counter_demo:counter"]]}"#[..]).unwrap()));
            NativeBinding::from_registered_guest_prepared(
                &prepared,
                "counter_demo:counter_type",
                registry,
            )
            .unwrap()
        }
        fn host_guest() -> World {
            let binding = guest_binding();
            let mut world = bare(2, 0);
            let mut grant = host_grant();
            world
                .bind_host_counter_bootstrap(&mut grant, "minecraft:overworld")
                .unwrap();
            world
                .initialize_registered_guest_pair_from_host(&binding, &target(), &mut grant)
                .unwrap();
            world
        }
        #[test]
        fn registered_guest_placement_stages_grant_pair_and_independent_heads_before_storage() {
            use super::super::super::slot_restore::{SlotAllocation, SlotStatus};
            let binding = guest_binding();
            let mut world = bare(0, 0);
            let mut grant = host_grant();
            let id = grant.world;
            world
                .bind_host_counter_bootstrap(&mut grant, "minecraft:overworld")
                .unwrap();
            world
                .install_registered_guest_placement_policy(&binding)
                .unwrap();
            let before = host_state(&world);
            assert!(
                world
                    .set(POSITION[0], POSITION[1], POSITION[2], binding.state_id())
                    .is_err()
            );
            let mut other = host_grant();
            assert!(
                world
                    .set_from_registered_guest_host_level(
                        POSITION,
                        binding.state_id(),
                        "minecraft:overworld",
                        Some(id),
                        Some(&mut other),
                    )
                    .is_err()
            );
            assert!(
                world
                    .set_from_registered_guest_host_level(
                        POSITION,
                        binding.state_id(),
                        "minecraft:overworld",
                        None,
                        Some(&mut grant),
                    )
                    .is_err()
            );
            assert_eq!(host_state(&world), before);
            assert!(!grant.spent && !other.spent);
            world
                .set_from_registered_guest_host_level(
                    POSITION,
                    binding.state_id(),
                    "minecraft:overworld",
                    Some(id),
                    Some(&mut grant),
                )
                .unwrap();
            assert!(grant.spent);
            assert_eq!(
                world.block(POSITION[0], POSITION[1], POSITION[2]),
                binding.state_id()
            );
            assert_eq!(world.counter_domain_snapshot().unwrap().high_water, 1);
            let metadata = world.persistent_world_slot_metadata().unwrap();
            assert_eq!(metadata.high_water, 1);
            assert_eq!(metadata.status, SlotStatus::Healthy);
            assert_eq!(
                metadata.allocation,
                SlotAllocation::Active {
                    slot_generation: 1,
                    machine_generation: 1
                }
            );
            let first = world.counter_trial_snapshot().unwrap().record().unwrap();
            let after = host_state(&world);
            assert!(
                world
                    .set_from_registered_guest_host_level(
                        [POSITION[0] + 1, POSITION[1], POSITION[2]],
                        binding.state_id(),
                        "minecraft:overworld",
                        Some(id),
                        Some(&mut grant),
                    )
                    .is_err()
            );
            assert_eq!(host_state(&world), after);
            world
                .set(POSITION[0], POSITION[1], POSITION[2], world.air)
                .unwrap();
            let pending = world
                .world_slot_snapshot()
                .unwrap()
                .pending
                .unwrap()
                .allocation
                .key;
            let retired = world.counter_trial_snapshot().unwrap().last_retirement();
            let before = host_state(&world);
            let slot_before = world.persistent_world_slot_metadata();
            assert!(
                world
                    .set_from_registered_guest_host_level(
                        POSITION,
                        binding.state_id(),
                        "minecraft:overworld",
                        Some(id),
                        None,
                    )
                    .is_err()
            );
            assert_eq!(host_state(&world), before);
            assert_eq!(world.persistent_world_slot_metadata(), slot_before);
            let original = world
                .take_persistent_world_slot_retirement(&pending)
                .unwrap();
            assert!(
                original
                    .into_parts()
                    .0
                    .into_parts()
                    .1
                    .into_stack()
                    .is_none()
            );
            world
                .set_from_registered_guest_host_level(
                    POSITION,
                    binding.state_id(),
                    "minecraft:overworld",
                    Some(id),
                    None,
                )
                .unwrap();
            assert_eq!(world.counter_domain_snapshot().unwrap().high_water, 2);
            assert_eq!(
                world.persistent_world_slot_metadata().unwrap().high_water,
                2
            );
            let current = world.counter_trial_snapshot().unwrap();
            assert_ne!(current.record().unwrap().instance(), first.instance());
            assert_eq!(current.last_retirement(), retired);

            // A consistent, independently exhausted slot head must refuse before
            // either head, its retained retirement or native storage changes.
            world
                .set(POSITION[0], POSITION[1], POSITION[2], world.air)
                .unwrap();
            let pending = world
                .world_slot_snapshot()
                .unwrap()
                .pending
                .unwrap()
                .allocation
                .key;
            world
                .take_persistent_world_slot_retirement(&pending)
                .unwrap();
            let configuration = world
                .counter_trial
                .as_ref()
                .unwrap()
                .configuration()
                .clone();
            world.world_slot_trial = Some(Box::new(
                crate::world_slot_trial::Trial::restore_guest_checked(
                    configuration,
                    u64::MAX,
                    &mut None,
                )
                .unwrap(),
            ));
            world
                .counter_domain
                .as_mut()
                .unwrap()
                .slot
                .as_mut()
                .unwrap()
                .high_water = u64::MAX;
            let before = host_state(&world);
            let slot_before = world.persistent_world_slot_metadata();
            let error = world
                .set_from_registered_guest_host_level(
                    POSITION,
                    binding.state_id(),
                    "minecraft:overworld",
                    Some(id),
                    None,
                )
                .unwrap_err();
            assert_eq!(
                error.downcast_ref::<RestoreError>(),
                Some(&RestoreError::GenerationExhausted)
            );
            assert_eq!(host_state(&world), before);
            assert_eq!(world.persistent_world_slot_metadata(), slot_before);
            assert!(grant.spent);
        }
        #[test]
        fn persistent_guest_host_invalid_reply_preserves_record_and_first_fault() {
            for field in 0..5 {
                let mut world = host_guest();
                world.game_time = 1;
                let token = world
                    .prepare_persistent_guest_counter_phase(true, true)
                    .unwrap()
                    .unwrap();
                let phase = token.phase();
                let record = phase.record();
                let mut proposal = Proposal {
                    world_instance: phase.configuration().scope().world_instance(),
                    instance: record.instance(),
                    game_time: phase.game_time(),
                    expected_counter: record.counter(),
                    next_counter: record.counter() + 1,
                };
                match field {
                    0 => proposal.world_instance += 1,
                    1 => proposal.instance += 1,
                    2 => proposal.game_time += 1,
                    3 => proposal.expected_counter += 1,
                    _ => proposal.next_counter += 1,
                }
                assert_eq!(
                    world.commit_persistent_guest_counter_phase(token, proposal, true, true),
                    Err(PhaseError::Fault(Fault::GuestFailure(
                        GuestFailure::InvalidReply
                    )))
                );
                assert_eq!(
                    world.counter_trial_snapshot().unwrap().record(),
                    Some(record)
                );
                let status = GuestStatus::Faulted {
                    code: GuestFaultCode::GuestInvalidReply,
                    observed_tick: 1,
                };
                assert_eq!(
                    world
                        .guest_storage_view()
                        .unwrap()
                        .counter()
                        .execution
                        .status,
                    status
                );
                world.game_time = 2;
                assert!(matches!(
                    world.prepare_persistent_guest_counter_phase(true, true),
                    Err(PhaseError::Fault(Fault::GuestFailure(
                        GuestFailure::InvalidReply
                    )))
                ));
                assert_eq!(
                    world
                        .guest_storage_view()
                        .unwrap()
                        .counter()
                        .execution
                        .status,
                    status
                );
                assert_eq!(
                    world.counter_trial_snapshot().unwrap().record(),
                    Some(record)
                );
            }
        }
        #[test]
        fn persistent_guest_host_exhaustion_is_real_factory_state_and_stops_before_callback() {
            let mut world = host_guest();
            let config = world
                .counter_trial
                .as_ref()
                .unwrap()
                .configuration()
                .clone();
            world.counter_trial = Some(Box::new(
                Trial::restore_persistent_guest(config, Some((u64::MAX, Some(0))), None, 0, None)
                    .unwrap(),
            ));
            let record = world.counter_trial_snapshot().unwrap().record().unwrap();
            assert!(world.persistent_counter_key().unwrap().is_some());
            world.game_time = 1;
            assert!(matches!(
                world.prepare_persistent_guest_counter_phase(true, true),
                Err(PhaseError::Fault(Fault::CounterExhausted))
            ));
            assert_eq!(
                world.counter_trial_snapshot().unwrap().record(),
                Some(record)
            );
            let saved = world.guest_storage_view().unwrap().counter().clone();
            assert_eq!(
                saved.execution.status,
                GuestStatus::Faulted {
                    code: GuestFaultCode::CounterExhausted,
                    observed_tick: 1
                }
            );
            assert!(matches!(
                saved.fields.placement,
                Placement::Active {
                    counter: u64::MAX,
                    last_advanced: Some(0),
                    ..
                }
            ));
            assert_eq!(
                GuestCounterFrameV3::decode(&saved.encode().unwrap()).unwrap(),
                saved
            );
            world.game_time = 2;
            assert!(matches!(
                world.prepare_persistent_guest_counter_phase(true, true),
                Err(PhaseError::Fault(Fault::CounterExhausted))
            ));
            assert_eq!(
                world.counter_trial_snapshot().unwrap().record(),
                Some(record)
            );
        }
        #[test]
        fn persistent_guest_host_storage_refuses_fault_witness_and_structural_mismatch() {
            for mode in 0..6 {
                let mut world = host_guest();
                world.game_time = 1;
                let token = world
                    .prepare_persistent_guest_counter_phase(true, true)
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    world.fail_persistent_guest_counter_phase(
                        token,
                        GuestFailure::Trap,
                        true,
                        true
                    ),
                    Err(PhaseError::Fault(Fault::GuestFailure(GuestFailure::Trap)))
                );
                assert!(world.guest_storage_view().is_ok());
                let record = world.counter_trial_snapshot().unwrap().record();
                let domain = world.counter_domain.as_mut().unwrap();
                match mode {
                    0 => {
                        domain.guest_fault.as_mut().unwrap().status = GuestStatus::Faulted {
                            code: GuestFaultCode::GuestFuel,
                            observed_tick: 1,
                        }
                    }
                    1 => {
                        domain.guest_fault.as_mut().unwrap().status = GuestStatus::Faulted {
                            code: GuestFaultCode::GuestTrap,
                            observed_tick: 2,
                        }
                    }
                    2 => domain.guest_fault.as_mut().unwrap().key.instance += 1,
                    3 => domain.status = DomainStatus::Faulted,
                    4 => domain.slot.as_mut().unwrap().high_water += 1,
                    _ => domain.guest_fault = None,
                }
                assert!(world.guest_storage_view().is_err(), "mode {mode}");
                assert!(world.guest_runtime_state().is_err(), "mode {mode}");
                assert!(
                    world
                        .prepare_persistent_guest_counter_phase(true, true)
                        .is_err()
                );
                assert_eq!(world.counter_trial_snapshot().unwrap().record(), record);
                assert_eq!(
                    world.counter_trial_snapshot().unwrap().fault(),
                    Some(Fault::GuestFailure(GuestFailure::Trap))
                );
            }
        }
        #[test]
        fn persistent_guest_host_stale_foreign_world_does_not_fault_owner() {
            let mut source = host_guest();
            source.game_time = 1;
            let success = source
                .prepare_persistent_guest_counter_phase(true, true)
                .unwrap()
                .unwrap();
            let phase = success.phase();
            let proposal = Proposal {
                world_instance: phase.configuration().scope().world_instance(),
                instance: phase.record().instance(),
                game_time: phase.game_time(),
                expected_counter: 0,
                next_counter: 1,
            };
            let failure = source
                .prepare_persistent_guest_counter_phase(true, true)
                .unwrap()
                .unwrap();
            let mut other = host_guest();
            other.game_time = 1;
            let before = other.counter_trial_snapshot().unwrap();
            assert_eq!(
                other.commit_persistent_guest_counter_phase(success, proposal, true, true),
                Ok(Completion::Stale)
            );
            assert_eq!(
                other.fail_persistent_guest_counter_phase(failure, GuestFailure::Trap, true, true),
                Ok(Completion::Stale)
            );
            assert_eq!(other.counter_trial_snapshot().unwrap(), before);
            assert_eq!(
                other
                    .guest_storage_view()
                    .unwrap()
                    .counter()
                    .execution
                    .status,
                GuestStatus::Ready
            );
        }
    }
}

// Explicit persistent Guest execution. Existing Native frame headers remain unchanged.
pub const GUEST_COUNTER_KIND: &str = "registered-guest-counter-checkpoint";
pub const GUEST_SLOT_KIND: &str = "registered-guest-slot-checkpoint";
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GuestFaultCode {
    GuestTrap,
    GuestFuel,
    GuestMemory,
    GuestRejected,
    GuestInvalidReply,
    GuestUnavailable,
    GuestEvaluation,
    CounterExhausted,
}
impl GuestFaultCode {
    fn from_fault(fault: crate::counter_trial::Fault) -> Result<Self> {
        use crate::counter_trial::{Fault, GuestFailure as G};
        Ok(match fault {
            Fault::GuestFailure(G::Trap) => Self::GuestTrap,
            Fault::GuestFailure(G::Fuel) => Self::GuestFuel,
            Fault::GuestFailure(G::Memory) => Self::GuestMemory,
            Fault::GuestFailure(G::Rejected) => Self::GuestRejected,
            Fault::GuestFailure(G::InvalidReply) => Self::GuestInvalidReply,
            Fault::GuestFailure(G::Unavailable) => Self::GuestUnavailable,
            Fault::GuestFailure(G::Evaluation) => Self::GuestEvaluation,
            Fault::CounterExhausted => Self::CounterExhausted,
            _ => return Err(RestoreError::Faulted),
        })
    }
    pub fn fault(self) -> crate::counter_trial::Fault {
        use crate::counter_trial::{Fault, GuestFailure as G};
        match self {
            Self::GuestTrap => Fault::GuestFailure(G::Trap),
            Self::GuestFuel => Fault::GuestFailure(G::Fuel),
            Self::GuestMemory => Fault::GuestFailure(G::Memory),
            Self::GuestRejected => Fault::GuestFailure(G::Rejected),
            Self::GuestInvalidReply => Fault::GuestFailure(G::InvalidReply),
            Self::GuestUnavailable => Fault::GuestFailure(G::Unavailable),
            Self::GuestEvaluation => Fault::GuestFailure(G::Evaluation),
            Self::CounterExhausted => Fault::CounterExhausted,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase", deny_unknown_fields)]
pub enum GuestStatus {
    Ready,
    Faulted {
        code: GuestFaultCode,
        observed_tick: i64,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestExecution {
    pub profile: String,
    pub primary_owner: String,
    pub status: GuestStatus,
}
/// The reused field shape is NOT a valid Native frame: header3 and Guest kind
/// are validated here and never passed to Native restoration/encoding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestCounterFrameV3 {
    pub fields: CounterFrameV2,
    pub execution: GuestExecution,
}
impl GuestCounterFrameV3 {
    pub fn validate(&self) -> Result<()> {
        let f = &self.fields;
        if f.format != 3
            || f.kind != GUEST_COUNTER_KIND
            || self.execution.profile != "registered-guest-v1"
            || f.type_id.split_once(':').map(|(owner, _)| owner)
                != Some(self.execution.primary_owner.as_str())
        {
            return Err(RestoreError::InvalidFrame);
        }
        f.validate_fields(NativeBindingProfile::RegisteredCube)?;
        if f.allocation_high_water == 0
            || (matches!(f.placement, Placement::Dormant) && f.last_retired.is_none())
        {
            return Err(RestoreError::InvalidFrame);
        }
        if let GuestStatus::Faulted {
            code,
            observed_tick,
        } = self.execution.status
        {
            let Placement::Active {
                counter,
                last_advanced,
                ..
            } = f.placement
            else {
                return Err(RestoreError::InvalidFrame);
            };
            if observed_tick < 0
                || observed_tick > f.saved_game_time
                || last_advanced.is_some_and(|last| last >= observed_tick)
                || (code == GuestFaultCode::CounterExhausted && counter != u64::MAX)
            {
                return Err(RestoreError::InvalidFrame);
            }
        }
        Ok(())
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > 16 << 10 {
            return Err(RestoreError::InvalidFrame);
        }
        let mut nodes = 0;
        let mut decoder = serde_json::Deserializer::from_slice(bytes);
        let mut value = Seed {
            nodes: &mut nodes,
            depth: 0,
        }
        .deserialize(&mut decoder)
        .map_err(|_| RestoreError::InvalidFrame)?;
        decoder.end().map_err(|_| RestoreError::InvalidFrame)?;
        let execution = value
            .as_object_mut()
            .and_then(|v| v.remove("execution"))
            .ok_or(RestoreError::InvalidFrame)?;
        let frame = Self {
            fields: serde_json::from_value(value).map_err(|_| RestoreError::InvalidFrame)?,
            execution: serde_json::from_value(execution).map_err(|_| RestoreError::InvalidFrame)?,
        };
        frame.validate()?;
        Ok(frame)
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut value =
            serde_json::to_value(&self.fields).map_err(|_| RestoreError::InvalidFrame)?;
        value
            .as_object_mut()
            .ok_or(RestoreError::InvalidFrame)?
            .insert(
                "execution".into(),
                serde_json::to_value(&self.execution).map_err(|_| RestoreError::InvalidFrame)?,
            );
        let bytes = serde_json::to_vec(&value).map_err(|_| RestoreError::InvalidFrame)?;
        Self::decode(&bytes)?;
        Ok(bytes)
    }
}
struct GuestFaultWitness {
    key: RuntimeCounterKey,
    record: Record,
    status: GuestStatus,
}
impl NativeBinding {
    pub fn from_registered_guest_prepared(
        pair: &Arc<PreparedBundle>,
        type_id: &str,
        registry: &'static BlockRegistry,
    ) -> Result<Self> {
        if !cfg!(feature = "mods") {
            return Err(RestoreError::BindingMismatch);
        }
        let mut binding = Self::from_registered_prepared(pair, type_id, registry)?;
        binding.guest = Some(pair.clone());
        Ok(binding)
    }
    pub fn is_guest(&self) -> bool {
        self.guest.is_some()
    }
    pub fn guest_owner(&self) -> Option<&str> {
        self.guest
            .as_ref()
            .map(|p| p.bundle().content_owner().id.as_str())
    }
    fn matches_backend(&self, configuration: &Configuration) -> bool {
        use crate::counter_trial::Backend;
        match (&self.guest, configuration.backend()) {
            (None, Backend::Native) => true,
            (
                Some(pair),
                Backend::Guest {
                    primary_owner,
                    package_identity,
                },
            ) => {
                primary_owner == &pair.bundle().content_owner().id
                    && package_identity == pair.identity()
            }
            _ => false,
        }
    }
}
/// No healthy runtime key is exposed by faulted storage.
pub struct GuestStorageReceipt {
    key: RuntimeCounterKey,
    slot: crate::world_slot_trial::SlotKey,
    revision: u64,
    status: GuestStatus,
}
pub enum GuestRuntimeState {
    ReadyActive {
        counter: RuntimeCounterKey,
        slot: crate::world_slot_trial::SlotKey,
    },
    FaultedActive {
        receipt: GuestStorageReceipt,
    },
    ReadyDormant,
}
pub struct GuestStorageView<'a> {
    world: &'a World,
    frame: GuestCounterFrameV3,
}
impl GuestStorageView<'_> {
    pub fn counter(&self) -> &GuestCounterFrameV3 {
        &self.frame
    }
    /// Original borrowed native value, never a cloned owner or transfer capability.
    pub fn stack(&self) -> Option<&crate::items::ItemStack> {
        self.world
            .world_slot_trial
            .as_ref()?
            .borrowed()
            .active?
            .stack
    }
    pub fn slot_bytes(
        &self,
        items: &crate::items::Items,
    ) -> std::result::Result<Vec<u8>, super::slot_restore::SlotRestoreError> {
        self.world
            .guest_slot_checkpoint_bytes(items, self.world.game_time)
    }
}
pub struct PersistentGuestPhaseToken {
    key: RuntimeCounterKey,
    slot: crate::world_slot_trial::SlotKey,
    revision: u64,
    column: Arc<crate::chunk::Chunk>,
    phase: crate::counter_trial::PhaseToken,
}
impl PersistentGuestPhaseToken {
    pub fn phase(&self) -> &crate::counter_trial::PhaseToken {
        &self.phase
    }
}
struct PreparedGuestPair {
    domain: Box<Domain>,
    counter: Box<Trial>,
    slot: Box<crate::world_slot_trial::Trial>,
    key: RuntimeCounterKey,
    slot_key: crate::world_slot_trial::SlotKey,
}
fn same_guest_binding(left: &NativeBinding, right: &NativeBinding) -> bool {
    left.profile == NativeBindingProfile::RegisteredCube
        && right.profile == NativeBindingProfile::RegisteredCube
        && left.is_guest()
        && right.is_guest()
        && std::ptr::eq(left.registry, right.registry)
        && left.package == right.package
        && left.type_id() == right.type_id()
        && left.block_id == right.block_id
        && left.state_id == right.state_id
}
enum PreparedGuestPlacement {
    First(PreparedGuestPair),
    Readmit {
        counter: crate::counter_trial::PersistentAdmission,
        slot: crate::world_slot_trial::OwnedSlot,
        placement: PlacementKey,
        counter_generation: u64,
        slot_generation: u64,
    },
}
impl World {
    pub fn has_persistent_guest_counter(&self) -> bool {
        self.counter_domain
            .as_ref()
            .is_some_and(|d| d.binding.is_guest())
    }
    pub fn initialize_registered_guest_pair_from_host(
        &mut self,
        binding: &NativeBinding,
        target: &CounterTarget,
        grant: &mut HostCounterBootstrap,
    ) -> Result<(RuntimeCounterKey, crate::world_slot_trial::SlotKey)> {
        if self.registered_guest_placement.is_some() {
            return Err(RestoreError::Occupied);
        }
        let staged =
            self.prepare_registered_guest_pair(binding, target, grant, binding.state_id)?;
        let result = (staged.key.clone(), staged.slot_key.clone());
        self.publish_registered_guest_pair(staged, grant);
        Ok(result)
    }
    fn prepare_registered_guest_pair(
        &self,
        binding: &NativeBinding,
        target: &CounterTarget,
        grant: &HostCounterBootstrap,
        expected_current: BlockStateId,
    ) -> Result<PreparedGuestPair> {
        use super::slot_restore::{SlotAllocation, SlotMetadataSnapshot, SlotStatus};
        if !binding.is_guest()
            || grant.spent
            || !grant.bound
            || target.dimension != "minecraft:overworld"
            || self.game_time < 0
        {
            return Err(RestoreError::IdentityMismatch);
        }
        let marker = self
            .host_counter_bootstrap
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .ok_or(RestoreError::IdentityMismatch)?;
        if !Arc::ptr_eq(&marker, &grant.marker) {
            return Err(RestoreError::IdentityMismatch);
        }
        self.vacant_counter_domain()?;
        if self.world_slot_trial.is_some()
            || self.restore_current(target)? != expected_current
            || binding.state_id == self.air
        {
            return Err(RestoreError::StateMismatch);
        }
        let (session, scope) = Self::fresh_counter_authority(target)?;
        let configuration = binding.configuration(target, scope)?;
        let trial = Box::new(
            Trial::restore_persistent_guest(
                configuration.clone(),
                Some((0, None)),
                None,
                self.game_time,
                None,
            )
            .map_err(|_| RestoreError::RuntimeExhausted)?,
        );
        let mut input = Some(crate::world_slot_trial::RestoreAllocation {
            generation: 1,
            revision: 0,
            stack: None,
        });
        let slot_trial = Box::new(
            crate::world_slot_trial::Trial::restore_guest_checked(configuration, 1, &mut input)
                .map_err(|_| RestoreError::RuntimeExhausted)?,
        );
        let slot = slot_trial
            .borrowed()
            .active
            .ok_or(RestoreError::InvalidFrame)?
            .key
            .clone();
        let placement = PlacementKey {
            world: grant.world,
            dimension: target.dimension.clone(),
            position: target.position,
            generation: 1,
        };
        let key = RuntimeCounterKey {
            session: session.clone(),
            placement: placement.clone(),
            instance: trial
                .snapshot()
                .record()
                .ok_or(RestoreError::InvalidFrame)?
                .instance(),
        };
        let domain = Box::new(Domain {
            world: grant.world,
            target: target.clone(),
            high_water: 1,
            active_key: Some(placement),
            retired_key: None,
            session,
            status: DomainStatus::Healthy,
            guest_fault: None,
            binding: binding.clone(),
            slot: Some(SlotMetadataSnapshot {
                high_water: 1,
                allocation: SlotAllocation::Active {
                    slot_generation: 1,
                    machine_generation: 1,
                },
                status: SlotStatus::Healthy,
            }),
        });
        Ok(PreparedGuestPair {
            domain,
            counter: trial,
            slot: slot_trial,
            key,
            slot_key: slot,
        })
    }
    fn publish_registered_guest_pair(
        &mut self,
        staged: PreparedGuestPair,
        grant: &mut HostCounterBootstrap,
    ) {
        self.counter_domain = Some(staged.domain);
        self.counter_trial = Some(staged.counter);
        self.world_slot_trial = Some(staged.slot);
        grant.spent = true;
    }
    /// Captured policy data only. This does not create a domain or mint history.
    pub fn install_registered_guest_placement_policy(
        &mut self,
        binding: &NativeBinding,
    ) -> Result<()> {
        if binding.profile != NativeBindingProfile::RegisteredCube || !binding.is_guest() {
            return Err(RestoreError::BindingMismatch);
        }
        if let Some(policy) = &self.registered_guest_placement {
            return if same_guest_binding(policy, binding) {
                Ok(())
            } else {
                Err(RestoreError::BindingMismatch)
            };
        }
        if let Some(domain) = &self.counter_domain {
            domain.healthy()?;
            if !same_guest_binding(&domain.binding, binding) {
                return Err(RestoreError::BindingMismatch);
            }
            self.checked_slot_lifecycle()
                .map_err(|_| RestoreError::StateMismatch)?;
        } else {
            self.vacant_counter_domain()?;
            if self.world_slot_trial.is_some() {
                return Err(RestoreError::Occupied);
            }
        }
        self.registered_guest_placement = Some(Box::new(binding.clone()));
        Ok(())
    }
    pub fn has_registered_guest_placement_policy(&self) -> bool {
        self.registered_guest_placement.is_some()
    }
    pub(super) fn check_registered_guest_plain_write(
        &self,
        position: [i32; 3],
        state: BlockStateId,
    ) -> Result<()> {
        let Some(policy) = &self.registered_guest_placement else {
            return Ok(());
        };
        let current = self.block(position[0], position[1], position[2]);
        if state == policy.state_id && current != state {
            return Err(RestoreError::IdentityMismatch);
        }
        if current == policy.state_id
            || self
                .counter_domain
                .as_ref()
                .is_some_and(|d| d.target.position == position)
        {
            let domain = self
                .counter_domain
                .as_ref()
                .ok_or(RestoreError::StateMismatch)?;
            domain.healthy()?;
            if domain.target.position != position || !same_guest_binding(&domain.binding, policy) {
                return Err(RestoreError::BindingMismatch);
            }
            self.checked_slot_lifecycle()
                .map_err(|_| RestoreError::StateMismatch)?;
            if current == policy.state_id && domain.active_key.is_none() {
                return Err(RestoreError::StateMismatch);
            }
        }
        Ok(())
    }
    /// Native authoritative LevelAccess entry. The original session lends its
    /// grant; a restored domain supplies its own checked readmission history.
    pub fn set_from_registered_guest_host_level(
        &mut self,
        position: [i32; 3],
        state: BlockStateId,
        dimension: &str,
        world: Option<WorldId>,
        mut grant: Option<&mut HostCounterBootstrap>,
    ) -> crate::Result<()> {
        let [x, y, z] = position;
        let Some(policy) = &self.registered_guest_placement else {
            return self.set(x, y, z, state);
        };
        let world = world.ok_or(RestoreError::IdentityMismatch)?;
        if self
            .counter_domain
            .as_ref()
            .is_some_and(|domain| domain.world != world)
        {
            return Err(RestoreError::IdentityMismatch.into());
        }
        if state != policy.state_id || self.block(x, y, z) == state {
            return self.set(x, y, z, state);
        }
        if dimension != "minecraft:overworld" {
            return Err(RestoreError::IdentityMismatch.into());
        }
        let target = CounterTarget::new(dimension, position)?;
        let current = self.restore_current(&target)?;
        let staged = match &self.counter_domain {
            None => {
                if current != self.air {
                    return Err(RestoreError::StateMismatch.into());
                }
                let actual_grant = grant.as_deref().ok_or(RestoreError::IdentityMismatch)?;
                if actual_grant.world != world {
                    return Err(RestoreError::IdentityMismatch.into());
                }
                PreparedGuestPlacement::First(self.prepare_registered_guest_pair(
                    policy,
                    &target,
                    actual_grant,
                    self.air,
                )?)
            }
            Some(domain) => {
                use super::slot_restore::SlotAllocation;
                domain.healthy()?;
                if domain.target != target || !same_guest_binding(&domain.binding, policy) {
                    return Err(RestoreError::BindingMismatch.into());
                }
                let (metadata, anchor) = self
                    .checked_slot_lifecycle()
                    .map_err(|_| RestoreError::StateMismatch)?;
                let counter = self.counter_trial.as_ref().ok_or(RestoreError::Detached)?;
                let slot = self
                    .world_slot_trial
                    .as_ref()
                    .ok_or(RestoreError::Detached)?;
                if domain.active_key.is_some()
                    || counter.snapshot().record().is_some()
                    || !matches!(metadata.allocation, SlotAllocation::Dormant)
                    || slot.borrowed().pending
                    || slot.borrowed().active.is_some()
                    || anchor.generation.is_some()
                    || self.game_time < 0
                    || !policy
                        .registry
                        .state(current)
                        .is_some_and(|s| s.properties.is_empty())
                {
                    return Err(RestoreError::StateMismatch.into());
                }
                // Existing storage validation checks the complete retained history,
                // including the observed dormant descriptor and Guest fault state.
                self.guest_storage_counter()?;
                let counter_generation = domain
                    .high_water
                    .checked_add(1)
                    .ok_or(RestoreError::GenerationExhausted)?;
                let slot_generation = metadata
                    .high_water
                    .checked_add(1)
                    .ok_or(RestoreError::GenerationExhausted)?;
                let counter = counter
                    .persistent_prepare_admission()
                    .map_err(|_| RestoreError::RuntimeExhausted)?;
                let slot = slot
                    .persistent_prepare_admission()
                    .map_err(|_| RestoreError::RuntimeExhausted)?;
                if crate::world_slot_trial::Trial::persistent_admission_key(&slot).incarnation()
                    != slot_generation
                {
                    return Err(RestoreError::StateMismatch.into());
                }
                PreparedGuestPlacement::Readmit {
                    counter,
                    slot,
                    placement: domain.key(counter_generation),
                    counter_generation,
                    slot_generation,
                }
            }
        };
        // Exclusive World/grant access and no callbacks keep the inspected
        // source stable. Every returned-error operation precedes this write.
        self.set_resident(x, y, z, state, false)?;
        match staged {
            PreparedGuestPlacement::First(staged) => {
                self.publish_registered_guest_pair(staged, grant.take().unwrap());
            }
            PreparedGuestPlacement::Readmit {
                counter,
                slot,
                placement,
                counter_generation,
                slot_generation,
            } => {
                use super::slot_restore::SlotAllocation;
                self.counter_trial
                    .as_mut()
                    .unwrap()
                    .persistent_publish_admission(counter);
                self.world_slot_trial
                    .as_mut()
                    .unwrap()
                    .persistent_publish_admission(slot);
                let domain = self.counter_domain.as_mut().unwrap();
                domain.high_water = counter_generation;
                domain.active_key = Some(placement);
                let metadata = domain.slot.as_mut().unwrap();
                metadata.high_water = slot_generation;
                metadata.allocation = SlotAllocation::Active {
                    slot_generation,
                    machine_generation: counter_generation,
                };
            }
        }
        Ok(())
    }
    pub(super) fn guest_storage_counter(&self) -> Result<GuestCounterFrameV3> {
        let d = self.counter_domain.as_ref().ok_or(RestoreError::Detached)?;
        if d.status != DomainStatus::Healthy || !d.binding.is_guest() || self.game_time < 0 {
            return Err(RestoreError::Faulted);
        }
        let t = self.counter_trial.as_ref().ok_or(RestoreError::Detached)?;
        let s = t.snapshot();
        if !d.configuration_matches(s.configuration()) {
            return Err(RestoreError::BindingMismatch);
        }
        let current = self.restore_current(&d.target)?;
        let observed = d
            .binding
            .registry
            .state(current)
            .ok_or(RestoreError::StateMismatch)?;
        let placement = match (&d.active_key, s.record()) {
            (Some(key), Some(record)) if record.resident() && current == d.binding.state_id => {
                Placement::Active {
                    generation: key.generation,
                    counter: record.counter(),
                    last_advanced: record.last_advanced(),
                }
            }
            (None, None) if current != d.binding.state_id => Placement::Dormant,
            _ => return Err(RestoreError::StateMismatch),
        };
        let status = match (&d.guest_fault, s.fault(), s.record()) {
            (None, None, _) => GuestStatus::Ready,
            (Some(w), Some(fault), Some(record))
                if w.record == record
                    && w.key.instance == record.instance()
                    && Arc::ptr_eq(&w.key.session, &d.session)
                    && d.active_key.as_ref() == Some(&w.key.placement)
                    && matches!(w.status, GuestStatus::Faulted { code, .. } if code.fault() == fault) =>
            {
                w.status
            }
            _ => return Err(RestoreError::Faulted),
        };
        let last_retired = match (&d.retired_key, s.last_retirement()) {
            (None, None) => None,
            (Some(key), Some(retired)) => Some(RetiredFrame {
                generation: key.generation,
                counter: retired.record().counter(),
                last_advanced: retired.record().last_advanced(),
                reason: NativeRetirementReason::from_native(retired.reason()),
                game_time: retired.game_time(),
            }),
            _ => return Err(RestoreError::InvalidFrame),
        };
        let frame = GuestCounterFrameV3 {
            fields: CounterFrameV2 {
                format: 3,
                kind: GUEST_COUNTER_KIND.into(),
                world_uuid: d.world,
                dimension: d.target.dimension.clone(),
                position: d.target.position,
                allocation_high_water: d.high_water,
                package_identity: digest(&d.binding.package),
                type_id: d.binding.type_id().into(),
                save_schema: 1,
                binding: FrameBinding {
                    block_id: d.binding.block_id.clone(),
                    template: "minecraft:iron_block".into(),
                    properties: BTreeMap::new(),
                },
                observed_state: FrameState {
                    name: observed.name.clone(),
                    properties: observed.properties.clone(),
                },
                saved_game_time: self.game_time,
                placement,
                last_retired,
            },
            execution: GuestExecution {
                profile: "registered-guest-v1".into(),
                primary_owner: d
                    .binding
                    .guest_owner()
                    .ok_or(RestoreError::BindingMismatch)?
                    .into(),
                status,
            },
        };
        frame.validate()?;
        Ok(frame)
    }
    pub fn guest_storage_view(&self) -> Result<GuestStorageView<'_>> {
        let frame = self.guest_storage_counter()?;
        self.guest_storage_slot_guard()
            .map_err(|_| RestoreError::Faulted)?;
        Ok(GuestStorageView { world: self, frame })
    }
    pub fn guest_runtime_state(&self) -> Result<GuestRuntimeState> {
        let view = self.guest_storage_view()?;
        let d = self.counter_domain.as_ref().unwrap();
        let Some(placement) = &d.active_key else {
            return Ok(GuestRuntimeState::ReadyDormant);
        };
        let record = self
            .counter_trial
            .as_ref()
            .unwrap()
            .snapshot()
            .record()
            .ok_or(RestoreError::InvalidFrame)?;
        let slot = self.world_slot_trial.as_ref().unwrap().borrowed();
        let active = slot.active.ok_or(RestoreError::InvalidFrame)?;
        let key = RuntimeCounterKey {
            session: d.session.clone(),
            placement: placement.clone(),
            instance: record.instance(),
        };
        Ok(match view.frame.execution.status {
            GuestStatus::Ready => GuestRuntimeState::ReadyActive {
                counter: key,
                slot: active.key.clone(),
            },
            status @ GuestStatus::Faulted { .. } => GuestRuntimeState::FaultedActive {
                receipt: GuestStorageReceipt {
                    key,
                    slot: active.key.clone(),
                    revision: active.revision,
                    status,
                },
            },
        })
    }
    pub fn validate_guest_storage_receipt(&self, receipt: &GuestStorageReceipt) -> Result<()> {
        let view = self.guest_storage_view()?;
        let d = self.counter_domain.as_ref().unwrap();
        let slot = self.world_slot_trial.as_ref().unwrap().borrowed();
        let active = slot.active.ok_or(RestoreError::InvalidFrame)?;
        if !Arc::ptr_eq(&d.session, &receipt.key.session)
            || d.active_key.as_ref() != Some(&receipt.key.placement)
            || self
                .counter_trial
                .as_ref()
                .unwrap()
                .snapshot()
                .record()
                .map(|r| r.instance())
                != Some(receipt.key.instance)
            || active.key != &receipt.slot
            || active.revision != receipt.revision
            || view.frame.execution.status != receipt.status
        {
            return Err(RestoreError::IdentityMismatch);
        }
        Ok(())
    }
}
impl World {
    pub(super) fn guest_storage_anchor(&self) -> Result<SlotAnchor<'_>> {
        self.guest_storage_counter()?;
        let d = self.counter_domain.as_ref().ok_or(RestoreError::Detached)?;
        Ok(SlotAnchor {
            world: d.world,
            target: &d.target,
            high_water: d.high_water,
            generation: d.active_key.as_ref().map(|k| k.generation),
            binding: &d.binding,
        })
    }
    fn correlate_guest_failure(
        &mut self,
        key: RuntimeCounterKey,
        record: Record,
        fault: crate::counter_trial::Fault,
    ) -> Result<()> {
        let code = GuestFaultCode::from_fault(fault)?;
        let d = self.counter_domain.as_mut().ok_or(RestoreError::Detached)?;
        let snapshot = self
            .counter_trial
            .as_ref()
            .ok_or(RestoreError::Detached)?
            .snapshot();
        if d.status != DomainStatus::Healthy
            || d.guest_fault.is_some()
            || !d.binding.is_guest()
            || !Arc::ptr_eq(&d.session, &key.session)
            || d.active_key.as_ref() != Some(&key.placement)
            || snapshot.record() != Some(record)
            || snapshot.fault() != Some(fault)
            || record.instance() != key.instance
            || !d.configuration_matches(snapshot.configuration())
            || self.game_time < 0
            || record
                .last_advanced()
                .is_some_and(|time| time >= self.game_time)
        {
            d.status = DomainStatus::Faulted;
            return Err(RestoreError::Faulted);
        }
        d.guest_fault = Some(GuestFaultWitness {
            key,
            record,
            status: GuestStatus::Faulted {
                code,
                observed_tick: self.game_time,
            },
        });
        Ok(())
    }
    pub(super) fn prepare_persistent_guest_checked(
        &mut self,
        running: bool,
        eligible: bool,
    ) -> std::result::Result<Option<PersistentGuestPhaseToken>, crate::counter_trial::PhaseError>
    {
        use crate::counter_trial::{Fault, PhaseError};
        if !self.has_persistent_guest_counter() {
            return Err(PhaseError::DispatchMismatch);
        }
        self.persistent_counter_phase_guard();
        if let Some(w) = self
            .counter_domain
            .as_ref()
            .and_then(|d| d.guest_fault.as_ref())
        {
            return Err(PhaseError::Fault(match w.status {
                GuestStatus::Faulted { code, .. } => code.fault(),
                GuestStatus::Ready => Fault::PersistentDomain,
            }));
        }
        self.persistent_slot_guard()
            .map_err(|_| PhaseError::Fault(Fault::PersistentDomain))?;
        let Some(key) = self
            .persistent_counter_key()
            .map_err(|_| PhaseError::Fault(Fault::PersistentDomain))?
        else {
            return Ok(None);
        };
        let record = self
            .read_persistent_counter(&key)
            .map_err(|_| PhaseError::Fault(Fault::PersistentDomain))?;
        let view = self
            .world_slot_trial
            .as_ref()
            .ok_or(PhaseError::Fault(Fault::PersistentDomain))?
            .borrowed();
        let active = view
            .active
            .ok_or(PhaseError::Fault(Fault::PersistentDomain))?;
        let slot = active.key.clone();
        let revision = active.revision;
        let column = self
            .chunks
            .get(&(
                key.placement.position[0] >> 4,
                key.placement.position[2] >> 4,
            ))
            .ok_or(PhaseError::Fault(Fault::PersistentDomain))?
            .clone();
        let current = self.guest_counter_current();
        match self.counter_trial.as_mut().unwrap().prepare_guest(
            running,
            eligible,
            current,
            self.game_time,
        ) {
            Ok(Some(phase)) => Ok(Some(PersistentGuestPhaseToken {
                key,
                slot,
                revision,
                column,
                phase,
            })),
            Ok(None) => Ok(None),
            Err(error @ PhaseError::Fault(fault)) => {
                self.correlate_guest_failure(key, record, fault)
                    .map_err(|_| PhaseError::Fault(Fault::PersistentDomain))?;
                Err(error)
            }
            Err(error) => Err(error),
        }
    }
    pub(super) fn complete_persistent_guest_checked(
        &mut self,
        token: PersistentGuestPhaseToken,
        result: std::result::Result<
            crate::counter_trial::Proposal,
            crate::counter_trial::GuestFailure,
        >,
        running: bool,
        eligible: bool,
    ) -> std::result::Result<crate::counter_trial::Completion, crate::counter_trial::PhaseError>
    {
        use crate::counter_trial::{Completion, Fault, PhaseError};
        let same = self.counter_domain.as_ref().is_some_and(|d| {
            Arc::ptr_eq(&d.session, &token.key.session)
                && d.active_key.as_ref() == Some(&token.key.placement)
                && d.binding.is_guest()
                && d.guest_fault.is_none()
        }) && self.counter_trial.as_ref().is_some_and(|t| {
            t.configuration() == token.phase.configuration()
                && t.snapshot().record() == Some(token.phase.record())
        }) && self.world_slot_trial.as_ref().is_some_and(|t| {
            t.borrowed()
                .active
                .is_some_and(|a| a.key == &token.slot && a.revision == token.revision)
        }) && self.game_time == token.phase.game_time()
            && self
                .chunks
                .get(&(
                    token.key.placement.position[0] >> 4,
                    token.key.placement.position[2] >> 4,
                ))
                .is_some_and(|column| Arc::ptr_eq(column, &token.column));
        if !same {
            return Ok(Completion::Stale);
        }
        self.persistent_counter_phase_guard();
        self.persistent_slot_guard()
            .map_err(|_| PhaseError::Fault(Fault::PersistentDomain))?;
        self.read_persistent_counter(&token.key)
            .map_err(|_| PhaseError::Fault(Fault::PersistentDomain))?;
        let current = self.guest_counter_current();
        let record = token.phase.record();
        let completed = self.counter_trial.as_mut().unwrap().complete_guest(
            token.phase,
            result,
            running,
            eligible,
            current,
            self.game_time,
        );
        if let Err(PhaseError::Fault(fault)) = completed {
            self.correlate_guest_failure(token.key, record, fault)
                .map_err(|_| PhaseError::Fault(Fault::PersistentDomain))?;
        }
        completed
    }
    pub fn restore_guest_counter_frame(
        &mut self,
        binding: &NativeBinding,
        expected_world: WorldId,
        target: &CounterTarget,
        frame: &GuestCounterFrameV3,
    ) -> Result<()> {
        self.vacant_counter_domain()?;
        frame.validate()?;
        let f = &frame.fields;
        if !binding.is_guest()
            || f.world_uuid != expected_world
            || f.dimension != target.dimension
            || f.position != target.position
            || self.game_time != f.saved_game_time
        {
            return Err(RestoreError::IdentityMismatch);
        }
        if f.package_identity != digest(&binding.package)
            || f.type_id != binding.type_id()
            || f.binding.block_id != binding.block_id
            || frame.execution.primary_owner.as_str()
                != binding.guest_owner().ok_or(RestoreError::BindingMismatch)?
        {
            return Err(RestoreError::BindingMismatch);
        }
        let current = self.restore_current(target)?;
        let observed = binding
            .registry
            .state(current)
            .ok_or(RestoreError::StateMismatch)?;
        if observed.name != f.observed_state.name
            || observed.properties != f.observed_state.properties
            || binding.state_id == self.air
            || (matches!(f.placement, Placement::Active { .. }) && current != binding.state_id)
        {
            return Err(RestoreError::StateMismatch);
        }
        let (session, scope) = Self::fresh_counter_authority(target)?;
        let configuration = binding.configuration(target, scope)?;
        let (placement, active) = match f.placement {
            Placement::Active {
                generation,
                counter,
                last_advanced,
            } => (
                Some(PlacementKey {
                    world: expected_world,
                    dimension: target.dimension.clone(),
                    position: target.position,
                    generation,
                }),
                Some((counter, last_advanced)),
            ),
            Placement::Dormant => (None, None),
        };
        let retired = f
            .last_retired
            .as_ref()
            .map(|r| (r.counter, r.last_advanced, r.reason.native(), r.game_time));
        let fault = match frame.execution.status {
            GuestStatus::Ready => None,
            GuestStatus::Faulted { code, .. } => Some(code.fault()),
        };
        let trial = Box::new(
            Trial::restore_persistent_guest(
                configuration,
                active,
                retired,
                f.saved_game_time,
                fault,
            )
            .map_err(|_| RestoreError::RuntimeExhausted)?,
        );
        let guest_fault = match frame.execution.status {
            GuestStatus::Ready => None,
            status @ GuestStatus::Faulted { .. } => {
                let record = trial
                    .snapshot()
                    .record()
                    .ok_or(RestoreError::InvalidFrame)?;
                Some(GuestFaultWitness {
                    key: RuntimeCounterKey {
                        session: session.clone(),
                        placement: placement.clone().ok_or(RestoreError::InvalidFrame)?,
                        instance: record.instance(),
                    },
                    record,
                    status,
                })
            }
        };
        let retired_key = f.last_retired.as_ref().map(|r| PlacementKey {
            world: expected_world,
            dimension: target.dimension.clone(),
            position: target.position,
            generation: r.generation,
        });
        let domain = Box::new(Domain {
            world: expected_world,
            target: target.clone(),
            high_water: f.allocation_high_water,
            active_key: placement,
            retired_key,
            session,
            status: DomainStatus::Healthy,
            guest_fault,
            binding: binding.clone(),
            slot: None,
        });
        self.counter_domain = Some(domain);
        self.counter_trial = Some(trial);
        Ok(())
    }
}
#[cfg(test)]
mod persistent_guest_frame_tests {
    use super::*;
    fn ready() -> GuestCounterFrameV3 {
        GuestCounterFrameV3 {
            fields: CounterFrameV2 {
                format: 3,
                kind: GUEST_COUNTER_KIND.into(),
                world_uuid: WorldId::parse("00000000-0000-4000-8000-000000000001").unwrap(),
                dimension: "minecraft:overworld".into(),
                position: [2, 70, 2],
                allocation_high_water: 7,
                package_identity: "01".repeat(32),
                type_id: "guest:counter".into(),
                save_schema: 1,
                binding: FrameBinding {
                    block_id: "guest:counter".into(),
                    template: "minecraft:iron_block".into(),
                    properties: BTreeMap::new(),
                },
                observed_state: FrameState {
                    name: "guest:counter".into(),
                    properties: BTreeMap::new(),
                },
                saved_game_time: 8,
                placement: Placement::Active {
                    generation: 7,
                    counter: 3,
                    last_advanced: Some(5),
                },
                last_retired: None,
            },
            execution: GuestExecution {
                profile: "registered-guest-v1".into(),
                primary_owner: "guest".into(),
                status: GuestStatus::Ready,
            },
        }
    }
    #[test]
    fn guest3_closed_header_roundtrip_and_native_rejection() {
        let frame = ready();
        let bytes = frame.encode().unwrap();
        assert_eq!(GuestCounterFrameV3::decode(&bytes).unwrap(), frame);
        assert!(CounterFrameV2::decode(&bytes).is_err());
        for mode in 0..5 {
            let mut bad = frame.clone();
            match mode {
                0 => bad.fields.format = 2,
                1 => bad.fields.kind = "native-registered-counter-checkpoint".into(),
                2 => bad.execution.profile = "unknown".into(),
                3 => bad.execution.primary_owner = "other".into(),
                _ => bad.fields.save_schema = 2,
            }
            assert!(bad.encode().is_err());
        }
    }
    #[test]
    fn guest3_fault_clock_and_faulted_dormant_are_strict() {
        let mut frame = ready();
        for tick in [-1, 4, 5, 9] {
            frame.execution.status = GuestStatus::Faulted {
                code: GuestFaultCode::GuestEvaluation,
                observed_tick: tick,
            };
            assert!(frame.encode().is_err());
        }
        frame.execution.status = GuestStatus::Faulted {
            code: GuestFaultCode::GuestEvaluation,
            observed_tick: 6,
        };
        let bytes = frame.encode().unwrap();
        assert_eq!(GuestCounterFrameV3::decode(&bytes).unwrap(), frame);
        frame.fields.placement = Placement::Dormant;
        frame.fields.observed_state.name = "minecraft:air".into();
        assert!(frame.encode().is_err());
        frame.execution.status = GuestStatus::Ready;
        frame.fields.last_retired = Some(RetiredFrame {
            generation: 7,
            counter: 3,
            last_advanced: Some(5),
            reason: NativeRetirementReason::BlockChanged,
            game_time: 6,
        });
        assert!(frame.encode().is_ok());
    }
    #[test]
    fn guest3_duplicate_unknown_and_inconsistent_exhaustion_refuse() {
        let frame = ready();
        let bytes = frame.encode().unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let duplicate = text.replacen("\"execution\":", "\"format\":3,\"execution\":", 1);
        assert!(GuestCounterFrameV3::decode(duplicate.as_bytes()).is_err());
        let mut value = serde_json::to_value(&frame.fields).unwrap();
        value.as_object_mut().unwrap().insert("execution".into(),serde_json::json!({"profile":"registered-guest-v1","primary_owner":"guest","status":{"state":"faulted","code":"unknown","observed_tick":6}}));
        assert!(GuestCounterFrameV3::decode(&serde_json::to_vec(&value).unwrap()).is_err());
        let mut bad = frame;
        bad.execution.status = GuestStatus::Faulted {
            code: GuestFaultCode::CounterExhausted,
            observed_tick: 6,
        };
        assert!(bad.encode().is_err());
    }
}
