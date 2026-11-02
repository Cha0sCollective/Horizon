//! LAN game discovery: LanServerPinger announces a hosted world on the
//! multicast group 224.0.2.60:4445 every 1.5 seconds as
//! `[MOTD]text[/MOTD][AD]port[/AD]`, and LanServerDetection collects those
//! announcements for the multiplayer screen.
use std::{
    io,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

pub const GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 2, 60);
pub const PORT: u16 = 4445;
/// LanServerPinger sleeps 1500 ms between announcements.
const INTERVAL: Duration = Duration::from_millis(1500);

/// LanServerPinger.createPingString.
pub fn ping_string(motd: &str, port: u16) -> String {
    ping_string_text(motd, &port.to_string())
}
/// LanServerPinger.createPingString with the address as text.
pub fn ping_string_text(motd: &str, address: &str) -> String {
    format!("[MOTD]{motd}[/MOTD][AD]{address}[/AD]")
}
/// String.indexOf(needle, from) on a Rust string (byte offsets).
fn index_of(s: &str, needle: &str, from: usize) -> Option<usize> {
    s.get(from..)?.find(needle).map(|i| i + from)
}
/// LanServerPinger.parseMotd; "missing no" when there is none.
pub fn parse_motd(s: &str) -> String {
    let Some(start) = s.find("[MOTD]") else {
        return "missing no".to_owned();
    };
    match index_of(s, "[/MOTD]", start + "[MOTD]".len()) {
        Some(end) => s[start + "[MOTD]".len()..end].to_owned(),
        None => "missing no".to_owned(),
    }
}
/// LanServerPinger.parseAddress: the text between [AD] and [/AD] after the
/// only [/MOTD], as announced (vanilla does not check it is a port).
pub fn parse_address(s: &str) -> Option<String> {
    let end_motd = s.find("[/MOTD]")?;
    if index_of(s, "[/MOTD]", end_motd + "[/MOTD]".len()).is_some() {
        return None;
    }
    let start = index_of(s, "[AD]", end_motd + "[/MOTD]".len())?;
    let end = index_of(s, "[/AD]", start + "[AD]".len())?;
    Some(s[start + "[AD]".len()..end].to_owned())
}
/// LanServerDetection.LanServerList.addServer: an announcement from `ip`.
/// A new address joins the list; a known one keeps its first MOTD. True
/// when the list changed.
pub fn add_server(games: &mut Vec<LanGame>, ping: &str, ip: &str) -> bool {
    let motd = parse_motd(ping);
    let Some(address) = parse_address(ping) else {
        return false;
    };
    let address = format!("{ip}:{address}");
    if games.iter().any(|g| g.address == address) {
        return false;
    }
    games.push(LanGame { motd, address });
    true
}

/// Announces one hosted world until dropped.
pub struct Pinger {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Pinger {
    pub fn start(motd: String, port: u16) -> io::Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", 0))?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let message = ping_string(&motd, port);
        let thread = std::thread::Builder::new()
            .name("horizon-lan-ping".into())
            .spawn(move || {
                let target = SocketAddr::V4(SocketAddrV4::new(GROUP, PORT));
                while !flag.load(Ordering::Relaxed) {
                    let _ = socket.send_to(message.as_bytes(), target);
                    let mut slept = Duration::ZERO;
                    while slept < INTERVAL && !flag.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(50));
                        slept += Duration::from_millis(50);
                    }
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}
impl Drop for Pinger {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A native UDP error retained independently of the worker's lifetime.
#[cfg(feature = "paired-packages")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedPingerIoError {
    pub kind: io::ErrorKind,
    pub raw_os_error: Option<i32>,
    pub message: String,
}

#[cfg(feature = "paired-packages")]
impl std::fmt::Display for RetainedPingerIoError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[cfg(feature = "paired-packages")]
impl std::error::Error for RetainedPingerIoError {}

#[cfg(feature = "paired-packages")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetainedPingerStartPhase {
    Bind,
    SocketSetup,
    Spawn,
}

/// Failure before a native worker was acquired; the original OS evidence remains.
#[cfg(feature = "paired-packages")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedPingerStartError {
    pub phase: RetainedPingerStartPhase,
    pub error: RetainedPingerIoError,
}

