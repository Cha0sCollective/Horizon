//! Original remote client ownership, independent of native-world storage.
#[cfg(all(test, feature = "paired-packages"))]
#[path = "retained_remote_tests.rs"]
mod tests;
use super::remote_chunks::ChunkDecoder;
use horizon_client::net::Session;

pub(super) struct LegacyIo {
    pub(super) session: Session,
    pub(super) decoder: ChunkDecoder,
}

pub(super) enum RemoteIo {
    Legacy(Box<LegacyIo>),
    #[cfg(feature = "paired-packages")]
    Retained(Box<RetainedIo>),
}

impl RemoteIo {
    pub(super) fn legacy(session: Session) -> Self {
        Self::Legacy(Box::new(LegacyIo {
            session,
            decoder: ChunkDecoder::new(),
        }))
    }
    pub(super) fn existing(&self) -> &LegacyIo {
        match self {
            Self::Legacy(io) => io,
            #[cfg(feature = "paired-packages")]
            Self::Retained(_) => panic!("legacy-only connection operation"),
        }
    }
    pub(super) fn existing_mut(&mut self) -> &mut LegacyIo {
        match self {
            Self::Legacy(io) => io,
            #[cfg(feature = "paired-packages")]
            Self::Retained(_) => panic!("legacy-only connection operation"),
        }
    }
    pub(super) fn retained(&self) -> bool {
        match self {
            Self::Legacy(_) => false,
            #[cfg(feature = "paired-packages")]
            Self::Retained(_) => true,
        }
    }
    pub(super) fn address(&self) -> &str {
        match self {
            Self::Legacy(io) => &io.session.address,
            #[cfg(feature = "paired-packages")]
            Self::Retained(io) => &io.address,
        }
    }
    pub(super) fn name(&self) -> &str {
        match self {
            Self::Legacy(io) => &io.session.name,
            #[cfg(feature = "paired-packages")]
            Self::Retained(io) => &io.name,
        }
    }
    pub(super) fn send_play(&self, packet: horizon_protocol::packets::play::Serverbound) {
        match self {
            Self::Legacy(io) => io.session.send(packet),
            #[cfg(feature = "paired-packages")]
            Self::Retained(io) => io.send(SendIntent::Play(packet)),
        }
    }
    pub(super) fn send_pack_status(&self, id: u128, action: i32) {
        match self {
            Self::Legacy(io) => io.session.send_pack_status(id, action),
            #[cfg(feature = "paired-packages")]
            Self::Retained(io) => io.send(SendIntent::PackStatus(id, action)),
        }
    }
    pub(super) fn send_custom_click(&self, id: &str, payload: Option<horizon_world::nbt::Tag>) {
        match self {
            Self::Legacy(io) => io.session.send_custom_click(id, payload),
            #[cfg(feature = "paired-packages")]
            Self::Retained(io) => io.send(SendIntent::CustomClick(id.to_owned(), payload)),
        }
    }
    pub(super) fn network_counters(&self) -> Option<[i64; 3]> {
        match self {
            Self::Legacy(io) => io.session.counters.lock().unwrap().as_ref().map(|c| {
                let order = std::sync::atomic::Ordering::Relaxed;
                [
                    c.bytes_in.load(order),
                    c.sent.load(order),
                    c.received.load(order),
                ]
            }),
            #[cfg(feature = "paired-packages")]
            Self::Retained(io) => {
                let observation = io
                    .session
                    .as_ref()
                    .map(RetainedSession::observe)
                    .or_else(|| io.closed_session.as_ref().map(ClosedSession::observe))?;
                Some([
                    observation.bytes_in,
                    observation.packets_sent,
                    observation.packets_received,
                ])
            }
        }
    }
    #[cfg(feature = "paired-packages")]
    pub(super) fn fixed(&self) -> Option<&RetainedIo> {
        match self {
            Self::Retained(io) => Some(io),
            _ => None,
        }
    }
    #[cfg(feature = "paired-packages")]
    pub(super) fn fixed_mut(&mut self) -> Option<&mut RetainedIo> {
        match self {
            Self::Retained(io) => Some(io),
            _ => None,
        }
    }
}

