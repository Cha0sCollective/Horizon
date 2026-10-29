//! Opt-in transient ownership of one World-bound slot. No Game receiver, live
//! player adapter, callback, drops or persistence is provided by this experiment.
use crate::{
    counter_trial::{ConfigError, Configuration},
    mutation_observer::AuthorityScope,
    slot_transfer::{
        self, CommittedTransfer, Direction, NoMove, PreparedTransfer, ResolvedLimits,
        SlotProjection, SlotSnapshot, TransferError,
    },
    world::World,
};
use horizon_world::registry::BlockStateId;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct SlotKey {
    scope: AuthorityScope,
    trial: Arc<()>,
    incarnation: u64,
}
impl SlotKey {
    pub fn scope(&self) -> &AuthorityScope {
        &self.scope
    }
    pub fn incarnation(&self) -> u64 {
        self.incarnation
    }
}
impl PartialEq for SlotKey {
    fn eq(&self, other: &Self) -> bool {
        self.scope == other.scope
            && Arc::ptr_eq(&self.trial, &other.trial)
            && self.incarnation == other.incarnation
    }
}
impl Eq for SlotKey {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    IncarnationExhausted,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Disabled,
    PendingRetirement,
    Fault(Fault),
    StaleKey,
    NotResident,
    StateMismatch,
    Transfer(TransferError),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetirementReason {
    BlockChanged,
    ChunkReplaced,
    ReloadMismatch,
}

#[derive(Clone, Debug)]
pub struct ActiveSnapshot {
    pub key: SlotKey,
    pub slot: SlotSnapshot,
    pub resident: bool,
}
#[derive(Clone, Debug)]
pub struct RetirementSnapshot {
    pub allocation: ActiveSnapshot,
    pub reason: RetirementReason,
    pub game_time: i64,
}
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub configuration: Arc<Configuration>,
    pub active: Option<ActiveSnapshot>,
    pub pending: Option<RetirementSnapshot>,
    pub fault: Option<Fault>,
}

/// The projection is move-only; snapshots do not confer mutation authority.
#[derive(Debug)]
pub struct OwnedSlot {
    key: SlotKey,
    slot: SlotProjection,
    resident: bool,
}
impl OwnedSlot {
    pub fn snapshot(&self) -> ActiveSnapshot {
        ActiveSnapshot {
            key: self.key.clone(),
            slot: self.slot.snapshot(),
            resident: self.resident,
        }
    }
    pub fn into_parts(self) -> (SlotKey, SlotProjection, bool) {
        (self.key, self.slot, self.resident)
    }
}
/// An owned return value, never an overwriteable diagnostic or cloned item owner.
#[derive(Debug)]
pub struct PendingRetirement {
    allocation: OwnedSlot,
    reason: RetirementReason,
    game_time: i64,
}
impl PendingRetirement {
    pub fn snapshot(&self) -> RetirementSnapshot {
        RetirementSnapshot {
            allocation: self.allocation.snapshot(),
            reason: self.reason,
            game_time: self.game_time,
        }
    }
    pub fn into_parts(self) -> (OwnedSlot, RetirementReason, i64) {
        (self.allocation, self.reason, self.game_time)
    }
}
/// Explicitly transfers every retained owner before a World is discarded.
#[derive(Debug)]
pub struct Teardown {
    pub configuration: Arc<Configuration>,
    pub active: Option<OwnedSlot>,
    pub pending: Option<PendingRetirement>,
    pub fault: Option<Fault>,
}
#[derive(Debug)]
pub struct TransferPlan {
    key: SlotKey,
    transfer: PreparedTransfer,
}
#[derive(Debug)]
pub enum Preparation {
    NoMove(NoMove),
    Prepared(Box<TransferPlan>),
}

