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
    RetainedSender, RetainedWorkerStage,
};

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

        fn prepare_input(path: Option<PathBuf>) -> Self {
            let local = path.is_none();
            Self {
                group: Arc::new(Group {
                    sealed: AtomicBool::new(false),
                    cleanup_requested: AtomicBool::new(false),
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
            if self.group.sealed.load(Ordering::Acquire) {
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
            if self.group.sealed.load(Ordering::Acquire) {
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
        client_worker_inner(&group, &input);
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
