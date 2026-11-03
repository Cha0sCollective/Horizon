//! One accepted connection: handshake, status ping, login (offline, or online
//! with encryption and the session service's verdict) and the
//! configuration phase (ServerStatusPacketListenerImpl,
//! ServerLoginPacketListenerImpl, ServerConfigurationPacketListenerImpl). A
//! player that completes configuration continues in the play state: the
//! reader forwards decoded packets to the game thread while a writer thread
//! frames, compresses and encrypts what the game queues, and answers the
//! keep-alive schedule on its own so a long frame never times a client out.
use super::{
    Joined, NetEvent, Out, Shared,
    data::{CORE_PACK, SyncData},
};
use horizon_protocol::{
    PROTOCOL_VERSION,
    connection::{ConnReader, ConnWriter, Connection},
    crypto::{offline_uuid, server_hash, uuid_string},
    packets::{
        Empty,
        configuration::{self as cfg, ClientInformation, KnownPack},
        handshake, login, play, status,
    },
    text::translatable,
};
use std::{
    cell::Cell,
    net::TcpStream,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicI32, AtomicI64, AtomicU32, AtomicUsize, Ordering},
        mpsc::{Receiver, RecvTimeoutError, Sender, channel},
    },
    time::{Duration, Instant},
};

#[cfg(feature = "paired-packages")]
use super::{RetainedContext, RetainedOutcome};
#[cfg(not(feature = "paired-packages"))]
struct RetainedContext;

/// ServerCommonPacketListenerImpl.KEEPALIVE_TIME and the disconnect timeout.
const KEEP_ALIVE_EVERY: Duration = Duration::from_secs(15);
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(30);
/// Packets queued for one client beyond this many bytes disconnect it.
const MAX_PENDING: usize = 96 << 20;
/// Vanilla's server-side disconnect during login is a JSON component.
fn login_disconnect(c: &mut Connection, reason: &horizon_world::nbt::Tag) {
    let json = component_json(reason);
    let _ =
        c.send(&login::Clientbound::Disconnect(login::Disconnect { reason_json: json }).encode());
}

/// A component as ComponentSerialization.CODEC writes it with JsonOps (and
/// Gson without HTML escaping): plain text collapses to a string, the
/// content fields come first, then `extra`, then the style.
pub fn component_json(tag: &horizon_world::nbt::Tag) -> String {
    component_value(tag).compact()
}
fn component_value(tag: &horizon_world::nbt::Tag) -> crate::server::json::Json {
    use crate::server::json::Json;
    use horizon_world::nbt::Tag;
    const ORDER: &[&str] = &[
        "type",
        "text",
        "translate",
        "fallback",
        "with",
        "score",
        "selector",
        "separator",
        "keybind",
        "nbt",
        "interpret",
        "block",
        "entity",
        "storage",
        "source",
        "object",
        "sprite",
        "atlas",
        "player",
        "hat",
        "extra",
        "color",
        "shadow_color",
        "bold",
        "italic",
        "underlined",
        "strikethrough",
        "obfuscated",
        "click_event",
        "hover_event",
        "insertion",
        "font",
    ];
    match tag {
        Tag::String(s) => Json::String(s.to_string_lossy()),
        Tag::List(items) => Json::Array(items.iter().map(component_value).collect()),
        Tag::Compound(map) => {
            if map.len() == 1
                && let Some(Tag::String(t)) = map.get(&horizon_world::nbt::JavaString::from("text"))
            {
                return Json::String(t.to_string_lossy());
            }
            let mut keys: Vec<String> = map.keys().map(|k| k.to_string_lossy()).collect();
            keys.sort_by_key(|k| ORDER.iter().position(|o| o == k).unwrap_or(ORDER.len()));
            Json::Object(
                keys.into_iter()
                    .map(|k| {
                        let v = &map[&horizon_world::nbt::JavaString::from(k.as_str())];
                        let value = match v {
                            Tag::Byte(b) => Json::Bool(*b != 0),
                            Tag::Int(i) => Json::Number(i.to_string()),
                            other => component_value(other),
                        };
                        (k, value)
                    })
                    .collect(),
            )
        }
        Tag::Byte(b) => Json::Bool(*b != 0),
        Tag::Int(i) => Json::Number(i.to_string()),
        Tag::Long(i) => Json::Number(i.to_string()),
        _ => Json::Null,
    }
}

/// The inverse of [`component_value`]: a JSON component (a server.properties
/// prompt) as ComponentSerialization.CODEC writes it with NbtOps. An array is
/// its first component with the rest appended (`createFromList`); a list of
/// components mixing plain strings and objects has the strings as `{text}`.
pub fn component_tag(v: &crate::server::json::Json) -> horizon_world::nbt::Tag {
    use crate::server::json::Json;
    use horizon_world::nbt::{JavaString, Tag};
    fn list(items: &[Json]) -> Tag {
        same_kind(items.iter().map(component_tag).collect())
    }
    fn same_kind(mut tags: Vec<Tag>) -> Tag {
        if tags.iter().any(|t| matches!(t, Tag::Compound(_))) {
            for t in &mut tags {
                if let Tag::String(s) = t {
                    *t = Tag::compound([("text", Tag::String(s.clone()))]);
                }
            }
        }
        Tag::List(tags)
    }
    fn value(v: &Json) -> Tag {
        match v {
            Json::Bool(b) => Tag::Byte(i8::from(*b)),
            Json::Number(n) => n.parse().map_or_else(
                |_| Tag::Double(n.parse::<f64>().unwrap_or(0.0).to_bits()),
                Tag::Int,
            ),
            Json::String(s) => Tag::String(JavaString::from(s.as_str())),
            Json::Array(items) => list(items),
            Json::Object(members) => Tag::compound(
                members
                    .iter()
                    .map(|(k, v)| (k.as_str(), value(v)))
                    .collect::<Vec<_>>(),
            ),
            Json::Null => Tag::String(JavaString::from("")),
        }
    }
    match v {
        Json::Array(items) if !items.is_empty() => {
            let mut first = component_tag(&items[0]);
            if let Tag::String(s) = &first {
                first = Tag::compound([("text", Tag::String(s.clone()))]);
            }
            if let Tag::Compound(map) = &mut first {
                let mut extra = match map.remove(&JavaString::from("extra")) {
                    Some(Tag::List(e)) => e,
                    _ => Vec::new(),
                };
                extra.extend(items[1..].iter().map(component_tag));
                map.insert(JavaString::from("extra"), same_kind(extra));
            }
            first
        }
        Json::Object(members) => {
            if let [(key, Json::String(text))] = members.as_slice()
                && key == "text"
            {
                return Tag::String(JavaString::from(text.as_str()));
            }
            Tag::compound(
                members
                    .iter()
                    .map(|(k, v)| {
                        let tag = match k.as_str() {
                            "extra" | "with" => match v {
                                Json::Array(items) => list(items),
                                other => value(other),
                            },
                            _ => value(v),
                        };
                        (k.as_str(), tag)
                    })
                    .collect::<Vec<_>>(),
            )
        }
        Json::String(s) | Json::Number(s) => Tag::String(JavaString::from(s.as_str())),
        Json::Bool(b) => Tag::String(JavaString::from(b.to_string().as_str())),
        _ => Tag::String(JavaString::from("")),
    }
}

/// StringUtil.isValidPlayerName.
pub fn valid_name(name: &str) -> bool {
    (1..=16).contains(&name.len()) && name.chars().all(|c| c as u32 > 32 && (c as u32) < 127)
}

thread_local! {
    static HOLDING: Cell<bool> = const { Cell::new(false) };
}

/// While one is alive, the packets this thread sends to each player are held
/// and handed to the player's writer thread together by
/// [`PlayerSender::flush`]: one wake-up per player per tick rather than one
/// per packet. The bytes and their order don't change.
pub struct Holding(bool);
impl Holding {
    pub fn start() -> Self {
        Self(HOLDING.replace(true))
    }
}
impl Drop for Holding {
    fn drop(&mut self) {
        HOLDING.set(self.0);
    }
}

