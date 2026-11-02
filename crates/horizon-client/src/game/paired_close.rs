//! Dormant owned native metadata IO. This is neither a saved image nor a Game
//! close witness, and no ordinary save/close entry calls this module.
#![allow(dead_code)]

use super::Game;
use std::{sync::Arc, thread::JoinHandle, time::Instant};

/// Effects proven by the actual native operation, not inferred from equal
/// payloads or public namespace inventories after an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MetadataEffect {
    Unattempted,
    Skipped,
    Completed,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(super) enum MetadataFamily {
    RandomSequences,
    Stats,
    Advancements,
    Storage,
    Stopwatches,
    Border,
    Scoreboard,
    Bossbars,
    Schedule,
    Clock,
    Maps,
    Fight,
    Trader,
    Raids,
    Arrivals,
}
impl MetadataFamily {
    const ALL: [Self; 15] = [
        Self::RandomSequences,
        Self::Stats,
        Self::Advancements,
        Self::Storage,
        Self::Stopwatches,
        Self::Border,
        Self::Scoreboard,
        Self::Bossbars,
        Self::Schedule,
        Self::Clock,
        Self::Maps,
        Self::Fight,
        Self::Trader,
        Self::Raids,
        Self::Arrivals,
    ];
}

type Effects = [MetadataEffect; MetadataFamily::ALL.len()];
const UNATTEMPTED: Effects = [MetadataEffect::Unattempted; MetadataFamily::ALL.len()];

use super::{
    advancements::{
        AdvancementsExpected, AdvancementsInput, AdvancementsWritten, RetainedTreeState,
        RetainedTreeStatus,
    },
    bossbars::{BossMetadataExpected, BossMetadataInput, BossMetadataPost},
    chat::{ClockMetadataExpected, ClockMetadataInput, ClockMetadataPost},
    dragon::{FightMetadataExpected, FightMetadataInput, FightMetadataPost},
    functions::{ScheduleMetadataExpected, ScheduleMetadataInput, ScheduleMetadataPost},
    maps::{MapsMetadataEvidence, MapsMetadataExpected, MapsMetadataInput, MapsMetadataPost},
    nether_portal::{ArrivalsMetadataExpected, ArrivalsMetadataInput, ArrivalsMetadataPost},
    raids::{RaidsMetadataExpected, RaidsMetadataInput, RaidsMetadataPost},
    scoreboard::{ScoreMetadataExpected, ScoreMetadataInput, ScoreMetadataPost},
    stats::{StatsExpected, StatsInput, StatsWritten},
    trading::{RandomSequencesExpected, RandomSequencesInput, RandomSequencesWritten},
    wandering_trader::{TraderMetadataExpected, TraderMetadataInput, TraderMetadataPost},
};

struct Expected {
    maps: MapsMetadataExpected,
    fight: FightMetadataExpected,
    trader: TraderMetadataExpected,
    raids: RaidsMetadataExpected,
    arrivals: ArrivalsMetadataExpected,
    random: RandomSequencesExpected,
    stats: StatsExpected,
    advancements: AdvancementsExpected,
    score: ScoreMetadataExpected,
    boss: BossMetadataExpected,
    schedule: ScheduleMetadataExpected,
    clock: ClockMetadataExpected,
}
/// Read-only native projection; the original remains outside the IO thread.
#[derive(Clone)]
struct Input {
    maps: MapsMetadataInput,
    fight: FightMetadataInput,
    trader: TraderMetadataInput,
    raids: RaidsMetadataInput,
    arrivals: ArrivalsMetadataInput,
    random: RandomSequencesInput,
    stats: StatsInput,
    advancements: AdvancementsInput,
    score: ScoreMetadataInput,
    boss: BossMetadataInput,
    schedule: ScheduleMetadataInput,
    clock: ClockMetadataInput,
}
#[derive(Default)]
struct Written {
    maps: Option<MapsMetadataPost>,
    fight: Option<FightMetadataPost>,
    trader: Option<TraderMetadataPost>,
    raids: Option<RaidsMetadataPost>,
    arrivals: Option<ArrivalsMetadataPost>,
    random: Option<RandomSequencesWritten>,
    stats: Option<StatsWritten>,
    advancements: Option<AdvancementsWritten>,
    score: Option<ScoreMetadataPost>,
    boss: Option<BossMetadataPost>,
    schedule: Option<ScheduleMetadataPost>,
    clock: Option<ClockMetadataPost>,
}
impl Written {
    fn complete(&self) -> bool {
        self.random.is_some()
            && self.stats.is_some()
            && self.advancements.is_some()
            && self.score.is_some()
            && self.boss.is_some()
            && self.schedule.is_some()
            && self.clock.is_some()
            && self.maps.is_some()
            && self.fight.is_some()
            && self.trader.is_some()
            && self.raids.is_some()
            && self.arrivals.is_some()
    }
}
struct BatchIdentity;
struct AttemptIdentity {
    number: u64,
}
struct Request {
    batch: Arc<BatchIdentity>,
    attempt: Arc<AttemptIdentity>,
    input: Arc<Input>,
    #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
    control: Option<MetadataTestControl>,
}
struct Outcome {
    maps: Option<MapsMetadataEvidence>,
    batch: Arc<BatchIdentity>,
    attempt: Arc<AttemptIdentity>,
    effects: Effects,
    written: Written,
    error: Option<String>,
}
impl Outcome {
    fn success(&self) -> bool {
        self.error.is_none() && self.written.complete()
    }
}

