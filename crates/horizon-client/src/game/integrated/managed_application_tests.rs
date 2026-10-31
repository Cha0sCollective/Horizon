//! Coordinator-launched, one-case-per-process native window qualification.
//! No test starts another executable or changes process environment/cwd.
use super::*;
use crate::game::{
    integrated::{
        managed_stop::connected_tests::validate_continuation, transition_ownership_tests::Fixture,
    },
    lan::connected_recovery::tests::{
        Scratch, Source, client_matches, disconnect, inventory_count, inventory_tree, source,
        source_core_matches,
    },
    levels::session_checkpoint::{CheckpointTestFault, Publication},
};
use horizon_client::save::SessionLock;
use horizon_protocol::crypto::offline_uuid;
use std::{cell::RefCell, io::Write};
use winit::{dpi::PhysicalSize, platform::x11::EventLoopBuilderExtX11};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    Positive,
    StartupClose,
    Copied,
    Promoted,
    RenderBefore,
    RenderAfter,
    SkipFrame,
    PreReadyDeparture,
    SignalsReady,
    SignalsCopied,
    SignalsPromoted,
    NativeClose,
    NativeCopied,
    NativePromoted,
    FatalUnwind,
    FatalExit,
    LateDeparture,
}
impl Case {
    fn named(value: &str) -> Self {
        match value {
            "positive" => Self::Positive,
            "startup-close" => Self::StartupClose,
            "copied" => Self::Copied,
            "promoted" => Self::Promoted,
            "render-before" => Self::RenderBefore,
            "render-after" => Self::RenderAfter,
            "skip-frame" => Self::SkipFrame,
            "pre-ready-departure" => Self::PreReadyDeparture,
            "signals-ready" => Self::SignalsReady,
            "signals-copied" => Self::SignalsCopied,
            "signals-promoted" => Self::SignalsPromoted,
            "native-close" => Self::NativeClose,
            "native-copied" => Self::NativeCopied,
            "native-promoted" => Self::NativePromoted,
            "fatal-unwind" => Self::FatalUnwind,
            "fatal-exit" => Self::FatalExit,
            "late-departure" => Self::LateDeparture,
            _ => panic!("unknown bounded frozen window case"),
        }
    }
    fn fault(self) -> Option<CheckpointTestFault> {
        match self {
            Self::Copied | Self::SignalsCopied | Self::NativeCopied => {
                Some(CheckpointTestFault::Copied)
            }
            Self::Promoted | Self::SignalsPromoted | Self::NativePromoted => {
                Some(CheckpointTestFault::Promoted)
            }
            _ => None,
        }
    }
    fn signal(self) -> bool {
        matches!(
            self,
            Self::SignalsReady | Self::SignalsCopied | Self::SignalsPromoted
        )
    }
    fn screenshots(self) -> bool {
        !matches!(
            self,
            Self::StartupClose
                | Self::PreReadyDeparture
                | Self::FatalUnwind
                | Self::FatalExit
                | Self::LateDeparture
        )
    }
    fn native(self) -> bool {
        matches!(
            self,
            Self::NativeClose | Self::NativeCopied | Self::NativePromoted
        )
    }
    fn native_retry(self) -> bool {
        matches!(self, Self::NativeCopied | Self::NativePromoted)
    }
}

struct Screenshot {
    path: PathBuf,
    dimensions: Option<[u32; 2]>,
    failed: bool,
}

