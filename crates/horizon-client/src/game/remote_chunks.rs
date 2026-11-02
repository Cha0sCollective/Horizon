//! A server's chunk columns (`ClientboundLevelChunkWithLightPacket`): the
//! sections and light of each packet become a client column on a decoder
//! thread as soon as the packet arrives, so the frame that handles it in
//! order only inserts the result (`ClientPacketListener.handleLevelChunkWithLight`).
use super::Game;
use horizon_client::{
    chunk::{BIOME_STRATEGY, Chunk, Light, MIN_Y, SECTIONS, Section, block_strategy},
    streaming::Streaming,
};
use horizon_core::paletted::{PalettedContainer, Strategy};
use horizon_protocol::{
    chunk::{LightData, SectionLight},
    packets::play::{self, Clientbound as C},
    registry::DimensionType,
};
use std::sync::{
    Arc,
    mpsc::{self, TryRecvError},
};

/// What turning a chunk packet into a client column depends on: the
/// dimension's height and sky, and the server's biome registry mapped to
/// ours. The connection replaces it when either changes, so a column decoded
/// with an older one is decoded again.
#[derive(Default)]
pub(super) struct ChunkFormat {
    min_y: i32,
    sections: usize,
    has_skylight: bool,
    biome_map: Vec<u16>,
    biome_bits: u8,
}

impl ChunkFormat {
    pub(super) fn new(dimension: &DimensionType, biome_map: Vec<u16>, biome_bits: u8) -> Self {
        Self {
            min_y: dimension.min_y,
            sections: dimension.sections(),
            has_skylight: dimension.has_skylight,
            biome_map,
            biome_bits,
        }
    }
    pub(super) fn with_dimension(&self, dimension: &DimensionType) -> Self {
        Self::new(dimension, self.biome_map.clone(), self.biome_bits)
    }

    /// LevelChunkSection data into the client's fixed -64..320 column.
    fn column(&self, p: &play::LevelChunkWithLight) -> Result<Chunk, String> {
        let sections = p
            .chunk
            .sections(self.sections, 16, self.biome_bits)
            .map_err(|e| e.to_string())?;
        let mut chunk = Chunk::empty(p.x, p.z);
        let offset = (self.min_y - MIN_Y).div_euclid(16);
        // Biome table index per quart cell of the dimension's own sections.
        let mut biome_cells: Vec<[u32; 64]> = Vec::with_capacity(sections.len());
        for (i, s) in sections.iter().enumerate() {
            let target = offset + i as i32;
            let mut cells = [0u32; 64];
            for (j, cell) in cells.iter_mut().enumerate() {
                *cell = u32::from(
                    self.biome_map
                        .get(s.biomes.get(j) as usize)
                        .copied()
                        .unwrap_or(0),
                );
            }
            biome_cells.push(cells);
            if !(0..SECTIONS as i32).contains(&target) {
                continue;
            }
            // A global section is as wide as the server's registry: 16 bits
            // from a vanilla server, wider from a Horizon host with the same
            // content mods. Read it at that width, then widen it to ours.
            let ours = block_strategy();
            let strategy = if ours.wire_has_palette(s.states.bits) {
                ours
            } else {
                Strategy::blocks(s.states.bits)
            };
            let palette = s.states.palette.iter().map(|&p| p as u32).collect();
            let words = s.states.data.iter().map(|&w| w as u64).collect();
            let mut blocks = PalettedContainer::from_wire(strategy, s.states.bits, palette, words)
                .map_err(|e| e.to_string())?;
            if strategy.global_bits != ours.global_bits {
                let mut values = [0u32; 4096];
                blocks.unpack(&mut values);
                // Ids past our registry (a host with mods we lack) read as air.
                let limit = 1u64 << ours.global_bits;
                for v in &mut values {
                    if u64::from(*v) >= limit {
                        *v = 0;
                    }
                }
                blocks = PalettedContainer::from_values(ours, &values);
            }
            let mut section = Section {
                blocks,
                biomes: PalettedContainer::from_values(BIOME_STRATEGY, &cells),
                light: Light::Uniform(0xF0),
            };
            section.compact();
            chunk.sections[target as usize] = Arc::new(section);
        }
        // Sections outside the dimension's range take its nearest biomes
        // (the column's top or bottom cell layer repeated).
        if !biome_cells.is_empty() {
            let last = biome_cells.len() - 1;
            for si in 0..SECTIONS as i32 {
                let i = si - offset;
                if (0..sections.len() as i32).contains(&i) {
                    continue;
                }
                let (edge, layer) = if i < 0 { (0, 0) } else { (last, 3) };
                let mut cells = [0u32; 64];
                for (j, cell) in cells.iter_mut().enumerate() {
                    *cell = biome_cells[edge][layer * 16 + j % 16];
                }
                Arc::make_mut(&mut chunk.sections[si as usize]).biomes =
                    PalettedContainer::from_values(BIOME_STRATEGY, &cells);
            }
        }
        // ClientPacketListener.applyLightData: the column keeps the light
        // the server sent; nothing is relit.
        let (layers, above) = self.light(&p.light);
        horizon_client::light::light_from_layers(&mut chunk, &layers, above, self.has_skylight);
        Ok(chunk)
    }

    /// `ClientPacketListener.readSectionList`: the sky and block layers of a
    /// light packet, by section of the client's column, and the sky light
    /// above the column's top section (from the light section just above
    /// it, else open sky).
    pub(super) fn light(&self, light: &LightData) -> (Vec<SectionLight>, [u8; 256]) {
        let mut layers: Vec<SectionLight> = vec![(None, None); SECTIONS];
        let sky = self.has_skylight;
        let mut above = [if sky { 15 } else { 0 }; 256];
        // Light section b is the dimension's section b - 1.
        let first = (self.min_y - MIN_Y).div_euclid(16) - 1;
        for (b, layer) in light.layers(self.sections + 2).into_iter().enumerate() {
            let si = first + b as i32;
            if (0..SECTIONS as i32).contains(&si) {
                layers[si as usize] = layer;
            } else if si == SECTIONS as i32
                && sky
                && let Some(l) = &layer.0
            {
                for (i, a) in above.iter_mut().enumerate() {
                    *a = (l[i >> 1] >> ((i & 1) * 4)) & 15;
                }
            }
        }
        (layers, above)
    }
}

