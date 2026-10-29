//! Dormant synchronous lend of one actual player inventory cell to a World slot.
use super::{
    Game,
    slot_retirement::{DeliveryError, check_counts},
};
use horizon_client::{
    items::ItemStack,
    slot_transfer::{
        CommittedTransfer, Direction, NoMove, ResolvedLimits, SlotProjection, TransferError,
    },
    world_slot_trial::{self, Preparation, SlotKey},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TransferOutcome {
    NoMove(NoMove),
    Committed(CommittedTransfer),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlayerTransferError {
    NotAuthoritative,
    NoPlayer,
    InvalidIndex,
    WrongContext,
    UnsupportedStack,
    Reservation,
    Limits(TransferError),
    World(world_slot_trial::Error),
}

/// Only this local cell lend is unwind-safe, not the enclosing guest-turn wrapper.
struct CellLend<'a> {
    cell: &'a mut Option<ItemStack>,
    projection: Option<SlotProjection>,
}
impl<'a> CellLend<'a> {
    fn new(cell: &'a mut Option<ItemStack>) -> Self {
        let projection = Some(SlotProjection::new(cell.take()));
        Self { cell, projection }
    }
    fn projection(&self) -> &SlotProjection {
        self.projection
            .as_ref()
            .expect("cell lend owns its projection")
    }
    fn projection_mut(&mut self) -> &mut SlotProjection {
        self.projection
            .as_mut()
            .expect("cell lend owns its projection")
    }
}
impl Drop for CellLend<'_> {
    fn drop(&mut self) {
        *self.cell = self
            .projection
            .take()
            .expect("cell lend restored once")
            .into_stack();
    }
}

impl Game<'_> {
    /// The opaque native key is an allocation selector, not player authorization.
    pub(super) fn transfer_world_slot_inventory_cell(
        &mut self,
        key: &SlotKey,
        index: usize,
        direction: Direction,
        requested: u32,
    ) -> Result<TransferOutcome, PlayerTransferError> {
        use PlayerTransferError as E;
        if self.remote.is_some() || self.integrated.is_some() {
            return Err(E::NotAuthoritative);
        }
        if self.player.is_none() {
            return Err(E::NoPlayer);
        }
        if index >= 36 || index >= self.inventory.len() {
            return Err(E::InvalidIndex);
        }
        let snapshot = self
            .world
            .world_slot_snapshot()
            .ok_or(E::World(world_slot_trial::Error::Disabled))?;
        if key.scope() != snapshot.configuration.scope()
            || snapshot.configuration.scope().dimension() != self.dim.id()
            || snapshot.active.as_ref().is_some_and(|a| a.key != *key)
        {
            return Err(E::WrongContext);
        }
        let machine = snapshot.active.as_ref().and_then(|a| a.slot.stack());
        let player = self.inventory[index].as_ref();
        for stack in [machine, player].into_iter().flatten() {
            if &*stack.item == "minecraft:air" || self.res.items().get(&stack.item).is_none() {
                return Err(E::UnsupportedStack);
            }
            check_counts(stack).map_err(|error| match error {
                DeliveryError::Reservation => E::Reservation,
                _ => E::UnsupportedStack,
            })?;
        }
        let source = match direction {
            Direction::IntoMachine => player,
            Direction::IntoPlayer => machine,
        };
        // Same ceiling as native plain inventory/container cells (CONTAINER_MAX).
        // The real Game resolver honors max_stack_size overrides and fallback.
        let limit = |stack: Option<&ItemStack>| {
            stack
                .or(source)
                .map_or(99, |s| self.stack_max_size(s).min(99))
        };
        let limits = ResolvedLimits::new(limit(machine), limit(player)).map_err(E::Limits)?;

        // Exclusive field borrows keep registry/component/slot facts current.
        // Neither the plan nor the ephemeral player projection escapes this call.
        let mut lend = CellLend::new(&mut self.inventory[index]);
        match self
            .world
            .prepare_world_slot_transfer(key, lend.projection(), direction, requested, limits)
            .map_err(E::World)?
        {
            Preparation::NoMove(reason) => Ok(TransferOutcome::NoMove(reason)),
            Preparation::Prepared(plan) => self
                .world
                .commit_world_slot_transfer(*plan, lend.projection_mut(), limits)
                .map(TransferOutcome::Committed)
                .map_err(E::World),
        }
    }
}
