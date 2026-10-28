//! Opt-in transient counter with native and staged guest host phases.
//! No persistence or Game callback binding is established here.
//! Host configuration/phase authority is required; this is not machine activity
//! discovery, a saved generation, or a stable behavior interface.
use crate::{
    chunk::{MIN_Y, SECTIONS},
    mutation_observer::AuthorityScope,
};
use horizon_mods::content::machines::MachineType;
use horizon_world::{chunk::BlockState, registry::BlockStateId};
use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    InvalidMetadata,
    InvalidPosition,
    NotResident,
    AlreadyEnabled,
}
impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "counter trial configuration: {self:?}")
    }
}
impl std::error::Error for ConfigError {}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Dispatch refusal only; the native wrapper does not latch this fault.
    DispatchMismatch,
    GuestFailure(GuestFailure),
    InstanceExhausted,
    CounterExhausted,
    ReloadMismatch {
        actual: BlockStateId,
    },
    ResidentMismatch {
        actual: BlockStateId,
    },
}
impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "counter trial fault: {self:?}")
    }
}
impl std::error::Error for Fault {}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuestFailure {
    Trap,
    Fuel,
    Memory,
    Rejected,
    InvalidReply,
    Unavailable,
    Evaluation,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhaseError {
    DispatchMismatch,
    Fault(Fault),
}
impl fmt::Display for PhaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "counter guest phase: {self:?}")
    }
}
impl std::error::Error for PhaseError {}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completion {
    Committed,
    Skipped,
    Stale,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Backend {
    Native,
    Guest {
        primary_owner: String,
        package_identity: [u8; 32],
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    Disabled,
    StaleKey,
}
impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "counter trial read: {self:?}")
    }
}
impl std::error::Error for ReadError {}

