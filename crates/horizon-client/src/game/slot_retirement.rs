//! Dormant native retirement sink. No Game path enables World slot trials yet.
use super::Game;
use horizon_client::{entities::ItemEntity, items::ItemStack, world_slot_trial::SlotKey};
use horizon_core::{geometry::Vec3, random::RandomSource};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Delivery {
    EmptyAcknowledged,
    Delivered { entity_id: u32 },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DeliveryError {
    NotAuthoritative,
    NoPending,
    WrongContext,
    NotResident,
    UnsupportedStack,
    EntityIdExhausted,
    Reservation,
}

/// Only the known count encoding boundary, not a general component/NBT validator.
pub(super) fn check_counts(root: &ItemStack) -> Result<(), DeliveryError> {
    let mut work = Vec::new();
    work.try_reserve(1)
        .map_err(|_| DeliveryError::Reservation)?;
    work.push(root);
    while let Some(stack) = work.pop() {
        if stack.count == 0 || stack.count > i32::MAX as u32 {
            return Err(DeliveryError::UnsupportedStack);
        }
        let Some(data) = stack.data() else {
            continue;
        };
        let pots = data
            .pot_decorations
            .as_ref()
            .map_or(0, |p| p.iter().flatten().count());
        let children = data
            .container
            .len()
            .checked_add(data.charged.len())
            .and_then(|n| n.checked_add(data.bundle_contents.len()))
            .and_then(|n| n.checked_add(pots))
            .ok_or(DeliveryError::Reservation)?;
        work.try_reserve(children)
            .map_err(|_| DeliveryError::Reservation)?;
        work.extend(data.container.iter().map(|(_, stack)| stack));
        work.extend(data.charged.iter());
        work.extend(data.bundle_contents.iter());
        if let Some(pots) = &data.pot_decorations {
            work.extend(pots.iter().flatten());
        }
    }
    Ok(())
}

impl Game<'_> {
    /// `expected` selects a native allocation; it is not player authentication.
    pub(super) fn deliver_pending_world_slot(
        &mut self,
        expected: &SlotKey,
    ) -> Result<Delivery, DeliveryError> {
        if self.remote.is_some() || self.integrated.is_some() {
            return Err(DeliveryError::NotAuthoritative);
        }
        let snapshot = self
            .world
            .world_slot_snapshot()
            .ok_or(DeliveryError::NoPending)?;
        let pending = snapshot.pending.as_ref().ok_or(DeliveryError::NoPending)?;
        if pending.allocation.key != *expected
            || pending.allocation.key.scope() != snapshot.configuration.scope()
            || snapshot.configuration.scope().dimension() != self.dim.id()
        {
            return Err(DeliveryError::WrongContext);
        }
        let [x, y, z] = snapshot.configuration.position();
        // `level.dimension` is player metadata; levels::swap_level does not swap it.
        // The pending resident flag may describe the old allocation before reload.
        if !self.world.chunks.contains_key(&(x >> 4, z >> 4)) {
            return Err(DeliveryError::NotResident);
        }
        let Some(stack) = pending.allocation.slot.stack() else {
            let result = Delivery::EmptyAcknowledged;
            self.world
                .take_world_slot_retirement()
                .ok_or(DeliveryError::NoPending)?;
            return Ok(result);
        };
        if &*stack.item == "minecraft:air" || self.res.items().get(&stack.item).is_none() {
            return Err(DeliveryError::UnsupportedStack);
        }
        check_counts(stack)?;
        let entity_id = self
            .next_entity
            .checked_add(1)
            .ok_or(DeliveryError::EntityIdExhausted)?;
        self.dropped
            .try_reserve(1)
            .map_err(|_| DeliveryError::Reservation)?;
        let mut random = self.particles.random().clone();
        let bob_offset = random.next_float() * std::f32::consts::PI * 2.0;
        let position = Vec3::new(f64::from(x) + 0.5, f64::from(y) + 0.5, f64::from(z) + 0.5);
        let velocity = Vec3::new(0.0, 0.0, 0.0);
        let result = Delivery::Delivered { entity_id };

        // Exclusive Game access and no callbacks preserve the inspected pending
        // value. Nothing fallible follows a successful take of its owned slot.
        let pending = self
            .world
            .take_world_slot_retirement()
            .ok_or(DeliveryError::NoPending)?;
        let (allocation, _, _) = pending.into_parts();
        let (_, projection, _) = allocation.into_parts();
        let stack = projection
            .into_stack()
            .expect("positive retirement validated before take");
        let entity = ItemEntity {
            health: 5,
            fire_ticks: 0,
            thrower: None,
            target: None,
            id: entity_id,
            stack,
            position,
            previous: position,
            velocity,
            on_ground: false,
            age: 0,
            pickup_delay: 10,
            bob_offset,
            ticks: 0,
        };
        self.next_entity = entity_id;
        *self.particles.random() = random;
        self.dropped.push(entity);
        Ok(result)
    }
}