#[cfg(feature = "paired-packages")]
impl RetainedPingerStartError {
    fn new(phase: RetainedPingerStartPhase, error: io::Error) -> Self {
        Self {
            phase,
            error: RetainedPingerIoError {
                kind: error.kind(),
                raw_os_error: error.raw_os_error(),
                message: error.to_string(),
            },
        }
    }
}

#[cfg(feature = "paired-packages")]
impl std::fmt::Display for RetainedPingerStartError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:?}: {}", self.phase, self.error)
    }
}

#[cfg(feature = "paired-packages")]
impl std::error::Error for RetainedPingerStartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

#[cfg(feature = "paired-packages")]
#[derive(Clone, Debug)]
pub enum RetainedPingerExit {
    Stopped,
    SendFailed,
    Panicked(String),
}

/// `joined` records a checked native join, rather than a stop request or EOF.
#[cfg(feature = "paired-packages")]
#[derive(Clone, Debug)]
pub struct RetainedPingerObservation {
    pub stop_requested: bool,
    pub worker_finished: bool,
    pub joined: bool,
    pub attempted: u64,
    pub sent: u64,
    pub send_errors: Vec<RetainedPingerIoError>,
    pub terminal: Option<RetainedPingerExit>,
}

#[cfg(feature = "paired-packages")]
#[derive(Clone, Debug)]
pub struct RetainedPingerTerminal {
    pub observation: RetainedPingerObservation,
}

#[cfg(feature = "paired-packages")]
#[derive(Default)]
struct RetainedPingCounts {
    attempted: u64,
    sent: u64,
    send_errors: Vec<RetainedPingerIoError>,
}

#[cfg(feature = "paired-packages")]
#[derive(Default)]
struct RetainedPingControl {
    #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
    panic_before_send: bool,
    #[cfg(all(
        test,
        feature = "native-writer-test-control",
        debug_assertions,
        target_os = "linux"
    ))]
    acquisition: RetainedPingAcquisitionControl,
}

#[cfg(all(
    feature = "paired-packages",
    test,
    feature = "native-writer-test-control",
    debug_assertions,
    target_os = "linux"
))]
#[derive(Clone, Copy, Default)]
enum RetainedPingAcquisitionControl {
    #[default]
    Ordinary,
    OccupiedBind(SocketAddr),
    RefuseSpawn,
}

#[cfg(feature = "paired-packages")]
impl RetainedPingControl {
    fn before_send(&self) {
        let _ = self;
        #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
        if self.panic_before_send {
            panic!("retained native Pinger test panic before send");
        }
    }
}

/// Retains the actual announcement worker until a finished-only join.
/// Native socket setup, send and short locks have no hard latency guarantee.
#[cfg(feature = "paired-packages")]
pub(super) struct RetainedPinger {
    stop: Arc<AtomicBool>,
    counts: Arc<Mutex<RetainedPingCounts>>,
    thread: Option<JoinHandle<RetainedPingerExit>>,
    terminal: Option<RetainedPingerExit>,
}

#[cfg(feature = "paired-packages")]
impl RetainedPinger {
    pub(super) fn start(motd: String, port: u16) -> Result<Self, RetainedPingerStartError> {
        Self::start_controlled(motd, port, RetainedPingControl::default())
    }