pub(crate) struct Trial {
    configuration: Arc<Configuration>,
    identity: Arc<()>,
    last_incarnation: u64,
    active: Option<OwnedSlot>,
    pending: Option<PendingRetirement>,
    fault: Option<Fault>,
}
impl Trial {
    fn new(configuration: Configuration, current: BlockStateId) -> Self {
        let mut trial = Self {
            configuration: Arc::new(configuration),
            identity: Arc::new(()),
            last_incarnation: 0,
            active: None,
            pending: None,
            fault: None,
        };
        if current == trial.configuration.state_id() {
            trial.admit();
        }
        trial
    }
    pub(crate) fn watches_chunk(&self, key: (i32, i32)) -> bool {
        self.configuration.chunk() == key
    }
    pub(crate) fn position(&self) -> [i32; 3] {
        self.configuration.position()
    }
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            configuration: self.configuration.clone(),
            active: self.active.as_ref().map(OwnedSlot::snapshot),
            pending: self.pending.as_ref().map(PendingRetirement::snapshot),
            fault: self.fault,
        }
    }
    fn admit(&mut self) {
        if self.active.is_some() || self.pending.is_some() || self.fault.is_some() {
            return;
        }
        let Some(incarnation) = self.last_incarnation.checked_add(1) else {
            self.fault = Some(Fault::IncarnationExhausted);
            return;
        };
        let key = SlotKey {
            scope: self.configuration.scope().clone(),
            trial: self.identity.clone(),
            incarnation,
        };
        self.active = Some(OwnedSlot {
            key,
            slot: SlotProjection::new(None),
            resident: true,
        });
        self.last_incarnation = incarnation;
    }
    fn retire(&mut self, reason: RetirementReason, game_time: i64) {
        // Admission is inhibited while pending: never take/drop a second owner.
        if self.pending.is_some() {
            return;
        }
        if let Some(allocation) = self.active.take() {
            self.pending = Some(PendingRetirement {
                allocation,
                reason,
                game_time,
            });
        }
    }
    pub(crate) fn changed(&mut self, position: [i32; 3], current: BlockStateId, game_time: i64) {
        if position != self.position() {
            return;
        }
        self.retire(RetirementReason::BlockChanged, game_time);
        if current == self.configuration.state_id() {
            self.admit();
        }
    }
    pub(crate) fn inserted(&mut self, replaced: bool, current: BlockStateId, game_time: i64) {
        if replaced {
            self.retire(RetirementReason::ChunkReplaced, game_time);
        }
        if self.pending.is_some() {
            return;
        }
        if self.active.is_some() {
            if current == self.configuration.state_id() {
                if let Some(active) = &mut self.active {
                    active.resident = true;
                }
            } else {
                self.retire(RetirementReason::ReloadMismatch, game_time);
            }
        } else if current == self.configuration.state_id() {
            self.admit();
        }
    }
    pub(crate) fn unloaded(&mut self) {
        if let Some(active) = &mut self.active {
            active.resident = false;
        }
    }
    fn available(&self) -> Result<(), Error> {
        if self.pending.is_some() {
            return Err(Error::PendingRetirement);
        }
        if let Some(fault) = self.fault {
            return Err(Error::Fault(fault));
        }
        Ok(())
    }
    fn ready(&self, key: &SlotKey, current: Option<BlockStateId>) -> Result<(), Error> {
        self.available()?;
        let active = self
            .active
            .as_ref()
            .filter(|a| a.key == *key)
            .ok_or(Error::StaleKey)?;
        if !active.resident || current.is_none() {
            return Err(Error::NotResident);
        }
        if current != Some(self.configuration.state_id()) {
            return Err(Error::StateMismatch);
        }
        Ok(())
    }
}

