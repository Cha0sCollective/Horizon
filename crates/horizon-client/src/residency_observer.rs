//! Passive coordinate residency observations, not chunk or machine snapshots.
//! A load followed by unload cannot reconstruct its former contents. There are
//! no activity, lifetime retirement or destruction notifications here.
use crate::mutation_observer::AuthorityScope;
use std::{fmt, sync::Arc};

pub const MAX_WATCHED_CHUNKS: usize = 64;
pub const MAX_EVENTS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    InvalidCapacity,
    InvalidWatch,
    AlreadyEnabled,
}
impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidCapacity => "residency observer capacity must be in 1..=4096",
            Self::InvalidWatch => "residency observer needs 1..=64 supplied chunk coordinates",
            Self::AlreadyEnabled => {
                "residency observer is already enabled; explicitly take it before replacement"
            }
        })
    }
}
impl std::error::Error for ConfigError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    Overflow { capacity: usize, sequence: u64 },
    SequenceExhausted,
}
impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overflow { capacity, sequence } => write!(
                f,
                "residency observer exceeded {capacity} events at sequence {sequence}"
            ),
            Self::SequenceExhausted => f.write_str("residency observer sequence exhausted"),
        }
    }
}
impl std::error::Error for Fault {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Loaded,
    Unloaded,
    Replaced,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    scope: Arc<AuthorityScope>,
    coordinate: (i32, i32),
    kind: Kind,
    game_time: i64,
    sequence: u64,
}
impl Event {
    pub fn scope(&self) -> &AuthorityScope {
        &self.scope
    }
    pub fn coordinate(&self) -> (i32, i32) {
        self.coordinate
    }
    pub fn kind(&self) -> Kind {
        self.kind
    }
    pub fn game_time(&self) -> i64 {
        self.game_time
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
}

/// The enable-time membership of every watched coordinate, sorted. This is not
/// history and does not pretend pre-existing chunks just loaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    scope: Arc<AuthorityScope>,
    game_time: i64,
    entries: Vec<((i32, i32), bool)>,
}
impl Snapshot {
    pub fn scope(&self) -> &AuthorityScope {
        &self.scope
    }
    pub fn game_time(&self) -> i64 {
        self.game_time
    }
    pub fn entries(&self) -> &[((i32, i32), bool)] {
        &self.entries
    }
}

#[derive(Debug)]
pub struct Observer {
    scope: Arc<AuthorityScope>,
    watched: Vec<(i32, i32)>,
    capacity: usize,
    pending: Vec<Event>,
    next_sequence: Option<u64>,
    fault: Option<Fault>,
}
impl Observer {
    pub(crate) fn new(
        scope: AuthorityScope,
        watched: impl IntoIterator<Item = (i32, i32)>,
        capacity: usize,
    ) -> Result<Self, ConfigError> {
        if !(1..=MAX_EVENTS).contains(&capacity) {
            return Err(ConfigError::InvalidCapacity);
        }
        let mut coordinates = Vec::new();
        for (i, coordinate) in watched.into_iter().enumerate() {
            // Bound supplied inputs too, including duplicate-only iterators.
            if i == MAX_WATCHED_CHUNKS {
                return Err(ConfigError::InvalidWatch);
            }
            coordinates.push(coordinate);
        }
        if coordinates.is_empty() {
            return Err(ConfigError::InvalidWatch);
        }
        coordinates.sort_unstable();
        coordinates.dedup();
        Ok(Self {
            scope: Arc::new(scope),
            watched: coordinates,
            capacity,
            pending: Vec::new(),
            next_sequence: Some(1),
            fault: None,
        })
    }
    pub fn scope(&self) -> &AuthorityScope {
        &self.scope
    }
    pub fn fault(&self) -> Option<Fault> {
        self.fault
    }
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
    pub(crate) fn snapshot(
        &self,
        game_time: i64,
        resident: impl Fn((i32, i32)) -> bool,
    ) -> Snapshot {
        Snapshot {
            scope: self.scope.clone(),
            game_time,
            entries: self.watched.iter().map(|&c| (c, resident(c))).collect(),
        }
    }
    pub(crate) fn record(&mut self, coordinate: (i32, i32), kind: Kind, game_time: i64) {
        if self.fault.is_some() || self.watched.binary_search(&coordinate).is_err() {
            return;
        }
        let Some(sequence) = self.next_sequence else {
            self.fault = Some(Fault::SequenceExhausted);
            return;
        };
        if self.pending.len() == self.capacity {
            self.fault = Some(Fault::Overflow {
                capacity: self.capacity,
                sequence,
            });
            return;
        }
        self.pending.push(Event {
            scope: self.scope.clone(),
            coordinate,
            kind,
            game_time,
            sequence,
        });
        self.next_sequence = sequence.checked_add(1);
    }
    pub fn drain(&mut self) -> Result<Vec<Event>, Fault> {
        if let Some(fault) = self.fault {
            return Err(fault);
        }
        Ok(std::mem::take(&mut self.pending))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn observer(
        watched: impl IntoIterator<Item = (i32, i32)>,
        capacity: usize,
    ) -> Result<Observer, ConfigError> {
        Observer::new(
            AuthorityScope::new(7, "minecraft:overworld").unwrap(),
            watched,
            capacity,
        )
    }
    #[test]
    fn residency_configuration_is_bounded() {
        for capacity in [0, MAX_EVENTS + 1] {
            assert!(matches!(
                observer([(0, 0)], capacity),
                Err(ConfigError::InvalidCapacity)
            ));
        }
        assert!(matches!(observer([], 1), Err(ConfigError::InvalidWatch)));
        assert!(matches!(
            observer(std::iter::repeat((0, 0)), 1),
            Err(ConfigError::InvalidWatch)
        ));
        assert!(observer((0..64).map(|x| (x, 0)), MAX_EVENTS).is_ok());
    }
    #[test]
    fn residency_sequences_do_not_wrap_or_silently_reset_after_drain() {
        let mut observer = observer([(0, 0)], 2).unwrap();
        observer.next_sequence = Some(u64::MAX);
        observer.record((0, 0), Kind::Loaded, 25);
        assert_eq!(observer.drain().unwrap()[0].sequence(), u64::MAX);
        observer.record((0, 0), Kind::Unloaded, 26);
        assert_eq!(observer.drain(), Err(Fault::SequenceExhausted));
        observer.record((0, 0), Kind::Loaded, 27);
        assert_eq!(observer.drain(), Err(Fault::SequenceExhausted));
    }
}