/// Expected source data and a fixed schedule, never a Game or worker callback.
pub(super) struct Probe {
    source: Source,
    case: Case,
    began: Instant,
    errors: Vec<String>,
    stage: u8,
    turns: u64,
    ready: u64,
    retained: u64,
    cleanup_blocked: u64,
    actual_drawn: u64,
    actual_skipped: u64,
    injected: u64,
    ui_exercised: bool,
    resize_seen: bool,
    minimize_ack: Option<bool>,
    startup_close: bool,
    departed: bool,
    retry: bool,
    signals_observed: u32,
    screenshots: [Option<Screenshot>; 2],
    native_lock_held: Option<bool>,
    uuid_lease_held: Option<bool>,
    keys: Option<(
        horizon_client::world::counter_restore::RuntimeCounterKey,
        horizon_client::world_slot_trial::SlotKey,
    )>,
}
impl Probe {
    fn new(source: Source, case: Case) -> Self {
        Self {
            source,
            case,
            began: Instant::now(),
            errors: Vec::new(),
            stage: 0,
            turns: 0,
            ready: 0,
            retained: 0,
            cleanup_blocked: 0,
            actual_drawn: 0,
            actual_skipped: 0,
            injected: 0,
            ui_exercised: false,
            resize_seen: false,
            minimize_ack: None,
            startup_close: false,
            departed: false,
            retry: false,
            signals_observed: 0,
            screenshots: [None, None],
            native_lock_held: None,
            uuid_lease_held: None,
            keys: None,
        }
    }
    fn record(&mut self, condition: bool, detail: &str) {
        if !condition && self.errors.len() < 24 && !self.errors.iter().any(|s| s == detail) {
            self.errors.push(detail.into());
        }
    }
    fn capture(&mut self, renderer: &mut Renderer, retained: bool) -> bool {
        let index = usize::from(retained);
        if self.screenshots[index].is_none() {
            let path = self.source.root.parent().unwrap().join(if retained {
                "retained.png"
            } else {
                "ready.png"
            });
            let fresh = matches!(fs::symlink_metadata(&path), Err(error) if error.kind() == std::io::ErrorKind::NotFound);
            self.record(fresh, "status screenshot destination was not fresh");
            self.screenshots[index] = Some(Screenshot {
                path: path.clone(),
                dimensions: None,
                failed: !fresh,
            });
            if fresh {
                renderer.screenshot_result = None;
                renderer.screenshot = Some(path);
            }
            return false;
        }
        let shot = self.screenshots[index].as_ref().unwrap();
        if shot.dimensions.is_some() {
            return true;
        }
        if shot.failed {
            return false;
        }
        let path = shot.path.clone();
        let Some(result) = renderer.screenshot_result.take() else {
            return false;
        };
        match result {
            Ok(dimensions) => {
                // Check the actual PNG header, including the renderer-reported
                // dimensions. The renderer owns readback and PNG encoding.
                let valid = fs::symlink_metadata(&path)
                    .is_ok_and(|m| m.is_file() && !m.file_type().is_symlink() && m.len() > 24)
                    && fs::read(&path).is_ok_and(|bytes| {
                        bytes.len() > 24
                            && &bytes[..8] == b"\x89PNG\r\n\x1a\n"
                            && bytes[12..16] == *b"IHDR"
                            && dimensions[0] > 0
                            && dimensions[1] > 0
                            && u32::from_be_bytes(bytes[16..20].try_into().unwrap())
                                == dimensions[0]
                            && u32::from_be_bytes(bytes[20..24].try_into().unwrap())
                                == dimensions[1]
                    });
                self.record(
                    valid,
                    "status screenshot result lacked its actual nonempty PNG/dimensions",
                );
                let shot = self.screenshots[index].as_mut().unwrap();
                if valid {
                    shot.dimensions = Some(dimensions);
                } else {
                    shot.failed = true;
                }
                valid
            }
            Err(_) => {
                self.record(
                    false,
                    "actual status screenshot readback or PNG write failed",
                );
                self.screenshots[index].as_mut().unwrap().failed = true;
                false
            }
        }
    }
    fn marker(&self, phase: &str, control: Option<&ManagedStopControl>) {
        let view = control.map(ManagedStopControl::view);
        let connected = view.as_ref().and_then(|v| v.connected_observation.as_ref()).map(|observation| {
            serde_json::json!({
                "world_time":observation.world_time,"day_time":observation.day_time,
                "hub_ticks":observation.hub_ticks,"guests":observation.guests,
                "play_entries":observation.play_entries,"pending_transport_bytes":observation.pending_transport_bytes,
                "attempts":observation.attempts,"core_matches":source_core_matches(observation,&self.source),
                "live_keys_valid":observation.live_keys.is_ok(),
                "keys_match":self.keys.as_ref().is_some_and(|(counter,slot)|counter==&observation.counter_key&&slot==&observation.slot_key)
            })
        });
        eprintln!(
            "FROZEN_WINDOW_MARKER {}",
            serde_json::json!({
                "phase":phase,"pid":std::process::id(),"case":format!("{:?}",self.case),
                "signals":horizon_threads::stop_requests(),"turns":self.turns,"errors":&self.errors,
                "ready":self.ready,"retained":self.retained,"cleanup_blocked":self.cleanup_blocked,"ui_exercised":self.ui_exercised,
                "actual_drawn":self.actual_drawn,"actual_skipped":self.actual_skipped,
                "typed_render_injections":self.injected,"native_resize":self.resize_seen,
                "minimize_ack":self.minimize_ack,"last_native_lock_held":self.native_lock_held,
                "last_uuid_lease_held":self.uuid_lease_held,
                "state":view.as_ref().map(|v|format!("{:?}",v.state)),
                "last_admitted":view.as_ref().map(|v|v.last_admitted),"connected":connected
            })
        );
        let _ = std::io::stderr().flush();
    }
}
struct Report {
    probe: Probe,
    receipt: Option<CheckpointReceipt>,
    finished: bool,
    offers: usize,
    frames: u64,
    last_admitted: u64,
    consumed_once: bool,
    error: String,
}
thread_local! {
    static PENDING: RefCell<Option<Probe>> = const { RefCell::new(None) };
    static REPORT: RefCell<Option<Report>> = const { RefCell::new(None) };
}
pub(super) fn attach(app: &mut FrozenApplication<'_>) -> bool {
    let probe = PENDING.with(|slot| slot.borrow_mut().take());
    let Some(probe) = probe else {
        return false;
    };
    app.start
        .as_mut()
        .expect("unstarted production application")
        .first_stop_fault = probe.case.fault();
    app.probe = Some(probe);
    true
}
pub(super) fn event_loop() -> Result<EventLoop<()>> {
    let mut builder = EventLoop::builder();
    builder.with_any_thread(true);
    Ok(builder.build()?)
}
pub(super) fn complete(app: &mut FrozenApplication<'_>) {
    let Some(mut probe) = app.probe.take() else {
        return;
    };
    // A native Retry can finish before another probe turn. Observe the real
    // admitted command after join as well; this never submits test input.
    if probe.case.native_retry()
        && app
            .control
            .as_ref()
            .is_some_and(|control| control.view().last_admitted == 2)
    {
        probe.retry = true;
    }
    let receipt = app
        .completion
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .cloned();
    let consumed_once = app
        .worker
        .as_mut()
        .is_some_and(|worker| worker.take_completion().is_none());
    let report = Report {
        probe,
        receipt,
        finished: app.finished,
        offers: app.offers,
        frames: app.frames,
        last_admitted: app.control.as_ref().map_or(0, |c| c.view().last_admitted),
        consumed_once,
        error: app.error.clone(),
    };
    REPORT.with(|slot| {
        assert!(
            slot.borrow_mut().replace(report).is_none(),
            "one application report"
        )
    });
}

