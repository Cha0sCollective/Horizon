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
    raids::{
        LevelRaidsExpected, LevelRaidsInput, LevelRaidsPost, RaidsMetadataExpected,
        RaidsMetadataInput, RaidsMetadataPost,
    },
    scoreboard::{ScoreMetadataExpected, ScoreMetadataInput, ScoreMetadataPost},
    stats::{StatsExpected, StatsInput, StatsWritten},
    trading::{RandomSequencesExpected, RandomSequencesInput, RandomSequencesWritten},
    wandering_trader::{TraderMetadataExpected, TraderMetadataInput, TraderMetadataPost},
};

enum RaidExpected {
    Single(RaidsMetadataExpected),
    Levels(LevelRaidsExpected),
}
#[derive(Clone)]
enum RaidInput {
    Single(RaidsMetadataInput),
    Levels(LevelRaidsInput),
}
enum RaidPost {
    Single(RaidsMetadataPost),
    Levels(LevelRaidsPost),
}
struct RaidIo {
    effect: MetadataEffect,
    result: Result<RaidPost, String>,
}
impl RaidInput {
    fn write_native(&self) -> RaidIo {
        match self {
            Self::Single(input) => {
                let io = input.write_native();
                RaidIo {
                    effect: io.effect,
                    result: io.result.map(RaidPost::Single),
                }
            }
            Self::Levels(input) => {
                let io = input.write_native();
                RaidIo {
                    effect: io.effect,
                    result: io.result.map(RaidPost::Levels),
                }
            }
        }
    }
    fn apply_to_level(&self, level: &mut horizon_client::save::Level) {
        if let Self::Levels(input) = self {
            input.apply_to_level(level);
        }
    }
}
fn preflight_raid(game: &Game<'_>, expected: &RaidExpected, post: &RaidPost) -> Result<(), String> {
    match (expected, post) {
        (RaidExpected::Single(expected), RaidPost::Single(post)) => {
            game.preflight_retained_raids_metadata(expected, post)
        }
        (RaidExpected::Levels(expected), RaidPost::Levels(post)) => {
            game.preflight_retained_level_raids_metadata(expected, post)
        }
        _ => Err("native raid route association changed".into()),
    }
}
fn commit_raid(game: &mut Game<'_>, post: RaidPost) {
    match post {
        RaidPost::Single(post) => game.commit_retained_raids_metadata(post),
        RaidPost::Levels(post) => game.commit_retained_level_raids_metadata(post),
    }
}

struct Expected {
    maps: MapsMetadataExpected,
    fight: FightMetadataExpected,
    trader: TraderMetadataExpected,
    raids: RaidExpected,
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
    raids: RaidInput,
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
    raids: Option<RaidPost>,
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
    game: &'a mut Game<'window>,
    state: MetadataState,
}
impl<'a, 'window> NativeMetadata<'a, 'window> {
    pub(super) fn begin(game: &'a mut Game<'window>, deadline: Instant) -> Box<Self> {
        Box::new(Self {
            game,
            state: MetadataState::new(deadline),
        })
    }
    pub(super) fn failures(&self) -> &[MetadataFailure] {
        self.state.failures()
    }
    pub(super) fn sampled_ms(&self) -> i64 {
        self.state.sampled_ms()
    }
    fn effects(&self) -> Effects {
        self.state.effects()
    }
    fn preflight(&self) -> Result<(), String> {
        self.state.preflight(self.game)
    }
    pub(super) fn poll(&mut self) -> MetadataStatus {
        self.state.poll(self.game)
    }
    pub(super) fn retry(&mut self, deadline: Instant) -> Result<(), MetadataRetryRefusal> {
        self.state.retry(self.game, deadline)
    }
    pub(super) fn acknowledge(mut self: Box<Self>) -> Result<MetadataAcknowledged, Box<Self>> {
        if let Err(error) = self.state.preflight(self.game) {
            self.state.block(error);
            return Err(self);
        }
        Ok(self.state.commit_preflighted(self.game))
    }
}