/// The sending half given to the game thread.
#[derive(Clone)]
pub struct PlayerSender {
    pub tx: Sender<Out>,
    pub pending: Arc<AtomicUsize>,
    held: Arc<Mutex<Vec<Vec<u8>>>>,
    /// What [`flush_all`] gave the post thread and it hasn't passed on yet.
    posted: Arc<AtomicUsize>,
    #[cfg(feature = "paired-packages")]
    staged: Option<StagedSender>,
    #[cfg(feature = "paired-packages")]
    retained: Option<Arc<RetainedOutbound>>,
}
impl PlayerSender {
    #[cfg(feature = "paired-packages")]
    pub(super) fn retained_matches(&self, outbound: &Arc<RetainedOutbound>) -> bool {
        self.retained
            .as_ref()
            .is_some_and(|owner| Arc::ptr_eq(owner, outbound))
    }
    #[cfg(feature = "paired-packages")]
    pub(in crate::lan) fn same_retained_connection(&self, other: &Self) -> bool {
        match (&self.retained, &other.retained) {
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }
    pub fn new(tx: Sender<Out>, pending: Arc<AtomicUsize>) -> Self {
        Self {
            tx,
            pending,
            held: Arc::default(),
            posted: Arc::default(),
            #[cfg(feature = "paired-packages")]
            staged: None,
            #[cfg(feature = "paired-packages")]
            retained: None,
        }
    }
    fn held(&self) -> std::sync::MutexGuard<'_, Vec<Vec<u8>>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }
    pub fn send(&self, body: Vec<u8>) {
        #[cfg(feature = "paired-packages")]
        if let Some(retained) = &self.retained {
            retained.enqueue(vec![body], HOLDING.get());
            return;
        }
        #[cfg(feature = "paired-packages")]
        if let Some(staged) = &self.staged {
            staged.send(body);
            return;
        }
        self.pending.fetch_add(body.len(), Ordering::Relaxed);
        if HOLDING.get() {
            self.held().push(body);
        } else {
            self.flush();
            self.deliver(Out::Packet(body));
        }
    }
    /// Several packets in order, as [`Self::send`] each: under one lock
    /// while the tick holds them.
    pub fn send_many(&self, bodies: impl IntoIterator<Item = Vec<u8>>) {
        #[cfg(feature = "paired-packages")]
        if let Some(retained) = &self.retained {
            retained.enqueue(bodies.into_iter().collect(), HOLDING.get());
            return;
        }
        #[cfg(feature = "paired-packages")]
        if self.staged.is_some() {
            bodies.into_iter().for_each(|body| self.send(body));
            return;
        }
        if !HOLDING.get() {
            bodies.into_iter().for_each(|body| self.send(body));
            return;
        }
        let mut held = self.held();
        for body in bodies {
            self.pending.fetch_add(body.len(), Ordering::Relaxed);
            held.push(body);
        }
    }
    /// Hand the held packets to the writer thread.
    pub fn flush(&self) {
        #[cfg(feature = "paired-packages")]
        if let Some(retained) = &self.retained {
            retained.flush();
            return;
        }
        let held = std::mem::take(&mut *self.held());
        if !held.is_empty() {
            self.deliver(Out::Packets(held));
        }
    }
    /// Send a play disconnect with this reason after what was sent before.
    pub fn disconnect(&self, reason: horizon_world::nbt::Tag) {
        #[cfg(feature = "paired-packages")]
        if let Some(staged) = &self.staged {
            if let Some(stage) = staged.stage.upgrade() {
                stage.fail(super::StagedFailure::ConnectionEnded);
            }
            return;
        }
        self.flush();
        self.deliver(Out::Disconnect(reason));
    }
    /// To the writer thread, after anything still with the post thread.
    fn deliver(&self, out: Out) {
        #[cfg(feature = "paired-packages")]
        if let Some(retained) = &self.retained {
            match out {
                Out::Packet(body) => retained.enqueue(vec![body], false),
                Out::Packets(bodies) => retained.enqueue(bodies, false),
                Out::Disconnect(reason) => retained.enqueue_internal(
                    vec![play::Clientbound::Disconnect(cfg::Disconnect { reason }).encode()],
                    false,
                    true,
                ),
                Out::Close => retained.seal(),
            }
            return;
        }
        if self.posted.load(Ordering::Acquire) == 0 {
            let _ = self.tx.send(out);
        } else {
            post(vec![self.posting(out)]);
        }
    }
    fn posting(&self, out: Out) -> Posted {
        self.posted.fetch_add(1, Ordering::AcqRel);
        Posted {
            tx: self.tx.clone(),
            out,
            posted: self.posted.clone(),
        }
    }

    #[cfg(feature = "paired-packages")]
    pub fn staged_identity(&self) -> Option<u32> {
        self.staged.as_ref().map(|s| s.conn)
    }
    #[cfg(feature = "paired-packages")]
    pub fn staged_pending(&self) -> Option<usize> {
        self.staged
            .as_ref()
            .map(|_| self.pending.load(Ordering::Acquire))
    }
    #[cfg(feature = "paired-packages")]
    pub fn same_connection(&self, other: &Self) -> bool {
        self.staged_identity().is_some()
            && self.staged_identity() == other.staged_identity()
            && Arc::ptr_eq(&self.pending, &other.pending)
    }
}

#[cfg(feature = "paired-packages")]
#[derive(Clone)]
struct StagedSender {
    tx: Sender<StagedBody>,
    stage: std::sync::Weak<super::StagedState>,
    pending: Arc<AtomicUsize>,
    conn: u32,
}
#[cfg(feature = "paired-packages")]
impl StagedSender {
    fn send(&self, body: Vec<u8>) {
        let Some(stage) = self.stage.upgrade() else {
            return;
        };
        if stage.closing.load(Ordering::Acquire) {
            return;
        }
        // Reservation precedes retention in every staged queue. A body remains
        // charged while the native write is active, including on writer errors.
        let Some(charged) = StagedBody::reserve(body, self.pending.clone(), stage.outbound.clone())
        else {
            stage.fail(super::StagedFailure::OutboundOverflow);
            return;
        };
        if self.tx.send(charged).is_err() {
            stage.fail(super::StagedFailure::Writer);
        }
    }
}
#[cfg(feature = "paired-packages")]
struct StagedBody {
    body: Vec<u8>,
    pending: Arc<AtomicUsize>,
    host: Arc<AtomicUsize>,
}
#[cfg(feature = "paired-packages")]
impl StagedBody {
    fn reserve(body: Vec<u8>, pending: Arc<AtomicUsize>, host: Arc<AtomicUsize>) -> Option<Self> {
        let bytes = body.len();
        // Keep the crate's Rust 1.90 floor: fetch_update is deprecated in the
        // pinned compiler, while its renamed replacement requires Rust 1.95.
        let mut current = host.load(Ordering::Acquire);
        loop {
            let next = current
                .checked_add(bytes)
                .filter(|n| *n <= super::STAGED_OUT_BYTES)?;
            match host.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
        pending.fetch_add(bytes, Ordering::AcqRel);
        Some(Self {
            body,
            pending,
            host,
        })
    }
}
#[cfg(feature = "paired-packages")]
impl Drop for StagedBody {
    fn drop(&mut self) {
        self.pending.fetch_sub(self.body.len(), Ordering::AcqRel);
        self.host.fetch_sub(self.body.len(), Ordering::AcqRel);
    }
}

/// The registry keeps this queue even when either native actor panics. An active
/// body stays here until a successful native write; failure never drains it.
#[cfg(feature = "paired-packages")]
pub(super) struct RetainedOutbound {
    inner: Mutex<RetainedQueue>,
    ready: std::sync::Condvar,
    pending: Arc<AtomicUsize>,
    close_latch: Arc<std::sync::atomic::AtomicBool>,
}
#[cfg(feature = "paired-packages")]
#[derive(Default)]
struct RetainedQueue {
    queued: std::collections::VecDeque<Arc<[u8]>>,
    held: Vec<Arc<[u8]>>,
    active: Option<Arc<[u8]>>,
    written: usize,
    failed: usize,
    sealed: bool,
    disconnecting: bool,
    close_after: Option<Arc<[u8]>>,
    failure: Option<RetainedOutcome>,
    // These are the original, unadmitted vectors, never wire work to retry.
    refused: Vec<RetainedRefusedBatch>,
}
#[cfg(feature = "paired-packages")]
struct RetainedRefusedBatch {
    reason: &'static str,
    bodies: Vec<Vec<u8>>,
}
/// Counts describe retained bodies and successful native writes, not delivery.
#[cfg(feature = "paired-packages")]
#[derive(Clone, Debug)]
pub struct RetainedOutboundStatus {
    pub queued_bodies: usize,
    pub queued_bytes: usize,
    pub active_bodies: usize,
    pub active_bytes: usize,
    pub written_bodies: usize,
    pub failed_bodies: usize,
    pub sealed: bool,
    pub refused_bodies: usize,
    pub refused_bytes: usize,
    pub failure: Option<RetainedOutcome>,
    pub first_refusal: Option<&'static str>,
}
#[cfg(feature = "paired-packages")]
impl RetainedOutbound {
    fn new(close_latch: Arc<std::sync::atomic::AtomicBool>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::default(),
            ready: std::sync::Condvar::new(),
            pending: Arc::default(),
            close_latch,
        })
    }
    fn inner(&self) -> std::sync::MutexGuard<'_, RetainedQueue> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
    pub(super) fn seal(&self) {
        self.inner().sealed = true;
        self.ready.notify_all();
    }
    pub(super) fn snapshot(&self) -> RetainedOutboundStatus {
        let q = self.inner();
        RetainedOutboundStatus {
            queued_bodies: q.queued.len() + q.held.len(),
            queued_bytes: q.queued.iter().chain(&q.held).map(|b| b.len()).sum(),
            active_bodies: usize::from(q.active.is_some()),
            active_bytes: q.active.as_ref().map_or(0, |b| b.len()),
            written_bodies: q.written,
            failed_bodies: q.failed,
            sealed: q.sealed || q.disconnecting || self.close_latch.load(Ordering::Acquire),
            refused_bodies: q.refused.iter().map(|r| r.bodies.len()).sum(),
            refused_bytes: q.refused.iter().flat_map(|r| &r.bodies).map(Vec::len).sum(),
            failure: q.failure.clone(),
            first_refusal: q.refused.first().map(|r| r.reason),
        }
    }
    fn enqueue(&self, bodies: Vec<Vec<u8>>, held: bool) {
        self.enqueue_internal(bodies, held, false);
    }
    fn enqueue_internal(&self, bodies: Vec<Vec<u8>>, held: bool, close_after: bool) {
        let bytes = bodies
            .iter()
            .try_fold(0usize, |sum, b| sum.checked_add(b.len()));
        let mut q = self.inner();
        // This load is the enqueue admission point shared with the registry's
        // permanent close latch, including the gap before individual seals.
        if self.close_latch.load(Ordering::Acquire) || q.sealed || q.disconnecting {
            let reason = if self.close_latch.load(Ordering::Acquire) {
                "closing"
            } else if q.disconnecting {
                "disconnecting"
            } else {
                "sealed"
            };
            q.refused.push(RetainedRefusedBatch { reason, bodies });
            q.failure.get_or_insert_with(|| {
                RetainedOutcome::Protocol(
                    "outbound admission refused after close/seal/disconnect".into(),
                )
            });
            return;
        }
        let Some(next) = bytes
            .and_then(|n| self.pending.load(Ordering::Acquire).checked_add(n))
            .filter(|n| *n <= MAX_PENDING)
        else {
            q.refused.push(RetainedRefusedBatch {
                reason: "capacity",
                bodies,
            });
            q.failure.get_or_insert(RetainedOutcome::Protocol(
                "outbound queue limit exceeded".into(),
            ));
            q.sealed = true;
            self.ready.notify_all();
            return;
        };
        self.pending.store(next, Ordering::Release);
        let bodies: Vec<Arc<[u8]>> = bodies.into_iter().map(Arc::from).collect();
        if close_after {
            q.disconnecting = true;
            q.close_after = bodies.last().cloned();
        }
        if held {
            q.held.extend(bodies);
        } else {
            let previous = std::mem::take(&mut q.held);
            q.queued.extend(previous);
            q.queued.extend(bodies);
        }
        self.ready.notify_one();
    }
    fn flush(&self) {
        let mut q = self.inner();
        // Preserve held custody after closure, without starting new IO.
        if !self.close_latch.load(Ordering::Acquire) && !q.sealed {
            let held = std::mem::take(&mut q.held);
            q.queued.extend(held);
            self.ready.notify_one();
        }
    }
    fn next(&self) -> Option<Arc<[u8]>> {
        let mut q = self.inner();
        if q.queued.is_empty() && !q.sealed && !self.close_latch.load(Ordering::Acquire) {
            q = self
                .ready
                .wait_timeout(q, Duration::from_millis(200))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        if self.close_latch.load(Ordering::Acquire) || q.sealed {
            return None;
        }
        let body = q.queued.pop_front()?;
        q.active = Some(body.clone());
        Some(body)
    }
    fn written(&self) {
        let mut q = self.inner();
        if let Some(body) = q.active.take() {
            self.pending.fetch_sub(body.len(), Ordering::AcqRel);
            q.written += 1;
            if q.close_after
                .as_ref()
                .is_some_and(|last| Arc::ptr_eq(last, &body))
            {
                q.sealed = true;
            }
        }
    }
    fn failed(&self, failure: RetainedOutcome) {
        let mut q = self.inner();
        q.failed += usize::from(q.active.is_some());
        q.failure.get_or_insert(failure);
        q.sealed = true;
        self.ready.notify_all();
    }
    fn sender(self: &Arc<Self>) -> PlayerSender {
        let (tx, rx) = channel();
        drop(rx);
        PlayerSender {
            tx,
            pending: self.pending.clone(),
            held: Arc::default(),
            posted: Arc::default(),
            staged: None,
            retained: Some(self.clone()),
        }
    }
}

/// Hand every player's held packets to their writer threads. Waking a writer
/// costs a system call, and on a busy machine often the rest of the core's
/// time slice; one thread of its own wakes them, so the game thread wakes one
/// thread per tick, not one per player. Each player's packets keep their
/// order, also with what is sent to it next.
pub fn flush_all<'a>(senders: impl IntoIterator<Item = &'a PlayerSender>) {
    let batch: Vec<Posted> = senders
        .into_iter()
        .filter_map(|s| {
            #[cfg(feature = "paired-packages")]
            if let Some(retained) = &s.retained {
                retained.flush();
                return None;
            }
            let held = std::mem::take(&mut *s.held());
            (!held.is_empty()).then(|| s.posting(Out::Packets(held)))
        })
        .collect();
    if !batch.is_empty() {
        post(batch);
    }
}

/// Packets on their way through the post thread.
struct Posted {
    tx: Sender<Out>,
    out: Out,
    posted: Arc<AtomicUsize>,
}
impl Posted {
    fn hand_over(self) {
        let _ = self.tx.send(self.out);
        self.posted.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Give a batch to the post thread, started the first time; without one
/// (the thread could not start) the batch is handed over here.
fn post(batch: Vec<Posted>) {
    #[cfg(all(
        test,
        feature = "paired-packages",
        feature = "native-writer-test-control",
        debug_assertions
    ))]
    retained_controls::POST_CALLS.fetch_add(1, Ordering::Relaxed);
    static POST: std::sync::OnceLock<Option<Sender<Vec<Posted>>>> = std::sync::OnceLock::new();
    let post = POST.get_or_init(|| {
        let (tx, rx) = channel::<Vec<Posted>>();
        std::thread::Builder::new()
            .name("horizon-lan-post".to_owned())
            .spawn(move || {
                for batch in rx {
                    batch.into_iter().for_each(Posted::hand_over);
                }
            })
            .ok()
            .map(|_| tx)
    });
    let unsent = match post {
        Some(tx) => tx.send(batch).err().map(|e| e.0),
        None => Some(batch),
    };
    unsent.into_iter().flatten().for_each(Posted::hand_over);
}

/// LegacyQueryHandler: the pre-1.7 server list ping. What the first read
/// holds decides the reply (1.3 and older: no more bytes; 1.4 and 1.5: 0x01;
/// 1.6: 0x01 then the MC|PingHost payload); anything else gets no reply.
fn serve_legacy(mut stream: TcpStream, shared: &Shared) -> Result<(), String> {
    use std::io::{Read, Write};
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let mut data = Vec::new();
    let mut chunk = [0u8; 1024];
    // The bytes of the first read: what arrived together.
    if let Ok(n) = stream.read(&mut chunk) {
        data.extend_from_slice(&chunk[..n]);
    }
    let in_string = |d: &[u8], at: &mut usize| -> Option<String> {
        let len = usize::from(u16::from_be_bytes([*d.get(*at)?, *d.get(*at + 1)?]));
        let bytes = d.get(*at + 2..*at + 2 + len * 2)?;
        *at += 2 + len * 2;
        Some(String::from_utf16_lossy(
            &bytes
                .chunks(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect::<Vec<_>>(),
        ))
    };
    let motd = shared.motd();
    let (online, max) = (shared.online(), shared.max_players());
    let body = if data.len() == 1 {
        format!("{motd}\u{a7}{online}\u{a7}{max}")
    } else {
        if data[1] != 1 {
            return Ok(());
        }
        if data.len() > 2 {
            // readCustomPayloadPacket.
            let mut at = 3;
            let valid = data[2] == 0xFA
                && in_string(&data, &mut at).as_deref() == Some("MC|PingHost")
                && data
                    .get(at..at + 2)
                    .map(|b| usize::from(u16::from_be_bytes([b[0], b[1]])))
                    == Some(data.len().saturating_sub(at + 2))
                && {
                    at += 2;
                    data.get(at).is_some_and(|v| *v >= 73)
                }
                && {
                    at += 1;
                    in_string(&data, &mut at).is_some()
                }
                && data
                    .get(at..at + 4)
                    .is_some_and(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]) <= 65535);
            if !valid {
                return Ok(());
            }
        }
        format!(
            "\u{a7}1\0127\0{}\0{motd}\0{online}\0{max}",
            horizon_core::VANILLA_VERSION
        )
    };
    let units: Vec<u16> = body.encode_utf16().collect();
    let mut out = vec![0xFF];
    out.extend_from_slice(&(units.len() as u16).to_be_bytes());
    for u in units {
        out.extend_from_slice(&u.to_be_bytes());
    }
    stream.write_all(&out).map_err(|e| e.to_string())?;
    let _ = stream.shutdown(std::net::Shutdown::Both);
    Ok(())
}

pub fn serve(
    stream: TcpStream,
    address: std::net::SocketAddr,
    shared: Arc<Shared>,
) -> Result<(), String> {
    let e = |e: horizon_protocol::Error| e.to_string();
    // LegacyQueryHandler, ahead of the frame decoder while status is on.
    let mut first = [0u8; 1];
    if shared.config.enable_status
        && stream.peek(&mut first).is_ok_and(|n| n == 1)
        && first[0] == 0xFE
    {
        return serve_legacy(stream, &shared);
    }
    serve_connection(Connection::from_stream(stream).map_err(e)?, address, shared)
}

/// `ServerConnectionListener.startMemoryChannel`: an integrated server's
/// own player, over one end of an in-memory pipe.
pub fn serve_local(
    pipe: horizon_protocol::connection::Pipe,
    shared: Arc<Shared>,
) -> Result<(), String> {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
    serve_connection(Connection::local(pipe), address, shared)
}

/// The caller registered this connection's original shutdown capability before
/// releasing its native actor. This entry deliberately bypasses legacy peek.
#[cfg(feature = "paired-packages")]
pub(super) fn serve_retained(
    c: Connection,
    address: std::net::SocketAddr,
    shared: Arc<Shared>,
    owner: RetainedContext,
) -> RetainedOutcome {
    let _terminal = RetainedInterrupt(owner.clone());
    match serve_modern(c, address, shared, Some(&owner)) {
        Ok(()) => RetainedOutcome::Returned,
        Err(error) => retained_outcome(error),
    }
}

#[cfg(feature = "paired-packages")]
struct RetainedInterrupt(RetainedContext);
#[cfg(feature = "paired-packages")]
impl Drop for RetainedInterrupt {
    fn drop(&mut self) {
        self.0.interrupt();
    }
}

#[cfg(feature = "paired-packages")]
fn retained_outcome(error: String) -> RetainedOutcome {
    if let Some(native) = error.strip_prefix("retained native IO: ") {
        RetainedOutcome::Io(native.to_owned())
    } else if let Some(receive) = error.strip_prefix("retained receive: ") {
        RetainedOutcome::Receive(receive.to_owned())
    } else if let Some(encode) = error.strip_prefix("retained encode: ") {
        RetainedOutcome::Encode(encode.to_owned())
    } else if error == "retained EOF: Connection closed by the server" {
        RetainedOutcome::Eof
    } else if let Some(spawn) = error.strip_prefix("retained writer spawn: ") {
        RetainedOutcome::Spawn(spawn.to_owned())
    } else if error == "retained connection closed" {
        RetainedOutcome::Closed
    } else if error == "retained queue closed" {
        RetainedOutcome::QueueClosed
    } else {
        RetainedOutcome::Protocol(error)
    }
}

#[cfg(feature = "paired-packages")]
thread_local! {
    static STAGED_CONN: Cell<Option<u32>> = const { Cell::new(None) };
}

#[cfg(feature = "paired-packages")]
pub(super) fn serve_local_staged(
    pipe: horizon_protocol::connection::Pipe,
    shared: Arc<Shared>,
    conn: u32,
) {
    let Some(stage) = shared.stage.clone() else {
        return;
    };
    let mut terminal = StagedAttemptTerminal {
        stage,
        conn,
        reason: super::StagedFailure::ConnectionEnded,
    };
    STAGED_CONN.set(Some(conn));
    if serve_local(pipe, shared).is_err() {
        terminal.reason = super::StagedFailure::Decode;
    }
}

/// A terminal slot cannot be lost to an outer reader panic before Joined. This
/// guard only records bounded evidence; it does not join, detach or discard owners.
#[cfg(feature = "paired-packages")]
struct StagedAttemptTerminal {
    stage: Arc<super::StagedState>,
    conn: u32,
    reason: super::StagedFailure,
}
#[cfg(feature = "paired-packages")]
impl Drop for StagedAttemptTerminal {
    fn drop(&mut self) {
        self.stage.terminal(
            self.conn,
            if std::thread::panicking() {
                super::StagedFailure::ThreadPanic
            } else {
                self.reason
            },
        );
    }
}