impl FrozenApplication<'_> {
    /// The fixed signal schedule holds only automatic close admission. Native
    /// owner polling, both client pumps and drawing still run on every turn.
    pub(super) fn probe_holds_close(&self) -> bool {
        self.probe.as_ref().is_some_and(|probe| {
            probe.case == Case::SignalsReady && probe.stage == 3 && probe.signals_observed < 2
        })
    }
    pub(super) fn probe_render_result(&mut self, actual: Result<bool>) -> Result<bool> {
        let Some(probe) = self.probe.as_mut() else {
            return actual;
        };
        match &actual {
            Ok(true) => probe.actual_drawn += 1,
            Ok(false) => probe.actual_skipped += 1,
            Err(_) => probe.record(false, "unexpected native renderer error"),
        }
        // The renderer above really ran. This typed result seam proves owner
        // containment, and makes no claim of inducing a driver/surface fault.
        let ready = self
            .snapshot
            .as_ref()
            .is_some_and(|v| matches!(v.state, State::Ready));
        if probe.injected == 0 && actual.is_ok() {
            let error = (probe.case == Case::RenderBefore && !ready)
                || (probe.case == Case::RenderAfter && ready && probe.ready >= 3);
            if error {
                probe.injected += 1;
                return Err("frozen qualification renderer-result error".into());
            }
            if probe.case == Case::SkipFrame && ready {
                probe.injected += 1;
                return Ok(false);
            }
        }
        actual
    }
    pub(super) fn probe_turn(&mut self, event_loop: &ActiveEventLoop) {
        // Detach typed test data during native event dispatch, so recursive
        // production turns cannot recursively run this schedule.
        let Some(mut probe) = self.probe.take() else {
            return;
        };
        probe.turns += 1;
        if probe.began.elapsed() > Duration::from_secs(400) {
            probe.marker("DEADLINE_UNRESOLVED", self.control.as_ref());
            panic!("frozen application qualification deadline; unresolved owner is fatal");
        }
        let Some(window) = self.window.clone() else {
            self.probe = Some(probe);
            return;
        };
        let id = window.id();
        let snapshot = self.control.as_ref().map(|c| c.view());
        let ready = snapshot
            .as_ref()
            .is_some_and(|v| matches!(v.state, State::Ready));
        if probe.case == Case::SignalsReady && probe.stage == 3 {
            observe_signals(&mut probe, self.control.as_ref().unwrap(), false);
        }
        if !ready && !probe.startup_close && probe.case == Case::StartupClose {
            probe.startup_close = true;
            self.window_event(event_loop, id, WindowEvent::CloseRequested);
            probe.record(self.want_exit, "startup close did not record intent");
            probe.record(
                self.control
                    .as_ref()
                    .is_some_and(|c| c.view().last_admitted == 0),
                "startup close admitted before Ready",
            );
        }
        if probe.case == Case::PreReadyDeparture && self.offers == 1 && !probe.departed && !ready {
            probe.departed = true;
            disconnect(&mut self.clients[1]);
            probe.marker("PRE_READY_DEPARTURE", self.control.as_ref());
        }
        if ready && probe.stage != 4 {
            let clients_match =
                self.clients
                    .iter()
                    .zip(["MemoryOwner", "MemoryGuest"])
                    .all(|(client, name)| {
                        probe
                            .source
                            .rows
                            .iter()
                            .find(|row| row.profile_uuid == offline_uuid(name))
                            .is_some_and(|row| client_matches(client, row))
                    })
                    && inventory_count(&self.clients[0]) == 3
                    && inventory_count(&self.clients[1]) == 5;
            if clients_match {
                probe.ready += 1;
                if let Some(observation) = snapshot
                    .as_ref()
                    .and_then(|v| v.connected_observation.as_ref())
                {
                    probe.record(
                        source_core_matches(observation, &probe.source),
                        "Ready native core/rows/clocks changed",
                    );
                    let keys = (
                        observation.counter_key.clone(),
                        observation.slot_key.clone(),
                    );
                    probe.record(
                        probe.keys.as_ref().is_none_or(|old| old == &keys),
                        "Ready runtime keys changed",
                    );
                    probe.keys.get_or_insert(keys);
                } else {
                    probe.record(false, "Ready lacks real connected observation");
                }
                let destination = probe.source.root.parent().unwrap().join("recovered");
                let held = SessionLock::acquire(&destination).is_err();
                probe.native_lock_held = Some(held);
                probe.record(held, "native lock released while Ready");
                let blocked = probe.source.root.parent().unwrap().join("blocked-copy");
                let held = probe
                    .source
                    .namespace
                    .acquire_recovery(&probe.source.root, &probe.source.archive, &blocked)
                    .is_err();
                probe.uuid_lease_held = Some(held);
                probe.record(held, "UUID lease released while Ready");
            }
            let ready_image = !probe.case.screenshots()
                || (clients_match
                    && self
                        .snapshot
                        .as_ref()
                        .is_some_and(|v| matches!(v.state, State::Ready))
                    && self
                        .renderer
                        .as_mut()
                        .is_some_and(|renderer| probe.capture(renderer, false)));
            if probe.ready >= 3 && probe.actual_drawn >= 2 && ready_image && !probe.ui_exercised {
                probe.ui_exercised = true;
                probe.stage = 1;
                self.view = FrozenView::default();
                self.pointer = None;
                for key in [
                    KeyCode::KeyW,
                    KeyCode::KeyE,
                    KeyCode::KeyT,
                    KeyCode::F3,
                    KeyCode::Space,
                    KeyCode::Digit1,
                ] {
                    self.physical_key(PhysicalKey::Code(key));
                }
                self.window_event(
                    event_loop,
                    id,
                    WindowEvent::Ime(winit::event::Ime::Commit(
                        "/say ignored frozen input".into(),
                    )),
                );
                probe.record(
                    self.clients.iter().all(|c| c.physical_keys.is_empty()),
                    "gameplay key reached ordinary Game input",
                );
                probe.record(
                    self.control
                        .as_ref()
                        .is_some_and(|c| c.view().last_admitted == 0),
                    "ignored input admitted close",
                );
                let _ = window.request_inner_size(PhysicalSize::new(720, 480));
                window.set_minimized(true);
                probe.minimize_ack = window.is_minimized();
                self.window_event(event_loop, id, WindowEvent::Occluded(true));
                self.window_event(
                    event_loop,
                    id,
                    WindowEvent::Resized(PhysicalSize::new(0, 0)),
                );
                probe.marker("OCCLUDED_SUPERVISION", self.control.as_ref());
            } else if probe.stage == 1 && probe.ready >= 8 {
                probe.stage = 2;
                window.set_minimized(false);
                self.window_event(event_loop, id, WindowEvent::Occluded(false));
                self.window_event(
                    event_loop,
                    id,
                    WindowEvent::Resized(PhysicalSize::new(720, 480)),
                );
                self.window_event(event_loop, id, WindowEvent::Focused(false));
                self.view = FrozenView::default();
            } else if probe.stage == 2 && probe.ready >= 12 && self.frames >= 3 {
                probe.resize_seen = window.inner_size() == PhysicalSize::new(720, 480);
                probe.stage = 3;
                match probe.case {
                    Case::FatalUnwind => {
                        probe.marker("FATAL_UNWIND_READY", self.control.as_ref());
                        panic!("deliberate live frozen application unwind");
                    }
                    Case::FatalExit => {
                        probe.marker("FATAL_EXIT_READY", self.control.as_ref());
                        event_loop.exit();
                    }
                    Case::LateDeparture => {
                        disconnect(&mut self.clients[1]);
                        probe.departed = true;
                        probe.marker("LATE_DEPARTURE", self.control.as_ref());
                    }
                    Case::SignalsReady => {
                        probe.marker("WAIT_SIGNALS_READY", self.control.as_ref());
                    }
                    case if case.native() => {
                        probe.marker("WAIT_NATIVE_CLOSE", self.control.as_ref());
                    }
                    Case::PreReadyDeparture => {
                        probe.record(false, "pre-Ready departure unexpectedly reached Ready")
                    }
                    _ => self.window_event(event_loop, id, WindowEvent::CloseRequested),
                }
            }
        }
        if let Some(view) = snapshot.as_ref() {
            if probe.case.native_retry() && view.last_admitted == 2 {
                probe.retry = true;
            }
            if let State::RetainedFailure {
                attempt,
                publication,
                ..
            } = &view.state
                && !(probe.case.native_retry() && probe.retry)
            {
                probe.retained += 1;
                let retained_image = !probe.case.screenshots()
                    || (self
                        .snapshot
                        .as_ref()
                        .is_some_and(|v| matches!(v.state, State::RetainedFailure { .. }))
                        && self
                            .renderer
                            .as_mut()
                            .is_some_and(|renderer| probe.capture(renderer, true)));
                probe.record(
                    *attempt == 1 && view.last_admitted == 1,
                    "retained attempt was retried automatically",
                );
                probe.record(
                    matches!(
                        (probe.case.fault(), publication),
                        (Some(CheckpointTestFault::Copied), Publication::NotPublished)
                            | (Some(CheckpointTestFault::Promoted), Publication::Uncertain)
                    ),
                    "retained publication classification mismatch",
                );
                if let Some(observation) = &view.connected_observation {
                    probe.record(
                        source_core_matches(observation, &probe.source),
                        "retained native core/rows/clocks changed",
                    );
                    let keys = (
                        observation.counter_key.clone(),
                        observation.slot_key.clone(),
                    );
                    probe.record(
                        probe.keys.as_ref() == Some(&keys),
                        "retained runtime keys changed",
                    );
                }
                let root = probe.source.root.parent().unwrap().to_owned();
                let held = SessionLock::acquire(&root.join("recovered")).is_err();
                probe.native_lock_held = Some(held);
                probe.record(held, "retained failure released native lock");
                let held = probe
                    .source
                    .namespace
                    .acquire_recovery(
                        &probe.source.root,
                        &probe.source.archive,
                        &root.join("retained-blocked-copy"),
                    )
                    .is_err();
                probe.uuid_lease_held = Some(held);
                probe.record(held, "retained failure released UUID lease");
                if probe.retained == 1 {
                    self.view = FrozenView::default();
                    self.window_event(event_loop, id, WindowEvent::CloseRequested);
                    self.physical_key(PhysicalKey::Code(KeyCode::Escape));
                    self.physical_key(PhysicalKey::Code(KeyCode::Enter));
                    probe.record(
                        self.control.as_ref().unwrap().view().last_admitted == 1,
                        "repeated Close retried retained failure",
                    );
                    probe.marker("RETAINED_FAILURE", self.control.as_ref());
                    if probe.case.signal() {
                        probe.marker("WAIT_SIGNALS_RETAINED", self.control.as_ref());
                    }
                }
                if probe.case.signal() {
                    observe_signals(&mut probe, self.control.as_ref().unwrap(), true);
                }
                if probe.retained >= 10
                    && retained_image
                    && !probe.retry
                    && (!probe.case.signal() || probe.signals_observed >= 2)
                {
                    if probe.case.native_retry() {
                        if probe.stage != 5 {
                            probe.stage = 5;
                            probe.marker("WAIT_NATIVE_RETRY", self.control.as_ref());
                        }
                    } else {
                        probe.retry = true;
                        self.physical_key(PhysicalKey::Code(KeyCode::KeyR));
                        probe.record(
                            self.control.as_ref().unwrap().view().last_admitted == 2,
                            "explicit native retry was not admitted once",
                        );
                    }
                }
            }
            if matches!(view.state, State::Connected(ConnectedPhase::CleanupBlocked)) {
                if probe.case != Case::LateDeparture {
                    probe.record(false, "unexpected unresolved CleanupBlocked");
                }
                if probe.cleanup_blocked < 10 {
                    probe.cleanup_blocked += 1;
                    let before = view.last_admitted;
                    self.window_event(event_loop, id, WindowEvent::CloseRequested);
                    self.physical_key(PhysicalKey::Code(KeyCode::KeyR));
                    let current = self.control.as_ref().unwrap().view();
                    probe.record(
                        matches!(
                            current.state,
                            State::Connected(ConnectedPhase::CleanupBlocked)
                        ) && current.last_admitted == before
                            && before == 0,
                        "CleanupBlocked admitted Close or Retry",
                    );
                    let root = probe.source.root.parent().unwrap().to_owned();
                    let held = SessionLock::acquire(&root.join("recovered")).is_err();
                    probe.native_lock_held = Some(held);
                    probe.record(held, "CleanupBlocked released native lock");
                    let held = probe
                        .source
                        .namespace
                        .acquire_recovery(
                            &probe.source.root,
                            &probe.source.archive,
                            &root.join("departure-blocked-copy"),
                        )
                        .is_err();
                    probe.uuid_lease_held = Some(held);
                    probe.record(held, "CleanupBlocked released UUID lease");
                    if let Some(observation) = &view.connected_observation {
                        probe.record(
                            source_core_matches(observation, &probe.source),
                            "CleanupBlocked changed native core or player rows",
                        );
                        probe.record(
                            observation.guests == 2 && observation.play_entries == 2,
                            "CleanupBlocked released retained native players",
                        );
                        let keys = (
                            observation.counter_key.clone(),
                            observation.slot_key.clone(),
                        );
                        probe.record(
                            probe.keys.as_ref() == Some(&keys),
                            "CleanupBlocked changed native runtime keys",
                        );
                        probe.record(
                            observation.world_time == probe.source.world_time
                                && observation.day_time == probe.source.day_time
                                && observation.hub_ticks == 0,
                            "CleanupBlocked advanced clocks",
                        );
                    } else {
                        probe.record(false, "CleanupBlocked lacks actual observation");
                    }
                    if probe.cleanup_blocked == 10 {
                        probe.stage = 4;
                        probe.marker("CLEANUP_BLOCKED_RETAINED", self.control.as_ref());
                    }
                }
            }
        }
        self.probe = Some(probe);
    }
}