type Decoded = (Box<C>, Result<Chunk, String>);

/// One thread that decodes chunk packets in the order they arrived; its
/// results come back in that order, one per `submit`.
pub(super) struct ChunkDecoder {
    jobs: mpsc::Sender<(Arc<ChunkFormat>, Box<C>)>,
    done: mpsc::Receiver<Decoded>,
}

impl ChunkDecoder {
    pub(super) fn new() -> Self {
        let (jobs, todo) = mpsc::channel::<(Arc<ChunkFormat>, Box<C>)>();
        let (finished, done) = mpsc::channel();
        // The thread ends with the connection, when `jobs` is dropped.
        let spawned = std::thread::Builder::new()
            .name("Chunk decoder".into())
            .spawn(move || {
                // A frame waits for its columns, as it would for the render
                // thread's own work.
                horizon_threads::favour_current();
                for (format, packet) in todo {
                    let column = match &*packet {
                        C::LevelChunkWithLight(p) => format.column(p),
                        _ => Err("not a chunk packet".into()),
                    };
                    if finished.send((packet, column)).is_err() {
                        break;
                    }
                }
            });
        if let Err(e) = spawned {
            log::warn!("Horizon: no chunk decoder thread ({e}); chunks decode in the frame");
        }
        Self { jobs, done }
    }

    /// Start decoding a chunk packet with the format it arrived under.
    /// Without a decoder thread the packet comes back undecoded.
    pub(super) fn submit(&self, format: Arc<ChunkFormat>, packet: Box<C>) -> Option<Box<C>> {
        self.jobs.send((format, packet)).err().map(|e| (e.0).1)
    }

    /// The oldest submitted packet and its column, unless it is still
    /// decoding: a frame never waits for the decoder.
    pub(super) fn next(&self) -> Next {
        match self.done.try_recv() {
            Ok(decoded) => Next::Ready(decoded),
            Err(TryRecvError::Empty) => Next::Decoding,
            // The decoder thread panicked: its columns are gone.
            Err(TryRecvError::Disconnected) => Next::Lost,
        }
    }
}

pub(super) enum Next {
    Ready(Decoded),
    Decoding,
    Lost,
}

/// Original native decoder ownership for the dormant paired client recipe.
/// Ordinary ChunkDecoder construction and callers remain independent.
#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
pub(super) mod retained {
    use super::{C, Chunk, ChunkFormat};
    use std::{
        collections::VecDeque,
        fmt,
        sync::{
            Arc, Mutex, MutexGuard, TryLockError,
            atomic::{AtomicBool, AtomicU8, Ordering},
            mpsc,
        },
        thread::{self, JoinHandle},
        time::Instant,
    };

