//! Concrete transport custody for the original Hub. Ordinary factories stay legacy.
use super::*;

pub struct HubTransport {
    kind: Transport,
}
enum Transport {
    Legacy(Lan),
    #[cfg(feature = "paired-packages")]
    Retained(Box<RetainedTransport>),
}
impl HubTransport {
    pub(super) fn new_legacy(lan: Lan) -> Self {
        Self {
            kind: Transport::Legacy(lan),
        }
    }
    pub fn legacy(&self) -> Option<&Lan> {
        match &self.kind {
            Transport::Legacy(lan) => Some(lan),
            #[cfg(feature = "paired-packages")]
            Transport::Retained(_) => None,
        }
    }
    pub fn legacy_mut(&mut self) -> Option<&mut Lan> {
        match &mut self.kind {
            Transport::Legacy(lan) => Some(lan),
            #[cfg(feature = "paired-packages")]
            Transport::Retained(_) => None,
        }
    }
    pub fn port(&self) -> u16 {
        match &self.kind {
            Transport::Legacy(lan) => lan.port,
            #[cfg(feature = "paired-packages")]
            Transport::Retained(owner) => owner.port,
        }
    }
    pub fn max_players(&self) -> usize {
        match &self.kind {
            Transport::Legacy(lan) => lan.shared.max_players(),
            #[cfg(feature = "paired-packages")]
            Transport::Retained(owner) => owner.max_players,
        }
    }
    pub fn online(&self) -> usize {
        match &self.kind {
            Transport::Legacy(lan) => lan.shared.online(),
            #[cfg(feature = "paired-packages")]
            Transport::Retained(owner) => owner
                .lan
                .as_ref()
                .map_or(0, super::super::RetainedLan::online),
        }
    }
    pub(super) fn motd(&self) -> String {
        match &self.kind {
            Transport::Legacy(lan) => lan.shared.motd(),
            #[cfg(feature = "paired-packages")]
            Transport::Retained(owner) => owner.motd.clone(),
        }
    }
    pub(super) fn favicon(&self) -> Option<Vec<u8>> {
        self.legacy()
            .and_then(|lan| lan.shared.config.favicon.clone())
    }
    pub(super) fn online_mode(&self) -> bool {
        self.legacy()
            .is_some_and(|lan| lan.shared.config.online.is_some())
    }
    pub(super) fn enforces_secure_chat(&self) -> bool {
        self.legacy()
            .is_some_and(|lan| lan.shared.config.enforces_secure_chat())
    }
    pub(super) fn is_retained(&self) -> bool {
        self.legacy().is_none()
    }
}

impl Hub {
    pub(super) fn retained_input(&mut self, packet: S) -> S {
        #[cfg(feature = "paired-packages")]
        if let Transport::Retained(owner) = &mut self.lan.kind {
            let process = packet.clone();
            owner.applying = Some(packet);
            return process;
        }
        packet
    }
    pub(super) fn finish_retained_input(&mut self) -> bool {
        if !self.retained_turn_open() {
            return false;
        }
        #[cfg(feature = "paired-packages")]
        if let Transport::Retained(owner) = &mut self.lan.kind {
            owner.applying = None;
        }
        true
    }
    /// This is also called between original inbox packets, not just once per tick.
    pub(super) fn retained_turn_open(&mut self) -> bool {
        #[cfg(feature = "paired-packages")]
        if let Transport::Retained(owner) = &mut self.lan.kind {
            if owner.phase != RetainedHubPhase::Open {
                return false;
            }
            let failure = owner.lan.as_ref().and_then(|lan| {
                lan.harvest();
                let observation = lan.observe();
                observation
                    .attempts
                    .iter()
                    .find_map(|attempt| {
                        attempt
                            .outbound
                            .as_ref()
                            .and_then(|out| out.failure.as_ref())
                            .map(|failure| {
                                format!("original outbound {}: {failure:?}", attempt.conn)
                            })
                    })
                    .or_else(|| {
                        observation.terminals.iter().find_map(|terminal| {
                            // A completed status query has no admitted UUID and is normal.
                            let status_return = terminal.outcome
                                == super::super::RetainedOutcome::Returned
                                && terminal.conn.is_some_and(|conn| {
                                    observation.attempts.iter().any(|attempt| {
                                        attempt.conn == conn && attempt.uuid.is_none()
                                    })
                                });
                            (!status_return)
                                .then(|| format!("original transport terminal: {terminal:?}"))
                        })
                    })
            });
            if let Some(failure) = failure {
                owner.fail(failure);
                return false;
            }
        }
        true
    }
}