fn observe_signals(probe: &mut Probe, control: &ManagedStopControl, retained: bool) {
    // One observation per actual application turn. The coordinator sends the
    // next OS signal only after the previous count's flushed marker.
    let count = horizon_threads::stop_requests();
    if count > probe.signals_observed {
        // External coordinator sends the next signal only after this marker.
        probe.record(
            count == probe.signals_observed + 1,
            "signals were not separately observed",
        );
        let view = control.view();
        probe.record(
            if retained {
                matches!(view.state, State::RetainedFailure { attempt: 1, .. })
                    && view.last_admitted == 1
            } else {
                matches!(view.state, State::Ready) && view.last_admitted == 0
            },
            "signal changed native admission without explicit UI intent",
        );
        probe.signals_observed = count;
        probe.marker(
            if count == 1 {
                "SIGNAL_ONE_OBSERVED"
            } else {
                "SIGNAL_TWO_OBSERVED"
            },
            Some(control),
        );
    }
}

fn arguments(root: &Path) -> Vec<String> {
    let resources = std::env::var_os("HORIZON_TEST_RESOURCES").expect("pinned full resources");
    let package = std::env::var_os("HORIZON_COUNTER_PAIR_FIXTURE").expect("pinned prepared pair");
    [
        "--managed-frozen-recovery".into(),
        root.to_str().unwrap().into(),
        "--resources".into(),
        resources.to_str().unwrap().into(),
        "--dev-mod".into(),
        package.to_str().unwrap().into(),
        "--name".into(),
        "MemoryOwner".into(),
        "--width".into(),
        "800".into(),
        "--height".into(),
        "600".into(),
    ]
    .into()
}

