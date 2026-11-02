//! Hosting a world on the local network. The network side lives here: the
//! listener, the per-connection handshake/login/configuration threads and the
//! writer threads (`conn`), plus the data the configuration phase sends
//! (`data`). The play state is driven by the game thread (`hub`), which only
//! ever exchanges decoded packets with these threads through channels, so a
//! world that is not open to the network pays nothing.
mod advancements;
pub mod attributes;
pub mod border;
pub mod chunks;
mod command_order;
pub mod command_suggestions;
pub mod commands;
pub mod conn;
pub mod containers;
pub mod data;
pub mod discovery;
pub mod entity;
pub mod guest;
pub mod hub;
pub mod login_negotiation;
pub mod menu;
pub mod peer_contract;
pub mod playerdata;
mod recipes;
pub mod scoreboard;
pub mod tracking;
pub mod waypoints;

pub use conn::{Holding, PlayerSender, flush_all};
use horizon_protocol::packets::play;
use horizon_protocol::{
    GAME_VERSION, PROTOCOL_VERSION, crypto::uuid_string, packets::configuration::ClientInformation,
    text::translatable,
};
use std::{
    collections::HashSet,
    io,
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize, Ordering},
        mpsc::{Receiver, Sender, channel},
    },
    thread::JoinHandle,
    time::Duration,
};

/// IntegratedServer.MAX_PLAYERS.
pub const MAX_PLAYERS: usize = 8;

/// Messages from the connection threads to the game thread.
pub enum NetEvent {
    /// A client finished configuration and enters the play state.
    Joined(Box<Joined>),
    Packet(u32, Box<play::Serverbound>),
    /// The connection ended (reason for the log).
    Left(u32, String),
    /// PlayerList.disconnectAllPlayersWithProfile: a new login of this
    /// profile replaces the player already in the world.
    Duplicate(u128),
}
pub struct Joined {
    pub conn: u32,
    pub uuid: u128,
    pub name: String,
    /// The profile's properties (skin textures, in online mode).
    pub properties: Vec<horizon_protocol::packets::login::Property>,
    pub info: ClientInformation,
    pub brand: String,
    pub sender: PlayerSender,
    /// Smoothed keep-alive round trip in milliseconds.
    pub latency: Arc<AtomicI32>,
    pub address: SocketAddr,
}

/// Experimental offline memory fixture policy; not general network admission.
#[cfg(feature = "paired-packages")]
#[derive(Clone, Copy)]
pub struct StagedProfiles(());
#[cfg(feature = "paired-packages")]
impl StagedProfiles {
    pub fn memory_pair() -> Self {
        Self(())
    }
    pub fn profiles(&self) -> [(u128, &'static str); 2] {
        ["MemoryOwner", "MemoryGuest"]
            .map(|name| (horizon_protocol::crypto::offline_uuid(name), name))
    }
}

#[cfg(feature = "paired-packages")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StagedFailure {
    NotStaged,
    InvalidConfiguration,
    AttemptLimit,
    Closed,
    WrongProfile,
    DuplicateProfile,
    InboundOverflow,
    PayloadTooLarge,
    Decode,
    Writer,
    ConnectionEnded,
    ThreadPanic,
    Unfinished,
    Identity,
    OutboundOverflow,
}
#[cfg(feature = "paired-packages")]
impl std::fmt::Display for StagedFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
#[cfg(feature = "paired-packages")]
impl std::error::Error for StagedFailure {}

#[cfg(feature = "paired-packages")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StagedTerminal {
    pub conn: u32,
    pub reason: StagedFailure,
}
#[cfg(feature = "paired-packages")]
#[derive(Debug)]
pub struct StagedTransportStatus {
    pub queued_events: usize,
    pub queued_bytes: usize,
    pub pending_outbound: usize,
    pub attempts: usize,
    pub terminal_count: usize,
    pub terminals: Vec<StagedTerminal>,
    pub threads_finished: bool,
    pub closed: bool,
}

#[cfg(feature = "paired-packages")]
const STAGED_ATTEMPTS: usize = 3;
#[cfg(feature = "paired-packages")]
const STAGED_EVENTS: usize = 64;
#[cfg(feature = "paired-packages")]
const STAGED_IN_BYTES: usize = 256 << 10;
#[cfg(feature = "paired-packages")]
const STAGED_OUT_BYTES: usize = 16 << 20;

#[cfg(feature = "paired-packages")]
struct StagedAttempt {
    conn: u32,
    shutdown: horizon_protocol::connection::PipeShutdown,
    profile: Option<usize>,
    joined: bool,
    pending: Arc<AtomicUsize>,
    terminal: Option<StagedTerminal>,
}
#[cfg(feature = "paired-packages")]
struct StagedQueued {
    event: NetEvent,
    bytes: usize,
}
#[cfg(feature = "paired-packages")]
#[derive(Default)]
struct StagedInner {
    events: std::collections::VecDeque<StagedQueued>,
    bytes: usize,
    attempts: Vec<StagedAttempt>,
    claimed: [bool; 2],
    failure: Option<StagedFailure>,
}
#[cfg(feature = "paired-packages")]
struct StagedState {
    profiles: StagedProfiles,
    inner: Mutex<StagedInner>,
    closing: AtomicBool,
    outbound: Arc<AtomicUsize>,
}
#[cfg(feature = "paired-packages")]
impl StagedState {
    fn inner(&self) -> std::sync::MutexGuard<'_, StagedInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn fail(&self, reason: StagedFailure) {
        {
            let mut inner = self.inner();
            inner.failure.get_or_insert(reason);
            self.closing.store(true, Ordering::Release);
        }
        self.close();
    }
    fn close(&self) {
        // Mark closed before inspecting registrations. A concurrent offer either
        // registered under this same lock already, or sees closing and refuses.
        self.closing.store(true, Ordering::Release);
        let shutdowns: Vec<_> = self
            .inner()
            .attempts
            .iter()
            .map(|a| a.shutdown.clone())
            .collect();
        // Never take a native channel mutex while holding the stage mutex.
        for shutdown in shutdowns {
            shutdown.shutdown();
        }
    }
    fn admit(&self, uuid: u128, name: &str) -> bool {
        let mut inner = self.inner();
        if self.closing.load(Ordering::Acquire) {
            return false;
        }
        let Some(profile) = self
            .profiles
            .profiles()
            .iter()
            .position(|(u, n)| *u == uuid && *n == name)
        else {
            inner.failure.get_or_insert(StagedFailure::WrongProfile);
            self.closing.store(true, Ordering::Release);
            drop(inner);
            self.close();
            return false;
        };
        if inner.claimed[profile] {
            inner.failure.get_or_insert(StagedFailure::DuplicateProfile);
            self.closing.store(true, Ordering::Release);
            drop(inner);
            self.close();
            return false;
        }
        inner.claimed[profile] = true;
        true
    }
    fn bind_sender(&self, conn: u32, uuid: u128, pending: &Arc<AtomicUsize>) -> bool {
        let profile = self
            .profiles
            .profiles()
            .iter()
            .position(|(u, _)| *u == uuid);
        let mut inner = self.inner();
        let Some(attempt) = inner.attempts.iter_mut().find(|a| a.conn == conn) else {
            return false;
        };
        if !Arc::ptr_eq(&attempt.pending, pending) || profile.is_none() || attempt.profile.is_some()
        {
            return false;
        }
        attempt.profile = profile;
        true
    }
    fn emit(&self, conn: u32, event: NetEvent, bytes: usize) -> bool {
        let mut inner = self.inner();
        if self.closing.load(Ordering::Acquire) {
            return false;
        }
        let Some(attempt_index) = inner
            .attempts
            .iter()
            .position(|a| a.conn == conn && a.terminal.is_none())
        else {
            inner.failure.get_or_insert(StagedFailure::Identity);
            self.closing.store(true, Ordering::Release);
            drop(inner);
            self.close();
            return false;
        };
        if let NetEvent::Joined(joined) = &event {
            let attempt = &inner.attempts[attempt_index];
            let genuine = !attempt.joined
                && attempt.profile.is_some_and(|p| {
                    let (uuid, name) = self.profiles.profiles()[p];
                    uuid == joined.uuid && name == joined.name
                })
                && joined.conn == conn
                && joined.sender.staged_identity() == Some(conn)
                && Arc::ptr_eq(&attempt.pending, &joined.sender.pending);
            if !genuine {
                inner.failure.get_or_insert(StagedFailure::Identity);
                self.closing.store(true, Ordering::Release);
                drop(inner);
                self.close();
                return false;
            }
        }
        if inner.events.len() >= STAGED_EVENTS
            || bytes > STAGED_IN_BYTES.saturating_sub(inner.bytes)
        {
            inner.failure.get_or_insert(StagedFailure::InboundOverflow);
            self.closing.store(true, Ordering::Release);
            drop(inner);
            self.close();
            return false;
        }
        if matches!(&event, NetEvent::Joined(_)) {
            inner.attempts[attempt_index].joined = true;
        }
        inner.bytes += bytes;
        inner.events.push_back(StagedQueued { event, bytes });
        true
    }
    fn terminal(&self, conn: u32, reason: StagedFailure) {
        let mut inner = self.inner();
        if let Some(attempt) = inner.attempts.iter_mut().find(|a| a.conn == conn) {
            attempt
                .terminal
                .get_or_insert(StagedTerminal { conn, reason });
        }
        if !self.closing.swap(true, Ordering::AcqRel) {
            inner.failure.get_or_insert(reason);
        }
        drop(inner);
        self.close();
    }
    fn poll(&self, max: usize) -> Result<Vec<NetEvent>, StagedFailure> {
        let mut inner = self.inner();
        if let Some(failure) = inner.failure {
            return Err(failure);
        }
        if self.closing.load(Ordering::Acquire) {
            return Err(StagedFailure::Closed);
        }
        let mut events = Vec::with_capacity(max.min(16));
        for _ in 0..max.min(16) {
            let Some(queued) = inner.events.pop_front() else {
                break;
            };
            inner.bytes -= queued.bytes;
            events.push(queued.event);
        }
        Ok(events)
    }
}
#[cfg(feature = "paired-packages")]
struct StagedThread {
    handle: JoinHandle<()>,
}
/// Queued for a connection's writer thread.
pub enum Out {
    /// A packet body: VarInt id and payload.
    Packet(Vec<u8>),
    /// Packet bodies held during a tick, in order.
    Packets(Vec<Vec<u8>>),
    /// Send a play disconnect with this component, then close.
    Disconnect(horizon_world::nbt::Tag),
    Close,
}

