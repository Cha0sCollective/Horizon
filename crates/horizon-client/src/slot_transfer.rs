//! Experimental transfers between two detached native slot projections.
//! No live inventory, player authority, replay protocol or persistence is provided.
use crate::items::ItemStack;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    IntoMachine,
    IntoPlayer,
}

/// Limits resolved by a native caller, not by a guest proposal. The future live
/// adapter must resolve these again from the actual registry, components and slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedLimits {
    machine: u32,
    player: u32,
}
impl ResolvedLimits {
    pub fn new(machine: u32, player: u32) -> Result<Self, TransferError> {
        if machine == 0 || player == 0 {
            return Err(TransferError::ZeroLimit);
        }
        Ok(Self { machine, player })
    }
    pub fn machine(self) -> u32 {
        self.machine
    }
    pub fn player(self) -> u32 {
        self.player
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferError {
    ZeroLimit,
    AliasedSlots,
    Stale,
    RevisionExhausted,
    ArithmeticOverflow,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoMove {
    ZeroRequested,
    EmptySource,
    Incompatible,
    NoCapacity,
}

/// Owns one detached value. There is deliberately no Clone or mutable stack view.
#[derive(Debug)]
pub struct SlotProjection {
    identity: Arc<()>,
    revision: u64,
    stack: Option<ItemStack>,
}
#[derive(Clone, Debug)]
pub struct SlotSnapshot {
    identity: Arc<()>,
    revision: u64,
    stack: Option<ItemStack>,
}
impl SlotSnapshot {
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn stack(&self) -> Option<&ItemStack> {
        self.stack.as_ref()
    }
    fn matches(&self, slot: &SlotProjection) -> bool {
        Arc::ptr_eq(&self.identity, &slot.identity)
            && self.revision == slot.revision
            && self.stack == slot.stack
    }
}
impl SlotProjection {
    /// Move an already validated durable value into fresh projection authority.
    pub(crate) fn restored(stack: Option<ItemStack>, revision: u64) -> Self {
        Self {
            identity: Arc::new(()),
            revision,
            stack,
        }
    }
    pub(crate) fn borrowed(&self) -> (u64, Option<&ItemStack>) {
        (self.revision, self.stack.as_ref())
    }
    pub fn new(stack: Option<ItemStack>) -> Self {
        Self {
            identity: Arc::new(()),
            revision: 0,
            stack,
        }
    }
    pub fn snapshot(&self) -> SlotSnapshot {
        SlotSnapshot {
            identity: self.identity.clone(),
            revision: self.revision,
            stack: self.stack.clone(),
        }
    }
    /// Consume the projection, preserving the exact original stack representation.
    pub fn into_stack(self) -> Option<ItemStack> {
        self.stack
    }
    /// Native equality ignores runtime bundle selection, including nested stacks.
    /// An equal replacement must retain the original raw value, not publish `stack`.
    pub fn replace(&mut self, stack: Option<ItemStack>) -> Result<(), TransferError> {
        if self.stack == stack {
            return Ok(());
        }
        let revision = next_revision(self.revision)?;
        self.stack = stack;
        self.revision = revision;
        Ok(())
    }
}
fn next_revision(revision: u64) -> Result<u64, TransferError> {
    revision
        .checked_add(1)
        .ok_or(TransferError::RevisionExhausted)
}

#[derive(Debug)]
pub enum Preparation {
    NoMove(NoMove),
    Prepared(Box<PreparedTransfer>),
}
/// Consumed by commit; dropping it aborts without touching either projection.
#[derive(Debug)]
pub struct PreparedTransfer {
    machine: SlotSnapshot,
    player: SlotSnapshot,
    after_machine: Option<ItemStack>,
    after_player: Option<ItemStack>,
    direction: Direction,
    requested: u32,
    moved: u32,
    limits: ResolvedLimits,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommittedTransfer {
    pub moved: u32,
    pub machine_revision: u64,
    pub player_revision: u64,
}

pub fn prepare(
    machine: &SlotProjection,
    player: &SlotProjection,
    direction: Direction,
    requested: u32,
    limits: ResolvedLimits,
) -> Result<Preparation, TransferError> {
    if Arc::ptr_eq(&machine.identity, &player.identity) {
        return Err(TransferError::AliasedSlots);
    }
    if requested == 0 {
        return Ok(Preparation::NoMove(NoMove::ZeroRequested));
    }
    let (source, destination, limit) = match direction {
        Direction::IntoMachine => (&player.stack, &machine.stack, limits.machine),
        Direction::IntoPlayer => (&machine.stack, &player.stack, limits.player),
    };
    let Some(source) = source.as_ref().filter(|s| s.count > 0) else {
        return Ok(Preparation::NoMove(NoMove::EmptySource));
    };
    if destination
        .as_ref()
        .is_some_and(|s| !s.same_item_same_components(source))
    {
        return Ok(Preparation::NoMove(NoMove::Incompatible));
    }
    let have = destination.as_ref().map_or(0, |s| s.count);
    let moved = requested.min(source.count).min(limit.saturating_sub(have));
    if moved == 0 {
        return Ok(Preparation::NoMove(NoMove::NoCapacity));
    }
    let remaining = source
        .count
        .checked_sub(moved)
        .ok_or(TransferError::ArithmeticOverflow)?;
    let destination_count = have
        .checked_add(moved)
        .ok_or(TransferError::ArithmeticOverflow)?;
    let after_source = (remaining > 0).then(|| ItemStack {
        count: remaining,
        ..source.clone()
    });
    let after_destination = Some(ItemStack {
        count: destination_count,
        ..destination.as_ref().unwrap_or(source).clone()
    });
    let (after_machine, after_player) = match direction {
        Direction::IntoMachine => (after_destination, after_source),
        Direction::IntoPlayer => (after_source, after_destination),
    };
    Ok(Preparation::Prepared(Box::new(PreparedTransfer {
        machine: machine.snapshot(),
        player: player.snapshot(),
        after_machine,
        after_player,
        direction,
        requested,
        moved,
        limits,
    })))
}
impl PreparedTransfer {
    pub fn moved(&self) -> u32 {
        self.moved
    }
    pub fn direction(&self) -> Direction {
        self.direction
    }
    pub fn requested(&self) -> u32 {
        self.requested
    }
    pub fn limits(&self) -> ResolvedLimits {
        self.limits
    }
    pub fn after_machine(&self) -> Option<&ItemStack> {
        self.after_machine.as_ref()
    }
    pub fn after_player(&self) -> Option<&ItemStack> {
        self.after_player.as_ref()
    }

    pub fn commit(
        self,
        machine: &mut SlotProjection,
        player: &mut SlotProjection,
        fresh_limits: ResolvedLimits,
    ) -> Result<CommittedTransfer, TransferError> {
        if self.limits != fresh_limits
            || !self.machine.matches(machine)
            || !self.player.matches(player)
        {
            return Err(TransferError::Stale);
        }
        let machine_revision = next_revision(machine.revision)?;
        let player_revision = next_revision(player.revision)?;
        let result = CommittedTransfer {
            moved: self.moved,
            machine_revision,
            player_revision,
        };
        // All checks and result construction precede publication. No callbacks or IO.
        machine.stack = self.after_machine;
        machine.revision = machine_revision;
        player.stack = self.after_player;
        player.revision = player_revision;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_data::BundleSelected;
    use horizon_world::nbt::Tag;

    fn stack(count: u32) -> ItemStack {
        ItemStack::new("minecraft:stone", count)
    }
    #[test]
    fn restored_projection_moves_box_preserves_revision_and_rejects_old_identity() {
        let value = rich(7, -1);
        let address = value.extra.as_deref().unwrap() as *const _;
        let restored = SlotProjection::restored(Some(value), u64::MAX);
        let (revision, stack) = restored.borrowed();
        assert_eq!(revision, u64::MAX);
        assert_eq!(
            stack.unwrap().extra.as_deref().unwrap() as *const _,
            address
        );
        let second = SlotProjection::restored(Some(rich(7, -1)), u64::MAX);
        assert!(!restored.snapshot().matches(&second));
        let value = restored.into_stack().unwrap();
        assert_eq!(value.extra.as_deref().unwrap() as *const _, address);
    }
    fn rich(count: u32, selection: i32) -> ItemStack {
        let mut value = stack(count);
        value.damage = 7;
        let data = value.data_mut();
        data.custom_name = Some("retained".into());
        data.set_other(
            "example:opaque",
            Some(Tag::compound([("payload", Tag::Long(42))])),
        );
        data.bundle_selected = BundleSelected(selection);
        let mut nested = stack(1);
        nested.data_mut().bundle_selected = BundleSelected(selection + 10);
        data.container.push((3, nested));
        value
    }
    fn selections(value: &ItemStack) -> (i32, i32) {
        let data = value.data().unwrap();
        (
            data.bundle_selected.0,
            data.container[0].1.data().unwrap().bundle_selected.0,
        )
    }
    #[test]
    fn consuming_extraction_preserves_none_zero_and_original_metadata_allocation() {
        assert!(SlotProjection::new(None).into_stack().is_none());
        for count in [0, 3] {
            let stack = rich(count, 4);
            let original_box = stack.extra.as_deref().unwrap() as *const _;
            let extracted = SlotProjection::new(Some(stack)).into_stack().unwrap();
            assert_eq!(extracted.count, count);
            assert_eq!(selections(&extracted), (4, 14));
            assert_eq!(
                extracted.extra.as_deref().unwrap() as *const _,
                original_box
            );
            assert_eq!(extracted, rich(count, 4));
        }
    }
    fn limits(n: u32) -> ResolvedLimits {
        ResolvedLimits::new(n, n).unwrap()
    }
    fn plan(
        m: &SlotProjection,
        p: &SlotProjection,
        d: Direction,
        n: u32,
        l: ResolvedLimits,
    ) -> PreparedTransfer {
        match prepare(m, p, d, n, l).unwrap() {
            Preparation::Prepared(plan) => *plan,
            other => panic!("expected transfer: {other:?}"),
        }
    }
    fn unchanged(before: &[SlotSnapshot; 2], m: &SlotProjection, p: &SlotProjection) {
        assert!(before[0].matches(m));
        assert!(before[1].matches(p));
        for (old, current) in before.iter().zip([m, p]) {
            if let Some(value) = old.stack().filter(|v| v.custom_name() == Some("retained")) {
                assert_eq!(
                    selections(value),
                    selections(current.stack.as_ref().unwrap())
                );
            }
        }
    }
    fn conserve(before: &[SlotSnapshot; 2], after: &[SlotSnapshot; 2]) {
        // Include identities found on either side: loss or manufacture of a distinct
        // damage/component identity must fail even when total item-name counts match.
        for identity in before.iter().chain(after).filter_map(|s| s.stack()) {
            let total = |slots: &[SlotSnapshot; 2]| -> u64 {
                slots
                    .iter()
                    .filter_map(|s| s.stack())
                    .filter(|s| s.same_item_same_components(identity))
                    .map(|s| u64::from(s.count))
                    .sum()
            };
            assert_eq!(total(before), total(after));
        }
    }

    #[test]
    fn exact_partial_and_native_limits_conserve_both_directions() {
        for direction in [Direction::IntoMachine, Direction::IntoPlayer] {
            for (source, dest, requested, cap, moved) in [
                (8, 0, 8, 64, 8),
                (8, 60, 8, 64, 4),
                (2, 0, 9, 64, 2),
                (8, 0, 3, 64, 3),
                (8, 0, 8, 1, 1),
                (120, 90, 120, 99, 9),
                (u32::MAX, 1, u32::MAX, u32::MAX, u32::MAX - 1),
            ] {
                let (source_count, destination_count) = (source, dest);
                let source = Some(rich(source, 1));
                let dest = (dest > 0).then(|| rich(dest, 2));
                let destination_present = dest.is_some();
                let (m, p) = match direction {
                    Direction::IntoMachine => (dest, source),
                    Direction::IntoPlayer => (source, dest),
                };
                let (mut m, mut p) = (SlotProjection::new(m), SlotProjection::new(p));
                let before = [m.snapshot(), p.snapshot()];
                let prepared = plan(&m, &p, direction, requested, limits(cap));
                assert_eq!(prepared.moved(), moved);
                assert_eq!(prepared.direction(), direction);
                assert_eq!(prepared.requested(), requested);
                assert_eq!(prepared.limits(), limits(cap));
                let result = prepared.commit(&mut m, &mut p, limits(cap)).unwrap();
                assert_eq!(
                    result,
                    CommittedTransfer {
                        moved,
                        machine_revision: 1,
                        player_revision: 1
                    }
                );
                let after = [m.snapshot(), p.snapshot()];
                conserve(&before, &after);
                let (destination, remainder) = match direction {
                    Direction::IntoMachine => (&m, &p),
                    Direction::IntoPlayer => (&p, &m),
                };
                assert_eq!(
                    destination.stack.as_ref().unwrap().count,
                    destination_count + moved
                );
                assert_eq!(
                    remainder.stack.as_ref().map(|s| s.count),
                    (source_count > moved).then_some(source_count - moved)
                );
                assert_eq!(
                    selections(destination.stack.as_ref().unwrap()),
                    if destination_present {
                        (2, 12)
                    } else {
                        (1, 11)
                    }
                );
                if let Some(remainder) = &remainder.stack {
                    assert_eq!(selections(remainder), (1, 11));
                }
            }
        }
        assert_eq!(ResolvedLimits::new(0, 64), Err(TransferError::ZeroLimit));
        assert_eq!(ResolvedLimits::new(64, 0), Err(TransferError::ZeroLimit));
    }

    #[test]
    fn no_move_preserves_zero_and_oversized_representations() {
        let mut different = rich(1, 2);
        different.damage += 1;
        let mut component = rich(1, 2);
        component
            .data_mut()
            .set_other("example:opaque", Some(Tag::Int(4)));
        let cases = [
            (None, None, 1, NoMove::EmptySource),
            (None, Some(rich(0, 1)), 1, NoMove::EmptySource),
            (Some(rich(0, 2)), Some(rich(1, 1)), 0, NoMove::ZeroRequested),
            (Some(rich(64, 2)), Some(rich(1, 1)), 1, NoMove::NoCapacity),
            (
                Some(rich(u32::MAX, 2)),
                Some(rich(1, 1)),
                u32::MAX,
                NoMove::NoCapacity,
            ),
            (
                Some(ItemStack::new("minecraft:dirt", 0)),
                Some(rich(1, 1)),
                1,
                NoMove::Incompatible,
            ),
            (Some(different), Some(rich(1, 1)), 1, NoMove::Incompatible),
            (Some(component), Some(rich(1, 1)), 1, NoMove::Incompatible),
            (
                Some(stack(0)),
                Some({
                    let mut s = stack(1);
                    s.data_mut();
                    s
                }),
                1,
                NoMove::Incompatible,
            ),
        ];
        for (m, p, requested, reason) in cases {
            let (m, p) = (SlotProjection::new(m), SlotProjection::new(p));
            let before = [m.snapshot(), p.snapshot()];
            assert!(
                matches!(prepare(&m, &p, Direction::IntoMachine, requested, limits(64)).unwrap(), Preparation::NoMove(r) if r == reason)
            );
            unchanged(&before, &m, &p);
        }
    }

    #[test]
    fn compatible_zero_destination_keeps_its_selection_and_oversized_source_drains() {
        let (mut m, mut p) = (
            SlotProjection::new(Some(rich(0, 2))),
            SlotProjection::new(Some(rich(100, 1))),
        );
        let before = [m.snapshot(), p.snapshot()];
        plan(&m, &p, Direction::IntoMachine, u32::MAX, limits(64))
            .commit(&mut m, &mut p, limits(64))
            .unwrap();
        assert_eq!(m.stack.as_ref().unwrap().count, 64);
        assert_eq!(p.stack.as_ref().unwrap().count, 36);
        assert_eq!(selections(m.stack.as_ref().unwrap()), (2, 12));
        assert_eq!(selections(p.stack.as_ref().unwrap()), (1, 11));
        conserve(&before, &[m.snapshot(), p.snapshot()]);
    }

    #[test]
    fn equal_replace_retains_actual_top_level_and_nested_selection() {
        let (mut m, mut p) = (
            SlotProjection::new(None),
            SlotProjection::new(Some(rich(2, 1))),
        );
        let prepared = plan(&m, &p, Direction::IntoMachine, 1, limits(64));
        p.replace(Some(rich(2, 99))).unwrap();
        assert_eq!(p.revision, 0);
        assert_eq!(selections(p.stack.as_ref().unwrap()), (1, 11));
        prepared.commit(&mut m, &mut p, limits(64)).unwrap();
        assert_eq!(selections(m.stack.as_ref().unwrap()), (1, 11));
        assert_eq!(selections(p.stack.as_ref().unwrap()), (1, 11));
        p.revision = u64::MAX;
        p.replace(Some(rich(1, 99))).unwrap();
        assert_eq!(selections(p.stack.as_ref().unwrap()), (1, 11));
    }

    #[test]
    fn abort_duplicate_stale_limits_aba_and_new_allocation_publish_nothing() {
        let (mut m, mut p) = (
            SlotProjection::new(None),
            SlotProjection::new(Some(rich(8, 1))),
        );
        let before = [m.snapshot(), p.snapshot()];
        drop(plan(&m, &p, Direction::IntoMachine, 1, limits(64)));
        unchanged(&before, &m, &p);
        let first = plan(&m, &p, Direction::IntoMachine, 1, limits(64));
        let duplicate = plan(&m, &p, Direction::IntoMachine, 1, limits(64));
        first.commit(&mut m, &mut p, limits(64)).unwrap();
        let before = [m.snapshot(), p.snapshot()];
        assert_eq!(
            duplicate.commit(&mut m, &mut p, limits(64)),
            Err(TransferError::Stale)
        );
        unchanged(&before, &m, &p);
        for changed in [
            ResolvedLimits::new(99, 64).unwrap(),
            ResolvedLimits::new(64, 99).unwrap(),
        ] {
            let prepared = plan(&m, &p, Direction::IntoMachine, 1, limits(64));
            assert_eq!(
                prepared.commit(&mut m, &mut p, changed),
                Err(TransferError::Stale)
            );
            unchanged(&before, &m, &p);
        }
        let prepared = plan(&m, &p, Direction::IntoMachine, 1, limits(64));
        let old = p.stack.clone();
        p.replace(Some(stack(3))).unwrap();
        p.replace(old).unwrap();
        let before = [m.snapshot(), p.snapshot()];
        assert_eq!(
            prepared.commit(&mut m, &mut p, limits(64)),
            Err(TransferError::Stale)
        );
        unchanged(&before, &m, &p);
        let prepared = plan(&m, &p, Direction::IntoMachine, 1, limits(64));
        let mut reconstructed = SlotProjection::new(m.stack.clone());
        reconstructed.revision = m.revision;
        let before = [reconstructed.snapshot(), p.snapshot()];
        assert_eq!(
            prepared.commit(&mut reconstructed, &mut p, limits(64)),
            Err(TransferError::Stale)
        );
        unchanged(&before, &reconstructed, &p);
        assert_eq!(
            prepare(&m, &m, Direction::IntoMachine, 1, limits(64)).unwrap_err(),
            TransferError::AliasedSlots
        );
    }

    #[test]
    fn revision_exhaustion_preflights_both_sides_in_both_directions() {
        for direction in [Direction::IntoMachine, Direction::IntoPlayer] {
            for exhausted_machine in [true, false] {
                let (mut m, mut p) = (
                    SlotProjection::new(Some(rich(3, 2))),
                    SlotProjection::new(Some(rich(3, 1))),
                );
                if exhausted_machine {
                    m.revision = u64::MAX;
                } else {
                    p.revision = u64::MAX;
                }
                let before = [m.snapshot(), p.snapshot()];
                let prepared = plan(&m, &p, direction, 1, limits(64));
                assert_eq!(
                    prepared.commit(&mut m, &mut p, limits(64)),
                    Err(TransferError::RevisionExhausted)
                );
                unchanged(&before, &m, &p);
                assert_eq!(selections(m.stack.as_ref().unwrap()), (2, 12));
                assert_eq!(selections(p.stack.as_ref().unwrap()), (1, 11));
                let slot = if exhausted_machine { &mut m } else { &mut p };
                let old = slot.snapshot();
                assert_eq!(slot.replace(None), Err(TransferError::RevisionExhausted));
                assert!(old.matches(slot));
            }
        }
    }
}