/// Ordinary receives retain their existing behavior. Staged receives use the
/// same decoder; a separately registered close-only capability wakes native IO.
fn recv_connection(
    c: &mut Connection,
    shared: &Shared,
) -> horizon_protocol::Result<(i32, Vec<u8>)> {
    #[cfg(feature = "paired-packages")]
    if let Some(stage) = &shared.stage {
        let result = recv_staged(&mut c.reader, stage);
        if result.is_err() {
            c.writer.shutdown();
        }
        return result;
    }
    let _ = shared;
    c.recv()
}

#[cfg(feature = "paired-packages")]
fn recv_staged(
    reader: &mut ConnReader,
    stage: &super::StagedState,
) -> horizon_protocol::Result<(i32, Vec<u8>)> {
    if stage.closing.load(Ordering::Acquire) {
        return Err(horizon_protocol::Error::new("staged connection closed"));
    }
    let result = reader.recv();
    // Native close may return a complete buffered frame first, or drain a
    // finite incomplete prefix before EOF. Never dispatch it after closure.
    if stage.closing.load(Ordering::Acquire) {
        return Err(horizon_protocol::Error::new("staged connection closed"));
    }
    result
}

fn serve_connection(
    c: Connection,
    address: std::net::SocketAddr,
    shared: Arc<Shared>,
) -> Result<(), String> {
    serve_modern(c, address, shared, None)
}

// The optional owner follows the existing packet pipeline explicitly. Ordinary
// callers retain their codecs, admission and legacy handling unchanged.
fn serve_modern(
    mut c: Connection,
    address: std::net::SocketAddr,
    shared: Arc<Shared>,
    retained: Option<&RetainedContext>,
) -> Result<(), String> {
    let e = |e: horizon_protocol::Error| e.to_string();
    c.reader
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| native_error(retained, e))?;
    let (id, payload) = recv_modern(&mut c, &shared, retained)?;
    let intention = match handshake::Serverbound::decode(id, &payload).map_err(e)? {
        handshake::Serverbound::Intention(i) => i,
        _ => return Err("expected a handshake".into()),
    };
    #[cfg(feature = "paired-packages")]
    if shared.stage.is_some() && intention.intent != handshake::LOGIN {
        return Err("staged connection requires offline login".into());
    }
    match intention.intent {
        // ServerHandshakePacketListenerImpl: a server that does not reply
        // to status requests closes the connection.
        handshake::STATUS if !shared.config.enable_status => Ok(()),
        handshake::STATUS => serve_status(&mut c, &shared, retained),
        handshake::TRANSFER if !shared.config.accepts_transfers => {
            if retained.is_some() {
                return Err("retained connection requires modern offline login".into());
            }
            login_disconnect(
                &mut c,
                &translatable("multiplayer.disconnect.transfers_disabled", &[]),
            );
            Ok(())
        }
        handshake::LOGIN | handshake::TRANSFER => {
            if intention.protocol != PROTOCOL_VERSION {
                let key = if intention.protocol < 754 {
                    "multiplayer.disconnect.outdated_client"
                } else {
                    "multiplayer.disconnect.incompatible"
                };
                let json = format!(
                    "{{\"translate\":\"{key}\",\"with\":[\"{}\"]}}",
                    horizon_protocol::GAME_VERSION
                );
                let body = &login::Clientbound::Disconnect(login::Disconnect { reason_json: json })
                    .encode();
                if retained.is_some() {
                    send_modern(&mut c, body, retained)?;
                    return Err("retained connection requires protocol 777".into());
                }
                let _ = c.send(body);
                return Ok(());
            }
            serve_login(c, address, &shared, retained)
        }
        other => Err(format!("unknown intent {other}")),
    }
}

fn native_error(retained: Option<&RetainedContext>, error: impl std::fmt::Display) -> String {
    if retained.is_some() {
        format!("retained native IO: {error}")
    } else {
        error.to_string()
    }
}

fn recv_modern(
    c: &mut Connection,
    shared: &Shared,
    retained: Option<&RetainedContext>,
) -> Result<(i32, Vec<u8>), String> {
    #[cfg(feature = "paired-packages")]
    if let Some(owner) = retained {
        if owner.is_closing() {
            return Err("retained connection closed".into());
        }
        #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
        retained_controls::read_started(owner.conn());
        let packet = c.recv();
        #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
        retained_controls::read_finished(owner.conn());
        let packet = packet.map_err(|e| receive_error(retained, e))?;
        if owner.is_closing() {
            return Err("retained connection closed".into());
        }
        return Ok(packet);
    }
    recv_connection(c, shared).map_err(|e| native_error(retained, e))
}

#[cfg(feature = "paired-packages")]
fn receive_error(retained: Option<&RetainedContext>, error: horizon_protocol::Error) -> String {
    if retained.is_some() {
        // ConnReader::recv emits this exact fixed message only at read()==0.
        // Every other baseline Error may be native IO or a codec failure; its
        // text alone cannot recover a missing typed source.
        if error.0 == "Connection closed by the server" {
            return format!("retained EOF: {error}");
        }
        return format!("retained receive: {error}");
    }
    error.to_string()
}

fn send_modern(
    c: &mut Connection,
    body: &[u8],
    retained: Option<&RetainedContext>,
) -> Result<(), String> {
    #[cfg(feature = "paired-packages")]
    if retained.is_some_and(RetainedContext::is_closing) {
        return Err("retained connection closed".into());
    }
    if retained.is_some() {
        let contents =
            prepare_retained_body(&c.writer, body).map_err(|e| format!("retained encode: {e}"))?;
        return c
            .writer
            .send_contents(&contents)
            .map_err(|e| native_error(retained, e));
    }
    c.send(body).map_err(|e| e.to_string())
}

fn serve_status(
    c: &mut Connection,
    shared: &Shared,
    retained: Option<&RetainedContext>,
) -> Result<(), String> {
    let e = |e: horizon_protocol::Error| e.to_string();
    let mut requested = false;
    loop {
        let (id, payload) = recv_modern(c, shared, retained)?;
        match status::Serverbound::decode(id, &payload).map_err(e)? {
            // ServerStatusPacketListenerImpl: a second request disconnects.
            status::Serverbound::StatusRequest(_) if requested => return Ok(()),
            status::Serverbound::StatusRequest(_) => {
                requested = true;
                let json = shared.status_json();
                send_modern(
                    c,
                    &status::Clientbound::StatusResponse(status::StatusResponse { json }).encode(),
                    retained,
                )?;
            }
            status::Serverbound::PingRequest(p) => {
                send_modern(c, &status::Clientbound::PongResponse(p).encode(), retained)?;
                return Ok(());
            }
            status::Serverbound::Unknown(..) => return Ok(()),
        }
    }
}

/// ServerLoginPacketListenerImpl.getUserName: the name and the address
/// (Connection.getLoggableAddress, `/ip:port` as InetSocketAddress prints it).
fn user_name(name: &str, address: std::net::SocketAddr, log: &super::NetLog) -> String {
    if log.log_ips {
        format!("{name} (/{address})")
    } else {
        format!("{name} (<ip address withheld>)")
    }
}

/// ServerLoginPacketListenerImpl.disconnect: log the reason's text and send
/// it. Returns the text, for the "lost connection" line that follows.
fn refuse(
    c: &mut Connection,
    shared: &Shared,
    thread: &str,
    user: &str,
    reason: &horizon_world::nbt::Tag,
) -> String {
    let text = shared
        .config
        .log
        .as_ref()
        .map(|log| horizon_protocol::text::plain(reason, &log.lang))
        .unwrap_or_default();
    if let Some(log) = &shared.config.log {
        (log.write)(thread, "INFO", &format!("Disconnecting {user}: {text}"));
    }
    login_disconnect(c, reason);
    text
}

/// ServerLoginPacketListenerImpl.onDisconnect, once the server thread sees
/// the connection closed: by a refusal, or by the client ("Disconnected").
fn lost(shared: &Shared, user: &str, text: &str) {
    if let Some(log) = &shared.config.log {
        (log.write)(
            "Server thread",
            "INFO",
            &format!("{user} lost connection: {text}"),
        );
    }
}

/// UNIQUE_THREAD_ID: the User Authenticator threads are numbered from 1.
static AUTHENTICATORS: AtomicU32 = AtomicU32::new(0);

/// ServerLoginPacketListenerImpl.handleKey and the User Authenticator
/// thread: check the client's answer to the encryption request, encrypt the
/// connection, and ask the session service whether the player joined with
/// this server's hash. The profile, or the text of the refusal.
fn authenticate(
    c: &mut Connection,
    shared: &Shared,
    online: &super::OnlineMode,
    name: &str,
    address: std::net::SocketAddr,
    user: &str,
) -> Result<Result<(u128, Vec<login::Property>), String>, String> {
    let e = |e: horizon_protocol::Error| e.to_string();
    let mut challenge = [0u8; 4];
    horizon_protocol::crypto::random_bytes(&mut challenge).map_err(e)?;
    c.send(
        &login::Clientbound::Hello(login::Hello {
            server_id: String::new(),
            public_key: online.public_der.clone(),
            challenge: challenge.to_vec(),
            should_authenticate: true,
        })
        .encode(),
    )
    .map_err(e)?;
    let (id, payload) = recv_connection(c, shared).map_err(e)?;
    let key = match login::Serverbound::decode(id, &payload).map_err(e)? {
        login::Serverbound::Key(k) => k,
        _ => return Err("Unexpected packet during login".into()),
    };
    // ServerboundKeyPacket.isChallengeValid and getSecretKey.
    let protocol_error = || "Protocol error".to_owned();
    if online.key.decrypt(&key.challenge).ok().as_deref() != Some(&challenge[..]) {
        return Err(protocol_error());
    }
    let secret: [u8; 16] = online
        .key
        .decrypt(&key.key)
        .ok()
        .and_then(|s| s.try_into().ok())
        .ok_or_else(protocol_error)?;
    let digest = server_hash("", &secret, &online.public_der);
    c.enable_encryption(&secret);
    let thread = format!(
        "User Authenticator #{}",
        AUTHENTICATORS.fetch_add(1, Ordering::Relaxed) + 1
    );
    let log = |level: &str, line: &str| {
        if let Some(log) = &shared.config.log {
            (log.write)(&thread, level, line);
        }
    };
    let ip = online.prevent_proxy.then(|| address.ip());
    match crate::auth::has_joined(name, &digest, ip) {
        Ok(Some(profile)) => {
            log(
                "INFO",
                &format!("UUID of player {name} is {}", uuid_string(profile.uuid)),
            );
            Ok(Ok((profile.uuid, profile.properties)))
        }
        Ok(None) => {
            let text = refuse(
                c,
                shared,
                &thread,
                user,
                &translatable("multiplayer.disconnect.unverified_username", &[]),
            );
            log(
                "ERROR",
                &format!("Username '{name}' tried to join with an invalid session"),
            );
            Ok(Err(text))
        }
        Err(_) => {
            let text = refuse(
                c,
                shared,
                &thread,
                user,
                &translatable("multiplayer.disconnect.authservers_down", &[]),
            );
            log(
                "ERROR",
                "Couldn't verify username because servers are unavailable",
            );
            Ok(Err(text))
        }
    }
}