/// Captured type provenance is supplied by MachineType. The authoritative caller
/// must also validate its selected block binding and registry resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Configuration {
    scope: AuthorityScope,
    machine_type: MachineType,
    state: BlockState,
    state_id: BlockStateId,
    position: [i32; 3],
    backend: Backend,
}
impl Configuration {
    pub fn new(
        scope: AuthorityScope,
        machine_type: &MachineType,
        state: BlockState,
        state_id: BlockStateId,
        position: [i32; 3],
    ) -> Result<Self, ConfigError> {
        if !(MIN_Y..MIN_Y + SECTIONS as i32 * 16).contains(&position[1]) {
            return Err(ConfigError::InvalidPosition);
        }
        let (namespace, path) = state
            .name
            .split_once(':')
            .ok_or(ConfigError::InvalidMetadata)?;
        let owner = machine_type.id().split_once(':').map(|(owner, _)| owner);
        if machine_type.save_schema() != 1
            || !state.properties.is_empty()
            || Some(namespace) != owner
            || horizon_mods::content::check_path(&format!("data/{namespace}/{path}")).is_err()
        {
            return Err(ConfigError::InvalidMetadata);
        }
        Ok(Self {
            scope,
            machine_type: machine_type.clone(),
            state,
            state_id,
            position,
            backend: Backend::Native,
        })
    }
    /// Captured metadata binding only; runtime purity/activation is a caller prerequisite.
    pub fn with_guest_package(
        mut self,
        prepared: &horizon_mods::paired::PreparedBundle,
    ) -> Result<Self, ConfigError> {
        let owner = &prepared.bundle().content_owner().id;
        let content = prepared.content();
        let mut blocks = content
            .blocks
            .iter()
            .filter(|b| b.machine_type() == Some(self.machine_type.id()));
        let block = blocks.next().ok_or(ConfigError::InvalidMetadata)?;
        if self.backend != Backend::Native
            || self.machine_type.id().split_once(':').map(|(id, _)| id) != Some(owner.as_str())
            || content.machine_types().get(self.machine_type.id()) != Some(&self.machine_type)
            || blocks.next().is_some()
            || block.id != self.state.name
            || block.template != "minecraft:iron_block"
            || block.state_plan.is_some()
        {
            return Err(ConfigError::InvalidMetadata);
        }
        self.backend = Backend::Guest {
            primary_owner: owner.clone(),
            package_identity: *prepared.identity(),
        };
        Ok(self)
    }
    pub fn backend(&self) -> &Backend {
        &self.backend
    }
    pub fn scope(&self) -> &AuthorityScope {
        &self.scope
    }
    pub fn machine_type(&self) -> &MachineType {
        &self.machine_type
    }
    pub fn state(&self) -> &BlockState {
        &self.state
    }
    pub fn state_id(&self) -> BlockStateId {
        self.state_id
    }
    pub fn position(&self) -> [i32; 3] {
        self.position
    }
    pub(crate) fn chunk(&self) -> (i32, i32) {
        (self.position[0] >> 4, self.position[2] >> 4)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    instance: u64,
    counter: u64,
    last_advanced: Option<i64>,
    resident: bool,
}
impl Record {
    pub fn instance(&self) -> u64 {
        self.instance
    }
    pub fn counter(&self) -> u64 {
        self.counter
    }
    pub fn last_advanced(&self) -> Option<i64> {
        self.last_advanced
    }
    pub fn resident(&self) -> bool {
        self.resident
    }
}
/// An invocation-local read token. No public construction, cloning or mutation.
#[derive(Debug)]
pub struct PhaseToken {
    configuration: Arc<Configuration>,
    record: Record,
    game_time: i64,
}
impl PhaseToken {
    pub fn configuration(&self) -> &Configuration {
        &self.configuration
    }
    pub fn record(&self) -> Record {
        self.record
    }
    pub fn game_time(&self) -> i64 {
        self.game_time
    }
}
/// Untrusted scalar output; constructing this does not authorize any mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proposal {
    pub world_instance: u64,
    pub instance: u64,
    pub game_time: i64,
    pub expected_counter: u64,
    pub next_counter: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetirementReason {
    BlockChanged,
    ChunkReplaced,
    Explicit,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retirement {
    record: Record,
    reason: RetirementReason,
    game_time: i64,
}
impl Retirement {
    pub fn record(&self) -> Record {
        self.record
    }
    pub fn reason(&self) -> RetirementReason {
        self.reason
    }
    pub fn game_time(&self) -> i64 {
        self.game_time
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    configuration: Arc<Configuration>,
    record: Option<Record>,
    last_retirement: Option<Retirement>,
    fault: Option<Fault>,
}
impl Snapshot {
    pub fn configuration(&self) -> &Configuration {
        &self.configuration
    }
    pub fn record(&self) -> Option<Record> {
        self.record
    }
    pub fn last_retirement(&self) -> Option<Retirement> {
        self.last_retirement
    }
    pub fn fault(&self) -> Option<Fault> {
        self.fault
    }
}

static LAST_INSTANCE: AtomicU64 = AtomicU64::new(0);
fn allocate_instance(allocator: &AtomicU64) -> Result<u64, Fault> {
    let mut previous = allocator.load(Ordering::Relaxed);
    loop {
        let next = previous.checked_add(1).ok_or(Fault::InstanceExhausted)?;
        match allocator.compare_exchange_weak(previous, next, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => return Ok(next),
            Err(observed) => previous = observed,
        }
    }
}
pub(crate) struct Trial {
    configuration: Arc<Configuration>,
    record: Option<Record>,
    last_retirement: Option<Retirement>,
    fault: Option<Fault>,
}
impl Trial {
    pub(crate) fn new(configuration: Configuration, current: Option<BlockStateId>) -> Self {
        let mut trial = Self {
            configuration: Arc::new(configuration),
            record: None,
            last_retirement: None,
            fault: None,
        };
        if current == Some(trial.configuration.state_id) {
            trial.admit();
        }
        trial
    }
    pub(crate) fn configuration(&self) -> &Configuration {
        &self.configuration
    }
    pub(crate) fn snapshot(&self) -> Snapshot {
        Snapshot {
            configuration: self.configuration.clone(),
            record: self.record,
            last_retirement: self.last_retirement,
            fault: self.fault,
        }
    }
    pub(crate) fn read(&self, scope: &AuthorityScope, instance: u64) -> Result<Record, ReadError> {
        self.record
            .filter(|r| scope == &self.configuration.scope && r.instance == instance)
            .ok_or(ReadError::StaleKey)
    }
    fn admit(&mut self) {
        self.admit_from(&LAST_INSTANCE);
    }
    fn admit_from(&mut self, allocator: &AtomicU64) {
        if self.fault.is_some() {
            return;
        }
        match allocate_instance(allocator) {
            Ok(instance) => {
                self.record = Some(Record {
                    instance,
                    counter: 0,
                    last_advanced: None,
                    resident: true,
                })
            }
            Err(fault) => self.fault = Some(fault),
        }
    }
    pub(crate) fn retire(&mut self, reason: RetirementReason, game_time: i64) {
        if let Some(record) = self.record.take() {
            self.last_retirement = Some(Retirement {
                record,
                reason,
                game_time,
            });
        }
    }
    pub(crate) fn changed(&mut self, position: [i32; 3], current: BlockStateId, game_time: i64) {
        if position != self.configuration.position {
            return;
        }
        self.retire(RetirementReason::BlockChanged, game_time);
        if current == self.configuration.state_id {
            self.admit();
        }
    }
    pub(crate) fn inserted(&mut self, replaced: bool, current: BlockStateId, game_time: i64) {
        if replaced {
            self.retire(RetirementReason::ChunkReplaced, game_time);
        }
        if let Some(record) = &mut self.record {
            if current == self.configuration.state_id {
                record.resident = true;
            } else {
                record.resident = false;
                if self.fault.is_none() {
                    self.fault = Some(Fault::ReloadMismatch { actual: current });
                }
            }
        } else if current == self.configuration.state_id {
            self.admit();
        }
    }
    pub(crate) fn unloaded(&mut self) {
        if let Some(record) = &mut self.record {
            record.resident = false;
        }
    }
    fn guest_ready(
        &mut self,
        running: bool,
        eligible: bool,
        current: Option<BlockStateId>,
        game_time: i64,
    ) -> Result<bool, PhaseError> {
        if !matches!(self.configuration.backend(), Backend::Guest { .. }) {
            return Err(PhaseError::DispatchMismatch);
        }
        if let Some(fault) = self.fault {
            return Err(PhaseError::Fault(fault));
        }
        let Some(record) = self.record else {
            return Ok(false);
        };
        if !running || !eligible || !record.resident {
            return Ok(false);
        }
        let Some(current) = current else {
            return Ok(false);
        };
        let fault = if current != self.configuration.state_id {
            Some(Fault::ResidentMismatch { actual: current })
        } else if record.last_advanced == Some(game_time) {
            return Ok(false);
        } else if record.counter.checked_add(1).is_none() {
            Some(Fault::CounterExhausted)
        } else {
            None
        };
        if let Some(fault) = fault {
            self.fault = Some(fault);
            return Err(PhaseError::Fault(fault));
        }
        Ok(true)
    }
    pub(crate) fn prepare_guest(
        &mut self,
        running: bool,
        eligible: bool,
        current: Option<BlockStateId>,
        game_time: i64,
    ) -> Result<Option<PhaseToken>, PhaseError> {
        if !self.guest_ready(running, eligible, current, game_time)? {
            return Ok(None);
        }
        Ok(self.record.map(|record| PhaseToken {
            configuration: self.configuration.clone(),
            record,
            game_time,
        }))
    }
    pub(crate) fn complete_guest(
        &mut self,
        token: PhaseToken,
        result: Result<Proposal, GuestFailure>,
        running: bool,
        eligible: bool,
        current: Option<BlockStateId>,
        game_time: i64,
    ) -> Result<Completion, PhaseError> {
        if self.configuration != token.configuration
            || self.record != Some(token.record)
            || game_time != token.game_time
        {
            return Ok(Completion::Stale);
        }
        if !self.guest_ready(running, eligible, current, game_time)? {
            return Ok(Completion::Skipped);
        }
        let valid = |p: &Proposal| {
            p.world_instance == token.configuration.scope.world_instance()
                && p.instance == token.record.instance
                && p.game_time == token.game_time
                && p.expected_counter == token.record.counter
                && token.record.counter.checked_add(1) == Some(p.next_counter)
        };
        match result {
            Ok(proposal) if valid(&proposal) => {
                let Some(record) = &mut self.record else {
                    return Ok(Completion::Stale);
                };
                record.counter = proposal.next_counter;
                record.last_advanced = Some(game_time);
                Ok(Completion::Committed)
            }
            other => {
                let fault = Fault::GuestFailure(other.err().unwrap_or(GuestFailure::InvalidReply));
                self.fault = Some(fault);
                Err(PhaseError::Fault(fault))
            }
        }
    }
    pub(crate) fn advance(
        &mut self,
        running: bool,
        eligible: bool,
        current: Option<BlockStateId>,
        game_time: i64,
    ) -> Result<bool, Fault> {
        if let Some(fault) = self.fault {
            return Err(fault);
        }
        let Some(record) = &mut self.record else {
            return Ok(false);
        };
        if !running || !eligible || !record.resident {
            return Ok(false);
        }
        let Some(current) = current else {
            return Ok(false);
        };
        if current != self.configuration.state_id {
            let fault = Fault::ResidentMismatch { actual: current };
            self.fault = Some(fault);
            return Err(fault);
        }
        if record.last_advanced == Some(game_time) {
            return Ok(false);
        }
        let Some(next) = record.counter.checked_add(1) else {
            self.fault = Some(Fault::CounterExhausted);
            return Err(Fault::CounterExhausted);
        };
        record.counter = next;
        record.last_advanced = Some(game_time);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{chunk::Chunk, world::World};
    use std::{fs, sync::OnceLock};
    const POSITION: [i32; 3] = [2, 70, 2];
    fn metadata() -> &'static MachineType {
        static TYPE: OnceLock<MachineType> = OnceLock::new();
        TYPE.get_or_init(|| {
            captured(b"not executed", "counter_demo", "counter")
                .content()
                .machine_types()
                .get("counter_demo:counter_type")
                .unwrap()
                .clone()
        })
    }
    fn captured(server: &[u8], owner: &str, block: &str) -> horizon_mods::paired::PreparedBundle {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "horizon-counter-metadata-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&dir).unwrap();
        let descriptor = format!(
            "format=1\nhost-contract='paired-preview-1'\n[content]\nid='{owner}'\nname='Counter'\nversion='0'\napi='0.2'\nside='both'\ncapabilities=['registries']\n[client]\nentry='client.wasm'\napi='0.1'\ncapabilities=[]\n[server]\nentry='server.wasm'\napi='0.3'\ncapabilities=[]\n"
        );
        let machine_path = format!("data/{owner}/horizon/machine/counter_type.json");
        let block_path = format!("data/{owner}/horizon/block/{block}.json");
        let binding = format!(
            "{{\"template\":\"minecraft:iron_block\",\"machine-type\":\"{owner}:counter_type\"}}"
        );
        for (name, bytes) in [
            ("horizon-paired.toml", descriptor.as_bytes()),
            ("client.wasm", b"not executed"),
            ("server.wasm", server),
            (machine_path.as_str(), br#"{"format":1,"save-schema":1}"#),
            (block_path.as_str(), binding.as_bytes()),
        ] {
            let path = dir.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        let bundle = horizon_mods::paired::PreparedBundle::read_folder(&dir).unwrap();
        fs::remove_dir_all(dir).unwrap();
        bundle
    }
    fn configuration(dimension: &str) -> Configuration {
        Configuration::new(
            AuthorityScope::new(7, dimension).unwrap(),
            metadata(),
            BlockState {
                name: "counter_demo:counter".into(),
                properties: Default::default(),
            },
            BlockStateId(1),
            POSITION,
        )
        .unwrap()
    }
    fn chunk(state: u32) -> Chunk {
        let mut chunk = Chunk::empty(0, 0);
        chunk.set(POSITION[0], POSITION[1], POSITION[2], state);
        chunk
    }
    fn world(state: u32, dimension: &str) -> World {
        let mut world = World::new(BlockStateId(0), 0);
        world.chunks.insert((0, 0), Arc::new(chunk(state)));
        world
            .enable_counter_trial(configuration(dimension))
            .unwrap();
        world
    }
    fn record(world: &World) -> Record {
        world.counter_trial_snapshot().unwrap().record().unwrap()
    }
    fn write(world: &mut World, state: u32) {
        world
            .set(POSITION[0], POSITION[1], POSITION[2], BlockStateId(state))
            .unwrap();
    }

    #[test]
    fn native_counter_configuration_requires_typed_resident_nonair_binding() {
        let config = configuration("minecraft:overworld");
        assert_eq!(config.machine_type().id(), "counter_demo:counter_type");
        assert_eq!(config.machine_type().save_schema(), 1);
        let mut empty = World::new(BlockStateId(0), 0);
        assert!(empty.counter_trial_snapshot().is_none());
        assert!(empty.advance_counter_trial(true, true).is_none());
        assert_eq!(
            empty.enable_counter_trial(config.clone()),
            Err(ConfigError::NotResident)
        );
        let mut invalid = config.clone();
        invalid.state_id = BlockStateId(0);
        assert_eq!(
            empty.enable_counter_trial(invalid),
            Err(ConfigError::InvalidMetadata)
        );
        for (name, properties, y) in [
            ("foreign:counter", Default::default(), 70),
            ("counter_demo:a:b", Default::default(), 70),
            (
                "counter_demo:counter",
                [("axis".into(), "x".into())].into(),
                70,
            ),
            ("counter_demo:counter", Default::default(), MIN_Y - 1),
        ] {
            assert!(
                Configuration::new(
                    config.scope.clone(),
                    metadata(),
                    BlockState {
                        name: name.into(),
                        properties
                    },
                    BlockStateId(1),
                    [2, y, 2]
                )
                .is_err()
            );
        }
        let mut world = world(0, "minecraft:overworld");
        assert!(world.counter_trial_snapshot().unwrap().record().is_none());
        assert_eq!(
            world.enable_counter_trial(config),
            Err(ConfigError::AlreadyEnabled)
        );
        assert_eq!(
            world.read_counter_trial(&AuthorityScope::new(7, "minecraft:overworld").unwrap(), 1),
            Err(ReadError::StaleKey)
        );
    }
    #[test]
    fn native_counter_placement_duplicate_phase_and_stale_keys() {
        let mut world = world(0, "minecraft:overworld");
        write(&mut world, 1);
        let first = record(&world);
        write(&mut world, 1);
        assert_eq!(record(&world), first);
        assert_eq!(world.advance_counter_trial(false, true), Some(Ok(false)));
        assert_eq!(world.advance_counter_trial(true, false), Some(Ok(false)));
        assert_eq!(record(&world).counter(), 0);
        assert_eq!(world.advance_counter_trial(true, true), Some(Ok(true)));
        assert_eq!(world.advance_counter_trial(true, true), Some(Ok(false)));
        world.game_time = 1000;
        world.advance_counter_trial(true, true).unwrap().unwrap();
        assert_eq!(record(&world).counter(), 2); // Never catches up elapsed time.
        world.game_time = -1;
        world.advance_counter_trial(true, true).unwrap().unwrap();
        assert_eq!(record(&world).counter(), 3); // Only equal time is a duplicate.
        assert!(world.set(16, 70, 0, BlockStateId(1)).is_err());
        let scope = configuration("minecraft:overworld").scope;
        assert_eq!(
            world
                .read_counter_trial(&scope, first.instance())
                .unwrap()
                .counter(),
            3
        );
        assert_eq!(
            world.read_counter_trial(
                &configuration("minecraft:the_nether").scope,
                first.instance()
            ),
            Err(ReadError::StaleKey)
        );
        write(&mut world, 0);
        assert_eq!(
            world.read_counter_trial(&scope, first.instance()),
            Err(ReadError::StaleKey)
        );
        let retired = world
            .counter_trial_snapshot()
            .unwrap()
            .last_retirement()
            .unwrap();
        assert_eq!(retired.record().counter(), 3);
        assert_eq!(retired.reason(), RetirementReason::BlockChanged);
        write(&mut world, 1);
        let next = record(&world);
        assert_ne!(next.instance(), first.instance());
        assert_eq!(next.counter(), 0);
        assert_eq!(
            world.read_counter_trial(&scope, first.instance()),
            Err(ReadError::StaleKey)
        );
        let final_snapshot = world.take_counter_trial().unwrap();
        assert!(final_snapshot.record().is_none());
        assert_eq!(final_snapshot.last_retirement().unwrap().record(), next);
        assert_eq!(
            final_snapshot.last_retirement().unwrap().reason(),
            RetirementReason::Explicit
        );
        assert_eq!(
            world.read_counter_trial(&scope, next.instance()),
            Err(ReadError::Disabled)
        );
        world
            .enable_counter_trial(configuration("minecraft:overworld"))
            .unwrap();
        assert_ne!(record(&world).instance(), next.instance());
        assert_eq!(
            world.read_counter_trial(&scope, next.instance()),
            Err(ReadError::StaleKey)
        );
    }
    #[test]
    fn native_counter_dormant_reload_replacement_and_sdk_lending() {
        let mut world = world(1, "minecraft:overworld");
        world.game_time = 1;
        world.advance_counter_trial(true, true).unwrap().unwrap();
        let first = record(&world);
        let saved = world.remove_streamed_chunk(&(0, 0)).unwrap();
        assert!(!record(&world).resident());
        world.game_time = 100;
        assert_eq!(world.advance_counter_trial(true, true), Some(Ok(false)));
        world.insert_streamed_chunk((*saved).clone());
        assert_eq!(record(&world), first);
        world.advance_counter_trial(true, true).unwrap().unwrap();
        assert_eq!(record(&world).counter(), 2);
        let before_lend = world.counter_trial_snapshot().unwrap();
        let lent = std::mem::take(&mut world.chunks);
        assert_eq!(world.advance_counter_trial(true, true), Some(Ok(false)));
        assert_eq!(world.counter_trial_snapshot().unwrap(), before_lend);
        world.chunks = lent;
        assert_eq!(world.counter_trial_snapshot().unwrap(), before_lend);
        world.insert_streamed_chunk(chunk(1));
        assert_ne!(record(&world).instance(), first.instance());
        assert_eq!(record(&world).counter(), 0);
        let retired = world
            .counter_trial_snapshot()
            .unwrap()
            .last_retirement()
            .unwrap();
        assert_eq!(retired.reason(), RetirementReason::ChunkReplaced);
        assert_eq!(retired.record().counter(), 2);
        assert_eq!(retired.game_time(), 100);
    }
    #[test]
    fn native_counter_reload_mismatch_fault_is_sticky_and_removal_retires() {
        let mut world = world(1, "minecraft:overworld");
        let first = record(&world);
        world.remove_streamed_chunk(&(0, 0)).unwrap();
        world.insert_streamed_chunk(chunk(2));
        let fault = Fault::ReloadMismatch {
            actual: BlockStateId(2),
        };
        assert_eq!(world.counter_trial_snapshot().unwrap().fault(), Some(fault));
        assert_eq!(record(&world).instance(), first.instance());
        assert!(!record(&world).resident());
        assert_eq!(world.advance_counter_trial(true, true), Some(Err(fault)));
        write(&mut world, 0);
        assert!(world.counter_trial_snapshot().unwrap().record().is_none());
        write(&mut world, 1); // Fault never prevents the physical write.
        assert_eq!(world.block(2, 70, 2), BlockStateId(1));
        assert!(world.counter_trial_snapshot().unwrap().record().is_none());
        let final_snapshot = world.take_counter_trial().unwrap();
        assert_eq!(final_snapshot.fault(), Some(fault));
        assert_eq!(
            final_snapshot
                .last_retirement()
                .unwrap()
                .record()
                .instance(),
            first.instance()
        );
    }
    #[test]
    fn native_counter_world_swaps_keep_typed_records_and_scopes() {
        let mut first = world(1, "minecraft:overworld");
        let mut second = world(1, "minecraft:the_nether");
        first.game_time = 5;
        first.advance_counter_trial(true, true).unwrap().unwrap();
        second.game_time = 8;
        second.advance_counter_trial(true, true).unwrap().unwrap();
        let parked = first.counter_trial_snapshot().unwrap();
        std::mem::swap(&mut first, &mut second);
        first.game_time = 9;
        first.advance_counter_trial(true, true).unwrap().unwrap();
        assert_eq!(record(&first).counter(), 2);
        assert_eq!(
            first
                .counter_trial_snapshot()
                .unwrap()
                .configuration()
                .scope()
                .dimension(),
            "minecraft:the_nether"
        );
        assert_eq!(second.counter_trial_snapshot().unwrap(), parked);
        assert_eq!(
            first
                .take_counter_trial()
                .unwrap()
                .last_retirement()
                .unwrap()
                .record()
                .counter(),
            2
        );
    }
    #[test]
    fn native_counter_arithmetic_and_instance_exhaustion_preserve_commits() {
        let mut trial = Trial::new(configuration("minecraft:overworld"), Some(BlockStateId(1)));
        trial.record.as_mut().unwrap().counter = u64::MAX;
        assert_eq!(
            trial.advance(true, true, Some(BlockStateId(1)), 10),
            Err(Fault::CounterExhausted)
        );
        assert_eq!(trial.record.unwrap().counter(), u64::MAX);
        assert_eq!(trial.record.unwrap().last_advanced(), None);
        trial.changed(POSITION, BlockStateId(0), 11);
        assert!(trial.record.is_none());
        assert_eq!(trial.last_retirement.unwrap().record().counter(), u64::MAX);
        trial.changed(POSITION, BlockStateId(1), 12);
        assert!(trial.record.is_none());
        assert_eq!(trial.fault, Some(Fault::CounterExhausted));
        let allocator = AtomicU64::new(u64::MAX - 1);
        assert_eq!(allocate_instance(&allocator), Ok(u64::MAX));
        assert_eq!(allocate_instance(&allocator), Err(Fault::InstanceExhausted));
        let mut trial = Trial::new(configuration("minecraft:overworld"), Some(BlockStateId(1)));
        trial.retire(RetirementReason::BlockChanged, 1);
        let committed = trial.last_retirement;
        trial.admit_from(&allocator);
        assert_eq!(trial.fault, Some(Fault::InstanceExhausted));
        assert_eq!(trial.last_retirement, committed);
        assert!(trial.record.is_none());
        assert_eq!(
            trial.advance(true, true, Some(BlockStateId(1)), 2),
            Err(Fault::InstanceExhausted)
        );
    }
    fn guest_configuration() -> Configuration {
        configuration("minecraft:overworld")
            .with_guest_package(&captured(b"guest one", "counter_demo", "counter"))
            .unwrap()
    }
    fn guest_world() -> World {
        let mut w = world(1, "minecraft:overworld");
        w.take_counter_trial().unwrap();
        w.enable_counter_trial(guest_configuration()).unwrap();
        w
    }
    fn phase(w: &mut World) -> PhaseToken {
        w.prepare_guest_counter_phase(true, true).unwrap().unwrap()
    }
    fn proposal(token: &PhaseToken) -> Proposal {
        Proposal {
            world_instance: token.configuration().scope().world_instance(),
            instance: token.record().instance(),
            game_time: token.game_time(),
            expected_counter: token.record().counter(),
            next_counter: token.record().counter() + 1,
        }
    }
    #[test]
    fn guest_binding_uses_captured_owner_type_block_and_identity() {
        let config = configuration("minecraft:overworld");
        let first = config
            .clone()
            .with_guest_package(&captured(b"one", "counter_demo", "counter"))
            .unwrap();
        let second = config
            .clone()
            .with_guest_package(&captured(b"two", "counter_demo", "counter"))
            .unwrap();
        assert_ne!(first, second);
        assert!(
            matches!(first.backend(), Backend::Guest { primary_owner, .. } if primary_owner == "counter_demo")
        );
        assert_eq!(
            first
                .clone()
                .with_guest_package(&captured(b"one", "counter_demo", "counter")),
            Err(ConfigError::InvalidMetadata)
        );
        for (owner, block) in [("other", "counter"), ("counter_demo", "other")] {
            assert_eq!(
                config
                    .clone()
                    .with_guest_package(&captured(b"one", owner, block)),
                Err(ConfigError::InvalidMetadata)
            );
        }
        let mut different_type = config.clone();
        different_type.machine_type = captured(b"one", "other", "counter")
            .content()
            .machine_types()
            .get("other:counter_type")
            .unwrap()
            .clone();
        assert_eq!(
            different_type.with_guest_package(&captured(b"one", "counter_demo", "counter")),
            Err(ConfigError::InvalidMetadata)
        );
        assert_eq!(config.backend(), &Backend::Native);
    }
    #[test]
    fn guest_phase_commits_once_without_native_fallback_and_preserves_rewind() {
        let mut w = guest_world();
        let before = w.counter_trial_snapshot();
        assert_eq!(
            w.advance_counter_trial(true, true),
            Some(Err(Fault::DispatchMismatch))
        );
        assert_eq!(w.counter_trial_snapshot(), before);
        let first = phase(&mut w);
        let duplicate = phase(&mut w);
        let reply = proposal(&first);
        assert_eq!(w.counter_trial_snapshot(), before);
        assert_eq!(
            w.commit_guest_counter_phase(first, reply, true, true),
            Ok(Completion::Committed)
        );
        let committed = w.counter_trial_snapshot();
        assert_eq!(
            w.commit_guest_counter_phase(duplicate, reply, true, true),
            Ok(Completion::Stale)
        );
        assert_eq!(w.counter_trial_snapshot(), committed);
        assert!(w.prepare_guest_counter_phase(true, true).unwrap().is_none());
        w.game_time = -1;
        let token = phase(&mut w);
        let reply = proposal(&token);
        assert_eq!(
            w.commit_guest_counter_phase(token, reply, true, true),
            Ok(Completion::Committed)
        );
        assert_eq!(record(&w).counter(), 2);
        assert_eq!(record(&w).last_advanced(), Some(-1));
    }
    #[test]
    fn guest_prepare_and_completion_respect_fresh_authority_and_storage() {
        let mut w = guest_world();
        for (running, eligible) in [(false, true), (true, false)] {
            let before = w.counter_trial_snapshot();
            assert!(
                w.prepare_guest_counter_phase(running, eligible)
                    .unwrap()
                    .is_none()
            );
            let token = phase(&mut w);
            let reply = proposal(&token);
            assert_eq!(
                w.commit_guest_counter_phase(token, reply, running, eligible),
                Ok(Completion::Skipped)
            );
            let token = phase(&mut w);
            assert_eq!(
                w.fail_guest_counter_phase(token, GuestFailure::Trap, running, eligible),
                Ok(Completion::Skipped)
            );
            assert_eq!(w.counter_trial_snapshot(), before);
        }
        let token = phase(&mut w);
        let reply = proposal(&token);
        let lent = std::mem::take(&mut w.chunks);
        assert!(w.prepare_guest_counter_phase(true, true).unwrap().is_none());
        assert_eq!(
            w.commit_guest_counter_phase(token, reply, true, true),
            Ok(Completion::Skipped)
        );
        w.chunks = lent;
        let token = phase(&mut w);
        w.remove_streamed_chunk(&(0, 0)).unwrap();
        let before = w.counter_trial_snapshot();
        assert_eq!(
            w.fail_guest_counter_phase(token, GuestFailure::Trap, true, true),
            Ok(Completion::Stale)
        );
        assert_eq!(w.counter_trial_snapshot(), before);
        assert!(w.prepare_guest_counter_phase(true, true).unwrap().is_none());
        let mut absent = world(0, "minecraft:overworld");
        absent.take_counter_trial().unwrap();
        assert!(
            absent
                .prepare_guest_counter_phase(true, true)
                .unwrap()
                .is_none()
        );
        absent.enable_counter_trial(guest_configuration()).unwrap();
        assert!(
            absent
                .prepare_guest_counter_phase(true, true)
                .unwrap()
                .is_none()
        );
        let mut native = world(1, "minecraft:overworld");
        assert!(matches!(
            native.prepare_guest_counter_phase(true, true),
            Err(PhaseError::DispatchMismatch)
        ));
    }
    #[test]
    fn guest_invalid_replies_and_failures_latch_without_losing_committed_fields() {
        for invalid in 0..7 {
            let mut w = guest_world();
            let token = phase(&mut w);
            let mut reply = proposal(&token);
            match invalid {
                0 => reply.world_instance += 1,
                1 => reply.instance += 1,
                2 => reply.game_time += 1,
                3 => reply.expected_counter += 1,
                4 => reply.next_counter = 0,
                5 => reply.next_counter = 2,
                _ => reply.next_counter = u64::MAX,
            }
            let before = record(&w);
            let fault = Fault::GuestFailure(GuestFailure::InvalidReply);
            assert_eq!(
                w.commit_guest_counter_phase(token, reply, true, true),
                Err(PhaseError::Fault(fault))
            );
            assert_eq!(record(&w), before);
            assert!(
                matches!(w.prepare_guest_counter_phase(false, false), Err(PhaseError::Fault(f)) if f == fault)
            );
        }
        for failure in [
            GuestFailure::Trap,
            GuestFailure::Fuel,
            GuestFailure::Memory,
            GuestFailure::Rejected,
            GuestFailure::Unavailable,
            GuestFailure::Evaluation,
        ] {
            let mut w = guest_world();
            let token = phase(&mut w);
            let reply = proposal(&token);
            w.commit_guest_counter_phase(token, reply, true, true)
                .unwrap();
            w.game_time += 1;
            let token = phase(&mut w);
            let another = phase(&mut w);
            let before = record(&w);
            let fault = Fault::GuestFailure(failure);
            assert_eq!(
                w.fail_guest_counter_phase(token, failure, true, true),
                Err(PhaseError::Fault(fault))
            );
            assert_eq!(
                w.fail_guest_counter_phase(another, GuestFailure::InvalidReply, true, true),
                Err(PhaseError::Fault(fault))
            );
            assert_eq!(record(&w), before);
            write(&mut w, 0);
            write(&mut w, 1);
            assert!(w.counter_trial_snapshot().unwrap().record().is_none());
            assert_eq!(
                w.block(POSITION[0], POSITION[1], POSITION[2]),
                BlockStateId(1)
            );
        }
    }
    #[test]
    fn guest_stale_results_cannot_commit_or_fault_another_record_or_configuration() {
        for failure in [false, true] {
            for replacement in 0..5 {
                let mut w = guest_world();
                let token = phase(&mut w);
                let reply = proposal(&token);
                match replacement {
                    0 => {
                        write(&mut w, 0);
                        write(&mut w, 1);
                    }
                    1 => {
                        w.take_counter_trial().unwrap();
                        w.enable_counter_trial(guest_configuration()).unwrap();
                    }
                    2 => {
                        w.take_counter_trial().unwrap();
                        w.enable_counter_trial(configuration("minecraft:overworld"))
                            .unwrap();
                    }
                    3 => w.game_time += 1,
                    _ => {
                        w.take_counter_trial().unwrap();
                    }
                }
                let before = w.counter_trial_snapshot();
                let result = if failure {
                    w.fail_guest_counter_phase(token, GuestFailure::Trap, true, true)
                } else {
                    w.commit_guest_counter_phase(token, reply, true, true)
                };
                assert_eq!(result, Ok(Completion::Stale));
                assert_eq!(w.counter_trial_snapshot(), before);
            }
        }
        for change in 0..10 {
            let mut trial = Trial::new(guest_configuration(), Some(BlockStateId(1)));
            let token = trial
                .prepare_guest(true, true, Some(BlockStateId(1)), 1)
                .unwrap()
                .unwrap();
            let reply = proposal(&token);
            let config = Arc::make_mut(&mut trial.configuration);
            match change {
                0 => config.scope = AuthorityScope::new(8, "minecraft:overworld").unwrap(),
                1 => config.scope = AuthorityScope::new(7, "minecraft:the_nether").unwrap(),
                2 => config.position[0] += 1,
                3 => config.state.name = "counter_demo:other".into(),
                4 => config.state_id = BlockStateId(2),
                5 => {
                    config.backend = configuration("minecraft:overworld")
                        .with_guest_package(&captured(b"different", "counter_demo", "counter"))
                        .unwrap()
                        .backend
                }
                6 => trial.record.as_mut().unwrap().counter += 1,
                7 => trial.record.as_mut().unwrap().last_advanced = Some(0),
                8 => {
                    config.machine_type = captured(b"one", "other", "counter")
                        .content()
                        .machine_types()
                        .get("other:counter_type")
                        .unwrap()
                        .clone()
                }
                _ => {
                    config.state.properties.insert("axis".into(), "x".into());
                }
            }
            let before = trial.snapshot();
            assert_eq!(
                trial.complete_guest(token, Ok(reply), true, true, Some(BlockStateId(1)), 1),
                Ok(Completion::Stale)
            );
            assert_eq!(trial.snapshot(), before);
        }
    }
    #[test]
    fn guest_rechecks_actual_block_during_prepare_and_completion() {
        for prepared in [false, true] {
            let mut w = guest_world();
            let token = prepared.then(|| phase(&mut w));
            let before = record(&w);
            Arc::make_mut(w.chunks.get_mut(&(0, 0)).unwrap()).set(
                POSITION[0],
                POSITION[1],
                POSITION[2],
                2,
            );
            let fault = Fault::ResidentMismatch {
                actual: BlockStateId(2),
            };
            if let Some(token) = token {
                let reply = proposal(&token);
                assert_eq!(
                    w.commit_guest_counter_phase(token, reply, true, true),
                    Err(PhaseError::Fault(fault))
                );
            } else {
                assert!(
                    matches!(w.prepare_guest_counter_phase(true, true), Err(PhaseError::Fault(f)) if f == fault)
                );
            }
            assert_eq!(record(&w), before);
            assert_eq!(w.counter_trial_snapshot().unwrap().fault(), Some(fault));
        }
    }
    #[test]
    fn guest_and_native_precedence_preserve_duplicate_mismatch_and_overflow() {
        for guest in [false, true] {
            let config = if guest {
                guest_configuration()
            } else {
                configuration("minecraft:overworld")
            };
            let mut trial = Trial::new(config, Some(BlockStateId(1)));
            trial.record.as_mut().unwrap().last_advanced = Some(10);
            trial.record.as_mut().unwrap().counter = u64::MAX;
            if guest {
                assert!(
                    trial
                        .prepare_guest(true, true, Some(BlockStateId(1)), 10)
                        .unwrap()
                        .is_none()
                );
            } else {
                assert_eq!(
                    trial.advance(true, true, Some(BlockStateId(1)), 10),
                    Ok(false)
                );
            }
            let expected = Fault::ResidentMismatch {
                actual: BlockStateId(2),
            };
            if guest {
                assert!(
                    matches!(trial.prepare_guest(true, true, Some(BlockStateId(2)), 10), Err(PhaseError::Fault(f)) if f == expected)
                );
            } else {
                assert_eq!(
                    trial.advance(true, true, Some(BlockStateId(2)), 10),
                    Err(expected)
                );
            }
            assert_eq!(trial.record.unwrap().counter(), u64::MAX);
        }
        let mut trial = Trial::new(guest_configuration(), Some(BlockStateId(1)));
        trial.record.as_mut().unwrap().counter = u64::MAX;
        assert!(matches!(
            trial.prepare_guest(true, true, Some(BlockStateId(1)), 1),
            Err(PhaseError::Fault(Fault::CounterExhausted))
        ));
        assert_eq!(trial.record.unwrap().last_advanced(), None);
    }
}