/// PlayerList.canPlayerLogin for a dedicated server: the disconnect reason
/// (bans, whitelist) for a profile connecting from an address, if refused.
pub type LoginGate =
    Arc<dyn Fn(SocketAddr, u128, &str) -> Option<horizon_world::nbt::Tag> + Send + Sync>;

/// online-mode=true: the key pair the server made at startup
/// (MinecraftServer.initializeKeyPair) and how players are verified.
pub struct OnlineMode {
    pub key: horizon_protocol::rsa::RsaPrivateKey,
    /// The public key as X.509 (PublicKey.getEncoded), sent in the hello.
    pub public_der: Vec<u8>,
    /// prevent-proxy-connections: the session service also checks the address.
    pub prevent_proxy: bool,
}

/// The dedicated server's log for the login phase: (thread, level, line)
/// written as vanilla's Log4j would, and the language that turns a
/// disconnect reason into its text (Component.getString).
#[derive(Clone)]
pub struct NetLog {
    pub write: Arc<dyn Fn(&str, &str, &str) + Send + Sync>,
    pub lang: Arc<horizon_protocol::text::Language>,
    /// log-ips: addresses in the log, or "<ip address withheld>".
    pub log_ips: bool,
}

/// How the server side of the network behaves: an integrated server's LAN
/// uses the defaults, a dedicated server takes them from server.properties.
#[derive(Clone)]
pub struct NetConfig {
    /// server-ip (all interfaces when absent).
    pub bind: Option<std::net::IpAddr>,
    /// 0 picks a free port.
    pub port: u16,
    pub motd: String,
    pub max_players: usize,
    /// network-compression-threshold; negative disables compression.
    pub compression_threshold: i32,
    /// enable-status: answer status pings.
    pub enable_status: bool,
    pub hide_online_players: bool,
    /// server-icon.png (64x64 PNG bytes).
    pub favicon: Option<Vec<u8>>,
    /// Secure chat (a dedicated server; none on a LAN world).
    pub secure_chat: Option<Arc<SecureChat>>,
    pub accepts_transfers: bool,
    /// LanServerPinger: announce the world on the local network.
    pub announce: bool,
    /// Accept connections from the network. Without it only local ones
    /// (`Lan::connect_local`): an integrated server not open to the LAN.
    pub listen: bool,
    /// Ops bypass a full server (ServerOpList.canBypassPlayerLimit).
    pub bypasses_limit: Option<Arc<dyn Fn(u128) -> bool + Send + Sync>>,
    pub gate: Option<LoginGate>,
    /// Online mode: players are authenticated (none on a LAN world).
    pub online: Option<Arc<OnlineMode>>,
    pub log: Option<NetLog>,
    /// server.properties `resource-pack`: offered to every player during
    /// configuration (a dedicated server's; MinecraftServer.getServerResourcePack).
    pub resource_pack: Option<crate::server::properties::ResourcePackInfo>,
    /// MinecraftServer.serverLinks (a dedicated server's `bug-report-link`):
    /// sent in configuration when non-empty
    /// (ServerConfigurationPacketListenerImpl.startConfiguration).
    pub server_links: Vec<horizon_protocol::packets::configuration::ServerLink>,
}
impl NetConfig {
    /// MinecraftServer.enforceSecureProfile.
    pub fn enforces_secure_chat(&self) -> bool {
        self.secure_chat.as_ref().is_some_and(|s| s.enforced())
    }
    /// IntegratedServer.publishServer.
    pub fn lan(port: u16, motd: String, max_players: usize) -> Self {
        Self {
            bind: None,
            port,
            motd,
            max_players,
            compression_threshold: 256,
            enable_status: true,
            hide_online_players: false,
            favicon: None,
            secure_chat: None,
            accepts_transfers: false,
            announce: true,
            listen: true,
            bypasses_limit: None,
            gate: None,
            online: None,
            log: None,
            resource_pack: None,
            server_links: Vec::new(),
        }
    }
}

/// A dedicated server's secure chat (DedicatedServer.enforceSecureProfile,
/// Services.profileKeySignatureValidator): the services' profile-key keys,
/// fetched in the background from startup and waited for by their first
/// use, as authlib's RetryableFetch does.
pub struct SecureChat {
    /// enforce-secure-profile, in online mode.
    pub enforce: bool,
    keys: std::sync::OnceLock<Vec<horizon_protocol::rsa::RsaPublicKey>>,
    fetch: Mutex<Option<JoinHandle<Vec<horizon_protocol::rsa::RsaPublicKey>>>>,
}

impl SecureChat {
    pub fn new(
        enforce: bool,
        fetch: impl FnOnce() -> Vec<horizon_protocol::rsa::RsaPublicKey> + Send + 'static,
    ) -> Self {
        let handle = std::thread::Builder::new()
            .name("Services keys".into())
            .spawn(fetch)
            .ok();
        Self {
            enforce,
            keys: std::sync::OnceLock::new(),
            fetch: Mutex::new(handle),
        }
    }
    /// The keys players' chat keys must be signed by (none when the
    /// services couldn't be reached).
    pub fn keys(&self) -> &[horizon_protocol::rsa::RsaPublicKey] {
        self.keys.get_or_init(|| {
            let handle = self.fetch.lock().ok().and_then(|mut h| h.take());
            handle.and_then(|h| h.join().ok()).unwrap_or_default()
        })
    }
    /// enforceSecureProfile: enforced, and able to check keys.
    pub fn enforced(&self) -> bool {
        self.enforce && !self.keys().is_empty()
    }
}