/// This private state has no Game reference and is never exposed through a
/// mutable accessor. Both callers keep their original Game exclusively owned.
struct MetadataState {
    level_raids: bool,
    armed: bool,
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
impl MetadataState {
    fn new(deadline: Instant) -> Self {
        Self {
            level_raids: false,
            armed: true,
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
        }
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
    fn prepare(&mut self, game: &Game<'_>) -> Result<(), String> {
        // Every value here is a read-only projection, not authority moved from
        // Game. A failed projection leaves the entire original in this guard.
        let (random, random_input) = game
            .prepare_metadata_random_sequences()
            .map_err(|e| e.to_string())?;
        let (stats, stats_input) = game.prepare_metadata_stats();
        let (advancements, advancement_input) = game
            .prepare_metadata_advancements(&self.tree)
            .map_err(|e| e.to_string())?;
        let (score, score_input) = game.prepare_retained_score_metadata(self.sampled_ms)?;
        #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
        let score_input = score_input.with_test_control(self.control.clone());
        let (boss, boss_input) = game.prepare_retained_boss_metadata();
        let (schedule, schedule_input) = game.prepare_retained_schedule_metadata();
        let (clock, clock_input) = game.prepare_retained_clock_metadata();
        let (maps, maps_input) = game.prepare_retained_maps_metadata()?;
        let (fight, fight_input) = game.prepare_retained_fight_metadata()?;
        let (trader, trader_input) = game.prepare_retained_trader_metadata();
        let (raids, raids_input) = if self.level_raids {
            let (expected, input) = game.prepare_retained_level_raids_metadata()?;
            (RaidExpected::Levels(expected), RaidInput::Levels(input))
        } else {
            let (expected, input) = game.prepare_retained_raids_metadata()?;
            (RaidExpected::Single(expected), RaidInput::Single(input))
        };
        let (arrivals, arrivals_input) = game.prepare_retained_arrivals_metadata();
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
    fn poll(&mut self, game: &mut Game<'_>) -> MetadataStatus {
        self.harvest();
        if Instant::now() >= self.deadline && !self.expired {
            self.expired = true;
            self.block("metadata original attempt deadline expired");
        }
        let tree = self.poll_tree(game, !self.expired && self.blocked.is_none());
        if self.input.is_none()
            && self.blocked.is_none()
            && !self.expired
            && matches!(
                tree,
                RetainedTreeStatus::Ready | RetainedTreeStatus::Skipped(_)
            )
            && let Err(error) = self.prepare(game)
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
    fn poll_tree(&mut self, game: &mut Game<'_>, allow_start: bool) -> RetainedTreeStatus {
        if !self.tree_started && allow_start {
            self.tree_started = true;
            if let Err(error) = game.start_metadata_advancement_tree(&mut self.tree) {
                self.block(error.to_string());
            }
        }
        // Even after a deadline/error, keep harvesting an actual original helper.
        let tree = if self.tree_started {
            game.poll_metadata_advancement_tree(&mut self.tree)
        } else {
            RetainedTreeStatus::Unstarted
        };
        if let RetainedTreeStatus::Failed(error) = &tree {
            self.block(error.clone());
        }
        tree
    }
    fn retry(
        &mut self,
        game: &mut Game<'_>,
        deadline: Instant,
    ) -> Result<(), MetadataRetryRefusal> {
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
            game.retry_metadata_advancement_tree(&mut self.tree)
                .map_err(|_| MetadataRetryRefusal::OriginalProjectionUnavailable)?;
        }
        self.blocked = None;
        self.expired = false;
        self.deadline = deadline;
        Ok(())
    }
    fn preflight(&self, game: &Game<'_>) -> Result<(), String> {
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
        game.preflight_metadata_random_sequences(
            &expected.random,
            written.random.as_ref().unwrap(),
        )
        .map_err(|e| e.to_string())?;
        game.preflight_metadata_stats(&expected.stats, written.stats.as_ref().unwrap())
            .map_err(|e| e.to_string())?;
        game.preflight_metadata_advancements(
            &expected.advancements,
            written.advancements.as_ref().unwrap(),
        )
        .map_err(|e| e.to_string())?;
        game.preflight_retained_score_metadata(&expected.score, written.score.as_ref().unwrap())?;
        game.preflight_retained_boss_metadata(&expected.boss, written.boss.as_ref().unwrap())?;
        game.preflight_retained_schedule_metadata(
            &expected.schedule,
            written.schedule.as_ref().unwrap(),
        )?;
        game.preflight_retained_clock_metadata(&expected.clock, written.clock.as_ref().unwrap())?;
        game.preflight_retained_maps_metadata(&expected.maps, written.maps.as_ref().unwrap())?;
        game.preflight_retained_fight_metadata(&expected.fight, written.fight.as_ref().unwrap())?;
        game.preflight_retained_trader_metadata(
            &expected.trader,
            written.trader.as_ref().unwrap(),
        )?;
        preflight_raid(game, &expected.raids, written.raids.as_ref().unwrap())?;
        game.preflight_retained_arrivals_metadata(
            &expected.arrivals,
            written.arrivals.as_ref().unwrap(),
        )?;
        Ok(())
    }
    fn commit_preflighted(&mut self, game: &mut Game<'_>) -> MetadataAcknowledged {
        let expected = self.expected.as_ref().unwrap();
        let written = &mut self.outcome.as_mut().unwrap().written;
        // ALL preflights succeeded before ANY live marker can change.
        game.commit_metadata_random_sequences(&expected.random);
        game.commit_metadata_stats(&expected.stats);
        game.commit_metadata_advancements(&expected.advancements);
        game.commit_retained_score_metadata(written.score.take().unwrap());
        game.commit_retained_boss_metadata(written.boss.take().unwrap());
        game.commit_retained_schedule_metadata(written.schedule.take().unwrap());
        game.commit_retained_clock_metadata(written.clock.take().unwrap());
        game.commit_retained_maps_metadata(written.maps.take().unwrap());
        game.commit_retained_fight_metadata(written.fight.take().unwrap());
        game.commit_retained_trader_metadata(written.trader.take().unwrap());
        commit_raid(game, written.raids.take().unwrap());
        game.commit_retained_arrivals_metadata(written.arrivals.take().unwrap());
        self.armed = false;
        MetadataAcknowledged {
            maps: self
                .outcome
                .as_ref()
                .and_then(|outcome| outcome.maps.clone()),
            _batch: self.batch.clone(),
            effects: self.effects(),
            sampled_ms: self.sampled_ms,
            failures: std::mem::take(&mut self.history),
        }
    }
}
impl Drop for MetadataState {
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

use super::levels::session_checkpoint::{
    CoherentImageAttempt, CoherentImageBuildInput, CoherentImageContext, CoherentImageReadInput,
    CoherentImageReadPost, CoherentImageWritten, NativeWorldReadContext, NativeWorldReadInput,
    NativeWorldReadPost,
};
use super::levels::{NativeLevelsStatus, RetainedNativeLevels, RetainedNativePayloads};
use horizon_client::save::{
    NativeWriterClosed, NativeWriterDurable, NativeWriterFinal, NativeWriterFinalInput,
    NativeWriterFinalSubmitted, NativeWriterStaged, NativeWriterStagedStatus,
};

/// A dormant native completion profile. Neither profile implies transport closure.
pub(super) enum CloseProfile {
    NativeWorld,
    CoherentSession { archive: std::path::PathBuf },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClosePhase {
    Resources,
    Producers,
    Historical,
    ReadPreflight,
    Prepare,
    NativeIo,
    FinalWriters,
    Image,
    Preflight,
    NativeReady,
}
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CloseStatus {
    Pending(ClosePhase),
    Blocked(String),
    NativeReady,
}
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CloseRetryRefusal {
    Busy,
    NotBlocked,
    Deadline,
    Refused(String),
}

/// Fixed values only: callers cannot obtain Game/LAN owners through inspection.
#[derive(Clone, Debug)]
pub(super) struct CloseImageObservation {
    pub epoch: u64,
    pub pending: std::path::PathBuf,
    pub published: std::path::PathBuf,
    pub phase: super::levels::session_checkpoint::CoherentImagePhase,
    pub publication: super::levels::session_checkpoint::Publication,
}
impl CloseImageObservation {
    fn capture(attempt: &CoherentImageAttempt) -> Self {
        Self {
            epoch: attempt.epoch(),
            pending: attempt.pending().to_owned(),
            published: attempt.published().to_owned(),
            phase: attempt.phase(),
            publication: attempt.publication(),
        }
    }
}
#[derive(Clone, Debug)]
pub(super) struct CloseObservation {
    pub phase: ClosePhase,
    pub dimension: super::Dim,
    pub participants: usize,
    pub levels: usize,
    pub identity_preparations: usize,
    pub failed_attempts: usize,
    pub read_refusals: Vec<(super::Dim, String)>,
    pub image: Option<(u64, std::path::PathBuf)>,
    pub image_attempt: Option<CloseImageObservation>,
    pub prior_images: Vec<CloseImageObservation>,
}
/// Native completion retains all outer owners for a later bounded composition.
/// Its unresolved Drop deliberately retains CloseOwner's invariant backstop.
pub(super) struct NativeReady<'window> {
    owner: Box<CloseOwner<'window>>,
}
pub(super) struct NativeDisposed {
    _native_only: (),
    server_transport: Option<horizon_client::lan::hub::RetainedHubCloseView>,
    ordinary_world: Option<OrdinaryClosedWorld>,
}
/// One actual completed ordinary world, issued only by NativeReady consuming
/// disposal. A path, UUID file or empty runtime cannot construct this record.
pub(in crate::game) struct OrdinaryClosedWorld {
    root: std::path::PathBuf,
    world: horizon_client::world::counter_restore::WorldId,
    startup: std::sync::Arc<super::mod_packages::FrozenHostInputs>,
    coherent: bool,
    original: std::sync::Mutex<Option<super::world_loading::NativeCloseAuthority>>,
}
impl std::fmt::Debug for OrdinaryClosedWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrdinaryClosedWorld")
            .finish_non_exhaustive()
    }
}
impl OrdinaryClosedWorld {
    fn from_completed(game: &mut Game<'_>, profile: &CloseProfile) -> Option<Self> {
        let startup = game.ordinary_startup.as_ref()?.clone();
        let world = *game.session_lock.as_ref()?.expected_world()?;
        Some(Self {
            root: game.world_root.clone(),
            world,
            startup,
            coherent: matches!(profile, CloseProfile::CoherentSession { .. }),
            original: std::sync::Mutex::new(Some(game.native_close_authority.take()?)),
        })
    }
    pub(in crate::game) fn claim_native(
        self: &std::sync::Arc<Self>,
        root: &std::path::Path,
    ) -> Option<NativeReopenClaim> {
        if self.coherent || self.root != root {
            return None;
        }
        let original = self
            .original
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()?;
        Some(NativeReopenClaim {
            source: self.clone(),
            original: Some(original),
        })
    }
    pub(in crate::game) fn is_coherent_source(
        &self,
        root: &std::path::Path,
        world: horizon_client::world::counter_restore::WorldId,
        startup: &std::sync::Arc<super::mod_packages::FrozenHostInputs>,
    ) -> bool {
        self.coherent
            && self.root == root
            && self.world == world
            && std::sync::Arc::ptr_eq(&self.startup, startup)
            && self
                .original
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_some()
    }
    pub(in crate::game) fn root(&self) -> &std::path::Path {
        &self.root
    }
}
/// The one original completion claim moves with the request and its actual
/// PrepareOwner. Arc aliases cannot obtain a second claim. A failed preparation
/// returns the SAME claim only after its checked owner is allowed to drop.
pub(in crate::game) struct NativeReopenClaim {
    source: std::sync::Arc<OrdinaryClosedWorld>,
    original: Option<super::world_loading::NativeCloseAuthority>,
}
impl NativeReopenClaim {
    pub(in crate::game) fn world(
        &self,
        root: &std::path::Path,
    ) -> Option<horizon_client::world::counter_restore::WorldId> {
        (self.original.is_some() && self.source.root == root && !self.source.coherent)
            .then_some(self.source.world)
    }
    pub(in crate::game) fn spend_on_adoption(&mut self) {
        self.original = None;
    }
}
impl Drop for NativeReopenClaim {
    fn drop(&mut self) {
        if let Some(original) = self.original.take() {
            let mut slot = self
                .source
                .original
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            assert!(slot.is_none(), "original native completion claim is unique");
            *slot = Some(original);
        }
    }
}
impl NativeDisposed {
    pub(super) fn take_ordinary_world(&mut self) -> Option<OrdinaryClosedWorld> {
        self.ordinary_world.take()
    }
    pub(super) fn server_transport(
        &self,
    ) -> Option<&horizon_client::lan::hub::RetainedHubCloseView> {
        self.server_transport.as_ref()
    }
}
fn dormant_async_owners_absent(game: &Game<'_>, allow_constructor_lan: bool) -> bool {
    remaining_async_owners_absent(game, allow_constructor_lan, false)
}
fn remaining_async_owners_absent(
    game: &Game<'_>,
    allow_constructor_lan: bool,
    retain_presentation: bool,
) -> bool {
    (allow_constructor_lan || game.lan.is_none())
        && game.remote.is_none()
        && game.integrated.is_none()
        && game.serving.is_none()
        && game.friends.is_none()
        && game.control.is_none()
        && game.world_loader.is_none()
        && game.rest_loader.is_none()
        && game.retained_prepare_loader.is_none()
        && game.retained_preparation.is_none()
        && game.retained_entry_owner.is_none()
        && game.pending_entry.is_none()
        && game.pending_save.is_none()
        && game.adv.pending_tree.is_none()
        && game.lod.is_none()
        && (retain_presentation || (game.window.is_none() && game.renderer.is_none()))
}
impl NativeReady<'_> {
    /// Return real presentation only after the original ordinary world and all
    /// its relevant children completed. Old world/save authority never escapes.
    pub(super) fn try_return_window(
        mut self,
        completion: super::window_close::OrdinaryClientCompletion,
    ) -> Result<
        super::window_close::ReturnedFrontend,
        (Self, super::window_close::OrdinaryClientCompletion, String),
    > {
        let game = self.owner.game.as_ref().unwrap();
        if !self.owner.native_ready_proven()
            || !remaining_async_owners_absent(game, false, true)
            || !completion.matches(game)
            || !game
                .native_close_authority
                .as_ref()
                .is_some_and(|authority| !authority.is_server() && authority.authorizes(game))
        {
            return Err((
                self,
                completion,
                "ordinary window/world completion is not original or remains active".into(),
            ));
        }
        // Completion validates the retained original presentation; all other
        // asynchronous/world owners have completed before their remainder drops.
        self.owner.armed = false;
        let closed_world = OrdinaryClosedWorld::from_completed(
            self.owner.game.as_mut().unwrap(),
            &self.owner.profile,
        );
        let game = self
            .owner
            .game
            .take()
            .expect("original completed ordinary Game");
        let mut returned = super::window_close::ReturnedFrontend::from_completed_game(game);
        returned.closed_world = closed_world;
        drop(completion);
        Ok(returned)
    }
    /// This server path keeps the original Ready Hub in Game until native
    /// projection, metadata acknowledgment and all writers have completed.
    pub(super) fn try_dispose_server(mut self) -> Result<NativeDisposed, (Self, String)> {
        let game = self.owner.game.as_ref().unwrap();
        if !self.owner.native_ready_proven()
            || (!matches!(self.owner.profile, CloseProfile::NativeWorld)
                && game.preflight_ordinary_coherent_owner().is_err())
            || !dormant_async_owners_absent(game, true)
            || !game
                .native_close_authority
                .as_ref()
                .is_some_and(|authority| authority.is_server() && authority.authorizes(game))
        {
            return Err((
                self,
                "original server native/transport completion incomplete".into(),
            ));
        }
        let host = self.owner.game.as_mut().unwrap().lan.take();
        let closed = match host {
            Some(host) => match host.try_retained_closed() {
                Ok(closed) => Some(closed),
                Err(host) => {
                    self.owner.game.as_mut().unwrap().lan = Some(host);
                    return Err((
                        self,
                        "original server Hub consuming close refused; host restored".into(),
                    ));
                }
            },
            None => None,
        };
        let ordinary_world = OrdinaryClosedWorld::from_completed(
            self.owner.game.as_mut().unwrap(),
            &self.owner.profile,
        );
        self.owner.armed = false;
        let server_transport = closed.as_ref().map(|host| host.hub.view());
        drop(self.owner.game.take());
        drop(closed);
        Ok(NativeDisposed {
            _native_only: (),
            server_transport,
            ordinary_world,
        })
    }
    pub(super) fn observe(&self) -> CloseObservation {
        self.owner.observe()
    }
    pub(super) fn window_presentation(&self) -> super::window_close::Presentation {
        self.owner.window_presentation()
    }
    pub(super) fn resize_window(&mut self, width: u32, height: u32) {
        self.owner.resize_window(width, height);
    }
    pub(super) fn redraw_window(&self, title: &str) {
        self.owner.redraw_window(title);
    }
    pub(super) fn draw_window_status(&mut self, text: &str) -> horizon_client::Result<bool> {
        self.owner.draw_window_status(text)
    }
    /// Distinct from dormant disposal: the constructor-bound client handoff
    /// owns original library closures, and this owner still holds the original
    /// renderer/window until their owning-thread destruction below.
    pub(super) fn try_dispose_window(
        mut self,
        completion: super::window_close::ClientCompletion,
    ) -> Result<NativeDisposed, (Self, super::window_close::ClientCompletion, String)> {
        let game = self.owner.game.as_ref().unwrap();
        let absent = game.lan.is_none()
            && game.remote.is_none()
            && game.integrated.is_none()
            && game.serving.is_none()
            && game.friends.is_none()
            && game.control.is_none()
            && game.world_loader.is_none()
            && game.rest_loader.is_none()
            && game.retained_prepare_loader.is_none()
            && game.retained_preparation.is_none()
            && game.retained_entry_owner.is_none()
            && game.pending_entry.is_none()
            && game.pending_save.is_none()
            && game.adv.pending_tree.is_none()
            && game.lod.is_none();
        if !self.owner.native_ready_proven() || !absent || !completion.matches(game) {
            return Err((
                self,
                completion,
                "original window/client/native completion incomplete".into(),
            ));
        }
        self.owner.armed = false;
        // winit invokes this fixed transition on the renderer/window owning
        // thread. No GPU-driver quiescence or hard Drop latency is asserted.
        drop(self.owner.game.take());
        drop(completion);
        Ok(NativeDisposed {
            _native_only: (),
            server_transport: None,
            ordinary_world: None,
        })
    }
    pub(super) fn try_dispose_dormant(mut self) -> Result<NativeDisposed, (Self, String)> {
        if !self.owner.native_ready_proven()
            || self.owner.game.as_ref().unwrap().retained_server.is_some()
            || !dormant_async_owners_absent(self.owner.game.as_ref().unwrap(), false)
        {
            return Err((
                self,
                "remaining original asynchronous or transport owners require composition".into(),
            ));
        }
        self.owner.armed = false;
        // Metadata has already been acknowledged and every native actual handle
        // is terminal; remaining native file/resource owners have no writer.
        drop(self.owner.game.take());
        Ok(NativeDisposed {
            _native_only: (),
            server_transport: None,
            ordinary_world: None,
        })
    }
}

struct CloseWriter {
    save: Arc<horizon_client::save::SaveDir>,
    controller: Option<NativeWriterStaged>,
    historical: Option<NativeWriterDurable>,
    capability: Option<Arc<NativeWriterFinal>>,
    input: Option<NativeWriterFinalInput>,
    submitted: Option<NativeWriterFinalSubmitted>,
    sealed: bool,
}
#[derive(Clone)]
struct FinalWriterJob {
    capability: Arc<NativeWriterFinal>,
    input: NativeWriterFinalInput,
}
struct NativeCloseInput {
    metadata: Arc<Input>,
    level: super::NativeLevelInput,
    writers: Vec<FinalWriterJob>,
    // The ordinary coherent checkpoint persists this actual loaded projection
    // even when clean; it does not acknowledge a fabricated dirty marker.
    session_raids: Option<(std::path::PathBuf, horizon_world::nbt::Tag)>,
}
struct NativeCloseWritten {
    metadata: Option<Outcome>,
    level: Option<super::NativeLevelPost>,
    submitted: Vec<NativeWriterFinalSubmitted>,
    error: Option<String>,
    level_effects: [horizon_client::save::NativeLevelWriteEffect; 6],
}
enum CloseIoJob {
    NativeMetadata {
        input: Arc<NativeCloseInput>,
        batch: Arc<BatchIdentity>,
        attempt: Arc<AttemptIdentity>,
        #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
        control: Option<MetadataTestControl>,
    },
    CoherentRead(CoherentImageReadInput),
    NativeRead(NativeWorldReadInput),
    CoherentImage {
        input: CoherentImageBuildInput,
        attempt: Arc<CoherentImageAttempt>,
    },
}
enum CloseIoResult {
    NativeMetadata(NativeCloseWritten),
    CoherentRead(Result<CoherentImageReadPost, String>),
    NativeRead(Result<NativeWorldReadPost, String>),
    CoherentImage(Result<CoherentImageWritten, String>),
}
impl CloseIoJob {
    fn run(self) -> CloseIoResult {
        match self {
            Self::NativeRead(input) => {
                CloseIoResult::NativeRead(input.run().map_err(|e| e.to_string()))
            }
            Self::CoherentRead(input) => {
                CloseIoResult::CoherentRead(input.run().map_err(|e| e.to_string()))
            }
            Self::CoherentImage { input, attempt } => CloseIoResult::CoherentImage(
                input
                    .build_verify_publish(&attempt)
                    .map_err(|e| e.to_string()),
            ),
            Self::NativeMetadata {
                input,
                batch,
                attempt,
                #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
                control,
            } => {
                let mut out = NativeCloseWritten {
                    metadata: None,
                    level: None,
                    submitted: Vec::new(),
                    error: None,
                    level_effects: [horizon_client::save::NativeLevelWriteEffect::Unattempted; 6],
                };
                // Capture unknown fields and every native conditional decision
                // before core Clock or any other final metadata mutation.
                let operation = (|| -> Result<(), String> {
                    input.level.capture_native()?;
                    for writer in &input.writers {
                        out.submitted.push(
                            writer
                                .capability
                                .submit_native(&writer.input)
                                .map_err(|e| e.to_string())?,
                        );
                    }
                    let metadata = write_metadata(Request {
                        batch,
                        attempt,
                        input: input.metadata.clone(),
                        #[cfg(all(
                            test,
                            feature = "native-writer-test-control",
                            debug_assertions
                        ))]
                        control,
                    });
                    let success = metadata.success();
                    let error = metadata.error.clone();
                    out.metadata = Some(metadata);
                    if !success {
                        return Err(
                            error.unwrap_or_else(|| "native metadata outcome incomplete".into())
                        );
                    }
                    if let Some((path, tag)) = &input.session_raids {
                        horizon_client::save::write_dat(path, tag.clone())
                            .map_err(|e| e.to_string())?;
                    }
                    let level = input.level.write_native();
                    out.level_effects = level.native_effects;
                    out.level = Some(level.result?);
                    Ok(())
                })();
                out.error = operation.err();
                CloseIoResult::NativeMetadata(out)
            }
        }
    }
}
struct CloseIoAttempt {
    number: u64,
}
struct CloseIoReturn {
    attempt: Arc<CloseIoAttempt>,
    result: CloseIoResult,
}