    const CAPACITY: usize = 8;
    const CLAIMED: u8 = 1;
    const SEALED: u8 = 2;
    const QUEUED: u8 = 0;
    const ACTIVE: u8 = 1;
    const READY: u8 = 2;

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn try_lock<T>(mutex: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
        match mutex.try_lock() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(error)) => Some(error.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    struct Payload {
        format: Arc<ChunkFormat>,
        packet: Option<Box<C>>,
        column: Option<Result<Chunk, String>>,
    }
    struct Slot {
        phase: AtomicU8,
        payload: Mutex<Payload>,
    }
    struct State {
        // CAS 0 -> CLAIMED is the start-of-decode linearization. Closing sets
        // SEALED in this same atomic, so a queued claim cannot pass the fence.
        fence: AtomicU8,
        slots: Mutex<VecDeque<Arc<Slot>>>,
        panicked: AtomicBool,
        latch_failed: AtomicBool,
        #[cfg(all(
            test,
            feature = "native-writer-test-control",
            debug_assertions,
            target_os = "linux"
        ))]
        control: Option<TestControl>,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(in crate::game) enum Failure {
        Spawn(String),
        Panicked,
        StartLatch,
    }
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(in crate::game) enum StartRefusal {
        Closed,
        Attempted,
        Spawn(String),
        StartLatch,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(in crate::game) enum SubmissionRefusal {
        NotRunning,
        Closed,
        Busy,
        Full,
    }
    pub(in crate::game) struct RefusedInput {
        pub reason: SubmissionRefusal,
        pub format: Arc<ChunkFormat>,
        pub packet: Box<C>,
    }
    impl fmt::Debug for RefusedInput {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("RefusedInput")
                .field("reason", &self.reason)
                .finish_non_exhaustive()
        }
    }
    pub(in crate::game) struct RetainedDecoded {
        pub format: Arc<ChunkFormat>,
        pub packet: Box<C>,
        pub column: Result<Chunk, String>,
    }
    /// A terminal disposition is not proof that the column reached a Game.
    pub(in crate::game) struct ClosedInput {
        pub format: Arc<ChunkFormat>,
        pub packet: Box<C>,
        pub column: Option<Result<Chunk, String>>,
        pub was_active: bool,
    }
    #[derive(Clone, Debug)]
    pub(in crate::game) struct Observation {
        pub sealed: bool,
        pub start_attempted: bool,
        pub spawned: bool,
        pub native_finished: bool,
        pub joined: bool,
        pub native_quiescent: bool,
        pub queued: usize,
        pub active: usize,
        pub ready: usize,
        pub owned: usize,
        pub attempt: u64,
        pub deadline_expired: bool,
        pub failure: Option<Failure>,
    }
    #[derive(Clone, Debug)]
    pub(in crate::game) enum CloseStatus {
        Busy,
        Pending(Observation),
        Blocked {
            attempt: u64,
            /// None means the original registry is busy, never an empty census.
            observation: Option<Observation>,
        },
        Quiescent(Observation),
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(in crate::game) enum RetryRefusal {
        State,
        Attempt,
        Deadline,
        Exhausted,
    }

    pub(in crate::game) struct RetainedChunkDecoder {
        state: Arc<State>,
        handle: Option<JoinHandle<()>>,
        start_attempted: bool,
        spawned: bool,
        joined: bool,
        failure: Option<Failure>,
        deadline: Option<Instant>,
        attempt: u64,
        resolved: bool,
    }
    pub(in crate::game) struct ClosedDecoder {
        state: Arc<State>,
        final_observation: Observation,
    }

    impl RetainedChunkDecoder {
        pub(in crate::game) fn prepare() -> Self {
            Self {
                state: Arc::new(State {
                    fence: AtomicU8::new(0),
                    slots: Mutex::new(VecDeque::new()),
                    panicked: AtomicBool::new(false),
                    latch_failed: AtomicBool::new(false),
                    #[cfg(all(
                        test,
                        feature = "native-writer-test-control",
                        debug_assertions,
                        target_os = "linux"
                    ))]
                    control: None,
                }),
                handle: None,
                start_attempted: false,
                spawned: false,
                joined: false,
                failure: None,
                deadline: None,
                attempt: 0,
                resolved: false,
            }
        }

        pub(in crate::game) fn start(&mut self) -> Result<(), StartRefusal> {
            if self.state.fence.load(Ordering::Acquire) & SEALED != 0 {
                return Err(StartRefusal::Closed);
            }
            if self.start_attempted {
                return Err(StartRefusal::Attempted);
            }
            self.start_attempted = true;
            let (release, wait) = mpsc::channel();
            let state = self.state.clone();
            #[cfg(all(
                test,
                feature = "native-writer-test-control",
                debug_assertions,
                target_os = "linux"
            ))]
            let refuse_spawn = self
                .state
                .control
                .as_ref()
                .is_some_and(|c| c.0.refuse_spawn);
            #[cfg(not(all(
                test,
                feature = "native-writer-test-control",
                debug_assertions,
                target_os = "linux"
            )))]
            let refuse_spawn = false;
            let spawned = if refuse_spawn {
                Err(std::io::Error::other(
                    "controlled retained decoder spawn refusal",
                ))
            } else {
                thread::Builder::new()
                    .name("Retained chunk decoder".into())
                    .spawn(move || {
                        let _guard = ActorGuard(state.clone());
                        if wait.recv().is_err() {
                            state.latch_failed.store(true, Ordering::Release);
                            state.fence.fetch_or(SEALED, Ordering::AcqRel);
                            return;
                        }
                        horizon_threads::favour_current();
                        worker(&state);
                    })
            };
            match spawned {
                Ok(handle) => {
                    // The native handle is owned before the worker may claim
                    // any payload, including an immediate native worker fault.
                    self.handle = Some(handle);
                    self.spawned = true;
                    if release.send(()).is_err() {
                        self.state.fence.fetch_or(SEALED, Ordering::AcqRel);
                        self.failure = Some(Failure::StartLatch);
                        return Err(StartRefusal::StartLatch);
                    }
                    Ok(())
                }
                Err(error) => {
                    let detail = error.to_string();
                    self.failure = Some(Failure::Spawn(detail.clone()));
                    self.state.fence.fetch_or(SEALED, Ordering::AcqRel);
                    Err(StartRefusal::Spawn(detail))
                }
            }
        }

        pub(in crate::game) fn submit(
            &mut self,
            format: Arc<ChunkFormat>,
            packet: Box<C>,
        ) -> Result<(), RefusedInput> {
            let reason = if self.state.fence.load(Ordering::Acquire) & SEALED != 0 {
                Some(SubmissionRefusal::Closed)
            } else if !self.spawned {
                Some(SubmissionRefusal::NotRunning)
            } else {
                None
            };
            if let Some(reason) = reason {
                return Err(RefusedInput {
                    reason,
                    format,
                    packet,
                });
            }
            let Some(mut slots) = try_lock(&self.state.slots) else {
                return Err(RefusedInput {
                    reason: SubmissionRefusal::Busy,
                    format,
                    packet,
                });
            };
            if self.state.fence.load(Ordering::Acquire) & SEALED != 0 {
                return Err(RefusedInput {
                    reason: SubmissionRefusal::Closed,
                    format,
                    packet,
                });
            }
            if slots.len() >= CAPACITY {
                return Err(RefusedInput {
                    reason: SubmissionRefusal::Full,
                    format,
                    packet,
                });
            }
            slots.push_back(Arc::new(Slot {
                phase: AtomicU8::new(QUEUED),
                payload: Mutex::new(Payload {
                    format,
                    packet: Some(packet),
                    column: None,
                }),
            }));
            drop(slots);
            // unpark retains a token if the worker has not parked yet: no
            // notify-without-predicate-lock lost-wakeup window is introduced.
            self.handle.as_ref().unwrap().thread().unpark();
            Ok(())
        }

        pub(in crate::game) fn try_next(&mut self) -> Option<RetainedDecoded> {
            let mut slots = try_lock(&self.state.slots)?;
            let slot = slots.front()?;
            if slot.phase.load(Ordering::Acquire) != READY {
                return None;
            }
            let mut payload = try_lock(&slot.payload)?;
            if payload.packet.is_none() || payload.column.is_none() {
                return None;
            }
            let decoded = RetainedDecoded {
                format: payload.format.clone(),
                packet: payload.packet.take().unwrap(),
                column: payload.column.take().unwrap(),
            };
            drop(payload);
            slots.pop_front();
            Some(decoded)
        }

        pub(in crate::game) fn observe(&self) -> Option<Observation> {
            let slots = try_lock(&self.state.slots)?;
            let sealed = self.state.fence.load(Ordering::Acquire) & SEALED != 0;
            let failure = self.failure.clone().or_else(|| {
                if self.state.panicked.load(Ordering::Acquire) {
                    Some(Failure::Panicked)
                } else if self.state.latch_failed.load(Ordering::Acquire) {
                    Some(Failure::StartLatch)
                } else {
                    None
                }
            });
            Some(Observation {
                sealed,
                start_attempted: self.start_attempted,
                spawned: self.spawned,
                native_finished: self.joined
                    || self.handle.as_ref().is_some_and(JoinHandle::is_finished),
                joined: self.joined,
                native_quiescent: self.deadline.is_some()
                    && sealed
                    && (!self.spawned || self.joined),
                queued: slots
                    .iter()
                    .filter(|s| s.phase.load(Ordering::Acquire) == QUEUED)
                    .count(),
                active: slots
                    .iter()
                    .filter(|s| s.phase.load(Ordering::Acquire) == ACTIVE)
                    .count(),
                ready: slots
                    .iter()
                    .filter(|s| s.phase.load(Ordering::Acquire) == READY)
                    .count(),
                owned: slots.len(),
                attempt: self.attempt,
                deadline_expired: self
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline),
                failure,
            })
        }

        pub(in crate::game) fn begin_close(&mut self, deadline: Instant) {
            // This atomic is the same ordering used for an active claim. No
            // worker payload/queue lock can prevent the close fence or wake.
            self.state.fence.fetch_or(SEALED, Ordering::AcqRel);
            if self.deadline.is_none() {
                self.deadline = Some(deadline);
                self.attempt = 1;
            }
            if let Some(handle) = &self.handle {
                handle.thread().unpark();
            }
        }

        pub(in crate::game) fn retry_close(
            &mut self,
            expected_attempt: u64,
            deadline: Instant,
        ) -> Result<u64, RetryRefusal> {
            let now = Instant::now();
            if self.deadline.is_none_or(|previous| now < previous) || !self.spawned || self.joined {
                return Err(RetryRefusal::State);
            }
            if expected_attempt != self.attempt {
                return Err(RetryRefusal::Attempt);
            }
            if deadline <= now {
                return Err(RetryRefusal::Deadline);
            }
            self.attempt = self.attempt.checked_add(1).ok_or(RetryRefusal::Exhausted)?;
            self.deadline = Some(deadline);
            if let Some(handle) = &self.handle {
                handle.thread().unpark();
            }
            Ok(self.attempt)
        }

        pub(in crate::game) fn poll_close(&mut self) -> CloseStatus {
            if self.handle.as_ref().is_some_and(JoinHandle::is_finished) {
                let joined = self.handle.take().unwrap().join();
                self.joined = true;
                if joined.is_err() {
                    self.failure.get_or_insert(Failure::Panicked);
                }
            }
            match self.observe() {
                Some(observation) if observation.native_quiescent => {
                    CloseStatus::Quiescent(observation)
                }
                observation
                    if self
                        .deadline
                        .is_some_and(|deadline| Instant::now() >= deadline) =>
                {
                    // Deadline/attempt belong to this original owner and do
                    // not require the actor's registry lock to be observable.
                    CloseStatus::Blocked {
                        attempt: self.attempt,
                        observation,
                    }
                }
                Some(observation) => CloseStatus::Pending(observation),
                None => CloseStatus::Busy,
            }
        }

        pub(in crate::game) fn try_closed(mut self) -> Result<ClosedDecoder, Self> {
            let CloseStatus::Quiescent(final_observation) = self.poll_close() else {
                return Err(self);
            };
            self.resolved = true;
            Ok(ClosedDecoder {
                state: self.state.clone(),
                final_observation,
            })
        }

        #[cfg(all(
            test,
            feature = "native-writer-test-control",
            debug_assertions,
            target_os = "linux"
        ))]
        pub(super) fn prepare_controlled(control: TestControl) -> Self {
            let mut decoder = Self::prepare();
            Arc::get_mut(&mut decoder.state).unwrap().control = Some(control);
            decoder
        }
    }

    impl Drop for RetainedChunkDecoder {
        fn drop(&mut self) {
            if !self.resolved {
                use std::io::Write;
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "fatal: unresolved retained chunk decoder owner"
                );
                std::process::abort();
            }
        }
    }
    impl ClosedDecoder {
        pub(in crate::game) fn observe(&self) -> Option<Observation> {
            let slots = try_lock(&self.state.slots)?;
            let mut observation = self.final_observation.clone();
            observation.owned = slots.len();
            observation.queued = slots
                .iter()
                .filter(|s| s.phase.load(Ordering::Acquire) == QUEUED)
                .count();
            observation.active = slots
                .iter()
                .filter(|s| s.phase.load(Ordering::Acquire) == ACTIVE)
                .count();
            observation.ready = slots
                .iter()
                .filter(|s| s.phase.load(Ordering::Acquire) == READY)
                .count();
            Some(observation)
        }
        /// Explicit original-payload disposition after checked native closure.
        pub(in crate::game) fn take_next(&mut self) -> Option<ClosedInput> {
            let mut slots = try_lock(&self.state.slots)?;
            let slot = slots.front()?;
            let mut payload = try_lock(&slot.payload)?;
            let packet = payload.packet.take()?;
            let input = ClosedInput {
                format: payload.format.clone(),
                packet,
                column: payload.column.take(),
                was_active: slot.phase.load(Ordering::Acquire) == ACTIVE,
            };
            drop(payload);
            slots.pop_front();
            Some(input)
        }
    }

    struct ActorGuard(Arc<State>);
    impl Drop for ActorGuard {
        fn drop(&mut self) {
            if thread::panicking() {
                self.0.panicked.store(true, Ordering::Release);
                self.0.fence.fetch_or(SEALED, Ordering::AcqRel);
            }
        }
    }
    fn worker(state: &State) {
        loop {
            if state.fence.load(Ordering::Acquire) & SEALED != 0 {
                break;
            }
            let selected = {
                let slots = lock(&state.slots);
                let next = slots
                    .iter()
                    .find(|slot| slot.phase.load(Ordering::Acquire) == QUEUED);
                match next {
                    Some(slot) => {
                        #[cfg(all(
                            test,
                            feature = "native-writer-test-control",
                            debug_assertions,
                            target_os = "linux"
                        ))]
                        if let Some(control) = &state.control {
                            control.gate(TestPoint::BeforeClaim);
                        }
                        if state
                            .fence
                            .compare_exchange(0, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                        {
                            slot.phase.store(ACTIVE, Ordering::Release);
                            Some(slot.clone())
                        } else {
                            None
                        }
                    }
                    None => None,
                }
            };
            let Some(slot) = selected else {
                if state.fence.load(Ordering::Acquire) & SEALED != 0 {
                    break;
                }
                thread::park();
                continue;
            };
            {
                // The original packet/format remain inside this registered
                // slot even if the real decoding algorithm unwinds.
                let mut payload = lock(&slot.payload);
                #[cfg(all(
                    test,
                    feature = "native-writer-test-control",
                    debug_assertions,
                    target_os = "linux"
                ))]
                if let Some(control) = &state.control {
                    control.gate(TestPoint::BeforeDecode);
                }
                let column = match payload.packet.as_deref().unwrap() {
                    C::LevelChunkWithLight(packet) => payload.format.column(packet),
                    _ => Err("not a chunk packet".into()),
                };
                payload.column = Some(column);
                slot.phase.store(READY, Ordering::Release);
            }
            #[cfg(all(
                test,
                feature = "native-writer-test-control",
                debug_assertions,
                target_os = "linux"
            ))]
            if let Some(control) = &state.control {
                control.gate(TestPoint::AfterResult);
            }
            state.fence.fetch_and(!CLAIMED, Ordering::AcqRel);
        }
        #[cfg(all(
            test,
            feature = "native-writer-test-control",
            debug_assertions,
            target_os = "linux"
        ))]
        if let Some(control) = &state.control {
            control.gate(TestPoint::BeforeExit);
        }
    }

    #[cfg(all(
        test,
        feature = "native-writer-test-control",
        debug_assertions,
        target_os = "linux"
    ))]
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) enum TestPoint {
        BeforeClaim,
        BeforeDecode,
        AfterResult,
        BeforeExit,
    }
    #[cfg(all(
        test,
        feature = "native-writer-test-control",
        debug_assertions,
        target_os = "linux"
    ))]
    struct TestState {
        point: TestPoint,
        occurrence: usize,
        visits: std::sync::atomic::AtomicUsize,
        entered: AtomicBool,
        released: AtomicBool,
        panic: bool,
        refuse_spawn: bool,
    }
    #[cfg(all(
        test,
        feature = "native-writer-test-control",
        debug_assertions,
        target_os = "linux"
    ))]
    #[derive(Clone)]
    pub(super) struct TestControl(Arc<TestState>);
    #[cfg(all(
        test,
        feature = "native-writer-test-control",
        debug_assertions,
        target_os = "linux"
    ))]
    impl TestControl {
        pub(super) fn hold(point: TestPoint, occurrence: usize, panic: bool) -> Self {
            Self(Arc::new(TestState {
                point,
                occurrence,
                visits: std::sync::atomic::AtomicUsize::new(0),
                entered: AtomicBool::new(false),
                released: AtomicBool::new(false),
                panic,
                refuse_spawn: false,
            }))
        }
        pub(super) fn spawn_refusal() -> Self {
            let mut control = Self::hold(TestPoint::BeforeExit, 1, false);
            Arc::get_mut(&mut control.0).unwrap().refuse_spawn = true;
            control
        }
        pub(super) fn entered(&self) -> bool {
            self.0.entered.load(Ordering::Acquire)
        }
        pub(super) fn release(&self) {
            self.0.released.store(true, Ordering::Release);
        }
        fn gate(&self, point: TestPoint) {
            if point != self.0.point
                || self.0.visits.fetch_add(1, Ordering::AcqRel) + 1 != self.0.occurrence
            {
                return;
            }
            self.0.entered.store(true, Ordering::Release);
            while !self.0.released.load(Ordering::Acquire) {
                thread::park_timeout(std::time::Duration::from_millis(1));
            }
            assert!(!self.0.panic, "controlled original decoder worker panic");
        }
    }
}