/// State shared with the connection threads.
pub struct Shared {
    pub sync: Arc<data::SyncData>,
    pub events: Sender<NetEvent>,
    pub config: NetConfig,
    motd: Mutex<String>,
    max_players: AtomicUsize,
    /// PlayerList.players in join order: profile id and name.
    online: Mutex<Vec<(u128, String)>>,
    /// Profiles held by the host (the LAN host's own name).
    reserved: Mutex<HashSet<u128>>,
    next: AtomicU32,
    stop: AtomicBool,
    random: Mutex<u64>,
    required_negotiation: Option<login_negotiation::RequiredNegotiation>,
    #[cfg(feature = "paired-packages")]
    stage: Option<Arc<StagedState>>,
}
impl Shared {
    pub fn next_id(&self) -> u32 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }
    /// PlayerList.canPlayerLogin then disconnectAllPlayersWithProfile: the
    /// refusal reason, or None once the profile holds a place. A profile
    /// already playing is kicked and its place awaited.
    pub fn try_admit(
        &self,
        address: SocketAddr,
        uuid: u128,
        name: &str,
    ) -> Option<horizon_world::nbt::Tag> {
        // Staging never enters the ordinary duplicate replacement/wait loop.
        #[cfg(feature = "paired-packages")]
        if let Some(stage) = &self.stage {
            return (!stage.admit(uuid, name))
                .then(|| translatable("multiplayer.disconnect.duplicate_login", &[]));
        }
        if let Some(gate) = &self.config.gate
            && let Some(reason) = gate(address, uuid, name)
        {
            return Some(reason);
        }
        if self.reserved.lock().unwrap().contains(&uuid) {
            return Some(translatable("multiplayer.disconnect.duplicate_login", &[]));
        }
        {
            let online = self.online.lock().unwrap();
            let bypass = self.config.bypasses_limit.as_ref().is_some_and(|b| b(uuid));
            let taken = online.len() + self.reserved.lock().unwrap().len();
            if taken >= self.max_players() && !bypass {
                return Some(translatable("multiplayer.disconnect.server_full", &[]));
            }
        }
        if self.is_online(uuid) {
            let _ = self.events.send(NetEvent::Duplicate(uuid));
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while self.is_online(uuid) {
                if std::time::Instant::now() > deadline {
                    return Some(translatable("multiplayer.disconnect.slow_login", &[]));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        self.online.lock().unwrap().push((uuid, name.to_owned()));
        None
    }
    fn is_online(&self, uuid: u128) -> bool {
        self.online.lock().unwrap().iter().any(|(u, _)| *u == uuid)
    }
    pub fn motd(&self) -> String {
        self.motd.lock().unwrap().clone()
    }
    pub fn max_players(&self) -> usize {
        self.max_players.load(Ordering::Relaxed)
    }
    pub fn release(&self, uuid: u128) {
        self.online.lock().unwrap().retain(|(u, _)| *u != uuid);
    }
    pub fn online(&self) -> usize {
        self.online.lock().unwrap().len() + self.reserved.lock().unwrap().len()
    }
    fn next_random(&self, bound: usize) -> usize {
        let mut r = self.random.lock().unwrap();
        *r = r
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((*r >> 33) as usize) % bound.max(1)
    }
    /// The status response (MinecraftServer.buildServerStatus encoded with
    /// ServerStatus.CODEC): description, players with up to 12 shuffled
    /// names, version, favicon and enforcesSecureChat when true.
    pub fn status_json(&self) -> String {
        use crate::server::json::Json;
        let motd = self.motd.lock().unwrap().clone();
        let online: Vec<(u128, String)> = self.online.lock().unwrap().clone();
        let count = self.online();
        let mut players = vec![
            (
                "max".to_owned(),
                Json::Number(self.max_players().to_string()),
            ),
            ("online".to_owned(), Json::Number(count.to_string())),
        ];
        if !self.config.hide_online_players && !online.is_empty() {
            let size = online.len().min(12);
            let offset = self.next_random(online.len() - size + 1);
            let mut sample: Vec<(u128, String)> = online[offset..offset + size].to_vec();
            for i in (1..sample.len()).rev() {
                let j = self.next_random(i + 1);
                sample.swap(i, j);
            }
            players.push((
                "sample".to_owned(),
                Json::Array(
                    sample
                        .into_iter()
                        .map(|(u, n)| {
                            Json::Object(vec![
                                ("id".to_owned(), Json::String(uuid_string(u))),
                                ("name".to_owned(), Json::String(n)),
                            ])
                        })
                        .collect(),
                ),
            ));
        }
        let mut status = vec![
            ("description".to_owned(), Json::String(motd)),
            ("players".to_owned(), Json::Object(players)),
            (
                "version".to_owned(),
                Json::Object(vec![
                    ("name".to_owned(), Json::String(GAME_VERSION.to_owned())),
                    (
                        "protocol".to_owned(),
                        Json::Number(PROTOCOL_VERSION.to_string()),
                    ),
                ]),
            ),
        ];
        if let Some(icon) = &self.config.favicon {
            status.push((
                "favicon".to_owned(),
                Json::String(format!("data:image/png;base64,{}", base64(icon))),
            ));
        }
        if self.config.enforces_secure_chat() {
            status.push(("enforcesSecureChat".to_owned(), Json::Bool(true)));
        }
        Json::Object(status).compact()
    }
}

/// java.util.Base64.getEncoder (padded, no line breaks).
pub fn base64(data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        out.push(char::from(A[(n >> 18) as usize & 63]));
        out.push(char::from(A[(n >> 12) as usize & 63]));
        out.push(if chunk.len() > 1 {
            char::from(A[(n >> 6) as usize & 63])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(A[n as usize & 63])
        } else {
            '='
        });
    }
    out
}

/// A running LAN server.
pub struct Lan {
    pub shared: Arc<Shared>,
    events: Receiver<NetEvent>,
    pub port: u16,
    listener: Option<JoinHandle<()>>,
    /// Stops the accept loop alone: local connections stay.
    listener_stop: Arc<AtomicBool>,
    /// The multicast announcement (LanServerPinger); absent when the socket failed.
    _pinger: Option<discovery::Pinger>,
    #[cfg(feature = "paired-packages")]
    staged_threads: Mutex<Vec<StagedThread>>,
}
impl Lan {
    /// Listen on `port` (0 picks a free one).
    pub fn start(
        sync: Arc<data::SyncData>,
        port: u16,
        motd: String,
        max_players: usize,
    ) -> io::Result<Self> {
        Self::start_with(sync, NetConfig::lan(port, motd, max_players))
    }
    /// ServerConnectionListener.startTcpServerListener with a configuration.
    pub fn start_with(sync: Arc<data::SyncData>, config: NetConfig) -> io::Result<Self> {
        Self::start_policy(sync, config, None)
    }
    /// Explicit compatibility-only TCP path. Matching peers are still refused
    /// before reservation/configuration; ordinary launch constructors stay off.
    pub fn start_with_required_negotiation(
        sync: Arc<data::SyncData>,
        config: NetConfig,
        required: login_negotiation::RequiredNegotiation,
    ) -> io::Result<Self> {
        if !required.expectation.retains_sync(&sync) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "required negotiation SyncData owner differs",
            ));
        }
        Self::start_policy(sync, config, Some(required))
    }
    fn start_policy(
        sync: Arc<data::SyncData>,
        config: NetConfig,
        required_negotiation: Option<login_negotiation::RequiredNegotiation>,
    ) -> io::Result<Self> {
        let (tx, events) = channel();
        let (listen, port, announce) = (config.listen, config.port, config.announce);
        let shared = Arc::new(Shared {
            sync,
            events: tx,
            motd: Mutex::new(config.motd.clone()),
            max_players: AtomicUsize::new(config.max_players),
            online: Mutex::new(Vec::new()),
            reserved: Mutex::new(HashSet::new()),
            next: AtomicU32::new(1),
            stop: AtomicBool::new(false),
            random: Mutex::new(crate::server::java_properties::now_millis() as u64 | 1),
            config,
            required_negotiation,
            #[cfg(feature = "paired-packages")]
            stage: None,
        });
        let mut lan = Self {
            shared,
            events,
            port: 0,
            listener: None,
            listener_stop: Arc::new(AtomicBool::new(false)),
            _pinger: None,
            #[cfg(feature = "paired-packages")]
            staged_threads: Mutex::new(Vec::new()),
        };
        if listen {
            lan.listen(port, announce)?;
        }
        Ok(lan)
    }
    /// Experimental bounded mailbox/body staging over the existing memory pipe.
    /// The memory pipe itself remains unbounded; these are not total-memory limits.
    #[cfg(feature = "paired-packages")]
    pub fn start_staged(
        sync: Arc<data::SyncData>,
        config: NetConfig,
        profiles: StagedProfiles,
    ) -> io::Result<Self> {
        if config.listen
            || config.announce
            || config.online.is_some()
            || config.resource_pack.is_some()
            || config.secure_chat.is_some()
            || config.gate.is_some()
            || config.bypasses_limit.is_some()
            || config.accepts_transfers
            || config.max_players != 2
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                StagedFailure::InvalidConfiguration,
            ));
        }
        let mut lan = Self::start_with(sync, config)?;
        Arc::get_mut(&mut lan.shared)
            .expect("closed LAN has no connection owners")
            .stage = Some(Arc::new(StagedState {
            profiles,
            inner: Mutex::new(StagedInner::default()),
            closing: AtomicBool::new(false),
            outbound: Arc::new(AtomicUsize::new(0)),
        }));
        Ok(lan)
    }

    #[cfg(feature = "paired-packages")]
    pub fn connect_local_staged(&self) -> io::Result<horizon_protocol::connection::Pipe> {
        let stage =
            self.shared.stage.as_ref().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, StagedFailure::NotStaged)
            })?;
        // Serialize offers, spawn and handle registration. A successfully spawned
        // reader is never briefly outside this bounded owning collection.
        let mut threads = self
            .staged_threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (client, server) = horizon_protocol::connection::Pipe::pair();
        let shutdown = server.shutdown_handle();
        let conn;
        {
            let mut inner = stage.inner();
            if stage.closing.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    StagedFailure::Closed,
                ));
            }
            if inner.attempts.len() == STAGED_ATTEMPTS {
                inner.failure.get_or_insert(StagedFailure::AttemptLimit);
                stage.closing.store(true, Ordering::Release);
                drop(inner);
                stage.close();
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    StagedFailure::AttemptLimit,
                ));
            }
            conn = self.shared.next_id();
            inner.attempts.push(StagedAttempt {
                conn,
                shutdown,
                profile: None,
                joined: false,
                pending: Arc::new(AtomicUsize::new(0)),
                terminal: None,
            });
        }
        let shared = self.shared.clone();
        match std::thread::Builder::new()
            .name(format!("horizon-staged-local-{conn}"))
            .spawn(move || conn::serve_local_staged(server, shared, conn))
        {
            Ok(handle) => threads.push(StagedThread { handle }),
            Err(err) => {
                stage.terminal(conn, StagedFailure::Writer);
                return Err(err);
            }
        }
        Ok(client)
    }

    /// Terminal notices live outside the data mailbox and are checked first.
    #[cfg(feature = "paired-packages")]
    pub fn poll_staged(&self, max: usize) -> Result<Vec<NetEvent>, StagedFailure> {
        let stage = self.shared.stage.as_ref().ok_or(StagedFailure::NotStaged)?;
        stage.poll(max)
    }

    #[cfg(feature = "paired-packages")]
    pub fn staged_failure(&self) -> Option<StagedFailure> {
        self.shared
            .stage
            .as_ref()
            .and_then(|stage| stage.inner().failure)
    }

    #[cfg(feature = "paired-packages")]
    pub fn staged_status(&self) -> Result<StagedTransportStatus, StagedFailure> {
        let stage = self.shared.stage.as_ref().ok_or(StagedFailure::NotStaged)?;
        let threads = self
            .staged_threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let inner = stage.inner();
        let terminals: Vec<_> = inner.attempts.iter().filter_map(|a| a.terminal).collect();
        Ok(StagedTransportStatus {
            queued_events: inner.events.len(),
            queued_bytes: inner.bytes,
            pending_outbound: stage.outbound.load(Ordering::Acquire),
            attempts: inner.attempts.len(),
            terminal_count: terminals.len(),
            terminals,
            threads_finished: threads.iter().all(|t| t.handle.is_finished()),
            closed: stage.closing.load(Ordering::Acquire),
        })
    }

    #[cfg(feature = "paired-packages")]
    pub fn validate_staged_join(&self, joined: &Joined) -> bool {
        let Some(stage) = &self.shared.stage else {
            return false;
        };
        let inner = stage.inner();
        if inner.failure.is_some() || stage.closing.load(Ordering::Acquire) {
            return false;
        }
        inner.attempts.iter().any(|a| {
            a.conn == joined.conn
                && a.terminal.is_none()
                && a.profile.is_some_and(|p| {
                    let (uuid, name) = stage.profiles.profiles()[p];
                    uuid == joined.uuid && name == joined.name
                })
                && joined.sender.staged_identity() == Some(joined.conn)
                && Arc::ptr_eq(&a.pending, &joined.sender.pending)
        })
    }

    /// Close native channels even while a reader is inside an incomplete frame.
    #[cfg(feature = "paired-packages")]
    pub fn close_staged(&self) -> Result<(), StagedFailure> {
        let stage = self.shared.stage.as_ref().ok_or(StagedFailure::NotStaged)?;
        stage.close();
        Ok(())
    }

    /// Linearize intentional positive-roster close after a confirmed checkpoint.
    /// A previously observed departure/failure refuses without releasing owners.
    #[cfg(feature = "paired-packages")]
    pub fn close_staged_if_live(&self) -> Result<(), StagedFailure> {
        let stage = self.shared.stage.as_ref().ok_or(StagedFailure::NotStaged)?;
        let shutdowns: Vec<_> = {
            let inner = stage.inner();
            if let Some(failure) = inner.failure {
                return Err(failure);
            }
            if let Some(terminal) = inner.attempts.iter().find_map(|a| a.terminal) {
                return Err(terminal.reason);
            }
            if stage.closing.load(Ordering::Acquire) {
                return Err(StagedFailure::Closed);
            }
            if inner.attempts.len() != 2
                || inner.claimed != [true, true]
                || ![0, 1].into_iter().all(|p| {
                    inner
                        .attempts
                        .iter()
                        .filter(|a| a.profile == Some(p) && a.joined)
                        .count()
                        == 1
                })
            {
                return Err(StagedFailure::Identity);
            }
            if !inner.events.is_empty()
                || inner.bytes != 0
                || stage.outbound.load(Ordering::Acquire) != 0
                || inner
                    .attempts
                    .iter()
                    .any(|a| a.pending.load(Ordering::Acquire) != 0)
            {
                return Err(StagedFailure::Unfinished);
            }
            // Terminal insertion and admission use this same mutex. Only this
            // point converts a live accepted pair to intentionally closing.
            stage.closing.store(true, Ordering::Release);
            inner.attempts.iter().map(|a| a.shutdown.clone()).collect()
        };
        for shutdown in shutdowns {
            shutdown.shutdown();
        }
        Ok(())
    }

    #[cfg(feature = "paired-packages")]
    pub fn staged_finished(&self) -> bool {
        self.staged_status().is_ok_and(|s| {
            s.closed
                && s.threads_finished
                && s.pending_outbound == 0
                && s.terminal_count == s.attempts
        })
    }

    /// Join only positively finished outer readers (which already joined writers).
    /// Historical refusal remains available in staged_failure after clean cleanup.
    #[cfg(feature = "paired-packages")]
    pub fn join_staged_finished(&self) -> Result<(), StagedFailure> {
        let stage = self.shared.stage.as_ref().ok_or(StagedFailure::NotStaged)?;
        if !self.staged_finished() {
            return Err(StagedFailure::Unfinished);
        }
        let mut threads = self
            .staged_threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut panic = false;
        for thread in threads.drain(..) {
            panic |= thread.handle.join().is_err();
        }
        let mut inner = stage.inner();
        inner.events.clear();
        inner.bytes = 0;
        if panic {
            inner.failure.get_or_insert(StagedFailure::ThreadPanic);
            return Err(StagedFailure::ThreadPanic);
        }
        if stage.outbound.load(Ordering::Acquire) != 0
            || inner
                .attempts
                .iter()
                .any(|a| a.pending.load(Ordering::Acquire) != 0)
        {
            return Err(StagedFailure::Unfinished);
        }
        Ok(())
    }
    /// Start accepting connections on `port` (0 picks a free one), and
    /// announce the world on the local network when `announce`
    /// (`IntegratedServer.publishServer`'s LanServerPinger). Returns the port.
    pub fn listen(&mut self, port: u16, announce: bool) -> io::Result<u16> {
        #[cfg(feature = "paired-packages")]
        if self.shared.stage.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                StagedFailure::InvalidConfiguration,
            ));
        }
        if self.listener.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "already listening",
            ));
        }
        let listener = Self::bind(self.shared.config.bind, port)?;
        self.listen_on(listener, announce)
    }
    /// The socket [`Lan::listen`] accepts on, bound ahead so that a failure
    /// can be reported before the hub is at hand.
    pub fn bind(address: Option<std::net::IpAddr>, port: u16) -> io::Result<TcpListener> {
        let bind = address.unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
        let listener = TcpListener::bind((bind, port))?;
        listener.set_nonblocking(true)?;
        Ok(listener)
    }
    /// [`Lan::listen`] on a socket already bound.
    pub fn listen_on(&mut self, listener: TcpListener, announce: bool) -> io::Result<u16> {
        #[cfg(feature = "paired-packages")]
        if self.shared.stage.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                StagedFailure::InvalidConfiguration,
            ));
        }
        if self.listener.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "already listening",
            ));
        }
        let port = listener.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let (accept_shared, accept_stop) = (self.shared.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("horizon-lan-accept".into())
            .spawn(move || accept_loop(listener, accept_shared, &accept_stop))?;
        self._pinger = if announce {
            discovery::Pinger::start(self.shared.motd(), port).ok()
        } else {
            None
        };
        self.listener = Some(thread);
        self.listener_stop = stop;
        self.port = port;
        Ok(port)
    }
    /// Stop accepting connections and announcing; connections already in
    /// (the local one included) stay.
    pub fn stop_listening(&mut self) {
        self.listener_stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.listener.take() {
            let _ = t.join();
        }
        self._pinger = None;
        self.port = 0;
    }
    /// Whether connections from the network are accepted.
    pub fn listening(&self) -> bool {
        self.listener.is_some()
    }
    /// `Connection.connectToLocalServer`: a connection through memory, its
    /// server end served on its own thread like an accepted socket.
    pub fn connect_local(&self) -> io::Result<horizon_protocol::connection::Pipe> {
        #[cfg(feature = "paired-packages")]
        if self.shared.stage.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "use tracked staged connection",
            ));
        }
        let (client, server) = horizon_protocol::connection::Pipe::pair();
        let shared = self.shared.clone();
        std::thread::Builder::new()
            .name("horizon-local-conn".into())
            .spawn(move || {
                if let Err(e) = conn::serve_local(server, shared)
                    && std::env::var_os("HORIZON_NET_DEBUG").is_some()
                {
                    log::warn!("[lan] local: {e}");
                }
            })?;
        Ok(client)
    }
    /// Everything the connection threads reported since the last call.
    pub fn poll(&self) -> Vec<NetEvent> {
        #[cfg(feature = "paired-packages")]
        assert!(self.shared.stage.is_none(), "use bounded staged polling");
        self.events.try_iter().collect()
    }
    /// Reserve a uuid for the host so a guest cannot take the name.
    pub fn reserve(&self, uuid: u128) {
        self.shared.reserved.lock().unwrap().insert(uuid);
    }
    pub fn stop(&mut self) {
        if let Some(required) = &self.shared.required_negotiation {
            required.close();
        }
        #[cfg(feature = "paired-packages")]
        if self.shared.stage.is_some() {
            let _ = self.close_staged();
            return;
        }
        self.shared.stop.store(true, Ordering::Relaxed);
        self.stop_listening();
    }
}
impl Drop for Lan {
    fn drop(&mut self) {
        #[cfg(feature = "paired-packages")]
        if self.shared.stage.is_some()
            && !self
                .staged_threads
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        {
            // No implicit joins or detach. The owning coordinator must explicitly
            // close, observe completion, and consume every tracked handle.
            let _ = std::io::Write::write_all(
                &mut std::io::stderr(),
                b"fatal: unresolved staged LAN connection owner\n",
            );
            std::process::abort();
        }
        self.stop();
    }
}