#[cfg(feature = "paired-packages")]
use {
    super::super::{
        ClosedLan, RetainedCloseStatus, RetainedLan, RetainedObservation, RetainedOfflineConfig,
    },
    horizon_protocol::connection::Pipe,
    std::{collections::VecDeque, io, time::Duration},
};

#[cfg(feature = "paired-packages")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetainedHubPhase {
    Open,
    Frozen,
    Closing,
    Blocked,
    Ready,
}
#[cfg(feature = "paired-packages")]
#[derive(Clone, Debug)]
pub struct RetainedHubCloseView {
    pub phase: RetainedHubPhase,
    pub attempt: u64,
    pub failure: Option<String>,
    pub transport: RetainedObservation,
    pub guests: usize,
    pub inbox_packets: usize,
    pub pending_events: usize,
    pub applying_packet: bool,
}
#[cfg(feature = "paired-packages")]
struct RetainedTransport {
    lan: Option<RetainedLan>,
    closed: Option<ClosedLan>,
    pending: VecDeque<NetEvent>,
    applying: Option<S>,
    phase: RetainedHubPhase,
    attempt: u64,
    failure: Option<String>,
    port: u16,
    max_players: usize,
    motd: String,
}
#[cfg(feature = "paired-packages")]
impl RetainedTransport {
    fn fail(&mut self, failure: String) {
        self.failure.get_or_insert(failure);
        if self.phase == RetainedHubPhase::Open {
            self.phase = RetainedHubPhase::Frozen;
            if let Some(lan) = &self.lan {
                lan.fence();
            }
        }
    }
    fn observation(&self) -> RetainedObservation {
        if let Some(lan) = &self.lan {
            lan.observe()
        } else {
            self.closed
                .as_ref()
                .expect("original closed transport")
                .observation
                .clone()
        }
    }
}

/// Refusal returns all inert original construction inputs. No native actor exists.
#[cfg(feature = "paired-packages")]
pub struct RetainedHubRefusal {
    pub sync: Arc<SyncData>,
    pub encoder: ChunkEncoder,
    pub config: HubConfig,
    pub offline: RetainedOfflineConfig,
    pub reason: String,
}
#[cfg(feature = "paired-packages")]
pub struct ClosedHub {
    hub: Hub,
}
#[cfg(feature = "paired-packages")]
impl ClosedHub {
    pub fn view(&self) -> RetainedHubCloseView {
        self.hub.retained_close_view().expect("retained completion")
    }
}