fn write_metadata(request: Request) -> Outcome {
    let mut out = Outcome {
        maps: None,
        batch: request.batch,
        attempt: request.attempt,
        effects: UNATTEMPTED,
        written: Written::default(),
        error: None,
    };
    #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
    if let Some(control) = &request.control {
        control.before_io();
    }
    // A failed operation can have native filesystem effects. Preserve every
    // completed step and mark the attempted failing operation honestly unknown.
    macro_rules! simple {
        ($field:ident, $family:ident) => {
            #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
            if let Some(control) = &request.control {
                control.before_family(MetadataFamily::$family);
            }
            match request.input.$field.write_native() {
                Ok(written) => {
                    out.effects[MetadataFamily::$family as usize] = written.effect();
                    out.written.$field = Some(written);
                }
                Err(error) => {
                    out.effects[MetadataFamily::$family as usize] = MetadataEffect::Unknown;
                    out.error = Some(error.to_string());
                    return out;
                }
            }
        };
    }
    simple!(random, RandomSequences);
    simple!(stats, Stats);
    simple!(advancements, Advancements);
    let score = request.input.score.write_native();
    out.effects[3..7].copy_from_slice(&score.effects);
    match score.result {
        Ok(post) => out.written.score = Some(post),
        Err(error) => {
            out.error = Some(error);
            return out;
        }
    }
    macro_rules! reported {
        ($field:ident, $family:ident) => {
            #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
            if let Some(control) = &request.control {
                control.before_family(MetadataFamily::$family);
            }
            let io = request.input.$field.write_native();
            out.effects[MetadataFamily::$family as usize] = io.effect;
            match io.result {
                Ok(post) => out.written.$field = Some(post),
                Err(error) => {
                    out.error = Some(error);
                    return out;
                }
            }
        };
    }
    reported!(boss, Bossbars);
    reported!(schedule, Schedule);
    reported!(clock, Clock);
    #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
    if let Some(control) = &request.control {
        control.before_family(MetadataFamily::Maps);
    }
    let maps = request.input.maps.write_native();
    out.effects[MetadataFamily::Maps as usize] = maps.effect;
    out.maps = Some(maps.files);
    match maps.result {
        Ok(post) => out.written.maps = Some(post),
        Err(error) => {
            out.error = Some(error);
            return out;
        }
    }
    reported!(fight, Fight);
    reported!(trader, Trader);
    reported!(raids, Raids);
    reported!(arrivals, Arrivals);
    out
}

#[derive(Clone, Debug)]
pub(super) struct MetadataFailure {
    /// Actual returned per-map/index evidence. None is no subfile proof; if the
    /// Maps family is Unknown, unreturned subfile effects remain unknown too.
    pub maps: Option<MapsMetadataEvidence>,
    pub attempt: u64,
    pub effects: Effects,
    pub detail: String,
}
#[derive(Debug, PartialEq, Eq)]
pub(super) enum MetadataStatus {
    Preparing,
    Pending,
    Blocked(String),
    ReadyToAcknowledge,
}
#[derive(Debug, PartialEq, Eq)]
pub(super) enum MetadataRetryRefusal {
    Busy,
    NotBlocked,
    OriginalProjectionUnavailable,
    AttemptExhausted,
}