/// The original Box is never exposed mutably while closing or blocked. Private
/// state holds no reference into it, so moving the owner creates no self-reference.
pub(super) struct CloseOwner<'window> {
    armed: bool,
    game: Option<Box<Game<'window>>>,
    profile: CloseProfile,
    phase: ClosePhase,
    deadline: Instant,
    blocked: Option<String>,
    failures: Vec<String>,
    metadata: MetadataState,
    metadata_ack: Option<MetadataAcknowledged>,
    levels: Option<RetainedNativeLevels>,
    payloads: Option<RetainedNativePayloads>,
    writers: Vec<CloseWriter>,
    closed: Vec<NativeWriterClosed>,
    level_expected: Option<super::NativeLevelExpected>,
    native_input: Option<Arc<NativeCloseInput>>,
    native_written: Option<NativeCloseWritten>,
    failed_native_attempts: Vec<NativeCloseWritten>,
    io_uncertainties: Vec<(u64, ClosePhase)>,
    image_context: Option<Arc<CoherentImageContext>>,
    image_read: Option<CoherentImageReadPost>,
    native_read_context: Option<Arc<NativeWorldReadContext>>,
    native_read: Option<NativeWorldReadPost>,
    image_input: Option<CoherentImageBuildInput>,
    image_attempt: Option<Arc<CoherentImageAttempt>>,
    old_image_attempts: Vec<Arc<CoherentImageAttempt>>,
    image_written: Option<CoherentImageWritten>,
    next_image_epoch: u64,
    io_handle: Option<JoinHandle<Option<CloseIoReturn>>>,
    io_attempt: Option<Arc<CloseIoAttempt>>,
    io_number: u64,
    identity_preparations: usize,
    #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
    image_control: Option<super::levels::session_checkpoint::CoherentImageTestControl>,
    #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
    read_control: Option<super::levels::session_checkpoint::CoherentImageTestControl>,
}
impl<'window> CloseOwner<'window> {
    pub(super) fn window_presentation(&self) -> super::window_close::Presentation {
        super::window_close::presentation(self.game.as_ref().unwrap())
    }
    pub(super) fn resize_window(&mut self, width: u32, height: u32) {
        super::window_close::resize(self.game.as_mut().unwrap(), width, height);
    }
    pub(super) fn redraw_window(&self, title: &str) {
        super::window_close::request_redraw(self.game.as_ref().unwrap(), title);
    }
    pub(super) fn draw_window_status(&mut self, text: &str) -> horizon_client::Result<bool> {
        super::window_close::draw_status(self.game.as_mut().unwrap(), text)
    }
    /// Startup failure can enter close before winit's first resumed callback.
    /// This fixed original presentation recipe cannot mutate frozen world data.
    pub(super) fn create_window_presentation(
        &mut self,
        window: Arc<winit::window::Window>,
    ) -> horizon_client::Result<()> {
        let game = self.game.as_mut().unwrap();
        if game.window.is_some() || game.renderer.is_some() {
            return Err("original close presentation was already acquired".into());
        }
        game.window = Some(window.clone());
        game.renderer = Some(super::window_close::renderer(
            window,
            &game.pack,
            &game.gui,
            game.settings.horizon.api,
        )?);
        Ok(())
    }
    #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
    pub(super) fn hold_window_native_io_for_test(&mut self) -> WindowIoTestGate {
        let (control, receipt) = MetadataTestControl::new(MetadataTestAction::HoldBeforeIo);
        self.metadata.control = Some(control);
        WindowIoTestGate(receipt)
    }
    pub(super) fn begin(
        game: Box<Game<'window>>,
        profile: CloseProfile,
        deadline: Instant,
    ) -> Box<Self> {
        let mut metadata = MetadataState::new(deadline);
        metadata.level_raids = true;
        Box::new(Self {
            armed: true,
            game: Some(game),
            profile,
            phase: ClosePhase::Resources,
            deadline,
            blocked: None,
            failures: Vec::new(),
            metadata,
            metadata_ack: None,
            levels: None,
            payloads: None,
            writers: Vec::new(),
            closed: Vec::new(),
            level_expected: None,
            native_input: None,
            native_written: None,
            failed_native_attempts: Vec::new(),
            io_uncertainties: Vec::new(),
            image_context: None,
            image_read: None,
            native_read_context: None,
            native_read: None,
            image_input: None,
            image_attempt: None,
            old_image_attempts: Vec::new(),
            image_written: None,
            next_image_epoch: 0,
            io_handle: None,
            io_attempt: None,
            io_number: 0,
            identity_preparations: 0,
            #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
            image_control: None,
            #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
            read_control: None,
        })
    }
    pub(super) fn failures(&self) -> &[String] {
        &self.failures
    }
    #[cfg(all(
        test,
        feature = "native-writer-test-control",
        debug_assertions,
        target_os = "linux"
    ))]
    fn foreign_native_read_context_refuses_for_test(&self) -> Result<bool, String> {
        let game = self.game.as_ref().unwrap();
        let levels = self.levels.as_ref().ok_or("native levels absent")?;
        let post = self
            .native_read
            .as_ref()
            .ok_or("actual native read outcome absent")?;
        let foreign = game
            .prepare_native_world_read_context(levels)
            .map_err(|e| e.to_string())?;
        Ok(game
            .preflight_native_world_read(&foreign, levels, post)
            .is_err())
    }
    pub(super) fn identity_preparations(&self) -> usize {
        self.identity_preparations
    }
    pub(super) fn observe(&self) -> CloseObservation {
        let game = self.game.as_ref().unwrap();
        CloseObservation {
            phase: self.phase,
            dimension: game.dim,
            participants: game.lan.as_ref().map_or(0, |host| host.hub.guests.len()),
            levels: self.writers.len(),
            identity_preparations: self.identity_preparations,
            failed_attempts: self.failed_native_attempts.len() + self.io_uncertainties.len(),
            read_refusals: self
                .levels
                .as_ref()
                .map_or_else(Vec::new, |levels| levels.read_refusals().to_vec()),
            image: self
                .image_written
                .as_ref()
                .map(|written| (written.receipt().epoch, written.receipt().root.clone())),
            image_attempt: self
                .image_attempt
                .as_deref()
                .map(CloseImageObservation::capture),
            prior_images: self
                .old_image_attempts
                .iter()
                .map(|attempt| CloseImageObservation::capture(attempt))
                .collect(),
        }
    }
    fn native_ready_proven(&self) -> bool {
        self.phase == ClosePhase::NativeReady
            && self.blocked.is_none()
            && self.io_handle.is_none()
            && !self.metadata.armed
            && self.metadata_ack.is_some()
            && !self.metadata.tree.unresolved()
            && self
                .levels
                .as_ref()
                .is_some_and(|levels| levels.acknowledged())
            && self
                .writers
                .iter()
                .all(|writer| writer.controller.is_none())
            && self.closed.len() == self.writers.len()
    }
    pub(super) fn try_native_ready(self: Box<Self>) -> Result<NativeReady<'window>, Box<Self>> {
        if !self.native_ready_proven() {
            return Err(self);
        }
        Ok(NativeReady { owner: self })
    }
    fn block(&mut self, error: impl Into<String>) {
        let error = error.into();
        if !self.failures.contains(&error) {
            self.failures.push(error.clone());
        }
        self.blocked.get_or_insert(error);
    }
    fn dispatch(&mut self, job: CloseIoJob) -> Result<(), String> {
        if self.io_handle.is_some() {
            return Err("close IO still busy".into());
        }
        #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
        if self
            .metadata
            .control
            .as_ref()
            .is_some_and(MetadataTestControl::refuse_spawn)
        {
            return Err("controlled close IO spawn refusal".into());
        }
        let number = self
            .io_number
            .checked_add(1)
            .ok_or("close IO attempt exhausted")?;
        self.io_number = number;
        let attempt = Arc::new(CloseIoAttempt { number });
        let (send, receive) = std::sync::mpsc::sync_channel::<(Arc<CloseIoAttempt>, CloseIoJob)>(1);
        let handle = std::thread::Builder::new()
            .name("Retained native close IO".into())
            .spawn(move || {
                receive.recv().ok().map(|(attempt, job)| CloseIoReturn {
                    attempt,
                    result: job.run(),
                })
            })
            .map_err(|e| format!("close IO spawn: {e}"))?;
        // Register the actual handle before the worker receives any owned input.
        self.io_handle = Some(handle);
        self.io_attempt = Some(attempt.clone());
        #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
        if self
            .metadata
            .control
            .as_ref()
            .is_some_and(MetadataTestControl::disconnect_dispatch)
        {
            drop(send);
            drop(job);
            return Err("controlled close IO dispatch disconnect".into());
        }
        send.try_send((attempt, job))
            .map_err(|_| "close IO dispatch disconnected".into())
    }
    fn harvest(&mut self) {
        if !self.io_handle.as_ref().is_some_and(|h| h.is_finished()) {
            return;
        }
        match self.io_handle.take().unwrap().join() {
            Ok(Some(returned)) => {
                if !self
                    .io_attempt
                    .as_ref()
                    .is_some_and(|a| Arc::ptr_eq(a, &returned.attempt))
                {
                    self.block("close IO returned a foreign actual attempt");
                    return;
                }
                match returned.result {
                    CloseIoResult::NativeRead(result) => match result {
                        Ok(read) => {
                            self.native_read = Some(read);
                            self.phase = ClosePhase::Prepare;
                        }
                        Err(error) => self.block(error),
                    },
                    CloseIoResult::CoherentRead(result) => match result {
                        Ok(read) => {
                            self.next_image_epoch = read.next_epoch();
                            self.image_read = Some(read);
                            self.phase = ClosePhase::Prepare;
                        }
                        Err(e) => self.block(e),
                    },
                    CloseIoResult::NativeMetadata(written) => {
                        let error = written.error.clone();
                        self.native_written = Some(written);
                        if let Some(e) = error {
                            self.block(e);
                        } else {
                            self.phase = ClosePhase::FinalWriters;
                        }
                    }
                    CloseIoResult::CoherentImage(result) => match result {
                        Ok(written) => {
                            self.image_written = Some(written);
                            self.phase = ClosePhase::Preflight;
                        }
                        Err(e) => self.block(e),
                    },
                }
            }
            Ok(None) => {
                self.io_uncertainties
                    .push((self.io_attempt.as_ref().unwrap().number, self.phase));
                self.block("close IO exited without dispatched outcome; file effects unknown");
            }
            Err(_) => {
                self.io_uncertainties
                    .push((self.io_attempt.as_ref().unwrap().number, self.phase));
                self.block(
                    "close IO panicked; native effects unknown and publication remains unproven",
                );
            }
        }
    }
    pub(super) fn poll(&mut self) -> CloseStatus {
        self.harvest();
        if self.phase == ClosePhase::NativeReady {
            return CloseStatus::NativeReady;
        }
        // Reap genuine original helpers even after an earlier refusal/deadline.
        // These operations cannot acknowledge Game markers or dispatch final IO.
        if self.metadata.tree_started {
            self.metadata.poll_tree(self.game.as_mut().unwrap(), false);
        }
        let mut writer_errors = Vec::new();
        for writer in &mut self.writers {
            if let Some(controller) = &mut writer.controller
                && let NativeWriterStagedStatus::Blocked(error) = controller.poll()
            {
                writer_errors.push(error);
            }
        }
        for error in writer_errors {
            self.block(error);
        }
        if Instant::now() >= self.deadline {
            self.block("coherent native close absolute deadline expired");
        }
        // Actual outstanding work is harvested even when blocked. Never dispatch
        // a new operation or silently renew an expired attempt here.
        if let Some(error) = &self.blocked {
            return CloseStatus::Blocked(error.clone());
        }
        if self.io_handle.is_none() {
            match self.advance() {
                Ok(Some(job)) => {
                    if let Err(error) = self.dispatch(job) {
                        self.block(error);
                    }
                }
                Ok(None) => {}
                Err(error) => self.block(error),
            }
        }
        match &self.blocked {
            Some(error) => CloseStatus::Blocked(error.clone()),
            None if self.phase == ClosePhase::NativeReady => CloseStatus::NativeReady,
            None => CloseStatus::Pending(self.phase),
        }
    }
    fn advance(&mut self) -> Result<Option<CloseIoJob>, String> {
        let game = self.game.as_mut().ok_or("original close Game absent")?;
        if game.retained_server.is_some() && !matches!(self.profile, CloseProfile::NativeWorld) {
            game.preflight_ordinary_coherent_owner()?;
        }
        match self.phase {
            ClosePhase::Resources => {
                if matches!(self.profile, CloseProfile::CoherentSession { .. })
                    && !self.metadata.tree_started
                    && game.adv.tree.list.is_empty()
                    && game.adv.pending_tree.is_none()
                {
                    game.prefetch_advancement_tree();
                    if game.adv.pending_tree.is_none() {
                        return Err("native advancement resource helper failed to start".into());
                    }
                }
                match self.metadata.poll_tree(game, true) {
                    RetainedTreeStatus::Ready | RetainedTreeStatus::Skipped(_) => {
                        if let Some(error) = &self.metadata.blocked {
                            return Err(error.clone());
                        }
                        self.levels = Some(game.begin_retained_native_levels(self.deadline)?);
                        self.phase = ClosePhase::Producers;
                    }
                    RetainedTreeStatus::Failed(error) => return Err(error),
                    _ => {}
                }
            }
            ClosePhase::Producers => {
                match game.poll_retained_native_levels(self.levels.as_mut().unwrap()) {
                    NativeLevelsStatus::Pending => {}
                    NativeLevelsStatus::Blocked(error) => return Err(error),
                    NativeLevelsStatus::Sealed => {
                        self.writers = self
                            .levels
                            .as_ref()
                            .unwrap()
                            .saves()
                            .into_iter()
                            .map(|save| CloseWriter {
                                save,
                                controller: None,
                                historical: None,
                                capability: None,
                                input: None,
                                submitted: None,
                                sealed: false,
                            })
                            .collect();
                        self.phase = ClosePhase::Historical;
                    }
                }
            }
            ClosePhase::Historical => {
                for writer in &mut self.writers {
                    if writer.controller.is_none() {
                        writer.controller = writer
                            .save
                            .begin_staged_writer_close(self.deadline)
                            .map_err(|e| e.to_string())?;
                        return Ok(None);
                    }
                    if writer.historical.is_none() {
                        let controller = writer.controller.as_mut().unwrap();
                        match controller.poll() {
                            NativeWriterStagedStatus::Durable => {
                                writer.historical =
                                    controller.durability().map_err(|e| e.to_string())?
                            }
                            NativeWriterStagedStatus::Blocked(error) => return Err(error),
                            _ => {}
                        }
                        return Ok(None);
                    }
                }
                self.phase = ClosePhase::ReadPreflight;
            }
            ClosePhase::ReadPreflight => match &self.profile {
                CloseProfile::CoherentSession { archive } => {
                    if self.image_context.is_none() {
                        self.image_context = Some(
                            game.prepare_coherent_image_context(archive)
                                .map_err(|e| e.to_string())?,
                        );
                    }
                    let input = self
                        .image_context
                        .as_ref()
                        .unwrap()
                        .read_preflight_input(self.deadline);
                    #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
                    let input = match &self.read_control {
                        Some(control) => input.with_test_control(control.clone()),
                        None => input,
                    };
                    return Ok(Some(CloseIoJob::CoherentRead(input)));
                }
                CloseProfile::NativeWorld => {
                    if self.native_read_context.is_none() {
                        self.native_read_context = Some(
                            game.prepare_native_world_read_context(self.levels.as_ref().unwrap())
                                .map_err(|e| e.to_string())?,
                        );
                    }
                    let input = self
                        .native_read_context
                        .as_ref()
                        .unwrap()
                        .read_preflight_input(self.deadline);
                    #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
                    let input = match &self.read_control {
                        Some(control) => input.with_test_control(control.clone()),
                        None => input,
                    };
                    return Ok(Some(CloseIoJob::NativeRead(input)));
                }
            },
            ClosePhase::Prepare => {
                if self.payloads.is_none() {
                    let reserved = match &self.native_read {
                        Some(read) => read.reserved_uuids(),
                        None => self
                            .image_read
                            .as_ref()
                            .ok_or("old-source preflight missing")?
                            .reserved_item_identities()
                            .into_iter()
                            .collect(),
                    };
                    let prepared = game.prepare_close_identity_fields_avoiding(&reserved)?;
                    self.identity_preparations += prepared.count();
                    self.payloads = Some(game.prepare_retained_native_payloads(
                        self.levels.as_mut().unwrap(),
                        &reserved,
                    )?);
                }
                if let Some(read) = &self.native_read {
                    read.preflight_final_payloads(self.payloads.as_ref().unwrap())
                        .map_err(|e| e.to_string())?;
                }
                if self.metadata.input.is_none() {
                    self.metadata.prepare(game)?;
                }
                if self.level_expected.is_none() {
                    let mut level = game.level.clone();
                    self.metadata
                        .input
                        .as_ref()
                        .unwrap()
                        .raids
                        .apply_to_level(&mut level);
                    let (expected, input) = game.prepare_retained_level_metadata_from(level)?;
                    let session_raids =
                        if matches!(self.profile, CloseProfile::CoherentSession { .. }) {
                            game.session_checkpoint_raids()
                                .map_err(|e| e.to_string())?
                                .map(|tag| (game.raids_path(game.dim), tag))
                        } else {
                            None
                        };
                    for (writer, payload) in self
                        .writers
                        .iter_mut()
                        .zip(self.payloads.as_ref().unwrap().levels())
                    {
                        if !Arc::ptr_eq(&writer.save, payload.save()) {
                            return Err("fixed native writer/payload order changed".into());
                        }
                        writer.input = Some(
                            NativeWriterFinalInput::new(
                                &writer.save,
                                payload.chunks(),
                                payload.entities(),
                                payload.forced(),
                                payload.protected_nonresident(),
                                game.pack.assets.registry,
                                game.res.biomes(),
                            )
                            .map_err(|e| e.to_string())?,
                        );
                    }
                    self.level_expected = Some(expected);
                    self.native_input = Some(Arc::new(NativeCloseInput {
                        metadata: self.metadata.input.as_ref().unwrap().clone(),
                        level: input,
                        writers: Vec::new(),
                        session_raids,
                    }));
                }
                for writer in &mut self.writers {
                    if writer.capability.is_none() {
                        writer.capability = writer
                            .controller
                            .as_mut()
                            .unwrap()
                            .begin_final(writer.historical.as_ref().unwrap())
                            .map_err(|e| e.to_string())?
                            .map(Arc::new);
                        return Ok(None);
                    }
                }
                let native = Arc::get_mut(self.native_input.as_mut().unwrap())
                    .ok_or("native input escaped before fixed writer binding")?;
                native.writers = self
                    .writers
                    .iter()
                    .map(|writer| FinalWriterJob {
                        capability: writer.capability.as_ref().unwrap().clone(),
                        input: writer.input.as_ref().unwrap().clone(),
                    })
                    .collect();
                if matches!(self.profile, CloseProfile::CoherentSession { .. })
                    && self.image_input.is_none()
                {
                    self.image_input = Some(
                        game.freeze_coherent_image_input(
                            self.image_context.as_ref().unwrap(),
                            self.image_read.as_ref().unwrap(),
                        )
                        .map_err(|e| e.to_string())?,
                    );
                }
                self.phase = ClosePhase::NativeIo;
            }
            ClosePhase::NativeIo => {
                let next = self
                    .metadata
                    .next_attempt
                    .checked_add(1)
                    .ok_or("native metadata attempt exhausted")?;
                self.metadata.next_attempt = next;
                let attempt = Arc::new(AttemptIdentity { number: next });
                self.metadata.attempt = Some(attempt.clone());
                return Ok(Some(CloseIoJob::NativeMetadata {
                    input: self.native_input.as_ref().unwrap().clone(),
                    batch: self.metadata.batch.clone(),
                    attempt,
                    #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
                    control: self.metadata.control.clone(),
                }));
            }
            ClosePhase::FinalWriters => {
                let written = self
                    .native_written
                    .as_mut()
                    .ok_or("native final outcome absent")?;
                if written.error.is_some()
                    || written.level.is_none()
                    || written.submitted.len() != self.writers.len()
                {
                    return Err("native final outcome incomplete".into());
                }
                for (writer, receipt) in self.writers.iter_mut().zip(&written.submitted) {
                    if writer.sealed || writer.controller.is_none() {
                        continue;
                    }
                    if writer
                        .controller
                        .as_mut()
                        .unwrap()
                        .seal_final(receipt)
                        .map_err(|e| e.to_string())?
                        .is_some()
                    {
                        writer.sealed = true;
                    }
                    return Ok(None);
                }
                for writer in &mut self.writers {
                    let Some(controller) = writer.controller.as_mut() else {
                        continue;
                    };
                    match controller.poll() {
                        NativeWriterStagedStatus::Complete => {
                            let original = writer.controller.take().unwrap();
                            match original.finish() {
                                Ok(closed) => self.closed.push(closed),
                                Err(original) => writer.controller = Some(original),
                            }
                        }
                        NativeWriterStagedStatus::Blocked(error) => return Err(error),
                        _ => {}
                    }
                    return Ok(None);
                }
                match game
                    .poll_retained_native_writer_ack(self.levels.as_mut().unwrap(), &self.closed)
                {
                    NativeLevelsStatus::Pending => {}
                    NativeLevelsStatus::Blocked(error) => return Err(error),
                    NativeLevelsStatus::Sealed => {
                        self.phase = if self.image_input.is_some() {
                            ClosePhase::Image
                        } else {
                            ClosePhase::Preflight
                        }
                    }
                }
            }
            ClosePhase::Image => {
                if self.image_attempt.is_none() {
                    let epoch = self.next_image_epoch;
                    self.next_image_epoch = epoch
                        .checked_add(1)
                        .ok_or("coherent image epoch exhausted")?;
                    let input = self.image_input.as_ref().unwrap();
                    #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
                    let attempt = match &self.image_control {
                        Some(control) => {
                            input.attempt_with_test_control(epoch, self.deadline, control.clone())
                        }
                        None => input.attempt(epoch, self.deadline),
                    };
                    #[cfg(not(all(
                        test,
                        feature = "native-writer-test-control",
                        debug_assertions
                    )))]
                    let attempt = input.attempt(epoch, self.deadline);
                    self.image_attempt = Some(attempt.map_err(|e| e.to_string())?);
                }
                return Ok(Some(CloseIoJob::CoherentImage {
                    input: self.image_input.as_ref().unwrap().clone(),
                    attempt: self.image_attempt.as_ref().unwrap().clone(),
                }));
            }
            ClosePhase::Preflight => {
                if let (Some(context), Some(read)) = (&self.native_read_context, &self.native_read)
                {
                    game.preflight_native_world_read(context, self.levels.as_ref().unwrap(), read)
                        .map_err(|e| e.to_string())?;
                    read.preflight_final_payloads(self.payloads.as_ref().unwrap())
                        .map_err(|e| e.to_string())?;
                }
                game.preflight_retained_native_payloads(
                    self.levels.as_ref().unwrap(),
                    self.payloads.as_ref().unwrap(),
                )?;
                let written = self
                    .native_written
                    .as_ref()
                    .ok_or("native final outcome absent")?;
                game.preflight_retained_level_metadata(
                    self.level_expected.as_ref().unwrap(),
                    written.level.as_ref().ok_or("native Level result absent")?,
                )?;
                // Move the actual same-batch result into the metadata state only
                // after its worker has finished; no independent IO handle exists.
                if self.metadata.outcome.is_none() {
                    self.metadata.outcome = self.native_written.as_mut().unwrap().metadata.take();
                }
                self.metadata.preflight(game)?;
                if let Some(input) = &self.image_input {
                    game.preflight_coherent_image(
                        input,
                        self.image_attempt.as_ref().unwrap(),
                        self.image_written
                            .as_ref()
                            .ok_or("verified coherent image absent")?,
                    )
                    .map_err(|e| e.to_string())?;
                }
                if !self.levels.as_ref().unwrap().acknowledged() {
                    return Err("native producer/writer acknowledgements incomplete".into());
                }
                if Instant::now() >= self.deadline {
                    return Err(
                        "coherent native close deadline expired during aggregate preflight".into(),
                    );
                }
                // No fallible action or projection follows the first marker ack.
                self.metadata_ack = Some(self.metadata.commit_preflighted(game));
                game.commit_retained_native_payloads(self.levels.as_mut().unwrap());
                game.commit_retained_level_metadata(
                    self.native_written.as_mut().unwrap().level.take().unwrap(),
                );
                self.phase = ClosePhase::NativeReady;
            }
            ClosePhase::NativeReady => {}
        }
        Ok(None)
    }
    pub(super) fn retry(&mut self, deadline: Instant) -> Result<(), CloseRetryRefusal> {
        self.harvest();
        if self.io_handle.is_some() {
            return Err(CloseRetryRefusal::Busy);
        }
        if self.blocked.is_none() {
            return Err(CloseRetryRefusal::NotBlocked);
        }
        if deadline <= Instant::now() {
            return Err(CloseRetryRefusal::Deadline);
        }
        let game = self.game.as_mut().unwrap();
        if self.metadata.tree.unresolved() {
            self.metadata.poll_tree(game, false);
            if self.metadata.tree.unresolved() {
                return Err(CloseRetryRefusal::Busy);
            }
        }
        if matches!(self.metadata.tree.status(), RetainedTreeStatus::Failed(_)) {
            game.retry_metadata_advancement_tree(&mut self.metadata.tree)
                .map_err(|e| CloseRetryRefusal::Refused(e.to_string()))?;
        }
        if self.phase == ClosePhase::Producers {
            game.retry_retained_native_levels(self.levels.as_mut().unwrap(), deadline)
                .map_err(CloseRetryRefusal::Refused)?;
        }
        for writer in &mut self.writers {
            if let Some(controller) = &mut writer.controller {
                match controller.poll() {
                    NativeWriterStagedStatus::Blocked(_)
                    | NativeWriterStagedStatus::Durable
                    | NativeWriterStagedStatus::Submitting => controller
                        .retry(deadline)
                        .map_err(|e| CloseRetryRefusal::Refused(e.to_string()))?,
                    _ => {}
                }
            }
        }
        if self.phase == ClosePhase::Image {
            // Every failed/uncertain image keeps its original paths and phase.
            // New publication attempts use a fresh epoch and the same projection.
            if let Some(attempt) = self.image_attempt.take() {
                self.old_image_attempts.push(attempt);
            }
        }
        if self.phase == ClosePhase::NativeIo
            && let Some(written) = self.native_written.take()
        {
            self.failures.push(format!(
                "retained native attempt {} effects {:?}, Level {:?}: {:?}",
                self.metadata.next_attempt,
                written.metadata.as_ref().map(|out| out.effects),
                written.level_effects,
                written.error
            ));
            self.failed_native_attempts.push(written);
            // Complete immutable input/capabilities remain in native_input;
            // no final writer has been sealed on an unsuccessful worker return.
        }
        if self.phase == ClosePhase::ReadPreflight && self.image_read.is_some() {
            self.phase = ClosePhase::Prepare;
        }
        self.deadline = deadline;
        self.metadata.deadline = deadline;
        self.metadata.expired = false;
        self.metadata.blocked = None;
        self.blocked = None;
        Ok(())
    }
}
impl Drop for CloseOwner<'_> {
    fn drop(&mut self) {
        if self.armed {
            use std::io::Write;
            let _ = writeln!(
                std::io::stderr().lock(),
                "fatal: unresolved coherent native close owner"
            );
            std::process::abort();
        }
    }
}