#[cfg(feature = "paired-packages")]
impl Hub {
    pub fn new_retained(
        sync: Arc<SyncData>,
        encoder: ChunkEncoder,
        config: HubConfig,
        offline: RetainedOfflineConfig,
    ) -> Result<Self, Box<RetainedHubRefusal>> {
        let valid = config.max_players == offline.max_players
            && (2..=32).contains(&config.view_distance)
            && (2..=32).contains(&config.simulation_distance)
            && (0..=3).contains(&config.default_game_mode)
            && sync
                .id("minecraft:dimension_type", &config.dimension)
                .is_some();
        let lan = if valid {
            RetainedLan::new(sync.clone(), offline.clone()).map_err(|e| e.to_string())
        } else {
            Err("invalid retained Hub configuration".into())
        };
        let lan = match lan {
            Ok(lan) => lan,
            Err(reason) => {
                return Err(Box::new(RetainedHubRefusal {
                    sync,
                    encoder,
                    config,
                    offline,
                    reason,
                }));
            }
        };
        let transport = HubTransport {
            kind: Transport::Retained(Box::new(RetainedTransport {
                max_players: lan.max_players(),
                motd: lan.motd(),
                lan: Some(lan),
                closed: None,
                pending: VecDeque::new(),
                applying: None,
                phase: RetainedHubPhase::Open,
                attempt: 0,
                failure: None,
                port: 0,
            })),
        };
        Ok(Self::with_transport(transport, sync, encoder, config))
    }
    pub fn retained_listen(&mut self) -> io::Result<u16> {
        if !self.retained_turn_open() {
            return Err(io::Error::other("retained Hub fenced"));
        }
        let Transport::Retained(owner) = &mut self.lan.kind else {
            return Err(io::Error::other("not a retained Hub"));
        };
        match owner
            .lan
            .as_mut()
            .expect("active original transport")
            .listen()
        {
            Ok(port) => {
                owner.port = port;
                Ok(port)
            }
            Err(error) => {
                owner.fail(format!("original listener acquisition: {error}"));
                Err(error)
            }
        }
    }
    pub fn retained_connect_local(&mut self) -> io::Result<Pipe> {
        if !self.retained_turn_open() {
            return Err(io::Error::other("retained Hub fenced"));
        }
        let Transport::Retained(owner) = &mut self.lan.kind else {
            return Err(io::Error::other("not a retained Hub"));
        };
        match owner
            .lan
            .as_ref()
            .expect("active original transport")
            .connect_local()
        {
            Ok(pipe) => Ok(pipe),
            Err(error) => {
                owner.fail(format!("original memory acquisition: {error}"));
                Err(error)
            }
        }
    }
    /// At most sixteen native events per turn; failed/undispatched originals stay here.
    pub fn poll_retained(&mut self, ctx: &mut TickCtx, max: usize) -> Vec<HostEvent> {
        let mut output = Vec::new();
        if !self.lan.is_retained() || !self.retained_turn_open() {
            return output;
        }
        let Transport::Retained(owner) = &mut self.lan.kind else {
            unreachable!()
        };
        self.ticks = match self.ticks.checked_add(1) {
            Some(ticks) => ticks,
            None => {
                owner.fail("Hub tick counter exhausted".into());
                return output;
            }
        };
        if owner.pending.is_empty() {
            owner.pending.extend(
                owner
                    .lan
                    .as_ref()
                    .expect("active transport")
                    .poll_events(max.min(16)),
            );
        }
        for _ in 0..max.min(16) {
            if !self.retained_turn_open() {
                break;
            }
            let Transport::Retained(owner) = &mut self.lan.kind else {
                unreachable!()
            };
            let Some(event) = owner.pending.front() else {
                break;
            };
            let refusal = match event {
                NetEvent::Joined(joined)
                    if !owner
                        .lan
                        .as_ref()
                        .expect("original transport")
                        .validate_joined(joined)
                        || self
                            .guests
                            .iter()
                            .any(|g| g.conn == joined.conn || g.uuid == joined.uuid)
                        || *ctx.next_entity >= i32::MAX as u32
                        || !ctx.spawn.iter().all(|n| n.is_finite()) =>
                {
                    Some("retained Joined identity/capacity refused".to_owned())
                }
                NetEvent::Packet(conn, _) if !self.guests.iter().any(|g| g.conn == *conn) => {
                    Some("retained packet has no original Guest".to_owned())
                }
                NetEvent::Left(conn, reason) => {
                    Some(format!("retained peer {conn} left: {reason}"))
                }
                NetEvent::Duplicate(uuid) => Some(format!("retained duplicate profile {uuid}")),
                _ => None,
            };
            if let Some(reason) = refusal {
                owner.fail(reason);
                break;
            }
            let event = owner.pending.pop_front().expect("front retained");
            match event {
                NetEvent::Joined(joined) => {
                    let conn = self.admit(*joined, ctx);
                    output.push(HostEvent::Joined(conn));
                }
                NetEvent::Packet(conn, packet) => self
                    .guest_mut(conn)
                    .expect("checked original Guest")
                    .inbox
                    .push_back(*packet),
                NetEvent::Left(_, _) | NetEvent::Duplicate(_) => {
                    unreachable!("terminal retained above")
                }
            }
        }
        self.retained_turn_open();
        output
    }
    pub fn begin_retained_close(&mut self, timeout: Duration) -> bool {
        let Transport::Retained(owner) = &mut self.lan.kind else {
            return false;
        };
        if !matches!(
            owner.phase,
            RetainedHubPhase::Open | RetainedHubPhase::Frozen
        ) {
            return false;
        }
        owner.phase = RetainedHubPhase::Closing;
        owner.attempt = 1;
        owner
            .lan
            .as_mut()
            .expect("original transport")
            .begin_close(timeout.min(Duration::from_secs(120)));
        true
    }
    pub fn retry_retained_close(&mut self, expected: u64, timeout: Duration) -> bool {
        self.poll_retained_close();
        let Transport::Retained(owner) = &mut self.lan.kind else {
            return false;
        };
        if owner.phase != RetainedHubPhase::Blocked || owner.attempt != expected {
            return false;
        }
        let Some(next) = owner.attempt.checked_add(1) else {
            owner
                .failure
                .get_or_insert("close attempt exhausted".into());
            return false;
        };
        owner.attempt = next;
        owner.phase = RetainedHubPhase::Closing;
        owner
            .lan
            .as_mut()
            .expect("original transport")
            .retry_close(timeout.min(Duration::from_secs(120)));
        true
    }
    pub fn poll_retained_close(&mut self) -> Option<RetainedHubCloseView> {
        let Transport::Retained(owner) = &mut self.lan.kind else {
            return None;
        };
        if matches!(
            owner.phase,
            RetainedHubPhase::Closing | RetainedHubPhase::Blocked
        ) {
            owner.phase = match owner.lan.as_mut().expect("original transport").poll_close() {
                RetainedCloseStatus::Pending(_) => RetainedHubPhase::Closing,
                RetainedCloseStatus::Blocked(_) => RetainedHubPhase::Blocked,
                RetainedCloseStatus::Quiescent(_) => RetainedHubPhase::Ready,
            };
        }
        self.retained_close_view()
    }
    pub fn retained_close_view(&self) -> Option<RetainedHubCloseView> {
        let Transport::Retained(owner) = &self.lan.kind else {
            return None;
        };
        Some(RetainedHubCloseView {
            phase: owner.phase,
            attempt: owner.attempt,
            failure: owner.failure.clone(),
            transport: owner.observation(),
            guests: self.guests.len(),
            inbox_packets: self.guests.iter().map(|g| g.inbox.len()).sum(),
            pending_events: owner.pending.len(),
            applying_packet: owner.applying.is_some(),
        })
    }
    #[allow(clippy::result_large_err)]
    pub fn try_retained_closed(mut self) -> Result<ClosedHub, Self> {
        self.poll_retained_close();
        let Transport::Retained(owner) = &mut self.lan.kind else {
            return Err(self);
        };
        if owner.phase != RetainedHubPhase::Ready {
            return Err(self);
        }
        if owner.closed.is_none() {
            let Some(lan) = owner.lan.take() else {
                return Err(self);
            };
            match lan.try_closed() {
                Ok(closed) => owner.closed = Some(closed),
                Err(lan) => {
                    owner.lan = Some(lan);
                    return Err(self);
                }
            }
        }
        Ok(ClosedHub { hub: self })
    }
}

#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
#[path = "retained_tests.rs"]
mod tests;