    fn start_controlled(
        motd: String,
        port: u16,
        control: RetainedPingControl,
    ) -> Result<Self, RetainedPingerStartError> {
        let bind_address = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));
        #[cfg(all(
            test,
            feature = "native-writer-test-control",
            debug_assertions,
            target_os = "linux"
        ))]
        let bind_address = match control.acquisition {
            RetainedPingAcquisitionControl::OccupiedBind(address) => address,
            _ => bind_address,
        };
        let socket = UdpSocket::bind(bind_address).map_err(|error| {
            RetainedPingerStartError::new(RetainedPingerStartPhase::Bind, error)
        })?;
        socket.set_nonblocking(true).map_err(|error| {
            RetainedPingerStartError::new(RetainedPingerStartPhase::SocketSetup, error)
        })?;
        let stop = Arc::new(AtomicBool::new(false));
        let counts = Arc::new(Mutex::new(RetainedPingCounts::default()));
        let flag = stop.clone();
        let worker_counts = counts.clone();
        let message = ping_string(&motd, port);
        // No fallible setup follows successful spawn: custody is returned intact.
        let builder = std::thread::Builder::new().name("horizon-lan-ping-retained".into());
        #[cfg(all(
            test,
            feature = "native-writer-test-control",
            debug_assertions,
            target_os = "linux"
        ))]
        let builder = if matches!(
            control.acquisition,
            RetainedPingAcquisitionControl::RefuseSpawn
        ) {
            builder.stack_size(usize::MAX / 2)
        } else {
            builder
        };
        let thread = builder
            .spawn(move || {
                let target = SocketAddr::V4(SocketAddrV4::new(GROUP, PORT));
                while !flag.load(Ordering::Acquire) {
                    control.before_send();
                    worker_counts
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .attempted += 1;
                    let result = socket.send_to(message.as_bytes(), target);
                    {
                        let mut counts = worker_counts
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        match result {
                            Ok(_) => counts.sent += 1,
                            Err(error) => {
                                counts.send_errors.push(RetainedPingerIoError {
                                    kind: error.kind(),
                                    raw_os_error: error.raw_os_error(),
                                    message: error.to_string(),
                                });
                                return RetainedPingerExit::SendFailed;
                            }
                        }
                    }
                    if !flag.load(Ordering::Acquire) {
                        std::thread::park_timeout(INTERVAL);
                    }
                }
                RetainedPingerExit::Stopped
            })
            .map_err(|error| {
                RetainedPingerStartError::new(RetainedPingerStartPhase::Spawn, error)
            })?;
        Ok(Self {
            stop,
            counts,
            thread: Some(thread),
            terminal: None,
        })
    }

    pub(super) fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = &self.thread {
            thread.thread().unpark();
        }
    }

    pub(super) fn poll(&mut self) {
        if !self.thread.as_ref().is_some_and(JoinHandle::is_finished) {
            return;
        }
        if let Some(thread) = self.thread.take() {
            self.terminal = Some(match thread.join() {
                Ok(exit) => exit,
                Err(panic) => {
                    let message = if let Some(message) = panic.downcast_ref::<String>() {
                        message.clone()
                    } else if let Some(message) = panic.downcast_ref::<&str>() {
                        (*message).to_owned()
                    } else {
                        "non-string native Pinger panic payload".to_owned()
                    };
                    RetainedPingerExit::Panicked(message)
                }
            });
        }
    }

    pub(super) fn observe(&self) -> RetainedPingerObservation {
        let counts = self
            .counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        RetainedPingerObservation {
            stop_requested: self.stop.load(Ordering::Acquire),
            worker_finished: self.terminal.is_some()
                || self.thread.as_ref().is_some_and(JoinHandle::is_finished),
            joined: self.terminal.is_some() && self.thread.is_none(),
            attempted: counts.attempted,
            sent: counts.sent,
            send_errors: counts.send_errors.clone(),
            terminal: self.terminal.clone(),
        }
    }

    pub(super) fn try_dispose(self) -> Result<RetainedPingerTerminal, Self> {
        if self.terminal.is_some() && self.thread.is_none() {
            Ok(RetainedPingerTerminal {
                observation: self.observe(),
            })
        } else {
            Err(self)
        }
    }
}

#[cfg(feature = "paired-packages")]
impl Drop for RetainedPinger {
    fn drop(&mut self) {
        if self.thread.is_some() {
            eprintln!("fatal: retained LAN Pinger dropped before checked join");
            std::process::abort();
        }
    }
}

/// A world announced on the local network.
#[derive(Clone, Debug, PartialEq)]
pub struct LanGame {
    pub motd: String,
    pub address: String,
}
type Games = Arc<Mutex<Vec<LanGame>>>;

