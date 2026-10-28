//! Experimental, passive observations of successful resident writes.
//!
//! IDs and sequences belong to the supplied authority instance, not a save or
//! transport schema. This is not a transaction journal or machine lifecycle API.
use horizon_world::registry::BlockStateId;
use std::{fmt, num::NonZeroU64, sync::Arc};

pub const MAX_EVENTS: usize = 4096;
pub const MAX_WATCHED_STATES: usize = 65_536;
const MAX_DIMENSION_BYTES: usize = 256;

/// Host-supplied authority context, fixed before any observation is made.
/// The instance is transient; it is not persistent world identity or generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorityScope {
    world_instance: NonZeroU64,
    dimension: Box<str>,
}
impl AuthorityScope {
    pub fn new(world_instance: u64, dimension: &str) -> Result<Self, ConfigError> {
        let world_instance =
            NonZeroU64::new(world_instance).ok_or(ConfigError::InvalidWorldInstance)?;
        if dimension.len() > MAX_DIMENSION_BYTES {
            return Err(ConfigError::InvalidDimension);
        }
        let valid = dimension.split_once(':').is_some_and(|(namespace, path)| {
            let token = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_.-".contains(&c);
            !namespace.is_empty()
                && !path.is_empty()
                && namespace.bytes().all(token)
                && path.bytes().all(|c| token(c) || c == b'/')
        });
        if !valid {
            return Err(ConfigError::InvalidDimension);
        }
        Ok(Self {
            world_instance,
            dimension: dimension.into(),
        })
    }
    pub fn world_instance(&self) -> u64 {
        self.world_instance.get()
    }
    pub fn dimension(&self) -> &str {
        &self.dimension
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    InvalidWorldInstance,
    InvalidDimension,
    InvalidCapacity,
    EmptyWatchedStates,
    TooManyWatchedStates,
    AlreadyEnabled,
}
impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidWorldInstance => "mutation observer needs a nonzero authority instance",
            Self::InvalidDimension => "mutation observer needs a bounded canonical dimension ID",
            Self::InvalidCapacity => "mutation observer capacity must be in 1..=4096",
            Self::EmptyWatchedStates => "mutation observer needs at least one watched state",
            Self::TooManyWatchedStates => {
                "mutation observer accepts at most 65536 watched state IDs"
            }
            Self::AlreadyEnabled => {
                "mutation observer is already enabled; explicitly take it before replacement"
            }
        })
    }
}
impl std::error::Error for ConfigError {}

/// Cause classification has not been established at the resident write seam.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    Unclassified,
}

/// One committed transition. Numeric states are valid only for this session's
/// frozen registry; later consumers must resolve canonical identities there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mutation {
    scope: Arc<AuthorityScope>,
    position: [i32; 3],
    previous: BlockStateId,
    current: BlockStateId,
    game_time: i64,
    sequence: u64,
    cause: Cause,
}
impl Mutation {
    pub fn scope(&self) -> &AuthorityScope {
        &self.scope
    }
    pub fn position(&self) -> [i32; 3] {
        self.position
    }
    pub fn previous(&self) -> BlockStateId {
        self.previous
    }
    pub fn current(&self) -> BlockStateId {
        self.current
    }
    pub fn game_time(&self) -> i64 {
        self.game_time
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn cause(&self) -> Cause {
        self.cause
    }
}

/// Sticky faults invalidate the observation batch without changing world writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObserverFault {
    Overflow { capacity: usize, sequence: u64 },
    SequenceExhausted,
}
impl fmt::Display for ObserverFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overflow { capacity, sequence } => {
                write!(
                    f,
                    "mutation observer exceeded {capacity} events at sequence {sequence}"
                )
            }
            Self::SequenceExhausted => f.write_str("mutation observer sequence exhausted"),
        }
    }
}
impl std::error::Error for ObserverFault {}