fn accept_loop(listener: TcpListener, shared: Arc<Shared>, stop: &AtomicBool) {
    while !shared.stop.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, addr)) => {
                let shared = shared.clone();
                let _ = std::thread::Builder::new()
                    .name("horizon-lan-conn".into())
                    .spawn(move || handle(stream, addr, shared));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}
fn handle(stream: TcpStream, addr: SocketAddr, shared: Arc<Shared>) {
    let _ = stream.set_nonblocking(false);
    if let Err(e) = conn::serve(stream, addr, shared)
        && std::env::var_os("HORIZON_NET_DEBUG").is_some()
    {
        log::warn!("[lan] {addr}: {e}");
    }
}

/// HttpUtil.getAvailablePort: a free local port.
pub fn available_port() -> u16 {
    TcpListener::bind(("0.0.0.0", 0))
        .and_then(|l| l.local_addr())
        .map_or(25565, |a| a.port())
}

#[cfg(feature = "paired-packages")]
use retained::RetainedContext;
#[cfg(feature = "paired-packages")]
pub use retained::{
    ClosedLan, RetainedActor, RetainedAttemptObservation, RetainedCloseStatus, RetainedLan,
    RetainedObservation, RetainedOfflineConfig, RetainedOutcome, RetainedTerminal,
};

#[cfg(all(test, feature = "paired-packages"))]
pub(super) fn retained_test_sync() -> Arc<data::SyncData> {
    static SYNC: std::sync::OnceLock<Arc<data::SyncData>> = std::sync::OnceLock::new();
    SYNC.get_or_init(|| {
        let resources = std::env::var_os("HORIZON_RETAINED_TEST_RESOURCES")
            .expect("retained native tests require pinned HORIZON_RETAINED_TEST_RESOURCES");
        Arc::new(
            data::SyncData::load(std::path::Path::new(&resources))
                .expect("retained native tests require valid pinned prepared resources"),
        )
    })
    .clone()
}

/// A separate native owner. Ordinary LAN constructors never enter this module.
#[cfg(feature = "paired-packages")]
mod retained {
    use super::*;
    use horizon_protocol::connection::{Connection, ConnectionShutdown, Pipe};
    use std::collections::VecDeque;
    use std::time::Instant;

