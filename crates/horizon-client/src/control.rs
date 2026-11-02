//! Local control socket: lets a script or `horizon-ctl` drive a running game.
//!
//! Each connection sends commands as text lines and receives one JSON line per
//! command. Commands are executed on the game thread between frames; replies
//! that depend on a later frame (screenshots, benchmarks) arrive when ready.
//!
//! Two transports carry the same lines. On Linux and macOS the endpoint is a
//! Unix domain socket. Where those don't exist (Windows), or when the endpoint
//! path ends in `.json`, the game listens on a random localhost TCP port and
//! writes `{"transport":"tcp","port":N,"token":"..."}` to the path, readable
//! only by its user; a client's first line must then be `auth TOKEN`.
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};

#[cfg(unix)]
pub use std::os::unix::net::{UnixListener, UnixStream};

/// A connection to the control endpoint, over either transport.
pub enum Stream {
    #[cfg(unix)]
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Stream {
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(match self {
            #[cfg(unix)]
            Self::Unix(s) => Self::Unix(s.try_clone()?),
            Self::Tcp(s) => Self::Tcp(s.try_clone()?),
        })
    }
    /// A read timeout (None waits forever).
    pub fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(s) => s.set_read_timeout(timeout),
            Self::Tcp(s) => s.set_read_timeout(timeout),
        }
    }
}
impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(s) => s.read(buf),
            Self::Tcp(s) => s.read(buf),
        }
    }
}
impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(s) => s.write(buf),
            Self::Tcp(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(s) => s.flush(),
            Self::Tcp(s) => s.flush(),
        }
    }
}

/// Whether the endpoint at `path` is a TCP port described by a JSON file.
pub fn uses_tcp(path: &Path) -> bool {
    !cfg!(unix) || path.extension().is_some_and(|e| e == "json")
}

/// The TCP endpoint file: (port, token).
fn read_endpoint(path: &Path) -> io::Result<(u16, String)> {
    let text = std::fs::read_to_string(path)?;
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "not a control endpoint file");
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|_| bad())?;
    let port = v["port"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok())
        .ok_or_else(bad)?;
    let token = v["token"].as_str().ok_or_else(bad)?.to_owned();
    Ok((port, token))
}

/// Connect to a running game's control endpoint (and authenticate over TCP).
pub fn connect(path: &Path) -> io::Result<Stream> {
    #[cfg(unix)]
    if !uses_tcp(path) {
        return UnixStream::connect(path).map(Stream::Unix);
    }
    let (port, token) = read_endpoint(path)?;
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))?;
    stream.set_nodelay(true)?;
    writeln!(stream, "auth {token}")?;
    let mut reply = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut reply)?;
    if !reply.contains("\"ok\"") {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("the game refused the token: {}", reply.trim()),
        ));
    }
    Ok(Stream::Tcp(stream))
}

/// 128 random bits as hex, from the standard library's OS-seeded hasher keys.
fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    (0..2)
        .map(|i| {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u64(i);
            h.write_u128(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos()),
            );
            h.write_u32(std::process::id());
            format!("{:016x}", h.finish())
        })
        .collect()
}

/// Write the endpoint file so that only this user can read it.
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(text.as_bytes())
}

