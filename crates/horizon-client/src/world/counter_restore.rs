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
#[derive(Clone)]
pub struct NativeBinding {
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
            name: "minecraft:iron_block".into(),
            properties: BTreeMap::new(),
        };
        let state_id = registry
            .id(&physical)
            .ok_or(RestoreError::BindingMismatch)?;
        Ok(Self {
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
        Configuration::new(
            scope,
            &self.machine_type,
            self.symbolic_state(),
            self.state_id,
            target.position,
        )
        .map_err(|_| RestoreError::BindingMismatch)
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
    pub fn validate(&self) -> Result<()> {
        CounterTarget::new(&self.dimension, self.position)?;
        let generation =
            |generation: u64| generation > 0 && generation <= self.allocation_high_water;
        let payload = |counter: u64, last: Option<i64>, end: i64| {
            (counter == 0) == last.is_none() && last.is_none_or(|time| time >= 0 && time <= end)
        };
        if self.format != 2
            || self.kind != "native-counter-checkpoint"
            || self.save_schema != 1
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
                || self.observed_state.name != self.binding.template
                || self
                    .last_retired
                    .as_ref()
                    .is_some_and(|retired| retired.generation >= active)
            {
                return Err(RestoreError::InvalidFrame);
            }
        } else if self.observed_state.name == self.binding.template {
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
        configuration.backend() == &crate::counter_trial::Backend::Native
            && configuration.scope().dimension() == self.target.dimension
            && configuration.position() == self.target.position
            && configuration.machine_type() == &self.binding.machine_type
            && configuration.state_id() == self.binding.state_id
            && configuration.state() == &self.binding.symbolic_state()
    }
}

impl World {
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
            status: domain.status,
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
            kind: "native-counter-checkpoint".into(),
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
}