#[test]
#[ignore = "coordinator isolated one-case process, real Linux window/resources/source and optional external signals"]
fn managed_frozen_actual_production_window_child() {
    let name = std::env::var("HORIZON_FROZEN_WINDOW_CASE").expect("explicit bounded window case");
    let case = Case::named(&name);
    let scratch = Scratch::new(&format!("frozen-{name}"));
    let fixture = Fixture::new("frozen-window-source");
    let captured = source(&fixture, &scratch);
    let before = inventory_tree(&captured.archive);
    let root = captured.root.parent().unwrap().to_owned();
    let continuation = scratch.dir("continuation");
    let parsed = Options::read_from(arguments(&root))
        .unwrap()
        .expect("production parser");
    let launch = parsed.frozen.expect("explicit frozen launch");
    PENDING.with(|slot| {
        assert!(
            slot.borrow_mut()
                .replace(Probe::new(captured, case))
                .is_none()
        )
    });
    let result = crate::game::run_managed_frozen(parsed.options, launch);
    let report = REPORT
        .with(|slot| slot.borrow_mut().take())
        .expect("production run returned a joined probe report");
    let probe = report.probe;
    assert!(
        report.finished && report.consumed_once,
        "explicit join/one-shot classification required: result={result:?}, app_error={}, offers={}, finished={}",
        report.error,
        report.offers,
        report.finished
    );
    assert!(probe.errors.is_empty(), "{:?}", probe.errors);
    assert_eq!(report.offers, 1);
    assert_eq!(inventory_tree(&probe.source.archive), before);
    if case == Case::PreReadyDeparture {
        assert!(probe.departed && result.is_err() && report.receipt.is_none());
    } else {
        result.unwrap();
        let receipt = report.receipt.expect("genuine joined checkpoint receipt");
        let expected = if case.fault().is_some() { 2 } else { 1 };
        assert_eq!(
            report.last_admitted,
            if case.fault().is_some() { 2 } else { 1 }
        );
        if case == Case::StartupClose {
            assert!(probe.startup_close);
        } else {
            assert!(probe.ready >= 12 && probe.ui_exercised && probe.keys.is_some());
            assert!(probe.actual_drawn >= 2 && report.frames >= 3);
            assert!(probe.resize_seen, "native resize was not observed");
        }
        if case.fault().is_some() {
            assert!(probe.retained >= 10 && probe.retry);
        }
        if case.screenshots() {
            assert!(
                probe.screenshots[0]
                    .as_ref()
                    .is_some_and(|shot| shot.dimensions.is_some()),
                "Ready status screenshot required"
            );
            if case.fault().is_some() {
                assert!(
                    probe.screenshots[1]
                        .as_ref()
                        .is_some_and(|shot| shot.dimensions.is_some()),
                    "RetainedFailure status screenshot required"
                );
            }
        }
        if case.signal() {
            assert_eq!(probe.signals_observed, 2);
        }
        if matches!(
            case,
            Case::RenderBefore | Case::RenderAfter | Case::SkipFrame
        ) {
            assert_eq!(probe.injected, 1);
        }
        if matches!(case, Case::RenderBefore | Case::RenderAfter) {
            assert!(!report.error.is_empty());
        }
        drop(SessionLock::acquire(&root.join("recovered")).unwrap());
        drop(
            probe
                .source
                .namespace
                .acquire_recovery(
                    &probe.source.root,
                    &probe.source.archive,
                    &root.join("after-join"),
                )
                .unwrap(),
        );
        validate_continuation(
            &fixture,
            &probe.source,
            &scratch,
            &receipt,
            &continuation,
            expected,
        );
    }
    eprintln!(
        "FROZEN_WINDOW_RESULT {}",
        serde_json::json!({"case":name,"joined":report.finished,
        "offers":report.offers,"ready":probe.ready,"retained":probe.retained,"frames":report.frames,
        "actual_drawn":probe.actual_drawn,"actual_skipped":probe.actual_skipped,"typed_render_injections":probe.injected,
        "ready_screenshot":probe.screenshots[0].as_ref().map(|shot|serde_json::json!({"path":&shot.path,"dimensions":shot.dimensions})),
        "retained_screenshot":probe.screenshots[1].as_ref().map(|shot|serde_json::json!({"path":&shot.path,"dimensions":shot.dimensions})),
        "minimize_ack":probe.minimize_ack,"native_resize":probe.resize_seen,"signals":probe.signals_observed,
        "player_counts":[3,5],"source":probe.source.root,"archive":probe.source.archive,"continuation":continuation})
    );
}