impl super::Game<'_> {
    pub(super) fn is_retained_remote(&self) -> bool {
        self.remote.as_ref().is_some_and(|r| r.io.retained())
    }
    pub(super) fn client_column_ready(&self, x: i32, z: i32) -> bool {
        if let Ok(streaming) = self.streaming.as_existing() {
            return streaming.is_lit(x, z);
        }
        #[cfg(feature = "paired-packages")]
        return self
            .streaming
            .remote()
            .is_some_and(|s| s.received_ready(&self.world, (x, z)));
        #[cfg(not(feature = "paired-packages"))]
        false
    }
}

#[cfg(feature = "paired-packages")]
use {
    super::remote_chunks::retained::{ClosedDecoder, RetainedChunkDecoder, RetainedDecoded},
    horizon_client::net::{
        ClosedSession, Event, RetainedClose, RetainedSession, SendAdmission, SendRefusal,
    },
    horizon_protocol::packets::play::Serverbound,
    horizon_world::nbt::Tag,
    std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        time::Instant,
    },
};

#[cfg(feature = "paired-packages")]
pub(super) enum SendIntent {
    Play(Serverbound),
    PackStatus(u128, i32),
    CustomClick(String, Option<Tag>),
}

#[cfg(feature = "paired-packages")]
pub(super) struct RefusedSend {
    pub(super) first: Option<(SendRefusal, Vec<u8>)>,
    // These have never been encoded or offered to the native Session.
    pub(super) subsequent: VecDeque<SendIntent>,
}

#[cfg(feature = "paired-packages")]
pub(super) struct RetainedIo {
    pub(super) session: Option<RetainedSession>,
    pub(super) closed_session: Option<ClosedSession>,
    pub(super) decoder: Option<RetainedChunkDecoder>,
    pub(super) closed_decoder: Option<ClosedDecoder>,
    pub(super) decoded: Option<RetainedDecoded>,
    pub(super) refused: RefCell<Option<RefusedSend>>,
    pub(super) close_reason: RefCell<Option<String>>,
    leave_requested: Cell<bool>,
    pub(super) address: String,
    pub(super) name: String,
    pub(super) login: bool,
    pub(super) sealed: bool,
    pub(super) deadline: Option<Instant>,
    // Observation boundary, never a claim about native error causality.
    pre_stop_terminals: usize,
    deliberate_stop: bool,
    pub(super) pending: VecDeque<Event>,
    // The original accepted packet remains here while a data clone is applied
    // by legacy consuming handlers. Failure never substitutes a clone for it.
    pub(super) applying: Option<Box<horizon_protocol::packets::play::Clientbound>>,
    pub(super) waiting: bool,
    pub(super) refused_decode: Option<super::remote_chunks::retained::RefusedInput>,
    pending_job: Option<(horizon_client::streaming::RemoteInput, PendingWork)>,
    pub(super) retired_columns: VecDeque<std::sync::Arc<horizon_client::chunk::Chunk>>,
    pub(super) parent_attempt: u64,
    session_attempt: u64,
    decoder_attempt: u64,
    stream_attempt: u64,
}