/// Constant-time comparison of the presented token.
fn same_token(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// One command line from a client, with the channel for its JSON reply.
pub struct Request {
    pub line: String,
    pub reply: mpsc::Sender<String>,
}

pub struct Server {
    path: PathBuf,
    requests: mpsc::Receiver<Request>,
    sender: mpsc::Sender<Request>,
    /// Clients connected now (an MCP server keeps one open; horizon-ctl one per command).
    clients: Arc<AtomicUsize>,
    /// Set when the server is dropped: the listener stops accepting and exits.
    closed: Arc<AtomicBool>,
    /// The TCP port, to wake the listener when closing.
    port: Option<u16>,
}

/// `$HORIZON_CONTROL`, else `horizon-control.sock` in the user's runtime
/// directory (`horizon-control.json` in the temporary directory on Windows).
pub fn default_path() -> PathBuf {
    if let Some(path) = std::env::var_os("HORIZON_CONTROL") {
        return path.into();
    }
    if !cfg!(unix) {
        return std::env::temp_dir().join("horizon-control.json");
    }
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    dir.join("horizon-control.sock")
}

impl Server {
    pub fn start(path: &Path) -> io::Result<Self> {
        // An endpoint left by a crashed game refuses connections; replace it.
        if path.exists() && connect(path).is_err() {
            std::fs::remove_file(path)?;
        }
        let (sender, requests) = mpsc::channel();
        let local = sender.clone();
        let clients = Arc::new(AtomicUsize::new(0));
        let counted = clients.clone();
        let closed = Arc::new(AtomicBool::new(false));
        let mut tcp_port = None;
        if uses_tcp(path) {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
            let token = random_token();
            let port = listener.local_addr()?.port();
            tcp_port = Some(port);
            write_private(
                path,
                &serde_json::json!({ "transport": "tcp", "port": port, "token": token })
                    .to_string(),
            )?;
            spawn_listener(
                move || listener.accept().map(|(s, _)| s),
                sender,
                counted,
                closed.clone(),
                move |stream: TcpStream| {
                    let _ = stream.set_nodelay(true);
                    let mut stream = Stream::Tcp(stream);
                    authenticate(&mut stream, &token).then_some(stream)
                },
            )?;
        } else {
            #[cfg(unix)]
            {
                let listener = UnixListener::bind(path)?;
                spawn_listener(
                    move || listener.accept().map(|(s, _)| s),
                    sender,
                    counted,
                    closed.clone(),
                    |s| Some(Stream::Unix(s)),
                )?;
            }
        }
        Ok(Self {
            path: path.to_owned(),
            requests,
            sender: local,
            clients,
            closed,
            port: tcp_port,
        })
    }
    /// Commands from inside the process only (`--benchmark` without `--control`).
    pub fn local() -> Self {
        let (sender, requests) = mpsc::channel();
        Self {
            path: PathBuf::new(),
            requests,
            sender,
            clients: Arc::default(),
            closed: Arc::default(),
            port: None,
        }
    }
    /// How many clients are connected.
    pub fn clients(&self) -> usize {
        self.clients.load(Ordering::Relaxed)
    }
    /// Sends commands as a socket client would, from another thread.
    pub fn sender(&self) -> mpsc::Sender<Request> {
        self.sender.clone()
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn try_recv(&self) -> Option<Request> {
        self.requests.try_recv().ok()
    }
}

/// Accept connections on a thread, one more thread per client.
fn spawn_listener<S: Send + 'static>(
    mut accept_next: impl FnMut() -> io::Result<S> + Send + 'static,
    sender: mpsc::Sender<Request>,
    clients: Arc<AtomicUsize>,
    closed: Arc<AtomicBool>,
    accept: impl Fn(S) -> Option<Stream> + Send + Sync + 'static,
) -> io::Result<()> {
    let accept = std::sync::Arc::new(accept);
    thread::Builder::new()
        .name("horizon-control".into())
        .spawn(move || {
            loop {
                let next = accept_next();
                if closed.load(Ordering::Relaxed) {
                    return;
                }
                let Ok(stream) = next else {
                    thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                };
                let sender = sender.clone();
                let accept = accept.clone();
                let clients = clients.clone();
                let _ = thread::Builder::new()
                    .name("horizon-control-client".into())
                    .spawn(move || {
                        if let Some(stream) = accept(stream) {
                            clients.fetch_add(1, Ordering::Relaxed);
                            serve(stream, &sender);
                            clients.fetch_sub(1, Ordering::Relaxed);
                        }
                    });
            }
        })?;
    Ok(())
}

/// The first line of a TCP client must be `auth TOKEN`.
fn authenticate(stream: &mut Stream, token: &str) -> bool {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    // Read the line byte by byte so nothing after it is buffered away.
    while line.len() < 256 {
        match stream.read(&mut byte) {
            Ok(1) if byte[0] == b'\n' => break,
            Ok(1) => line.push(byte[0]),
            _ => return false,
        }
    }
    let line = String::from_utf8_lossy(&line);
    let ok = line
        .trim()
        .strip_prefix("auth ")
        .is_some_and(|t| same_token(t.trim(), token));
    let reply = if ok {
        r#"{"ok":true}"#
    } else {
        r#"{"error":"bad or missing token: the first line must be auth TOKEN"}"#
    };
    let _ = writeln!(stream, "{reply}");
    ok
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        // Wake the listener so it sees `closed` and exits (agent control turned off in-game).
        self.closed.store(true, Ordering::Relaxed);
        match self.port {
            Some(port) => drop(TcpStream::connect((Ipv4Addr::LOCALHOST, port))),
            #[cfg(unix)]
            None => drop(UnixStream::connect(&self.path)),
            #[cfg(not(unix))]
            None => {}
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn serve(stream: Stream, sender: &mpsc::Sender<Request>) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let (reply, response) = mpsc::channel();
        if sender.send(Request { line, reply }).is_err() {
            return;
        }
        // The game dropping the reply sender means it exited mid-command.
        let text = response
            .recv()
            .unwrap_or_else(|_| r#"{"error":"game exited"}"#.to_owned());
        if writeln!(writer, "{text}").is_err() {
            return;
        }
    }
}

#[cfg(feature = "paired-packages")]
pub use retained::{
    ClosedControl, RetainedControl, RetainedControlObservation, RetainedControlPhase,
    RetainedControlStatus, RetainedEndpointState, RetainedFailure, RetainedFailureKind,
    RetainedSender, RetainedWorkerStage, WindowCommand, WindowFrameObservation, WindowReply,
    WindowReplyRefusal, WindowRequest,
};

#[cfg(all(
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
pub use retained::WindowCleanupHold;

/// Dormant, original-constructor ownership. This does not adopt a legacy `Server`.
#[cfg(feature = "paired-packages")]
mod retained {
    use super::*;
    use std::{
        collections::VecDeque,
        fs::{File, Metadata, OpenOptions},
        net::Shutdown,
        sync::{Mutex, MutexGuard, Weak},
        time::{Duration, Instant},
    };

    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    pub use window::WindowCleanupHold;
    pub use window::{
        WindowCommand, WindowFrameObservation, WindowReply, WindowReplyRefusal, WindowRequest,
    };

    fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
        value
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum RetainedControlPhase {
        Prepared,
        Running,
        Closing,
        Blocked,
        Quiescent,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum RetainedEndpointState {
        Unpublished,
        Published,
        Released,
        Refused,
        Replaced,
        Ambiguous,
        Failed,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum RetainedWorkerStage {
        Acquisition,
        Listener,
        Authentication,
        CommandRead,
        ResponseWait,
        ResponseWrite,
        Cleanup,
        Stop,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum RetainedFailureKind {
        SpawnRefused,
        Panicked,
        NativeIo(io::ErrorKind),
        EndpointExists,
        EndpointReplaced,
        EndpointAmbiguous,
        Deadline,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct RetainedFailure {
        pub stage: RetainedWorkerStage,
        pub kind: RetainedFailureKind,
        pub detail: String,
    }

    /// Counts describe custody, not delivery, Game cancellation, or semantic completion.
    #[derive(Clone, Debug)]
    pub struct RetainedControlObservation {
        pub phase: RetainedControlPhase,
        pub endpoint: RetainedEndpointState,
        pub admission_sealed: bool,
        pub native_quiescent: bool,
        pub endpoint_released: bool,
        pub outstanding_handles: usize,
        pub pending_registrations: usize,
        pub joins_in_progress: usize,
        pub registered_clients: usize,
        pub retired_client_sockets: usize,
        pub refused_clients: usize,
        pub queued_requests: usize,
        pub handed_off_requests: usize,
        pub retained_reply_receivers: usize,
        pub unobserved_replies: usize,
        pub observed_replies: usize,
        pub delivered_replies: usize,
        pub close_attempts: usize,
        pub deadline_expired: bool,
        pub failures: Vec<RetainedFailure>,
    }

    #[derive(Clone, Debug)]
    pub struct RetainedControlStatus {
        pub observation: RetainedControlObservation,
    }

    /// Preparation owns the input and channels but performs no native IO or spawning.
    /// `start` is explicit and does not change ordinary `Server` construction.
    pub struct RetainedControl {
        group: Arc<Group>,
        resolved: bool,
    }

    /// Clones share the permanent admission fence. Refused input is returned intact.
    #[derive(Clone)]
    pub struct RetainedSender {
        group: Arc<Group>,
    }

    /// Keeps original inputs, queued requests, response receivers and native outcomes.
    /// This is not a Game completion or a global close-commit witness.
    pub struct ClosedControl {
        group: Arc<Group>,
    }

    impl ClosedControl {
        pub fn observation(&self) -> RetainedControlObservation {
            observe(&self.group)
        }
    }

    struct Group {
        sealed: AtomicBool,
        cleanup_requested: AtomicBool,
        path: PathBuf,
        window: Option<Arc<window::WindowGroup>>,
        registry: Mutex<Registry>,
        acquisition: Arc<Mutex<Acquisition>>,
        #[cfg(test)]
        test: TestControl,
    }

    struct Registry {
        phase: RetainedControlPhase,
        started: bool,
        local: bool,
        listener: Option<thread::JoinHandle<()>>,
        listener_state: WorkerState,
        cleanup: Option<thread::JoinHandle<()>>,
        cleanup_state: WorkerState,
        cleanup_done: bool,
        endpoint: RetainedEndpointState,
        clients: Vec<ClientRecord>,
        queue: VecDeque<Request>,
        replies: Vec<Arc<PendingReply>>,
        handed: usize,
        delivered: usize,
        observed: usize,
        refused: usize,
        failures: Vec<RetainedFailure>,
        deadline: Option<Instant>,
        deadline_reported: bool,
        attempts: usize,
    }

    struct ClientRecord {
        input: Arc<ClientInput>,
        stop: Option<Arc<NativeSocket>>,
        handle: Option<thread::JoinHandle<()>>,
        state: WorkerState,
        refused: bool,
    }

    #[derive(Clone, Copy, Eq, PartialEq)]
    enum WorkerState {
        Unstarted,
        SpawnPending,
        Running,
        Joining,
        Joined,
        Refused,
    }

    impl WorkerState {
        fn terminal(self) -> bool {
            matches!(self, Self::Unstarted | Self::Joined | Self::Refused)
        }
    }

    /// Actors never move these payloads onto their unwind stacks. The UI never
    /// locks acquisition/client/reply state; its registry lock is separate.
    struct Acquisition {
        path: Option<PathBuf>,
        listener: Option<NativeListener>,
        endpoint_file: Option<File>,
        identity: Option<EndpointIdentity>,
        owns_path: bool,
        publication: Vec<u8>,
        publication_written: usize,
        token: String,
        accepted: Option<Arc<ClientInput>>,
        window_reservation: Option<window::SlotId>,
    }

    enum NativeListener {
        #[cfg(unix)]
        Unix(UnixListener),
        Tcp(TcpListener),
    }

    impl NativeListener {
        fn nonblocking(&self) -> io::Result<()> {
            match self {
                #[cfg(unix)]
                Self::Unix(listener) => listener.set_nonblocking(true),
                Self::Tcp(listener) => listener.set_nonblocking(true),
            }
        }

        fn accept(&self) -> io::Result<Stream> {
            match self {
                #[cfg(unix)]
                Self::Unix(listener) => listener.accept().map(|(s, _)| Stream::Unix(s)),
                Self::Tcp(listener) => listener.accept().map(|(s, _)| Stream::Tcp(s)),
            }
        }
    }

    struct NativeSocket(Stream);

    impl NativeSocket {
        fn read(&self, bytes: &mut [u8]) -> io::Result<usize> {
            match &self.0 {
                #[cfg(unix)]
                Stream::Unix(stream) => {
                    let mut borrowed = stream;
                    borrowed.read(bytes)
                }
                Stream::Tcp(stream) => {
                    let mut borrowed = stream;
                    borrowed.read(bytes)
                }
            }
        }

        fn write(&self, bytes: &[u8]) -> io::Result<usize> {
            match &self.0 {
                #[cfg(unix)]
                Stream::Unix(stream) => {
                    let mut borrowed = stream;
                    borrowed.write(bytes)
                }
                Stream::Tcp(stream) => {
                    let mut borrowed = stream;
                    borrowed.write(bytes)
                }
            }
        }

        fn shutdown(&self) {
            let _ = match &self.0 {
                #[cfg(unix)]
                Stream::Unix(stream) => stream.shutdown(Shutdown::Both),
                Stream::Tcp(stream) => stream.shutdown(Shutdown::Both),
            };
        }
    }

    struct ClientInput {
        window: Option<Arc<window::WindowClient>>,
        token: Option<String>,
        io: Mutex<ClientIo>,
        stage: AtomicUsize,
        bytes_written: AtomicUsize,
        auth_read: AtomicUsize,
        command_read: AtomicUsize,
        fd_retired: AtomicBool,
    }

    #[derive(Default)]
    struct ClientIo {
        socket: Option<NativeSocket>,
        byte: [u8; 1],
        auth: Vec<u8>,
        command: Vec<u8>,
        outgoing: Vec<u8>,
        written: usize,
        pending: Option<Arc<PendingReply>>,
    }

    struct PendingReply {
        window: Option<Arc<window::WindowPending>>,
        state: Mutex<ReplyState>,
        observed: AtomicBool,
        delivered: AtomicBool,
    }

    struct ReplyState {
        receiver: mpsc::Receiver<String>,
        text: Option<String>,
        disconnected: bool,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct EndpointIdentity {
        #[cfg(unix)]
        device: u64,
        #[cfg(unix)]
        inode: u64,
        #[cfg(not(unix))]
        created: Option<std::time::SystemTime>,
        #[cfg(not(unix))]
        modified: Option<std::time::SystemTime>,
        #[cfg(not(unix))]
        length: u64,
    }

    impl EndpointIdentity {
        fn from_metadata(metadata: &Metadata) -> Self {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                Self {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                }
            }
            #[cfg(not(unix))]
            {
                Self {
                    created: metadata.created().ok(),
                    modified: metadata.modified().ok(),
                    length: metadata.len(),
                }
            }
        }

        fn usable(&self) -> bool {
            #[cfg(unix)]
            {
                true
            }
            #[cfg(not(unix))]
            {
                self.created.is_some() && self.modified.is_some()
            }
        }
    }

    impl RetainedControl {
        pub fn prepare(path: &Path) -> Self {
            Self::prepare_input(Some(path.to_owned()))
        }

        /// Owns no native worker and makes no claim about an external benchmark.
        pub fn prepare_local() -> Self {
            Self::prepare_input(None)
        }

        /// The fixed window profile is selected before any native acquisition.
        /// It admits no local/raw requests and requires explicit frame grants.
        pub fn prepare_window(path: &Path) -> Self {
            let mut owner = Self::prepare(path);
            Arc::get_mut(&mut owner.group).unwrap().window =
                Some(Arc::new(window::WindowGroup::new()));
            owner
        }

        pub fn path(&self) -> &Path {
            &self.group.path
        }

        /// Native connections, including incomplete authentication; never a close proof.
        pub fn connected_clients(&self) -> usize {
            lock(&self.group.registry)
                .clients
                .iter()
                .filter(|client| !client.input.fd_retired.load(Ordering::Acquire))
                .count()
        }

        pub fn grant_window_frame(&mut self, generation: u64) -> WindowFrameObservation {
            self.poll_close();
            match &self.group.window {
                Some(window) if !self.group.sealed.load(Ordering::Acquire) => {
                    window.grant(generation)
                }
                Some(window) => window.observe(false),
                None => WindowFrameObservation::unsupported(),
            }
        }

        pub fn window_observation(&self) -> WindowFrameObservation {
            self.group
                .window
                .as_ref()
                .map_or_else(WindowFrameObservation::unsupported, |window| {
                    window.observe(false)
                })
        }

        /// Fixed qualification control of this original cleanup actor only.
        #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
        pub fn arm_window_cleanup_hold(&mut self) -> Result<WindowCleanupHold, String> {
            if lock(&self.group.registry).started || self.group.sealed.load(Ordering::Acquire) {
                return Err("window cleanup hold requires original prepared owner".into());
            }
            self.group
                .window
                .as_ref()
                .ok_or("window cleanup hold requires fixed window construction")?
                .arm_cleanup_hold()
        }

        pub fn try_recv_window(&mut self) -> Option<WindowRequest> {
            let window = self.group.window.as_ref()?;
            if self.group.sealed.load(Ordering::Acquire) {
                return None;
            }
            let request = window.take_request()?;
            lock(&self.group.registry).handed += 1;
            Some(request)
        }

        fn prepare_input(path: Option<PathBuf>) -> Self {
            let local = path.is_none();
            Self {
                group: Arc::new(Group {
                    sealed: AtomicBool::new(false),
                    cleanup_requested: AtomicBool::new(false),
                    path: path.clone().unwrap_or_default(),
                    window: None,
                    registry: Mutex::new(Registry {
                        phase: RetainedControlPhase::Prepared,
                        started: false,
                        local,
                        listener: None,
                        listener_state: WorkerState::Unstarted,
                        cleanup: None,
                        cleanup_state: WorkerState::Unstarted,
                        cleanup_done: local,
                        endpoint: RetainedEndpointState::Unpublished,
                        clients: Vec::new(),
                        queue: VecDeque::new(),
                        replies: Vec::new(),
                        handed: 0,
                        delivered: 0,
                        observed: 0,
                        refused: 0,
                        failures: Vec::new(),
                        deadline: None,
                        deadline_reported: false,
                        attempts: 0,
                    }),
                    acquisition: Arc::new(Mutex::new(Acquisition {
                        path,
                        listener: None,
                        endpoint_file: None,
                        identity: None,
                        owns_path: false,
                        publication: Vec::new(),
                        publication_written: 0,
                        token: String::new(),
                        accepted: None,
                        window_reservation: None,
                    })),
                    #[cfg(test)]
                    test: TestControl::default(),
                }),
                resolved: false,
            }
        }

        pub fn start(&mut self) -> Result<(), String> {
            {
                let mut registry = lock(&self.group.registry);
                if registry.started || self.group.sealed.load(Ordering::Acquire) {
                    return Err("retained control already started or sealed".into());
                }
                registry.started = true;
                registry.phase = RetainedControlPhase::Running;
                if registry.local {
                    return Ok(());
                }
                registry.listener_state = WorkerState::SpawnPending;
                registry.cleanup_state = WorkerState::SpawnPending;
            }
            // Worker creation is confined to this explicit startup boundary.
            // The cleanup actor waits for positively joined actor receipts.
            if let Err(error) = spawn_fixed(&self.group, Actor::Cleanup) {
                let mut registry = lock(&self.group.registry);
                registry.listener_state = WorkerState::Unstarted;
                registry.cleanup_done = true;
                return Err(error);
            }
            spawn_fixed(&self.group, Actor::Listener)
        }

        pub fn sender(&self) -> RetainedSender {
            RetainedSender {
                group: self.group.clone(),
            }
        }

        pub fn try_recv(&mut self) -> Option<Request> {
            let mut registry = lock(&self.group.registry);
            if self.group.window.is_some() || self.group.sealed.load(Ordering::Acquire) {
                return None;
            }
            let request = registry.queue.pop_front()?;
            registry.handed += 1;
            Some(request)
        }

        pub fn observation(&self) -> RetainedControlObservation {
            observe(&self.group)
        }

        /// Repeated calls preserve the first deadline and never reopen admission.
        pub fn begin_close(&mut self, deadline: Instant) {
            {
                let mut registry = lock(&self.group.registry);
                self.group.sealed.store(true, Ordering::Release);
                if registry.deadline.is_none() {
                    registry.deadline = Some(deadline);
                    registry.attempts = 1;
                    registry.phase = RetainedControlPhase::Closing;
                }
            }
            request_socket_stop(&self.group);
        }

        pub fn retry_close(&mut self, deadline: Instant) -> Result<(), String> {
            {
                let mut registry = lock(&self.group.registry);
                if registry.deadline.is_none() || registry.phase == RetainedControlPhase::Quiescent
                {
                    return Err("retained control has no incomplete close to retry".into());
                }
                registry.deadline = Some(deadline);
                registry.deadline_reported = false;
                registry.attempts += 1;
                registry.phase = RetainedControlPhase::Closing;
            }
            request_socket_stop(&self.group);
            Ok(())
        }

        /// Only positively finished handles are removed, then joined outside locks.
        pub fn poll_close(&mut self) -> RetainedControlStatus {
            let finished = {
                let mut registry = lock(&self.group.registry);
                let mut finished = Vec::new();
                if registry
                    .listener
                    .as_ref()
                    .is_some_and(thread::JoinHandle::is_finished)
                {
                    registry.listener_state = WorkerState::Joining;
                    finished.push((Actor::Listener, registry.listener.take().unwrap()));
                }
                for (index, client) in registry.clients.iter_mut().enumerate() {
                    if client
                        .handle
                        .as_ref()
                        .is_some_and(thread::JoinHandle::is_finished)
                    {
                        client.state = WorkerState::Joining;
                        finished.push((Actor::Client(index), client.handle.take().unwrap()));
                    }
                }
                if registry
                    .cleanup
                    .as_ref()
                    .is_some_and(thread::JoinHandle::is_finished)
                {
                    registry.cleanup_state = WorkerState::Joining;
                    finished.push((Actor::Cleanup, registry.cleanup.take().unwrap()));
                }
                finished
            };
            for (actor, handle) in finished {
                let outcome = handle.join();
                let mut registry = lock(&self.group.registry);
                if let Actor::Client(index) = actor {
                    registry.clients[index].state = WorkerState::Joined;
                }
                if matches!(actor, Actor::Listener) {
                    registry.listener_state = WorkerState::Joined;
                    // Only this actor can finish a pending native client spawn.
                    // Its checked join proves these registrations cannot acquire
                    // a handle later, including when their parent unwound.
                    let mut refused = 0;
                    for client in &mut registry.clients {
                        if client.state == WorkerState::SpawnPending {
                            client.state = WorkerState::Refused;
                            client.refused = true;
                            refused += 1;
                        }
                    }
                    registry.refused += refused;
                }
                if matches!(actor, Actor::Cleanup) {
                    registry.cleanup_state = WorkerState::Joined;
                    registry.cleanup_done = true;
                }
                if outcome.is_err() {
                    if let Actor::Client(index) = actor
                        && let Some(window) = &registry.clients[index].input.window
                    {
                        window.failed.store(true, Ordering::Release);
                    }
                    registry.failures.push(RetainedFailure {
                        stage: actor.stage(),
                        kind: RetainedFailureKind::Panicked,
                        detail: "original retained native worker panicked".into(),
                    });
                    if matches!(actor, Actor::Cleanup) {
                        registry.endpoint = RetainedEndpointState::Failed;
                    }
                }
            }
            {
                let mut registry = lock(&self.group.registry);
                if registry.deadline.is_some() {
                    let actors_joined = registry.listener_state.terminal()
                        && registry
                            .clients
                            .iter()
                            .all(|client| client.state.terminal());
                    if actors_joined {
                        self.group.cleanup_requested.store(true, Ordering::Release);
                    }
                    // A prepared-but-unstarted native owner has no native work.
                    if !registry.started {
                        registry.cleanup_done = true;
                    }
                    let joined = actors_joined && registry.cleanup_state.terminal();
                    if joined && registry.cleanup_done {
                        registry.phase = RetainedControlPhase::Quiescent;
                    } else if registry
                        .deadline
                        .is_some_and(|deadline| Instant::now() >= deadline)
                    {
                        registry.phase = RetainedControlPhase::Blocked;
                        if !registry.deadline_reported {
                            registry.deadline_reported = true;
                            registry.failures.push(RetainedFailure {
                                stage: RetainedWorkerStage::Stop,
                                kind: RetainedFailureKind::Deadline,
                                detail: "original native work has not completed before deadline"
                                    .into(),
                            });
                        }
                    }
                }
            }
            RetainedControlStatus {
                observation: observe(&self.group),
            }
        }

        /// A failure handoff remains visibly different from successful endpoint release.
        pub fn try_closed(mut self) -> Result<ClosedControl, Self> {
            if lock(&self.group.registry).phase != RetainedControlPhase::Quiescent {
                return Err(self);
            }
            self.resolved = true;
            Ok(ClosedControl {
                group: self.group.clone(),
            })
        }
    }

    impl Drop for RetainedControl {
        fn drop(&mut self) {
            if !self.resolved {
                eprintln!("fatal: unresolved retained native control owner");
                std::process::abort();
            }
        }
    }

    impl RetainedSender {
        pub fn try_send(&self, request: Request) -> Result<(), Request> {
            let mut registry = lock(&self.group.registry);
            if self.group.window.is_some() || self.group.sealed.load(Ordering::Acquire) {
                return Err(request);
            }
            registry.queue.push_back(request);
            Ok(())
        }
    }

    fn observe(group: &Group) -> RetainedControlObservation {
        let registry = lock(&group.registry);
        let handles = usize::from(registry.listener.is_some())
            + usize::from(registry.cleanup.is_some())
            + registry
                .clients
                .iter()
                .filter(|client| client.handle.is_some())
                .count();
        RetainedControlObservation {
            phase: registry.phase,
            endpoint: registry.endpoint,
            admission_sealed: group.sealed.load(Ordering::Acquire),
            native_quiescent: registry.deadline.is_some()
                && handles == 0
                && registry.listener_state.terminal()
                && registry.cleanup_state.terminal()
                && registry
                    .clients
                    .iter()
                    .all(|client| client.state.terminal()),
            endpoint_released: match registry.endpoint {
                RetainedEndpointState::Released => {
                    registry.cleanup_done && registry.cleanup_state == WorkerState::Joined
                }
                RetainedEndpointState::Unpublished => {
                    registry.local
                        || !registry.started
                        || matches!(
                            registry.listener_state,
                            WorkerState::Unstarted | WorkerState::Refused
                        )
                        || (registry.listener_state.terminal()
                            && registry.cleanup_done
                            && registry.cleanup_state.terminal())
                }
                _ => false,
            },
            outstanding_handles: handles,
            pending_registrations: usize::from(
                registry.listener_state == WorkerState::SpawnPending,
            ) + usize::from(
                registry.cleanup_state == WorkerState::SpawnPending,
            ) + registry
                .clients
                .iter()
                .filter(|client| client.state == WorkerState::SpawnPending)
                .count(),
            joins_in_progress: usize::from(registry.listener_state == WorkerState::Joining)
                + usize::from(registry.cleanup_state == WorkerState::Joining)
                + registry
                    .clients
                    .iter()
                    .filter(|client| client.state == WorkerState::Joining)
                    .count(),
            registered_clients: registry.clients.len(),
            retired_client_sockets: registry
                .clients
                .iter()
                .filter(|client| client.input.fd_retired.load(Ordering::Acquire))
                .count(),
            refused_clients: registry.refused,
            queued_requests: registry.queue.len(),
            handed_off_requests: registry.handed,
            retained_reply_receivers: registry.replies.len(),
            unobserved_replies: registry
                .replies
                .iter()
                .filter(|reply| !reply.observed.load(Ordering::Acquire))
                .count(),
            observed_replies: registry.observed,
            delivered_replies: registry.delivered,
            close_attempts: registry.attempts,
            deadline_expired: registry
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline),
            failures: registry.failures.clone(),
        }
    }

    fn request_socket_stop(group: &Group) {
        let sockets: Vec<_> = lock(&group.registry)
            .clients
            .iter()
            .filter_map(|client| client.stop.clone())
            .collect();
        for socket in sockets {
            socket.shutdown();
        }
    }

    #[derive(Clone, Copy)]
    enum Actor {
        Listener,
        Client(usize),
        Cleanup,
    }

    impl Actor {
        fn stage(self) -> RetainedWorkerStage {
            match self {
                Self::Listener => RetainedWorkerStage::Listener,
                Self::Client(_) => RetainedWorkerStage::CommandRead,
                Self::Cleanup => RetainedWorkerStage::Cleanup,
            }
        }
    }

    /// Only a latch and weak identity enter the spawn closure. Original native
    /// inputs remain registered even when the OS refuses to spawn that closure.
    fn spawn_fixed(group: &Arc<Group>, actor: Actor) -> Result<(), String> {
        let (release, wait) = mpsc::channel();
        let weak = Arc::downgrade(group);
        #[cfg(test)]
        let refused = match actor {
            Actor::Listener => group.test.refuse_listener.load(Ordering::Acquire),
            Actor::Client(_) => group.test.refuse_client.load(Ordering::Acquire),
            Actor::Cleanup => group.test.refuse_cleanup.load(Ordering::Acquire),
        };
        #[cfg(not(test))]
        let refused = false;
        let spawned = if refused {
            Err(io::Error::other("fixed test spawn refusal"))
        } else {
            thread::Builder::new()
                .name(
                    match actor {
                        Actor::Listener => "horizon-retained-control",
                        Actor::Client(_) => "horizon-retained-control-client",
                        Actor::Cleanup => "horizon-retained-control-cleanup",
                    }
                    .into(),
                )
                .spawn(move || {
                    if wait.recv().is_err() {
                        return;
                    }
                    match actor {
                        Actor::Listener => listener_worker(weak),
                        Actor::Client(index) => client_worker(weak, index),
                        Actor::Cleanup => cleanup_worker(weak),
                    }
                })
        };
        match spawned {
            Ok(handle) => {
                {
                    let mut registry = lock(&group.registry);
                    match actor {
                        Actor::Listener => {
                            registry.listener = Some(handle);
                            registry.listener_state = WorkerState::Running;
                        }
                        Actor::Client(index) => {
                            registry.clients[index].handle = Some(handle);
                            registry.clients[index].state = WorkerState::Running;
                        }
                        Actor::Cleanup => {
                            registry.cleanup = Some(handle);
                            registry.cleanup_state = WorkerState::Running;
                        }
                    }
                }
                let _ = release.send(());
                Ok(())
            }
            Err(error) => {
                let detail = error.to_string();
                let mut registry = lock(&group.registry);
                registry.failures.push(RetainedFailure {
                    stage: actor.stage(),
                    kind: RetainedFailureKind::SpawnRefused,
                    detail: detail.clone(),
                });
                match actor {
                    Actor::Client(index) => {
                        registry.clients[index].state = WorkerState::Refused;
                        registry.clients[index].refused = true;
                        registry.refused += 1;
                    }
                    Actor::Listener => registry.listener_state = WorkerState::Refused,
                    Actor::Cleanup => registry.cleanup_state = WorkerState::Refused,
                }
                Err(detail)
            }
        }
    }

    fn record_failure(group: &Weak<Group>, stage: RetainedWorkerStage, error: &io::Error) {
        if let Some(group) = group.upgrade() {
            lock(&group.registry).failures.push(RetainedFailure {
                stage,
                kind: RetainedFailureKind::NativeIo(error.kind()),
                detail: error.to_string(),
            });
        }
    }

    fn sealed(group: &Weak<Group>) -> bool {
        group
            .upgrade()
            .is_none_or(|group| group.sealed.load(Ordering::Acquire))
    }

    fn listener_worker(group: Weak<Group>) {
        if group.upgrade().is_some_and(|owner| owner.window.is_some()) {
            window::listener_worker(group);
            return;
        }
        let Some(owner) = group.upgrade() else { return };
        let acquisition = owner.acquisition.clone();
        drop(owner);
        #[cfg(test)]
        test_gate(&group, TestPoint::Acquisition);
        if sealed(&group) {
            return;
        }
        let initialized = {
            let mut input = lock(&acquisition);
            acquire_endpoint(&mut input)
        };
        if let Err(error) = initialized {
            if let Some(owner) = group.upgrade() {
                let mut registry = lock(&owner.registry);
                let exists = error.kind() == io::ErrorKind::AlreadyExists;
                registry.endpoint = if exists {
                    RetainedEndpointState::Refused
                } else {
                    RetainedEndpointState::Failed
                };
                registry.failures.push(RetainedFailure {
                    stage: RetainedWorkerStage::Acquisition,
                    kind: if exists {
                        RetainedFailureKind::EndpointExists
                    } else {
                        RetainedFailureKind::NativeIo(error.kind())
                    },
                    detail: error.to_string(),
                });
            }
            return;
        }
        #[cfg(test)]
        test_gate(&group, TestPoint::Acquired);
        if let Some(owner) = group.upgrade() {
            lock(&owner.registry).endpoint = RetainedEndpointState::Published;
        }
        #[cfg(test)]
        test_gate(&group, TestPoint::Published);
        while !sealed(&group) {
            let accepted = {
                let mut input = lock(&acquisition);
                let result = input
                    .listener
                    .as_ref()
                    .expect("registered listener")
                    .accept();
                match result {
                    Ok(stream) => {
                        let token = if matches!(stream, Stream::Tcp(_)) {
                            Some(input.token.clone())
                        } else {
                            None
                        };
                        input.accepted = Some(Arc::new(ClientInput {
                            window: None,
                            token,
                            io: Mutex::new(ClientIo {
                                socket: Some(NativeSocket(stream)),
                                ..ClientIo::default()
                            }),
                            stage: AtomicUsize::new(0),
                            bytes_written: AtomicUsize::new(0),
                            auth_read: AtomicUsize::new(0),
                            command_read: AtomicUsize::new(0),
                            fd_retired: AtomicBool::new(false),
                        }));
                        Ok(input.accepted.as_ref().unwrap().clone())
                    }
                    Err(error) => Err(error),
                }
            };
            let client = match accepted {
                Ok(client) => client,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(error) => {
                    if !sealed(&group) {
                        record_failure(&group, RetainedWorkerStage::Listener, &error);
                    }
                    return;
                }
            };
            #[cfg(test)]
            test_gate(&group, TestPoint::Accepted);
            let Some(owner) = group.upgrade() else { return };
            let (index, refused) = {
                let mut registry = lock(&owner.registry);
                let refused = owner.sealed.load(Ordering::Acquire);
                let index = registry.clients.len();
                registry.clients.push(ClientRecord {
                    input: client.clone(),
                    stop: None,
                    handle: None,
                    state: if refused {
                        WorkerState::Refused
                    } else {
                        WorkerState::SpawnPending
                    },
                    refused,
                });
                if refused {
                    registry.refused += 1;
                }
                (index, refused)
            };
            lock(&acquisition).accepted = None;
            if refused {
                retire_client_socket(&group, index, &client);
            } else {
                let cap = lock(&client.io).socket.as_ref().unwrap().0.try_clone();
                match cap {
                    Ok(cap) => {
                        let stop = Arc::new(NativeSocket(cap));
                        let fenced = {
                            let mut registry = lock(&owner.registry);
                            registry.clients[index].stop = Some(stop.clone());
                            owner.sealed.load(Ordering::Acquire)
                        };
                        if fenced {
                            stop.shutdown();
                        }
                        if spawn_fixed(&owner, Actor::Client(index)).is_err() {
                            retire_client_socket(&group, index, &client);
                        }
                    }
                    Err(error) => {
                        record_failure(&group, RetainedWorkerStage::Listener, &error);
                        let mut registry = lock(&owner.registry);
                        registry.clients[index].state = WorkerState::Refused;
                        registry.clients[index].refused = true;
                        registry.refused += 1;
                        drop(registry);
                        retire_client_socket(&group, index, &client);
                    }
                }
            }
            #[cfg(test)]
            if owner
                .test
                .panic_listener_after_client
                .load(Ordering::Acquire)
            {
                panic!("fixed retained listener panic after client registration");
            }
        }
    }

    fn acquire_endpoint(input: &mut Acquisition) -> io::Result<()> {
        let path = input.path.as_ref().expect("native constructor path");
        match std::fs::symlink_metadata(path) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "retained endpoint already exists",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if uses_tcp(path) {
            input.listener = Some(NativeListener::Tcp(TcpListener::bind((
                Ipv4Addr::LOCALHOST,
                0,
            ))?));
            let port = match input.listener.as_ref().unwrap() {
                NativeListener::Tcp(listener) => listener.local_addr()?.port(),
                #[cfg(unix)]
                NativeListener::Unix(_) => unreachable!(),
            };
            input.token = random_token();
            input.publication =
                serde_json::json!({"transport":"tcp", "port":port, "token":input.token})
                    .to_string()
                    .into_bytes();
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            input.endpoint_file = Some(options.open(path)?);
            input.owns_path = true;
            input.identity = Some(EndpointIdentity::from_metadata(
                &input.endpoint_file.as_ref().unwrap().metadata()?,
            ));
            while input.publication_written < input.publication.len() {
                let count = input
                    .endpoint_file
                    .as_mut()
                    .unwrap()
                    .write(&input.publication[input.publication_written..])?;
                if count == 0 {
                    return Err(io::ErrorKind::WriteZero.into());
                }
                input.publication_written += count;
            }
            // On platforms without inode identity, retain the post-publication identity.
            input.identity = Some(EndpointIdentity::from_metadata(
                &input.endpoint_file.as_ref().unwrap().metadata()?,
            ));
        } else {
            #[cfg(unix)]
            {
                input.listener = Some(NativeListener::Unix(UnixListener::bind(path)?));
                input.owns_path = true;
                input.identity = Some(EndpointIdentity::from_metadata(&std::fs::symlink_metadata(
                    path,
                )?));
            }
        }
        input.listener.as_ref().unwrap().nonblocking()
    }

    fn client_worker(group: Weak<Group>, index: usize) {
        let Some(owner) = group.upgrade() else { return };
        let input = lock(&owner.registry).clients[index].input.clone();
        drop(owner);
        if input.window.is_some() {
            window::client_worker(&group, &input);
        } else {
            client_worker_inner(&group, &input);
        }
        // This is the last native IO actor for this connection. Normal EOF,
        // authentication refusal and stop retire both descriptors here, even
        // while the parent/sender clones remain alive. Panic bypasses this;
        // the original state then belongs to checked cleanup after its join.
        retire_client_socket(&group, index, &input);
    }

    fn retire_client_socket(group: &Weak<Group>, index: usize, input: &ClientInput) {
        let stop = group
            .upgrade()
            .and_then(|owner| lock(&owner.registry).clients[index].stop.take());
        if let Some(stop) = &stop {
            stop.shutdown();
        }
        drop(stop);
        let original = lock(&input.io).socket.take();
        if let Some(original) = &original {
            original.shutdown();
        }
        drop(original);
        input.fd_retired.store(true, Ordering::Release);
    }

    fn read_byte(state: &mut ClientIo) -> io::Result<usize> {
        let ClientIo { socket, byte, .. } = state;
        socket.as_ref().unwrap().read(byte)
    }

    fn client_worker_inner(group: &Weak<Group>, input: &ClientInput) {
        #[cfg(test)]
        test_gate(group, TestPoint::Client);
        if sealed(group) {
            return;
        }
        if let Some(token) = &input.token {
            input.stage.store(1, Ordering::Release);
            if !read_authentication(group, input, token) {
                return;
            }
        }
        loop {
            if sealed(group) {
                return;
            }
            input.stage.store(2, Ordering::Release);
            {
                let mut state = lock(&input.io);
                let newline_terminated = loop {
                    match read_byte(&mut state) {
                        Ok(0) if state.command.is_empty() => return,
                        Ok(0) => break false,
                        Ok(_) => {
                            input.command_read.fetch_add(1, Ordering::Release);
                            let byte = state.byte[0];
                            if byte == b'\n' {
                                break true;
                            }
                            state.command.push(byte);
                        }
                        Err(error) => {
                            if !sealed(group) {
                                record_failure(group, RetainedWorkerStage::CommandRead, &error);
                            }
                            return;
                        }
                    }
                };
                // BufRead::lines dispatches a nonempty EOF tail unchanged. A
                // shutdown-induced EOF must instead retain its original bytes.
                if !newline_terminated && sealed(group) {
                    return;
                }
                if newline_terminated && state.command.last() == Some(&b'\r') {
                    state.command.pop();
                }
                let Ok(line) = String::from_utf8(state.command.clone()) else {
                    return;
                };
                if line.trim().is_empty() {
                    state.command = Vec::new();
                    continue;
                }
                let (reply, receiver) = mpsc::channel();
                let pending = Arc::new(PendingReply {
                    window: None,
                    state: Mutex::new(ReplyState {
                        receiver,
                        text: None,
                        disconnected: false,
                    }),
                    observed: AtomicBool::new(false),
                    delivered: AtomicBool::new(false),
                });
                state.pending = Some(pending.clone());
                let Some(owner) = group.upgrade() else { return };
                let mut registry = lock(&owner.registry);
                registry.replies.push(pending);
                if owner.sealed.load(Ordering::Acquire) {
                    // No request escaped; keep its original reply sender in the queue.
                    registry.queue.push_back(Request { line, reply });
                    return;
                }
                registry.queue.push_back(Request { line, reply });
            }
            #[cfg(test)]
            if group
                .upgrade()
                .is_some_and(|owner| owner.test.panic_client.load(Ordering::Acquire))
            {
                panic!("fixed retained client panic after request registration");
            }
            input.stage.store(3, Ordering::Release);
            loop {
                if sealed(group) {
                    return;
                }
                let state = lock(&input.io);
                let pending = state.pending.as_ref().unwrap();
                let mut response = lock(&pending.state);
                match response.receiver.try_recv() {
                    Ok(mut text) => {
                        text.push('\n');
                        response.text = Some(text);
                        pending.observed.store(true, Ordering::Release);
                        if let Some(owner) = group.upgrade() {
                            lock(&owner.registry).observed += 1;
                        }
                        break;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        response.disconnected = true;
                        response.text = Some("{\"error\":\"game exited\"}\n".into());
                        pending.observed.store(true, Ordering::Release);
                        if let Some(owner) = group.upgrade() {
                            lock(&owner.registry).observed += 1;
                        }
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
                drop(response);
                drop(state);
                thread::sleep(Duration::from_millis(2));
            }
            input.stage.store(4, Ordering::Release);
            {
                let mut state = lock(&input.io);
                let pending = state.pending.as_ref().unwrap().clone();
                let mut response = lock(&pending.state);
                let bytes = response.text.as_ref().unwrap().as_bytes();
                while state.written < bytes.len() {
                    match state
                        .socket
                        .as_ref()
                        .unwrap()
                        .write(&bytes[state.written..])
                    {
                        Ok(0) => {
                            record_failure(
                                group,
                                RetainedWorkerStage::ResponseWrite,
                                &io::ErrorKind::WriteZero.into(),
                            );
                            return;
                        }
                        Ok(count) => {
                            state.written += count;
                            input.bytes_written.fetch_add(count, Ordering::Release);
                        }
                        Err(error) => {
                            if !sealed(group) {
                                record_failure(group, RetainedWorkerStage::ResponseWrite, &error);
                            }
                            return;
                        }
                    }
                }
                pending.delivered.store(true, Ordering::Release);
                response.text = None;
                state.command = Vec::new();
                state.written = 0;
                state.pending = None;
                if let Some(owner) = group.upgrade() {
                    let mut registry = lock(&owner.registry);
                    registry.delivered += 1;
                    registry
                        .replies
                        .retain(|reply| !Arc::ptr_eq(reply, &pending));
                }
            }
        }
    }

    fn read_authentication(group: &Weak<Group>, input: &ClientInput, token: &str) -> bool {
        let mut state = lock(&input.io);
        while state.auth.len() < 256 {
            match read_byte(&mut state) {
                Ok(0) => return false,
                Ok(_) => {
                    input.auth_read.fetch_add(1, Ordering::Release);
                    let byte = state.byte[0];
                    if byte == b'\n' {
                        break;
                    }
                    state.auth.push(byte);
                }
                Err(error) => {
                    if !sealed(group) {
                        record_failure(group, RetainedWorkerStage::Authentication, &error);
                    }
                    return false;
                }
            }
        }
        let line = String::from_utf8_lossy(&state.auth);
        let ok = line
            .trim()
            .strip_prefix("auth ")
            .is_some_and(|value| same_token(value.trim(), token));
        state.outgoing = if ok {
            b"{\"ok\":true}\n".to_vec()
        } else {
            b"{\"error\":\"bad or missing token: the first line must be auth TOKEN\"}\n".to_vec()
        };
        while state.written < state.outgoing.len() {
            match state
                .socket
                .as_ref()
                .unwrap()
                .write(&state.outgoing[state.written..])
            {
                Ok(0) => return false,
                Ok(count) => state.written += count,
                Err(error) => {
                    if !sealed(group) {
                        record_failure(group, RetainedWorkerStage::Authentication, &error);
                    }
                    return false;
                }
            }
        }
        state.written = 0;
        state.outgoing = Vec::new();
        if ok {
            state.auth = Vec::new();
        }
        ok
    }

    fn cleanup_worker(group: Weak<Group>) {
        // Prestarted fixed actor, not a new shutdown job. The UI sets this
        // receipt only after all original acquisition/client handles are joined.
        loop {
            let Some(owner) = group.upgrade() else { return };
            if owner.cleanup_requested.load(Ordering::Acquire) {
                break;
            }
            drop(owner);
            thread::sleep(Duration::from_millis(2));
        }
        let Some(owner) = group.upgrade() else { return };
        let acquisition = owner.acquisition.clone();
        let replies = lock(&owner.registry).replies.clone();
        let clients: Vec<_> = lock(&owner.registry)
            .clients
            .iter()
            .enumerate()
            .map(|(index, client)| (index, client.input.clone()))
            .collect();
        drop(owner);
        #[cfg(test)]
        test_gate(&group, TestPoint::Cleanup);
        if let Some(owner) = group.upgrade()
            && let Some(window) = &owner.window
        {
            window::harvest_retired(&owner, window);
        }
        for (index, client) in clients {
            retire_client_socket(&group, index, &client);
        }
        // Original response receivers survive workers, including a client panic.
        for pending in replies {
            if pending.observed.load(Ordering::Acquire) {
                continue;
            }
            let mut response = lock(&pending.state);
            match response.receiver.try_recv() {
                Ok(text) => {
                    response.text = Some(text);
                    pending.observed.store(true, Ordering::Release);
                    if let Some(owner) = group.upgrade() {
                        lock(&owner.registry).observed += 1;
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    response.disconnected = true;
                    pending.observed.store(true, Ordering::Release);
                    if let Some(owner) = group.upgrade() {
                        lock(&owner.registry).observed += 1;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
        if let Some(owner) = group.upgrade()
            && let Some(window) = &owner.window
        {
            window.wait_cleanup_hold();
        }
        let result = {
            let mut input = lock(&acquisition);
            cleanup_endpoint(&group, &mut input)
        };
        if let Some(owner) = group.upgrade() {
            let mut registry = lock(&owner.registry);
            match result {
                Ok(Some(state)) => registry.endpoint = state,
                Ok(None) => {}
                Err((state, kind, detail)) => {
                    registry.endpoint = state;
                    registry.failures.push(RetainedFailure {
                        stage: RetainedWorkerStage::Cleanup,
                        kind,
                        detail,
                    });
                }
            }
        }
    }

    /// Cooperating isolated-path contract only: checking identity then unlinking
    /// is not an atomic defense against an uncooperating same-user replacement.
    fn cleanup_endpoint(
        group: &Weak<Group>,
        input: &mut Acquisition,
    ) -> Result<Option<RetainedEndpointState>, (RetainedEndpointState, RetainedFailureKind, String)>
    {
        input.listener.take();
        if !input.owns_path {
            return Ok(None);
        }
        #[cfg(test)]
        if group
            .upgrade()
            .is_some_and(|owner| owner.test.fail_cleanup.load(Ordering::Acquire))
        {
            return Err((
                RetainedEndpointState::Failed,
                RetainedFailureKind::NativeIo(io::ErrorKind::PermissionDenied),
                "fixed native cleanup refusal".into(),
            ));
        }
        #[cfg(not(test))]
        let _ = group;
        let path = input.path.as_ref().unwrap();
        let Some(expected) = input.identity.as_ref().filter(|identity| identity.usable()) else {
            return Err((
                RetainedEndpointState::Ambiguous,
                RetainedFailureKind::EndpointAmbiguous,
                "original endpoint identity unavailable".into(),
            ));
        };
        let metadata = std::fs::symlink_metadata(path).map_err(|error| {
            (
                RetainedEndpointState::Failed,
                RetainedFailureKind::NativeIo(error.kind()),
                error.to_string(),
            )
        })?;
        if &EndpointIdentity::from_metadata(&metadata) != expected {
            return Err((
                RetainedEndpointState::Replaced,
                RetainedFailureKind::EndpointReplaced,
                "observed replacement endpoint preserved".into(),
            ));
        }
        if uses_tcp(path) {
            let actual = std::fs::read(path).map_err(|error| {
                (
                    RetainedEndpointState::Failed,
                    RetainedFailureKind::NativeIo(error.kind()),
                    error.to_string(),
                )
            })?;
            if actual != input.publication[..input.publication_written] {
                return Err((
                    RetainedEndpointState::Replaced,
                    RetainedFailureKind::EndpointReplaced,
                    "observed changed endpoint content preserved".into(),
                ));
            }
        }
        input.endpoint_file.take();
        std::fs::remove_file(path).map_err(|error| {
            (
                RetainedEndpointState::Failed,
                RetainedFailureKind::NativeIo(error.kind()),
                error.to_string(),
            )
        })?;
        input.owns_path = false;
        Ok(Some(RetainedEndpointState::Released))
    }

    /// Fixed window-only policy. Native actors own all IO; grants are credits,
    /// never callbacks, jobs, socket handles, or a public executor.
    mod window {
        use super::*;
        #[cfg(test)]
        use std::sync::atomic::AtomicU64;

        const SLOTS: usize = 8;
        const LINE: usize = 4096;
        const BUFFER: usize = LINE + 2;
        const OUTPUT: usize = 1024 * 1024;
        const TOTAL_OUTPUT: usize = SLOTS * OUTPUT;
        const OPERATION: Duration = Duration::from_secs(10);
        const TURN: Duration = Duration::from_millis(2);

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum WindowCommand {
            Status,
            CloseStatus,
            Close { quit: bool },
            Retry { expected_attempt: u64 },
            Unsupported,
        }

        pub enum WindowReply<'a> {
            Status {
                phase: &'a str,
                stage: &'a str,
                attempt: u64,
                detail: &'a str,
                world_closed: bool,
                control_sealed: bool,
            },
            Admission {
                accepted: bool,
                attempt: u64,
            },
            Error {
                message: &'a str,
            },
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum WindowReplyRefusal {
            TooLarge,
            Budget,
            Allocation,
            Disconnected,
        }

        /// No raw reply Sender escapes. Dropping a request resolves its one-shot
        /// ticket; the actual receiver stays owned until native observation.
        pub struct WindowRequest {
            line: String,
            command: WindowCommand,
            reply: Option<mpsc::Sender<String>>,
            pending: Arc<WindowPending>,
            owner: Weak<WindowGroup>,
        }

        impl WindowRequest {
            pub fn line(&self) -> &str {
                &self.line
            }
            pub fn command(&self) -> WindowCommand {
                self.command
            }

            pub fn reply(mut self, reply: WindowReply<'_>) -> Result<(), WindowReplyRefusal> {
                let owner = self
                    .owner
                    .upgrade()
                    .ok_or(WindowReplyRefusal::Disconnected)?;
                let text = encode(&owner, &self.pending, reply)?;
                match self.reply.take().unwrap().send(text) {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        drop(error.0);
                        release_reply(&owner, &self.pending);
                        Err(WindowReplyRefusal::Disconnected)
                    }
                }
            }
        }

        impl Drop for WindowRequest {
            fn drop(&mut self) {
                // The channel's actual disconnect is observed by a native actor.
                self.reply.take();
                self.pending.ticket_live.store(false, Ordering::Release);
            }
        }

        #[derive(Clone, Debug)]
        pub struct WindowFrameObservation {
            pub supported: bool,
            pub generation: u64,
            pub grant_accepted: bool,
            pub total_starts: usize,
            pub listener_starts: usize,
            pub client_starts: [usize; SLOTS],
            pub lifetime_starts: usize,
            pub write_would_block: usize,
            pub deadline_expired: bool,
            pub reserved_slots: usize,
            pub live_slots: usize,
            pub pending_requests: usize,
            pub queued_requests: usize,
            pub reserved_response_bytes: usize,
            pub canceled_reply_bytes: usize,
            pub expired_operations: usize,
            pub retired_slots: usize,
        }

        impl WindowFrameObservation {
            pub(super) fn unsupported() -> Self {
                Self {
                    supported: false,
                    generation: 0,
                    grant_accepted: false,
                    total_starts: 0,
                    listener_starts: 0,
                    client_starts: [0; SLOTS],
                    lifetime_starts: 0,
                    write_would_block: 0,
                    deadline_expired: false,
                    reserved_slots: 0,
                    live_slots: 0,
                    pending_requests: 0,
                    queued_requests: 0,
                    reserved_response_bytes: 0,
                    canceled_reply_bytes: 0,
                    expired_operations: 0,
                    retired_slots: 0,
                }
            }
        }

        pub(super) struct WindowGroup {
            state: Mutex<WindowState>,
            // Native IO holds this separate mutex for one nonblocking syscall.
            // The UI ONLY try_locks it, and observes the separately published
            // immutable ledger; it never waits behind a native operation.
            native: Mutex<Option<Arc<FrameLedger>>>,
            bytes: AtomicUsize,
            starts: AtomicUsize,
            write_blocks: AtomicUsize,
            canceled: AtomicUsize,
            expired: AtomicUsize,
            retired: AtomicUsize,
            pending: AtomicUsize,
            #[cfg(test)]
            time_offset: AtomicU64,
            #[cfg(test)]
            retirement: TestGate,
            #[cfg(test)]
            auth_bulk_read: TestGate,
            #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
            cleanup_hold: Mutex<Option<Arc<WindowCleanupGate>>>,
        }

        #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
        struct WindowCleanupGate {
            entered: AtomicBool,
            release: AtomicBool,
        }

        #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
        pub struct WindowCleanupHold {
            gate: Arc<WindowCleanupGate>,
        }

        #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
        impl WindowCleanupHold {
            pub fn entered(&self) -> bool {
                self.gate.entered.load(Ordering::Acquire)
            }
            pub fn release(&self) {
                self.gate.release.store(true, Ordering::Release);
            }
        }

        #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
        impl Drop for WindowCleanupHold {
            fn drop(&mut self) {
                self.release();
            }
        }

        struct WindowState {
            slots: [Option<Slot>; SLOTS],
            generations: [u64; SLOTS],
            requests: VecDeque<WindowRequest>,
            frame: Option<Arc<FrameLedger>>,
        }

        struct FrameLedger {
            generation: u64,
            deadline: Instant,
            total: AtomicUsize,
            listener: AtomicUsize,
            clients: [AtomicUsize; SLOTS],
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub(super) struct SlotId {
            index: usize,
            generation: u64,
        }

        struct Slot {
            id: SlotId,
            client: Option<usize>,
        }

        pub(super) struct WindowClient {
            id: SlotId,
            io: Mutex<WindowIo>,
            pub(super) failed: AtomicBool,
        }

        struct WindowIo {
            buffer: Option<Box<[u8; BUFFER]>>,
            used: usize,
            first_byte: Option<Instant>,
            authenticated: bool,
            auth_started: Instant,
            auth_output: Option<String>,
            auth_reserved: usize,
            written: usize,
            eof: bool,
            pending: Option<Arc<PendingReply>>,
            response_started: Option<Instant>,
        }

        pub(super) struct WindowPending {
            id: SlotId,
            bytes: AtomicUsize,
            ticket_live: AtomicBool,
        }

        impl WindowGroup {
            pub(super) fn new() -> Self {
                Self {
                    state: Mutex::new(WindowState {
                        slots: std::array::from_fn(|_| None),
                        generations: [0; SLOTS],
                        requests: VecDeque::with_capacity(SLOTS),
                        frame: None,
                    }),
                    native: Mutex::new(None),
                    bytes: AtomicUsize::new(0),
                    starts: AtomicUsize::new(0),
                    write_blocks: AtomicUsize::new(0),
                    canceled: AtomicUsize::new(0),
                    expired: AtomicUsize::new(0),
                    retired: AtomicUsize::new(0),
                    pending: AtomicUsize::new(0),
                    #[cfg(test)]
                    time_offset: AtomicU64::new(0),
                    #[cfg(test)]
                    retirement: TestGate::default(),
                    #[cfg(test)]
                    auth_bulk_read: TestGate::default(),
                    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
                    cleanup_hold: Mutex::new(None),
                }
            }

            #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
            pub(super) fn arm_cleanup_hold(&self) -> Result<WindowCleanupHold, String> {
                let mut slot = lock(&self.cleanup_hold);
                if slot.is_some() {
                    return Err("original window cleanup hold already armed".into());
                }
                let gate = Arc::new(WindowCleanupGate {
                    entered: AtomicBool::new(false),
                    release: AtomicBool::new(false),
                });
                *slot = Some(gate.clone());
                Ok(WindowCleanupHold { gate })
            }

            #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
            pub(super) fn wait_cleanup_hold(&self) {
                let gate = lock(&self.cleanup_hold).clone();
                if let Some(gate) = gate {
                    gate.entered.store(true, Ordering::Release);
                    while !gate.release.load(Ordering::Acquire) {
                        thread::sleep(Duration::from_millis(1));
                    }
                }
            }

            fn now(&self) -> Instant {
                #[cfg(test)]
                {
                    Instant::now() + Duration::from_millis(self.time_offset.load(Ordering::Acquire))
                }
                #[cfg(not(test))]
                {
                    Instant::now()
                }
            }

            pub(super) fn grant(&self, generation: u64) -> WindowFrameObservation {
                let Ok(mut native) = self.native.try_lock() else {
                    return self.observe(false);
                };
                if native
                    .as_ref()
                    .is_some_and(|frame| generation <= frame.generation)
                {
                    return self.observe(false);
                }
                let frame = Arc::new(FrameLedger {
                    generation,
                    deadline: self.now() + TURN,
                    total: AtomicUsize::new(0),
                    listener: AtomicUsize::new(0),
                    clients: std::array::from_fn(|_| AtomicUsize::new(0)),
                });
                *native = Some(frame.clone());
                lock(&self.state).frame = Some(frame);
                drop(native);
                self.observe(true)
            }

            pub(super) fn observe(&self, accepted: bool) -> WindowFrameObservation {
                let state = lock(&self.state);
                let mut result = WindowFrameObservation::unsupported();
                result.supported = true;
                result.grant_accepted = accepted;
                result.reserved_slots = state
                    .slots
                    .iter()
                    .flatten()
                    .filter(|slot| slot.client.is_none())
                    .count();
                result.live_slots = state
                    .slots
                    .iter()
                    .flatten()
                    .filter(|slot| slot.client.is_some())
                    .count();
                result.queued_requests = state.requests.len();
                result.pending_requests = self.pending.load(Ordering::Acquire);
                if let Some(frame) = &state.frame {
                    result.generation = frame.generation;
                    result.deadline_expired = self.now() >= frame.deadline;
                    result.total_starts = frame.total.load(Ordering::Acquire);
                    result.listener_starts = frame.listener.load(Ordering::Acquire);
                    result.client_starts =
                        std::array::from_fn(|index| frame.clients[index].load(Ordering::Acquire));
                }
                result.lifetime_starts = self.starts.load(Ordering::Acquire);
                result.write_would_block = self.write_blocks.load(Ordering::Acquire);
                result.reserved_response_bytes = self.bytes.load(Ordering::Acquire);
                result.canceled_reply_bytes = self.canceled.load(Ordering::Acquire);
                result.expired_operations = self.expired.load(Ordering::Acquire);
                result.retired_slots = self.retired.load(Ordering::Acquire);
                result
            }

            pub(super) fn take_request(&self) -> Option<WindowRequest> {
                lock(&self.state).requests.pop_front()
            }

            fn reserve_slot(&self) -> Option<SlotId> {
                let mut state = lock(&self.state);
                let index = state.slots.iter().position(Option::is_none)?;
                let generation = state.generations[index].checked_add(1)?;
                state.generations[index] = generation;
                let id = SlotId { index, generation };
                state.slots[index] = Some(Slot { id, client: None });
                Some(id)
            }

            fn release_slot(&self, id: SlotId) {
                let mut state = lock(&self.state);
                if state.slots[id.index]
                    .as_ref()
                    .is_some_and(|slot| slot.id == id)
                {
                    state.slots[id.index] = None;
                    self.retired.fetch_add(1, Ordering::Release);
                }
            }

            fn reserve_bytes(&self, size: usize) -> bool {
                // Keep the workspace MSRV: try_update is newer, while its old
                // name is deprecated by the pinned development compiler.
                let mut current = self.bytes.load(Ordering::Acquire);
                loop {
                    let Some(total) = current
                        .checked_add(size)
                        .filter(|total| *total <= TOTAL_OUTPUT)
                    else {
                        return false;
                    };
                    match self.bytes.compare_exchange_weak(
                        current,
                        total,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return true,
                        Err(observed) => current = observed,
                    }
                }
            }

            fn credit<'a>(
                &'a self,
                slot: Option<SlotId>,
            ) -> Option<MutexGuard<'a, Option<Arc<FrameLedger>>>> {
                let native = self.native.try_lock().ok()?;
                let frame = native.as_ref()?;
                let counter = match slot {
                    Some(id) => &frame.clients[id.index],
                    None => &frame.listener,
                };
                let limit = if slot.is_some() { 3 } else { 8 };
                if self.now() >= frame.deadline
                    || counter.load(Ordering::Acquire) >= limit
                    || frame.total.load(Ordering::Acquire) >= 32
                {
                    return None;
                }
                counter.fetch_add(1, Ordering::Release);
                frame.total.fetch_add(1, Ordering::Release);
                self.starts.fetch_add(1, Ordering::Release);
                Some(native)
            }

            fn accept(&self, listener: &NativeListener) -> Option<io::Result<Stream>> {
                let _credit = self.credit(None)?;
                Some(listener.accept())
            }

            fn read(
                &self,
                id: SlotId,
                socket: &NativeSocket,
                bytes: &mut [u8],
            ) -> Option<io::Result<usize>> {
                let _credit = self.credit(Some(id))?;
                Some(socket.read(bytes))
            }

            fn write(
                &self,
                id: SlotId,
                socket: &NativeSocket,
                bytes: &[u8],
            ) -> Option<io::Result<usize>> {
                let _credit = self.credit(Some(id))?;
                Some(socket.write(bytes))
            }
        }

        fn command(line: &str) -> WindowCommand {
            let mut words = line.split_whitespace();
            match (words.next(), words.next(), words.next()) {
                (Some("status"), None, None) => WindowCommand::Status,
                (Some("close-status"), None, None) => WindowCommand::CloseStatus,
                (Some("close" | "world-leave"), None, None) => WindowCommand::Close { quit: false },
                (Some("quit"), None, None) => WindowCommand::Close { quit: true },
                (Some("close-retry"), Some(attempt), None) => attempt
                    .parse()
                    .map_or(WindowCommand::Unsupported, |expected_attempt| {
                        WindowCommand::Retry { expected_attempt }
                    }),
                _ => WindowCommand::Unsupported,
            }
        }

        fn quoted_size(value: &str) -> Option<usize> {
            if value.len() > OUTPUT {
                return None;
            }
            value.bytes().try_fold(2usize, |size, byte| {
                size.checked_add(match byte {
                    b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 8 | 12 => 2,
                    0..=31 => 6,
                    _ => 1,
                })
                .filter(|size| *size <= OUTPUT)
            })
        }

        fn digits(mut value: u64) -> usize {
            let mut count = 1;
            while value >= 10 {
                value /= 10;
                count += 1;
            }
            count
        }

        fn bool_size(value: bool) -> usize {
            if value { 4 } else { 5 }
        }

        fn reply_size(reply: &WindowReply<'_>) -> Option<usize> {
            match reply {
                WindowReply::Status {
                    phase,
                    stage,
                    attempt,
                    detail,
                    world_closed,
                    control_sealed,
                } => {
                    let fixed = "{\"phase\":,\"stage\":,\"attempt\":,\"detail\":,\"world_closed\":,\"control_sealed\":}\n".len();
                    fixed
                        .checked_add(quoted_size(phase)?)?
                        .checked_add(quoted_size(stage)?)?
                        .checked_add(digits(*attempt))?
                        .checked_add(quoted_size(detail)?)?
                        .checked_add(bool_size(*world_closed))?
                        .checked_add(bool_size(*control_sealed))
                }
                WindowReply::Admission { accepted, attempt } => {
                    "{\"ok\":true,\"accepted\":,\"attempt\":}\n"
                        .len()
                        .checked_add(bool_size(*accepted))?
                        .checked_add(digits(*attempt))
                }
                WindowReply::Error { message } => {
                    "{\"error\":}\n".len().checked_add(quoted_size(message)?)
                }
            }
        }

        fn quote(out: &mut String, value: &str) {
            use std::fmt::Write as _;
            out.push('"');
            for character in value.chars() {
                match character {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    '\u{8}' => out.push_str("\\b"),
                    '\u{c}' => out.push_str("\\f"),
                    '\0'..='\u{1f}' => {
                        let _ = write!(out, "\\u{:04x}", character as u32);
                    }
                    _ => out.push(character),
                }
            }
            out.push('"');
        }

        fn release_reply(owner: &WindowGroup, pending: &WindowPending) {
            let bytes = pending.bytes.swap(0, Ordering::AcqRel);
            owner.bytes.fetch_sub(bytes, Ordering::AcqRel);
        }

        fn encode(
            owner: &WindowGroup,
            pending: &WindowPending,
            reply: WindowReply<'_>,
        ) -> Result<String, WindowReplyRefusal> {
            use std::fmt::Write as _;
            if !lock(&owner.state).slots[pending.id.index]
                .as_ref()
                .is_some_and(|slot| slot.id == pending.id)
            {
                return Err(WindowReplyRefusal::Disconnected);
            }
            let size = reply_size(&reply)
                .filter(|size| *size <= OUTPUT)
                .ok_or(WindowReplyRefusal::TooLarge)?;
            if !owner.reserve_bytes(size) {
                return Err(WindowReplyRefusal::Budget);
            }
            pending.bytes.store(size, Ordering::Release);
            let mut out = String::new();
            if out.try_reserve_exact(size).is_err() {
                release_reply(owner, pending);
                return Err(WindowReplyRefusal::Allocation);
            }
            match reply {
                WindowReply::Status {
                    phase,
                    stage,
                    attempt,
                    detail,
                    world_closed,
                    control_sealed,
                } => {
                    out.push_str("{\"phase\":");
                    quote(&mut out, phase);
                    out.push_str(",\"stage\":");
                    quote(&mut out, stage);
                    let _ = write!(out, ",\"attempt\":{attempt},\"detail\":");
                    quote(&mut out, detail);
                    let _ = writeln!(
                        out,
                        ",\"world_closed\":{world_closed},\"control_sealed\":{control_sealed}}}"
                    );
                }
                WindowReply::Admission { accepted, attempt } => {
                    let _ = writeln!(
                        out,
                        "{{\"ok\":true,\"accepted\":{accepted},\"attempt\":{attempt}}}"
                    );
                }
                WindowReply::Error { message } => {
                    out.push_str("{\"error\":");
                    quote(&mut out, message);
                    out.push_str("}\n");
                }
            }
            debug_assert_eq!(size, out.len());
            Ok(out)
        }

        fn native_failure(
            group: &Weak<Group>,
            client: &WindowClient,
            stage: RetainedWorkerStage,
            error: &io::Error,
        ) {
            if sealed(group) {
                return;
            }
            client.failed.store(true, Ordering::Release);
            record_failure(group, stage, error);
        }

        pub(super) fn listener_worker(group: Weak<Group>) {
            let Some(owner) = group.upgrade() else { return };
            let acquisition = owner.acquisition.clone();
            let window = owner.window.as_ref().unwrap().clone();
            drop(owner);
            #[cfg(test)]
            test_gate(&group, TestPoint::Acquisition);
            if sealed(&group) {
                return;
            }
            let result = acquire_endpoint(&mut lock(&acquisition));
            if let Err(error) = result {
                if let Some(owner) = group.upgrade() {
                    let mut registry = lock(&owner.registry);
                    let exists = error.kind() == io::ErrorKind::AlreadyExists;
                    registry.endpoint = if exists {
                        RetainedEndpointState::Refused
                    } else {
                        RetainedEndpointState::Failed
                    };
                    registry.failures.push(RetainedFailure {
                        stage: RetainedWorkerStage::Acquisition,
                        kind: if exists {
                            RetainedFailureKind::EndpointExists
                        } else {
                            RetainedFailureKind::NativeIo(error.kind())
                        },
                        detail: error.to_string(),
                    });
                }
                return;
            }
            #[cfg(test)]
            test_gate(&group, TestPoint::Acquired);
            if let Some(owner) = group.upgrade() {
                lock(&owner.registry).endpoint = RetainedEndpointState::Published;
            }
            #[cfg(test)]
            test_gate(&group, TestPoint::Published);
            while !sealed(&group) {
                if let Some(owner) = group.upgrade() {
                    harvest_retired(&owner, &window);
                }
                let Some(id) = window.reserve_slot() else {
                    thread::sleep(Duration::from_millis(1));
                    continue;
                };
                let next = {
                    let mut acquisition = lock(&acquisition);
                    acquisition.window_reservation = Some(id);
                    match window.accept(acquisition.listener.as_ref().unwrap()) {
                        Some(Ok(stream)) => {
                            let token = if matches!(stream, Stream::Tcp(_)) {
                                Some(acquisition.token.clone())
                            } else {
                                None
                            };
                            acquisition.accepted = Some(Arc::new(ClientInput {
                                window: Some(Arc::new(WindowClient {
                                    id,
                                    io: Mutex::new(WindowIo {
                                        buffer: Some(Box::new([0; BUFFER])),
                                        used: 0,
                                        first_byte: None,
                                        authenticated: token.is_none(),
                                        auth_started: window.now(),
                                        auth_output: None,
                                        auth_reserved: 0,
                                        written: 0,
                                        eof: false,
                                        pending: None,
                                        response_started: None,
                                    }),
                                    failed: AtomicBool::new(false),
                                })),
                                token,
                                io: Mutex::new(ClientIo {
                                    socket: Some(NativeSocket(stream)),
                                    ..ClientIo::default()
                                }),
                                stage: AtomicUsize::new(0),
                                bytes_written: AtomicUsize::new(0),
                                auth_read: AtomicUsize::new(0),
                                command_read: AtomicUsize::new(0),
                                fd_retired: AtomicBool::new(false),
                            }));
                            Ok(acquisition.accepted.as_ref().unwrap().clone())
                        }
                        Some(Err(error)) => Err(Some(error)),
                        None => Err(None),
                    }
                };
                let input = match next {
                    Ok(input) => input,
                    Err(error) => {
                        lock(&acquisition).window_reservation = None;
                        window.release_slot(id);
                        if let Some(error) = error
                            && error.kind() != io::ErrorKind::WouldBlock
                        {
                            record_failure(&group, RetainedWorkerStage::Listener, &error);
                            return;
                        }
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                };
                #[cfg(test)]
                test_gate(&group, TestPoint::Accepted);
                let Some(owner) = group.upgrade() else { return };
                let (index, refused) = {
                    let mut registry = lock(&owner.registry);
                    let index = registry.clients.len();
                    let refused = owner.sealed.load(Ordering::Acquire);
                    registry.clients.push(ClientRecord {
                        input: input.clone(),
                        stop: None,
                        handle: None,
                        state: if refused {
                            WorkerState::Refused
                        } else {
                            WorkerState::SpawnPending
                        },
                        refused,
                    });
                    if refused {
                        registry.refused += 1;
                    }
                    (index, refused)
                };
                {
                    let mut state = lock(&window.state);
                    state.slots[id.index].as_mut().unwrap().client = Some(index);
                }
                {
                    let mut acquisition = lock(&acquisition);
                    acquisition.accepted = None;
                    acquisition.window_reservation = None;
                }
                let cap = {
                    let state = lock(&input.io);
                    let stream = &state.socket.as_ref().unwrap().0;
                    let nonblocking = match stream {
                        #[cfg(unix)]
                        Stream::Unix(stream) => stream.set_nonblocking(true),
                        Stream::Tcp(stream) => stream.set_nonblocking(true),
                    };
                    nonblocking.and_then(|()| stream.try_clone())
                };
                match cap {
                    Ok(cap) if !refused => {
                        let stop = Arc::new(NativeSocket(cap));
                        let fenced = {
                            let mut registry = lock(&owner.registry);
                            registry.clients[index].stop = Some(stop.clone());
                            owner.sealed.load(Ordering::Acquire)
                        };
                        if fenced {
                            stop.shutdown();
                        }
                        if spawn_fixed(&owner, Actor::Client(index)).is_err() {
                            retire_client_socket(&group, index, &input);
                        }
                    }
                    Ok(cap) => {
                        drop(cap);
                        retire_client_socket(&group, index, &input);
                    }
                    Err(error) => {
                        input
                            .window
                            .as_ref()
                            .unwrap()
                            .failed
                            .store(true, Ordering::Release);
                        record_failure(&group, RetainedWorkerStage::Listener, &error);
                        let mut registry = lock(&owner.registry);
                        registry.clients[index].state = WorkerState::Refused;
                        registry.clients[index].refused = true;
                        registry.refused += usize::from(!refused);
                        drop(registry);
                        retire_client_socket(&group, index, &input);
                    }
                }
                #[cfg(test)]
                if owner
                    .test
                    .panic_listener_after_client
                    .load(Ordering::Acquire)
                {
                    panic!("fixed retained listener panic after client registration");
                }
            }
        }

        fn consume(io: &mut WindowIo, count: usize) {
            let used = io.used;
            io.buffer.as_mut().unwrap().copy_within(count..used, 0);
            io.used -= count;
            if io.used == 0 {
                io.first_byte = None;
            }
        }

        fn expired(window: &WindowGroup, started: Option<Instant>) -> bool {
            started
                .is_some_and(|started| window.now().saturating_duration_since(started) >= OPERATION)
        }

        fn read_more(
            window: &WindowGroup,
            state: &mut WindowIo,
            socket: &NativeSocket,
            id: SlotId,
        ) -> Option<io::Result<usize>> {
            let used = state.used;
            let result = window.read(id, socket, &mut state.buffer.as_mut().unwrap()[used..])?;
            if let Ok(count) = result {
                if count > 0 && state.used == 0 {
                    state.first_byte = Some(window.now());
                }
                state.used += count;
                state.eof = count == 0;
            }
            Some(result)
        }

        pub(super) fn client_worker(group: &Weak<Group>, input: &ClientInput) {
            let Some(owner) = group.upgrade() else { return };
            let window = owner.window.as_ref().unwrap().clone();
            drop(owner);
            let client = input.window.as_ref().unwrap();
            #[cfg(test)]
            test_gate(group, TestPoint::Client);
            loop {
                if sealed(group) {
                    return;
                }
                let mut io = lock(&client.io);
                let socket_guard = lock(&input.io);
                let socket = socket_guard.socket.as_ref().unwrap();
                // A bulk read may already own the next incomplete line while
                // authentication or an earlier response is still in flight.
                // Its original clock runs independently of those operations;
                // observing it neither parses the tail nor renews its deadline.
                if !io.eof
                    && io.used > 0
                    && io.buffer.as_ref().unwrap()[io.used - 1] != b'\n'
                    && expired(&window, io.first_byte)
                {
                    window.expired.fetch_add(1, Ordering::Release);
                    return;
                }
                if !io.authenticated {
                    input.stage.store(1, Ordering::Release);
                    if expired(&window, Some(io.auth_started)) {
                        window.expired.fetch_add(1, Ordering::Release);
                        return;
                    }
                    if io.auth_output.is_none() {
                        if let Some(newline) = io.buffer.as_ref().unwrap()[..io.used]
                            .iter()
                            .position(|byte| *byte == b'\n')
                        {
                            if newline >= 256 {
                                return;
                            }
                            let line =
                                String::from_utf8_lossy(&io.buffer.as_ref().unwrap()[..newline]);
                            let ok = line.trim().strip_prefix("auth ").is_some_and(|token| {
                                same_token(token.trim(), input.token.as_deref().unwrap())
                            });
                            let response = if ok {
                                "{\"ok\":true}\n"
                            } else {
                                "{\"error\":\"bad or missing token: the first line must be auth TOKEN\"}\n"
                            };
                            if !window.reserve_bytes(response.len()) {
                                return;
                            }
                            io.auth_reserved = response.len();
                            io.auth_output = Some(response.to_owned());
                            consume(&mut io, newline + 1);
                            // Failed authentication is retired after its fixed error write.
                            io.eof |= !ok;
                        } else if io.used >= 256 || io.eof {
                            return;
                        } else {
                            let read = read_more(&window, &mut io, socket, client.id);
                            if let Some(Err(error)) = &read
                                && error.kind() != io::ErrorKind::WouldBlock
                            {
                                native_failure(
                                    group,
                                    client,
                                    RetainedWorkerStage::Authentication,
                                    error,
                                );
                                return;
                            }
                            #[cfg(test)]
                            let received = matches!(read, Some(Ok(count)) if count > 0);
                            drop(socket_guard);
                            drop(io);
                            #[cfg(test)]
                            if received {
                                window.auth_bulk_read.entered.store(true, Ordering::Release);
                                while window.auth_bulk_read.hold.load(Ordering::Acquire) {
                                    thread::sleep(Duration::from_millis(1));
                                }
                            }
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                    }
                    let text = io.auth_output.as_ref().unwrap();
                    match window.write(client.id, socket, &text.as_bytes()[io.written..]) {
                        Some(Ok(0)) => return,
                        Some(Ok(count)) => io.written += count,
                        Some(Err(error)) if error.kind() == io::ErrorKind::WouldBlock => {
                            window.write_blocks.fetch_add(1, Ordering::Release);
                        }
                        Some(Err(error)) if error.kind() != io::ErrorKind::WouldBlock => {
                            native_failure(
                                group,
                                client,
                                RetainedWorkerStage::Authentication,
                                &error,
                            );
                            return;
                        }
                        _ => {}
                    }
                    if io.written == io.auth_output.as_ref().unwrap().len() {
                        window.bytes.fetch_sub(io.auth_reserved, Ordering::AcqRel);
                        io.auth_reserved = 0;
                        io.auth_output = None;
                        io.written = 0;
                        io.authenticated = true;
                        if io.eof {
                            return;
                        }
                    }
                } else if let Some(pending) = io.pending.clone() {
                    input.stage.store(3, Ordering::Release);
                    if expired(&window, io.response_started) {
                        window.expired.fetch_add(1, Ordering::Release);
                        return;
                    }
                    let mut response = lock(&pending.state);
                    if response.text.is_none() && !pending.observed.load(Ordering::Acquire) {
                        match response.receiver.try_recv() {
                            Ok(text) => {
                                response.text = Some(text);
                                pending.observed.store(true, Ordering::Release);
                                if let Some(owner) = group.upgrade() {
                                    lock(&owner.registry).observed += 1;
                                }
                            }
                            Err(mpsc::TryRecvError::Disconnected) => {
                                response.disconnected = true;
                                let policy = pending.window.as_ref().unwrap();
                                match encode(
                                    &window,
                                    policy,
                                    WindowReply::Error {
                                        message: "request abandoned or reply refused",
                                    },
                                ) {
                                    Ok(text) => response.text = Some(text),
                                    Err(_) => return,
                                }
                                pending.observed.store(true, Ordering::Release);
                                if let Some(owner) = group.upgrade() {
                                    lock(&owner.registry).observed += 1;
                                }
                            }
                            Err(mpsc::TryRecvError::Empty) => {}
                        }
                    }
                    if let Some(text) = &response.text {
                        input.stage.store(4, Ordering::Release);
                        if io.written < text.len() {
                            match window.write(client.id, socket, &text.as_bytes()[io.written..]) {
                                Some(Ok(0)) => return,
                                Some(Ok(count)) => {
                                    io.written += count;
                                    input.bytes_written.fetch_add(count, Ordering::Release);
                                }
                                Some(Err(error)) if error.kind() == io::ErrorKind::WouldBlock => {
                                    window.write_blocks.fetch_add(1, Ordering::Release);
                                }
                                Some(Err(error)) if error.kind() != io::ErrorKind::WouldBlock => {
                                    native_failure(
                                        group,
                                        client,
                                        RetainedWorkerStage::ResponseWrite,
                                        &error,
                                    );
                                    return;
                                }
                                _ => {}
                            }
                        }
                        if io.written == text.len()
                            && !pending
                                .window
                                .as_ref()
                                .unwrap()
                                .ticket_live
                                .load(Ordering::Acquire)
                        {
                            pending.delivered.store(true, Ordering::Release);
                            response.text = None;
                            release_reply(&window, pending.window.as_ref().unwrap());
                            window.pending.fetch_sub(1, Ordering::AcqRel);
                            io.pending = None;
                            io.response_started = None;
                            io.written = 0;
                            if let Some(owner) = group.upgrade() {
                                let mut registry = lock(&owner.registry);
                                registry.delivered += 1;
                                registry
                                    .replies
                                    .retain(|reply| !Arc::ptr_eq(reply, &pending));
                            }
                        }
                    }
                } else {
                    input.stage.store(2, Ordering::Release);
                    if expired(&window, io.first_byte) {
                        window.expired.fetch_add(1, Ordering::Release);
                        return;
                    }
                    let newline = io.buffer.as_ref().unwrap()[..io.used]
                        .iter()
                        .position(|byte| *byte == b'\n');
                    if newline.is_none() && !io.eof {
                        if io.used > LINE {
                            return;
                        }
                        if let Some(Err(error)) = read_more(&window, &mut io, socket, client.id)
                            && error.kind() != io::ErrorKind::WouldBlock
                        {
                            native_failure(group, client, RetainedWorkerStage::CommandRead, &error);
                            return;
                        }
                    } else {
                        if io.used == 0 {
                            return;
                        }
                        let length = newline.unwrap_or(io.used);
                        if length > LINE {
                            return;
                        }
                        let end = if newline.is_some()
                            && length > 0
                            && io.buffer.as_ref().unwrap()[length - 1] == b'\r'
                        {
                            length - 1
                        } else {
                            length
                        };
                        let Ok(line) = std::str::from_utf8(&io.buffer.as_ref().unwrap()[..end])
                        else {
                            return;
                        };
                        if line.trim().is_empty() {
                            consume(&mut io, newline.map_or(length, |n| n + 1));
                        } else {
                            let mut owned_line = String::new();
                            if owned_line.try_reserve_exact(line.len()).is_err() {
                                client.failed.store(true, Ordering::Release);
                                return;
                            }
                            owned_line.push_str(line);
                            let line = owned_line;
                            let selector = command(&line);
                            let (reply, receiver) = mpsc::channel();
                            let policy = Arc::new(WindowPending {
                                id: client.id,
                                bytes: AtomicUsize::new(0),
                                ticket_live: AtomicBool::new(true),
                            });
                            let pending = Arc::new(PendingReply {
                                window: Some(policy.clone()),
                                state: Mutex::new(ReplyState {
                                    receiver,
                                    text: None,
                                    disconnected: false,
                                }),
                                observed: AtomicBool::new(false),
                                delivered: AtomicBool::new(false),
                            });
                            io.pending = Some(pending.clone());
                            window.pending.fetch_add(1, Ordering::Release);
                            io.response_started = Some(window.now());
                            consume(&mut io, newline.map_or(length, |n| n + 1));
                            let Some(owner) = group.upgrade() else { return };
                            lock(&owner.registry).replies.push(pending);
                            let mut state = lock(&window.state);
                            state.requests.push_back(WindowRequest {
                                line,
                                command: selector,
                                reply: Some(reply),
                                pending: policy,
                                owner: Arc::downgrade(&window),
                            });
                            #[cfg(test)]
                            if owner.test.panic_client.load(Ordering::Acquire) {
                                panic!("fixed retained client panic after request registration");
                            }
                        }
                    }
                }
                drop(socket_guard);
                drop(io);
                thread::sleep(Duration::from_millis(1));
            }
        }

        /// Checked join precedes late-receiver resolution and index reuse. The
        /// consumed payload is replaced by compact canceled-byte/outcome history;
        /// this says nothing about external Game cancellation or peer delivery.
        pub(super) fn harvest_retired(group: &Group, window: &WindowGroup) {
            let indices: Vec<_> = lock(&window.state)
                .slots
                .iter()
                .flatten()
                .filter_map(|slot| slot.client)
                .collect();
            let clients: Vec<_> = {
                let registry = lock(&group.registry);
                indices
                    .into_iter()
                    .filter_map(|index| {
                        let client = &registry.clients[index];
                        (client.state.terminal()
                            && client.input.fd_retired.load(Ordering::Acquire)
                            && client.input.window.is_some())
                        .then(|| (index, client.input.clone()))
                    })
                    .collect()
            };
            for (index, input) in clients {
                #[cfg(test)]
                {
                    window.retirement.entered.store(true, Ordering::Release);
                    while window.retirement.hold.load(Ordering::Acquire) {
                        thread::sleep(Duration::from_millis(1));
                    }
                }
                let client = input.window.as_ref().unwrap();
                if client.failed.load(Ordering::Acquire) {
                    continue;
                }
                let mut io = lock(&client.io);
                if let Some(pending) = io.pending.clone() {
                    let policy = pending.window.as_ref().unwrap();
                    if policy.ticket_live.load(Ordering::Acquire) {
                        continue;
                    }
                    let mut response = lock(&pending.state);
                    if !pending.observed.load(Ordering::Acquire) {
                        match response.receiver.try_recv() {
                            Ok(text) => {
                                response.text = Some(text);
                                pending.observed.store(true, Ordering::Release);
                                lock(&group.registry).observed += 1;
                            }
                            Err(mpsc::TryRecvError::Disconnected) => {
                                response.disconnected = true;
                                pending.observed.store(true, Ordering::Release);
                                lock(&group.registry).observed += 1;
                            }
                            Err(mpsc::TryRecvError::Empty) => continue,
                        }
                    }
                    if !pending.delivered.load(Ordering::Acquire) {
                        window.canceled.fetch_add(
                            response.text.as_ref().map_or(0, String::len),
                            Ordering::Release,
                        );
                    }
                    response.text = None;
                    release_reply(window, policy);
                    window.pending.fetch_sub(1, Ordering::AcqRel);
                    io.pending = None;
                    lock(&group.registry)
                        .replies
                        .retain(|reply| !Arc::ptr_eq(reply, &pending));
                }
                window.bytes.fetch_sub(io.auth_reserved, Ordering::AcqRel);
                io.auth_reserved = 0;
                io.auth_output = None;
                io.buffer = None;
                drop(io);
                // Remove only this exact original generation, never a foreign slot.
                let matches = lock(&window.state).slots[client.id.index]
                    .as_ref()
                    .is_some_and(|slot| slot.id == client.id && slot.client == Some(index));
                if matches {
                    window.release_slot(client.id);
                }
            }
        }

        #[cfg(test)]
        mod tests {
            use super::*;
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            const BACKSTOP: Duration = Duration::from_secs(10);

            struct Fixture {
                path: PathBuf,
                owner: Option<RetainedControl>,
                frame: u64,
            }
            struct Peer {
                stream: Stream,
                received: Vec<u8>,
            }

            impl Peer {
                fn write(&mut self, bytes: &[u8]) {
                    self.stream.write_all(bytes).unwrap();
                }
                fn line(&mut self) -> Option<String> {
                    let mut bytes = [0; 8192];
                    loop {
                        if let Some(end) = self.received.iter().position(|byte| *byte == b'\n') {
                            let line: Vec<_> = self.received.drain(..=end).collect();
                            return Some(String::from_utf8(line).unwrap());
                        }
                        match self.stream.read(&mut bytes) {
                            Ok(0) => return None,
                            Ok(count) => self.received.extend_from_slice(&bytes[..count]),
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return None,
                            Err(error) => panic!("native peer read: {error}"),
                        }
                    }
                }
            }

            impl Fixture {
                fn new(tcp: bool) -> Self {
                    let path = std::env::temp_dir().join(format!(
                        "horizon-window-control-{}-{}.{}",
                        std::process::id(),
                        NEXT.fetch_add(1, Ordering::Relaxed),
                        if tcp { "json" } else { "sock" }
                    ));
                    assert!(!path.exists());
                    Self {
                        owner: Some(RetainedControl::prepare_window(&path)),
                        path,
                        frame: 0,
                    }
                }
                fn owner(&mut self) -> &mut RetainedControl {
                    self.owner.as_mut().unwrap()
                }
                fn policy(&self) -> Arc<WindowGroup> {
                    self.owner
                        .as_ref()
                        .unwrap()
                        .group
                        .window
                        .as_ref()
                        .unwrap()
                        .clone()
                }
                fn pump(&mut self) -> WindowFrameObservation {
                    self.frame = self.frame.checked_add(1).unwrap();
                    let frame = self.frame;
                    let view = self.owner().grant_window_frame(frame);
                    assert!(view.total_starts <= 32 && view.listener_starts <= 8);
                    assert!(view.client_starts.iter().all(|count| *count <= 3));
                    assert!(view.live_slots + view.reserved_slots <= SLOTS);
                    assert!(
                        view.pending_requests <= SLOTS
                            && view.reserved_response_bytes <= TOTAL_OUTPUT
                    );
                    view
                }
                fn until(&mut self, mut condition: impl FnMut(&mut Self) -> bool) {
                    let deadline = Instant::now() + BACKSTOP;
                    while !condition(self) {
                        assert!(Instant::now() < deadline, "window native fixture backstop");
                        self.pump();
                        thread::sleep(Duration::from_millis(1));
                    }
                }
                fn start(&mut self) {
                    self.owner().start().unwrap();
                    self.until(|fixture| {
                        fixture.owner().observation().endpoint == RetainedEndpointState::Published
                    });
                }
                fn raw(&self) -> Peer {
                    let stream = if uses_tcp(&self.path) {
                        let (port, _) = read_endpoint(&self.path).unwrap();
                        let stream = TcpStream::connect_timeout(
                            &std::net::SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
                            BACKSTOP,
                        )
                        .unwrap();
                        stream.set_nonblocking(true).unwrap();
                        Stream::Tcp(stream)
                    } else {
                        #[cfg(unix)]
                        {
                            let stream = UnixStream::connect(&self.path).unwrap();
                            stream.set_nonblocking(true).unwrap();
                            Stream::Unix(stream)
                        }
                        #[cfg(not(unix))]
                        unreachable!()
                    };
                    Peer {
                        stream,
                        received: Vec::new(),
                    }
                }
                fn client(&mut self) -> Peer {
                    let mut peer = self.raw();
                    if uses_tcp(&self.path) {
                        let (_, token) = read_endpoint(&self.path).unwrap();
                        peer.write(format!("auth {token}\n").as_bytes());
                        let mut line = None;
                        self.until(|_| {
                            line = peer.line();
                            line.is_some()
                        });
                        assert_eq!(line.as_deref(), Some("{\"ok\":true}\n"));
                    }
                    peer
                }
                fn request(&mut self) -> WindowRequest {
                    let mut request = None;
                    self.until(|fixture| {
                        request = fixture.owner().try_recv_window();
                        request.is_some()
                    });
                    request.unwrap()
                }
                fn closed(mut self) -> ClosedControl {
                    self.policy()
                        .retirement
                        .hold
                        .store(false, Ordering::Release);
                    self.policy()
                        .auth_bulk_read
                        .hold
                        .store(false, Ordering::Release);
                    let mut owner = self.owner.take().unwrap();
                    release_all_gates(&owner.group);
                    owner.begin_close(Instant::now() + BACKSTOP);
                    let deadline = Instant::now() + BACKSTOP;
                    while owner.poll_close().observation.phase != RetainedControlPhase::Quiescent {
                        assert!(Instant::now() < deadline);
                        thread::sleep(Duration::from_millis(1));
                    }
                    match owner.try_closed() {
                        Ok(closed) => closed,
                        Err(_) => panic!("native window close did not transfer"),
                    }
                }
            }

            fn release_all_gates(group: &Group) {
                for gate in [
                    &group.test.acquisition,
                    &group.test.acquired,
                    &group.test.published,
                    &group.test.accepted,
                    &group.test.client,
                    &group.test.cleanup,
                ] {
                    gate.hold.store(false, Ordering::Release);
                }
            }

            impl Drop for Fixture {
                fn drop(&mut self) {
                    if let Some(mut owner) = self.owner.take() {
                        owner
                            .group
                            .window
                            .as_ref()
                            .unwrap()
                            .retirement
                            .hold
                            .store(false, Ordering::Release);
                        owner
                            .group
                            .window
                            .as_ref()
                            .unwrap()
                            .auth_bulk_read
                            .hold
                            .store(false, Ordering::Release);
                        release_all_gates(&owner.group);
                        owner.begin_close(Instant::now() + BACKSTOP);
                        let deadline = Instant::now() + BACKSTOP;
                        while owner.poll_close().observation.phase
                            != RetainedControlPhase::Quiescent
                        {
                            assert!(Instant::now() < deadline);
                            thread::sleep(Duration::from_millis(1));
                        }
                        assert!(owner.try_closed().is_ok());
                    }
                }
            }

            #[test]
            fn window_raw_paths_refuse_before_start_and_after_fence() {
                let mut fixture = Fixture::new(true);
                let sender = fixture.owner().sender();
                let (reply, response) = mpsc::channel();
                let refused = match sender.try_send(Request {
                    line: "original-local".into(),
                    reply,
                }) {
                    Err(value) => value,
                    Ok(()) => panic!("bounded local injection"),
                };
                assert_eq!(refused.line, "original-local");
                refused.reply.send("same-channel".into()).unwrap();
                assert_eq!(response.recv_timeout(BACKSTOP).unwrap(), "same-channel");
                assert!(fixture.owner().try_recv().is_none());
                assert!(!fixture.path.exists());
                fixture.start();
                let mut peer = fixture.client();
                peer.write(b"close-retry 7\n");
                fixture.until(|fixture| fixture.owner().window_observation().queued_requests == 1);
                assert!(fixture.owner().try_recv().is_none());
                let request = fixture.request();
                assert_eq!(
                    request.command(),
                    WindowCommand::Retry {
                        expected_attempt: 7
                    }
                );
                assert_eq!(request.line(), "close-retry 7");
                request
                    .reply(WindowReply::Admission {
                        accepted: false,
                        attempt: 1,
                    })
                    .unwrap();
                let mut line = None;
                fixture.until(|_| {
                    line = peer.line();
                    line.is_some()
                });
                assert_eq!(
                    line.unwrap(),
                    "{\"ok\":true,\"accepted\":false,\"attempt\":1}\n"
                );
                fixture.owner().begin_close(Instant::now() + BACKSTOP);
                assert!(fixture.owner().try_recv_window().is_none());
                assert!(!fixture.pump().grant_accepted);
            }

            #[test]
            fn window_actual_credit_generation_and_expiry() {
                let mut fixture = Fixture::new(true);
                fixture.owner().start().unwrap();
                let deadline = Instant::now() + BACKSTOP;
                while fixture.owner().observation().endpoint != RetainedEndpointState::Published {
                    assert!(Instant::now() < deadline);
                    thread::yield_now();
                }
                let _peer = fixture.raw();
                assert_eq!(fixture.owner().window_observation().lifetime_starts, 0);
                let mut generation = 0;
                let deadline = Instant::now() + BACKSTOP;
                loop {
                    generation += 1;
                    fixture.owner().grant_window_frame(generation);
                    let end = Instant::now() + Duration::from_millis(5);
                    while Instant::now() < end {
                        thread::yield_now();
                    }
                    if fixture.owner().window_observation().lifetime_starts > 0 {
                        break;
                    }
                    assert!(Instant::now() < deadline);
                }
                let first = fixture.owner().window_observation();
                assert!(first.deadline_expired && first.lifetime_starts > 0);
                assert!(
                    first.total_starts <= 32
                        && first.listener_starts <= 8
                        && first.client_starts.iter().all(|count| *count <= 3)
                );
                assert!(
                    !fixture
                        .owner()
                        .grant_window_frame(generation)
                        .grant_accepted
                );
                let before = first.lifetime_starts;
                let end = Instant::now() + Duration::from_millis(5);
                while Instant::now() < end {
                    thread::yield_now();
                }
                assert_eq!(fixture.owner().window_observation().lifetime_starts, before);
                assert!(
                    fixture
                        .owner()
                        .grant_window_frame(generation + 1)
                        .grant_accepted
                );
                fixture.frame = generation + 1;
            }

            #[test]
            fn window_auth_bulk_tails_and_serial_requests() {
                for tcp in [true, false] {
                    let mut fixture = Fixture::new(tcp);
                    fixture.start();
                    let mut peer = fixture.raw();
                    let mut bytes = Vec::new();
                    if uses_tcp(&fixture.path) {
                        let (_, token) = read_endpoint(&fixture.path).unwrap();
                        bytes.extend_from_slice(format!("auth {token}\n").as_bytes());
                    }
                    bytes.extend_from_slice(b"status\r\nclose-status\n");
                    peer.write(&bytes);
                    if uses_tcp(&fixture.path) {
                        let mut auth = None;
                        fixture.until(|_| {
                            auth = peer.line();
                            auth.is_some()
                        });
                        assert_eq!(auth.as_deref(), Some("{\"ok\":true}\n"));
                    }
                    let first = fixture.request();
                    assert_eq!(first.command(), WindowCommand::Status);
                    assert_eq!(first.line(), "status");
                    for _ in 0..8 {
                        fixture.pump();
                        thread::sleep(Duration::from_millis(1));
                    }
                    assert!(fixture.owner().try_recv_window().is_none());
                    assert_eq!(fixture.owner().window_observation().pending_requests, 1);
                    first
                        .reply(WindowReply::Status {
                            phase: "Blocked",
                            stage: "Native\nIO",
                            attempt: 3,
                            detail: "quotes\" and \\ and \u{1}",
                            world_closed: false,
                            control_sealed: false,
                        })
                        .unwrap();
                    let mut line = None;
                    fixture.until(|_| {
                        line = peer.line();
                        line.is_some()
                    });
                    let reply: serde_json::Value =
                        serde_json::from_str(line.as_deref().unwrap()).unwrap();
                    assert_eq!(reply["stage"], "Native\nIO");
                    let second = fixture.request();
                    assert_eq!(second.command(), WindowCommand::CloseStatus);
                    second
                        .reply(WindowReply::Error { message: "fixed" })
                        .unwrap();
                    fixture.until(|_| peer.line().is_some());
                    assert!(fixture.closed().observation().endpoint_released);
                }
            }

            #[test]
            fn window_slots_reserve_before_accept_and_reuse_after_join() {
                let mut fixture = Fixture::new(true);
                fixture
                    .owner()
                    .group
                    .test
                    .accepted
                    .hold
                    .store(true, Ordering::Release);
                fixture.start();
                let peer = fixture.raw();
                fixture.until(|fixture| {
                    fixture
                        .owner()
                        .group
                        .test
                        .accepted
                        .entered
                        .load(Ordering::Acquire)
                });
                let view = fixture.owner().window_observation();
                assert_eq!(view.reserved_slots, 1);
                assert_eq!(view.live_slots, 0);
                assert_eq!(fixture.owner().observation().registered_clients, 0);
                fixture
                    .owner()
                    .group
                    .test
                    .accepted
                    .hold
                    .store(false, Ordering::Release);
                drop(peer);
                fixture.until(|fixture| {
                    fixture.owner().window_observation().live_slots == 0
                        && fixture.owner().observation().retired_client_sockets == 1
                });
                let mut peers = Vec::new();
                for _ in 0..SLOTS {
                    peers.push(fixture.client());
                }
                let ninth = fixture.raw();
                for _ in 0..10 {
                    fixture.pump();
                    thread::sleep(Duration::from_millis(1));
                }
                assert_eq!(fixture.owner().window_observation().live_slots, SLOTS);
                assert_eq!(fixture.owner().observation().registered_clients, SLOTS + 1);
                drop(ninth);
                drop(peers);
                fixture.until(|fixture| fixture.owner().window_observation().live_slots == 0);
                let _reused = fixture.client();
                assert!(fixture.owner().observation().registered_clients > SLOTS);
            }

            #[test]
            fn window_deadlines_expire_without_credit_and_do_not_renew() {
                let mut auth = Fixture::new(true);
                auth.start();
                let mut peer = auth.raw();
                peer.write(b"auth partial");
                auth.until(|fixture| fixture.owner().observation().registered_clients == 1);
                let policy = auth.policy();
                policy.time_offset.store(6000, Ordering::Release);
                peer.write(b"more");
                for _ in 0..4 {
                    auth.pump();
                    thread::sleep(Duration::from_millis(1));
                }
                policy.time_offset.store(10001, Ordering::Release);
                let deadline = Instant::now() + BACKSTOP;
                while auth.owner().observation().retired_client_sockets == 0 {
                    assert!(Instant::now() < deadline);
                    thread::yield_now();
                }
                assert_eq!(policy.observe(false).expired_operations, 1);

                let mut command = Fixture::new(true);
                command.start();
                let mut peer = command.client();
                peer.write(b"partial");
                command.until(|fixture| {
                    let input = lock(&fixture.owner().group.registry).clients[0]
                        .input
                        .clone();
                    input
                        .window
                        .as_ref()
                        .is_some_and(|client| lock(&client.io).used > 0)
                });
                let policy = command.policy();
                policy.time_offset.store(10001, Ordering::Release);
                let deadline = Instant::now() + BACKSTOP;
                while command.owner().observation().retired_client_sockets == 0 {
                    assert!(Instant::now() < deadline);
                    thread::yield_now();
                }
                assert_eq!(policy.observe(false).expired_operations, 1);

                let mut response = Fixture::new(true);
                response.start();
                let mut peer = response.client();
                peer.write(b"status\n");
                let request = response.request();
                let policy = response.policy();
                policy.time_offset.store(10001, Ordering::Release);
                let deadline = Instant::now() + BACKSTOP;
                while response.owner().observation().retired_client_sockets == 0 {
                    assert!(Instant::now() < deadline);
                    thread::yield_now();
                }
                assert_eq!(policy.observe(false).pending_requests, 1);
                drop(request);
                response
                    .until(|fixture| fixture.owner().window_observation().pending_requests == 0);

                // One real native read owns auth, a complete command and the
                // next partial line at t0. Delaying the prior operations must
                // not give that partial tail a fresh t9 response deadline.
                let mut bulk = Fixture::new(true);
                bulk.start();
                let policy = bulk.policy();
                policy.auth_bulk_read.hold.store(true, Ordering::Release);
                let mut peer = bulk.raw();
                let (_, token) = read_endpoint(&bulk.path).unwrap();
                let bytes = format!("auth {token}\nstatus\npartial-next");
                peer.write(bytes.as_bytes());
                bulk.until(|_| policy.auth_bulk_read.entered.load(Ordering::Acquire));
                let input = lock(&bulk.owner().group.registry).clients[0].input.clone();
                let client = input.window.as_ref().unwrap();
                let original_first_byte = {
                    let io = lock(&client.io);
                    assert_eq!(&io.buffer.as_ref().unwrap()[..io.used], bytes.as_bytes());
                    io.first_byte.unwrap()
                };
                policy.time_offset.store(9000, Ordering::Release);
                policy.auth_bulk_read.hold.store(false, Ordering::Release);
                let request = bulk.request();
                assert_eq!(request.command(), WindowCommand::Status);
                {
                    let io = lock(&client.io);
                    assert_eq!(&io.buffer.as_ref().unwrap()[..io.used], b"partial-next");
                    assert_eq!(io.first_byte, Some(original_first_byte));
                    assert!(
                        io.response_started.unwrap() > original_first_byte + Duration::from_secs(8)
                    );
                }
                assert_eq!(policy.observe(false).expired_operations, 0);
                policy.time_offset.store(10001, Ordering::Release);
                let deadline = Instant::now() + BACKSTOP;
                while !input.fd_retired.load(Ordering::Acquire) {
                    assert!(Instant::now() < deadline);
                    thread::yield_now();
                }
                assert_eq!(policy.observe(false).expired_operations, 1);
                assert_eq!(policy.observe(false).pending_requests, 1);
                assert!(bulk.owner().try_recv_window().is_none());
                assert_eq!(lock(&client.io).first_byte, Some(original_first_byte));
                request
                    .reply(WindowReply::Error {
                        message: "late original response after tail timeout",
                    })
                    .unwrap();
                bulk.until(|fixture| fixture.owner().window_observation().pending_requests == 0);
            }

            #[test]
            fn window_input_and_typed_output_limits() {
                let mut fixture = Fixture::new(true);
                fixture.start();
                let mut oversized = fixture.client();
                oversized.write(&vec![b'x'; LINE + 1]);
                fixture.until(|fixture| fixture.owner().observation().retired_client_sockets == 1);
                assert!(fixture.owner().try_recv_window().is_none());
                let mut peer = fixture.client();
                peer.write(b"status\n");
                let request = fixture.request();
                assert_eq!(
                    request.reply(WindowReply::Error {
                        message: &"x".repeat(OUTPUT)
                    }),
                    Err(WindowReplyRefusal::TooLarge)
                );
                let mut line = None;
                fixture.until(|_| {
                    line = peer.line();
                    line.is_some()
                });
                assert!(line.unwrap().contains("request abandoned or reply refused"));
                fixture.until(|fixture| {
                    fixture.owner().window_observation().reserved_response_bytes == 0
                });
                peer.write(b"close-retry 1 9\n");
                let request = fixture.request();
                assert_eq!(request.command(), WindowCommand::Unsupported);
                request
                    .reply(WindowReply::Error {
                        message: "unsupported fixed selector",
                    })
                    .unwrap();
                fixture.until(|_| peer.line().is_some());
            }

            #[test]
            fn window_late_reply_retains_slot_and_aggregate_reservation() {
                let mut fixture = Fixture::new(true);
                fixture.start();
                let mut peers = Vec::new();
                let mut requests = Vec::new();
                for _ in 0..SLOTS {
                    let mut peer = fixture.client();
                    peer.write(b"status\n");
                    requests.push(fixture.request());
                    peers.push(peer);
                }
                let policy = fixture.policy();
                policy.retirement.hold.store(true, Ordering::Release);
                policy.time_offset.store(10001, Ordering::Release);
                fixture
                    .until(|fixture| fixture.owner().observation().retired_client_sockets == SLOTS);
                fixture.until(|fixture| {
                    fixture.owner().poll_close();
                    lock(&fixture.owner().group.registry)
                        .clients
                        .iter()
                        .all(|client| client.state == WorkerState::Joined)
                });
                assert_eq!(policy.observe(false).live_slots, SLOTS);
                assert_eq!(policy.observe(false).pending_requests, SLOTS);
                let body = "x".repeat(OUTPUT - 13);
                for request in requests {
                    request
                        .reply(WindowReply::Error { message: &body })
                        .unwrap();
                }
                assert_eq!(policy.observe(false).reserved_response_bytes, TOTAL_OUTPUT);
                assert_eq!(policy.observe(false).live_slots, SLOTS);
                policy.retirement.hold.store(false, Ordering::Release);
                fixture.until(|fixture| {
                    fixture.owner().window_observation().pending_requests == 0
                        && fixture.owner().window_observation().live_slots == 0
                });
                assert_eq!(policy.observe(false).canceled_reply_bytes, TOTAL_OUTPUT);
                assert_eq!(policy.observe(false).reserved_response_bytes, 0);
                let _reused = fixture.client();
                drop(peers);
            }

            #[test]
            fn window_stop_without_grant_and_panic_custody() {
                let mut fixture = Fixture::new(true);
                fixture.start();
                let _peer = fixture.client();
                let deadline = Instant::now() + BACKSTOP;
                while !fixture.owner().window_observation().deadline_expired {
                    assert!(Instant::now() < deadline);
                    thread::yield_now();
                }
                let before = fixture.owner().window_observation().lifetime_starts;
                let closed = fixture.closed();
                assert!(closed.observation().native_quiescent);
                assert!(closed.observation().endpoint_released);
                assert_eq!(
                    closed
                        .group
                        .window
                        .as_ref()
                        .unwrap()
                        .observe(false)
                        .lifetime_starts,
                    before
                );

                #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
                {
                    let mut held = Fixture::new(true);
                    let receipt = held.owner().arm_window_cleanup_hold().unwrap();
                    held.start();
                    let _peer = held.client();
                    let original = lock(&held.owner().group.registry)
                        .cleanup
                        .as_ref()
                        .unwrap()
                        .thread()
                        .id();
                    held.owner().begin_close(Instant::now());
                    held.until(|_| receipt.entered());
                    assert_eq!(
                        held.owner().poll_close().observation.phase,
                        RetainedControlPhase::Blocked
                    );
                    assert_eq!(
                        lock(&held.owner().group.registry)
                            .cleanup
                            .as_ref()
                            .unwrap()
                            .thread()
                            .id(),
                        original
                    );
                    held.owner().retry_close(Instant::now() + BACKSTOP).unwrap();
                    assert_eq!(held.owner().observation().close_attempts, 2);
                    receipt.release();
                    assert!(held.closed().observation().endpoint_released);
                }

                let mut failed = Fixture::new(true);
                failed.start();
                let mut peer = failed.client();
                failed
                    .owner()
                    .group
                    .test
                    .panic_client
                    .store(true, Ordering::Release);
                peer.write(b"status\n");
                let request = failed.request();
                failed.until(|fixture| {
                    fixture.owner().poll_close();
                    lock(&fixture.owner().group.registry).clients[0].state == WorkerState::Joined
                });
                assert_eq!(failed.owner().window_observation().live_slots, 1);
                request
                    .reply(WindowReply::Error {
                        message: "late after original actor panic",
                    })
                    .unwrap();
                let closed = failed.closed();
                assert!(
                    closed
                        .observation()
                        .failures
                        .iter()
                        .any(|failure| failure.kind == RetainedFailureKind::Panicked)
                );
                let registry = lock(&closed.group.registry);
                assert_eq!(registry.replies.len(), 1);
                assert!(
                    lock(&registry.replies[0].state)
                        .text
                        .as_deref()
                        .unwrap()
                        .contains("late after original actor panic")
                );
            }

            #[test]
            #[cfg(unix)]
            fn window_actual_nonreading_peer_preserves_output_bound() {
                let mut fixture = Fixture::new(false);
                fixture.start();
                let mut peer = fixture.client();
                peer.write(b"status\n");
                let request = fixture.request();
                request
                    .reply(WindowReply::Error {
                        message: &"x".repeat(OUTPUT - 13),
                    })
                    .unwrap();
                fixture.until(|fixture| {
                    let input = lock(&fixture.owner().group.registry).clients[0]
                        .input
                        .clone();
                    input.bytes_written.load(Ordering::Acquire) > 0
                        && fixture.owner().window_observation().write_would_block > 0
                });
                let mut progress = [0; 4096];
                let count = peer.stream.read(&mut progress).unwrap();
                assert!(count > 0);
                for _ in 0..5 {
                    fixture.pump();
                    thread::sleep(Duration::from_millis(1));
                }
                {
                    let input = lock(&fixture.owner().group.registry).clients[0]
                        .input
                        .clone();
                    assert!(
                        !lock(&fixture.owner().group.registry).clients[0]
                            .handle
                            .as_ref()
                            .unwrap()
                            .is_finished()
                    );
                    assert!(input.bytes_written.load(Ordering::Acquire) < OUTPUT);
                }
                assert_eq!(
                    fixture.owner().window_observation().reserved_response_bytes,
                    OUTPUT
                );
                let closed = fixture.closed();
                assert!(closed.observation().native_quiescent);
                assert_eq!(
                    closed
                        .group
                        .window
                        .as_ref()
                        .unwrap()
                        .observe(false)
                        .reserved_response_bytes,
                    0
                );
            }
        }
    }

    #[cfg(test)]
    #[derive(Default)]
    struct TestControl {
        refuse_listener: AtomicBool,
        refuse_client: AtomicBool,
        refuse_cleanup: AtomicBool,
        panic_listener_after_client: AtomicBool,
        panic_client: AtomicBool,
        fail_cleanup: AtomicBool,
        acquisition: TestGate,
        acquired: TestGate,
        published: TestGate,
        accepted: TestGate,
        client: TestGate,
        cleanup: TestGate,
    }

    #[cfg(test)]
    #[derive(Default)]
    struct TestGate {
        hold: AtomicBool,
        entered: AtomicBool,
    }

    #[cfg(test)]
    enum TestPoint {
        Acquisition,
        Acquired,
        Published,
        Accepted,
        Client,
        Cleanup,
    }

    #[cfg(test)]
    fn test_gate(group: &Weak<Group>, point: TestPoint) {
        loop {
            let Some(owner) = group.upgrade() else { return };
            let gate = match point {
                TestPoint::Acquisition => &owner.test.acquisition,
                TestPoint::Acquired => &owner.test.acquired,
                TestPoint::Published => &owner.test.published,
                TestPoint::Accepted => &owner.test.accepted,
                TestPoint::Client => &owner.test.client,
                TestPoint::Cleanup => &owner.test.cleanup,
            };
            gate.entered.store(true, Ordering::Release);
            if !gate.hold.load(Ordering::Acquire) {
                return;
            }
            drop(owner);
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        static NEXT_PATH: AtomicUsize = AtomicUsize::new(0);
        const BACKSTOP: Duration = Duration::from_secs(10);

        struct Fixture {
            path: PathBuf,
            owner: Option<RetainedControl>,
        }

        fn wait_until(mut predicate: impl FnMut() -> bool) {
            let deadline = Instant::now() + BACKSTOP;
            while !predicate() {
                assert!(Instant::now() < deadline, "native control fixture backstop");
                thread::yield_now();
            }
        }

        fn release_gates(group: &Group) {
            for gate in [
                &group.test.acquisition,
                &group.test.acquired,
                &group.test.published,
                &group.test.accepted,
                &group.test.client,
                &group.test.cleanup,
            ] {
                gate.hold.store(false, Ordering::Release);
            }
        }

        fn finish(owner: &mut RetainedControl) {
            release_gates(&owner.group);
            owner.begin_close(Instant::now() + BACKSTOP);
            wait_until(|| owner.poll_close().observation.phase == RetainedControlPhase::Quiescent);
        }

        impl Fixture {
            fn new(tcp: bool) -> Self {
                let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "horizon-retained-{}-{sequence}.{}",
                    std::process::id(),
                    if tcp { "json" } else { "sock" }
                ));
                assert!(
                    !path.exists(),
                    "isolated native fixture path already exists"
                );
                Self {
                    owner: Some(RetainedControl::prepare(&path)),
                    path,
                }
            }

            fn owner(&mut self) -> &mut RetainedControl {
                self.owner.as_mut().unwrap()
            }

            fn start(&mut self) {
                self.owner().start().unwrap();
                wait_until(|| {
                    self.owner().observation().endpoint == RetainedEndpointState::Published
                });
            }

            fn client(&self) -> Stream {
                if uses_tcp(&self.path) {
                    let (port, token) = read_endpoint(&self.path).unwrap();
                    let socket = TcpStream::connect_timeout(
                        &std::net::SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
                        BACKSTOP,
                    )
                    .unwrap();
                    socket.set_read_timeout(Some(BACKSTOP)).unwrap();
                    socket.set_write_timeout(Some(BACKSTOP)).unwrap();
                    let mut stream = Stream::Tcp(socket);
                    writeln!(stream, "auth {token}").unwrap();
                    assert_eq!(read_line(&stream), "{\"ok\":true}");
                    stream
                } else {
                    #[cfg(unix)]
                    {
                        let socket = UnixStream::connect(&self.path).unwrap();
                        socket.set_read_timeout(Some(BACKSTOP)).unwrap();
                        socket.set_write_timeout(Some(BACKSTOP)).unwrap();
                        Stream::Unix(socket)
                    }
                    #[cfg(not(unix))]
                    unreachable!("non-Unix endpoints always use TCP")
                }
            }

            fn raw_tcp(&self) -> TcpStream {
                let (port, _) = read_endpoint(&self.path).unwrap();
                let socket = TcpStream::connect_timeout(
                    &std::net::SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
                    BACKSTOP,
                )
                .unwrap();
                socket.set_read_timeout(Some(BACKSTOP)).unwrap();
                socket.set_write_timeout(Some(BACKSTOP)).unwrap();
                socket
            }

            fn request(&mut self) -> Request {
                let mut result = None;
                wait_until(|| {
                    result = self.owner().try_recv();
                    result.is_some()
                });
                result.unwrap()
            }

            fn closed(mut self) -> ClosedControl {
                let mut owner = self.owner.take().unwrap();
                finish(&mut owner);
                match owner.try_closed() {
                    Ok(closed) => closed,
                    Err(_) => panic!("checked native close refused its completed owner"),
                }
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                if let Some(mut owner) = self.owner.take() {
                    finish(&mut owner);
                    assert!(owner.try_closed().is_ok());
                }
            }
        }

        fn read_line(stream: &Stream) -> String {
            let mut reply = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut reply)
                .unwrap();
            reply.trim_end().to_owned()
        }

        fn round_trip(fixture: &mut Fixture, peer: &mut Stream, line: &str) {
            writeln!(peer, "{line}").unwrap();
            let request = fixture.request();
            assert_eq!(request.line, line);
            request.reply.send("{\"ok\":true}".into()).unwrap();
            assert_eq!(read_line(peer), "{\"ok\":true}");
        }

        fn raw_line_round_trip(
            fixture: &mut Fixture,
            bytes: &[u8],
            expected: &str,
            half_close: bool,
        ) {
            let mut peer = fixture.client();
            peer.write_all(bytes).unwrap();
            if half_close {
                match &peer {
                    #[cfg(unix)]
                    Stream::Unix(socket) => socket.shutdown(Shutdown::Write).unwrap(),
                    Stream::Tcp(socket) => socket.shutdown(Shutdown::Write).unwrap(),
                }
            }
            let request = fixture.request();
            assert_eq!(request.line, expected);
            request.reply.send("{\"ok\":true}".into()).unwrap();
            assert_eq!(read_line(&peer), "{\"ok\":true}");
        }

        #[test]
        fn retained_prepare_local_and_owned_fence() {
            let mut owner = RetainedControl::prepare_local();
            let before = owner.observation();
            assert_eq!(before.phase, RetainedControlPhase::Prepared);
            assert_eq!(before.outstanding_handles, 0);
            assert!(!before.native_quiescent);
            owner.start().unwrap();
            let sender = owner.sender();
            let clone = sender.clone();
            let (reply, response) = mpsc::channel();
            assert!(
                sender
                    .try_send(Request {
                        line: "first".into(),
                        reply: reply.clone()
                    })
                    .is_ok()
            );
            assert!(
                clone
                    .try_send(Request {
                        line: "queued-original".into(),
                        reply: reply.clone()
                    })
                    .is_ok()
            );
            assert_eq!(owner.try_recv().unwrap().line, "first");
            owner.begin_close(Instant::now() + BACKSTOP);
            let refused = match clone.try_send(Request {
                line: "refused-original".into(),
                reply,
            }) {
                Err(request) => request,
                Ok(()) => panic!("cloned sender crossed permanent admission fence"),
            };
            assert_eq!(refused.line, "refused-original");
            refused.reply.send("original-channel".into()).unwrap();
            assert_eq!(response.recv_timeout(BACKSTOP).unwrap(), "original-channel");
            assert!(owner.try_recv().is_none());
            finish(&mut owner);
            let closed = match owner.try_closed() {
                Ok(value) => value,
                Err(_) => panic!("local close"),
            };
            let observation = closed.observation();
            assert!(observation.native_quiescent && observation.endpoint_released);
            assert_eq!(observation.queued_requests, 1);
            assert_eq!(observation.retained_reply_receivers, 0);
            assert_eq!(
                lock(&closed.group.registry).queue.front().unwrap().line,
                "queued-original"
            );
        }

        #[test]
        #[cfg(unix)]
        fn retained_unix_round_trip_and_checked_join() {
            let mut fixture = Fixture::new(false);
            assert!(!fixture.path.exists());
            assert_eq!(fixture.owner().observation().outstanding_handles, 0);
            fixture.start();
            let mut peer = fixture.client();
            round_trip(&mut fixture, &mut peer, "status");
            drop(peer);
            raw_line_round_trip(&mut fixture, b"newline-cr\r\n", "newline-cr", false);
            raw_line_round_trip(&mut fixture, b"eof-tail", "eof-tail", true);
            raw_line_round_trip(&mut fixture, b"eof-tail-cr\r", "eof-tail-cr\r", true);
            let path = fixture.path.clone();
            let closed = fixture.closed();
            assert!(closed.observation().native_quiescent);
            assert!(closed.observation().endpoint_released);
            assert_eq!(closed.observation().delivered_replies, 4);
            assert_eq!(closed.observation().retained_reply_receivers, 0);
            assert!(!path.exists());
        }

        #[test]
        fn retained_tcp_auth_refusal_preserves_listener() {
            let mut fixture = Fixture::new(true);
            fixture.start();
            let mut wrong = fixture.raw_tcp();
            writeln!(wrong, "auth wrong").unwrap();
            assert!(read_line(&Stream::Tcp(wrong)).contains("bad or missing token"));
            let mut first = fixture.client();
            round_trip(&mut fixture, &mut first, "first-valid-after-refusal");
            drop(first);
            wait_until(|| fixture.owner().observation().retired_client_sockets >= 2);
            let mut second = fixture.client();
            round_trip(&mut fixture, &mut second, "second-valid-after-eof");
            raw_line_round_trip(&mut fixture, b"newline-cr\r\n", "newline-cr", false);
            raw_line_round_trip(&mut fixture, b"eof-tail", "eof-tail", true);
            raw_line_round_trip(&mut fixture, b"eof-tail-cr\r", "eof-tail-cr\r", true);
            assert!(!fixture.owner().observation().admission_sealed);
            let sender = fixture.owner().sender();
            let closed = fixture.closed();
            assert_eq!(closed.observation().delivered_replies, 5);
            assert!(closed.observation().failures.is_empty());
            assert_eq!(closed.observation().retired_client_sockets, 6);
            drop(closed);
            assert!(
                lock(&sender.group.registry)
                    .clients
                    .iter()
                    .all(|client| client.stop.is_none()
                        && client.input.fd_retired.load(Ordering::Acquire))
            );
        }

        #[test]
        fn retained_partial_auth_and_command_shutdown() {
            let mut fixture = Fixture::new(true);
            fixture.start();
            let mut partial_auth = fixture.raw_tcp();
            partial_auth.write_all(b"auth partial").unwrap();
            let mut partial_command = fixture.client();
            partial_command.write_all(b"unfinished command").unwrap();
            wait_until(|| fixture.owner().observation().registered_clients == 2);
            let inputs: Vec<_> = lock(&fixture.owner().group.registry)
                .clients
                .iter()
                .map(|client| client.input.clone())
                .collect();
            wait_until(|| {
                inputs
                    .iter()
                    .any(|input| input.auth_read.load(Ordering::Acquire) == b"auth partial".len())
                    && inputs.iter().any(|input| {
                        input.command_read.load(Ordering::Acquire) == b"unfinished command".len()
                    })
            });
            let closed = fixture.closed();
            assert!(closed.observation().native_quiescent);
            assert!(
                inputs
                    .iter()
                    .any(|input| lock(&input.io).auth == b"auth partial")
            );
            assert!(
                inputs
                    .iter()
                    .any(|input| lock(&input.io).command == b"unfinished command")
            );
            assert_eq!(closed.observation().queued_requests, 0);
        }

        #[test]
        fn retained_native_write_backpressure() {
            let mut fixture = Fixture::new(true);
            fixture.start();
            let mut peer = fixture.client();
            writeln!(peer, "pressure").unwrap();
            let request = fixture.request();
            let input = lock(&fixture.owner().group.registry).clients[0]
                .input
                .clone();
            let original_thread = lock(&fixture.owner().group.registry).clients[0]
                .handle
                .as_ref()
                .unwrap()
                .thread()
                .id();
            const PAYLOAD: usize = 64 * 1024 * 1024;
            request.reply.send("x".repeat(PAYLOAD)).unwrap();
            let mut progress = [0u8; 8192];
            peer.read_exact(&mut progress).unwrap();
            assert!(progress.iter().all(|byte| *byte == b'x'));
            assert_eq!(input.stage.load(Ordering::Acquire), 4);
            {
                let registry = lock(&fixture.owner().group.registry);
                let handle = registry.clients[0].handle.as_ref().unwrap();
                assert_eq!(handle.thread().id(), original_thread);
                assert!(
                    !handle.is_finished(),
                    "peer progress must coexist with unfinished original writer"
                );
            }
            let closed = fixture.closed();
            assert!(closed.observation().native_quiescent);
            assert_eq!(closed.observation().delivered_replies, 0);
            let state = lock(&input.io);
            let pending = state.pending.as_ref().unwrap();
            assert_eq!(
                lock(&pending.state).text.as_ref().unwrap().len(),
                PAYLOAD + 1
            );
            assert!(!pending.delivered.load(Ordering::Acquire));
        }

        #[test]
        fn retained_queued_active_and_late_reply_custody() {
            let mut fixture = Fixture::new(true);
            fixture.start();
            let mut peers = Vec::new();
            for (index, line) in ["active-before", "active-after", "queued-original"]
                .iter()
                .enumerate()
            {
                let mut peer = fixture.client();
                writeln!(peer, "{line}").unwrap();
                peers.push(peer);
                wait_until(|| fixture.owner().observation().queued_requests == index + 1);
            }
            let before = fixture.request();
            let after = fixture.request();
            fixture
                .owner()
                .group
                .test
                .cleanup
                .hold
                .store(true, Ordering::Release);
            fixture.owner().begin_close(Instant::now() + BACKSTOP);
            wait_until(|| {
                fixture.owner().poll_close();
                fixture
                    .owner()
                    .group
                    .test
                    .cleanup
                    .entered
                    .load(Ordering::Acquire)
            });
            before.reply.send("late-before-cleanup".into()).unwrap();
            fixture
                .owner()
                .group
                .test
                .cleanup
                .hold
                .store(false, Ordering::Release);
            let closed = fixture.closed();
            let observation = closed.observation();
            assert_eq!(observation.queued_requests, 1);
            assert_eq!(observation.handed_off_requests, 2);
            assert_eq!(observation.retained_reply_receivers, 3);
            assert_eq!(observation.observed_replies, 1);
            assert_eq!(observation.unobserved_replies, 2);
            after.reply.send("late-after-closed".into()).unwrap();
            let registry = lock(&closed.group.registry);
            assert_eq!(registry.queue.front().unwrap().line, "queued-original");
            assert_eq!(
                lock(&registry.replies[0].state).text.as_deref(),
                Some("late-before-cleanup")
            );
            assert_eq!(
                lock(&registry.replies[1].state)
                    .receiver
                    .try_recv()
                    .unwrap(),
                "late-after-closed"
            );
            drop(registry);
            assert_eq!(closed.observation().delivered_replies, 0);
            drop(peers);
        }

        #[test]
        fn retained_listener_spawn_refusal_preserves_input() {
            let mut fixture = Fixture::new(true);
            fixture
                .owner()
                .group
                .test
                .refuse_listener
                .store(true, Ordering::Release);
            assert!(fixture.owner().start().is_err());
            assert!(!fixture.path.exists());
            let expected_path = fixture.path.clone();
            assert_eq!(
                lock(&fixture.owner().group.acquisition).path.as_ref(),
                Some(&expected_path)
            );
            let closed = fixture.closed();
            assert!(
                closed
                    .observation()
                    .failures
                    .iter()
                    .any(|failure| failure.kind == RetainedFailureKind::SpawnRefused)
            );
            assert!(closed.observation().endpoint_released);
        }

        #[test]
        fn retained_client_spawn_refusal_preserves_socket() {
            let mut fixture = Fixture::new(true);
            fixture
                .owner()
                .group
                .test
                .refuse_client
                .store(true, Ordering::Release);
            fixture.start();
            let _peer = fixture.raw_tcp();
            wait_until(|| fixture.owner().observation().refused_clients == 1);
            let input = lock(&fixture.owner().group.registry).clients[0]
                .input
                .clone();
            wait_until(|| input.fd_retired.load(Ordering::Acquire));
            assert!(input.fd_retired.load(Ordering::Acquire));
            assert!(lock(&input.io).socket.is_none());
            assert_eq!(input.stage.load(Ordering::Acquire), 0);
            let closed = fixture.closed();
            assert_eq!(closed.observation().registered_clients, 1);
            assert!(
                closed
                    .observation()
                    .failures
                    .iter()
                    .any(|failure| failure.stage == RetainedWorkerStage::CommandRead
                        && failure.kind == RetainedFailureKind::SpawnRefused)
            );
        }

        #[test]
        fn retained_listener_panic_keeps_independent_clients() {
            let mut fixture = Fixture::new(true);
            fixture.start();
            let mut first = fixture.client();
            let mut second = fixture.client();
            fixture
                .owner()
                .group
                .test
                .panic_listener_after_client
                .store(true, Ordering::Release);
            let _third = fixture.raw_tcp();
            wait_until(|| {
                lock(&fixture.owner().group.registry)
                    .listener
                    .as_ref()
                    .unwrap()
                    .is_finished()
            });
            round_trip(&mut fixture, &mut first, "first-after-parent-panic");
            round_trip(&mut fixture, &mut second, "second-after-parent-panic");
            let closed = fixture.closed();
            assert_eq!(closed.observation().registered_clients, 3);
            assert_eq!(closed.observation().delivered_replies, 2);
            assert!(
                closed
                    .observation()
                    .failures
                    .iter()
                    .any(|failure| failure.stage == RetainedWorkerStage::Listener
                        && failure.kind == RetainedFailureKind::Panicked)
            );
        }

        #[test]
        fn retained_client_panic_preserves_receiver_and_sibling() {
            let mut fixture = Fixture::new(true);
            fixture.start();
            let mut sibling = fixture.client();
            round_trip(&mut fixture, &mut sibling, "before-client-panic");
            fixture
                .owner()
                .group
                .test
                .panic_client
                .store(true, Ordering::Release);
            let mut doomed = fixture.client();
            writeln!(doomed, "original-panic-request").unwrap();
            let request = fixture.request();
            wait_until(|| {
                lock(&fixture.owner().group.registry).clients[1]
                    .handle
                    .as_ref()
                    .unwrap()
                    .is_finished()
            });
            fixture
                .owner()
                .group
                .test
                .panic_client
                .store(false, Ordering::Release);
            round_trip(&mut fixture, &mut sibling, "sibling-after-client-panic");
            request
                .reply
                .send("late-after-client-panic".into())
                .unwrap();
            let closed = fixture.closed();
            assert_eq!(closed.observation().retained_reply_receivers, 1);
            assert_eq!(closed.observation().observed_replies, 3);
            assert_eq!(
                lock(&lock(&closed.group.registry).replies[0].state)
                    .text
                    .as_deref(),
                Some("late-after-client-panic")
            );
            assert!(
                closed
                    .observation()
                    .failures
                    .iter()
                    .any(|failure| failure.kind == RetainedFailureKind::Panicked)
            );
        }

        #[test]
        fn retained_acquisition_and_registration_close_races() {
            let mut acquisition = Fixture::new(true);
            acquisition
                .owner()
                .group
                .test
                .acquisition
                .hold
                .store(true, Ordering::Release);
            acquisition.owner().start().unwrap();
            wait_until(|| {
                acquisition
                    .owner()
                    .group
                    .test
                    .acquisition
                    .entered
                    .load(Ordering::Acquire)
            });
            acquisition.owner().begin_close(Instant::now());
            assert_eq!(
                acquisition.owner().poll_close().observation.phase,
                RetainedControlPhase::Blocked
            );
            assert!(!acquisition.path.exists());
            let closed = acquisition.closed();
            assert_eq!(
                closed.observation().endpoint,
                RetainedEndpointState::Unpublished
            );

            for tcp in [true, false] {
                let mut acquired = Fixture::new(tcp);
                acquired
                    .owner()
                    .group
                    .test
                    .acquired
                    .hold
                    .store(true, Ordering::Release);
                acquired.owner().start().unwrap();
                wait_until(|| {
                    acquired
                        .owner()
                        .group
                        .test
                        .acquired
                        .entered
                        .load(Ordering::Acquire)
                });
                assert!(acquired.path.exists());
                let before_publication = acquired.owner().observation();
                assert_eq!(
                    before_publication.endpoint,
                    RetainedEndpointState::Unpublished
                );
                assert!(!before_publication.endpoint_released);
                acquired.owner().begin_close(Instant::now());
                let blocked = acquired.owner().poll_close().observation;
                assert_eq!(blocked.phase, RetainedControlPhase::Blocked);
                assert_eq!(blocked.endpoint, RetainedEndpointState::Unpublished);
                assert!(!blocked.endpoint_released);
                assert!(acquired.path.exists());
                let path = acquired.path.clone();
                let closed = acquired.closed();
                assert!(closed.observation().native_quiescent);
                assert!(closed.observation().endpoint_released);
                assert_eq!(
                    closed.observation().endpoint,
                    RetainedEndpointState::Released
                );
                assert!(!path.exists());
            }

            let mut registration = Fixture::new(true);
            registration
                .owner()
                .group
                .test
                .accepted
                .hold
                .store(true, Ordering::Release);
            registration.start();
            let _peer = registration.raw_tcp();
            wait_until(|| {
                registration
                    .owner()
                    .group
                    .test
                    .accepted
                    .entered
                    .load(Ordering::Acquire)
            });
            assert_eq!(registration.owner().observation().registered_clients, 0);
            registration.owner().begin_close(Instant::now());
            assert_eq!(
                registration.owner().poll_close().observation.phase,
                RetainedControlPhase::Blocked
            );
            let closed = registration.closed();
            assert_eq!(closed.observation().registered_clients, 1);
            assert_eq!(closed.observation().refused_clients, 1);

            let mut latched = Fixture::new(true);
            latched
                .owner()
                .group
                .test
                .client
                .hold
                .store(true, Ordering::Release);
            latched.start();
            let _peer = latched.raw_tcp();
            wait_until(|| {
                latched
                    .owner()
                    .group
                    .test
                    .client
                    .entered
                    .load(Ordering::Acquire)
            });
            assert_eq!(latched.owner().observation().outstanding_handles, 3);
            assert_eq!(latched.closed().observation().registered_clients, 1);
        }

        #[test]
        fn retained_finished_handles_deadline_and_retry() {
            let mut fixture = Fixture::new(true);
            fixture
                .owner()
                .group
                .test
                .client
                .hold
                .store(true, Ordering::Release);
            fixture.start();
            let _peer = fixture.raw_tcp();
            wait_until(|| {
                fixture
                    .owner()
                    .group
                    .test
                    .client
                    .entered
                    .load(Ordering::Acquire)
            });
            fixture.owner().begin_close(Instant::now());
            fixture.owner().begin_close(Instant::now() + BACKSTOP);
            let blocked = fixture.owner().poll_close().observation;
            assert_eq!(blocked.phase, RetainedControlPhase::Blocked);
            assert_eq!(blocked.close_attempts, 1);
            assert!(blocked.deadline_expired);
            let owner = fixture.owner.take().unwrap();
            fixture.owner = Some(match owner.try_closed() {
                Err(owner) => owner,
                Ok(_) => panic!("unfinished original handle released"),
            });
            fixture
                .owner()
                .retry_close(Instant::now() + BACKSTOP)
                .unwrap();
            assert_eq!(fixture.owner().observation().close_attempts, 2);
            fixture
                .owner()
                .group
                .test
                .client
                .hold
                .store(false, Ordering::Release);
            wait_until(|| {
                let registry = lock(&fixture.owner().group.registry);
                registry
                    .listener
                    .as_ref()
                    .is_none_or(thread::JoinHandle::is_finished)
                    && registry.clients[0].handle.as_ref().unwrap().is_finished()
            });
            assert!(!fixture.owner().observation().native_quiescent);
            let owner = fixture.owner.take().unwrap();
            fixture.owner = Some(match owner.try_closed() {
                Err(owner) => owner,
                Ok(_) => panic!("finished but unjoined handle released"),
            });
            let closed = fixture.closed();
            assert_eq!(closed.observation().close_attempts, 2);
            assert!(closed.observation().native_quiescent);
            assert!(
                closed
                    .observation()
                    .failures
                    .iter()
                    .any(|failure| failure.kind == RetainedFailureKind::Deadline)
            );
        }

        #[test]
        fn retained_existing_endpoint_is_never_probed() {
            let mut original = Fixture::new(true);
            original.start();
            let bytes = std::fs::read(&original.path).unwrap();
            let mut refused = RetainedControl::prepare(&original.path);
            refused.start().unwrap();
            wait_until(|| refused.observation().endpoint == RetainedEndpointState::Refused);
            assert_eq!(original.owner().observation().registered_clients, 0);
            finish(&mut refused);
            let closed = match refused.try_closed() {
                Ok(value) => value,
                Err(_) => panic!("refused close"),
            };
            assert_eq!(
                closed.observation().endpoint,
                RetainedEndpointState::Refused
            );
            assert!(!closed.observation().endpoint_released);
            assert_eq!(std::fs::read(&original.path).unwrap(), bytes);
            let mut peer = original.client();
            round_trip(&mut original, &mut peer, "original-still-owned");
            assert!(original.closed().observation().endpoint_released);
        }

        #[test]
        fn retained_replaced_endpoint_is_preserved() {
            let mut fixture = Fixture::new(true);
            fixture.start();
            let path = fixture.path.clone();
            let moved = path.with_extension("original");
            std::fs::rename(&path, &moved).unwrap();
            std::fs::write(&path, b"replacement-owner").unwrap();
            let closed = fixture.closed();
            assert!(closed.observation().native_quiescent);
            assert_eq!(
                closed.observation().endpoint,
                RetainedEndpointState::Replaced
            );
            assert!(!closed.observation().endpoint_released);
            assert_eq!(std::fs::read(&path).unwrap(), b"replacement-owner");
            std::fs::remove_file(path).unwrap();
            std::fs::remove_file(moved).unwrap();
        }

        #[test]
        fn retained_cleanup_failure_is_not_release() {
            #[cfg(unix)]
            struct RestorePermissions(PathBuf, std::fs::Permissions);
            #[cfg(unix)]
            impl Drop for RestorePermissions {
                fn drop(&mut self) {
                    let _ = std::fs::set_permissions(&self.0, self.1.clone());
                }
            }
            #[cfg(unix)]
            let parent = std::env::temp_dir().join(format!(
                "horizon-retained-permissions-{}-{}",
                std::process::id(),
                NEXT_PATH.fetch_add(1, Ordering::Relaxed)
            ));
            #[cfg(unix)]
            let mut fixture = {
                std::fs::create_dir(&parent).unwrap();
                let path = parent.join("endpoint.json");
                Fixture {
                    owner: Some(RetainedControl::prepare(&path)),
                    path,
                }
            };
            #[cfg(not(unix))]
            let mut fixture = Fixture::new(true);
            fixture.start();
            #[cfg(unix)]
            let restore = {
                use std::os::unix::fs::PermissionsExt;
                let restore = RestorePermissions(
                    parent.clone(),
                    std::fs::metadata(&parent).unwrap().permissions(),
                );
                std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o500)).unwrap();
                restore
            };
            #[cfg(not(unix))]
            fixture
                .owner()
                .group
                .test
                .fail_cleanup
                .store(true, Ordering::Release);
            let path = fixture.path.clone();
            let bytes = std::fs::read(&path).unwrap();
            let closed = fixture.closed();
            assert!(closed.observation().native_quiescent);
            assert_eq!(closed.observation().endpoint, RetainedEndpointState::Failed);
            assert!(!closed.observation().endpoint_released);
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert!(
                closed
                    .observation()
                    .failures
                    .iter()
                    .any(|failure| failure.stage == RetainedWorkerStage::Cleanup)
            );
            #[cfg(unix)]
            {
                assert!(
                    closed
                        .observation()
                        .failures
                        .iter()
                        .any(|failure| failure.kind
                            == RetainedFailureKind::NativeIo(io::ErrorKind::PermissionDenied))
                );
                let original = lock(&closed.group.acquisition);
                assert!(original.owns_path && original.identity.is_some());
                drop(original);
                drop(restore);
                assert_eq!(std::fs::read(&path).unwrap(), bytes);
            }
            std::fs::remove_file(path).unwrap();
            #[cfg(unix)]
            std::fs::remove_dir(parent).unwrap();

            let mut refused = Fixture::new(true);
            refused
                .owner()
                .group
                .test
                .refuse_cleanup
                .store(true, Ordering::Release);
            assert!(refused.owner().start().is_err());
            let path = refused.path.clone();
            let closed = refused.closed();
            assert!(closed.observation().native_quiescent);
            assert!(closed.observation().endpoint_released);
            assert!(!path.exists());
            assert!(
                closed
                    .observation()
                    .failures
                    .iter()
                    .any(|failure| failure.stage == RetainedWorkerStage::Cleanup
                        && failure.kind == RetainedFailureKind::SpawnRefused)
            );
        }

        #[test]
        #[ignore = "separate process: unresolved retained ownership deliberately aborts"]
        fn retained_unresolved_owner_fatal() {
            if std::env::var("HORIZON_RETAINED_CONTROL_FATAL").as_deref() != Ok("1") {
                return;
            }
            eprintln!("retained-control-fatal-entered");
            drop(RetainedControl::prepare_local());
            panic!("unresolved retained owner unexpectedly returned");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_endpoint_round_trip() {
        let path =
            std::env::temp_dir().join(format!("horizon-ctl-test-{}.json", std::process::id()));
        let server = Server::start(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"tcp\""));
        let mut client = connect(&path).unwrap();
        writeln!(client, "status").unwrap();
        let request = loop {
            if let Some(r) = server.try_recv() {
                break r;
            }
            thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(request.line, "status");
        request.reply.send(r#"{"ok":true}"#.into()).unwrap();
        let mut reply = String::new();
        BufReader::new(client).read_line(&mut reply).unwrap();
        assert_eq!(reply.trim(), r#"{"ok":true}"#);
        drop(server);
        assert!(!path.exists());
    }

    #[test]
    fn tcp_refuses_a_wrong_token() {
        let path =
            std::env::temp_dir().join(format!("horizon-ctl-bad-{}.json", std::process::id()));
        let _server = Server::start(&path).unwrap();
        let (port, _) = read_endpoint(&path).unwrap();
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        writeln!(stream, "auth nope").unwrap();
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).unwrap();
        assert!(reply.contains("error"));
    }

    #[test]
    fn tokens_differ() {
        assert_ne!(random_token(), random_token());
        assert_eq!(random_token().len(), 32);
        assert!(same_token("abc", "abc") && !same_token("abc", "abd") && !same_token("a", "ab"));
    }
}

/// Frame durations since the last reset, summarized like the Java recorder,
/// with the CPU part of each frame and GPU times from timestamp queries.
#[derive(Default)]
pub struct FrameStats {
    millis: Vec<f32>,
    cpu: Vec<f32>,
    gpu: Vec<f32>,
    /// Milliseconds per frame phase (`PHASES`), one entry per rendered frame.
    phases: Vec<[f32; PHASES.len()]>,
}

/// The parts of a frame's CPU time: world streaming and uploads, game ticks,
/// building the frame's geometry and HUD, and the renderer's own work.
pub const PHASES: [&str; 4] = ["update", "tick", "prepare", "render"];

fn percentiles(values: &[f32]) -> serde_json::Value {
    let mut sorted = values.to_vec();
    sorted.sort_by(f32::total_cmp);
    let n = sorted.len();
    if n == 0 {
        return serde_json::Value::Null;
    }
    let total: f64 = sorted.iter().map(|&m| f64::from(m)).sum();
    let percentile = |p: f64| sorted[((n as f64 * p).ceil() as usize).clamp(1, n) - 1];
    serde_json::json!({
        "avg_ms": total / n as f64,
        "p50_ms": percentile(0.5),
        "p95_ms": percentile(0.95),
        "p99_ms": percentile(0.99),
        "max_ms": sorted[n - 1],
    })
}

impl FrameStats {
    const LIMIT: usize = 1 << 20;
    pub fn record(&mut self, millis: f32) {
        if self.millis.len() < Self::LIMIT {
            self.millis.push(millis);
        }
    }
    /// The frame's own work: its duration minus waits for the GPU, the
    /// display and the frame limiter.
    pub fn record_cpu(&mut self, millis: f32) {
        if self.cpu.len() < Self::LIMIT {
            self.cpu.push(millis);
        }
    }
    pub fn record_phases(&mut self, millis: [f32; PHASES.len()]) {
        if self.phases.len() < Self::LIMIT {
            self.phases.push(millis);
        }
    }
    /// Average milliseconds per phase over the frames whose CPU time is at
    /// least `min_cpu` (all frames for 0): where the slow frames go.
    fn phase_average(&self, min_cpu: f32) -> serde_json::Value {
        let mut sums = [0.0f64; PHASES.len()];
        let mut n = 0;
        for (i, phases) in self.phases.iter().enumerate() {
            if self.cpu.get(i).is_some_and(|&c| c >= min_cpu) {
                for (sum, m) in sums.iter_mut().zip(phases) {
                    *sum += f64::from(*m);
                }
                n += 1;
            }
        }
        PHASES
            .iter()
            .zip(sums)
            .map(|(name, sum)| (name.to_string(), (sum / f64::from(n.max(1))).into()))
            .collect::<serde_json::Map<_, _>>()
            .into()
    }
    pub fn record_gpu(&mut self, millis: &[f32]) {
        if self.gpu.len() < Self::LIMIT {
            self.gpu.extend_from_slice(millis);
        }
    }
    /// Every recorded frame time, in milliseconds, in order.
    pub fn frame_millis(&self) -> &[f32] {
        &self.millis
    }
    /// CPU time of every recorded frame, in milliseconds.
    pub fn cpu_millis(&self) -> &[f32] {
        &self.cpu
    }
    /// GPU time of every frame the GPU timed, in milliseconds.
    pub fn gpu_millis(&self) -> &[f32] {
        &self.gpu
    }
    pub fn reset(&mut self) {
        self.millis.clear();
        self.cpu.clear();
        self.gpu.clear();
        self.phases.clear();
    }
    pub fn summary(&self) -> serde_json::Value {
        let mut sorted = self.millis.clone();
        sorted.sort_by(f32::total_cmp);
        let n = sorted.len();
        if n == 0 {
            return serde_json::json!({ "frames": 0 });
        }
        let total: f64 = sorted.iter().map(|&m| f64::from(m)).sum();
        let percentile = |p: f64| sorted[((n as f64 * p).ceil() as usize).clamp(1, n) - 1];
        // Average frame time of the slowest 1% (0.1%) of frames, as frames per second.
        let low = |per: usize| {
            let slowest = n.div_ceil(per);
            let slow: f64 = sorted[n - slowest..]
                .iter()
                .map(|&m| f64::from(m))
                .sum::<f64>()
                / slowest as f64;
            1000.0 / slow
        };
        // Where the slowest 1% of frames by CPU time go.
        let slow_phases = {
            let mut cpu = self.cpu.clone();
            cpu.sort_by(f32::total_cmp);
            let p99 = cpu
                .get(cpu.len().saturating_sub(cpu.len().div_ceil(100)))
                .copied();
            p99.map_or(serde_json::Value::Null, |p| self.phase_average(p))
        };
        serde_json::json!({
            "frames": n,
            "seconds": total / 1000.0,
            "avg_fps": n as f64 * 1000.0 / total,
            "low_1pct_fps": low(100),
            "low_01pct_fps": low(1000),
            "p50_ms": percentile(0.5),
            "p95_ms": percentile(0.95),
            "p99_ms": percentile(0.99),
            "max_ms": sorted[n - 1],
            "cpu": percentiles(&self.cpu),
            "gpu": percentiles(&self.gpu),
            "phases_avg_ms": self.phase_average(0.0),
            "slow_phases_avg_ms": slow_phases,
        })
    }
}