#[test]
fn managed_frozen_tokens_refuse_session_and_incompatible_modes_before_paths() {
    let base: Vec<String> = [
        "--managed-frozen-recovery",
        "/not-opened",
        "--resources",
        "/not-opened-r",
        "--dev-mod",
        "/not-opened-p",
    ]
    .map(str::to_owned)
    .into();
    assert!(checked_tokens(&base, false, false).unwrap().is_some());
    assert!(checked_tokens(&base, true, false).is_err());
    assert!(checked_tokens(&base, false, true).is_err());
    for flag in [
        "--session-file",
        "--session-file=",
        "--session-file=secret",
        "--world",
        "--server",
        "--dedicated",
        "--in-frame",
        "--offscreen",
        "--frames",
        "--world-frames",
        "--screenshot-after",
        "--report",
        "--control",
        "--benchmark",
        "--creative",
        "--time",
        "--weather",
        "--dimension",
        "--look",
        "--pregen",
        "--quickPlaySingleplayer",
        "--quickPlayMultiplayer",
        "--net-report",
        "--name=MemoryOwner",
    ] {
        let mut args = base.clone();
        args.extend([flag.into(), "value".into()]);
        assert!(
            checked_tokens(&args, false, false).is_err(),
            "accepted {flag}"
        );
    }
    for tail in [
        vec!["--name", "Other"],
        vec!["--resources", "/duplicate"],
        vec!["--width", "0"],
        vec!["--height", "3841"],
        vec!["--width", ""],
        vec!["--width"],
    ] {
        let mut args = base.clone();
        args.extend(tail.into_iter().map(str::to_owned));
        assert!(checked_tokens(&args, false, false).is_err());
    }
    assert!(checked_tokens(&["--managed-frozen-recovery=/x".into()], false, false).is_err());
    assert!(
        checked_tokens(&["--world".into(), "ordinary".into()], true, true)
            .unwrap()
            .is_none()
    );
}