/// Listens for announcements until the last clone is dropped.
#[derive(Clone, Debug)]
pub struct Scanner {
    inner: Arc<ScanInner>,
}
#[derive(Debug)]
struct ScanInner {
    games: Games,
    stop: Arc<AtomicBool>,
}
impl Drop for ScanInner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
impl Scanner {
    /// Bind the discovery port; without it the list simply stays empty.
    pub fn start() -> Self {
        let games: Games = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        if let Ok(socket) = bind_listener() {
            let (games, stop) = (games.clone(), stop.clone());
            let _ = std::thread::Builder::new()
                .name("horizon-lan-scan".into())
                .spawn(move || listen(socket, games, stop));
        }
        Self {
            inner: Arc::new(ScanInner { games, stop }),
        }
    }
    /// The worlds announced since the scan started, in order (vanilla's
    /// list keeps them while the multiplayer screen is open).
    pub fn games(&self) -> Vec<LanGame> {
        self.inner.games.lock().unwrap().clone()
    }
}
fn bind_listener() -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind(("0.0.0.0", PORT))?;
    socket.join_multicast_v4(&GROUP, &Ipv4Addr::UNSPECIFIED)?;
    socket.set_read_timeout(Some(Duration::from_millis(250)))?;
    Ok(socket)
}
fn listen(socket: UdpSocket, games: Games, stop: Arc<AtomicBool>) {
    let mut buf = [0u8; 1024];
    while !stop.load(Ordering::Relaxed) {
        let Ok((n, from)) = socket.recv_from(&mut buf) else {
            continue;
        };
        let text = String::from_utf8_lossy(&buf[..n]);
        add_server(&mut games.lock().unwrap(), &text, &from.ip().to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_string_round_trips() {
        let s = ping_string("Steve - World", 25565);
        assert_eq!(s, "[MOTD]Steve - World[/MOTD][AD]25565[/AD]");
        assert_eq!(parse_motd(&s), "Steve - World");
        assert_eq!(parse_address(&s).as_deref(), Some("25565"));
        assert_eq!(parse_address("[AD]1[/AD]"), None);
        assert_eq!(parse_address("[/MOTD][AD]x[/AD][/MOTD]"), None);
        assert_eq!(parse_motd("junk"), "missing no");
        let mut games = Vec::new();
        assert!(add_server(&mut games, &s, "10.0.0.2"));
        assert!(!add_server(
            &mut games,
            &ping_string("Other", 25565),
            "10.0.0.2"
        ));
        assert_eq!(games[0].motd, "Steve - World");
    }
}

#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
mod retained_tests {
    use super::*;
    use std::time::Instant;

    fn wait_for(
        pinger: &RetainedPinger,
        predicate: fn(&RetainedPingerObservation) -> bool,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if predicate(&pinger.observe()) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    // Always stop and reap actual native custody before asserting a test result.
    fn finish(mut pinger: RetainedPinger) -> RetainedPingerObservation {
        pinger.request_stop();
        loop {
            pinger.poll();
            if pinger.observe().joined {
                return match pinger.try_dispose() {
                    Ok(terminal) => terminal.observation,
                    Err(owner) => finish(owner),
                };
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn retained_pinger_native_udp_start_stop() {
        let pinger = RetainedPinger::start("retained UDP start stop".into(), 25565)
            .expect("bind and spawn actual native Pinger");
        let observed_send = wait_for(&pinger, |observation| {
            observation.sent > 0 || observation.worker_finished
        });
        let observation = finish(pinger);
        assert!(observed_send);
        assert!(observation.stop_requested);
        assert!(observation.worker_finished);
        assert!(observation.joined);
        assert!(observation.attempted > 0);
        assert!(observation.sent > 0);
        assert!(observation.send_errors.is_empty());
        assert!(matches!(
            observation.terminal,
            Some(RetainedPingerExit::Stopped)
        ));
    }

    #[test]
    fn retained_pinger_native_send_failure_survives_join() {
        // The native UDP send rejects a datagram exceeding the IPv4 payload limit.
        let pinger = RetainedPinger::start("x".repeat(65_536), 25565)
            .expect("bind and spawn oversized native UDP announcement");
        let finished = wait_for(&pinger, |observation| observation.worker_finished);
        let observation = finish(pinger);
        assert!(finished);
        assert_eq!(observation.attempted, 1);
        assert_eq!(observation.sent, 0);
        assert_eq!(observation.send_errors.len(), 1);
        assert!(!observation.send_errors[0].message.is_empty());
        assert!(matches!(
            observation.terminal,
            Some(RetainedPingerExit::SendFailed)
        ));
    }

    #[test]
    fn retained_pinger_native_worker_panic_survives_join() {
        let pinger = RetainedPinger::start_controlled(
            "retained UDP panic".into(),
            25565,
            RetainedPingControl {
                panic_before_send: true,
                #[cfg(target_os = "linux")]
                acquisition: RetainedPingAcquisitionControl::Ordinary,
            },
        )
        .expect("bind and spawn actual native panic-controlled Pinger");
        let finished = wait_for(&pinger, |observation| observation.worker_finished);
        let observation = finish(pinger);
        assert!(finished);
        assert_eq!(observation.attempted, 0);
        assert_eq!(observation.sent, 0);
        assert!(matches!(
            observation.terminal,
            Some(RetainedPingerExit::Panicked(ref message))
                if message == "retained native Pinger test panic before send"
        ));
    }

    #[test]
    fn retained_pinger_finished_requires_explicit_checked_join() {
        let pinger = RetainedPinger::start("x".repeat(65_536), 25565)
            .expect("bind and spawn actual native Pinger");
        let finished = wait_for(&pinger, |observation| observation.worker_finished);
        let before = pinger.observe();
        let (pinger, refused_unjoined) = match pinger.try_dispose() {
            Err(owner) => (owner, true),
            Ok(_) => panic!("unjoined Pinger was disposed"),
        };
        let observation = finish(pinger);
        assert!(finished);
        assert!(!before.joined);
        assert!(before.terminal.is_none());
        assert!(refused_unjoined);
        assert!(observation.joined);
        assert!(observation.terminal.is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn retained_pinger_native_acquisition_failures_keep_exact_phase() {
        let occupied = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .expect("occupy an actual native UDP address without reuse");
        let address = occupied.local_addr().expect("occupied UDP address");
        let bind_error = match RetainedPinger::start_controlled(
            "retained occupied native UDP bind".into(),
            25565,
            RetainedPingControl {
                panic_before_send: false,
                acquisition: RetainedPingAcquisitionControl::OccupiedBind(address),
            },
        ) {
            Err(error) => Some(error),
            Ok(owner) => {
                finish(owner);
                None
            }
        };
        drop(occupied);
        let spawn_error = match RetainedPinger::start_controlled(
            "retained native spawn refusal".into(),
            25565,
            RetainedPingControl {
                panic_before_send: false,
                acquisition: RetainedPingAcquisitionControl::RefuseSpawn,
            },
        ) {
            Err(error) => Some(error),
            Ok(owner) => {
                finish(owner);
                None
            }
        };
        // Both real acquisition attempts have released/reaped their resources.
        let bind_error = bind_error.expect("the occupied native bind must fail");
        assert_eq!(bind_error.phase, RetainedPingerStartPhase::Bind);
        assert_eq!(bind_error.error.kind, io::ErrorKind::AddrInUse);
        assert!(bind_error.error.raw_os_error.is_some());
        assert!(!bind_error.error.message.is_empty());
        let spawn_error = spawn_error.expect("the native MAX/2 stack spawn must fail");
        assert_eq!(spawn_error.phase, RetainedPingerStartPhase::Spawn);
        assert!(spawn_error.error.raw_os_error.is_some());
        assert!(!spawn_error.error.message.is_empty());
        assert_ne!(spawn_error.error.kind, io::ErrorKind::AddrInUse);
    }

    #[test]
    #[ignore = "fatal process: root verifies marker, diagnostic, abort and reaping"]
    fn retained_pinger_unresolved_drop_is_fatal() {
        let pinger = RetainedPinger::start("retained UDP fatal drop".into(), 25565)
            .expect("bind and spawn actual native Pinger");
        eprintln!("S1TL1_RETAINED_PINGER_DROP_ARMED");
        drop(pinger);
        panic!("unresolved retained Pinger drop returned");
    }
}