impl World {
    fn world_slot_current(&self) -> Option<BlockStateId> {
        let trial = self.world_slot_trial.as_ref()?;
        let [x, y, z] = trial.position();
        self.chunks
            .get(&trial.configuration.chunk())
            .map(|c| c.block(x & 15, y, z & 15))
    }
    /// Native library experiment only; no Game path enables this service.
    pub fn enable_world_slot_trial(
        &mut self,
        configuration: Configuration,
    ) -> Result<Snapshot, ConfigError> {
        if self.world_slot_trial.is_some() {
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
        let trial = Trial::new(configuration, current);
        let snapshot = trial.snapshot();
        self.world_slot_trial = Some(Box::new(trial));
        Ok(snapshot)
    }
    pub fn world_slot_snapshot(&self) -> Option<Snapshot> {
        self.world_slot_trial.as_ref().map(|t| t.snapshot())
    }
    pub fn read_world_slot(&self, key: &SlotKey) -> Result<ActiveSnapshot, Error> {
        self.world_slot_trial
            .as_ref()
            .ok_or(Error::Disabled)?
            .active
            .as_ref()
            .filter(|a| a.key == *key)
            .map(OwnedSlot::snapshot)
            .ok_or(Error::StaleKey)
    }
    pub fn prepare_world_slot_transfer(
        &self,
        key: &SlotKey,
        player: &SlotProjection,
        direction: Direction,
        amount: u32,
        limits: ResolvedLimits,
    ) -> Result<Preparation, Error> {
        let trial = self.world_slot_trial.as_ref().ok_or(Error::Disabled)?;
        trial.ready(key, self.world_slot_current())?;
        let active = trial.active.as_ref().ok_or(Error::StaleKey)?;
        match slot_transfer::prepare(&active.slot, player, direction, amount, limits)
            .map_err(Error::Transfer)?
        {
            slot_transfer::Preparation::NoMove(reason) => Ok(Preparation::NoMove(reason)),
            slot_transfer::Preparation::Prepared(transfer) => {
                Ok(Preparation::Prepared(Box::new(TransferPlan {
                    key: key.clone(),
                    transfer: *transfer,
                })))
            }
        }
    }
    pub fn commit_world_slot_transfer(
        &mut self,
        plan: TransferPlan,
        player: &mut SlotProjection,
        fresh_limits: ResolvedLimits,
    ) -> Result<CommittedTransfer, Error> {
        let current = self.world_slot_current();
        let trial = self.world_slot_trial.as_mut().ok_or(Error::Disabled)?;
        trial.ready(&plan.key, current)?;
        let active = trial.active.as_mut().ok_or(Error::StaleKey)?;
        plan.transfer
            .commit(&mut active.slot, player, fresh_limits)
            .map_err(Error::Transfer)
    }
    /// The caller now owns this value and must retain it if its sink refuses it.
    pub fn take_world_slot_retirement(&mut self) -> Option<PendingRetirement> {
        self.world_slot_trial.as_mut()?.pending.take()
    }
    pub fn reconcile_world_slot_trial(&mut self) -> Result<Snapshot, Error> {
        let current = self.world_slot_current();
        let trial = self.world_slot_trial.as_mut().ok_or(Error::Disabled)?;
        trial.available()?;
        let current = current.ok_or(Error::NotResident)?;
        if current == trial.configuration.state_id() {
            trial.admit();
        }
        Ok(trial.snapshot())
    }
    /// Move every owner out. No automatic Drop delivery or receiver is implied.
    pub fn take_world_slot_trial(&mut self) -> Option<Teardown> {
        self.world_slot_trial.take().map(|trial| Teardown {
            configuration: trial.configuration,
            active: trial.active,
            pending: trial.pending,
            fault: trial.fault,
        })
    }
}

#[cfg(test)]
mod tests;

#[cfg(feature = "paired-packages")]
impl World {
    /// Presence only, including empty, nonresident, pending, and faulted trials.
    /// This observation confers no ownership or save/publication authority.
    pub fn has_world_slot_trial(&self) -> bool {
        self.world_slot_trial.is_some()
    }
}

#[cfg(all(test, feature = "paired-packages"))]
mod presence_tests {
    use super::*;
    use crate::{chunk::Chunk, item_data::BundleSelected, items::ItemStack};
    use horizon_world::{chunk::BlockState, nbt::Tag};
    use std::fs;
    fn configured() -> (World, std::path::PathBuf) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "horizon-trial-presence-{}-{nonce}",
            std::process::id()
        ));
        for (name, bytes) in [
            ("horizon-paired.toml", b"format=1\nhost-contract='paired-preview-1'\n[content]\nid='presence'\nname='Presence'\nversion='0'\napi='0.2'\nside='both'\ncapabilities=['registries']\n[client]\nentry='client.wasm'\napi='0.1'\ncapabilities=[]\n[server]\nentry='server.wasm'\napi='0.3'\ncapabilities=[]\n".as_slice()),
            ("client.wasm", b"not executed".as_slice()), ("server.wasm", b"not executed".as_slice()),
            ("data/presence/horizon/machine/type.json", br#"{"format":1,"save-schema":1}"#.as_slice()),
            ("data/presence/horizon/block/cube.json", br#"{"template":"minecraft:iron_block","machine-type":"presence:type"}"#.as_slice()),
        ] { let path = root.join(name); fs::create_dir_all(path.parent().unwrap()).unwrap(); fs::write(path, bytes).unwrap(); }
        let bundle = horizon_mods::paired::PreparedBundle::read_folder(&root).unwrap();
        let configuration = Configuration::new(
            AuthorityScope::new(91, "minecraft:overworld").unwrap(),
            bundle
                .content()
                .machine_types()
                .get("presence:type")
                .unwrap(),
            BlockState {
                name: "presence:cube".into(),
                properties: Default::default(),
            },
            BlockStateId(1),
            [2, 70, 2],
        )
        .unwrap();
        let mut world = World::new(BlockStateId(0), 17);
        world.insert_streamed_chunk(Chunk::empty(0, 0));
        assert!(!world.has_world_slot_trial());
        world.enable_world_slot_trial(configuration).unwrap();
        (world, root)
    }
    #[test]
    fn presence_observes_empty_active_pending_nonresident_and_faulted_trials_without_taking() {
        for mode in 0..5 {
            let (mut world, root) = configured();
            if mode == 4 {
                world.world_slot_trial.as_mut().unwrap().last_incarnation = u64::MAX;
                world.set(2, 70, 2, BlockStateId(1)).unwrap();
                assert_eq!(
                    world.world_slot_snapshot().unwrap().fault,
                    Some(Fault::IncarnationExhausted)
                );
            } else if mode > 0 {
                world.set(2, 70, 2, BlockStateId(1)).unwrap();
                let key = world.world_slot_snapshot().unwrap().active.unwrap().key;
                let mut stack = ItemStack::new("minecraft:stone", 7);
                stack.data_mut().bundle_selected = BundleSelected(4);
                stack
                    .data_mut()
                    .set_other("fixture:opaque", Some(Tag::Int(-73)));
                let mut child = ItemStack::new("minecraft:dirt", 2);
                child.data_mut().bundle_selected = BundleSelected(9);
                stack.data_mut().bundle_contents.push(child);
                let mut player = SlotProjection::new(Some(stack));
                let limits = ResolvedLimits::new(64, 64).unwrap();
                let plan = match world
                    .prepare_world_slot_transfer(&key, &player, Direction::IntoMachine, 7, limits)
                    .unwrap()
                {
                    Preparation::Prepared(plan) => *plan,
                    other => panic!("native positive transfer: {other:?}"),
                };
                world
                    .commit_world_slot_transfer(plan, &mut player, limits)
                    .unwrap();
                if mode == 2 {
                    world.set(2, 70, 2, BlockStateId(0)).unwrap();
                }
                if mode == 3 {
                    world.remove_streamed_chunk(&(0, 0));
                }
            }
            let before = format!("{:?}", world.world_slot_snapshot());
            let allocation = world
                .world_slot_trial
                .as_ref()
                .map(|trial| trial.as_ref() as *const _ as usize);
            for _ in 0..3 {
                assert!(world.has_world_slot_trial());
            }
            assert_eq!(format!("{:?}", world.world_slot_snapshot()), before);
            assert_eq!(
                world
                    .world_slot_trial
                    .as_ref()
                    .map(|trial| trial.as_ref() as *const _ as usize),
                allocation
            );
            fs::remove_dir_all(root).unwrap();
        }
    }
}