fn serve_login(
    mut c: Connection,
    address: std::net::SocketAddr,
    shared: &Arc<Shared>,
    retained: Option<&RetainedContext>,
) -> Result<(), String> {
    let e = |e: horizon_protocol::Error| e.to_string();
    let (id, payload) = recv_modern(&mut c, shared, retained)?;
    let hello = match login::Serverbound::decode(id, &payload).map_err(e)? {
        login::Serverbound::Hello(h) => h,
        _ => return Err("expected a login hello".into()),
    };
    if !valid_name(&hello.name) {
        if retained.is_some() {
            send_modern(
                &mut c,
                &login::Clientbound::Disconnect(login::Disconnect {
                    reason_json: component_json(&translatable(
                        "multiplayer.disconnect.invalid_name",
                        &[],
                    )),
                })
                .encode(),
                retained,
            )?;
            return Err("invalid retained player name".into());
        }
        login_disconnect(
            &mut c,
            &translatable("multiplayer.disconnect.invalid_name", &[]),
        );
        return Ok(());
    }
    let user = shared
        .config
        .log
        .as_ref()
        .map(|log| user_name(&hello.name, address, log))
        .unwrap_or_default();
    // handleHello: online mode authenticates first; offline mode derives the
    // profile from the name.
    let (uuid, properties) = match &shared.config.online {
        Some(online) => match authenticate(&mut c, shared, online, &hello.name, address, &user) {
            Ok(Ok(profile)) => profile,
            Ok(Err(refused)) => {
                lost(shared, &user, &refused);
                return Ok(());
            }
            Err(err) => {
                lost(shared, &user, "Disconnected");
                return Err(err);
            }
        },
        None => (offline_uuid(&hello.name), Vec::new()),
    };
    // verifyLoginAndFinishConnectionSetup: PlayerList.canPlayerLogin, then
    // duplicates and a full server.
    #[cfg(feature = "paired-packages")]
    if let Some(owner) = retained
        && let Some(required) = owner.required()
    {
        use super::login_negotiation::{NegotiationConnection, negotiate_server};
        let mut scope = match NegotiationConnection::for_retained(c, &required, required.deadline())
        {
            Ok(scope) => scope,
            Err((scope, reason)) => {
                owner.retain_scope(scope);
                return Err(reason.to_string());
            }
        };
        let receipt = match negotiate_server(&mut scope, required.expectation.clone(), uuid) {
            Ok(receipt) => receipt,
            Err(reason) => {
                owner.retain_scope(scope);
                return Err(reason.to_string());
            }
        };
        let compression = if shared.config.compression_threshold >= 0 && !scope.is_memory() {
            Some(shared.config.compression_threshold)
        } else {
            None
        };
        let endpoint = match scope.configure_server(
            receipt,
            required.expectation.clone(),
            login::GameProfile {
                uuid,
                name: hello.name.clone(),
                properties,
            },
            compression,
        ) {
            Ok(endpoint) => endpoint,
            Err(failure) => {
                let reason = failure.reason.to_string();
                owner.retain_scope(failure.into_scope());
                return Err(reason);
            }
        };
        let profile = endpoint.profile().clone();
        let info = endpoint.information().clone();
        let mut connection = owner.transfer_configured(endpoint)?;
        if let Some(reason) = owner.admit_paired(uuid, &hello.name) {
            let result = send_modern(
                &mut connection,
                &play::Clientbound::Disconnect(cfg::Disconnect { reason }).encode(),
                retained,
            );
            owner.retain_connection(connection);
            return result;
        }
        let parts = Parts {
            uuid,
            name: profile.name,
            properties: profile.properties,
            info,
            brand: "horizon".into(),
        };
        return play_retained_inner(connection, parts, address, owner.clone(), true);
    }
    if let Some(required) = &shared.required_negotiation {
        use super::login_negotiation::{NegotiationConnection, negotiate_server};
        let mut scope = NegotiationConnection::for_required(c, required, required.deadline())
            .map_err(|e| e.to_string())?;
        let result = negotiate_server(&mut scope, required.expectation.clone(), uuid);
        match result {
            Ok(receipt) => {
                // No reservation exists. Never try_admit or release this UUID:
                // an existing genuine player may already own the same profile.
                let result = scope.refuse_configuration().map_err(|e| e.to_string());
                drop(receipt);
                return result;
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    #[cfg(feature = "paired-packages")]
    if let Some(owner) = retained {
        if let Some(reason) = owner.admit(shared, uuid, &hello.name) {
            send_modern(
                &mut c,
                &login::Clientbound::Disconnect(login::Disconnect {
                    reason_json: component_json(&reason),
                })
                .encode(),
                retained,
            )?;
            return Ok(());
        }
        let result = login_and_configure(&mut c, shared, uuid, &hello.name, properties, retained)?;
        if let Some(parts) = result {
            return play_retained(c, parts, address, owner.clone());
        }
        return Ok(());
    }
    if let Some(reason) = shared.try_admit(address, uuid, &hello.name) {
        let text = refuse(&mut c, shared, "Server thread", &user, &reason);
        lost(shared, &user, &text);
        return Ok(());
    }
    let result = login_and_configure(&mut c, shared, uuid, &hello.name, properties, retained);
    match result {
        Ok(Some(parts)) => {
            play(c, shared, parts, address);
            Ok(())
        }
        Ok(None) => {
            shared.release(uuid);
            Ok(())
        }
        Err(err) => {
            shared.release(uuid);
            // ServerConfigurationPacketListenerImpl.onDisconnect.
            let profile = format!("{} ({})", hello.name, uuid_string(uuid));
            lost(shared, &profile, "Disconnected");
            Err(err)
        }
    }
}

#[cfg(feature = "paired-packages")]
fn play_retained(
    c: Connection,
    parts: Parts,
    address: std::net::SocketAddr,
    owner: RetainedContext,
) -> Result<(), String> {
    play_retained_inner(c, parts, address, owner, false)
}
#[cfg(feature = "paired-packages")]
fn play_retained_inner(
    c: Connection,
    parts: Parts,
    address: std::net::SocketAddr,
    owner: RetainedContext,
    paired: bool,
) -> Result<(), String> {
    let outbound = RetainedOutbound::new(owner.close_latch());
    if !owner.attach_outbound(outbound.clone()) {
        return Err("retained connection closed".into());
    }
    let conn = owner.conn();
    let (mut reader, writer) = c.split();
    reader
        .set_read_timeout(Some(KEEP_ALIVE_TIMEOUT))
        .map_err(|e| native_error(Some(&owner), e))?;
    let alive = Arc::new(AtomicI64::new(-1));
    let latency = Arc::new(AtomicI32::new(0));
    let (start, started) = channel();
    let worker = {
        let owner = owner.clone();
        let outbound = outbound.clone();
        let alive = alive.clone();
        let latency = latency.clone();
        let builder = std::thread::Builder::new().name(format!("horizon-retained-out-{conn}"));
        #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
        let builder = if retained_controls::writer_spawn_fails(conn) {
            builder.stack_size(usize::MAX / 2)
        } else {
            builder
        };
        builder
            .spawn(move || {
                let _interrupt = RetainedInterrupt(owner.clone());
                let _active = RetainedActiveGuard(outbound.clone());
                if started.recv().is_err() {
                    return RetainedOutcome::QueueClosed;
                }
                horizon_threads::behind_server();
                write_retained(writer, &outbound, &owner, &alive, &latency)
            })
            .map_err(|e| format!("retained writer spawn: {e}"))?
    };
    // The original handle is now registry-owned, even when close won this race.
    let accepted = owner.install_writer(worker);
    if !accepted {
        outbound.seal();
    }
    if start.send(()).is_err() {
        owner.record_failure(RetainedOutcome::QueueClosed);
        return Err("retained queue closed".into());
    }
    if !accepted {
        return Err("retained connection closed".into());
    }
    let joined = Joined {
        conn,
        uuid: parts.uuid,
        name: parts.name,
        properties: parts.properties,
        info: parts.info,
        brand: parts.brand,
        sender: outbound.sender(),
        latency,
        address,
    };
    #[cfg(test)]
    if paired {
        owner.publication_gate();
    }
    let published = if paired {
        owner.publish_paired(joined)
    } else {
        owner.emit(NetEvent::Joined(Box::new(joined)))
    };
    if !published {
        return Err("retained queue closed".into());
    }
    #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
    retained_controls::outer_started(conn);
    let result = loop {
        if owner.is_closing() {
            break Err("retained connection closed".into());
        }
        #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
        retained_controls::read_started(conn);
        let packet = reader.recv();
        #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
        retained_controls::read_finished(conn);
        let (id, payload) = match packet {
            Ok(packet) => packet,
            Err(error) => break Err(receive_error(Some(&owner), error)),
        };
        if owner.is_closing() {
            break Err("retained connection closed".into());
        }
        match play::Serverbound::decode(id, &payload) {
            Ok(play::Serverbound::KeepAlive(k)) => alive.store(k.id, Ordering::Relaxed),
            Ok(packet) => {
                if !owner.emit(NetEvent::Packet(conn, Box::new(packet))) {
                    break Err("retained queue closed".into());
                }
            }
            Err(error) => break Err(format!("Invalid packet {id}: {error}")),
        }
    };
    let reason = result.as_ref().err().cloned().unwrap_or_default();
    let _ = owner.emit(NetEvent::Left(conn, reason));
    // Exact UUID release belongs to registry harvest after BOTH original joins.
    result
}

#[cfg(feature = "paired-packages")]
struct RetainedActiveGuard(Arc<RetainedOutbound>);
#[cfg(feature = "paired-packages")]
impl Drop for RetainedActiveGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.failed(RetainedOutcome::Panic);
        }
        self.0.seal();
    }
}

#[cfg(feature = "paired-packages")]
fn write_retained(
    mut writer: ConnWriter,
    outbound: &RetainedOutbound,
    owner: &RetainedContext,
    alive: &AtomicI64,
    latency: &AtomicI32,
) -> RetainedOutcome {
    let start = Instant::now();
    let mut sent_at: Option<(i64, Instant)> = None;
    let mut next = Instant::now() + KEEP_ALIVE_EVERY;
    loop {
        if let Some(failure) = outbound.inner().failure.clone() {
            return failure;
        }
        if owner.is_closing() || outbound.inner().sealed {
            return RetainedOutcome::Closed;
        }
        if let Some(body) = outbound.next() {
            let contents = match prepare_retained_body(&writer, &body) {
                Ok(contents) => contents,
                Err(error) => {
                    let outcome = RetainedOutcome::Encode(error.to_string());
                    outbound.failed(outcome.clone());
                    return outcome;
                }
            };
            #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
            retained_controls::before_write(owner.conn());
            if let Err(error) = writer.send_contents(&contents) {
                let outcome = RetainedOutcome::Io(error.to_string());
                outbound.failed(outcome.clone());
                return outcome;
            }
            outbound.written();
            #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
            retained_controls::after_write(owner.conn());
        }
        let now = Instant::now();
        if let Some((id, at)) = sent_at {
            if alive.load(Ordering::Relaxed) == id {
                latency.store(
                    i32::try_from(at.elapsed().as_millis()).unwrap_or(i32::MAX),
                    Ordering::Relaxed,
                );
                sent_at = None;
            } else if at.elapsed() > KEEP_ALIVE_TIMEOUT {
                return RetainedOutcome::Protocol("keep-alive timeout".into());
            }
        }
        if sent_at.is_none() && now >= next {
            let id = i64::try_from(start.elapsed().as_millis()).unwrap_or(0);
            outbound.enqueue(
                vec![play::Clientbound::KeepAlive(cfg::KeepAlive { id }).encode()],
                false,
            );
            sent_at = Some((id, now));
            next = now + KEEP_ALIVE_EVERY;
        }
    }
}

/// Fixed, serial-test controls touch the actual native actors at acquisition or
/// immediately before their existing socket writer. No arbitrary task callback.
#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
mod retained_controls {
    use super::*;
    #[derive(Default)]
    pub(super) struct Control {
        pub conn: u32,
        pub panic_writer: bool,
        pub panic_outer: bool,
        pub spawn_failure: bool,
        pub pause_writer: bool,
        pub entered: usize,
        pub completed: usize,
    }
    static CONTROL: Mutex<Control> = Mutex::new(Control {
        conn: 0,
        panic_writer: false,
        panic_outer: false,
        spawn_failure: false,
        pause_writer: false,
        entered: 0,
        completed: 0,
    });
    static WAKE: std::sync::Condvar = std::sync::Condvar::new();
    static READS: Mutex<Vec<(u32, usize, usize)>> = Mutex::new(Vec::new());
    pub(super) static POST_CALLS: AtomicUsize = AtomicUsize::new(0);
    pub(super) fn reads(conn: u32) -> (usize, usize) {
        READS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|r| r.0 == conn)
            .map_or((0, 0), |r| (r.1, r.2))
    }
    pub(super) fn read_started(conn: u32) {
        let mut reads = READS.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(r) = reads.iter_mut().find(|r| r.0 == conn) {
            r.1 += 1;
        } else {
            reads.push((conn, 1, 0));
        }
    }
    pub(super) fn read_finished(conn: u32) {
        let mut reads = READS.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(r) = reads.iter_mut().find(|r| r.0 == conn) {
            r.2 += 1;
        }
    }
    pub(super) fn set(control: Control) {
        *CONTROL.lock().unwrap_or_else(PoisonError::into_inner) = control;
        WAKE.notify_all();
    }
    pub(super) fn release() {
        CONTROL
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pause_writer = false;
        WAKE.notify_all();
    }
    pub(super) fn counts() -> (usize, usize) {
        let c = CONTROL.lock().unwrap_or_else(PoisonError::into_inner);
        (c.entered, c.completed)
    }
    pub(super) fn writer_spawn_fails(conn: u32) -> bool {
        let c = CONTROL.lock().unwrap_or_else(PoisonError::into_inner);
        c.conn == conn && c.spawn_failure
    }
    pub(super) fn outer_started(conn: u32) {
        let c = CONTROL.lock().unwrap_or_else(PoisonError::into_inner);
        let panic = c.conn == conn && c.panic_outer;
        drop(c);
        assert!(
            !panic,
            "intentional retained outer actor panic after Joined"
        );
    }
    pub(super) fn before_write(conn: u32) {
        let mut c = CONTROL.lock().unwrap_or_else(PoisonError::into_inner);
        if c.conn != conn {
            return;
        }
        c.entered += 1;
        while c.pause_writer {
            c = WAKE.wait(c).unwrap_or_else(PoisonError::into_inner);
        }
        let panic = c.panic_writer;
        drop(c);
        assert!(
            !panic,
            "intentional retained native writer panic with active payload"
        );
    }
    pub(super) fn after_write(conn: u32) {
        let mut c = CONTROL.lock().unwrap_or_else(PoisonError::into_inner);
        if c.conn == conn {
            c.completed += 1;
        }
    }
}

#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
pub(super) fn retained_test_read_progress(conn: u32) -> (usize, usize) {
    retained_controls::reads(conn)
}

#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
pub(in crate::lan) fn retained_test_pause_writer(conn: u32) {
    retained_controls::set(retained_controls::Control {
        conn,
        pause_writer: true,
        ..Default::default()
    });
}
#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
pub(in crate::lan) fn retained_test_writer_fault(conn: u32, panic: bool) {
    retained_controls::set(retained_controls::Control {
        conn,
        panic_writer: panic,
        spawn_failure: !panic,
        ..Default::default()
    });
}
#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
pub(in crate::lan) fn retained_test_writer_entered() -> bool {
    retained_controls::counts().0 != 0
}
#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
pub(in crate::lan) fn retained_test_outbound_limit() -> usize {
    MAX_PENDING
}
#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
pub(in crate::lan) fn retained_test_release_writer() {
    retained_controls::release();
}

struct Parts {
    uuid: u128,
    name: String,
    properties: Vec<login::Property>,
    info: ClientInformation,
    brand: String,
}

fn login_and_configure(
    c: &mut Connection,
    shared: &Arc<Shared>,
    uuid: u128,
    name: &str,
    properties: Vec<login::Property>,
    retained: Option<&RetainedContext>,
) -> Result<Option<Parts>, String> {
    let e = |e: horizon_protocol::Error| e.to_string();
    // ServerLoginPacketListenerImpl: memory connections are not compressed.
    let threshold = shared.config.compression_threshold;
    if threshold >= 0 && !c.is_memory() {
        send_modern(
            c,
            &login::Clientbound::Compression(login::Compression { threshold }).encode(),
            retained,
        )?;
        c.set_compression(threshold);
    }
    send_modern(
        c,
        &login::Clientbound::LoginFinished(login::LoginFinished {
            profile: login::GameProfile {
                uuid,
                name: name.to_owned(),
                properties: properties.clone(),
            },
            session: uuid,
        })
        .encode(),
        retained,
    )?;
    // The client may send its settings before acknowledging the login.
    loop {
        let (id, payload) = recv_modern(c, shared, retained)?;
        match login::Serverbound::decode(id, &payload).map_err(e)? {
            login::Serverbound::LoginAcknowledged(_) => break,
            login::Serverbound::CustomQueryAnswer(_) | login::Serverbound::CookieResponse(_) => {}
            _ => return Err("unexpected packet during login".into()),
        }
    }
    // Configuration.
    let sync: &SyncData = &shared.sync;
    let send = |c: &mut Connection, p: cfg::Clientbound| send_modern(c, &p.encode(), retained);
    send(
        c,
        cfg::Clientbound::CustomPayload(cfg::CustomPayload {
            channel: "minecraft:brand".into(),
            data: {
                let mut w = horizon_protocol::buf::Writer::new();
                // MinecraftServer.getServerModName.
                w.string("vanilla");
                w.data
            },
        }),
    )?;
    // ServerConfigurationPacketListenerImpl.startConfiguration: links, only
    // when there are any (MinecraftServer.serverLinks is empty elsewhere).
    if !shared.config.server_links.is_empty() {
        send(
            c,
            cfg::Clientbound::ServerLinks(cfg::ServerLinks {
                links: shared.config.server_links.clone(),
            }),
        )?;
    }
    send(
        c,
        cfg::Clientbound::UpdateEnabledFeatures(cfg::UpdateEnabledFeatures {
            features: sync.features.clone(),
        }),
    )?;
    let core = KnownPack {
        namespace: CORE_PACK.0.into(),
        id: CORE_PACK.1.into(),
        version: CORE_PACK.2.into(),
    };
    send(
        c,
        cfg::Clientbound::SelectKnownPacks(cfg::SelectKnownPacks {
            packs: vec![core.clone()],
        }),
    )?;
    let mut info = ClientInformation::new(10);
    let mut brand = String::from("vanilla");
    let known_pack = loop {
        let (id, payload) = recv_modern(c, shared, retained)?;
        match cfg::Serverbound::decode(id, &payload).map_err(e)? {
            cfg::Serverbound::ClientInformation(i) => info = i,
            cfg::Serverbound::CustomPayload(p) if p.channel == "minecraft:brand" => {
                brand = horizon_protocol::buf::Reader::new(&p.data)
                    .string(32767)
                    .unwrap_or(brand);
            }
            cfg::Serverbound::SelectKnownPacks(p) => break p.packs == vec![core.clone()],
            _ => {}
        }
    };
    // A Horizon client hears that this server runs its mods' quiet commands
    // (horizon_protocol::quiet); a vanilla one is told nothing.
    if brand == "horizon" {
        send(
            c,
            cfg::Clientbound::CustomPayload(cfg::CustomPayload {
                channel: horizon_protocol::quiet::CHANNEL.into(),
                data: Vec::new(),
            }),
        )?;
    }
    for packet in sync.registry_packets(known_pack) {
        send(c, cfg::Clientbound::RegistryData(packet))?;
    }
    send(c, cfg::Clientbound::UpdateTags(sync.tags.clone()))?;
    if let Some(pack) = &shared.config.resource_pack
        && let Some(rejected) = offer_pack(c, pack, &mut info)?
    {
        let (reason, text) = pack_rejected(shared, name, rejected);
        let _ = send(c, cfg::Clientbound::Disconnect(cfg::Disconnect { reason }));
        lost(shared, &format!("{name} ({})", uuid_string(uuid)), &text);
        return Ok(None);
    }
    send(c, cfg::Clientbound::FinishConfiguration(Empty))?;
    loop {
        let (id, payload) = recv_modern(c, shared, retained)?;
        match cfg::Serverbound::decode(id, &payload).map_err(e)? {
            cfg::Serverbound::FinishConfiguration(_) => break,
            cfg::Serverbound::ClientInformation(i) => info = i,
            _ => {}
        }
    }
    Ok(Some(Parts {
        uuid,
        name: name.to_owned(),
        properties,
        info,
        brand,
    }))
}