#[cfg(feature = "paired-packages")]
impl RetainedIo {
    pub(super) fn new(session: RetainedSession, address: String, name: String) -> Self {
        Self {
            session: Some(session),
            closed_session: None,
            decoder: Some(RetainedChunkDecoder::prepare()),
            closed_decoder: None,
            decoded: None,
            refused: RefCell::new(None),
            close_reason: RefCell::new(None),
            leave_requested: Cell::new(false),
            address,
            name,
            login: false,
            sealed: false,
            deadline: None,
            pre_stop_terminals: 0,
            deliberate_stop: false,
            pending: VecDeque::new(),
            applying: None,
            waiting: false,
            pending_job: None,
            refused_decode: None,
            retired_columns: VecDeque::new(),
            parent_attempt: 0,
            session_attempt: 0,
            decoder_attempt: 0,
            stream_attempt: 0,
        }
    }
    pub(super) fn fail(&self, message: impl Into<String>) {
        self.close_reason
            .borrow_mut()
            .get_or_insert_with(|| message.into());
    }
    pub(super) fn failure(&self) -> Option<String> {
        self.close_reason
            .borrow()
            .clone()
            .or_else(|| {
                self.refused.borrow().as_ref().and_then(|r| match &r.first {
                    Some((reason, _)) => Some(format!("remote send refused: {reason:?}")),
                    None if self.leave_requested.get() => None,
                    None => Some("remote sends fenced before admission".into()),
                })
            })
            .or_else(|| self.session_terminal_failure())
    }
    fn session_terminals(&self) -> Vec<horizon_client::net::RetainedTerminal> {
        self.session
            .as_ref()
            .map(RetainedSession::observe)
            .or_else(|| self.closed_session.as_ref().map(ClosedSession::observe))
            .map_or_else(Vec::new, |o| o.terminals)
    }
    fn is_receive_diagnostic(
        &self,
        index: usize,
        terminal: &horizon_client::net::RetainedTerminal,
    ) -> bool {
        self.sealed
            && self.deliberate_stop
            && index >= self.pre_stop_terminals
            && terminal.actor == horizon_client::net::RetainedActor::Reader
            && matches!(
                terminal.outcome,
                horizon_client::net::RetainedOutcome::Receive(_)
            )
    }
    fn session_terminal_failure(&self) -> Option<String> {
        self.session_terminals()
            .iter()
            .enumerate()
            .find_map(|(index, terminal)| {
                (terminal.outcome != horizon_client::net::RetainedOutcome::Closed
                    && !self.is_receive_diagnostic(index, terminal))
                .then(|| format!("original Session terminal: {:?}", terminal.outcome))
            })
    }
    fn receive_diagnostics(&self) -> Vec<String> {
        self.session_terminals().iter().enumerate()
            .filter(|(index, terminal)| self.is_receive_diagnostic(*index, terminal))
            .map(|(_, terminal)| format!(
                "Reader receive first observed after deliberate stop request; cause unresolved: {:?}", terminal.outcome))
            .collect()
    }
    pub(super) fn send(&self, intent: SendIntent) {
        let mut refused = self.refused.borrow_mut();
        if let Some(first) = refused.as_mut() {
            first.subsequent.push_back(intent);
            return;
        }
        // Keep unattempted typed input intact after an earlier non-send failure.
        if self.sealed || self.leave_requested.get() || self.close_reason.borrow().is_some() {
            *refused = Some(RefusedSend {
                first: None,
                subsequent: VecDeque::from([intent]),
            });
            return;
        }
        let session = self
            .session
            .as_ref()
            .expect("open retained connection has its original Session");
        let outcome = match intent {
            SendIntent::Play(packet) => session.send(packet),
            SendIntent::PackStatus(id, action) => session.send_pack_status(id, action),
            SendIntent::CustomClick(id, payload) => session.send_custom_click(&id, payload),
        };
        if let SendAdmission::Refused { reason, body } = outcome {
            *refused = Some(RefusedSend {
                first: Some((reason, body)),
                subsequent: VecDeque::new(),
            });
        }
    }
    pub(super) fn begin_close(&mut self, deadline: Instant) {
        if self.deadline.is_none() {
            // Preserve queued original events before the native stop request.
            // A snapshot cannot establish when a racing receive failed.
            if let Some(session) = &self.session {
                self.pending.extend(session.poll_events(usize::MAX));
            }
            let pending_error = self.pending.iter().find_map(|event| match event {
                Event::Disconnected { reason, .. } => {
                    Some(format!("remote disconnected: {reason}"))
                }
                Event::DecodeError(reason) => Some(format!("remote decode failed: {reason}")),
                _ => None,
            });
            if let Some(error) = pending_error {
                self.fail(error);
            }
            let before = self.session_terminals();
            self.pre_stop_terminals = before.len();
            for terminal in before {
                if terminal.outcome != horizon_client::net::RetainedOutcome::Closed {
                    self.fail(format!("original Session terminal: {:?}", terminal.outcome));
                }
            }
            self.deliberate_stop = self.leave_requested.get() && self.failure().is_none();
        }
        self.sealed = true;
        if self.deadline.is_none() {
            self.parent_attempt = 1;
            self.session_attempt = 1;
            self.decoder_attempt = 1;
            self.stream_attempt = 1;
        }
        self.deadline.get_or_insert(deadline);
        let original = self.deadline.unwrap();
        if let Some(session) = &mut self.session {
            session.begin_close(original);
        }
        if let Some(decoder) = &mut self.decoder {
            decoder.begin_close(original);
        }
    }
    pub(super) fn start(&mut self) -> Result<(), String> {
        self.decoder
            .as_mut()
            .expect("original decoder")
            .start()
            .map_err(|e| format!("remote decoder start: {e:?}"))?;
        self.session
            .as_mut()
            .expect("original Session")
            .start()
            .map_err(|e| format!("remote Session start: {e:?}"))
    }
    pub(super) fn poll_close(&mut self) -> bool {
        if let Some(mut session) = self.session.take() {
            if session.poll_close() == RetainedClose::Quiescent {
                match session.try_closed() {
                    Ok(closed) => self.closed_session = Some(closed),
                    Err(original) => self.session = Some(original),
                }
            } else {
                self.session = Some(session);
            }
        }
        if let Some(decoder) = self.decoder.take() {
            match decoder.try_closed() {
                Ok(closed) => self.closed_decoder = Some(closed),
                Err(original) => self.decoder = Some(original),
            }
        }
        if let Some(error) = self.session_terminal_failure() {
            self.fail(error);
        }
        let decoder_failure = self
            .decoder
            .as_ref()
            .and_then(|d| d.observe())
            .or_else(|| self.closed_decoder.as_ref().and_then(|d| d.observe()))
            .and_then(|o| o.failure);
        if let Some(failure) = decoder_failure {
            self.fail(format!("original decoder terminal: {failure:?}"));
        }
        self.closed_session.is_some() && self.closed_decoder.is_some()
    }
}