#[test]
#[ignore = "coordinator child provides session flag/env sentinel, including empty or non-Unicode env"]
fn managed_frozen_production_session_refusal_child() {
    let sentinel = PathBuf::from(
        std::env::var_os("HORIZON_FROZEN_SESSION_SENTINEL").expect("isolated sentinel"),
    );
    let before = fs::read(&sentinel).unwrap();
    let mut args = vec![
        "--managed-frozen-recovery".into(),
        "/deliberately-not-resolved".into(),
    ];
    match std::env::var("HORIZON_FROZEN_SESSION_CASE")
        .unwrap()
        .as_str()
    {
        "flag" => args.extend(["--session-file".into(), sentinel.to_str().unwrap().into()]),
        "equals" => args.push(format!("--session-file={}", sentinel.display())),
        "malformed" => args.push("--session-file=".into()),
        "env" => assert!(std::env::var_os("HORIZON_SESSION_FILE").is_some()),
        _ => panic!("explicit session negative case"),
    }
    let error = Options::read_from(args)
        .err()
        .expect("actual common parser must refuse session before paths")
        .to_string();
    assert!(error.contains("without account configuration"));
    assert!(!error.contains(&sentinel.to_string_lossy().to_string()));
    assert_eq!(
        fs::read(&sentinel).unwrap(),
        before,
        "session sentinel must remain present/unchanged"
    );
    assert!(REPORT.with(|slot| slot.borrow().is_none()));
}