/// ServerboundResourcePackPacket.Action.DECLINED.
const PACK_DECLINED: i32 = 1;

/// ServerCommonPacketListenerImpl.handleResourcePackResponse: a player who
/// declined the required pack is disconnected. The reason and its text.
fn pack_rejected(shared: &Shared, name: &str, pack: u128) -> (horizon_world::nbt::Tag, String) {
    let reason = translatable("multiplayer.requiredTexturePrompt.disconnect", &[]);
    let Some(log) = &shared.config.log else {
        return (reason, String::new());
    };
    let line = format!(
        "Disconnecting {name} due to resource pack {} rejection",
        uuid_string(pack)
    );
    (log.write)("Server thread", "INFO", &line);
    let text = horizon_protocol::text::plain(&reason, &log.lang);
    (reason, text)
}

/// ServerResourcePackConfigurationTask: offer the server's pack, then wait
/// for the client's final answer (`Action.isTerminal`: anything but
/// ACCEPTED or DOWNLOADED). The player may take a while to answer the
/// prompt, so the connection is kept alive meanwhile, as
/// ServerConfigurationPacketListenerImpl.tick does. The id of a required
/// pack the client declined, which disconnects it.
fn offer_pack(
    c: &mut Connection,
    pack: &crate::server::properties::ResourcePackInfo,
    info: &mut ClientInformation,
) -> Result<Option<u128>, String> {
    let e = |e: horizon_protocol::Error| e.to_string();
    let push = cfg::ResourcePackPush {
        id: pack.id,
        url: pack.url.clone(),
        hash: pack.hash.clone(),
        required: pack.required,
        prompt: pack.prompt.as_ref().map(component_tag),
    };
    c.send(&cfg::Clientbound::ResourcePackPush(push).encode())
        .map_err(e)?;
    std::thread::scope(|scope| {
        let (stop, stopped) = channel::<()>();
        let writer = &mut c.writer;
        scope.spawn(move || {
            let start = Instant::now();
            while stopped.recv_timeout(KEEP_ALIVE_EVERY) == Err(RecvTimeoutError::Timeout) {
                let id = i64::try_from(start.elapsed().as_millis()).unwrap_or(0);
                let keep_alive = cfg::Clientbound::KeepAlive(cfg::KeepAlive { id });
                if writer.send(&keep_alive.encode()).is_err() {
                    break;
                }
            }
        });
        let answer = loop {
            let (id, payload) = match c.reader.recv() {
                Ok(packet) => packet,
                Err(err) => break Err(err.to_string()),
            };
            match cfg::Serverbound::decode(id, &payload) {
                Ok(cfg::Serverbound::ResourcePack(r)) if !matches!(r.action, 3 | 4) => {
                    let rejected = r.action == PACK_DECLINED && pack.required;
                    break Ok(rejected.then_some(r.id));
                }
                Ok(cfg::Serverbound::ClientInformation(i)) => *info = i,
                Ok(_) => {}
                Err(err) => break Err(err.to_string()),
            }
        };
        drop(stop);
        answer
    })
}

fn play(c: Connection, shared: &Arc<Shared>, parts: Parts, address: std::net::SocketAddr) {
    #[cfg(feature = "paired-packages")]
    if let Some(stage) = &shared.stage {
        let Some(conn) = STAGED_CONN.get() else {
            stage.fail(super::StagedFailure::Identity);
            return;
        };
        play_staged(c, shared, parts, address, conn, stage.clone());
        return;
    }
    let conn_id = shared.next_id();
    let events = shared.events.clone();
    let _ = c.reader.set_read_timeout(Some(KEEP_ALIVE_TIMEOUT));
    let (mut reader, writer) = c.split();
    let (tx, rx) = channel();
    let pending = Arc::new(AtomicUsize::new(0));
    let latency = Arc::new(AtomicI32::new(0));
    let alive = Arc::new(AtomicI64::new(-1));
    let sender = PlayerSender::new(tx.clone(), pending.clone());
    let writer_thread = {
        let (alive, latency, pending) = (alive.clone(), latency.clone(), pending.clone());
        std::thread::Builder::new()
            .name(format!("horizon-lan-out-{conn_id}"))
            .spawn(move || {
                // Ticks and frames come first (horizon_threads).
                horizon_threads::behind_server();
                write_loop(writer, rx, alive, latency, pending)
            })
            .ok()
    };
    let name = parts.name.clone();
    let joined = Joined {
        conn: conn_id,
        uuid: parts.uuid,
        name: parts.name,
        properties: parts.properties,
        info: parts.info,
        brand: parts.brand,
        sender,
        latency,
        address,
    };
    let required_pack = shared
        .config
        .resource_pack
        .as_ref()
        .is_some_and(|p| p.required);
    let reason = if events.send(NetEvent::Joined(Box::new(joined))).is_ok() {
        match read_loop(&mut reader, conn_id, &events, &alive, required_pack) {
            Ok(reason) => reason,
            Err(rejected) => {
                let (reason, text) = pack_rejected(shared, &name, rejected);
                let _ = tx.send(Out::Disconnect(reason));
                text
            }
        }
    } else {
        "server closed".to_owned()
    };
    let _ = tx.send(Out::Close);
    let _ = events.send(NetEvent::Left(conn_id, reason));
    if let Some(t) = writer_thread {
        let _ = t.join();
    }
    shared.release(parts.uuid);
}

#[cfg(feature = "paired-packages")]
struct StagedWriterOwner(Option<std::thread::JoinHandle<()>>);
#[cfg(feature = "paired-packages")]
impl Drop for StagedWriterOwner {
    fn drop(&mut self) {
        if self.0.is_some() {
            let _ = std::io::Write::write_all(
                &mut std::io::stderr(),
                b"fatal: unresolved staged LAN writer owner\n",
            );
            std::process::abort();
        }
    }
}

#[cfg(feature = "paired-packages")]
fn play_staged(
    c: Connection,
    shared: &Arc<Shared>,
    parts: Parts,
    address: std::net::SocketAddr,
    conn: u32,
    stage: Arc<super::StagedState>,
) {
    use super::StagedFailure;
    let pending = {
        let inner = stage.inner();
        let Some(attempt) = inner.attempts.iter().find(|a| a.conn == conn) else {
            drop(inner);
            stage.fail(StagedFailure::Identity);
            return;
        };
        attempt.pending.clone()
    };
    if !stage.bind_sender(conn, parts.uuid, &pending) {
        stage.fail(StagedFailure::Identity);
        return;
    }
    let (mut reader, writer) = c.split();
    let (tx, rx) = channel();
    // Keep the legacy public channel disconnected: it cannot bypass the staged
    // reservation path or inject an uncharged body into this writer.
    let (legacy, legacy_rx) = channel();
    drop(legacy_rx);
    let sender = PlayerSender {
        tx: legacy,
        pending: pending.clone(),
        held: Arc::default(),
        posted: Arc::default(),
        staged: Some(StagedSender {
            tx,
            stage: Arc::downgrade(&stage),
            pending: pending.clone(),
            conn,
        }),
        retained: None,
    };
    let latency = Arc::new(AtomicI32::new(0));
    let alive = Arc::new(AtomicI64::new(-1));
    let worker = {
        let stage = stage.clone();
        let alive = alive.clone();
        let latency = latency.clone();
        std::thread::Builder::new()
            .name(format!("horizon-staged-out-{conn}"))
            .spawn(move || {
                horizon_threads::behind_server();
                write_staged(writer, rx, alive, latency, pending, &stage);
            })
    };
    let Ok(worker) = worker else {
        stage.fail(StagedFailure::Writer);
        return;
    };
    let mut worker = StagedWriterOwner(Some(worker));
    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    if std::env::var("HORIZON_STAGED_WRITER_FATAL_CHILD").as_deref() == Ok("1") {
        panic!("intentional staged reader unwind with actual owned writer");
    }
    let joined = Joined {
        conn,
        uuid: parts.uuid,
        name: parts.name,
        properties: parts.properties,
        info: parts.info,
        brand: parts.brand,
        sender,
        latency,
        address,
    };
    if stage.emit(conn, NetEvent::Joined(Box::new(joined)), 0) {
        loop {
            let (id, payload) = match recv_staged(&mut reader, &stage) {
                Ok(packet) => packet,
                Err(_) => {
                    if !stage.closing.load(Ordering::Acquire) {
                        stage.fail(StagedFailure::ConnectionEnded);
                    }
                    break;
                }
            };
            // The native frame allocation has already occurred. This limit is
            // checked before play decoding and mailbox retention, not before IO.
            if payload.len() > 64 << 10 {
                stage.fail(StagedFailure::PayloadTooLarge);
                break;
            }
            let bytes = payload.len();
            match play::Serverbound::decode(id, &payload) {
                Ok(play::Serverbound::KeepAlive(k)) => alive.store(k.id, Ordering::Relaxed),
                Ok(packet) => {
                    if !stage.emit(conn, NetEvent::Packet(conn, Box::new(packet)), bytes) {
                        break;
                    }
                }
                Err(_) => {
                    stage.fail(StagedFailure::Decode);
                    break;
                }
            }
        }
    }
    // Force native close before waiting for writer completion. This closes all
    // attempts, including readers still inside login/configuration frames.
    stage.close();
    while !worker
        .0
        .as_ref()
        .expect("owned staged writer")
        .is_finished()
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    if worker
        .0
        .take()
        .expect("finished staged writer")
        .join()
        .is_err()
    {
        stage.fail(StagedFailure::ThreadPanic);
    }
    shared.release(parts.uuid);
}

#[cfg(feature = "paired-packages")]
fn write_staged(
    mut writer: ConnWriter,
    rx: Receiver<StagedBody>,
    alive: Arc<AtomicI64>,
    latency: Arc<AtomicI32>,
    pending: Arc<AtomicUsize>,
    stage: &super::StagedState,
) {
    use super::StagedFailure;
    let start = Instant::now();
    let mut sent_at: Option<(i64, Instant)> = None;
    let mut next = Instant::now() + KEEP_ALIVE_EVERY;
    while !stage.closing.load(Ordering::Acquire) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(body) => {
                let result = send_body(&mut writer, &body.body);
                // Drop releases the queue AND active-write reservation together.
                drop(body);
                if result.is_err() {
                    stage.fail(StagedFailure::Writer);
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                stage.fail(StagedFailure::Writer);
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        let now = Instant::now();
        if let Some((id, at)) = sent_at {
            if alive.load(Ordering::Relaxed) == id {
                latency.store(
                    i32::try_from(at.elapsed().as_millis()).unwrap_or(i32::MAX),
                    Ordering::Relaxed,
                );
                sent_at = None;
            } else if at.elapsed() > KEEP_ALIVE_TIMEOUT {
                stage.fail(StagedFailure::ConnectionEnded);
                break;
            }
        }
        if sent_at.is_none() && now >= next {
            let id = i64::try_from(start.elapsed().as_millis()).unwrap_or(0);
            let Some(body) = StagedBody::reserve(
                play::Clientbound::KeepAlive(cfg::KeepAlive { id }).encode(),
                pending.clone(),
                stage.outbound.clone(),
            ) else {
                stage.fail(StagedFailure::OutboundOverflow);
                break;
            };
            let result = writer.send(&body.body);
            drop(body);
            if result.is_err() {
                stage.fail(StagedFailure::Writer);
                break;
            }
            sent_at = Some((id, now));
            next = now + KEEP_ALIVE_EVERY;
        }
    }
    writer.shutdown();
    drop(rx);
}

/// Forward the player's packets until the connection ends: why it did, or
/// the id of a required resource pack the player declined (with
/// `required_pack`), which disconnects them.
fn read_loop(
    reader: &mut ConnReader,
    conn: u32,
    events: &Sender<NetEvent>,
    alive: &AtomicI64,
    required_pack: bool,
) -> Result<String, u128> {
    loop {
        let (id, payload) = match reader.recv() {
            Ok(p) => p,
            // The client closed the connection: Connection.channelInactive
            // without details, multiplayer.disconnect.generic.
            Err(e)
                if e.0 == "Connection closed by the server"
                    || e.0.contains("reset by peer")
                    || e.0.contains("aborted") =>
            {
                return Ok("Disconnected".to_owned());
            }
            Err(e) => return Ok(e.to_string()),
        };
        match play::Serverbound::decode(id, &payload) {
            Ok(play::Serverbound::KeepAlive(k)) => alive.store(k.id, Ordering::Relaxed),
            Ok(play::Serverbound::ResourcePack(r))
                if r.action == PACK_DECLINED && required_pack =>
            {
                return Err(r.id);
            }
            Ok(p) => {
                if events.send(NetEvent::Packet(conn, Box::new(p))).is_err() {
                    return Ok("server closed".to_owned());
                }
            }
            // A malformed packet disconnects like vanilla's decoder exception.
            Err(e) => return Ok(format!("Invalid packet {id}: {e}")),
        }
    }
}

/// Packet bodies at least this large are compressed once for every
/// connection: chunk columns, mostly, which every player near them receives.
const SHARED_FRAME_MIN: usize = 8 << 10;
/// The compressed frames kept, in bytes; the oldest go first.
const SHARED_FRAME_BYTES: usize = 32 << 20;

/// Compressed frame contents by the body they were made from: its length,
/// two independently keyed 64-bit hashes of it, and the threshold.
struct SharedFrames {
    hashers: (std::hash::RandomState, std::hash::RandomState),
    frames: Mutex<FrameCache>,
    /// [`SHARED_FRAME_BYTES`].
    limit: usize,
}
type FrameKey = (usize, u64, u64, usize);
#[derive(Default)]
struct FrameCache {
    frames: crate::fast_hash::FxHashMap<FrameKey, Arc<[u8]>>,
    order: std::collections::VecDeque<FrameKey>,
    bytes: usize,
}
impl SharedFrames {
    fn get() -> &'static Self {
        static FRAMES: std::sync::OnceLock<SharedFrames> = std::sync::OnceLock::new();
        FRAMES.get_or_init(|| Self::new(SHARED_FRAME_BYTES))
    }
    fn new(limit: usize) -> Self {
        Self {
            hashers: Default::default(),
            frames: Mutex::default(),
            limit,
        }
    }
    /// `frame::contents(body, Some(threshold))`, compressed only when no
    /// connection has sent the same body lately.
    fn contents(&self, body: &[u8], threshold: usize) -> horizon_protocol::Result<Arc<[u8]>> {
        use std::hash::BuildHasher;
        let key = (
            body.len(),
            self.hashers.0.hash_one(body),
            self.hashers.1.hash_one(body),
            threshold,
        );
        let cache = || self.frames.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(contents) = cache().frames.get(&key) {
            return Ok(contents.clone());
        }
        let contents: Arc<[u8]> = horizon_protocol::frame::contents(body, Some(threshold))?.into();
        let mut cache = cache();
        if cache.frames.insert(key, contents.clone()).is_none() {
            cache.order.push_back(key);
            cache.bytes += contents.len();
            while cache.bytes > self.limit
                && let Some(old) = cache.order.pop_front()
            {
                if let Some(c) = cache.frames.remove(&old) {
                    cache.bytes -= c.len();
                }
            }
        }
        Ok(contents)
    }
}