impl Game<'_> {
    /// `handleLevelChunkWithLight`: the column replaces any loaded one, and
    /// its block entities load their update tags. `decoded` is the column the
    /// decoder made with `format`; another format since means decode again.
    pub(super) fn remote_chunk(
        &mut self,
        p: &play::LevelChunkWithLight,
        decoded: Option<(&Arc<ChunkFormat>, Result<Chunk, String>)>,
    ) {
        // ClientChunkCache.replaceWithPacketData: a column outside the
        // storage around the view centre is ignored.
        if !self.remote_chunk_in_range(p.x, p.z) {
            log::warn!(
                "Ignoring chunk since it's not in the view range: {}, {}",
                p.x,
                p.z
            );
            return;
        }
        let Some(format) = self.remote.as_ref().map(|r| r.chunk_format.clone()) else {
            return;
        };
        let column = match decoded {
            Some((used, column)) if Arc::ptr_eq(used, &format) => column,
            _ => format.column(p),
        };
        let chunk = match column {
            Ok(chunk) => chunk,
            Err(e) => {
                log::warn!("Horizon: chunk {} {}: {e}", p.x, p.z);
                return;
            }
        };
        let (x, z) = (p.x, p.z);
        self.streaming.insert_remote(&mut self.world, chunk);
        self.world
            .block_entities
            .retain(|k, _| (k.0 >> 4, k.2 >> 4) != (x, z));
        for info in &p.chunk.block_entities {
            let pos = (
                x * 16 + i32::from(info.packed_xz as u8 >> 4),
                i32::from(info.y),
                z * 16 + i32::from(info.packed_xz as u8 & 15),
            );
            let state = self.world.block(pos.0, pos.1, pos.2);
            self.sync_block_entity(pos, state);
            // LevelChunk.replaceWithPacketData: the block entity loads its update tag.
            self.remote_block_entity_data([pos.0, pos.1, pos.2], info.kind, &info.tag);
        }
    }

    /// `handleLightUpdatePacket`: the sections it names take its light.
    pub(super) fn remote_light_update(&mut self, p: &play::LightUpdate) {
        let Some(format) = self.remote.as_ref().map(|r| r.chunk_format.clone()) else {
            return;
        };
        if !self.world.chunks.contains_key(&(p.x, p.z)) {
            return;
        }
        let (layers, _) = format.light(&p.light);
        let Some(chunk) = self.world.chunks.get_mut(&(p.x, p.z)) else {
            return;
        };
        let changed = horizon_client::light::update_light_layers(
            Arc::make_mut(chunk),
            &layers,
            format.has_skylight,
        );
        Streaming::light_updated(&mut self.world, p.x, p.z, &changed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use horizon_protocol::chunk::{ChunkData, Paletted, Section as Wire, Strategy as Wiring};

    /// A column of `state` with biome 1 everywhere, unlit.
    fn packet(x: i32, state: i32) -> Box<C> {
        let section = Wire {
            non_empty: 4096,
            fluids: 0,
            states: Paletted::from_values(&[state; 4096], Wiring::Blocks, 16),
            biomes: Paletted::from_values(&[1; 64], Wiring::Biomes, 2),
        };
        let none = LightData {
            sky_mask: Vec::new(),
            block_mask: Vec::new(),
            empty_sky_mask: Vec::new(),
            empty_block_mask: Vec::new(),
            sky: Vec::new(),
            block: Vec::new(),
        };
        Box::new(C::LevelChunkWithLight(play::LevelChunkWithLight {
            x,
            z: -3,
            chunk: ChunkData {
                heightmaps: Vec::new(),
                buffer: ChunkData::from_sections(&vec![section; 24]),
                block_entities: Vec::new(),
            },
            light: none,
        }))
    }

    /// Columns come back one per packet, in the order the packets went in,
    /// as the frame would have decoded them itself.
    #[test]
    fn the_decoder_returns_columns_in_order() {
        let overworld = DimensionType::vanilla("minecraft:overworld");
        let format = Arc::new(ChunkFormat::new(&overworld, vec![0, 7, 9], 2));
        let decoder = ChunkDecoder::new();
        for x in 0..20 {
            assert!(decoder.submit(format.clone(), packet(x, 1 + x)).is_none());
        }
        for x in 0..20 {
            let (packet, column) = loop {
                match decoder.next() {
                    Next::Ready(decoded) => break decoded,
                    Next::Decoding => std::thread::yield_now(),
                    Next::Lost => panic!("the decoder stopped"),
                }
            };
            let C::LevelChunkWithLight(p) = *packet else {
                panic!("not a chunk packet");
            };
            let (column, direct) = (column.unwrap(), format.column(&p).unwrap());
            assert_eq!((column.x, column.z), (x, -3));
            for (cx, y, cz) in [(0, -64, 0), (5, 70, 9), (15, 319, 15)] {
                assert_eq!(column.get(cx, y, cz), 1 + x as u32);
                assert_eq!(column.get(cx, y, cz), direct.get(cx, y, cz));
                // Wire biome 1 is our biome 7.
                assert_eq!(column.biome(cx, y, cz), 7);
            }
        }
        assert!(matches!(decoder.next(), Next::Decoding));
    }

    #[cfg(all(
        feature = "paired-packages",
        feature = "native-writer-test-control",
        debug_assertions,
        target_os = "linux"
    ))]
    mod retained_tests {
        use super::super::retained::*;
        use super::{ChunkFormat, DimensionType, packet};
        use std::{
            sync::Arc,
            thread,
            time::{Duration, Instant},
        };

        fn until() -> Instant {
            Instant::now() + Duration::from_secs(5)
        }
        fn format(biome: u16) -> Arc<ChunkFormat> {
            Arc::new(ChunkFormat::new(
                &DimensionType::vanilla("minecraft:overworld"),
                vec![0, biome],
                2,
            ))
        }
        fn wait_entered(control: &TestControl) {
            let deadline = until();
            while !control.entered() {
                assert!(
                    Instant::now() < deadline,
                    "actual decoder worker gate not entered"
                );
                thread::yield_now();
            }
        }
        fn submit(
            decoder: &mut RetainedChunkDecoder,
            mut format: Arc<ChunkFormat>,
            mut packet: Box<super::C>,
        ) {
            let deadline = until();
            loop {
                match decoder.submit(format, packet) {
                    Ok(()) => return,
                    Err(refused) => {
                        assert_eq!(refused.reason, SubmissionRefusal::Busy);
                        assert!(Instant::now() < deadline, "decoder admission remained busy");
                        format = refused.format;
                        packet = refused.packet;
                        thread::yield_now();
                    }
                }
            }
        }
        fn next(decoder: &mut RetainedChunkDecoder) -> RetainedDecoded {
            let deadline = until();
            loop {
                if let Some(decoded) = decoder.try_next() {
                    return decoded;
                }
                assert!(Instant::now() < deadline, "actual decoder result missing");
                thread::yield_now();
            }
        }
        fn close(mut decoder: RetainedChunkDecoder) -> ClosedDecoder {
            decoder.begin_close(until());
            let deadline = until();
            loop {
                if matches!(decoder.poll_close(), CloseStatus::Quiescent(_)) {
                    return match decoder.try_closed() {
                        Ok(closed) => closed,
                        Err(_) => panic!("checked decoder closure refused its original handoff"),
                    };
                }
                assert!(
                    Instant::now() < deadline,
                    "actual native decoder did not finish"
                );
                thread::yield_now();
            }
        }
        fn packet_x(packet: &super::C) -> i32 {
            match packet {
                super::C::LevelChunkWithLight(packet) => packet.x,
                _ => panic!("wrong retained packet variant"),
            }
        }

        #[test]
        fn retained_decoder_prepare_and_spawn_refusal() {
            let mut prepared = RetainedChunkDecoder::prepare();
            let initial = prepared.observe().unwrap();
            assert!(!initial.start_attempted && !initial.spawned && !initial.joined);
            assert_eq!(initial.owned, 0);
            let original = format(7);
            let refused = prepared
                .submit(original.clone(), packet(42, 3))
                .unwrap_err();
            assert_eq!(refused.reason, SubmissionRefusal::NotRunning);
            assert!(Arc::ptr_eq(&original, &refused.format));
            assert_eq!(packet_x(&refused.packet), 42);
            prepared.begin_close(Instant::now());
            prepared.begin_close(until());
            assert_eq!(prepared.start(), Err(StartRefusal::Closed));
            let closed = close(prepared);
            let observation = closed.observe().unwrap();
            assert!(observation.native_quiescent && observation.deadline_expired);
            assert!(!observation.spawned && !observation.joined);
            assert_eq!(observation.attempt, 1);
            assert_eq!(observation.failure, None);

            let mut refused =
                RetainedChunkDecoder::prepare_controlled(TestControl::spawn_refusal());
            assert!(
                matches!(refused.start(), Err(StartRefusal::Spawn(detail)) if detail.contains("controlled"))
            );
            assert_eq!(refused.start(), Err(StartRefusal::Closed));
            let closed = close(refused);
            let observation = closed.observe().unwrap();
            assert!(observation.start_attempted && observation.native_quiescent);
            assert!(!observation.spawned && !observation.joined);
            assert!(matches!(observation.failure, Some(Failure::Spawn(_))));
        }

        #[test]
        fn retained_decoder_real_columns_keep_order_and_format() {
            let mut decoder = RetainedChunkDecoder::prepare();
            decoder.start().unwrap();
            assert_eq!(decoder.start(), Err(StartRefusal::Attempted));
            let formats = [format(7), format(9), format(11)];
            for (x, format) in formats.iter().enumerate() {
                submit(&mut decoder, format.clone(), packet(x as i32, 3 + x as i32));
            }
            for (x, original) in formats.iter().enumerate() {
                let decoded = next(&mut decoder);
                assert!(Arc::ptr_eq(original, &decoded.format));
                assert_eq!(packet_x(&decoded.packet), x as i32);
                let column = decoded.column.unwrap();
                assert_eq!((column.x, column.z), (x as i32, -3));
                for (cx, y, cz) in [(0, -64, 0), (5, 70, 9), (15, 319, 15)] {
                    assert_eq!(column.get(cx, y, cz), 3 + x as u32);
                    assert_eq!(column.biome(cx, y, cz), [7, 9, 11][x]);
                }
            }
            let observation = close(decoder).observe().unwrap();
            assert_eq!(observation.owned, 0);
            assert!(observation.spawned && observation.joined && observation.native_quiescent);
            assert_eq!(observation.failure, None);
        }

        #[test]
        fn retained_decoder_eight_slots_and_owned_handoff() {
            let gate = TestControl::hold(TestPoint::BeforeDecode, 1, false);
            let mut decoder = RetainedChunkDecoder::prepare_controlled(gate.clone());
            decoder.start().unwrap();
            let original = format(7);
            for x in 0..8 {
                submit(&mut decoder, original.clone(), packet(x, 1));
            }
            wait_entered(&gate);
            let observation = decoder.observe().unwrap();
            assert_eq!(
                (observation.owned, observation.active, observation.queued),
                (8, 1, 7)
            );
            // The actual actor holds its payload lock here; these fixed UI
            // operations still return without borrowing/waiting for that lock.
            assert!(decoder.try_next().is_none());
            let ninth = packet(8, 2);
            let address = &*ninth as *const super::C;
            let refused = decoder.submit(original.clone(), ninth).unwrap_err();
            assert_eq!(refused.reason, SubmissionRefusal::Full);
            assert!(Arc::ptr_eq(&original, &refused.format));
            assert_eq!(&*refused.packet as *const super::C, address);
            gate.release();
            assert_eq!(packet_x(&next(&mut decoder).packet), 0);
            submit(&mut decoder, refused.format, refused.packet);
            decoder.begin_close(until());
            let refused = decoder.submit(original.clone(), packet(9, 3)).unwrap_err();
            assert_eq!(refused.reason, SubmissionRefusal::Closed);
            assert_eq!(packet_x(&refused.packet), 9);
            let mut closed = close(decoder);
            assert_eq!(closed.observe().unwrap().owned, 8);
            for x in 1..=8 {
                let input = closed.take_next().unwrap();
                assert_eq!(packet_x(&input.packet), x);
                assert!(Arc::ptr_eq(&input.format, &original));
            }
            assert!(closed.take_next().is_none());
            assert_eq!(closed.observe().unwrap().owned, 0);
        }

        #[test]
        fn retained_decoder_close_keeps_active_queued_and_late_results() {
            let gate = TestControl::hold(TestPoint::BeforeDecode, 2, false);
            let mut decoder = RetainedChunkDecoder::prepare_controlled(gate.clone());
            decoder.start().unwrap();
            let original = format(7);
            for x in 0..3 {
                submit(&mut decoder, original.clone(), packet(x, 1));
            }
            wait_entered(&gate);
            let observation = decoder.observe().unwrap();
            assert_eq!(
                (observation.ready, observation.active, observation.queued),
                (1, 1, 1)
            );
            decoder.begin_close(Instant::now());
            decoder.begin_close(until());
            assert!(
                matches!(decoder.poll_close(), CloseStatus::Blocked { attempt: 1, observation: Some(o) } if o.attempt == 1 && o.owned == 3 && !o.joined)
            );
            let mut decoder = match decoder.try_closed() {
                Err(decoder) => decoder,
                Ok(_) => panic!("held native decoder falsely closed"),
            };
            assert_eq!(decoder.retry_close(0, until()), Err(RetryRefusal::Attempt));
            assert_eq!(
                decoder.retry_close(1, Instant::now()),
                Err(RetryRefusal::Deadline)
            );
            assert_eq!(decoder.retry_close(1, until()), Ok(2));
            gate.release();
            let mut closed = close(decoder);
            let observation = closed.observe().unwrap();
            assert_eq!(
                (observation.ready, observation.queued, observation.owned),
                (2, 1, 3)
            );
            assert_eq!(observation.attempt, 2);
            assert!(observation.joined && observation.native_quiescent);
            for x in 0..3 {
                let input = closed.take_next().unwrap();
                assert_eq!(packet_x(&input.packet), x);
                assert!(Arc::ptr_eq(&input.format, &original));
                if x < 2 {
                    assert!(input.column.unwrap().is_ok());
                } else {
                    assert!(input.column.is_none());
                }
            }
        }

        #[test]
        fn retained_decoder_malformed_and_panicked_work_stays_owned() {
            let mut decoder = RetainedChunkDecoder::prepare();
            decoder.start().unwrap();
            let original = format(7);
            let mut malformed = packet(4, 1);
            if let super::C::LevelChunkWithLight(packet) = &mut *malformed {
                packet.chunk.buffer.clear();
            }
            submit(&mut decoder, original.clone(), malformed);
            let decoded = next(&mut decoder);
            assert_eq!(packet_x(&decoded.packet), 4);
            assert!(decoded.column.is_err());
            assert_eq!(close(decoder).observe().unwrap().failure, None);

            for point in [TestPoint::BeforeDecode, TestPoint::AfterResult] {
                let occurrence = if point == TestPoint::BeforeDecode {
                    2
                } else {
                    1
                };
                let gate = TestControl::hold(point, occurrence, true);
                let mut decoder = RetainedChunkDecoder::prepare_controlled(gate.clone());
                decoder.start().unwrap();
                for x in 0..3 {
                    submit(&mut decoder, original.clone(), packet(x, 2));
                }
                wait_entered(&gate);
                decoder.begin_close(until());
                gate.release(); // Actual production actor unwinds, not an echo worker.
                let mut closed = close(decoder);
                let observation = closed.observe().unwrap();
                assert_eq!(observation.failure, Some(Failure::Panicked));
                assert!(observation.joined && observation.native_quiescent);
                assert_eq!(observation.owned, 3);
                for x in 0..3 {
                    let input = closed.take_next().unwrap();
                    assert_eq!(packet_x(&input.packet), x);
                    assert!(Arc::ptr_eq(&input.format, &original));
                    if x == 0 {
                        assert!(input.column.unwrap().is_ok());
                    } else {
                        assert!(input.column.is_none());
                        assert_eq!(input.was_active, point == TestPoint::BeforeDecode && x == 1);
                    }
                }
            }
        }

        #[test]
        fn retained_decoder_admission_close_and_exit_races() {
            let original = format(7);
            for panic in [false, true] {
                let gate = TestControl::hold(TestPoint::BeforeClaim, 1, panic);
                let mut decoder = RetainedChunkDecoder::prepare_controlled(gate.clone());
                decoder.start().unwrap();
                submit(&mut decoder, original.clone(), packet(17, 2));
                wait_entered(&gate); // Actual actor holds the registry before CAS.
                assert!(decoder.observe().is_none());
                let refused = decoder.submit(original.clone(), packet(18, 3)).unwrap_err();
                assert_eq!(refused.reason, SubmissionRefusal::Busy);
                assert_eq!(packet_x(&refused.packet), 18);
                decoder.begin_close(Instant::now()); // Fence does not wait for registry.
                assert!(matches!(
                    decoder.poll_close(),
                    CloseStatus::Blocked {
                        attempt: 1,
                        observation: None
                    }
                ));
                gate.release();
                let mut closed = close(decoder);
                let observation = closed.observe().unwrap();
                assert_eq!(
                    (observation.queued, observation.active, observation.ready),
                    (1, 0, 0)
                );
                assert_eq!(observation.failure, panic.then_some(Failure::Panicked));
                let input = closed.take_next().unwrap();
                assert_eq!(packet_x(&input.packet), 17);
                assert!(input.column.is_none());
            }

            let gate = TestControl::hold(TestPoint::BeforeExit, 1, false);
            let mut decoder = RetainedChunkDecoder::prepare_controlled(gate.clone());
            decoder.start().unwrap();
            // Repeated actual wake/idle transitions exercise retained unpark
            // tokens rather than a notification masquerading as completion.
            for x in 0..16 {
                submit(&mut decoder, original.clone(), packet(x, 1));
                assert_eq!(packet_x(&next(&mut decoder).packet), x);
            }
            decoder.begin_close(Instant::now());
            wait_entered(&gate);
            assert!(
                matches!(decoder.poll_close(), CloseStatus::Blocked { attempt: 1, observation: Some(o) } if o.owned == 0 && !o.joined && !o.native_finished)
            );
            assert_eq!(decoder.retry_close(1, until()), Ok(2));
            gate.release();
            let observation = close(decoder).observe().unwrap();
            assert!(observation.native_quiescent && observation.joined);
            assert_eq!(observation.failure, None);
        }

        #[test]
        #[ignore = "requires an isolated fatal child with HORIZON_RETAINED_DECODER_FATAL=1"]
        fn retained_decoder_unresolved_native_owner_aborts() {
            assert_eq!(
                std::env::var("HORIZON_RETAINED_DECODER_FATAL").as_deref(),
                Ok("1")
            );
            let gate = TestControl::hold(TestPoint::BeforeDecode, 1, false);
            let mut decoder = RetainedChunkDecoder::prepare_controlled(gate.clone());
            decoder.start().unwrap();
            submit(&mut decoder, format(7), packet(31, 1));
            wait_entered(&gate);
            let observation = decoder.observe().unwrap();
            assert!(observation.spawned && !observation.joined);
            assert_eq!(observation.active, 1);
            println!("S1DC1_NATIVE_DECODER_HELD");
            drop(decoder);
            panic!("unresolved native decoder destruction returned");
        }
    }
}