/// Exclusive lexical ownership keeps the original Game/session and every family
/// frozen. No Game pointer/reference is ever sent to the native IO worker.
pub(super) struct NativeMetadata<'a, 'window> {
    armed: bool,
    game: &'a mut Game<'window>,
    batch: Arc<BatchIdentity>,
    tree: RetainedTreeState,
    tree_started: bool,
    sampled_ms: i64,
    expected: Option<Expected>,
    input: Option<Arc<Input>>,
    handle: Option<JoinHandle<Option<Outcome>>>,
    attempt: Option<Arc<AttemptIdentity>>,
    outcome: Option<Outcome>,
    next_attempt: u64,
    deadline: Instant,
    expired: bool,
    blocked: Option<String>,
    history: Vec<MetadataFailure>,
    #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
    control: Option<MetadataTestControl>,
}

/// This certifies this metadata batch's checked CPU acknowledgment only. It is
/// not an image, a Game-close witness or authority to release a world lock.
pub(super) struct MetadataAcknowledged {
    pub maps: Option<MapsMetadataEvidence>,
    _batch: Arc<BatchIdentity>,
    pub effects: Effects,
    pub sampled_ms: i64,
    pub failures: Vec<MetadataFailure>,
}
impl<'a, 'window> NativeMetadata<'a, 'window> {
    pub(super) fn begin(game: &'a mut Game<'window>, deadline: Instant) -> Box<Self> {
        Box::new(Self {
            armed: true,
            game,
            batch: Arc::new(BatchIdentity),
            tree: RetainedTreeState::new(),
            tree_started: false,
            sampled_ms: horizon_client::stopwatches::current_time(),
            expected: None,
            input: None,
            handle: None,
            attempt: None,
            outcome: None,
            next_attempt: 0,
            deadline,
            expired: false,
            blocked: None,
            history: Vec::new(),
            #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
            control: None,
        })
    }
    pub(super) fn failures(&self) -> &[MetadataFailure] {
        &self.history
    }
    pub(super) fn sampled_ms(&self) -> i64 {
        self.sampled_ms
    }
    fn effects(&self) -> Effects {
        self.outcome.as_ref().map_or_else(
            || {
                if self.handle.is_some() {
                    [MetadataEffect::Unknown; MetadataFamily::ALL.len()]
                } else {
                    UNATTEMPTED
                }
            },
            |outcome| outcome.effects,
        )
    }
    fn block(&mut self, detail: impl Into<String>) {
        let detail = detail.into();
        let attempt = self.attempt.as_ref().map_or(0, |attempt| attempt.number);
        if !self
            .history
            .iter()
            .any(|failure| failure.attempt == attempt && failure.detail == detail)
        {
            self.history.push(MetadataFailure {
                maps: self
                    .outcome
                    .as_ref()
                    .and_then(|outcome| outcome.maps.clone()),
                attempt,
                effects: self.effects(),
                detail: detail.clone(),
            });
        }
        self.blocked.get_or_insert(detail);
    }
    fn prepare(&mut self) -> Result<(), String> {
        // Every value here is a read-only projection, not authority moved from
        // Game. A failed projection leaves the entire original in this guard.
        let (random, random_input) = self
            .game
            .prepare_metadata_random_sequences()
            .map_err(|e| e.to_string())?;
        let (stats, stats_input) = self.game.prepare_metadata_stats();
        let (advancements, advancement_input) = self
            .game
            .prepare_metadata_advancements(&self.tree)
            .map_err(|e| e.to_string())?;
        let (score, score_input) = self.game.prepare_retained_score_metadata(self.sampled_ms)?;
        #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
        let score_input = score_input.with_test_control(self.control.clone());
        let (boss, boss_input) = self.game.prepare_retained_boss_metadata();
        let (schedule, schedule_input) = self.game.prepare_retained_schedule_metadata();
        let (clock, clock_input) = self.game.prepare_retained_clock_metadata();
        let (maps, maps_input) = self.game.prepare_retained_maps_metadata()?;
        let (fight, fight_input) = self.game.prepare_retained_fight_metadata()?;
        let (trader, trader_input) = self.game.prepare_retained_trader_metadata();
        let (raids, raids_input) = self.game.prepare_retained_raids_metadata()?;
        let (arrivals, arrivals_input) = self.game.prepare_retained_arrivals_metadata();
        self.expected = Some(Expected {
            maps,
            fight,
            trader,
            raids,
            arrivals,
            random,
            stats,
            advancements,
            score,
            boss,
            schedule,
            clock,
        });
        self.input = Some(Arc::new(Input {
            maps: maps_input,
            fight: fight_input,
            trader: trader_input,
            raids: raids_input,
            arrivals: arrivals_input,
            random: random_input,
            stats: stats_input,
            advancements: advancement_input,
            score: score_input,
            boss: boss_input,
            schedule: schedule_input,
            clock: clock_input,
        }));
        Ok(())
    }
    fn dispatch(&mut self) {
        if self.handle.is_some() || self.outcome.is_some() || self.input.is_none() {
            return;
        }
        let Some(next) = self.next_attempt.checked_add(1) else {
            self.block("metadata attempt identity exhausted");
            return;
        };
        self.next_attempt = next;
        let attempt = Arc::new(AttemptIdentity { number: next });
        let (send, receive) = std::sync::mpsc::sync_channel::<Request>(1);
        #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
        if self
            .control
            .as_ref()
            .is_some_and(MetadataTestControl::refuse_spawn)
        {
            self.block("controlled metadata spawn refusal");
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("Native metadata IO".into())
            .spawn(move || receive.recv().ok().map(write_metadata));
        let handle = match spawned {
            Ok(handle) => handle,
            Err(error) => {
                self.block(format!("metadata IO spawn: {error}"));
                return;
            }
        };
        // Register the actual handle BEFORE any owned input reaches the thread.
        self.handle = Some(handle);
        self.attempt = Some(attempt.clone());
        let request = Request {
            batch: self.batch.clone(),
            attempt,
            input: self.input.as_ref().unwrap().clone(),
            #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
            control: self.control.clone(),
        };
        #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
        if self
            .control
            .as_ref()
            .is_some_and(MetadataTestControl::disconnect_dispatch)
        {
            drop(send);
            drop(request);
            self.block("controlled metadata dispatch disconnect");
            return;
        }
        if let Err(error) = send.try_send(request) {
            drop(error); // Only the returned attempt clone; original stays here.
            self.block("metadata one-job dispatch refused");
        }
    }
    fn correlated(&self, outcome: &Outcome) -> bool {
        Arc::ptr_eq(&outcome.batch, &self.batch)
            && self
                .attempt
                .as_ref()
                .is_some_and(|attempt| Arc::ptr_eq(attempt, &outcome.attempt))
    }
    fn harvest(&mut self) {
        if !self
            .handle
            .as_ref()
            .is_some_and(|handle| handle.is_finished())
        {
            return;
        }
        let joined = self.handle.take().unwrap().join();
        match joined {
            Ok(Some(outcome)) => {
                let correlated = self.correlated(&outcome);
                let error = outcome.error.clone();
                self.outcome = Some(outcome);
                if !correlated {
                    self.block("metadata worker returned a foreign batch/attempt");
                } else if let Some(error) = error {
                    self.block(error);
                }
            }
            Ok(None) => self.block("metadata worker exited without its dispatched outcome"),
            Err(_) => {
                let mut effects = UNATTEMPTED;
                effects.fill(MetadataEffect::Unknown);
                self.history.push(MetadataFailure {
                    maps: None,
                    attempt: self.attempt.as_ref().map_or(0, |a| a.number),
                    effects,
                    detail: "metadata IO worker panicked; file effects unproven".into(),
                });
                self.blocked
                    .get_or_insert_with(|| "metadata IO worker panicked".into());
            }
        }
    }
    pub(super) fn poll(&mut self) -> MetadataStatus {
        self.harvest();
        if Instant::now() >= self.deadline && !self.expired {
            self.expired = true;
            self.block("metadata original attempt deadline expired");
        }
        if !self.tree_started && !self.expired && self.blocked.is_none() {
            self.tree_started = true;
            if let Err(error) = self.game.start_metadata_advancement_tree(&mut self.tree) {
                self.block(error.to_string());
            }
        }
        // Even after a deadline/error, keep harvesting an actual original helper.
        let tree = if self.tree_started {
            self.game.poll_metadata_advancement_tree(&mut self.tree)
        } else {
            RetainedTreeStatus::Unstarted
        };
        if let RetainedTreeStatus::Failed(error) = &tree {
            self.block(error.clone());
        }
        if self.input.is_none()
            && self.blocked.is_none()
            && !self.expired
            && matches!(
                tree,
                RetainedTreeStatus::Ready | RetainedTreeStatus::Skipped(_)
            )
            && let Err(error) = self.prepare()
        {
            self.block(error);
        }
        if let Some(error) = &self.blocked {
            return MetadataStatus::Blocked(error.clone());
        }
        if self
            .outcome
            .as_ref()
            .is_some_and(|outcome| self.correlated(outcome) && outcome.success())
        {
            return MetadataStatus::ReadyToAcknowledge;
        }
        if self.input.is_some() && self.handle.is_none() {
            self.dispatch();
        }
        if let Some(error) = &self.blocked {
            MetadataStatus::Blocked(error.clone())
        } else if self.input.is_none() {
            MetadataStatus::Preparing
        } else {
            MetadataStatus::Pending
        }
    }
    pub(super) fn retry(&mut self, deadline: Instant) -> Result<(), MetadataRetryRefusal> {
        self.harvest();
        if self.handle.is_some() || self.tree.unresolved() {
            return Err(MetadataRetryRefusal::Busy);
        }
        if self.blocked.is_none() {
            return Err(MetadataRetryRefusal::NotBlocked);
        }
        if self
            .outcome
            .as_ref()
            .is_some_and(|outcome| self.correlated(outcome) && outcome.success())
        {
            // Explicit Retry may accept the exact harvested late success. It
            // never silently renews or replays a completed successful IO batch.
            self.blocked = None;
            self.expired = false;
            self.deadline = deadline;
            return Ok(());
        }
        if self.next_attempt == u64::MAX {
            return Err(MetadataRetryRefusal::AttemptExhausted);
        }
        if let Some(outcome) = self.outcome.take() {
            self.history.push(MetadataFailure {
                maps: outcome.maps,
                attempt: outcome.attempt.number,
                effects: outcome.effects,
                detail: outcome.error.unwrap_or_else(|| {
                    "unaccepted metadata attempt retained for explicit retry".into()
                }),
            });
        }
        if self.input.is_none() && matches!(self.tree.status(), RetainedTreeStatus::Failed(_)) {
            self.game
                .retry_metadata_advancement_tree(&mut self.tree)
                .map_err(|_| MetadataRetryRefusal::OriginalProjectionUnavailable)?;
        }
        self.blocked = None;
        self.expired = false;
        self.deadline = deadline;
        Ok(())
    }
    fn preflight(&self) -> Result<(), String> {
        if Instant::now() >= self.deadline
            || self.expired
            || self.blocked.is_some()
            || self.handle.is_some()
            || self.tree.unresolved()
        {
            return Err("metadata attempt is not ready for acknowledgment".into());
        }
        let expected = self
            .expected
            .as_ref()
            .ok_or("metadata original projection unavailable")?;
        let outcome = self
            .outcome
            .as_ref()
            .ok_or("metadata actual outcome unavailable")?;
        if !self.correlated(outcome) || !outcome.success() {
            return Err("metadata batch/attempt outcome mismatch".into());
        }
        let written = &outcome.written;
        self.game
            .preflight_metadata_random_sequences(&expected.random, written.random.as_ref().unwrap())
            .map_err(|e| e.to_string())?;
        self.game
            .preflight_metadata_stats(&expected.stats, written.stats.as_ref().unwrap())
            .map_err(|e| e.to_string())?;
        self.game
            .preflight_metadata_advancements(
                &expected.advancements,
                written.advancements.as_ref().unwrap(),
            )
            .map_err(|e| e.to_string())?;
        self.game
            .preflight_retained_score_metadata(&expected.score, written.score.as_ref().unwrap())?;
        self.game
            .preflight_retained_boss_metadata(&expected.boss, written.boss.as_ref().unwrap())?;
        self.game.preflight_retained_schedule_metadata(
            &expected.schedule,
            written.schedule.as_ref().unwrap(),
        )?;
        self.game
            .preflight_retained_clock_metadata(&expected.clock, written.clock.as_ref().unwrap())?;
        self.game
            .preflight_retained_maps_metadata(&expected.maps, written.maps.as_ref().unwrap())?;
        self.game
            .preflight_retained_fight_metadata(&expected.fight, written.fight.as_ref().unwrap())?;
        self.game.preflight_retained_trader_metadata(
            &expected.trader,
            written.trader.as_ref().unwrap(),
        )?;
        self.game
            .preflight_retained_raids_metadata(&expected.raids, written.raids.as_ref().unwrap())?;
        self.game.preflight_retained_arrivals_metadata(
            &expected.arrivals,
            written.arrivals.as_ref().unwrap(),
        )?;
        Ok(())
    }
    pub(super) fn acknowledge(mut self: Box<Self>) -> Result<MetadataAcknowledged, Box<Self>> {
        if let Err(error) = self.preflight() {
            self.block(error);
            return Err(self);
        }
        let expected = self.expected.as_ref().unwrap();
        let written = &mut self.outcome.as_mut().unwrap().written;
        // ALL preflights succeeded before ANY live marker can change.
        self.game.commit_metadata_random_sequences(&expected.random);
        self.game.commit_metadata_stats(&expected.stats);
        self.game
            .commit_metadata_advancements(&expected.advancements);
        self.game
            .commit_retained_score_metadata(written.score.take().unwrap());
        self.game
            .commit_retained_boss_metadata(written.boss.take().unwrap());
        self.game
            .commit_retained_schedule_metadata(written.schedule.take().unwrap());
        self.game
            .commit_retained_clock_metadata(written.clock.take().unwrap());
        self.game
            .commit_retained_maps_metadata(written.maps.take().unwrap());
        self.game
            .commit_retained_fight_metadata(written.fight.take().unwrap());
        self.game
            .commit_retained_trader_metadata(written.trader.take().unwrap());
        self.game
            .commit_retained_raids_metadata(written.raids.take().unwrap());
        self.game
            .commit_retained_arrivals_metadata(written.arrivals.take().unwrap());
        self.armed = false;
        Ok(MetadataAcknowledged {
            maps: self
                .outcome
                .as_ref()
                .and_then(|outcome| outcome.maps.clone()),
            _batch: self.batch.clone(),
            effects: self.effects(),
            sampled_ms: self.sampled_ms,
            failures: std::mem::take(&mut self.history),
        })
    }
}
impl Drop for NativeMetadata<'_, '_> {
    fn drop(&mut self) {
        if self.armed {
            use std::io::Write;
            let _ = writeln!(
                std::io::stderr().lock(),
                "fatal: unresolved native metadata owner"
            );
            std::process::abort();
        }
    }
}

#[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MetadataTestAction {
    HoldBeforeIo,
    HoldBeforeFamily(MetadataFamily),
    HoldThenPanic,
    RefuseSpawn,
    DisconnectDispatch,
}
#[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
struct MetadataTestState {
    action: MetadataTestAction,
    used: std::sync::atomic::AtomicBool,
    entered: std::sync::atomic::AtomicBool,
    released: std::sync::atomic::AtomicBool,
    mutex: std::sync::Mutex<()>,
    event: std::sync::Condvar,
}
#[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
#[derive(Clone)]
pub(in crate::game) struct MetadataTestControl(Arc<MetadataTestState>);
#[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
struct MetadataTestReceipt(Arc<MetadataTestState>);
#[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
impl MetadataTestControl {
    fn new(action: MetadataTestAction) -> (Self, MetadataTestReceipt) {
        let state = Arc::new(MetadataTestState {
            action,
            used: false.into(),
            entered: false.into(),
            released: false.into(),
            mutex: std::sync::Mutex::new(()),
            event: std::sync::Condvar::new(),
        });
        (Self(state.clone()), MetadataTestReceipt(state))
    }
    fn first(&self, action: MetadataTestAction) -> bool {
        self.0.action == action && !self.0.used.swap(true, std::sync::atomic::Ordering::SeqCst)
    }
    fn refuse_spawn(&self) -> bool {
        self.first(MetadataTestAction::RefuseSpawn)
    }
    fn disconnect_dispatch(&self) -> bool {
        self.first(MetadataTestAction::DisconnectDispatch)
    }
    fn before_io(&self) {
        if !matches!(
            self.0.action,
            MetadataTestAction::HoldBeforeIo | MetadataTestAction::HoldThenPanic
        ) || self.0.used.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        self.wait();
        if self.0.action == MetadataTestAction::HoldThenPanic {
            panic!("controlled native metadata IO panic before first family");
        }
    }
    pub(in crate::game) fn before_family(&self, family: MetadataFamily) {
        if self.first(MetadataTestAction::HoldBeforeFamily(family)) {
            self.wait();
        }
    }
    fn wait(&self) {
        let mut guard = self.0.mutex.lock().unwrap();
        self.0
            .entered
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.0.event.notify_all();
        while !self.0.released.load(std::sync::atomic::Ordering::SeqCst) {
            guard = self.0.event.wait(guard).unwrap();
        }
        drop(guard);
    }
}
#[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
impl MetadataTestReceipt {
    fn entered(&self) -> bool {
        self.0.entered.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn release(&self) {
        let _guard = self.0.mutex.lock().unwrap();
        self.0
            .released
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.0.event.notify_all();
    }
}
#[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
impl Drop for MetadataTestReceipt {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
#[path = "paired_close_metadata_tests.rs"]
mod tests;