/// Host-owned observer. No public method records speculative or guest writes.
/// It cannot be cloned, silently reset or drained successfully after a fault.
#[derive(Debug)]
pub struct Observer {
    scope: Arc<AuthorityScope>,
    watched: Vec<u32>,
    capacity: usize,
    pending: Vec<Mutation>,
    next_sequence: Option<u64>,
    fault: Option<ObserverFault>,
}
impl Observer {
    pub(crate) fn new(
        scope: AuthorityScope,
        watched: impl IntoIterator<Item = BlockStateId>,
        capacity: usize,
    ) -> Result<Self, ConfigError> {
        if !(1..=MAX_EVENTS).contains(&capacity) {
            return Err(ConfigError::InvalidCapacity);
        }
        let mut states = Vec::new();
        for (i, state) in watched.into_iter().enumerate() {
            // Bound inputs too, so a duplicate-only iterator cannot run forever.
            if i == MAX_WATCHED_STATES {
                return Err(ConfigError::TooManyWatchedStates);
            }
            states.push(state.0);
        }
        if states.is_empty() {
            return Err(ConfigError::EmptyWatchedStates);
        }
        states.sort_unstable();
        states.dedup();
        Ok(Self {
            scope: Arc::new(scope),
            watched: states,
            capacity,
            pending: Vec::new(),
            next_sequence: Some(1),
            fault: None,
        })
    }
    pub fn scope(&self) -> &AuthorityScope {
        &self.scope
    }
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
    pub fn fault(&self) -> Option<ObserverFault> {
        self.fault
    }
    pub(crate) fn record(
        &mut self,
        position: [i32; 3],
        previous: BlockStateId,
        current: BlockStateId,
        game_time: i64,
    ) {
        if self.fault.is_some()
            || previous == current
            || !(self.watched.binary_search(&previous.0).is_ok()
                || self.watched.binary_search(&current.0).is_ok())
        {
            return;
        }
        let Some(sequence) = self.next_sequence else {
            self.fault = Some(ObserverFault::SequenceExhausted);
            return;
        };
        if self.pending.len() == self.capacity {
            self.fault = Some(ObserverFault::Overflow {
                capacity: self.capacity,
                sequence,
            });
            return;
        }
        self.pending.push(Mutation {
            scope: self.scope.clone(),
            position,
            previous,
            current,
            game_time,
            sequence,
            cause: Cause::Unclassified,
        });
        self.next_sequence = sequence.checked_add(1);
    }
    pub fn drain(&mut self) -> Result<Vec<Mutation>, ObserverFault> {
        if let Some(fault) = self.fault {
            return Err(fault);
        }
        Ok(std::mem::take(&mut self.pending))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> AuthorityScope {
        AuthorityScope::new(7, "minecraft:overworld").unwrap()
    }

    #[test]
    fn configuration_is_bounded_before_enabling() {
        assert_eq!(
            AuthorityScope::new(0, "minecraft:overworld"),
            Err(ConfigError::InvalidWorldInstance)
        );
        for dimension in [
            "",
            "overworld",
            "Minecraft:overworld",
            "minecraft:",
            "minecraft:a:b",
        ] {
            assert_eq!(
                AuthorityScope::new(1, dimension),
                Err(ConfigError::InvalidDimension)
            );
        }
        assert_eq!(
            AuthorityScope::new(1, &format!("a:{}", "x".repeat(256))),
            Err(ConfigError::InvalidDimension)
        );
        for capacity in [0, MAX_EVENTS + 1] {
            assert!(matches!(
                Observer::new(scope(), [BlockStateId(1)], capacity),
                Err(ConfigError::InvalidCapacity)
            ));
        }
        assert!(matches!(
            Observer::new(scope(), [], 1),
            Err(ConfigError::EmptyWatchedStates)
        ));
        assert!(
            Observer::new(
                scope(),
                std::iter::repeat_n(BlockStateId(1), MAX_WATCHED_STATES),
                MAX_EVENTS
            )
            .is_ok()
        );
        assert!(matches!(
            Observer::new(scope(), std::iter::repeat(BlockStateId(1)), 1),
            Err(ConfigError::TooManyWatchedStates)
        ));
    }

    #[test]
    fn sequence_exhaustion_faults_without_wrapping_or_partial_success() {
        let mut observer = Observer::new(scope(), [BlockStateId(1)], 2).unwrap();
        observer.next_sequence = Some(u64::MAX);
        observer.record([0, 64, 0], BlockStateId(0), BlockStateId(1), 25);
        let final_event = observer.drain().unwrap();
        assert_eq!(final_event[0].sequence(), u64::MAX);
        observer.record([0, 64, 0], BlockStateId(1), BlockStateId(0), 25);
        assert_eq!(observer.drain(), Err(ObserverFault::SequenceExhausted));
        observer.record([0, 64, 0], BlockStateId(0), BlockStateId(1), 26);
        assert_eq!(observer.drain(), Err(ObserverFault::SequenceExhausted));
    }
}