#[test]
#[ignore = "coordinator child supplies isolated layout and selects invalid cwd, isolation, paths or resource fallback"]
fn managed_frozen_production_preflight_refusal_child() {
    let scratch = Scratch::new("frozen-preflight");
    let root = scratch.dir("layout");
    for name in [
        "source-world",
        "source-archive",
        "authority",
        "continuation",
    ] {
        fs::create_dir(root.join(name)).unwrap();
    }
    let kind = std::env::var("HORIZON_FROZEN_NEGATIVE_CASE").expect("explicit preflight case");
    let mut args = arguments(&root);
    match kind.as_str() {
        "relative" => args[1] = "relative-layout".into(),
        "linked" => {
            let link = root.parent().unwrap().join("linked-layout");
            std::os::unix::fs::symlink(&root, &link).unwrap();
            args[1] = link.to_str().unwrap().into();
        }
        "existing-destination" => {
            fs::create_dir(root.join("recovered")).unwrap();
        }
        "nonempty-continuation" => {
            fs::write(root.join("continuation/sentinel"), b"not empty").unwrap();
        }
        "cwd" | "isolation" | "hook-env" | "profile-env" | "resource-fallback" => {}
        _ => panic!("unknown negative case"),
    }
    if kind == "resource-fallback" {
        let parsed = Options::read_from(args).unwrap().unwrap();
        let result = crate::game::run_managed_frozen(parsed.options, parsed.frozen.unwrap());
        let error = result
            .expect_err("broken explicit pack/resource selection must refuse")
            .to_string();
        assert!(
            error.contains("refuses resource fallback or incomplete synchronous loading"),
            "did not refuse at resource fallback gate: {error}"
        );
    } else {
        assert!(Options::read_from(args).is_err(), "{kind} accepted");
    }
    assert!(REPORT.with(|slot| slot.borrow().is_none()));
    assert!(!root.join("recovered").exists() || kind == "existing-destination");
}

#[test]
#[ignore = "fresh isolated coordinator child; two actual OS signals, expected legacy exit130"]
fn managed_frozen_legacy_second_signal_exit130_child() {
    assert_eq!(
        std::env::var("HORIZON_FROZEN_LEGACY_SIGNAL_CHILD").as_deref(),
        Ok("1")
    );
    horizon_threads::catch_stop_signals();
    eprintln!("FROZEN_LEGACY_WAIT_SIGNAL_ONE {}", std::process::id());
    let _ = std::io::stderr().flush();
    let until = Instant::now() + Duration::from_secs(45);
    while horizon_threads::stop_requests() == 0 {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(2));
    }
    eprintln!("FROZEN_LEGACY_SIGNAL_ONE_OBSERVED");
    let _ = std::io::stderr().flush();
    loop {
        assert!(
            Instant::now() < until,
            "legacy second signal did not terminate child"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}