#[cfg(feature = "paired-packages")]
pub(super) struct RemoteCloseView {
    pub(super) attempt: u64,
    pub(super) quiescent: bool,
    pub(super) blocked: bool,
    pub(super) detail: String,
}

#[cfg(feature = "paired-packages")]
#[derive(Clone, Copy)]
enum PendingWork {
    Mesh((i32, i32, i32)),
    Light((i32, i32)),
}

#[cfg(feature = "paired-packages")]
impl super::Game<'_> {
    fn retire_remote_dirty(&mut self, work: PendingWork) {
        match work {
            PendingWork::Light(at) => {
                self.world.dirty_light.remove(&at);
            }
            PendingWork::Mesh((x, sy, z)) => {
                let bits = self.world.dirty_sections.take_column(x, z) & !(1 << sy);
                for other in 0..horizon_client::chunk::SECTIONS as i32 {
                    if bits & (1 << other) != 0 {
                        self.world.dirty_sections.insert((x, other, z));
                    }
                }
            }
        }
    }
    /// Real received-world work only. No native generation, save, entity
    /// unloading or background column-disposal route is reachable here.
    pub(super) fn update_retained_remote(&mut self) -> horizon_client::Result<()> {
        use horizon_client::{
            chunk::SECTIONS,
            light::{LightChanges, apply_relighting},
            streaming::{RemoteOutput, RemoteRefusal},
        };
        self.poll_remote()?;
        if let Some(failure) = self.retained_remote_failure() {
            return Err(failure.into());
        }
        if self.remote.as_ref().unwrap().io.fixed().unwrap().sealed {
            return Ok(());
        }
        let Some(stream) = self.streaming.remote() else {
            return Err("missing original remote streamer".into());
        };
        if let Some(o) = stream.try_observe()
            && (o.accounting_fault || !o.failures.is_empty())
        {
            return Err(format!("remote native worker failed: {:?}", o.failures).into());
        }
        if stream.profile().is_none() {
            return Ok(());
        }
        for _ in 0..16 {
            let stream = self.streaming.remote_mut().unwrap();
            match stream.try_reconcile() {
                Ok(_) | Err(RemoteRefusal::Busy) => {}
                Err(reason) => return Err(format!("remote reconcile refused: {reason:?}").into()),
            }
            let Some(handoff) = stream.try_take(&self.world) else {
                break;
            };
            let Some(output) = handoff.output else {
                return Err("running remote output missing".into());
            };
            match output {
                RemoteOutput::Mesh { section, mesh, .. } => {
                    if handoff.current {
                        if let Some(renderer) = &mut self.renderer {
                            renderer.upload(section, &mesh);
                        } else {
                            self.pending_uploads.push((section, mesh));
                        }
                    } else if self.world.chunks.contains_key(&(section.0, section.2)) {
                        self.world.dirty_sections.insert(section);
                    }
                }
                RemoteOutput::Light {
                    position, sections, ..
                } => {
                    if handoff.current {
                        if let Some(column) = self.world.chunks.get_mut(&position) {
                            // Keep the exact previous allocation until explicit
                            // bounded client disposition, not a freeing thread.
                            let previous = column.clone();
                            let mut changes = LightChanges::default();
                            apply_relighting(
                                std::sync::Arc::make_mut(column),
                                sections,
                                &mut changes,
                            );
                            self.remote
                                .as_mut()
                                .unwrap()
                                .io
                                .fixed_mut()
                                .unwrap()
                                .retired_columns
                                .push_back(previous);
                            changes.dirty_sections(|section| {
                                self.world.dirty_sections.insert(section);
                            });
                        }
                    } else if self.world.chunks.contains_key(&position) {
                        self.world.dirty_light.insert(position);
                    }
                }
            }
            // handoff.input is intentionally disposed only after the actual
            // output/current decision above; all work receipts are native-owned.
        }
        let pending = self
            .remote
            .as_mut()
            .unwrap()
            .io
            .fixed_mut()
            .unwrap()
            .pending_job
            .take();
        if let Some((input, work)) = pending {
            match self.streaming.remote_mut().unwrap().try_submit(input) {
                Ok(_) => self.retire_remote_dirty(work),
                Err(refused) => {
                    let retry = matches!(refused.reason, RemoteRefusal::Busy | RemoteRefusal::Full);
                    self.remote
                        .as_mut()
                        .unwrap()
                        .io
                        .fixed_mut()
                        .unwrap()
                        .pending_job = Some((refused.input, work));
                    if !retry {
                        return Err(
                            format!("original remote job refused: {:?}", refused.reason).into()
                        );
                    }
                    return Ok(());
                }
            }
        }
        let center = self.player.as_ref().map_or((0, 0), |p| {
            (
                (p.position.x.floor() as i32) >> 4,
                (p.position.z.floor() as i32) >> 4,
            )
        });
        let distance = |x: i32, z: i32| {
            (i64::from(x) - i64::from(center.0)).pow(2)
                + (i64::from(z) - i64::from(center.1)).pow(2)
        };
        let mut work = self
            .world
            .dirty_light
            .iter()
            .map(|&at| PendingWork::Light(at))
            .collect::<Vec<_>>();
        for ((x, z), bits) in self.world.dirty_sections.columns() {
            for sy in 0..SECTIONS as i32 {
                if bits & (1 << sy) != 0 {
                    work.push(PendingWork::Mesh((x, sy, z)));
                }
            }
        }
        work.sort_by_key(|work| match *work {
            PendingWork::Light((x, z)) | PendingWork::Mesh((x, _, z)) => distance(x, z),
        });
        let mut admitted = 0;
        for work in work {
            if admitted == 16 {
                break;
            }
            let stream = self.streaming.remote().unwrap();
            let input = match work {
                PendingWork::Light(at) => stream.light_input(&self.world, at),
                PendingWork::Mesh(at) if stream.mesh_pending(at) => continue,
                PendingWork::Mesh(at) => stream.mesh_input(&self.world, at),
            };
            let input = match input {
                Ok(input) => input,
                // Keep edge dirtiness, but continue to renderable interior work.
                Err(RemoteRefusal::MissingColumn) => continue,
                Err(reason) => return Err(format!("remote input refused: {reason:?}").into()),
            };
            match self.streaming.remote_mut().unwrap().try_submit(input) {
                Ok(_) => {
                    self.retire_remote_dirty(work);
                    admitted += 1;
                }
                Err(refused) => {
                    let retry = matches!(refused.reason, RemoteRefusal::Busy | RemoteRefusal::Full);
                    self.remote
                        .as_mut()
                        .unwrap()
                        .io
                        .fixed_mut()
                        .unwrap()
                        .pending_job = Some((refused.input, work));
                    if !retry {
                        return Err(format!("remote job refused: {:?}", refused.reason).into());
                    }
                    break;
                }
            }
        }
        self.forget_retained_outside_view()?;
        // Deliberate bounded disposition of original received allocations.
        for _ in 0..4 {
            drop(
                self.remote
                    .as_mut()
                    .unwrap()
                    .io
                    .fixed_mut()
                    .unwrap()
                    .retired_columns
                    .pop_front(),
            );
        }
        Ok(())
    }
    pub(super) fn retained_remote_failure(&self) -> Option<String> {
        let io = self.remote.as_ref()?.io.fixed()?;
        io.failure()
            .or_else(|| {
                io.decoder
                    .as_ref()
                    .and_then(|d| d.observe())
                    .or_else(|| io.closed_decoder.as_ref().and_then(|d| d.observe()))
                    .and_then(|o| o.failure)
                    .map(|failure| format!("original decoder failure: {failure:?}"))
            })
            .or_else(|| {
                let observation = self.streaming.remote_observation()?;
                (observation.accounting_fault || !observation.failures.is_empty()).then(|| {
                    format!(
                        "original remote streaming failure: {:?}",
                        observation.failures
                    )
                })
            })
    }
    pub(super) fn retained_remote_request_close(&self, reason: impl Into<String>) {
        if let Some(io) = self.remote.as_ref().and_then(|r| r.io.fixed()) {
            io.fail(reason);
        }
    }
    pub(super) fn retained_remote_request_leave(&self) {
        if let Some(io) = self.remote.as_ref().and_then(|r| r.io.fixed()) {
            io.leave_requested.set(true);
        }
    }
    pub(super) fn retained_remote_leave_requested(&self) -> bool {
        self.remote
            .as_ref()
            .and_then(|r| r.io.fixed())
            .is_some_and(|io| io.leave_requested.get())
    }
    pub(super) fn start_retained_remote(&mut self) -> Result<(), String> {
        self.remote
            .as_mut()
            .and_then(|r| r.io.fixed_mut())
            .ok_or("not a retained client")?
            .start()
    }
    pub(super) fn begin_retained_remote_close(&mut self, deadline: Instant) {
        if let Some(io) = self.remote.as_mut().and_then(|r| r.io.fixed_mut()) {
            io.begin_close(deadline);
            self.streaming.begin_remote_close(io.deadline.unwrap());
        }
        self.capture(false);
        self.keys.clear();
        self.physical_keys.clear();
        self.left = false;
        self.right = false;
    }
    pub(super) fn poll_retained_remote_close(&mut self) -> RemoteCloseView {
        use super::remote_chunks::retained::CloseStatus;
        use horizon_client::streaming::RemoteCloseStatus;
        let io = self
            .remote
            .as_mut()
            .and_then(|r| r.io.fixed_mut())
            .expect("original retained client");
        let io_closed = io.poll_close();
        let stream_closed = self.streaming.poll_remote_close();
        let session_blocked = io
            .session
            .as_ref()
            .is_some_and(|s| s.observe().close == RetainedClose::Blocked);
        let decoder_blocked = io
            .decoder
            .as_mut()
            .is_some_and(|d| matches!(d.poll_close(), CloseStatus::Blocked { .. }));
        let stream_blocked = matches!(
            self.streaming.remote_close_status(),
            Some(RemoteCloseStatus::Blocked { .. })
        );
        // No census is invented when a component registry is busy. Actual
        // closed owners remain in this Game, including failed/queued payloads.
        RemoteCloseView {
            attempt: io.parent_attempt,
            quiescent: io_closed && stream_closed,
            blocked: session_blocked || decoder_blocked || stream_blocked,
            detail: format!(
                "Session closed={}; decoder closed={}; streaming closed={}; {}",
                io.closed_session.is_some(),
                io.closed_decoder.is_some(),
                stream_closed,
                io.receive_diagnostics().join("; ")
            ),
        }
    }
    pub(super) fn retry_retained_remote_close(&mut self, expected: u64, deadline: Instant) -> bool {
        let io = self
            .remote
            .as_mut()
            .and_then(|r| r.io.fixed_mut())
            .expect("original retained client");
        if expected != io.parent_attempt || deadline <= Instant::now() {
            return false;
        }
        let Some(next) = io.parent_attempt.checked_add(1) else {
            return false;
        };
        let mut accepted = false;
        // Each actual owner validates its own deadline/state/attempt. Refusals
        // neither join nor replace native actors; a partial Retry stays partial.
        if let Some(session) = &mut io.session
            && let Ok(attempt) = session.retry_close(io.session_attempt, deadline)
        {
            io.session_attempt = attempt;
            accepted = true;
        }
        if let Some(decoder) = &mut io.decoder
            && let Ok(attempt) = decoder.retry_close(io.decoder_attempt, deadline)
        {
            io.decoder_attempt = attempt;
            accepted = true;
        }
        if let Ok(attempt) = self
            .streaming
            .retry_remote_close(io.stream_attempt, deadline)
        {
            io.stream_attempt = attempt;
            accepted = true;
        }
        if accepted {
            io.parent_attempt = next;
        }
        accepted
    }
}