/// Send one packet body: a large one compressed through [`SharedFrames`],
/// so a chunk column sent to fifty players is compressed once, not fifty
/// times. The bytes on the wire are the same.
fn send_body(writer: &mut ConnWriter, body: &[u8]) -> horizon_protocol::Result<()> {
    match writer.threshold {
        Some(t) if body.len() >= t.max(SHARED_FRAME_MIN) => {
            writer.send_contents(&SharedFrames::get().contents(body, t)?)
        }
        _ => writer.send(body),
    }
}

/// Retained preparation is separate from the actual native write. Ordinary and
/// staged sends retain their original send_body path and failure behavior.
fn prepare_retained_body(writer: &ConnWriter, body: &[u8]) -> horizon_protocol::Result<Arc<[u8]>> {
    match writer.threshold {
        Some(t) if body.len() >= t.max(SHARED_FRAME_MIN) => SharedFrames::get().contents(body, t),
        _ => horizon_protocol::frame::contents(body, writer.threshold).map(Arc::from),
    }
}

fn write_loop(
    mut writer: ConnWriter,
    rx: Receiver<Out>,
    alive: Arc<AtomicI64>,
    latency: Arc<AtomicI32>,
    pending: Arc<AtomicUsize>,
) {
    let start = Instant::now();
    let mut sent_at: Option<(i64, Instant)> = None;
    let mut next = Instant::now() + KEEP_ALIVE_EVERY;
    loop {
        let mut write = |body: Vec<u8>| {
            pending.fetch_sub(body.len(), Ordering::Relaxed);
            send_body(&mut writer, &body).is_ok() && pending.load(Ordering::Relaxed) <= MAX_PENDING
        };
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Out::Packet(body)) => {
                if !write(body) {
                    break;
                }
            }
            Ok(Out::Packets(bodies)) => {
                if !bodies.into_iter().all(&mut write) {
                    break;
                }
            }
            Ok(Out::Disconnect(reason)) => {
                let _ = writer
                    .send(&play::Clientbound::Disconnect(cfg::Disconnect { reason }).encode());
                std::thread::sleep(Duration::from_millis(50));
                break;
            }
            Ok(Out::Close) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let now = Instant::now();
        if let Some((id, at)) = sent_at {
            if alive.load(Ordering::Relaxed) == id {
                let ms = i32::try_from(at.elapsed().as_millis()).unwrap_or(i32::MAX);
                let old = latency.load(Ordering::Relaxed);
                latency.store((old * 3 + ms) / 4, Ordering::Relaxed);
                sent_at = None;
            } else if at.elapsed() > KEEP_ALIVE_TIMEOUT {
                let _ = writer.send(
                    &play::Clientbound::Disconnect(cfg::Disconnect {
                        reason: translatable("disconnect.timeout", &[]),
                    })
                    .encode(),
                );
                break;
            }
        }
        if sent_at.is_none() && now >= next {
            let id = i64::try_from(start.elapsed().as_millis()).unwrap_or(0);
            if writer
                .send(&play::Clientbound::KeepAlive(cfg::KeepAlive { id }).encode())
                .is_err()
            {
                break;
            }
            sent_at = Some((id, now));
            next = now + KEEP_ALIVE_EVERY;
        }
    }
    writer.shutdown();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_become_the_components_vanilla_sends() {
        let tag = |json: &str| {
            let tag = component_tag(&crate::server::json::parse(json, true).unwrap());
            component_json(&tag)
        };
        assert_eq!(tag(r#"{"text":"Hi"}"#), r#""Hi""#);
        assert_eq!(tag(r#""Hi""#), r#""Hi""#);
        assert_eq!(
            tag(r#"["Hi ",{"text":"there","bold":true}]"#),
            r#"{"text":"Hi ","extra":[{"text":"there","bold":true}]}"#
        );
        assert_eq!(
            tag(r#"{"translate":"a","with":["b",{"text":"c","color":"red"}]}"#),
            r#"{"translate":"a","with":["b",{"text":"c","color":"red"}]}"#
        );
    }

    fn unpack(out: Out) -> Vec<Vec<u8>> {
        match out {
            Out::Packet(body) => vec![body],
            Out::Packets(bodies) => bodies,
            Out::Disconnect(_) => vec![b"disconnect".to_vec()],
            Out::Close => vec![b"close".to_vec()],
        }
    }

    fn bodies(rx: &Receiver<Out>) -> Vec<Vec<Vec<u8>>> {
        rx.try_iter().map(unpack).collect()
    }

    #[test]
    fn packets_sent_after_a_posted_tick_follow_it() {
        let (tx, rx) = channel();
        let (tx2, rx2) = channel();
        let sender = PlayerSender::new(tx, Arc::default());
        let other = PlayerSender::new(tx2, Arc::default());
        for round in 0..50u8 {
            let holding = Holding::start();
            sender.send(vec![round, 1]);
            sender.send(vec![round, 2]);
            other.send(vec![round]);
            drop(holding);
            flush_all([&sender, &other]);
            // Straight after the tick, while the post thread may still hold it.
            sender.send(vec![round, 3]);
            let next = || unpack(rx.recv_timeout(Duration::from_secs(10)).unwrap());
            assert_eq!(next(), [vec![round, 1], vec![round, 2]]);
            assert_eq!(next(), [vec![round, 3]]);
            let got = unpack(rx2.recv_timeout(Duration::from_secs(10)).unwrap());
            assert_eq!(got, [vec![round]]);
        }
        let holding = Holding::start();
        sender.send(vec![9]);
        drop(holding);
        flush_all([&sender]);
        sender.disconnect(horizon_world::nbt::Tag::Byte(0));
        let next = || unpack(rx.recv_timeout(Duration::from_secs(10)).unwrap());
        assert_eq!(next(), [vec![9]]);
        assert_eq!(next(), [b"disconnect".to_vec()]);
        // Nothing sent twice, nothing left with the post thread.
        assert!(rx.try_recv().is_err());
        let deadline = Instant::now() + Duration::from_secs(10);
        while sender.posted.load(Ordering::Acquire) != 0 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
    }

    #[test]
    fn shared_frames_are_the_frames_compressed_here() {
        use horizon_protocol::frame;
        let frames = SharedFrames::new(usize::MAX);
        let a: Vec<u8> = (0..40_000u32).map(|i| (i * 7 % 13) as u8).collect();
        let mut b = a.clone();
        b[20_000] ^= 1;
        for (body, threshold) in [(&a, 256), (&b, 256), (&a, 256), (&a, 64), (&b, 256)] {
            let shared = frames.contents(body, threshold).unwrap();
            assert_eq!(*shared, *frame::contents(body, Some(threshold)).unwrap());
        }
        // One frame per body and threshold.
        assert_eq!(frames.frames.lock().unwrap().frames.len(), 3);
    }

    #[test]
    fn shared_frames_keep_the_newest_within_their_limit() {
        let first = SharedFrames::new(0)
            .contents(&[1; 9000], 256)
            .unwrap()
            .len();
        let frames = SharedFrames::new(first * 2);
        for n in 0..5u8 {
            frames.contents(&[n; 9000], 256).unwrap();
        }
        let cache = frames.frames.lock().unwrap();
        assert!((1..=2).contains(&cache.frames.len()));
        assert_eq!(cache.order.len(), cache.frames.len());
        assert_eq!(
            cache.bytes,
            cache.frames.values().map(|c| c.len()).sum::<usize>()
        );
        assert!(cache.bytes <= first * 2);
    }

    #[test]
    fn held_packets_go_together_and_in_order() {
        let (tx, rx) = channel();
        let sender = PlayerSender::new(tx, Arc::default());
        sender.send(vec![1]);
        assert_eq!(bodies(&rx), [vec![vec![1]]]);
        let holding = Holding::start();
        sender.send(vec![2]);
        sender.send(vec![3]);
        assert!(bodies(&rx).is_empty());
        drop(holding);
        // A packet sent after the tick still follows the held ones.
        sender.send(vec![4]);
        assert_eq!(bodies(&rx), [vec![vec![2], vec![3]], vec![vec![4]]]);
        let holding = Holding::start();
        sender.send(vec![5]);
        sender.disconnect(horizon_world::nbt::Tag::Byte(0));
        drop(holding);
        sender.flush();
        assert_eq!(bodies(&rx), [vec![vec![5]], vec![b"disconnect".to_vec()]]);
        assert_eq!(sender.pending.load(Ordering::Relaxed), 5);
    }
}

#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
mod retained_transport_tests {
    use super::super::{ClosedLan, RetainedActor, RetainedLan, RetainedOfflineConfig};
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn retained_refused_outbound_keeps_original_vectors_and_first_failure() {
        for mode in ["closing", "sealed", "disconnecting", "capacity"] {
            let latch = Arc::new(AtomicBool::new(false));
            let queue = RetainedOutbound::new(latch.clone());
            let sender = queue.sender();
            match mode {
                "closing" => latch.store(true, Ordering::Release),
                "sealed" => queue.seal(),
                "disconnecting" => {
                    sender.disconnect(horizon_world::nbt::Tag::String("stop".into()))
                }
                _ => {}
            }
            let body = vec![
                0xa7;
                if mode == "capacity" {
                    MAX_PENDING + 1
                } else {
                    7
                }
            ];
            let pointer = body.as_ptr();
            let capacity = body.capacity();
            sender.send(body);
            let first = queue.snapshot();
            let failure = first.failure.clone().expect("refusal is visible");
            let stored = queue.inner();
            assert_eq!(stored.refused.len(), 1);
            assert_eq!(stored.refused[0].reason, mode);
            assert_eq!(stored.refused[0].bodies[0].as_ptr(), pointer);
            assert_eq!(stored.refused[0].bodies[0].capacity(), capacity);
            assert!(stored.refused[0].bodies[0].iter().all(|b| *b == 0xa7));
            drop(stored);
            queue.failed(RetainedOutcome::Io("later native failure".into()));
            sender.send(vec![0xb2, 0x19]);
            let later = queue.snapshot();
            assert_eq!(later.failure, Some(failure));
            assert_eq!(later.first_refusal, Some(mode));
            assert_eq!(later.refused_bodies, 2);
            assert_eq!(later.refused_bytes, first.refused_bytes + 2);
            assert_eq!(queue.inner().refused[0].bodies[0].as_ptr(), pointer);
            assert_eq!(queue.inner().refused[1].bodies[0], [0xb2, 0x19]);
        }
    }

    struct Fixture(Option<RetainedLan>);
    impl Fixture {
        fn new() -> Self {
            Self::with_compression(-1)
        }
        fn with_compression(compression_threshold: i32) -> Self {
            retained_controls::set(Default::default());
            Self(Some(
                RetainedLan::new(
                    super::super::retained_test_sync(),
                    RetainedOfflineConfig {
                        bind: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
                        port: 0,
                        motd: "retained native test".into(),
                        max_players: 4,
                        compression_threshold,
                        enable_status: true,
                        hide_online_players: false,
                        announce: false,
                    },
                )
                .expect("valid retained fixture"),
            ))
        }
        fn lan(&mut self) -> &mut RetainedLan {
            self.0.as_mut().unwrap()
        }
        fn pipe(&mut self) -> Connection {
            let c = Connection::local(self.lan().connect_local().expect("actual native Pipe"));
            c.reader
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            c
        }
        fn tcp(&mut self) -> Connection {
            let port = self.lan().listen().expect("actual native listener");
            let c = Connection::from_stream(
                TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
                    .expect("actual loopback TCP"),
            )
            .unwrap();
            c.reader
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            c
        }
        fn wait(&mut self, predicate: impl Fn(&super::super::RetainedObservation) -> bool) -> bool {
            let until = Instant::now() + Duration::from_secs(5);
            loop {
                self.lan().poll_close();
                if predicate(&self.lan().observe()) {
                    return true;
                }
                if Instant::now() >= until {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        fn joined(&mut self) -> Option<Joined> {
            if !self.wait(|o| o.queued_events > 0) {
                return None;
            }
            self.lan()
                .poll_events(16)
                .into_iter()
                .find_map(|event| match event {
                    NetEvent::Joined(joined) => Some(*joined),
                    _ => None,
                })
        }
        fn close(&mut self) -> ClosedLan {
            retained_controls::release();
            self.lan().begin_close(Duration::from_secs(5));
            let completed = self.wait(|o| o.quiescent);
            assert!(completed, "actual retained native actors did not finish");
            match self.0.take().unwrap().try_closed() {
                Ok(closed) => closed,
                Err(owner) => {
                    self.0 = Some(owner);
                    panic!("quiescence failed checked consume");
                }
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            retained_controls::release();
            if self.0.is_some() {
                let _ = self.close();
            }
            retained_controls::set(Default::default());
        }
    }

    // This exercises every ordinary login/configuration encoder, including the
    // real prepared registry/tag data, rather than an abbreviated play fixture.
    fn login_client(c: &mut Connection, name: &str) -> Result<usize, String> {
        let e = |e: horizon_protocol::Error| e.to_string();
        c.send(
            &handshake::Serverbound::Intention(handshake::Intention {
                protocol: PROTOCOL_VERSION,
                host: "localhost".into(),
                port: 0,
                intent: handshake::LOGIN,
            })
            .encode(),
        )
        .map_err(e)?;
        c.send(
            &login::Serverbound::Hello(login::ServerHello {
                name: name.into(),
                uuid: 0,
            })
            .encode(),
        )
        .map_err(e)?;
        loop {
            let (id, body) = c.recv().map_err(e)?;
            match login::Clientbound::decode(id, &body).map_err(e)? {
                login::Clientbound::Compression(p) => c.set_compression(p.threshold),
                login::Clientbound::LoginFinished(p) => {
                    if p.profile.uuid != offline_uuid(name) {
                        return Err("incorrect offline UUID".into());
                    }
                    break;
                }
                other => return Err(format!("unexpected login packet {other:?}")),
            }
        }
        c.send(&login::Serverbound::LoginAcknowledged(Empty).encode())
            .map_err(e)?;
        let mut registries = 0;
        let mut tags = false;
        loop {
            let (id, body) = c.recv().map_err(e)?;
            match cfg::Clientbound::decode(id, &body).map_err(e)? {
                cfg::Clientbound::SelectKnownPacks(_) => {
                    c.send(
                        &cfg::Serverbound::SelectKnownPacks(cfg::SelectKnownPacks {
                            packs: Vec::new(),
                        })
                        .encode(),
                    )
                    .map_err(e)?;
                }
                cfg::Clientbound::RegistryData(_) => registries += 1,
                cfg::Clientbound::UpdateTags(_) => tags = true,
                cfg::Clientbound::FinishConfiguration(_) => {
                    if registries == 0 || !tags {
                        return Err("missing real configuration data".into());
                    }
                    c.send(&cfg::Serverbound::FinishConfiguration(Empty).encode())
                        .map_err(e)?;
                    return Ok(registries);
                }
                cfg::Clientbound::Disconnect(_) => return Err("configuration refused".into()),
                _ => {}
            }
        }
    }
    fn actor(
        closed: &ClosedLan,
        kind: RetainedActor,
        outcome: impl Fn(&RetainedOutcome) -> bool,
    ) -> bool {
        closed
            .observation
            .terminals
            .iter()
            .any(|t| t.actor == kind && outcome(&t.outcome))
    }

    #[test]
    fn retained_native_tcp_login_keeps_joined_and_packet_events() {
        let mut fixture = Fixture::new();
        let mut client = fixture.tcp();
        let login = login_client(&mut client, "RetainedTcp");
        // Unknown play packets still use the real decoder and mailbox route.
        let packet = client.send(&[0xff, 0x7f, 19]);
        let queued = fixture.wait(|o| o.queued_events >= 2);
        let closed = fixture.close();
        assert!(login.unwrap() > 0);
        packet.unwrap();
        assert!(queued);
        assert!(
            closed
                .events
                .iter()
                .any(|e| matches!(e, NetEvent::Joined(_)))
        );
        assert!(
            closed
                .events
                .iter()
                .any(|e| matches!(e, NetEvent::Packet(_, _)))
        );
        assert!(
            closed
                .observation
                .attempts
                .iter()
                .all(|a| a.released && !a.outer_pending && !a.writer_pending)
        );
        assert!(actor(&closed, RetainedActor::Writer, |_| true));
    }

    #[test]
    fn retained_native_pipe_login_and_eof_keep_unconsumed_joined() {
        let mut fixture = Fixture::new();
        let mut client = fixture.pipe();
        let login = login_client(&mut client, "RetainedPipe");
        let joined = fixture.wait(|o| o.queued_events > 0);
        drop(client);
        let ended = fixture.wait(|o| o.terminals.iter().any(|t| t.actor == RetainedActor::Outer));
        let closed = fixture.close();
        assert!(login.unwrap() > 0 && joined && ended);
        assert!(
            closed
                .events
                .iter()
                .any(|e| matches!(e, NetEvent::Joined(_)))
        );
        assert!(actor(&closed, RetainedActor::Outer, |o| matches!(
            o,
            RetainedOutcome::Eof
        )));
    }

    #[test]
    fn retained_direct_sender_flush_order_and_sealed_held_custody() {
        let mut fixture = Fixture::new();
        let mut client = fixture.pipe();
        let login = login_client(&mut client, "DirectOrder");
        let joined = fixture.joined();
        let Some(joined) = joined else {
            fixture.close();
            panic!("native Joined missing");
        };
        let sender = joined.sender;
        let before_post = retained_controls::POST_CALLS.load(Ordering::Acquire);
        sender.send(vec![1, 1]);
        let holding = Holding::start();
        sender.send_many([vec![1, 2], vec![1, 3]]);
        drop(holding);
        flush_all([&sender]);
        sender.send(vec![1, 4]);
        let packets: Vec<_> = (0..4).map(|_| client.recv()).collect();
        retained_controls::set(retained_controls::Control {
            conn: joined.conn,
            pause_writer: true,
            ..Default::default()
        });
        sender.send(vec![1, 5]);
        let writer_entered = fixture.wait(|_| retained_controls::counts().0 > 0);
        let holding = Holding::start();
        sender.send(vec![1, 6]);
        drop(holding);
        let public_disconnected = sender.tx.send(Out::Packet(vec![1, 99])).is_err();
        let latched = Arc::new(std::sync::Barrier::new(2));
        let resume = Arc::new(std::sync::Barrier::new(2));
        fixture
            .lan()
            .retained_test_close_gate(latched.clone(), resume.clone());
        let mut owner = fixture.0.take().unwrap();
        let closer = std::thread::spawn(move || {
            owner.begin_close(Duration::from_secs(5));
            owner
        });
        latched.wait();
        // The actual writer is paused with its active payload. Native queue
        // sealing has not happened, yet the irreversible registry latch rejects
        // every producer and leaves the already held body in original custody.
        let queue = sender.retained.as_ref().unwrap();
        let unsealed_in_gap = !queue.inner().sealed;
        let before_gap = queue.snapshot();
        sender.send(vec![1, 70]);
        sender.send_many([vec![1, 71], vec![1, 72]]);
        sender.flush();
        flush_all([&sender]);
        sender.deliver(Out::Packet(vec![1, 73]));
        sender.deliver(Out::Packets(vec![vec![1, 74]]));
        sender.disconnect(horizon_world::nbt::Tag::String("already closing".into()));
        let after_gap = queue.snapshot();
        resume.wait();
        fixture.0 = Some(
            closer
                .join()
                .expect("actual close coordinator returned original owner"),
        );
        let closed = fixture.close();
        sender.send(vec![1, 75]);
        sender.flush();
        flush_all([&sender]);
        let status = sender.retained.as_ref().unwrap().snapshot();
        let after_post = retained_controls::POST_CALLS.load(Ordering::Acquire);
        login.unwrap();
        assert!(public_disconnected);
        assert!(writer_entered && unsealed_in_gap);
        assert!(before_gap.sealed && after_gap.sealed);
        assert_eq!(before_gap.queued_bodies, after_gap.queued_bodies);
        assert_eq!(before_gap.queued_bytes, after_gap.queued_bytes);
        assert_eq!(after_gap.queued_bodies, 1);
        assert_eq!(after_gap.active_bodies, 1);
        assert_eq!(
            packets.into_iter().map(|p| p.unwrap()).collect::<Vec<_>>(),
            [(1, vec![1]), (1, vec![2]), (1, vec![3]), (1, vec![4])]
        );
        assert_eq!(before_post, after_post);
        assert_eq!(status.queued_bodies, 1);
        assert_eq!(status.queued_bytes, 2);
        assert_eq!(status.written_bodies, 4);
        assert_eq!(status.failed_bodies, 1);
        assert!(status.sealed && closed.observation.quiescent);

        // The disconnect route flushes held packets into the same direct queue,
        // then closes only after its own native write has succeeded.
        let mut fixture = Fixture::new();
        let mut client = fixture.pipe();
        let login = login_client(&mut client, "DirectDisconnect");
        let joined = fixture.joined();
        let Some(joined) = joined else {
            fixture.close();
            panic!("native Joined missing");
        };
        let before_post = retained_controls::POST_CALLS.load(Ordering::Acquire);
        let holding = Holding::start();
        joined.sender.send(vec![1, 7]);
        joined
            .sender
            .disconnect(horizon_world::nbt::Tag::String("closed".into()));
        drop(holding);
        let first = client.recv();
        let disconnect = client
            .recv()
            .and_then(|(id, body)| play::Clientbound::decode(id, &body));
        let closed = fixture.close();
        login.unwrap();
        assert_eq!(first.unwrap(), (1, vec![7]));
        assert!(matches!(
            disconnect.unwrap(),
            play::Clientbound::Disconnect(_)
        ));
        assert_eq!(
            before_post,
            retained_controls::POST_CALLS.load(Ordering::Acquire)
        );
        assert!(actor(&closed, RetainedActor::Writer, |o| matches!(
            o,
            RetainedOutcome::Closed
        )));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn retained_blocked_actual_tcp_writer_is_interrupted_with_payload_custody() {
        use std::io::Read;
        let mut fixture = Fixture::new();
        let mut observer: Option<TcpStream> = None;
        let precondition = (|| -> Result<(Joined, Arc<[u8]>), String> {
            fixture.lan().retained_test_socket_send_buffer(4096);
            let port = fixture.lan().listen().map_err(|e| e.to_string())?;
            let socket = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
                .map_err(|e| e.to_string())?;
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .map_err(|e| e.to_string())?;
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .map_err(|e| e.to_string())?;
            rustix::net::sockopt::set_socket_recv_buffer_size(&socket, 4096)
                .map_err(|e| e.to_string())?;
            let recv_bytes = rustix::net::sockopt::socket_recv_buffer_size(&socket)
                .map_err(|e| e.to_string())?;
            observer = Some(socket.try_clone().map_err(|e| e.to_string())?);
            let mut client = Connection::from_stream(socket).map_err(|e| e.to_string())?;
            login_client(&mut client, "BlockedWriter")?;
            let joined = fixture.joined().ok_or("native Joined missing")?;
            let send_bytes = fixture
                .lan()
                .retained_test_socket_buffer()?
                .ok_or("accepted native SO_SNDBUF observation missing")?;
            retained_controls::set(retained_controls::Control {
                conn: joined.conn,
                ..Default::default()
            });
            let marker = b"s1tl1-first-native-active-frame-unique-v1";
            let channel = "horizon:retained_writer_probe";
            let mut data = vec![0xa5; 1_999_900];
            data[..marker.len()].copy_from_slice(marker);
            let body = play::Clientbound::CustomPayload(cfg::CustomPayload {
                channel: channel.into(),
                data,
            })
            .encode();
            let expected_prefix = &body[..128];
            let capacity = send_bytes
                .checked_add(recv_bytes)
                .and_then(|n| n.checked_add(131))
                .and_then(|n| n.checked_mul(8))
                .ok_or("native buffer capacity arithmetic overflow")?;
            if send_bytes == 0 || recv_bytes == 0 || capacity >= body.len() {
                return Err(format!(
                    "native buffers too large for defining frame: send={send_bytes}, recv={recv_bytes}, frame={}",
                    body.len()
                ));
            }
            joined.sender.send(body.clone());
            if !fixture.wait(|_| retained_controls::counts().0 > 0) {
                return Err("original native writer did not enter first frame write".into());
            }
            let queue = joined.sender.retained.as_ref().unwrap();
            let original_active = queue
                .inner()
                .active
                .clone()
                .ok_or("first active body missing")?;
            if original_active.as_ref() != body.as_slice() {
                return Err("first active body differs from unique defining frame".into());
            }
            // This clone reads only a bounded prefix of the SAME first frame.
            // Actual length/id/channel/marker bytes establish native progress;
            // effective buffers plus the prefix cannot accommodate its remainder.
            let peer = observer.as_mut().unwrap();
            let mut length = 0usize;
            let mut prefix_done = false;
            for shift in [0, 7, 14] {
                let mut byte = [0u8; 1];
                peer.read_exact(&mut byte).map_err(|e| e.to_string())?;
                length |= usize::from(byte[0] & 0x7f) << shift;
                if byte[0] & 0x80 == 0 {
                    prefix_done = true;
                    break;
                }
            }
            if !prefix_done || length != body.len() {
                return Err("incorrect first native frame length".into());
            }
            let mut prefix = [0u8; 128];
            peer.read_exact(&mut prefix).map_err(|e| e.to_string())?;
            if prefix.as_slice() != expected_prefix {
                return Err("native first-frame prefix mismatch".into());
            }
            let mut r = horizon_protocol::buf::Reader::new(&prefix);
            if r.varint().map_err(|e| e.to_string())?
                != horizon_protocol::ids::play::clientbound::CUSTOM_PAYLOAD
                || r.string(32767).map_err(|e| e.to_string())? != channel
                || r.bytes(marker.len()).map_err(|e| e.to_string())? != marker
            {
                return Err("native first-frame identity mismatch".into());
            }
            let current = queue
                .inner()
                .active
                .clone()
                .ok_or("first active body disappeared")?;
            if !Arc::ptr_eq(&original_active, &current)
                || retained_controls::counts() != (1, 0)
                || fixture.lan().retained_test_writer_finished(joined.conn) != Some(false)
                || queue.snapshot().failed_bodies != 0
            {
                return Err(
                    "same first native write was not observed unfinished before interruption"
                        .into(),
                );
            }
            Ok((joined, original_active))
        })();
        // Cleanup cannot establish the passing precondition: all observations
        // above occurred before actual native interruption and checked reap.
        fixture.lan().begin_close(Duration::from_secs(5));
        if let Some(peer) = observer.take() {
            let _ = peer.shutdown(std::net::Shutdown::Both);
            drop(peer);
        }
        let closed = fixture.close();
        let (joined, original_active) =
            precondition.expect("actual first native write precondition");
        let queue = joined.sender.retained.as_ref().unwrap();
        let status = queue.snapshot();
        assert_eq!(status.active_bodies, 1);
        assert_eq!(status.failed_bodies, 1);
        assert_eq!(status.queued_bodies, 0);
        assert!(status.sealed);
        assert!(Arc::ptr_eq(
            &original_active,
            queue.inner().active.as_ref().unwrap()
        ));
        assert!(actor(&closed, RetainedActor::Writer, |o| matches!(
            o,
            RetainedOutcome::Io(_)
        )));
    }

    #[test]
    fn retained_codec_receive_and_oversized_encode_preserve_original_failures() {
        let mut fixture = Fixture::with_compression(1);
        let mut client = fixture.tcp();
        let login = login_client(&mut client, "MalformedCodec");
        let joined = fixture.wait(|o| o.queued_events > 0);
        // Declares eight uncompressed bytes, but contains the complete valid
        // zlib encoding of an empty body. This reaches the actual decompressor.
        let malformed = [8, 0x78, 0x9c, 3, 0, 0, 0, 0, 1];
        let sent = client.writer.send_contents(&malformed);
        let ended = fixture.wait(|o| o.terminals.iter().any(|t| t.actor == RetainedActor::Outer));
        let closed = fixture.close();
        drop(client);
        login.unwrap();
        sent.unwrap();
        assert!(joined && ended);
        assert!(actor(&closed, RetainedActor::Outer, |o| matches!(o,
            RetainedOutcome::Receive(text)
                if text == "Badly compressed packet - actual length 0 does not match declared size 8")));
        assert!(!actor(&closed, RetainedActor::Outer, |o| matches!(
            o,
            RetainedOutcome::Io(_) | RetainedOutcome::Protocol(_)
        )));
        assert!(
            closed
                .events
                .iter()
                .any(|e| matches!(e, NetEvent::Joined(_)))
        );
        assert!(actor(&closed, RetainedActor::Writer, |_| true));
        assert!(
            closed
                .observation
                .attempts
                .iter()
                .all(|a| a.released && !a.outer_pending && !a.writer_pending)
        );

        let mut fixture = Fixture::new();
        let mut client = fixture.pipe();
        let login = login_client(&mut client, "OversizedEncode");
        let joined = fixture.joined();
        let Some(joined) = joined else {
            fixture.close();
            panic!("native Joined missing");
        };
        retained_controls::set(retained_controls::Control {
            conn: joined.conn,
            ..Default::default()
        });
        let bytes = horizon_protocol::frame::MAX_FRAME + 1;
        joined.sender.send(vec![1; bytes]);
        let ended = fixture.wait(|o| o.terminals.iter().any(|t| t.actor == RetainedActor::Writer));
        let closed = fixture.close();
        drop(client);
        let queue = joined.sender.retained.as_ref().unwrap();
        let status = queue.snapshot();
        login.unwrap();
        assert!(ended);
        assert!(actor(&closed, RetainedActor::Writer, |o| matches!(o,
            RetainedOutcome::Encode(text) if text == &format!("packet too large: {bytes} bytes"))));
        assert!(!actor(&closed, RetainedActor::Writer, |o| matches!(
            o,
            RetainedOutcome::Io(_)
        )));
        assert_eq!(retained_controls::counts(), (0, 0));
        assert_eq!(status.active_bytes, bytes);
        assert_eq!(status.failed_bodies, 1);
        assert_eq!(status.written_bodies, 0);
        assert_eq!(queue.inner().active.as_ref().unwrap().len(), bytes);
        assert!(actor(&closed, RetainedActor::Outer, |_| true));
        assert!(
            closed
                .observation
                .attempts
                .iter()
                .all(|a| a.released && !a.outer_pending && !a.writer_pending)
        );
    }

    #[test]
    fn retained_actual_writer_panic_retains_active_body_and_both_join_outcomes() {
        let mut fixture = Fixture::new();
        let mut client = fixture.pipe();
        let login = login_client(&mut client, "WriterPanic");
        let joined = fixture.joined();
        let Some(joined) = joined else {
            fixture.close();
            panic!("native Joined missing");
        };
        retained_controls::set(retained_controls::Control {
            conn: joined.conn,
            panic_writer: true,
            ..Default::default()
        });
        joined.sender.send(vec![1, 71]);
        let ended = fixture.wait(|o| o.terminals.iter().any(|t| t.actor == RetainedActor::Writer));
        let closed = fixture.close();
        let status = joined.sender.retained.as_ref().unwrap().snapshot();
        login.unwrap();
        assert!(ended);
        assert_eq!(status.active_bodies, 1);
        assert_eq!(status.failed_bodies, 1);
        assert_eq!(status.active_bytes, 2);
        assert_eq!(
            &**joined
                .sender
                .retained
                .as_ref()
                .unwrap()
                .inner()
                .active
                .as_ref()
                .unwrap(),
            &[1, 71]
        );
        assert!(actor(&closed, RetainedActor::Writer, |o| matches!(
            o,
            RetainedOutcome::Panic
        )));
        assert!(actor(&closed, RetainedActor::Outer, |_| true));
    }

    #[test]
    fn retained_outer_panic_keeps_actual_writer_and_queued_joined() {
        let mut fixture = Fixture::new();
        retained_controls::set(retained_controls::Control {
            conn: 1,
            panic_outer: true,
            ..Default::default()
        });
        let mut client = fixture.pipe();
        let login = login_client(&mut client, "OuterPanic");
        let ended = fixture.wait(|o| o.terminals.iter().any(|t| t.actor == RetainedActor::Outer));
        let closed = fixture.close();
        login.unwrap();
        assert!(ended);
        assert!(
            closed
                .events
                .iter()
                .any(|e| matches!(e, NetEvent::Joined(_)))
        );
        assert!(actor(&closed, RetainedActor::Outer, |o| matches!(
            o,
            RetainedOutcome::Panic
        )));
        assert!(actor(&closed, RetainedActor::Writer, |_| true));
        assert!(closed.observation.attempts.iter().all(|a| a.released));
    }

    #[test]
    fn retained_writer_spawn_and_actual_pipe_write_errors_remain_distinct() {
        let mut fixture = Fixture::new();
        retained_controls::set(retained_controls::Control {
            conn: 1,
            spawn_failure: true,
            ..Default::default()
        });
        let mut client = fixture.pipe();
        let first_login = login_client(&mut client, "SpawnFailure");
        let ended = fixture.wait(|o| o.terminals.iter().any(|t| t.actor == RetainedActor::Outer));
        let failed_spawn = fixture.close();
        first_login.unwrap();
        assert!(ended);
        assert!(actor(&failed_spawn, RetainedActor::Outer, |o| matches!(
            o,
            RetainedOutcome::Spawn(_)
        )));

        let mut fixture = Fixture::new();
        let mut client = fixture.pipe();
        let login = login_client(&mut client, "NativeIoError");
        let joined = fixture.joined();
        let Some(joined) = joined else {
            fixture.close();
            panic!("native Joined missing");
        };
        retained_controls::set(retained_controls::Control {
            conn: joined.conn,
            pause_writer: true,
            ..Default::default()
        });
        joined.sender.send(vec![1, 83]);
        let entered = fixture.wait(|_| retained_controls::counts().0 > 0);
        client.writer.shutdown();
        retained_controls::release();
        let ended = fixture.wait(|o| o.terminals.iter().any(|t| t.actor == RetainedActor::Writer));
        let closed = fixture.close();
        let status = joined.sender.retained.as_ref().unwrap().snapshot();
        login.unwrap();
        assert!(entered && ended);
        assert_eq!(status.failed_bodies, 1);
        assert_eq!(status.active_bytes, 2);
        assert!(actor(&closed, RetainedActor::Writer, |o| matches!(
            o,
            RetainedOutcome::Io(_)
        )));
        assert!(!actor(&closed, RetainedActor::Writer, |o| matches!(
            o,
            RetainedOutcome::Closed
        )));
    }
}

#[cfg(all(test, feature = "paired-packages"))]
mod staged_transport_tests {
    use super::super::{StagedAttempt, StagedFailure, StagedInner, StagedProfiles, StagedState};
    use super::*;
    use horizon_protocol::connection::Pipe;
    use std::sync::atomic::AtomicBool;

    struct BudgetFixture {
        stage: Arc<StagedState>,
        sender: PlayerSender,
        rx: Receiver<StagedBody>,
        client: Connection,
        server: Connection,
    }

    fn sender_for(
        stage: &Arc<StagedState>,
        conn: u32,
        pending: Arc<AtomicUsize>,
    ) -> (PlayerSender, Receiver<StagedBody>) {
        let (tx, rx) = channel();
        let (legacy, legacy_rx) = channel();
        drop(legacy_rx);
        (
            PlayerSender {
                tx: legacy,
                pending: pending.clone(),
                held: Arc::default(),
                posted: Arc::default(),
                staged: Some(StagedSender {
                    tx,
                    stage: Arc::downgrade(stage),
                    pending,
                    conn,
                }),
                retained: None,
            },
            rx,
        )
    }

    fn budget() -> BudgetFixture {
        let (client, server) = Pipe::pair();
        let pending = Arc::new(AtomicUsize::new(0));
        let stage = Arc::new(StagedState {
            profiles: StagedProfiles::memory_pair(),
            inner: Mutex::new(StagedInner {
                attempts: vec![StagedAttempt {
                    conn: 1,
                    shutdown: server.shutdown_handle(),
                    profile: None,
                    joined: false,
                    pending: pending.clone(),
                    terminal: None,
                }],
                ..Default::default()
            }),
            closing: AtomicBool::new(false),
            outbound: Arc::default(),
        });
        let (sender, rx) = sender_for(&stage, 1, pending);
        let client = Connection::local(client);
        client
            .reader
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        BudgetFixture {
            stage,
            sender,
            rx,
            client,
            server: Connection::local(server),
        }
    }

    #[test]
    fn send_many_bypasses_holding_and_posting_without_losing_order_or_charges() {
        let fixture = budget();
        let holding = Holding::start();
        fixture
            .sender
            .send_many([vec![0, 1], vec![0, 2, 3], vec![0, 4, 5, 6]]);
        assert_eq!(fixture.sender.staged_pending(), Some(9));
        assert_eq!(fixture.stage.outbound.load(Ordering::Acquire), 9);
        assert!(fixture.sender.held().is_empty());
        assert_eq!(fixture.sender.posted.load(Ordering::Acquire), 0);
        assert!(fixture.sender.tx.send(Out::Packet(vec![0, 99])).is_err());
        drop(holding);
        flush_all([&fixture.sender]);
        let mut remaining = 9;
        for expected in [vec![0, 1], vec![0, 2, 3], vec![0, 4, 5, 6]] {
            let body = fixture.rx.try_recv().unwrap();
            assert_eq!(body.body, expected);
            assert_eq!(fixture.sender.staged_pending(), Some(remaining));
            remaining -= body.body.len();
            drop(body);
            assert_eq!(fixture.sender.staged_pending(), Some(remaining));
            assert_eq!(fixture.stage.outbound.load(Ordering::Acquire), remaining);
        }
        assert!(fixture.rx.try_recv().is_err());
    }

    #[test]
    fn active_body_stays_charged_through_native_write_success_error_and_drop() {
        let mut fixture = budget();
        fixture.sender.send(vec![0, 21, 22, 23]);
        let active = fixture.rx.try_recv().unwrap();
        assert_eq!(fixture.sender.staged_pending(), Some(4));
        assert_eq!(fixture.stage.outbound.load(Ordering::Acquire), 4);
        send_body(&mut fixture.server.writer, &active.body).unwrap();
        assert_eq!(fixture.client.recv().unwrap(), (0, vec![21, 22, 23]));
        assert_eq!(fixture.sender.staged_pending(), Some(4));
        assert_eq!(fixture.stage.outbound.load(Ordering::Acquire), 4);
        drop(active);
        assert_eq!(fixture.sender.staged_pending(), Some(0));
        fixture.client.writer.shutdown();
        fixture.sender.send(vec![0, 31, 32]);
        let failed = fixture.rx.try_recv().unwrap();
        assert!(send_body(&mut fixture.server.writer, &failed.body).is_err());
        assert_eq!(fixture.sender.staged_pending(), Some(3));
        assert_eq!(fixture.stage.outbound.load(Ordering::Acquire), 3);
        drop(failed);
        assert_eq!(fixture.sender.staged_pending(), Some(0));
        assert_eq!(fixture.stage.outbound.load(Ordering::Acquire), 0);
    }

    #[test]
    fn native_writer_delivers_queued_bodies_and_releases_every_charge() {
        let BudgetFixture {
            stage,
            sender,
            rx,
            mut client,
            server,
        } = budget();
        sender.send_many([vec![0, 41], vec![0, 42, 43], vec![0, 44]]);
        let pending = sender.pending.clone();
        assert_eq!(pending.load(Ordering::Acquire), 7);
        drop(sender);
        let (_, writer) = server.split();
        write_staged(
            writer,
            rx,
            Arc::new(AtomicI64::new(-1)),
            Arc::default(),
            pending.clone(),
            &stage,
        );
        assert_eq!(client.recv().unwrap(), (0, vec![41]));
        assert_eq!(client.recv().unwrap(), (0, vec![42, 43]));
        assert_eq!(client.recv().unwrap(), (0, vec![44]));
        assert!(client.recv().is_err());
        assert_eq!(pending.load(Ordering::Acquire), 0);
        assert_eq!(stage.outbound.load(Ordering::Acquire), 0);
        // This is a producer-channel termination/accounting case, not normal
        // session close evidence: dropping the producer deliberately refuses.
        assert_eq!(stage.inner().failure, Some(StagedFailure::Writer));
    }

    #[test]
    fn native_write_error_drops_active_and_queued_bodies_and_later_sends_charge_zero() {
        let BudgetFixture {
            stage,
            sender,
            rx,
            client,
            server,
        } = budget();
        sender.send_many([vec![0, 51], vec![0, 52, 53], vec![0, 54, 55, 56]]);
        assert_eq!(sender.staged_pending(), Some(9));
        client.writer.shutdown();
        let (_, writer) = server.split();
        write_staged(
            writer,
            rx,
            Arc::new(AtomicI64::new(-1)),
            Arc::default(),
            sender.pending.clone(),
            &stage,
        );
        assert_eq!(stage.inner().failure, Some(StagedFailure::Writer));
        assert_eq!(sender.staged_pending(), Some(0));
        assert_eq!(stage.outbound.load(Ordering::Acquire), 0);
        sender.send_many([vec![0, 61], vec![0, 62]]);
        assert_eq!(sender.staged_pending(), Some(0));
        assert_eq!(stage.outbound.load(Ordering::Acquire), 0);
    }

    #[test]
    fn disconnected_writer_channel_refuses_without_leaking_current_or_later_charges() {
        let BudgetFixture {
            stage,
            sender,
            rx,
            mut client,
            server: _server,
        } = budget();
        drop(rx);
        sender.send_many([vec![0, 71], vec![0, 72, 73]]);
        assert_eq!(stage.inner().failure, Some(StagedFailure::Writer));
        assert_eq!(sender.staged_pending(), Some(0));
        assert_eq!(stage.outbound.load(Ordering::Acquire), 0);
        assert!(client.recv().is_err());
        sender.send(vec![0, 74]);
        assert_eq!(sender.staged_pending(), Some(0));
    }

    #[test]
    fn shared_budget_overflow_preserves_active_and_queued_charges_until_drop() {
        let fixture = budget();
        let second_pending = Arc::new(AtomicUsize::new(0));
        let (second, second_rx) = sender_for(&fixture.stage, 2, second_pending);
        fixture.sender.send_many((0..8).map(|_| vec![0; 1 << 20]));
        second.send_many((0..8).map(|_| vec![0; 1 << 20]));
        assert_eq!(fixture.sender.staged_pending(), Some(8 << 20));
        assert_eq!(second.staged_pending(), Some(8 << 20));
        assert_eq!(fixture.stage.outbound.load(Ordering::Acquire), 16 << 20);
        let active = fixture.rx.try_recv().unwrap();
        // Dequeue alone cannot create capacity while the body is still active.
        second.send_many([vec![0], vec![0, 81]]);
        assert_eq!(
            fixture.stage.inner().failure,
            Some(StagedFailure::OutboundOverflow)
        );
        assert_eq!(fixture.stage.outbound.load(Ordering::Acquire), 16 << 20);
        assert_eq!(second.staged_pending(), Some(8 << 20));
        fixture.sender.send(vec![0, 82]);
        assert_eq!(fixture.sender.staged_pending(), Some(8 << 20));
        drop(fixture.rx);
        assert_eq!(fixture.sender.staged_pending(), Some(1 << 20));
        drop(second_rx);
        assert_eq!(second.staged_pending(), Some(0));
        assert_eq!(fixture.stage.outbound.load(Ordering::Acquire), 1 << 20);
        drop(active);
        assert_eq!(fixture.sender.staged_pending(), Some(0));
        assert_eq!(fixture.stage.outbound.load(Ordering::Acquire), 0);
        second.send(vec![0, 83]);
        assert_eq!(second.staged_pending(), Some(0));
    }
}