#[cfg(all(
    test,
    feature = "native-writer-test-control",
    debug_assertions,
    target_os = "linux"
))]
pub(in crate::game) struct TestCloseCustody<'window> {
    owner: Option<Box<CloseOwner<'window>>>,
    token: Box<super::levels::session_checkpoint::tests::RetainedCloseFixtureToken>,
    receivers: Vec<std::sync::mpsc::Receiver<horizon_client::lan::Out>>,
}
#[cfg(all(
    test,
    feature = "native-writer-test-control",
    debug_assertions,
    target_os = "linux"
))]
impl<'window> TestCloseCustody<'window> {
    pub(in crate::game) fn from_constructor(
        game: Box<Game<'window>>,
        profile: CloseProfile,
        deadline: Instant,
        token: super::levels::session_checkpoint::tests::RetainedCloseFixtureToken,
        receivers: Vec<std::sync::mpsc::Receiver<horizon_client::lan::Out>>,
    ) -> Self {
        assert!(
            token.matches(&game),
            "genuine uninterrupted fixture constructor custody"
        );
        Self {
            owner: Some(CloseOwner::begin(game, profile, deadline)),
            token: Box::new(token),
            receivers,
        }
    }
    pub(in crate::game) fn poll(&mut self) -> CloseStatus {
        self.owner.as_mut().unwrap().poll()
    }
    pub(in crate::game) fn retry(&mut self, until: Instant) -> Result<(), CloseRetryRefusal> {
        self.owner.as_mut().unwrap().retry(until)
    }
    pub(in crate::game) fn observe(&self) -> CloseObservation {
        self.owner.as_ref().unwrap().observe()
    }
    pub(in crate::game) fn failures(&self) -> &[String] {
        self.owner.as_ref().unwrap().failures()
    }
    pub(in crate::game) fn observe_native(
        &self,
    ) -> super::levels::session_checkpoint::tests::RetainedCloseFixtureObservation {
        self.token
            .observe(self.owner.as_ref().unwrap().game.as_ref().unwrap())
    }
    pub(in crate::game) fn source_root(&self) -> std::path::PathBuf {
        self.owner
            .as_ref()
            .unwrap()
            .game
            .as_ref()
            .unwrap()
            .world_root
            .clone()
    }
    pub(in crate::game) fn recovery_request(
        &self,
        destination: std::path::PathBuf,
        until: Instant,
    ) -> super::levels::session_checkpoint::RecoveryRequest {
        self.token.recovery_request(destination, until)
    }
    fn metadata_control(&mut self, control: MetadataTestControl) {
        self.owner.as_mut().unwrap().metadata.control = Some(control);
    }
    fn image_control(
        &mut self,
        control: super::levels::session_checkpoint::CoherentImageTestControl,
    ) {
        self.owner.as_mut().unwrap().image_control = Some(control);
    }
    fn read_control(
        &mut self,
        control: super::levels::session_checkpoint::CoherentImageTestControl,
    ) {
        self.owner.as_mut().unwrap().read_control = Some(control);
    }
    fn io_busy(&self) -> bool {
        self.owner.as_ref().unwrap().io_handle.is_some()
    }
    fn expire_deadline_for_test(&mut self) {
        let owner = self.owner.as_mut().unwrap();
        owner.deadline = Instant::now();
        owner.metadata.deadline = owner.deadline;
    }
    pub(in crate::game) fn production_disposal_refused(&mut self) -> Result<bool, String> {
        let owner = self.owner.take().unwrap();
        let ready = match owner.try_native_ready() {
            Ok(ready) => ready,
            Err(owner) => {
                self.owner = Some(owner);
                return Err("native readiness not established".into());
            }
        };
        match ready.try_dispose_dormant() {
            Err((ready, _)) => {
                self.owner = Some(ready.owner);
                Ok(true)
            }
            Ok(_) => Ok(false),
        }
    }
    pub(in crate::game) fn try_dispose(mut self) -> Result<NativeDisposed, (Self, String)> {
        let owner = self.owner.as_ref().unwrap();
        if !owner.native_ready_proven()
            || !self.token.matches(owner.game.as_ref().unwrap())
            || !dormant_async_owners_absent(owner.game.as_ref().unwrap(), true)
        {
            return Err((
                self,
                "original fixture/native/remaining owner proof incomplete".into(),
            ));
        }
        let mut owner = self.owner.take().unwrap();
        owner.armed = false;
        drop(owner.game.take());
        // The original channel receivers remained in this custody for the whole
        // populated positive; dispose them only together with the proven host.
        self.receivers.clear();
        Ok(NativeDisposed {
            _native_only: (),
            server_transport: None,
            ordinary_world: None,
        })
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
pub(super) struct WindowIoTestGate(MetadataTestReceipt);
#[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
impl WindowIoTestGate {
    pub(super) fn entered(&self) -> bool {
        self.0.entered()
    }
    pub(super) fn release(&self) {
        self.0.release();
    }
}
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

#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions,
    target_os = "linux"
))]
#[path = "paired_close_tests.rs"]
mod close_tests;