    /// Constructor input deliberately cannot own online services or callbacks.
    #[derive(Clone, Debug)]
    pub struct RetainedOfflineConfig {
        pub bind: Option<std::net::IpAddr>,
        pub port: u16,
        pub motd: String,
        pub max_players: usize,
        pub compression_threshold: i32,
        pub enable_status: bool,
        pub hide_online_players: bool,
        pub announce: bool,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum RetainedOutcome {
        Returned,
        Io(String),
        /// The baseline protocol error carries no native/codec discriminator.
        Receive(String),
        /// Outbound framing/compression failed before the native write call.
        Encode(String),
        Spawn(String),
        PingerStartup(discovery::RetainedPingerStartError),
        Protocol(String),
        Eof,
        Closed,
        QueueClosed,
        Panic,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum RetainedActor {
        Listener,
        Outer,
        Writer,
        Shutdown,
        Acquisition,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct RetainedTerminal {
        pub conn: Option<u32>,
        pub actor: RetainedActor,
        pub outcome: RetainedOutcome,
    }

    #[derive(Clone, Debug)]
    pub struct RetainedAttemptObservation {
        pub conn: u32,
        pub uuid: Option<u128>,
        pub released: bool,
        pub outer_pending: bool,
        pub writer_pending: bool,
        pub outbound: Option<conn::RetainedOutboundStatus>,
    }

    #[derive(Clone, Debug)]
    pub struct RetainedObservation {
        pub closing: bool,
        pub deadline_expired: bool,
        pub quiescent: bool,
        pub queued_events: usize,
        pub listener_pending: bool,
        pub attempts: Vec<RetainedAttemptObservation>,
        pub terminals: Vec<RetainedTerminal>,
        pub announcement: Option<discovery::RetainedPingerObservation>,
    }

    #[derive(Clone, Debug)]
    pub enum RetainedCloseStatus {
        Pending(RetainedObservation),
        Blocked(RetainedObservation),
        Quiescent(RetainedObservation),
    }

    /// Completion retains unconsumed game events and actual native outcomes.
    pub struct ClosedLan {
        pub events: Vec<NetEvent>,
        pub observation: RetainedObservation,
        pub announcement: Option<discovery::RetainedPingerTerminal>,
        // Preserve original queued/active wire payloads through the typed
        // closed handoff. They are disposed only when this closed owner drops.
        _registry: Arc<RetainedRegistry>,
    }

    #[derive(Default)]
    struct NativeSlot {
        pending: bool,
        handle: Option<JoinHandle<RetainedOutcome>>,
        outcome: Option<RetainedOutcome>,
    }
    impl NativeSlot {
        fn starting() -> Self {
            Self {
                pending: true,
                ..Self::default()
            }
        }
        fn unresolved(&self) -> bool {
            self.pending || self.handle.is_some()
        }
        fn take_finished(&mut self) -> Option<JoinHandle<RetainedOutcome>> {
            if self.handle.as_ref().is_some_and(JoinHandle::is_finished) {
                self.pending = true;
                self.handle.take()
            } else {
                None
            }
        }
    }

    struct Attempt {
        conn: u32,
        shutdown: ConnectionShutdown,
        outer: NativeSlot,
        writer: Option<NativeSlot>,
        outbound: Option<Arc<conn::RetainedOutbound>>,
        uuid: Option<u128>,
        released: bool,
    }
    #[derive(Default)]
    struct RegistryInner {
        closing: bool,
        listener: Option<NativeSlot>,
        attempts: Vec<Attempt>,
        events: VecDeque<NetEvent>,
        terminals: Vec<RetainedTerminal>,
        #[cfg(test)]
        controls: AcquisitionControls,
    }
    #[cfg(test)]
    #[derive(Default)]
    struct AcquisitionControls {
        outer_registered: Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>,
        close_latched: Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>,
        fail_outer_spawn: bool,
        fail_listener_spawn: bool,
        panic_listener: bool,
        #[cfg(all(
            feature = "native-writer-test-control",
            debug_assertions,
            target_os = "linux"
        ))]
        socket_send_buffer: Option<usize>,
        #[cfg(all(
            feature = "native-writer-test-control",
            debug_assertions,
            target_os = "linux"
        ))]
        socket_send_buffer_result: Option<Result<usize, String>>,
    }
    struct RetainedRegistry {
        inner: Mutex<RegistryInner>,
        shared: Arc<Shared>,
        closing_latch: Arc<AtomicBool>,
    }

    /// Explicit per-attempt codec context; never installed in ordinary Shared.
    #[derive(Clone)]
    pub(super) struct RetainedContext {
        registry: Arc<RetainedRegistry>,
        conn: u32,
    }
    impl RetainedContext {
        pub(super) fn conn(&self) -> u32 {
            self.conn
        }
        pub(super) fn is_closing(&self) -> bool {
            self.registry.closing_latch.load(Ordering::Acquire)
        }
        pub(super) fn close_latch(&self) -> Arc<AtomicBool> {
            self.registry.closing_latch.clone()
        }
        pub(super) fn admit(
            &self,
            shared: &Shared,
            uuid: u128,
            name: &str,
        ) -> Option<horizon_world::nbt::Tag> {
            let mut inner = self.registry.inner();
            if !std::ptr::eq(shared, Arc::as_ptr(&self.registry.shared)) || inner.closing {
                return Some(translatable("multiplayer.disconnect.server_shutdown", &[]));
            }
            let Some(index) = inner.attempts.iter().position(|a| a.conn == self.conn) else {
                return Some(translatable("multiplayer.disconnect.duplicate_login", &[]));
            };
            if inner.attempts[index].uuid.is_some()
                || inner
                    .attempts
                    .iter()
                    .any(|a| a.uuid == Some(uuid) && !a.released)
            {
                return Some(translatable("multiplayer.disconnect.duplicate_login", &[]));
            }
            if inner
                .attempts
                .iter()
                .filter(|a| a.uuid.is_some() && !a.released)
                .count()
                >= shared.max_players()
            {
                return Some(translatable("multiplayer.disconnect.server_full", &[]));
            }
            inner.attempts[index].uuid = Some(uuid);
            shared
                .online
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((uuid, name.to_owned()));
            None
        }
        pub(super) fn emit(&self, event: NetEvent) -> bool {
            let mut inner = self.registry.inner();
            if !inner.attempts.iter().any(|a| a.conn == self.conn) {
                return false;
            }
            // Close seals new admission/outbound production. Already decoded
            // events, including Left, remain available to the consumer.
            inner.events.push_back(event);
            true
        }
        pub(super) fn attach_outbound(&self, outbound: Arc<conn::RetainedOutbound>) -> bool {
            let mut inner = self.registry.inner();
            let closing = inner.closing;
            let Some(attempt) = inner.attempts.iter_mut().find(|a| a.conn == self.conn) else {
                drop(inner);
                outbound.seal();
                return false;
            };
            if attempt.outbound.is_some() {
                drop(inner);
                outbound.seal();
                return false;
            }
            attempt.outbound = Some(outbound.clone());
            drop(inner);
            if closing {
                outbound.seal();
            }
            !closing
        }
        /// The caller keeps its writer behind a start latch until this returns.
        /// A false admission still retains the original handle for checked reap.
        pub(super) fn install_writer(&self, handle: JoinHandle<RetainedOutcome>) -> bool {
            let mut inner = self.registry.inner();
            let closing = inner.closing;
            let attempt = inner
                .attempts
                .iter_mut()
                .find(|a| a.conn == self.conn)
                .expect("retained writer belongs to a registered attempt");
            assert!(
                attempt.writer.is_none(),
                "one original retained writer per attempt"
            );
            attempt.writer = Some(NativeSlot {
                handle: Some(handle),
                ..NativeSlot::default()
            });
            !closing
        }
        pub(super) fn record_failure(&self, outcome: RetainedOutcome) {
            self.registry
                .record(Some(self.conn), RetainedActor::Acquisition, outcome);
        }
        pub(super) fn interrupt(&self) {
            let (shutdown, outbound) = {
                let inner = self.registry.inner();
                let Some(attempt) = inner.attempts.iter().find(|a| a.conn == self.conn) else {
                    return;
                };
                (attempt.shutdown.clone(), attempt.outbound.clone())
            };
            if let Some(outbound) = outbound {
                outbound.seal();
            }
            if let Err(error) = shutdown.shutdown() {
                self.registry.record(
                    Some(self.conn),
                    RetainedActor::Shutdown,
                    RetainedOutcome::Io(error.to_string()),
                );
            }
        }
    }

    impl RetainedRegistry {
        fn inner(&self) -> std::sync::MutexGuard<'_, RegistryInner> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
        fn record(&self, conn: Option<u32>, actor: RetainedActor, outcome: RetainedOutcome) {
            self.inner().terminals.push(RetainedTerminal {
                conn,
                actor,
                outcome,
            });
        }
        #[cfg(all(
            test,
            feature = "native-writer-test-control",
            debug_assertions,
            target_os = "linux"
        ))]
        fn configure_test_socket(&self, stream: &TcpStream) -> Result<(), String> {
            let requested = self.inner().controls.socket_send_buffer;
            let Some(requested) = requested else {
                return Ok(());
            };
            // These safe native operations concern only this newly accepted
            // fixture socket. No raw transport escapes into the public API.
            let result = rustix::net::sockopt::set_socket_send_buffer_size(stream, requested)
                .and_then(|()| rustix::net::sockopt::socket_send_buffer_size(stream))
                .map_err(|error| error.to_string())
                .and_then(|effective| {
                    stream
                        .set_write_timeout(Some(Duration::from_secs(5)))
                        .map(|()| effective)
                        .map_err(|error| format!("accepted fixture write-timeout setup: {error}"))
                });
            self.inner().controls.socket_send_buffer_result = Some(result.clone());
            result.map(|_| ())
        }
        fn register(self: &Arc<Self>, connection: &Connection) -> (RetainedContext, bool) {
            let shutdown = connection.shutdown_handle();
            let mut inner = self.inner();
            let conn = self.shared.next_id();
            let accepted = !inner.closing;
            inner.attempts.push(Attempt {
                conn,
                shutdown,
                outer: if accepted {
                    NativeSlot::starting()
                } else {
                    NativeSlot {
                        outcome: Some(RetainedOutcome::Closed),
                        ..NativeSlot::default()
                    }
                },
                writer: None,
                outbound: None,
                uuid: None,
                released: false,
            });
            if !accepted {
                inner.terminals.push(RetainedTerminal {
                    conn: Some(conn),
                    actor: RetainedActor::Acquisition,
                    outcome: RetainedOutcome::Closed,
                });
            }
            (
                RetainedContext {
                    registry: self.clone(),
                    conn,
                },
                accepted,
            )
        }
        fn start(self: &Arc<Self>, connection: Connection, address: SocketAddr) -> io::Result<()> {
            let (context, accepted) = self.register(&connection);
            if !accepted {
                context.interrupt();
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "retained LAN closing",
                ));
            }
            let (release, start) = channel();
            let worker_context = context.clone();
            let shared = self.shared.clone();
            #[cfg(test)]
            let (registration_gate, fail_spawn) = {
                let mut inner = self.inner();
                (
                    inner.controls.outer_registered.take(),
                    inner.controls.fail_outer_spawn,
                )
            };
            #[cfg(test)]
            if let Some((registered, resume)) = registration_gate {
                registered.wait();
                resume.wait();
            }
            let builder = std::thread::Builder::new().name("horizon-retained-conn".into());
            // Force the actual native spawn operation to refuse an impossible
            // stack; the same installation/failure path retains its result.
            #[cfg(test)]
            let builder = if fail_spawn {
                builder.stack_size(usize::MAX / 2)
            } else {
                builder
            };
            let spawned = builder.spawn(move || {
                if start.recv().is_err() {
                    return RetainedOutcome::QueueClosed;
                }
                if worker_context.is_closing() {
                    worker_context.interrupt();
                    return RetainedOutcome::Closed;
                }
                conn::serve_retained(connection, address, shared, worker_context)
            });
            match spawned {
                Ok(handle) => {
                    let closing = {
                        let mut inner = self.inner();
                        let closing = inner.closing;
                        let attempt = inner
                            .attempts
                            .iter_mut()
                            .find(|a| a.conn == context.conn)
                            .unwrap();
                        attempt.outer.handle = Some(handle);
                        attempt.outer.pending = false;
                        closing
                    };
                    if closing {
                        context.interrupt();
                    }
                    let _ = release.send(());
                    Ok(())
                }
                Err(error) => {
                    self.complete(
                        Some(context.conn),
                        RetainedActor::Outer,
                        RetainedOutcome::Spawn(error.to_string()),
                    );
                    Err(error)
                }
            }
        }
        fn request_close(&self) {
            let owners: Vec<_> = {
                let mut inner = self.inner();
                inner.closing = true;
                // Queue admission reads this same permanent latch while holding
                // its own queue lock. No producer starting after this store can
                // enter the gap before the individual queues are sealed below.
                self.closing_latch.store(true, Ordering::Release);
                inner
                    .attempts
                    .iter()
                    .map(|a| (a.conn, a.shutdown.clone(), a.outbound.clone()))
                    .collect()
            };
            #[cfg(test)]
            let gate = self.inner().controls.close_latched.take();
            #[cfg(test)]
            if let Some((latched, resume)) = gate {
                latched.wait();
                resume.wait();
            }
            for (conn, shutdown, outbound) in owners {
                if let Some(outbound) = outbound {
                    outbound.seal();
                }
                if let Err(error) = shutdown.shutdown() {
                    self.record(
                        Some(conn),
                        RetainedActor::Shutdown,
                        RetainedOutcome::Io(error.to_string()),
                    );
                }
            }
        }
        fn complete(&self, conn: Option<u32>, actor: RetainedActor, outcome: RetainedOutcome) {
            let mut inner = self.inner();
            let slot = match conn {
                None => inner.listener.as_mut().expect("listener installed"),
                Some(conn) => {
                    let attempt = inner.attempts.iter_mut().find(|a| a.conn == conn).unwrap();
                    if actor == RetainedActor::Writer {
                        attempt.writer.as_mut().unwrap()
                    } else {
                        &mut attempt.outer
                    }
                }
            };
            slot.pending = false;
            slot.outcome = Some(outcome.clone());
            inner.terminals.push(RetainedTerminal {
                conn,
                actor,
                outcome,
            });
            // Only this original attempt can release its UUID, after both
            // original native actors have actually returned from checked join.
            let mut released = Vec::new();
            for attempt in &mut inner.attempts {
                if !attempt.released
                    && attempt.outer.outcome.is_some()
                    && !attempt.outer.unresolved()
                    && attempt
                        .writer
                        .as_ref()
                        .is_none_or(|w| w.outcome.is_some() && !w.unresolved())
                    && let Some(uuid) = attempt.uuid
                {
                    attempt.released = true;
                    released.push(uuid);
                }
            }
            if !released.is_empty() {
                self.shared
                    .online
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retain(|(uuid, _)| !released.contains(uuid));
            }
        }
        fn harvest(&self) {
            let finished = {
                let mut inner = self.inner();
                let mut finished = Vec::new();
                if let Some(slot) = &mut inner.listener
                    && let Some(handle) = slot.take_finished()
                {
                    finished.push((None, RetainedActor::Listener, handle));
                }
                for attempt in &mut inner.attempts {
                    if let Some(handle) = attempt.outer.take_finished() {
                        finished.push((Some(attempt.conn), RetainedActor::Outer, handle));
                    }
                    if let Some(writer) = &mut attempt.writer
                        && let Some(handle) = writer.take_finished()
                    {
                        finished.push((Some(attempt.conn), RetainedActor::Writer, handle));
                    }
                }
                finished
            };
            for (conn, actor, handle) in finished {
                let outcome = handle.join().unwrap_or(RetainedOutcome::Panic);
                self.complete(conn, actor, outcome);
            }
        }
        fn quiescent(inner: &RegistryInner) -> bool {
            inner.listener.as_ref().is_none_or(|s| !s.unresolved())
                && inner.attempts.iter().all(|a| {
                    !a.outer.unresolved() && a.writer.as_ref().is_none_or(|s| !s.unresolved())
                })
        }
    }

    pub struct RetainedLan {
        registry: Arc<RetainedRegistry>,
        deadline: Option<Instant>,
        announcement: Option<discovery::RetainedPinger>,
    }
    impl RetainedLan {
        /// This constructor acquires no listener, thread, online service or POST.
        pub fn new(sync: Arc<data::SyncData>, config: RetainedOfflineConfig) -> io::Result<Self> {
            if config.max_players == 0
                || config.max_players > 1024
                || config.motd.len() > 32_767
                || config.compression_threshold < -1
                || config.compression_threshold > (2 << 20)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid retained offline configuration",
                ));
            }
            let mut ordinary = NetConfig::lan(config.port, config.motd.clone(), config.max_players);
            ordinary.bind = config.bind;
            ordinary.compression_threshold = config.compression_threshold;
            ordinary.enable_status = config.enable_status;
            ordinary.hide_online_players = config.hide_online_players;
            ordinary.announce = config.announce;
            ordinary.listen = false;
            let (events, disconnected) = channel();
            drop(disconnected);
            let shared = Arc::new(Shared {
                sync,
                events,
                motd: Mutex::new(config.motd),
                max_players: AtomicUsize::new(config.max_players),
                online: Mutex::new(Vec::new()),
                reserved: Mutex::new(HashSet::new()),
                next: AtomicU32::new(1),
                stop: AtomicBool::new(false),
                random: Mutex::new(1),
                config: ordinary,
                required_negotiation: None,
                stage: None,
            });
            Ok(Self {
                registry: Arc::new(RetainedRegistry {
                    inner: Mutex::new(RegistryInner::default()),
                    shared,
                    closing_latch: Arc::new(AtomicBool::new(false)),
                }),
                deadline: None,
                announcement: None,
            })
        }
        pub fn listen(&mut self) -> io::Result<u16> {
            {
                let mut inner = self.registry.inner();
                if inner.closing {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "retained LAN closing",
                    ));
                }
                if inner.listener.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "retained listener already attempted",
                    ));
                }
                // Reserve before bind/spawn, so a close cannot claim quiescence
                // while a native listener is being acquired or installed.
                inner.listener = Some(NativeSlot::starting());
            }
            let config = &self.registry.shared.config;
            let listener = match Lan::bind(config.bind, config.port) {
                Ok(listener) => listener,
                Err(error) => {
                    self.registry.complete(
                        None,
                        RetainedActor::Listener,
                        RetainedOutcome::Io(error.to_string()),
                    );
                    return Err(error);
                }
            };
            let port = match listener.local_addr() {
                Ok(address) => address.port(),
                Err(error) => {
                    self.registry.complete(
                        None,
                        RetainedActor::Listener,
                        RetainedOutcome::Io(error.to_string()),
                    );
                    return Err(error);
                }
            };
            let registry = self.registry.clone();
            let (release, start) = channel();
            #[cfg(test)]
            let (fail_spawn, panic_listener) = {
                let inner = self.registry.inner();
                (
                    inner.controls.fail_listener_spawn,
                    inner.controls.panic_listener,
                )
            };
            let builder = std::thread::Builder::new().name("horizon-retained-accept".into());
            #[cfg(test)]
            let builder = if fail_spawn {
                builder.stack_size(usize::MAX / 2)
            } else {
                builder
            };
            let spawned = builder.spawn(move || {
                if start.recv().is_err() {
                    return RetainedOutcome::QueueClosed;
                }
                #[cfg(test)]
                if panic_listener {
                    panic!("intentional retained native listener panic");
                }
                while !registry.inner().closing {
                    match listener.accept() {
                        Ok((stream, address)) => {
                            if let Err(error) = stream.set_nonblocking(false) {
                                registry.record(
                                    None,
                                    RetainedActor::Acquisition,
                                    RetainedOutcome::Io(error.to_string()),
                                );
                                continue;
                            }
                            #[cfg(all(
                                test,
                                feature = "native-writer-test-control",
                                debug_assertions,
                                target_os = "linux"
                            ))]
                            if let Err(error) = registry.configure_test_socket(&stream) {
                                registry.record(
                                    None,
                                    RetainedActor::Acquisition,
                                    RetainedOutcome::Io(error),
                                );
                                continue;
                            }
                            match Connection::from_stream(stream) {
                                Ok(connection) => {
                                    let _ = registry.start(connection, address);
                                }
                                Err(error) => registry.record(
                                    None,
                                    RetainedActor::Acquisition,
                                    RetainedOutcome::Io(error.to_string()),
                                ),
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => return RetainedOutcome::Io(error.to_string()),
                    }
                }
                RetainedOutcome::Returned
            });
            match spawned {
                Ok(handle) => {
                    let mut inner = self.registry.inner();
                    let slot = inner.listener.as_mut().unwrap();
                    slot.handle = Some(handle);
                    slot.pending = false;
                }
                Err(error) => {
                    self.registry.complete(
                        None,
                        RetainedActor::Listener,
                        RetainedOutcome::Spawn(error.to_string()),
                    );
                    return Err(error);
                }
            }
            let _ = release.send(());
            if config.announce {
                match discovery::RetainedPinger::start(self.registry.shared.motd(), port) {
                    Ok(pinger) => {
                        if self.registry.inner().closing {
                            pinger.request_stop();
                        }
                        self.announcement = Some(pinger);
                    }
                    Err(error) => {
                        let returned = io::Error::new(error.error.kind, error.clone());
                        self.registry.record(
                            None,
                            RetainedActor::Acquisition,
                            RetainedOutcome::PingerStartup(error),
                        );
                        return Err(returned);
                    }
                }
            }
            Ok(port)
        }
        pub fn connect_local(&self) -> io::Result<Pipe> {
            if self.registry.inner().closing {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "retained LAN closing",
                ));
            }
            let (client, server) = Pipe::pair();
            self.registry.start(
                Connection::local(server),
                SocketAddr::from(([127, 0, 0, 1], 0)),
            )?;
            Ok(client)
        }
        #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
        pub(super) fn retained_test_close_gate(
            &self,
            latched: Arc<std::sync::Barrier>,
            resume: Arc<std::sync::Barrier>,
        ) {
            self.registry.inner().controls.close_latched = Some((latched, resume));
        }
        #[cfg(all(
            test,
            feature = "native-writer-test-control",
            debug_assertions,
            target_os = "linux"
        ))]
        pub(super) fn retained_test_socket_send_buffer(&self, bytes: usize) {
            let mut inner = self.registry.inner();
            inner.controls.socket_send_buffer = Some(bytes);
            inner.controls.socket_send_buffer_result = None;
        }
        #[cfg(all(
            test,
            feature = "native-writer-test-control",
            debug_assertions,
            target_os = "linux"
        ))]
        pub(super) fn retained_test_socket_buffer(&self) -> Result<Option<usize>, String> {
            self.registry
                .inner()
                .controls
                .socket_send_buffer_result
                .clone()
                .transpose()
        }
        #[cfg(all(
            test,
            feature = "native-writer-test-control",
            debug_assertions,
            target_os = "linux"
        ))]
        pub(super) fn retained_test_writer_finished(&self, conn: u32) -> Option<bool> {
            let inner = self.registry.inner();
            inner
                .attempts
                .iter()
                .find(|attempt| attempt.conn == conn)
                .and_then(|attempt| attempt.writer.as_ref())
                .and_then(|writer| writer.handle.as_ref())
                .map(JoinHandle::is_finished)
        }
        pub fn poll_events(&self, max: usize) -> Vec<NetEvent> {
            let mut inner = self.registry.inner();
            let count = max.min(inner.events.len());
            inner.events.drain(..count).collect()
        }
        pub fn begin_close(&mut self, timeout: Duration) {
            if self.deadline.is_none() {
                let now = Instant::now();
                self.deadline = Some(now.checked_add(timeout).unwrap_or(now));
            }
            self.registry.request_close();
            if let Some(pinger) = &self.announcement {
                pinger.request_stop();
            }
        }
        pub fn retry_close(&mut self, timeout: Duration) {
            let now = Instant::now();
            self.deadline = Some(now.checked_add(timeout).unwrap_or(now));
            self.registry.request_close();
            if let Some(pinger) = &self.announcement {
                pinger.request_stop();
            }
        }
        pub fn poll_close(&mut self) -> RetainedCloseStatus {
            self.registry.harvest();
            if let Some(pinger) = &mut self.announcement {
                pinger.poll();
            }
            let observation = self.observe();
            if observation.quiescent {
                RetainedCloseStatus::Quiescent(observation)
            } else if observation.deadline_expired {
                RetainedCloseStatus::Blocked(observation)
            } else {
                RetainedCloseStatus::Pending(observation)
            }
        }
        pub fn observe(&self) -> RetainedObservation {
            // Snapshot queue Arcs under the registry lock; never take writer
            // queue locks while holding admission/custody linearization.
            let (closing, queued_events, listener_pending, quiescent, terminals, attempts) = {
                let inner = self.registry.inner();
                (
                    inner.closing,
                    inner.events.len(),
                    inner.listener.as_ref().is_some_and(NativeSlot::unresolved),
                    RetainedRegistry::quiescent(&inner),
                    inner.terminals.clone(),
                    inner
                        .attempts
                        .iter()
                        .map(|a| {
                            (
                                RetainedAttemptObservation {
                                    conn: a.conn,
                                    uuid: a.uuid,
                                    released: a.released,
                                    outer_pending: a.outer.unresolved(),
                                    writer_pending: a
                                        .writer
                                        .as_ref()
                                        .is_some_and(NativeSlot::unresolved),
                                    outbound: None,
                                },
                                a.outbound.clone(),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            };
            let announcement = self
                .announcement
                .as_ref()
                .map(discovery::RetainedPinger::observe);
            RetainedObservation {
                closing,
                queued_events,
                listener_pending,
                deadline_expired: self
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline),
                quiescent: closing && quiescent && announcement.as_ref().is_none_or(|a| a.joined),
                terminals,
                announcement,
                attempts: attempts
                    .into_iter()
                    .map(|(mut attempt, queue)| {
                        attempt.outbound = queue.map(|q| q.snapshot());
                        attempt
                    })
                    .collect(),
            }
        }
        pub fn try_closed(mut self) -> Result<ClosedLan, Self> {
            if !self.observe().quiescent {
                return Err(self);
            }
            let observation = self.observe();
            let announcement = if let Some(pinger) = self.announcement.take() {
                match pinger.try_dispose() {
                    Ok(terminal) => Some(terminal),
                    Err(pinger) => {
                        self.announcement = Some(pinger);
                        return Err(self);
                    }
                }
            } else {
                None
            };
            let events = self.poll_events(usize::MAX);
            Ok(ClosedLan {
                events,
                observation,
                announcement,
                _registry: self.registry.clone(),
            })
        }
    }
    impl Drop for RetainedLan {
        fn drop(&mut self) {
            // Handles may never become detached as an accidental unwind effect.
            let unresolved = !RetainedRegistry::quiescent(&self.registry.inner())
                || self
                    .announcement
                    .as_ref()
                    .is_some_and(|p| !p.observe().joined);
            if unresolved {
                let _ = std::io::Write::write_all(
                    &mut std::io::stderr(),
                    b"fatal: unresolved retained LAN native owner\n",
                );
                std::process::abort();
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::Write;

        pub(super) fn config() -> RetainedOfflineConfig {
            RetainedOfflineConfig {
                bind: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
                port: 0,
                motd: "Retained native test".into(),
                max_players: 2,
                compression_threshold: -1,
                enable_status: true,
                hide_online_players: false,
                announce: false,
            }
        }
        fn owner() -> RetainedLan {
            RetainedLan::new(retained_test_sync(), config()).expect("valid retained fixture")
        }
        fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if condition() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            condition()
        }
        fn close(mut lan: RetainedLan) -> ClosedLan {
            lan.begin_close(Duration::from_secs(5));
            let finished =
                wait_until(|| matches!(lan.poll_close(), RetainedCloseStatus::Quiescent(_)));
            if !finished {
                // An unresolved native owner cannot be hidden to make a failed
                // assertion unwind safely. Keep requesting the original owners.
                lan.retry_close(Duration::from_secs(5));
                if !wait_until(|| matches!(lan.poll_close(), RetainedCloseStatus::Quiescent(_))) {
                    let _ = std::io::Write::write_all(
                        &mut std::io::stderr(),
                        b"retained fixture cleanup could not reap its original native owners\n",
                    );
                    std::process::abort();
                }
            }
            match lan.try_closed() {
                Ok(closed) => closed,
                Err(_) => unreachable!("proved quiescent owner"),
            }
        }
        fn context(lan: &RetainedLan, conn: u32) -> RetainedContext {
            RetainedContext {
                registry: lan.registry.clone(),
                conn,
            }
        }

        #[test]
        fn retained_constructor_validates_before_any_native_acquisition() {
            let mut invalid = config();
            invalid.max_players = 0;
            let refused = RetainedLan::new(retained_test_sync(), invalid).is_err();
            let lan = owner();
            let before = lan.observe();
            let closed = close(lan);
            assert!(refused);
            assert!(before.attempts.is_empty());
            assert!(!before.listener_pending);
            assert!(!before.closing);
            assert!(closed.observation.terminals.is_empty());
        }

        #[test]
        fn retained_silent_tcp_registers_before_modern_read_and_reaps_listener() {
            let mut lan = owner();
            let port = lan.listen().expect("bind fresh retained listener");
            let client = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
                .expect("actual retained TCP peer");
            let registered = wait_until(|| lan.observe().attempts.len() == 1);
            #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
            let read_entered = wait_until(|| {
                let (entered, completed) = conn::retained_test_read_progress(1);
                entered > completed
            });
            let before = lan.observe();
            let closed = close(lan);
            drop(client);
            assert!(registered);
            #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
            assert!(
                read_entered,
                "actual modern native receive remained outstanding"
            );
            assert!(before.attempts[0].outer_pending);
            assert!(closed.observation.quiescent);
            assert!(
                closed
                    .observation
                    .terminals
                    .iter()
                    .any(|t| t.actor == RetainedActor::Listener)
            );
            assert!(
                closed
                    .observation
                    .terminals
                    .iter()
                    .any(|t| t.actor == RetainedActor::Outer)
            );
        }

        #[test]
        fn retained_local_partial_frame_is_interrupted_without_waiting_for_login() {
            let lan = owner();
            let mut peer = lan.connect_local().expect("actual retained Pipe peer");
            let wrote_prefix = peer.write_all(&[0x80]).is_ok();
            #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
            let read_entered = wait_until(|| {
                let (entered, completed) = conn::retained_test_read_progress(1);
                entered > completed
            });
            let closed = close(lan);
            let refused_after_close = peer.write_all(&[0]).is_err();
            assert!(wrote_prefix);
            #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
            assert!(
                read_entered,
                "actual native partial-frame receive remained outstanding"
            );
            assert!(refused_after_close);
            assert_eq!(closed.observation.attempts.len(), 1);
            assert!(!closed.observation.attempts[0].outer_pending);
        }

        #[test]
        fn retained_close_racing_registered_outer_preserves_pending_install_and_deadline() {
            let mut lan = owner();
            let registered = Arc::new(std::sync::Barrier::new(2));
            let resume = Arc::new(std::sync::Barrier::new(2));
            lan.registry.inner().controls.outer_registered =
                Some((registered.clone(), resume.clone()));
            let port = lan.listen().expect("fresh listener");
            let peer =
                TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).expect("native peer");
            registered.wait();
            lan.begin_close(Duration::ZERO);
            let first_deadline = lan.deadline;
            let blocked = matches!(lan.poll_close(), RetainedCloseStatus::Blocked(_));
            lan.begin_close(Duration::from_secs(60));
            let repeated_deadline = lan.deadline;
            let refused_local = lan.connect_local().is_err();
            lan.retry_close(Duration::from_secs(5));
            let renewed = lan.deadline > first_deadline;
            resume.wait();
            let closed = close(lan);
            drop(peer);
            assert!(blocked);
            assert_eq!(first_deadline, repeated_deadline);
            assert!(refused_local);
            assert!(renewed);
            assert!(closed.observation.quiescent);
            assert_eq!(closed.observation.attempts.len(), 1);
        }

        #[test]
        fn retained_duplicate_reservation_releases_only_after_original_checked_terminal() {
            let lan = owner();
            let first = lan.connect_local().expect("first native Pipe");
            let second = lan.connect_local().expect("second native Pipe");
            let a = context(&lan, 1);
            let b = context(&lan, 2);
            let uuid = horizon_protocol::crypto::offline_uuid("ExactOwner");
            let initial = a.admit(&lan.registry.shared, uuid, "ExactOwner");
            let duplicate = b.admit(&lan.registry.shared, uuid, "ExactOwner");
            first.shutdown();
            let released = wait_until(|| {
                lan.registry.harvest();
                lan.observe().attempts[0].released
            });
            let replacement = b.admit(&lan.registry.shared, uuid, "ExactOwner");
            // Re-observing/reaping the already released attempt cannot remove
            // the new exact reservation held by attempt two.
            lan.registry.harvest();
            let replacement_online = lan.registry.shared.online();
            let stale = a.admit(&lan.registry.shared, uuid, "ExactOwner");
            let closed = close(lan);
            second.shutdown();
            assert!(initial.is_none());
            assert!(duplicate.is_some());
            assert!(released);
            assert!(replacement.is_none());
            assert_eq!(replacement_online, 1);
            assert!(stale.is_some());
            assert!(closed.observation.attempts.iter().all(|a| a.released));
        }

        #[test]
        fn retained_polling_preserves_terminal_history_and_unconsumed_payloads() {
            let lan = owner();
            let peer = lan.connect_local().expect("native mailbox attempt");
            let context = context(&lan, 1);
            let first = context.emit(NetEvent::Packet(
                1,
                Box::new(play::Serverbound::Unknown(7, vec![11])),
            ));
            let second = context.emit(NetEvent::Packet(
                1,
                Box::new(play::Serverbound::Unknown(7, vec![22])),
            ));
            let empty = lan.poll_events(0).is_empty();
            let polled = lan.poll_events(1);
            let closed = close(lan);
            peer.shutdown();
            assert!(first && second && empty);
            assert_eq!(polled.len(), 1);
            assert_eq!(closed.events.len(), 1);
            let NetEvent::Packet(1, packet) = &closed.events[0] else {
                panic!("retained packet");
            };
            assert!(matches!(&**packet, play::Serverbound::Unknown(7, body) if body == &[22]));
            assert!(
                closed
                    .observation
                    .terminals
                    .iter()
                    .any(|t| t.actor == RetainedActor::Outer)
            );
        }

        #[test]
        #[cfg(target_os = "linux")]
        fn retained_native_spawn_refusals_keep_listener_and_outer_attempt_history() {
            let mut listener = owner();
            listener.registry.inner().controls.fail_listener_spawn = true;
            let listener_failed = listener.listen().is_err();
            let listener_closed = close(listener);
            let outer = owner();
            outer.registry.inner().controls.fail_outer_spawn = true;
            let outer_failed = outer.connect_local().is_err();
            let outer_closed = close(outer);
            assert!(listener_failed && outer_failed);
            assert!(
                listener_closed
                    .observation
                    .terminals
                    .iter()
                    .any(|t| t.actor == RetainedActor::Listener
                        && matches!(t.outcome, RetainedOutcome::Spawn(_)))
            );
            assert!(
                outer_closed
                    .observation
                    .terminals
                    .iter()
                    .any(|t| t.actor == RetainedActor::Outer
                        && matches!(t.outcome, RetainedOutcome::Spawn(_)))
            );
            assert_eq!(outer_closed.observation.attempts.len(), 1);
        }

        #[test]
        fn retained_listener_panic_is_joined_once_and_remains_visible() {
            let mut lan = owner();
            lan.registry.inner().controls.panic_listener = true;
            let listening = lan.listen().is_ok();
            let closed = close(lan);
            assert!(listening);
            assert_eq!(
                closed
                    .observation
                    .terminals
                    .iter()
                    .filter(|t| t.actor == RetainedActor::Listener
                        && t.outcome == RetainedOutcome::Panic)
                    .count(),
                1
            );
        }

        #[test]
        #[ignore = "root executes separately and checks exact fatal diagnostic and exit"]
        fn retained_unresolved_drop_fatal_process() {
            let lan = owner();
            let _peer = lan
                .connect_local()
                .expect("fatal fixture actual native owner");
            eprintln!("RETAINED_LAN_UNRESOLVED_DROP_READY");
            drop(lan);
            panic!("unresolved retained LAN Drop returned");
        }
    }
}

#[cfg(all(test, feature = "paired-packages"))]
mod staged_transport_tests {
    use super::*;
    use horizon_protocol::connection::{Connection, Pipe};

    struct MailboxFixture {
        stage: StagedState,
        clients: Vec<Connection>,
        _servers: Vec<Pipe>,
    }

    fn mailbox() -> MailboxFixture {
        let mut attempts = Vec::new();
        let mut clients = Vec::new();
        let mut servers = Vec::new();
        for conn in 1..=3 {
            let (client, server) = Pipe::pair();
            attempts.push(StagedAttempt {
                conn,
                shutdown: server.shutdown_handle(),
                profile: None,
                joined: false,
                pending: Arc::default(),
                terminal: None,
            });
            let client = Connection::local(client);
            client
                .reader
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            clients.push(client);
            servers.push(server);
        }
        MailboxFixture {
            stage: StagedState {
                profiles: StagedProfiles::memory_pair(),
                inner: Mutex::new(StagedInner {
                    attempts,
                    ..Default::default()
                }),
                closing: AtomicBool::new(false),
                outbound: Arc::default(),
            },
            clients,
            _servers: servers,
        }
    }

    // Unknown payloads exercise the raw mailbox boundary, before the Hub's
    // separate packet allowlist. They create no Guest or readiness evidence.
    fn packet(marker: u8, bytes: usize) -> NetEvent {
        NetEvent::Packet(
            1,
            Box::new(play::Serverbound::Unknown(0x7fff, vec![marker; bytes])),
        )
    }

    #[test]
    fn bounded_poll_preserves_order_and_releases_exact_payload_charges() {
        let fixture = mailbox();
        for marker in 0..40u8 {
            let bytes = usize::from(marker) + 1;
            assert!(fixture.stage.emit(1, packet(marker, bytes), bytes));
        }
        assert_eq!(fixture.stage.inner().bytes, 820);
        assert!(fixture.stage.poll(0).unwrap().is_empty());
        assert_eq!(fixture.stage.inner().events.len(), 40);
        let mut observed = Vec::new();
        let first = fixture.stage.poll(usize::MAX).unwrap();
        assert_eq!(first.len(), 16);
        assert_eq!(fixture.stage.inner().events.len(), 24);
        assert_eq!(fixture.stage.inner().bytes, 820 - 136);
        for event in first
            .into_iter()
            .chain(fixture.stage.poll(7).unwrap())
            .chain(fixture.stage.poll(16).unwrap())
            .chain(fixture.stage.poll(16).unwrap())
        {
            let NetEvent::Packet(1, packet) = event else {
                panic!("unexpected mailbox event");
            };
            let play::Serverbound::Unknown(0x7fff, payload) = *packet else {
                panic!("unexpected raw packet");
            };
            observed.push(payload[0]);
        }
        assert_eq!(observed, (0..40u8).collect::<Vec<_>>());
        let inner = fixture.stage.inner();
        assert!(inner.events.is_empty());
        assert_eq!(inner.bytes, 0);
        assert_eq!(inner.failure, None);
    }

    #[test]
    fn event_overflow_keeps_all_reserved_terminals_and_closes_native_endpoints() {
        let mut fixture = mailbox();
        for marker in 0..64u8 {
            assert!(fixture.stage.emit(1, packet(marker, 1), 1));
        }
        assert!(!fixture.stage.emit(1, packet(64, 1), 1));
        assert_eq!(
            fixture.stage.poll(16).err(),
            Some(StagedFailure::InboundOverflow)
        );
        assert_eq!(fixture.stage.inner().events.len(), 64);
        assert_eq!(fixture.stage.inner().bytes, 64);
        for conn in 1..=3 {
            fixture.stage.terminal(conn, StagedFailure::ConnectionEnded);
        }
        // Repeated notices neither overwrite the first notice nor consume a
        // data slot, even when the data mailbox was already completely full.
        fixture.stage.terminal(2, StagedFailure::Decode);
        let terminals: Vec<_> = fixture
            .stage
            .inner()
            .attempts
            .iter()
            .filter_map(|a| a.terminal)
            .collect();
        assert_eq!(
            terminals,
            (1..=3)
                .map(|conn| StagedTerminal {
                    conn,
                    reason: StagedFailure::ConnectionEnded
                })
                .collect::<Vec<_>>()
        );
        assert_eq!(
            fixture.stage.poll(usize::MAX).err(),
            Some(StagedFailure::InboundOverflow)
        );
        for client in &mut fixture.clients {
            assert!(client.send(&[0]).is_err());
            assert!(client.recv().is_err());
        }
    }

    #[test]
    fn byte_overflow_refuses_before_retaining_excess_and_precedes_data_poll() {
        let fixture = mailbox();
        for marker in 0..4u8 {
            assert!(fixture.stage.emit(1, packet(marker, 64 << 10), 64 << 10));
        }
        assert_eq!(fixture.stage.inner().bytes, 256 << 10);
        assert!(!fixture.stage.emit(1, packet(4, 1), 1));
        fixture.stage.terminal(1, StagedFailure::ConnectionEnded);
        assert_eq!(
            fixture.stage.poll(1).err(),
            Some(StagedFailure::InboundOverflow)
        );
        let inner = fixture.stage.inner();
        assert_eq!(inner.events.len(), 4);
        assert_eq!(inner.bytes, 256 << 10);
        assert_eq!(
            inner
                .attempts
                .iter()
                .filter(|a| a.terminal.is_some())
                .count(),
            1
        );
    }
}
