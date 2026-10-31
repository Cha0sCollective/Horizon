//! Chunk streaming: a worker pool generates, lights and meshes chunk columns
//! around the player, closest first. The game thread owns the world and only
//! hands immutable `Arc` snapshots to workers; results carry a sequence number
//! so stale work is dropped.
use crate::fast_hash::{FxHashMap, FxHashSet};
use crate::{
    chunk::{Chunk, SECTIONS},
    light::{LightChanges, LightTables, apply_lighting, apply_relighting, light_chunk},
    mesher::{MeshTables, Mesher, Region, SectionMesh, borrow},
    world::World,
};
use horizon_core::biome::BiomeZoom;
use horizon_world::block_render::FaceVisibility;
use horizon_world::worldgen::decoration_order::DecorationOrder;
use std::{
    collections::{BinaryHeap, HashSet, VecDeque},
    sync::{
        Arc, Condvar, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

/// A chunk under generation, opaque to the streamer.
pub type Proto = Box<dyn std::any::Any + Send>;
/// Chunk generation in vanilla's stages: a neighbour-independent proto chunk
/// (biomes, noise, surface, carvers), decoration of a 3x3 neighbourhood
/// (FEATURES, which may write into neighbours), then conversion of a chunk
/// whose eight neighbours are decorated into a final column. Implementations
/// must be thread-safe; each worker thread owns one `Worker`.
pub trait ChunkGenerator: Send + Sync + 'static {
    type Worker: Send + 'static;
    fn worker(&self) -> Self::Worker;
    fn lod_sampler(self: Arc<Self>) -> Box<dyn LodSampler>
    where
        Self: Sized,
    {
        let worker = self.worker();
        Box::new(GeneratorSampler {
            generator: self,
            worker,
        })
    }
    /// An undecorated far-terrain column, without allocating gameplay chunks.
    fn lod_surface(
        &self,
        _worker: &mut Self::Worker,
        _x: i32,
        _z: i32,
    ) -> Option<crate::lod::Surface> {
        None
    }
    /// A previously saved final chunk, bypassing generation.
    fn load(&self, _x: i32, _z: i32) -> Option<Chunk> {
        None
    }
    /// What a loaded chunk needs from the generator before the game uses it,
    /// on the worker that loaded it.
    fn prepare_loaded(&self, _worker: &mut Self::Worker, _chunk: &Chunk) {}
    fn proto(&self, worker: &mut Self::Worker, x: i32, z: i32) -> Proto;
    fn decorates(&self) -> bool {
        false
    }
    /// Decorate `region[4]`; the region is row-major (dz, dx) around it.
    fn decorate(&self, _worker: &mut Self::Worker, _region: &mut [Proto; 9]) {}
    fn finish(&self, proto: Proto) -> Chunk;
    /// Whether `proto` has been decorated (a saved one can come back decorated).
    fn is_decorated(&self, _proto: &Proto) -> bool {
        false
    }
    /// What to save of a proto chunk dropped before it became final.
    /// A decorated proto chunk's blocks as a column, to light a finished
    /// neighbour against (vanilla lights with neighbours at FEATURES).
    fn light_view(&self, _proto: &Proto) -> Option<Chunk> {
        None
    }
    fn saved_proto(&self, _proto: &Proto) -> Option<crate::chunk::SavedProto> {
        None
    }
    /// Put a saved proto chunk's state back onto `proto`, generated again.
    fn restore_proto(&self, _proto: &mut Proto, _saved: crate::chunk::SavedProto) {}
    /// Save proto chunks decoration wrote to, so that they come back as they
    /// were: generating them again would lose those writes.
    fn save_protos(&self, _protos: &[&Proto]) -> Result<(), String> {
        Ok(())
    }
    /// Save a column that finished after the player moved away: its
    /// neighbours' decoration is done, so it can't be generated again.
    fn save_finished(&self, _chunk: &Arc<Chunk>) -> Result<(), String> {
        Ok(())
    }
}
pub type LodDetail = ((i32, i32), Arc<[crate::lod::Column; 256]>);

/// Independent, thread-owned far-terrain sampling state.
pub trait LodSampler: Send {
    /// Independent sampling state; native detail generation stays with the owner.
    fn fork(&self) -> Option<Box<dyn LodSampler>> {
        None
    }
    fn request_detail(&mut self, _chunks: &[(i32, i32)], _center: [i32; 2]) {}
    fn drain_details(&mut self) -> Vec<LodDetail> {
        Vec::new()
    }
    fn sample(&mut self, x: i32, z: i32) -> Option<crate::lod::Surface>;
    fn column(&mut self, x: i32, z: i32) -> crate::lod::Column {
        crate::lod::Column::from_surface(self.sample(x, z))
    }
    fn reduced_column(&mut self, x: i32, z: i32, _width: i32) -> crate::lod::Column {
        self.column(x, z)
    }
    fn grid(&mut self, x: i32, z: i32, stride: i32, side: usize) -> Vec<crate::lod::Column> {
        let mut columns = Vec::with_capacity(side * side);
        for dz in 0..side {
            for dx in 0..side {
                columns.push(self.reduced_column(
                    x + dx as i32 * stride,
                    z + dz as i32 * stride,
                    stride,
                ));
            }
        }
        columns
    }
}
pub trait LodSource: Send + Sync {
    fn sampler(self: Arc<Self>) -> Box<dyn LodSampler>;
}
struct GeneratorSampler<G: ChunkGenerator> {
    generator: Arc<G>,
    worker: G::Worker,
}
impl<G: ChunkGenerator> LodSampler for GeneratorSampler<G> {
    fn sample(&mut self, x: i32, z: i32) -> Option<crate::lod::Surface> {
        self.generator.lod_surface(&mut self.worker, x, z)
    }
}
impl<G: ChunkGenerator> LodSource for G {
    fn sampler(self: Arc<Self>) -> Box<dyn LodSampler> {
        ChunkGenerator::lod_sampler(self)
    }
}
/// The generator's saving, for the streamer (which doesn't know its type).
trait Saves: LodSource {
    fn save_protos(&self, protos: &[&Proto]) -> Result<(), String>;
    fn save_finished(&self, chunk: &Arc<Chunk>) -> Result<(), String>;
}
impl<G: ChunkGenerator> Saves for G {
    fn save_protos(&self, protos: &[&Proto]) -> Result<(), String> {
        ChunkGenerator::save_protos(self, protos)
    }
    fn save_finished(&self, chunk: &Arc<Chunk>) -> Result<(), String> {
        ChunkGenerator::save_finished(self, chunk)
    }
}
/// Per-job mesher inputs that change with the dimension or the options.
#[derive(Clone, Copy)]
struct MeshSettings {
    nether: bool,
    zoom: BiomeZoom,
    blend_radius: i32,
    smooth_lighting: bool,
    cutout_leaves: bool,
}
/// The native producer, carried from submission rather than inferred from Done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum NativeWorkOrigin {
    Generate,
    Decorate,
    Finish,
    Light,
    Mesh,
    Save,
    SaveProtosCallback,
    SaveFinishedCallback,
}
const WORK_ORIGINS: [NativeWorkOrigin; 8] = [
    NativeWorkOrigin::Generate,
    NativeWorkOrigin::Decorate,
    NativeWorkOrigin::Finish,
    NativeWorkOrigin::Light,
    NativeWorkOrigin::Mesh,
    NativeWorkOrigin::Save,
    NativeWorkOrigin::SaveProtosCallback,
    NativeWorkOrigin::SaveFinishedCallback,
];
/// Named lifetime counts for one Streaming instance. Accepted storage callbacks
/// mean enqueue acceptance only; the storage writer's lifetime is outside this report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NativeWorkCounts {
    pub submitted: u64,
    pub queued: u64,
    pub worker_held: u64,
    pub returned: u64,
    pub reconciling: u64,
    pub disposed: u64,
    pub unresolved: u64,
    pub accepted_storage_handoffs: u64,
    pub storage_errors: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeWorkObservation {
    pub origins: [(NativeWorkOrigin, NativeWorkCounts); 8],
    pub initialized_workers: u64,
    pub worker_initialization_fault: bool,
    pub worker_exit_fault: bool,
    pub result_disconnected: bool,
    pub receipt_mismatch: bool,
    pub accounting_fault: bool,
    pub stopped: bool,
}
#[derive(Default)]
struct OwnershipBook {
    counts: [NativeWorkCounts; 8],
    initialized_workers: u64,
    worker_initialization_fault: bool,
    worker_exit_fault: bool,
    result_disconnected: bool,
    receipt_mismatch: bool,
    accounting_fault: bool,
}
#[derive(Clone, Copy)]
enum WorkPhase {
    Queued,
    Worker,
    Returned,
    Reconciling,
}
impl OwnershipBook {
    fn add(value: &mut u64) -> bool {
        if let Some(next) = value.checked_add(1) {
            *value = next;
            true
        } else {
            false
        }
    }
    fn phase(&mut self, origin: NativeWorkOrigin, phase: WorkPhase) -> &mut u64 {
        let c = &mut self.counts[origin as usize];
        match phase {
            WorkPhase::Queued => &mut c.queued,
            WorkPhase::Worker => &mut c.worker_held,
            WorkPhase::Returned => &mut c.returned,
            WorkPhase::Reconciling => &mut c.reconciling,
        }
    }
}
struct WorkLease {
    book: Arc<Mutex<OwnershipBook>>,
    origin: NativeWorkOrigin,
    phase: WorkPhase,
    counted: bool,
    settled: bool,
}
impl WorkLease {
    fn new(book: &Arc<Mutex<OwnershipBook>>, origin: NativeWorkOrigin, phase: WorkPhase) -> Self {
        let mut state = book.lock().unwrap_or_else(|e| e.into_inner());
        let submitted = OwnershipBook::add(&mut state.counts[origin as usize].submitted);
        let counted = OwnershipBook::add(state.phase(origin, phase));
        state.accounting_fault |= !submitted || !counted;
        drop(state);
        Self {
            book: book.clone(),
            origin,
            phase,
            counted,
            settled: false,
        }
    }
    fn advance(&mut self, phase: WorkPhase) {
        let mut state = self.book.lock().unwrap_or_else(|e| e.into_inner());
        if self.counted {
            let old = state.phase(self.origin, self.phase);
            if let Some(next) = old.checked_sub(1) {
                *old = next;
            } else {
                state.accounting_fault = true;
            }
        }
        self.counted = OwnershipBook::add(state.phase(self.origin, phase));
        state.accounting_fault |= !self.counted;
        self.phase = phase;
    }
    fn storage(&self, accepted: bool) {
        let mut state = self.book.lock().unwrap_or_else(|e| e.into_inner());
        let c = &mut state.counts[self.origin as usize];
        let ok = OwnershipBook::add(if accepted {
            &mut c.accepted_storage_handoffs
        } else {
            &mut c.storage_errors
        });
        state.accounting_fault |= !ok;
    }
    fn settle(mut self) {
        let mut state = self.book.lock().unwrap_or_else(|e| e.into_inner());
        if self.counted {
            let old = state.phase(self.origin, self.phase);
            if let Some(next) = old.checked_sub(1) {
                *old = next;
            } else {
                state.accounting_fault = true;
            }
        }
        let ok = OwnershipBook::add(&mut state.counts[self.origin as usize].disposed);
        state.accounting_fault |= !ok;
        self.settled = true;
    }
}
impl Drop for WorkLease {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let mut state = self.book.lock().unwrap_or_else(|e| e.into_inner());
        if self.counted {
            let old = state.phase(self.origin, self.phase);
            if let Some(next) = old.checked_sub(1) {
                *old = next;
            } else {
                state.accounting_fault = true;
            }
        }
        let ok = OwnershipBook::add(&mut state.counts[self.origin as usize].unresolved);
        state.accounting_fault |= !ok;
    }
}
struct WorkerLifetime {
    book: Arc<Mutex<OwnershipBook>>,
    initialized: bool,
    stopped: bool,
}
impl WorkerLifetime {
    fn initialized(&mut self) {
        self.initialized = true;
        let mut book = self.book.lock().unwrap_or_else(|e| e.into_inner());
        let ok = OwnershipBook::add(&mut book.initialized_workers);
        book.accounting_fault |= !ok;
    }
}
impl Drop for WorkerLifetime {
    fn drop(&mut self) {
        let mut book = self.book.lock().unwrap_or_else(|e| e.into_inner());
        if !self.initialized {
            book.worker_initialization_fault = true;
        } else if !self.stopped {
            book.worker_exit_fault = true;
        }
    }
}
struct ReceivedWork {
    done: Done,
    owner: Option<WorkLease>,
}
struct Reconciliation(Option<WorkLease>);
impl Drop for Reconciliation {
    fn drop(&mut self) {
        if let Some(owner) = self.0.take()
            && !thread::panicking()
        {
            owner.settle();
        }
    }
}
#[derive(PartialEq, Eq)]
enum ResultStamp {
    Generated(i32, i32),
    Proto(i32, i32),
    Decorated(i32, i32),
    Lit(i32, i32, u64),
    Meshed(i32, i32, i32, u64),
    Saved,
}
impl Done {
    fn stamp(&self) -> ResultStamp {
        match self {
            Self::Generated(c) => ResultStamp::Generated(c.x, c.z),
            Self::Proto(x, z, ..) => ResultStamp::Proto(*x, *z),
            Self::Decorated(x, z, _) => ResultStamp::Decorated(*x, *z),
            Self::Lit(x, z, _, seq) => ResultStamp::Lit(*x, *z, *seq),
            Self::Meshed(x, y, z, _, seq) => ResultStamp::Meshed(*x, *y, *z, *seq),
            Self::Saved(..) => ResultStamp::Saved,
        }
    }
}
enum Job {
    Generate(i32, i32),
    Decorate(i32, i32, Box<[Proto; 9]>),
    Finish(Proto),
    Light(i32, i32, Box<Region>, u64, bool),
    Mesh(i32, i32, i32, Box<Region>, u64, MeshSettings),
    /// Proto chunks checked out to be saved (`ChunkGenerator::save_protos`),
    /// off the main thread like vanilla's IOWorker.
    Save(Vec<((i32, i32), Proto)>),
}
pub enum Done {
    Generated(Chunk),
    /// A proto chunk, and whether it was decorated (loaded so from the save).
    Proto(i32, i32, Proto, bool),
    Decorated(i32, i32, Box<[Proto; 9]>),
    Lit(i32, i32, Vec<crate::chunk::Light>, u64),
    Meshed(i32, i32, i32, SectionMesh, u64),
    Saved(Vec<((i32, i32), Proto)>, Result<(), String>),
}
struct Queued {
    priority: i64,
    /// The column the job is for and what it adds to the column's distance
    /// (`Streaming::submit`): the priority is computed again from them when
    /// the centre moves (`Streaming::requeue`). None for saves, which go first.
    at: Option<(i32, i32)>,
    bias: i64,
    order: u64,
    job: Job,
}
impl PartialEq for Queued {
    fn eq(&self, o: &Self) -> bool {
        self.priority == o.priority && self.order == o.order
    }
}
impl Eq for Queued {}
impl PartialOrd for Queued {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Queued {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        // BinaryHeap is a max-heap: lower priority value = more urgent.
        o.priority
            .cmp(&self.priority)
            .then(o.order.cmp(&self.order))
    }
}
struct Shared {
    queue: Mutex<BinaryHeap<Queued>>,
    ready: Condvar,
    stop: AtomicBool,
    /// What meshing reads (block models and sprites), swapped when resource
    /// packs reload; each mesh job takes the current one.
    context: RwLock<Arc<WorkerContext>>,
    ownership: Arc<Mutex<OwnershipBook>>,
    returned: Mutex<VecDeque<(ResultStamp, WorkLease)>>,
    queued_owners: Mutex<FxHashMap<u64, WorkLease>>,
    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    native_column_load_gate: Mutex<Option<Arc<NativeColumnLoadGate>>>,
    #[cfg(test)]
    work_gate: Mutex<Option<WorkGate>>,
    #[cfg(test)]
    idle_wait_gate: Mutex<Option<IdleWaitGate>>,
    #[cfg(test)]
    shutdown_events: Mutex<Option<mpsc::Sender<ShutdownEvent>>>,
}
/// One closed developer control at the real native Generate/load seam. The
/// actual work lease remains worker-held throughout the pause.
#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
struct NativeColumnLoadGate {
    column: (i32, i32),
    entered: mpsc::Sender<()>,
    entered_once: AtomicBool,
    resumed: AtomicBool,
    released: Mutex<bool>,
    ready: Condvar,
}
#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
impl NativeColumnLoadGate {
    fn release(&self) {
        *self.released.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.ready.notify_all();
    }
    fn wait(&self) {
        let mut released = self.released.lock().unwrap_or_else(|e| e.into_inner());
        if !self.entered_once.swap(true, Ordering::AcqRel) {
            let _ = self.entered.send(());
        }
        while !*released {
            released = self.ready.wait(released).unwrap_or_else(|e| e.into_inner());
        }
        self.resumed.store(true, Ordering::Release);
    }
}
/// A diagnostic observer, never an arrival or provider capability.
#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
#[doc(hidden)]
pub struct NativeColumnLoadTestObservation {
    gate: Arc<NativeColumnLoadGate>,
}
#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
impl NativeColumnLoadTestObservation {
    pub fn entered(&self) -> bool {
        self.gate.entered_once.load(Ordering::Acquire)
    }
    pub fn resumed(&self) -> bool {
        self.gate.resumed.load(Ordering::Acquire)
    }
}
/// Owned release-on-drop receipt. Neither timeout nor observation cancels work.
#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
#[doc(hidden)]
pub struct NativeColumnLoadTestControl {
    shared: std::sync::Weak<Shared>,
    gate: Arc<NativeColumnLoadGate>,
    entered: mpsc::Receiver<()>,
    entered_seen: bool,
}
#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
impl NativeColumnLoadTestControl {
    pub fn observer(&self) -> NativeColumnLoadTestObservation {
        NativeColumnLoadTestObservation {
            gate: self.gate.clone(),
        }
    }
    pub fn wait_entered(&mut self, timeout: std::time::Duration) -> crate::Result<()> {
        if !self.entered_seen {
            self.entered
                .recv_timeout(timeout)
                .map_err(|e| format!("native column load entry: {e}"))?;
            self.entered_seen = true;
        }
        Ok(())
    }
    pub fn release(&mut self) {
        // Cancellation before entry removes only this receipt's slot. An older
        // worker resuming later cannot remove a newly armed gate.
        if let Some(shared) = self.shared.upgrade() {
            let mut slot = shared
                .native_column_load_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if slot
                .as_ref()
                .is_some_and(|gate| Arc::ptr_eq(gate, &self.gate))
            {
                drop(slot.take());
            }
        }
        self.gate.release();
    }
}
#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
impl Drop for NativeColumnLoadTestControl {
    fn drop(&mut self) {
        self.release();
    }
}
#[cfg(test)]
struct WorkGate {
    origin: NativeWorkOrigin,
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}
#[cfg(test)]
struct IdleWaitGate {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShutdownEvent {
    QueueContended,
    Published,
}
#[cfg(test)]
impl Shared {
    fn shutdown_event(&self, event: ShutdownEvent) {
        if let Some(sender) = self.shutdown_events.lock().unwrap().as_ref() {
            let _ = sender.send(event);
        }
    }
}
pub struct Streaming {
    shared: Arc<Shared>,
    light_view: Arc<dyn Fn(&Proto) -> Option<Chunk> + Send + Sync>,
    results: mpsc::Receiver<Done>,
    threads: Vec<thread::JoinHandle<()>>,
    order: u64,
    generating: FxHashSet<(i32, i32)>,
    generation_scan: GenerationScan,
    /// Proto chunks: `None` while checked out to a decoration or save job.
    protos: FxHashMap<(i32, i32), Option<Proto>>,
    decorated: FxHashSet<(i32, i32)>,
    decorating: usize,
    /// Save jobs in flight.
    saving: usize,
    /// First failed generation-storage enqueue. Terrain production halts for this streamer's
    /// lifetime; existing results, lighting and meshes still reconcile normally.
    generation_save_error: Option<String>,
    /// Complete out-of-range fresh snapshots whose handoff did not complete.
    /// No retry/reset or absolute retention quota; only pre-existing jobs drain.
    failed_finished: Vec<Arc<Chunk>>,
    /// Vanilla's decoration order around the player, and the centre and
    /// radius it was last ranked for.
    decoration_order: DecorationOrder,
    order_area: Option<((i32, i32), i32)>,
    /// What the queued jobs' priorities were computed from (`requeue`): the
    /// centre, the loading screen, the band, and the extra areas.
    queue_basis: (((i32, i32), bool, Option<Band>), Vec<((i32, i32), i32)>),
    finishing: FxHashSet<(i32, i32)>,
    decorates: bool,
    lighting: FxHashMap<(i32, i32), u64>,
    meshing: FxHashMap<(i32, i32, i32), u64>,
    /// Chunks whose light is current.
    lit: FxHashSet<(i32, i32)>,
    /// Columns whose sections have been meshed at least once.
    meshed: FxHashSet<(i32, i32)>,
    /// Dirty sections of loaded columns past the render distance, meshed once
    /// the player comes closer, and the area they were set aside for.
    waiting: FxHashSet<(i32, i32, i32)>,
    waiting_area: ((i32, i32), i32, usize),
    /// Proto chunks decoration wrote to since they were generated or saved.
    written: FxHashSet<(i32, i32)>,
    saves: Arc<dyn Saves>,
    /// Results received while `save` waited, for the next `update`.
    deferred: Vec<Done>,
    deferred_owners: Vec<(usize, WorkLease)>,
    sequence: u64,
    pub center: (i32, i32),
    pub radius: i32,
    /// `/forceload`ed chunks: kept loaded whatever the distance.
    pub forced: HashSet<(i32, i32)>,
    /// No work is scheduled while paused (title screen).
    pub paused: bool,
    /// The level loading screen is up: work goes ring by ring (`Ranges::rings`).
    pub loading: bool,
    /// Dimension flags: sky light, Nether face shading.
    pub sky_light: bool,
    pub nether_shading: bool,
    /// `BiomeManager`'s zoom seed (the hashed world seed) and `biomeBlendRadius`.
    pub biome_zoom: BiomeZoom,
    pub biome_blend: i32,
    /// The `ao` and `cutoutLeaves` options.
    pub smooth_lighting: bool,
    pub cutout_leaves: bool,
    /// Incremental relighting of block edits on lit chunks (see `light::relight`).
    light_tables: Arc<LightTables>,
    /// Applied edits by sequence, replayed onto light jobs snapshotted before them.
    light_log: Vec<(u64, (i32, i32, i32))>,
    light_edit_sequence: u64,
    /// Edit sequence at each in-flight light job's snapshot.
    light_snapshots: FxHashMap<(i32, i32), u64>,
    pub generated_total: u64,
    pub meshed_total: u64,
    /// Chunks come from a server (`insert_remote`/`forget`): nothing is
    /// generated and columns are only unloaded when the server says so.
    pub remote: bool,
    /// More areas to keep loaded (centre chunk, radius), e.g. around the
    /// players of a server. Each is a square, like vanilla's ticket levels:
    /// chunks finish out to `radius + 1` (Chebyshev) and generation reaches
    /// past that as around the player. They are generated and kept but not meshed.
    pub extra: Vec<((i32, i32), i32)>,
    /// `extra`'s squares as a grid, made again when `extra` changes.
    cover: Option<ExtraCover>,
    /// Mesh the sections for drawing (false on a server thread).
    pub mesh: bool,
    /// The pregeneration band (`set_band`).
    band: Option<Band>,
    /// The columns and the next row the band's decoration ranks were given
    /// for (`set_band`).
    band_ranked: Option<(i32, i32, i32)>,
}
/// Snapshot only after a scan found no missing candidates. Public range fields
/// and `World::chunks` can change without going through Streaming, so compare
/// their values and structural membership token rather than a frame timeout.
#[derive(Default)]
struct GenerationScan {
    exhausted: bool,
    center: (i32, i32),
    radius: i32,
    extra: Vec<((i32, i32), i32)>,
    forced: HashSet<(i32, i32)>,
    band: Option<Band>,
    chunk_revision: u64,
}
impl GenerationScan {
    fn unchanged(&self, ranges: &Ranges<'_>, world: &World) -> bool {
        self.exhausted
            && self.center == ranges.center
            && self.radius == ranges.radius
            && self.extra == ranges.extra
            && self.forced == *ranges.forced
            && self.band.as_ref() == ranges.band
            && self.chunk_revision == world.chunks.membership_revision()
    }
    fn remember(&mut self, ranges: Ranges<'_>, world: &World) {
        self.exhausted = true;
        self.center = ranges.center;
        self.radius = ranges.radius;
        self.extra.clear();
        self.extra.extend_from_slice(ranges.extra);
        self.forced.clone_from(ranges.forced);
        self.band = ranges.band.copied();
        self.chunk_revision = world.chunks.membership_revision();
    }
}
struct GenerationInputs<'a> {
    ranges: Ranges<'a>,
    world: &'a World,
    generating: &'a FxHashSet<(i32, i32)>,
    protos: &'a FxHashMap<(i32, i32), Option<Proto>>,
    finishing: &'a FxHashSet<(i32, i32)>,
    decorates: bool,
    remote: bool,
    loading: bool,
}
impl GenerationScan {
    /// None means the scan was skipped; Some preserves the original candidate
    /// ordering and bounded batch, including an empty exhausted scan.
    fn candidates(
        &mut self,
        input: GenerationInputs<'_>,
        capacity: usize,
    ) -> Option<Vec<(i64, i32, i32)>> {
        // Generation (load or proto), closest first, bounded in flight.
        let (margin, reach) = Ranges::margins(input.decorates);
        if input.remote
            || input.generating.len() >= capacity
            || self.unchanged(&input.ranges, input.world)
        {
            return None;
        }
        {
            let mut wanted = Vec::new();
            // Each area's square of columns, as one run per row. Players'
            // areas overlap (a server's players often stand together), so
            // the runs of a row are merged and each column is looked at once.
            let mut runs: Vec<(i32, i32, i32)> = Vec::new();
            for (center, radius) in std::iter::once((input.ranges.center, input.ranges.radius))
                .chain(input.ranges.extra.iter().copied())
            {
                let r = radius + margin.max(reach + 1);
                runs.extend((center.1 - r..=center.1 + r).map(|z| (z, center.0 - r, center.0 + r)));
            }
            if let Some(band) = input.ranges.band {
                let r = margin.max(reach + 1);
                runs.extend((band.z0 - r..=band.z1 + r).map(|z| (z, band.x0 - r, band.x1 + r)));
            }
            runs.sort_unstable();
            let mut runs = runs.into_iter().peekable();
            while let Some((z, x0, mut x1)) = runs.next() {
                while let Some((_, _, next_x1)) =
                    runs.next_if(|&(nz, nx0, _)| nz == z && nx0 <= x1.saturating_add(1))
                {
                    x1 = x1.max(next_x1);
                }
                for x in x0..=x1 {
                    if !input.ranges.requested(x, z, input.decorates)
                        || input.world.chunks.contains_key(&(x, z))
                        || input.generating.contains(&(x, z))
                        || input.protos.contains_key(&(x, z))
                        || input.finishing.contains(&(x, z))
                    {
                        continue;
                    }
                    wanted.push((input.ranges.priority(x, z, input.loading), x, z));
                }
            }
            // Forced chunks and their generation neighbourhood.
            let reach = margin - 1;
            for &(fx, fz) in input.ranges.forced {
                for dz in -reach..=reach {
                    for dx in -reach..=reach {
                        let (x, z) = (fx + dx, fz + dz);
                        if input.world.chunks.contains_key(&(x, z))
                            || input.generating.contains(&(x, z))
                            || input.protos.contains_key(&(x, z))
                            || input.finishing.contains(&(x, z))
                        {
                            continue;
                        }
                        wanted.push((input.ranges.priority(x, z, input.loading), x, z));
                    }
                }
            }
            if wanted.is_empty() {
                let ranges = Ranges {
                    center: input.ranges.center,
                    radius: input.ranges.radius,
                    extra: input.ranges.extra,
                    forced: input.ranges.forced,
                    cover: input.ranges.cover,
                    band: input.ranges.band,
                };
                self.remember(ranges, input.world);
            } else {
                // A bounded batch may leave candidates behind. Keep scanning
                // as capacity opens, even if no range or membership changed.
                self.exhausted = false;
            }
            wanted.sort_unstable();
            wanted.dedup();
            wanted.truncate(capacity - input.generating.len());
            Some(wanted)
        }
    }
}
pub struct WorkerContext {
    pub assets: Arc<crate::assets::Assets>,
    pub render: Arc<crate::assets::RenderAssets>,
    pub tables: Arc<MeshTables>,
    pub biomes: Arc<crate::biome::Biomes>,
    pub light: Arc<LightTables>,
}
impl Streaming {
    pub fn new<G: ChunkGenerator>(
        generator: Arc<G>,
        context: Arc<WorkerContext>,
        threads: usize,
    ) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(BinaryHeap::new()),
            ready: Condvar::new(),
            stop: AtomicBool::new(false),
            context: RwLock::new(context.clone()),
            ownership: Arc::new(Mutex::new(OwnershipBook::default())),
            returned: Mutex::new(VecDeque::new()),
            queued_owners: Mutex::new(FxHashMap::default()),
            #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
            native_column_load_gate: Mutex::new(None),
            #[cfg(test)]
            work_gate: Mutex::new(None),
            #[cfg(test)]
            idle_wait_gate: Mutex::new(None),
            #[cfg(test)]
            shutdown_events: Mutex::new(None),
        });
        let (send, results) = mpsc::channel();
        let light_tables = context.light.clone();
        let saves: Arc<dyn Saves> = generator.clone();
        let threads = (0..threads.max(1))
            .map(|i| {
                let shared = shared.clone();
                let send = send.clone();
                let generator = generator.clone();
                let light = context.light.clone();
                thread::Builder::new()
                    .name(format!("horizon-worker-{i}"))
                    .spawn(move || {
                        let mut lifetime = WorkerLifetime {
                            book: shared.ownership.clone(),
                            initialized: false,
                            stopped: false,
                        };
                        // Ticks and frames come first (horizon_threads).
                        horizon_threads::behind_server();
                        let mut worker = generator.worker();
                        lifetime.initialized();
                        // The face visibility table and its occlusion cache stay with
                        // the worker across mesh jobs, like vanilla's per-thread cache:
                        // cloning it per job copied every state and face shape.
                        let mut faces: Option<(Arc<MeshTables>, FaceVisibility)> = None;
                        loop {
                            let (job, mut owner) = {
                                let mut queue = shared.queue.lock().unwrap();
                                loop {
                                    if shared.stop.load(Ordering::Relaxed) {
                                        lifetime.stopped = true;
                                        return;
                                    }
                                    if let Some(job) = queue.pop() {
                                        let mut owner =
                                            shared.queued_owners.lock().unwrap().remove(&job.order);
                                        if let Some(owner) = &mut owner {
                                            owner.advance(WorkPhase::Worker);
                                        }
                                        break (job.job, owner);
                                    }
                                    #[cfg(test)]
                                    {
                                        let gate = shared.idle_wait_gate.lock().unwrap().take();
                                        if let Some(gate) = gate {
                                            gate.entered.send(()).unwrap();
                                            gate.release
                                                .recv_timeout(std::time::Duration::from_secs(5))
                                                .unwrap();
                                        }
                                    }
                                    queue = shared.ready.wait(queue).unwrap();
                                }
                            };
                            #[cfg(test)]
                            {
                                let gate = {
                                    let mut gate = shared.work_gate.lock().unwrap();
                                    if gate.as_ref().is_some_and(|gate| {
                                        owner
                                            .as_ref()
                                            .is_some_and(|owner| owner.origin == gate.origin)
                                    }) {
                                        gate.take()
                                    } else {
                                        None
                                    }
                                };
                                if let Some(gate) = gate {
                                    gate.entered.send(()).unwrap();
                                    gate.release
                                        .recv_timeout(std::time::Duration::from_secs(5))
                                        .unwrap();
                                }
                            }
                            #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
                            {
                                let gate = {
                                    let slot = shared.native_column_load_gate.lock().unwrap_or_else(|e| e.into_inner());
                                    slot.as_ref().filter(|gate| {
                                        owner.as_ref().is_some_and(|owner| owner.origin == NativeWorkOrigin::Generate)
                                            && matches!(&job, Job::Generate(x, z) if (*x, *z) == gate.column)
                                    }).cloned()
                                };
                                if let Some(gate) = gate {
                                    gate.wait(); // queue and slot locks are both released.
                                    let mut slot = shared.native_column_load_gate.lock().unwrap_or_else(|e| e.into_inner());
                                    if slot.as_ref().is_some_and(|current| Arc::ptr_eq(current, &gate)) { drop(slot.take()); }
                                }
                            }
                            let done = match job {
                                Job::Generate(x, z) => match generator.load(x, z) {
                                    Some(chunk) => {
                                        generator.prepare_loaded(&mut worker, &chunk);
                                        Done::Generated(chunk)
                                    }
                                    None => {
                                        let proto = generator.proto(&mut worker, x, z);
                                        let decorated = generator.is_decorated(&proto);
                                        Done::Proto(x, z, proto, decorated)
                                    }
                                },
                                Job::Decorate(x, z, mut region) => {
                                    generator.decorate(&mut worker, &mut region);
                                    Done::Decorated(x, z, region)
                                }
                                Job::Finish(proto) => Done::Generated(generator.finish(proto)),
                                Job::Save(protos) => {
                                    let refs: Vec<&Proto> = protos.iter().map(|(_, p)| p).collect();
                                    let outcome = generator.save_protos(&refs);
                                    if let Some(owner) = &owner {
                                        owner.storage(outcome.is_ok());
                                    }
                                    Done::Saved(protos, outcome)
                                }
                                Job::Light(x, z, region, seq, sky) => {
                                    Done::Lit(x, z, light_chunk(&borrow(&region), &light, sky), seq)
                                }
                                Job::Mesh(x, sy, z, region, seq, settings) => {
                                    let context = shared.context.read().unwrap().clone();
                                    let visibility = match faces.take() {
                                        Some((tables, visibility))
                                            if Arc::ptr_eq(&tables, &context.tables) =>
                                        {
                                            visibility
                                        }
                                        _ => context.tables.visibility.clone(),
                                    };
                                    let mut mesher = Mesher {
                                        nether_shading: settings.nether,
                                        zoom: settings.zoom,
                                        blend_radius: settings.blend_radius,
                                        smooth_lighting: settings.smooth_lighting,
                                        cutout_leaves: settings.cutout_leaves,
                                        assets: &context.assets,
                                        render: &context.render,
                                        tables: &context.tables,
                                        biomes: &context.biomes,
                                        visibility,
                                    };
                                    let mut mesh = mesher.mesh(&borrow(&region), sy);
                                    mesh.sort_solid();
                                    faces = Some((context.tables.clone(), mesher.visibility));
                                    Done::Meshed(x, sy, z, mesh, seq)
                                }
                            };
                            // One lock orders the native send and its receipt across workers.
                            // Receivers pair only after releasing mpsc's internal receive lock.
                            let mut returned = shared.returned.lock().unwrap();
                            let has_receipt = owner.is_some();
                            if let Some(mut lease) = owner.take() {
                                lease.advance(WorkPhase::Returned);
                                returned.push_back((done.stamp(), lease));
                            }
                            if send.send(done).is_err() {
                                if has_receipt {
                                    returned.pop_back();
                                }
                                shared
                                    .ownership
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .result_disconnected = true;
                                return;
                            }
                        }
                    })
                    .expect("spawn worker")
            })
            .collect();
        let light_view = {
            let generator = generator.clone();
            Arc::new(move |p: &Proto| generator.light_view(p))
        };
        Self {
            shared,
            light_view,
            results,
            threads,
            order: 0,
            generating: Default::default(),
            generation_scan: GenerationScan::default(),
            protos: Default::default(),
            decorated: Default::default(),
            decorating: 0,
            saving: 0,
            generation_save_error: None,
            failed_finished: Vec::new(),
            decoration_order: DecorationOrder::default(),
            order_area: None,
            queue_basis: Default::default(),
            finishing: Default::default(),
            decorates: generator.decorates(),
            lighting: Default::default(),
            meshing: Default::default(),
            lit: Default::default(),
            meshed: Default::default(),
            waiting: Default::default(),
            waiting_area: ((0, 0), 0, 0),
            written: Default::default(),
            saves,
            deferred: Vec::new(),
            deferred_owners: Vec::new(),
            sequence: 0,
            center: (0, 0),
            radius: 8,
            forced: HashSet::new(),
            paused: false,
            loading: false,
            sky_light: true,
            nether_shading: false,
            biome_zoom: BiomeZoom::new(0),
            biome_blend: 2,
            smooth_lighting: true,
            cutout_leaves: true,
            light_tables,
            light_log: Vec::new(),
            light_edit_sequence: 0,
            light_snapshots: Default::default(),
            generated_total: 0,
            meshed_total: 0,
            remote: false,
            extra: Vec::new(),
            cover: None,
            mesh: true,
            band: None,
            band_ranked: None,
        }
    }
    /// Shares generation resources, but gives distant work its own worker state.
    pub fn lod_source(&self) -> Arc<dyn LodSource> {
        self.saves.clone()
    }
    /// Borrowed observation only. This neither receives results nor certifies
    /// readiness, including when every named count is zero. It excludes storage
    /// writer ownership after an accepted callback handoff.
    pub fn observe_native_work(&self) -> NativeWorkObservation {
        let lock = self.shared.ownership.lock();
        let poisoned = lock.is_err();
        let state = lock.unwrap_or_else(|e| e.into_inner());
        NativeWorkObservation {
            origins: WORK_ORIGINS.map(|origin| (origin, state.counts[origin as usize])),
            initialized_workers: state.initialized_workers,
            worker_initialization_fault: state.worker_initialization_fault,
            worker_exit_fault: state.worker_exit_fault,
            result_disconnected: state.result_disconnected,
            receipt_mismatch: state.receipt_mismatch,
            accounting_fault: state.accounting_fault || poisoned,
            stopped: self.shared.stop.load(Ordering::Relaxed),
        }
    }
    fn received(&self, done: Done) -> ReceivedWork {
        let mut returned = self
            .shared
            .returned
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let owner = if returned
            .front()
            .is_some_and(|(stamp, _)| *stamp == done.stamp())
        {
            returned.pop_front().map(|(_, owner)| owner)
        } else {
            // Raw synthetic test completions have no receipt. A mismatched
            // native receipt stays retained rather than settling a different job.
            if !returned.is_empty() {
                self.shared
                    .ownership
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .receipt_mismatch = true;
            }
            None
        };
        ReceivedWork { done, owner }
    }
    fn take_deferred(&mut self) -> Vec<ReceivedWork> {
        let mut owners = std::mem::take(&mut self.deferred_owners)
            .into_iter()
            .peekable();
        let work = std::mem::take(&mut self.deferred)
            .into_iter()
            .enumerate()
            .map(|(index, done)| {
                let owner = if owners.peek().is_some_and(|(at, _)| *at == index) {
                    owners.next().map(|(_, owner)| owner)
                } else {
                    None
                };
                ReceivedWork { done, owner }
            })
            .collect();
        // Unexpected removed entries fault their leases on Drop.
        work
    }
    fn defer(&mut self, work: ReceivedWork) {
        if let Some(owner) = work.owner {
            self.deferred_owners.push((self.deferred.len(), owner));
        }
        self.deferred.push(work.done);
    }
    fn result_disconnected(&self) {
        self.shared
            .ownership
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .result_disconnected = true;
    }
    /// Resource packs reloaded (`LevelRenderer.allChanged`): mesh with the new
    /// models and sprites, and rebuild every loaded section. Meshes in flight
    /// were made with the old resources; their results are dropped.
    pub fn reload_resources(&mut self, world: &mut World, context: Arc<WorkerContext>) {
        *self.shared.context.write().unwrap() = context;
        self.meshing.clear();
        for &(x, z) in world.chunks.keys() {
            if self.lit.contains(&(x, z)) {
                for sy in 0..SECTIONS as i32 {
                    world.dirty_sections.insert((x, sy, z));
                }
            }
        }
    }
    /// A column received from a server, with the light it sent
    /// (`ClientPacketListener.handleLevelChunkWithLight`): nothing is
    /// relit, and it and its neighbours remesh (`enableChunkLight`).
    pub fn insert_remote(&mut self, world: &mut World, chunk: Chunk) {
        let key = (chunk.x, chunk.z);
        self.lighting.remove(&key);
        self.light_snapshots.remove(&key);
        world.chunks.insert(key, Arc::new(chunk));
        self.lit.insert(key);
        self.generated_total += 1;
        for dz in -1..=1 {
            for dx in -1..=1 {
                for sy in 0..SECTIONS as i32 {
                    world.dirty_sections.insert((key.0 + dx, sy, key.1 + dz));
                }
            }
        }
    }
    /// A server's light update for a resident column: the sections whose
    /// light changed and their neighbours remesh
    /// (`setSectionDirtyWithNeighbors`).
    pub fn light_updated(world: &mut World, x: i32, z: i32, sections: &[usize]) {
        for &sy in sections {
            for dz in -1..=1 {
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        let s = sy as i32 + dy;
                        if (0..SECTIONS as i32).contains(&s) {
                            world.dirty_sections.insert((x + dx, s, z + dz));
                        }
                    }
                }
            }
        }
    }
    /// A column the server unloaded (ClientboundForgetLevelChunkPacket).
    pub fn forget(&mut self, world: &mut World, x: i32, z: i32) -> Option<Arc<Chunk>> {
        self.lit.remove(&(x, z));
        self.meshed.remove(&(x, z));
        self.lighting.remove(&(x, z));
        // By key, not by a pass over every pending section: a moved or
        // shrunk storage forgets thousands of columns in one frame.
        for sy in 0..SECTIONS as i32 {
            self.meshing.remove(&(x, sy, z));
            self.waiting.remove(&(x, sy, z));
        }
        world.dirty_sections.take_column(x, z);
        world.chunks.remove(&(x, z))
    }
    /// Queue a job for the column `at`, nearest first: its distance (`distance`)
    /// times four plus `bias`, which orders a column's jobs. A job without a
    /// column (a save) goes ahead of every other.
    fn submit(&mut self, at: Option<(i32, i32)>, bias: i64, job: Job) {
        let origin = match &job {
            Job::Generate(..) => NativeWorkOrigin::Generate,
            Job::Decorate(..) => NativeWorkOrigin::Decorate,
            Job::Finish(..) => NativeWorkOrigin::Finish,
            Job::Light(..) => NativeWorkOrigin::Light,
            Job::Mesh(..) => NativeWorkOrigin::Mesh,
            Job::Save(..) => NativeWorkOrigin::Save,
        };
        let owner = WorkLease::new(&self.shared.ownership, origin, WorkPhase::Queued);
        self.order += 1;
        let priority = at.map_or(i64::MIN, |(x, z)| self.distance(x, z) * 4 + bias);
        let mut queue = self.shared.queue.lock().unwrap();
        if self
            .shared
            .queued_owners
            .lock()
            .unwrap()
            .insert(self.order, owner)
            .is_some()
        {
            self.shared
                .ownership
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .accounting_fault = true;
        }
        queue.push(Queued {
            priority,
            at,
            bias,
            order: self.order,
            job,
        });
        drop(queue);
        self.shared.ready.notify_one();
    }
    /// Rank the queued jobs again when what their priorities were computed
    /// from changed: the centre, the extra areas, the band, or the loading
    /// screen (ring order, `Ranges::rings`, whose values are far above the
    /// distances used after it). Vanilla's chunk task queue follows the
    /// current ticket levels in the same way. Without it, generation queued
    /// while the loading screen was up waited behind every job queued after
    /// it, and with it the decoration of the chunks around the player: at
    /// render distance 32 nothing was decorated until every proto chunk had
    /// been generated.
    fn requeue(&mut self) {
        let basis = (self.center, self.loading, self.band);
        if self.queue_basis.0 == basis && self.queue_basis.1 == self.extra {
            return;
        }
        self.queue_basis = (basis, self.extra.clone());
        let mut queue = self.shared.queue.lock().unwrap();
        rank_again(&mut queue, |x, z| self.distance(x, z));
    }
    fn receive_decorated(&mut self, x: i32, z: i32, region: [Proto; 9]) {
        self.generation_scan.exhausted = false;
        self.decorating -= 1;
        self.decorated.insert((x, z));
        for (i, proto) in region.into_iter().enumerate() {
            let key = (x + (i % 3) as i32 - 1, z + (i / 3) as i32 - 1);
            self.protos.insert(key, Some(proto));
            self.written.insert(key);
        }
    }
    /// Saved proto chunks come back; those now out of range are dropped by
    /// the next `update`.
    fn receive_saved(&mut self, protos: Vec<((i32, i32), Proto)>, outcome: Result<(), String>) {
        self.generation_scan.exhausted = false;
        self.saving -= 1;
        for (key, proto) in protos {
            self.protos.insert(key, Some(proto));
            if outcome.is_err() {
                self.written.insert(key);
            }
        }
        if let Err(error) = outcome {
            self.fail_generation_save(error);
        }
    }
    fn fail_generation_save(&mut self, error: String) {
        if self.generation_save_error.is_none() {
            log::error!("Horizon: generation save failed; terrain streaming halted: {error}");
            self.generation_save_error = Some(error);
        }
    }
    /// No reset/retry policy: terrain production stays halted for this lifetime.
    /// Retained ownership needs explicit failure handling, not a full save claim.
    pub fn generation_save_error(&self) -> Option<&str> {
        self.generation_save_error.as_deref()
    }
    fn generation_save_result(&self) -> crate::Result<()> {
        match &self.generation_save_error {
            Some(error) => Err(error.clone().into()),
            None => Ok(()),
        }
    }
    /// Check out the proto chunks at `keys` and save them on a worker.
    fn save_checked_out(&mut self, keys: impl IntoIterator<Item = (i32, i32)>) {
        if self.generation_save_error.is_some() {
            return;
        }
        let protos: Vec<((i32, i32), Proto)> = keys
            .into_iter()
            .filter_map(|k| Some((k, self.protos.get_mut(&k)?.take()?)))
            .collect();
        if !protos.is_empty() {
            self.saving += 1;
            // Ahead of every other job: a column generated again meanwhile
            // would not find them saved.
            self.submit(None, 0, Job::Save(protos));
        }
    }
    fn receive_generated(&mut self, mut chunk: Chunk, world: &mut World) {
        self.generation_scan.exhausted = false;
        let key = (chunk.x, chunk.z);
        self.generating.remove(&key);
        self.finishing.remove(&key);
        self.decorated.insert(key);
        // A column that finished after the player moved away stays outside
        // World, including its block entities and saved entities. A fresh one
        // is handed to storage intact: decoration cannot be regenerated safely.
        // Failed handoffs (and subsequent old results after fault) stay retained
        // as complete snapshots, without activating entities or faking residency.
        if !self.ranges().chunk_kept(key.0, key.1, self.decorates) {
            if chunk.fresh {
                let chunk = Arc::new(chunk);
                if self.generation_save_error.is_some() {
                    self.failed_finished.push(chunk);
                } else {
                    let owner = WorkLease::new(
                        &self.shared.ownership,
                        NativeWorkOrigin::SaveFinishedCallback,
                        WorkPhase::Reconciling,
                    );
                    let outcome = self.saves.save_finished(&chunk);
                    owner.storage(outcome.is_ok());
                    match outcome {
                        Ok(()) => owner.settle(),
                        Err(error) => {
                            self.failed_finished.push(chunk);
                            self.fail_generation_save(error);
                            // Failed or partial storage enqueue stays a named fault.
                        }
                    }
                }
            }
            return;
        }
        // Generated columns are saved like edited ones, as vanilla saves
        // every chunk it generates.
        if chunk.fresh {
            world.modified.insert(key);
        }
        world
            .block_entities
            .load_chunk(std::mem::take(&mut chunk.block_entities));
        world
            .block_names
            .extend(std::mem::take(&mut chunk.block_names));
        let entities = std::mem::take(&mut chunk.entities);
        if !entities.is_empty() {
            world.loaded_entities.push(((chunk.x, chunk.z), entities));
        }
        self.generated_total += 1;
        // A chunk saved with its light needs no light job; its unlit
        // neighbours still light themselves from it.
        let loaded = chunk.light_loaded;
        world.insert_streamed_chunk(chunk);
        if loaded {
            self.lit.insert(key);
            for sy in 0..SECTIONS as i32 {
                world.dirty_sections.insert((key.0, sy, key.1));
            }
        }
        for dz in -1..=1 {
            for dx in -1..=1 {
                let n = (key.0 + dx, key.1 + dz);
                if !loaded || world.chunks.get(&n).is_some_and(|c| !c.light_loaded) {
                    world.dirty_light.insert(n);
                }
            }
        }
    }
    /// The order jobs around the player run in, lowest first.
    fn distance(&self, x: i32, z: i32) -> i64 {
        self.ranges().priority(x, z, self.loading)
    }
    /// The 3x3 a light job reads: the finished columns, and for a neighbour
    /// that will not finish (past the area), its decorated proto chunk, as
    /// vanilla lights its last FULL ring against neighbours at FEATURES.
    fn light_region(&self, world: &World, x: i32, z: i32) -> Option<Box<Region>> {
        let mut region: Region = Default::default();
        for dz in 0..3 {
            for dx in 0..3 {
                let n = (x + dx as i32 - 1, z + dz as i32 - 1);
                region[dz][dx] = Some(match world.chunks.get(&n) {
                    Some(chunk) => chunk.clone(),
                    None if !self.in_range(n.0, n.1, 1) && self.decorated.contains(&n) => {
                        let proto = self.protos.get(&n)?.as_ref()?;
                        Arc::new((self.light_view)(proto)?)
                    }
                    None => return None,
                });
            }
        }
        Some(Box::new(region))
    }
    fn region(world: &World, x: i32, z: i32) -> Option<Box<Region>> {
        let mut region: Region = Default::default();
        for dz in 0..3 {
            for dx in 0..3 {
                region[dz][dx] = Some(
                    world
                        .chunks
                        .get(&(x + dx as i32 - 1, z + dz as i32 - 1))?
                        .clone(),
                );
            }
        }
        Some(Box::new(region))
    }
    pub fn is_lit(&self, x: i32, z: i32) -> bool {
        self.lit.contains(&(x, z))
    }
    /// `ChunkMap.getLatestStatus` for the loading screen's chunk map: the
    /// furthest step this column has reached, `None` before it is asked for.
    pub fn latest_status(
        &self,
        world: &World,
        x: i32,
        z: i32,
    ) -> Option<crate::level_loading::Status> {
        use crate::level_loading::Status;
        let key = (x, z);
        Some(if self.lit.contains(&key) {
            Status::Full
        } else if world.chunks.contains_key(&key) {
            Status::InitializeLight
        } else if self.decorated.contains(&key) || self.finishing.contains(&key) {
            Status::Features
        } else if self.protos.contains_key(&key) {
            Status::Terrain
        } else if self.generating.contains(&key) {
            Status::Empty
        } else {
            return None;
        })
    }
    pub fn is_meshing(&self, section: (i32, i32, i32)) -> bool {
        self.meshing.contains_key(&section)
    }
    pub fn pending(&self) -> usize {
        self.shared.queue.lock().unwrap().len()
    }
    /// Whether any chunk is queued or on its way from generation to a mesh.
    pub fn busy(&self) -> bool {
        !(self.generating.is_empty()
            && self.decorating == 0
            && self.finishing.is_empty()
            && self.lighting.is_empty()
            && self.meshing.is_empty()
            && self.pending() == 0)
    }
    /// How much work each stage holds: queued jobs, then what is in flight or
    /// waiting at each step from generation to meshing.
    pub fn stages(&self) -> [(&'static str, usize); 7] {
        [
            ("queued", self.pending()),
            ("generating", self.generating.len()),
            (
                "protos",
                self.protos.values().filter(|p| p.is_some()).count(),
            ),
            ("decorating", self.decorating),
            ("finishing", self.finishing.len()),
            ("lighting", self.lighting.len()),
            ("meshing", self.meshing.len()),
        ]
    }
    /// Advance streaming: collect finished work, then schedule more.
    /// Returns meshes ready for upload and columns that were unloaded.
    pub fn update(
        &mut self,
        world: &mut World,
    ) -> (Vec<((i32, i32, i32), SectionMesh)>, Vec<Arc<Chunk>>) {
        let mut uploads = Vec::new();
        if self.paused {
            return (uploads, Vec::new());
        }
        ExtraCover::refresh(&mut self.cover, &self.extra);
        self.requeue();
        self.relight_edits(world);
        let deferred = self.take_deferred();
        let mut received = Vec::new();
        loop {
            match self.results.try_recv() {
                Ok(done) => received.push(self.received(done)),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.result_disconnected();
                    break;
                }
            }
        }
        for ReceivedWork { done, mut owner } in deferred.into_iter().chain(received) {
            if let Some(owner) = &mut owner {
                owner.advance(WorkPhase::Reconciling);
            }
            let _reconciliation = Reconciliation(owner);
            match done {
                Done::Proto(x, z, proto, decorated) => {
                    self.generation_scan.exhausted = false;
                    self.generating.remove(&(x, z));
                    if self.in_range(x, z, 4) || self.near_finished(x, z, 2) {
                        self.protos.insert((x, z), Some(proto));
                        if decorated || !self.decorates {
                            self.decorated.insert((x, z));
                        }
                    }
                }
                Done::Decorated(x, z, region) => self.receive_decorated(x, z, *region),
                Done::Generated(chunk) => self.receive_generated(chunk, world),
                Done::Saved(protos, outcome) => self.receive_saved(protos, outcome),
                Done::Lit(x, z, sections, seq) => {
                    if self.lighting.get(&(x, z)) != Some(&seq) {
                        continue;
                    }
                    self.lighting.remove(&(x, z));
                    let snapshot = self.light_snapshots.remove(&(x, z)).unwrap_or(u64::MAX);
                    if let Some(chunk) = world.chunks.get_mut(&(x, z)) {
                        let chunk = Arc::make_mut(chunk);
                        // Keep blocks edited since the snapshot; take only light.
                        // A chunk relit (a neighbour loaded or changed) remeshes
                        // only around the light that changed, as vanilla's light
                        // updates dirty only their sections (#270).
                        let first = self.lit.insert((x, z));
                        let mut changed = LightChanges::default();
                        if first {
                            apply_lighting(chunk, sections);
                        } else {
                            apply_relighting(chunk, sections, &mut changed);
                        }
                        // Saved unlit (at the edge of what was loaded), or its light
                        // changed: saved again.
                        if (first && !chunk.light_loaded) || !changed.is_empty() {
                            world.modified.insert((x, z));
                        }
                        chunk.lit = true;
                        if first {
                            for dz in -1..=1 {
                                for dx in -1..=1 {
                                    for sy in 0..SECTIONS as i32 {
                                        world.dirty_sections.insert((x + dx, sy, z + dz));
                                    }
                                }
                            }
                        } else {
                            Self::mark_light_changes(world, &changed);
                        }
                        self.replay_light_edits(world, x, z, snapshot);
                    }
                }
                Done::Meshed(x, sy, z, mesh, seq) => {
                    if self.meshing.get(&(x, sy, z)) != Some(&seq) {
                        continue;
                    }
                    // Its column unloaded while it was meshed: nothing would
                    // ever remove the uploaded section.
                    if !world.chunks.contains_key(&(x, z)) {
                        self.meshing.remove(&(x, sy, z));
                        continue;
                    }
                    self.meshing.remove(&(x, sy, z));
                    self.meshed_total += 1;
                    uploads.push(((x, sy, z), mesh));
                }
            }
        }
        // Unload far columns.
        let far: Vec<_> = world
            .chunks
            .keys()
            .copied()
            .filter(|&(x, z)| !self.remote && self.ranges().unloaded(x, z, self.decorates))
            .collect();
        for key in &far {
            self.decorated.remove(key);
        }
        let mut removed = Vec::with_capacity(far.len());
        for key in &far {
            if let Some(chunk) = world.remove_streamed_chunk(key) {
                removed.push(chunk);
            }
            self.lit.remove(key);
            self.meshed.remove(key);
            self.lighting.remove(key);
            self.light_snapshots.remove(key);
        }
        if !far.is_empty() {
            let far: HashSet<(i32, i32)> = far.iter().copied().collect();
            self.meshing.retain(|k, _| !far.contains(&(k.0, k.2)));
            self.waiting.retain(|k| !far.contains(&(k.0, k.2)));
            world
                .dirty_sections
                .retain_columns(|x, z| !far.contains(&(x, z)));
        }
        self.produce_terrain(world);
        // Lighting once all neighbours exist.
        let light: Vec<_> = world.dirty_light.iter().copied().collect();
        for (x, z) in light {
            if !world.chunks.contains_key(&(x, z)) {
                world.dirty_light.remove(&(x, z));
                continue;
            }
            let Some(region) = self.light_region(world, x, z) else {
                continue;
            };
            world.dirty_light.remove(&(x, z));
            self.sequence += 1;
            self.lighting.insert((x, z), self.sequence);
            self.light_snapshots
                .insert((x, z), self.light_edit_sequence);
            self.submit(
                Some((x, z)),
                1,
                Job::Light(x, z, region, self.sequence, self.sky_light),
            );
        }
        // A server draws nothing: its sections are never meshed.
        if !self.mesh {
            world.dirty_sections.clear();
            return (uploads, removed);
        }
        // Meshing once the column and its neighbours are lit, and the column
        // is within the render distance (`in_view_distance`, the columns
        // vanilla draws). Sections set aside past it come back
        // when the player moves: a column that loads with its light gets no
        // other request (#228).
        let area = (self.center, self.radius, self.forced.len());
        if self.waiting_area != area {
            self.waiting_area = area;
            let ranges = Ranges {
                center: self.center,
                radius: self.radius,
                extra: &self.extra,
                forced: &self.forced,
                cover: ExtraCover::matching(&self.cover, &self.extra),
                band: self.band.as_ref(),
            };
            self.waiting.retain(|&(x, sy, z)| {
                let back = ranges.in_view(x, z);
                if back {
                    world.dirty_sections.insert((x, sy, z));
                }
                !back
            });
        }
        // Column by column: the checks below hold for all of a column's
        // sections, and columns waiting for light cost one look per frame.
        let columns: Vec<_> = world.dirty_sections.columns().collect();
        for ((x, z), sections) in columns {
            if !world.chunks.contains_key(&(x, z)) {
                world.dirty_sections.take_column(x, z);
                continue;
            }
            if !self.ranges().in_view(x, z) {
                world.dirty_sections.take_column(x, z);
                for sy in section_indexes(sections) {
                    self.waiting.insert((x, sy, z));
                }
                continue;
            }
            // A neighbour being lit dirties this column again when its light
            // job finishes (new chunks relight their eight neighbours), so
            // wait for it rather than mesh once per neighbour. Columns still
            // in `dirty_light` here can't be lit yet (a neighbour of theirs is
            // missing): waiting for them could last until the player comes
            // back.
            let ready = (-1..=1).all(|dz| {
                (-1..=1).all(|dx| {
                    let n = (x + dx, z + dz);
                    self.lit.contains(&n) && !self.lighting.contains_key(&n)
                })
            });
            if !ready {
                continue;
            }
            let Some(region) = Self::region(world, x, z) else {
                continue;
            };
            world.dirty_sections.take_column(x, z);
            for sy in section_indexes(sections) {
                let first = self.meshed.insert((x, z));
                self.sequence += 1;
                self.meshing.insert((x, sy, z), self.sequence);
                self.submit(
                    Some((x, z)),
                    if first { 2 } else { 0 },
                    Job::Mesh(
                        x,
                        sy,
                        z,
                        region.clone(),
                        self.sequence,
                        MeshSettings {
                            nether: self.nether_shading,
                            zoom: self.biome_zoom,
                            blend_radius: self.biome_blend,
                            smooth_lighting: self.smooth_lighting,
                            cutout_leaves: self.cutout_leaves,
                        },
                    ),
                );
            }
        }
        (uploads, removed)
    }
    // A proto failure halts new producers before any failed snapshot can be
    // evicted, checked out, finalized or borrowed by neighboring decoration.
    fn produce_terrain(&mut self, world: &World) {
        if self.generation_save_error.is_some() {
            return;
        }
        // Drop far proto chunks that are not checked out.
        let (center, r4) = (self.center, i64::from(self.radius + 4));
        let ranges = self.ranges();
        let far: Vec<(i32, i32)> = self
            .protos
            .iter()
            .filter(|&(&(x, z), p)| {
                p.is_some()
                    && i64::from(x - center.0).pow(2) + i64::from(z - center.1).pow(2) > r4 * r4
                    && !ranges.in_extra(x, z, 4)
                    && !ranges.in_band(x, z, 4)
                    && !ranges
                        .forced
                        .iter()
                        .any(|f| (x - f.0).abs() <= 3 && (z - f.1).abs() <= 3)
            })
            .map(|(k, _)| *k)
            .collect();
        if !far.is_empty() {
            self.generation_scan.exhausted = false;
        }
        // Those decoration wrote to are saved first (see
        // `ChunkGenerator::save_protos`), and dropped once back.
        let mut written = Vec::new();
        for key in far {
            if self.written.remove(&key) {
                written.push(key);
            } else {
                self.protos.remove(&key);
            }
        }
        self.save_checked_out(written);
        let (protos, finishing) = (&self.protos, &self.finishing);
        self.decorated.retain(|k| {
            protos.contains_key(k) || finishing.contains(k) || world.chunks.contains_key(k)
        });
        // Finalize chunks whose neighbourhood is decorated.
        let candidates: Vec<(i32, i32)> = self.protos.keys().copied().collect();
        for (x, z) in candidates {
            if world.chunks.contains_key(&(x, z))
                || self.finishing.contains(&(x, z))
                || !self.in_range(x, z, 1)
            {
                continue;
            }
            let ready = (-1..=1).all(|dz| {
                (-1..=1).all(|dx| {
                    self.decorated.contains(&(x + dx, z + dz))
                        || world.chunks.contains_key(&(x + dx, z + dz))
                })
            });
            if ready && let Some(Some(_)) = self.protos.get(&(x, z)) {
                let proto = self
                    .protos
                    .remove(&(x, z))
                    .flatten()
                    .expect("present proto");
                self.finishing.insert((x, z));
                self.written.remove(&(x, z));
                self.submit(Some((x, z)), 0, Job::Finish(proto));
            }
        }
        // Decorate chunks whose 3x3 proto neighbourhood is available, in
        // vanilla's order: decoration is order dependent where write regions
        // overlap (see `DecorationOrder`).
        let workers = self.threads.len();
        if self.decorates && self.order_area != Some((self.center, self.radius)) {
            let (decorated, chunks) = (&self.decorated, &world.chunks);
            // The render distance is the server view distance in singleplayer.
            self.decoration_order
                .reassign(self.center, self.radius, |x, z| {
                    decorated.contains(&(x, z)) || chunks.contains_key(&(x, z))
                });
            self.order_area = Some((self.center, self.radius));
            let ranges = Ranges {
                center: self.center,
                radius: self.radius,
                extra: &self.extra,
                forced: &self.forced,
                cover: ExtraCover::matching(&self.cover, &self.extra),
                band: self.band.as_ref(),
            };
            let (protos, generating) = (&self.protos, &self.generating);
            self.decoration_order.retain(|x, z| {
                ranges.in_range(x, z, 4)
                    || ranges.near_finished(x, z, 2)
                    || protos.contains_key(&(x, z))
                    || generating.contains(&(x, z))
            });
        }
        if self.decorates && self.decorating < workers * 2 {
            let wants = |s: &Self, x: i32, z: i32| {
                !s.decorated.contains(&(x, z))
                    && !world.chunks.contains_key(&(x, z))
                    && (s.in_range(x, z, 2) || s.near_finished(x, z, 1))
            };
            // A wanted chunk holds back higher-ranked ones only if it can be
            // decorated: its 3x3 neighbourhood is (or will be) generated as
            // protos, none of them already final.
            let waits_for = |s: &Self, x: i32, z: i32| {
                wants(s, x, z)
                    && (-1..=1).all(|dz| {
                        (-1..=1).all(|dx| {
                            let n = (x + dx, z + dz);
                            !world.chunks.contains_key(&n)
                                && (s.protos.contains_key(&n)
                                    || s.generating.contains(&n)
                                    || s.ranges().requested(n.0, n.1, true))
                        })
                    })
            };
            let keys: Vec<(i32, i32)> = self
                .protos
                .iter()
                .filter(|(k, p)| p.is_some() && wants(self, k.0, k.1))
                .map(|(k, _)| *k)
                .collect();
            let mut wanted: Vec<(u64, i64, i32, i32)> = keys
                .into_iter()
                .map(|(x, z)| (0, self.distance(x, z), x, z))
                .collect();
            // Chunks no ticket reached (forced and extra areas) go last, nearest first.
            wanted.sort_unstable_by_key(|w| w.1);
            for w in &mut wanted {
                w.0 = self.decoration_order.rank_or_append(w.2, w.3);
            }
            wanted.sort_unstable();
            for (_, _, x, z) in wanted {
                if self.decorating >= workers * 2 {
                    break;
                }
                if self
                    .decoration_order
                    .blocked(x, z, |nx, nz| waits_for(self, nx, nz))
                {
                    continue;
                }
                let keys: Vec<(i32, i32)> =
                    (0..9).map(|i| (x + (i % 3) - 1, z + (i / 3) - 1)).collect();
                // A final neighbour cannot receive writes: skip decoration. Only
                // worlds saved before proto chunks were saved get here (their
                // final chunks' neighbours are generated again).
                if keys.iter().any(|k| world.chunks.contains_key(k)) {
                    self.decorated.insert((x, z));
                    continue;
                }
                if !keys
                    .iter()
                    .all(|k| matches!(self.protos.get(k), Some(Some(_))))
                {
                    continue;
                }
                let region: Vec<Proto> = keys
                    .iter()
                    .map(|k| {
                        self.protos
                            .insert(*k, None)
                            .flatten()
                            .expect("present proto")
                    })
                    .collect();
                let region: Box<[Proto; 9]> = match region.try_into() {
                    Ok(r) => Box::new(r),
                    Err(_) => continue,
                };
                self.decorating += 1;
                self.submit(Some((x, z)), 1, Job::Decorate(x, z, region));
            }
        }
        self.schedule_generation(world, workers);
    }
    fn schedule_generation(&mut self, world: &World, workers: usize) {
        if self.generation_save_error.is_some() {
            return;
        }
        let input = GenerationInputs {
            ranges: Ranges {
                center: self.center,
                radius: self.radius,
                extra: &self.extra,
                forced: &self.forced,
                cover: ExtraCover::matching(&self.cover, &self.extra),
                band: self.band.as_ref(),
            },
            world,
            generating: &self.generating,
            protos: &self.protos,
            finishing: &self.finishing,
            decorates: self.decorates,
            remote: self.remote,
            loading: self.loading,
        };
        if let Some(wanted) = self.generation_scan.candidates(input, workers * 3) {
            for (_, x, z) in wanted {
                self.generating.insert((x, z));
                self.submit(Some((x, z)), 0, Job::Generate(x, z));
            }
        }
    }
    /// Relight block edits on lit chunks in place (vanilla's incremental light
    /// engine) instead of recomputing the 3x3 chunks around each edit.
    fn relight_edits(&mut self, world: &mut World) {
        if world.light_edits.is_empty() {
            return;
        }
        let edits = std::mem::take(&mut world.light_edits);
        let positions: Vec<(i32, i32, i32)> = edits
            .iter()
            .filter(|(_, old, new)| self.light_tables.differs(*old, *new))
            .map(|(p, _, _)| *p)
            .collect();
        if positions.is_empty() {
            return;
        }
        let mut changed = LightChanges::default();
        crate::light::relight(
            &mut world.chunks,
            &self.light_tables,
            self.sky_light,
            &positions,
            false,
            &mut changed,
        );
        Self::mark_light_changes(world, &changed);
        if !self.light_snapshots.is_empty() {
            for p in positions {
                self.light_edit_sequence += 1;
                self.light_log.push((self.light_edit_sequence, p));
            }
        }
    }
    /// A light job computed from blocks older than some edits: redo those
    /// edits on top of its result.
    fn replay_light_edits(&mut self, world: &mut World, x: i32, z: i32, snapshot: u64) {
        let positions: Vec<(i32, i32, i32)> = self
            .light_log
            .iter()
            .filter(|(seq, p)| {
                *seq > snapshot && ((p.0 >> 4) - x).abs() <= 1 && ((p.2 >> 4) - z).abs() <= 1
            })
            .map(|(_, p)| *p)
            .collect();
        if !positions.is_empty() {
            let mut changed = LightChanges::default();
            crate::light::relight(
                &mut world.chunks,
                &self.light_tables,
                self.sky_light,
                &positions,
                true,
                &mut changed,
            );
            Self::mark_light_changes(world, &changed);
        }
        let oldest = self.light_snapshots.values().copied().min();
        match oldest {
            Some(oldest) => self.light_log.retain(|(seq, _)| *seq > oldest),
            None => self.light_log.clear(),
        }
    }
    /// Sections whose meshes sample a changed light value (faces and corners
    /// read light from neighbouring blocks).
    fn mark_light_changes(world: &mut World, changed: &LightChanges) {
        if let Some(log) = &mut world.light_log {
            for (x, sy, z) in changed.sections() {
                *log.entry((x, z)).or_default() |= 1 << sy;
            }
        }
        changed.dirty_sections(|section| {
            world.dirty_sections.insert(section);
        });
    }
    fn ranges(&self) -> Ranges<'_> {
        Ranges {
            center: self.center,
            radius: self.radius,
            extra: &self.extra,
            forced: &self.forced,
            cover: ExtraCover::matching(&self.cover, &self.extra),
            band: self.band.as_ref(),
        }
    }
    fn near_finished(&self, x: i32, z: i32, reach: i32) -> bool {
        self.ranges().near_finished(x, z, reach)
    }
    /// Save what generation holds outside the world: the proto chunks
    /// decoration wrote to. With `wait`, the decorations, columns and saves
    /// in flight are waited for first (their proto chunks are out in the
    /// jobs) and the protos are saved here, before the world's chunks;
    /// otherwise a worker saves them (vanilla's autosave hands its chunks to
    /// the IOWorker too), and those out in jobs are saved later, as they
    /// unload or at the next save.
    pub fn save(&mut self, world: &mut World, wait: bool) -> crate::Result<()> {
        if !wait {
            self.reconcile_ready_save_results(world);
            self.generation_save_result()?;
            let written: Vec<(i32, i32)> = self.written.drain().collect();
            self.save_checked_out(written);
            return Ok(());
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        self.save_until(world, deadline)
    }
    // The deadline bounds waiting for the tracked decoration/finish/save jobs.
    // Plain generation and non-generation storage owners are separate.
    fn save_until(&mut self, world: &mut World, deadline: std::time::Instant) -> crate::Result<()> {
        self.reconcile_ready_save_results(world);
        self.generation_save_result()?;
        while self.save_in_flight() {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.results.recv_timeout(left) {
                Ok(done) => {
                    self.receive_save_result(self.received(done), world);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(
                        "chunk generation did not finish before saving: deadline exceeded".into(),
                    );
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.result_disconnected();
                    return Err(
                        "chunk generation workers disconnected before saving finished".into(),
                    );
                }
            }
            self.generation_save_result()?;
            if self.save_in_flight() && std::time::Instant::now() >= deadline {
                return Err(
                    "chunk generation did not finish before saving: deadline exceeded".into(),
                );
            }
        }
        let protos: Vec<&Proto> = self
            .written
            .iter()
            .filter_map(|k| self.protos.get(k)?.as_ref())
            .collect();
        let outcome = if protos.is_empty() {
            Ok(())
        } else {
            let owner = WorkLease::new(
                &self.shared.ownership,
                NativeWorkOrigin::SaveProtosCallback,
                WorkPhase::Reconciling,
            );
            let outcome = self.saves.save_protos(&protos);
            owner.storage(outcome.is_ok());
            if outcome.is_ok() {
                owner.settle();
            }
            outcome
        };
        drop(protos);
        if let Err(error) = outcome {
            self.fail_generation_save(error);
            return self.generation_save_result();
        }
        self.written.clear();
        Ok(())
    }
    fn save_in_flight(&self) -> bool {
        self.decorating > 0 || !self.finishing.is_empty() || self.saving > 0
    }
    /// Close this streamer's native write producers without scheduling more work,
    /// then poll the fixture's storage writers under the same exclusive borrow.
    ///
    /// This is only a prerequisite experiment for inspected built-in generators.
    /// The caller must exclusively own the Game, account for every participating
    /// SaveDir, and exclude external producers. Arbitrary generator callbacks do
    /// not acquire that premise through this method. No capture admission or
    /// transferable readiness receipt follows from success.
    ///
    /// The deadline bounds deliberate waits, not callbacks, mutex acquisition,
    /// operating-system I/O, or destruction. The final observation does not
    /// promise future worker health after the borrow ends.
    pub fn native_write_fence_until(
        &mut self,
        world: &mut World,
        deadline: std::time::Instant,
        mut writers_finished: impl FnMut() -> crate::Result<bool>,
    ) -> crate::Result<()> {
        loop {
            self.reconcile_native_fence_results(world);
            if self.native_fence_producers_closed()? {
                break;
            }
            Self::native_fence_wait(deadline)?;
        }
        // All checked-out producers have returned. The existing synchronous
        // proto contract retains written markers on failed/partial handoff.
        self.save_until(world, deadline)?;
        loop {
            Self::native_fence_deadline(deadline)?;
            if writers_finished()? {
                // A worker may fault while the writer is being polled. This
                // must be a fresh validation AFTER the final writer poll.
                self.reconcile_native_fence_results(world);
                if !self.native_fence_producers_closed()? || !self.written.is_empty() {
                    return Err("native write fence changed during writer polling".into());
                }
                Self::native_fence_deadline(deadline)?;
                return Ok(());
            }
            // Fail early on faults without allowing any owner-side scheduling.
            self.native_fence_producers_closed()?;
            Self::native_fence_wait(deadline)?;
        }
    }
    fn native_fence_deadline(deadline: std::time::Instant) -> crate::Result<()> {
        if std::time::Instant::now() >= deadline {
            return Err("native write fence deadline exceeded".into());
        }
        Ok(())
    }
    fn native_fence_wait(deadline: std::time::Instant) -> crate::Result<()> {
        Self::native_fence_deadline(deadline)?;
        thread::sleep(
            deadline
                .saturating_duration_since(std::time::Instant::now())
                .min(std::time::Duration::from_millis(1)),
        );
        Ok(())
    }
    fn native_fence_producers_closed(&self) -> crate::Result<bool> {
        self.generation_save_result()?;
        let observation = self.observe_native_work();
        if observation.accounting_fault
            || observation.receipt_mismatch
            || observation.worker_initialization_fault
            || observation.worker_exit_fault
            || observation.result_disconnected
            || observation.stopped
        {
            return Err(format!("native write fence ownership fault: {observation:?}").into());
        }
        let mut closed = observation.initialized_workers == self.threads.len() as u64;
        for (origin, counts) in observation.origins {
            let total = [
                counts.queued,
                counts.worker_held,
                counts.returned,
                counts.reconciling,
                counts.disposed,
                counts.unresolved,
            ]
            .into_iter()
            .try_fold(0u64, u64::checked_add);
            if total != Some(counts.submitted)
                || counts.unresolved != 0
                || counts.storage_errors != 0
            {
                return Err(
                    format!("native write fence inconsistent {origin:?}: {counts:?}").into(),
                );
            }
            if !matches!(origin, NativeWorkOrigin::Light | NativeWorkOrigin::Mesh) {
                closed &= counts.queued == 0
                    && counts.worker_held == 0
                    && counts.returned == 0
                    && counts.reconciling == 0;
            }
        }
        Ok(closed
            && self.generating.is_empty()
            && !self.save_in_flight()
            && self.protos.values().all(Option::is_some)
            && self.failed_finished.is_empty())
    }
    fn reconcile_native_fence_results(&mut self, world: &mut World) {
        let mut ready = self.take_deferred();
        loop {
            match self.results.try_recv() {
                Ok(done) => ready.push(self.received(done)),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.result_disconnected();
                    break;
                }
            }
        }
        for work in ready {
            if matches!(&work.done, Done::Proto(..)) {
                let ReceivedWork { done, mut owner } = work;
                if let Some(owner) = &mut owner {
                    owner.advance(WorkPhase::Reconciling);
                }
                let _reconciliation = Reconciliation(owner);
                let Done::Proto(x, z, proto, decorated) = done else {
                    unreachable!()
                };
                self.generation_scan.exhausted = false;
                self.generating.remove(&(x, z));
                // Retain the actual payload, including out-of-range protos.
                // A subsequent ordinary update owns its normal disposition.
                self.protos.insert((x, z), Some(proto));
                if decorated || !self.decorates {
                    self.decorated.insert((x, z));
                }
            } else {
                // Generated is deliberately included: Generate can load a
                // fresh finished column and invoke a late storage callback.
                self.receive_save_result(work, world);
            }
        }
    }
    fn reconcile_ready_save_results(&mut self, world: &mut World) {
        let deferred = self.take_deferred();
        let mut ready = Vec::new();
        loop {
            match self.results.try_recv() {
                Ok(done) => ready.push(self.received(done)),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.result_disconnected();
                    break;
                }
            }
        }
        for done in deferred.into_iter().chain(ready) {
            self.receive_save_result(done, world);
        }
    }
    // Returns whether the result was reconciled, leaving every unrelated result
    // in arrival order for normal update. `wait` may already have deferred it.
    fn receive_save_result(&mut self, work: ReceivedWork, world: &mut World) -> bool {
        if !matches!(
            &work.done,
            Done::Decorated(..) | Done::Generated(..) | Done::Saved(..)
        ) {
            self.defer(work);
            return false;
        }
        let ReceivedWork { done, mut owner } = work;
        if let Some(owner) = &mut owner {
            owner.advance(WorkPhase::Reconciling);
        }
        let _reconciliation = Reconciliation(owner);
        match done {
            Done::Decorated(x, z, region) => self.receive_decorated(x, z, *region),
            Done::Generated(chunk) => self.receive_generated(chunk, world),
            Done::Saved(protos, outcome) => self.receive_saved(protos, outcome),
            _ => unreachable!("generation completion matched above"),
        }
        true
    }
    /// Finish generation for a closing world without waiting on the event
    /// loop. No new generation is scheduled while the save screen is shown.
    /// Proto snapshots are checked out to workers before chunk writes, just as
    /// in `save(true)`, so older generation cannot overwrite the final chunks.
    pub fn poll_save(&mut self, world: &mut World) -> crate::Result<bool> {
        self.poll_save_budget(world, std::time::Duration::from_millis(2))
    }
    fn poll_save_budget(
        &mut self,
        world: &mut World,
        budget: std::time::Duration,
    ) -> crate::Result<bool> {
        let started = std::time::Instant::now();
        let mut deferred = self.take_deferred().into_iter();
        while let Some(done) = deferred.next() {
            // Skip unrelated results until a completion makes progress; an
            // unrelated prefix must not repeatedly exhaust the polling budget.
            // Scanning that existing prefix is linear, not a strict time bound.
            if self.receive_save_result(done, world) && started.elapsed() >= budget {
                for work in deferred {
                    self.defer(work);
                }
                self.generation_save_result()?;
                return Ok(false);
            }
        }
        let disconnected = loop {
            match self.results.try_recv() {
                Ok(done) => {
                    self.receive_save_result(self.received(done), world);
                    if started.elapsed() >= budget {
                        self.generation_save_result()?;
                        return Ok(false);
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break false,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.result_disconnected();
                    break true;
                }
            }
        };
        self.generation_save_result()?;
        if disconnected
            && (self.save_in_flight()
                || self
                    .written
                    .iter()
                    .any(|key| self.protos.get(key).is_some_and(Option::is_some)))
        {
            return Err("chunk generation workers disconnected before saving finished".into());
        }
        if self.save_in_flight() {
            return Ok(false);
        }
        let written: Vec<_> = self.written.drain().collect();
        self.save_checked_out(written);
        Ok(self.saving == 0)
    }
    fn in_range(&self, x: i32, z: i32, margin: i32) -> bool {
        self.ranges().in_range(x, z, margin)
    }
    pub fn band(&self) -> Option<Band> {
        self.band
    }
    /// Rank the chunks `/forceload add` over chunks `(x0, z0)..=(x1, z1)`
    /// decorates in vanilla's order (`forced_order`), after every chunk
    /// ranked before.
    pub fn rank_forced(&mut self, x0: i32, z0: i32, x1: i32, z1: i32) {
        for (x, z) in horizon_world::worldgen::decoration_order::forced_order(x0, z0, x1, z1) {
            self.decoration_order.rank_or_append(x, z);
        }
    }
    /// Generate, light and keep the chunks of `band` (none: stop), and let
    /// go of what earlier bands held as it falls out of range. The chunks a
    /// band reaches are ranked for decoration row by row, as they first come
    /// within reach, so that a task decorates in the same order however its
    /// work is timed and shared among the workers.
    pub fn set_band(&mut self, band: Option<Band>) {
        if self.band == band {
            return;
        }
        self.band = band;
        self.generation_scan.exhausted = false;
        let Some(band) = band else {
            return;
        };
        // Decoration reaches two chunks past the band and generation three.
        const REACH: i32 = 4;
        let (x0, x1) = (band.x0 - REACH, band.x1 + REACH);
        let first = match self.band_ranked {
            Some((rx0, rx1, next)) if (rx0, rx1) == (x0, x1) => next,
            _ => band.z0 - REACH,
        };
        let end = band.z1 + REACH;
        for z in first..=end {
            for x in x0..=x1 {
                self.decoration_order.rank_or_append(x, z);
            }
        }
        self.band_ranked = Some((x0, x1, end.max(first - 1) + 1));
    }
    /// What holds up the columns `keys`, for a task that stopped advancing:
    /// how many are not loaded, loaded but unlit with a light job in flight,
    /// and unlit with none (those whose neighbours are all loaded are queued
    /// for light again).
    pub fn unstick_light(&mut self, world: &mut World, keys: &[(i32, i32)]) -> [usize; 3] {
        let mut counts = [0; 3];
        for &key in keys {
            if !world.chunks.contains_key(&key) {
                counts[0] += 1;
            } else if self.lit.contains(&key) {
            } else if self.lighting.contains_key(&key) {
                counts[1] += 1;
            } else {
                counts[2] += 1;
                if self.light_region(world, key.0, key.1).is_some() {
                    world.dirty_light.insert(key);
                }
            }
        }
        counts
    }
    /// Wait up to `timeout` for a worker to finish a job; the next `update`
    /// takes it. A server pumps generation with this between its ticks.
    pub fn wait(&mut self, timeout: std::time::Duration) {
        match self.results.recv_timeout(timeout) {
            Ok(done) => self.defer(self.received(done)),
            Err(mpsc::RecvTimeoutError::Disconnected) => self.result_disconnected(),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}
/// `ChunkTrackingView.isInViewDistance`: whether the column (x, z) is drawn
/// at this render distance around `center`, the camera's column
/// (`SectionOcclusionGraph.isInViewDistance`). Each axis gets a column of
/// slack, so this reaches past the circle of `radius` columns.
pub fn in_view_distance(center: (i32, i32), radius: i32, x: i32, z: i32) -> bool {
    let gap = |d: i32| i64::from((d.abs() - 1).max(0));
    let (dx, dz) = (gap(x - center.0), gap(z - center.1));
    dx * dx + dz * dz < i64::from(radius) * i64::from(radius)
}
/// The areas kept loaded: circles around the player and `extra`, and forced chunks.
struct Ranges<'a> {
    center: (i32, i32),
    radius: i32,
    extra: &'a [((i32, i32), i32)],
    forced: &'a HashSet<(i32, i32)>,
    /// `extra` as a grid, when one was made for it: `in_extra` without a
    /// scan of every area.
    cover: Option<&'a ExtraCover>,
    band: Option<&'a Band>,
}
/// The chunks a pregeneration task (`/pregen`) wants lit now: rows `z0..=z1`
/// of the columns `x0..=x1`, those within `circle` (a centre and a radius in
/// chunks) when there is one. Like an extra area, it finishes its chunks and
/// the ring around them, so that its own chunks can be lit, and generation
/// reaches past that. A task moves it down a strip of columns a row at a
/// time, so each chunk is generated once and only the rows in flight stay
/// loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Band {
    pub x0: i32,
    pub x1: i32,
    pub z0: i32,
    pub z1: i32,
    pub circle: Option<((i32, i32), i32)>,
}
impl Band {
    /// Whether (x, z) lies within `margin` (Chebyshev) of one of the band's
    /// chunks: the square of side `2 * margin + 1` around it meets the
    /// rectangle at a point inside the circle.
    pub fn within(&self, x: i32, z: i32, margin: i32) -> bool {
        let (x0, x1) = (
            x.saturating_sub(margin).max(self.x0),
            x.saturating_add(margin).min(self.x1),
        );
        let (z0, z1) = (
            z.saturating_sub(margin).max(self.z0),
            z.saturating_add(margin).min(self.z1),
        );
        if x0 > x1 || z0 > z1 {
            return false;
        }
        self.circle.is_none_or(|((cx, cz), r)| {
            let dx = i64::from(cx.clamp(x0, x1)) - i64::from(cx);
            let dz = i64::from(cz.clamp(z0, z1)) - i64::from(cz);
            dx * dx + dz * dz <= i64::from(r) * i64::from(r)
        })
    }
    /// The band's chunks go after the players' (lower runs first), row by
    /// row, as the task wants them lit.
    fn priority(&self, x: i32, z: i32) -> i64 {
        const AFTER_PLAYERS: i64 = 1 << 40;
        AFTER_PLAYERS
            + (i64::from(z - self.z0 + 8).max(0) << 20)
            + i64::from(x - self.x0 + 8).max(0)
    }
}
/// For every column near the extra areas, how far it lies outside the
/// nearest one's square (Chebyshev distance less its radius; 0 inside). A server has an area per player, and streaming asks of every
/// loaded and wanted column whether one reaches it, several times a tick:
/// with fifty players, a scan of fifty squares per question.
struct ExtraCover {
    extra: Vec<((i32, i32), i32)>,
    x0: i32,
    z0: i32,
    width: u32,
    depth: u32,
    cells: Box<[i8]>,
}
impl ExtraCover {
    /// The largest margin the grid answers for; beyond it (and outside the
    /// grid) a column is further than that from every square.
    const REACH: i32 = 4;
    /// The largest grid, in columns (a mebibyte).
    const CELLS: u64 = 1 << 20;
    fn matching<'a>(
        cover: &'a Option<ExtraCover>,
        extra: &[((i32, i32), i32)],
    ) -> Option<&'a ExtraCover> {
        cover.as_ref().filter(|c| c.extra == extra)
    }
    /// Make the grid again if `extra` changed; none for one area or none
    /// (a scan is as quick), or areas too far apart.
    fn refresh(cover: &mut Option<ExtraCover>, extra: &[((i32, i32), i32)]) {
        if cover.as_ref().is_some_and(|c| c.extra == extra) {
            return;
        }
        *cover = Self::new(extra);
    }
    fn new(extra: &[((i32, i32), i32)]) -> Option<Self> {
        if extra.len() < 2 || extra.iter().any(|&(_, r)| !(0..=64).contains(&r)) {
            return None;
        }
        let k = i64::from(Self::REACH);
        let (mut x0, mut z0, mut x1, mut z1) = (i64::MAX, i64::MAX, i64::MIN, i64::MIN);
        for &((x, z), r) in extra {
            let (x, z, r) = (i64::from(x), i64::from(z), i64::from(r) + k);
            (x0, z0, x1, z1) = (x0.min(x - r), z0.min(z - r), x1.max(x + r), z1.max(z + r));
        }
        let (width, depth) = ((x1 - x0 + 1) as u64, (z1 - z0 + 1) as u64);
        let fits = |lo: i64, hi: i64| lo >= i64::from(i32::MIN) && hi <= i64::from(i32::MAX);
        if width * depth > Self::CELLS || !fits(x0, x1) || !fits(z0, z1) {
            return None;
        }
        let (x0, z0, width, depth) = (x0 as i32, z0 as i32, width as u32, depth as u32);
        let mut cells = vec![(Self::REACH + 1) as i8; (width * depth) as usize].into_boxed_slice();
        // Inside a square counts as 0: no margin asked for is below it. Each
        // row of a square is a run of one value with a slope at either end.
        let lower = |run: &mut [i8], v: i8| run.iter_mut().for_each(|c| *c = (*c).min(v));
        for &((cx, cz), r) in extra {
            let reach = r + Self::REACH;
            for dz in -reach..=reach {
                let start = (cz + dz - z0) as usize * width as usize + (cx - x0) as usize;
                let row = &mut cells[start - reach as usize..=start + reach as usize];
                let flat = dz.abs().max(r);
                let centre = reach as usize;
                lower(
                    &mut row[centre - flat as usize..=centre + flat as usize],
                    (flat - r) as i8,
                );
                for d in flat + 1..=reach {
                    let v = (d - r) as i8;
                    lower(&mut row[centre - d as usize..=centre - d as usize], v);
                    lower(&mut row[centre + d as usize..=centre + d as usize], v);
                }
            }
        }
        Some(Self {
            extra: extra.to_vec(),
            x0,
            z0,
            width,
            depth,
            cells,
        })
    }
    /// Whether (x, z) is within `margin` of an area's square, for a
    /// `margin` up to `REACH`.
    fn within(&self, x: i32, z: i32, margin: i32) -> bool {
        let (dx, dz) = (
            x.wrapping_sub(self.x0) as u32,
            z.wrapping_sub(self.z0) as u32,
        );
        dx < self.width
            && dz < self.depth
            && i32::from(self.cells[(dz * self.width + dx) as usize]) <= margin
    }
}
impl Ranges<'_> {
    /// `distance`, or `rings` while the level loading screen is up.
    fn priority(&self, x: i32, z: i32, loading: bool) -> i64 {
        let own = if loading {
            self.rings(x, z)
        } else {
            self.distance(x, z)
        };
        match self.band {
            Some(band) if band.within(x, z, 4) => own.min(band.priority(x, z)),
            _ => own,
        }
    }
    /// Ring by ring: the Chebyshev distance to the nearest centre first, then
    /// the squared distance. Vanilla's chunk tasks run by ticket level, which
    /// spreads one level per ring of chunks (`ChunkTaskPriorityQueue`), so
    /// `LevelLoadingScreen`'s chunk map fills squares around the spawn.
    fn rings(&self, x: i32, z: i32) -> i64 {
        let d = |c: (i32, i32)| {
            let (dx, dz) = (i64::from(x - c.0), i64::from(z - c.1));
            let ring = dx.abs().max(dz.abs());
            (ring * ring) << 20 | (dx * dx + dz * dz)
        };
        self.extra
            .iter()
            .map(|(c, _)| d(*c))
            .fold(d(self.center), i64::min)
    }
    fn distance(&self, x: i32, z: i32) -> i64 {
        let d = |c: (i32, i32)| {
            let (dx, dz) = (i64::from(x - c.0), i64::from(z - c.1));
            dx * dx + dz * dz
        };
        self.extra
            .iter()
            .map(|(c, _)| d(*c))
            .fold(d(self.center), i64::min)
    }

    /// How far past the view distance chunks are generated, and how far around
    /// a finished chunk (`near_finished`): decorated chunks need their
    /// neighbours generated too.
    fn margins(decorates: bool) -> (i32, i32) {
        if decorates { (3, 2) } else { (1, 1) }
    }
    /// Chunks generation asks for: loaded from the save, or generated as protos.
    fn requested(&self, x: i32, z: i32, decorates: bool) -> bool {
        let (margin, reach) = Self::margins(decorates);
        self.in_range(x, z, margin) || self.near_finished(x, z, reach)
    }
    /// Finished columns kept when they arrive: within two of the view distance
    /// or next to a finished chunk, and every column generation asked for (a
    /// saved one loads finished; dropping it had it asked for and loaded again
    /// every frame).
    fn chunk_kept(&self, x: i32, z: i32, decorates: bool) -> bool {
        self.in_range(x, z, 2) || self.near_finished(x, z, 1) || self.requested(x, z, decorates)
    }
    /// Columns unloaded: past every column kept.
    fn unloaded(&self, x: i32, z: i32, decorates: bool) -> bool {
        !self.in_range(x, z, 3) && !self.near_finished(x, z, Self::margins(decorates).1)
    }
    /// Whether a chunk finished here (`in_range(_, _, 1)`) lies within `reach`
    /// chunks of (x, z) on both axes: its decoration needs its 3x3 neighbours
    /// decorated, which need their own 3x3 neighbours generated. The ranges are
    /// circles, so a fixed margin alone misses the corners of those squares
    /// (render distance 2 never finished the chunks next to the player).
    fn near_finished(&self, x: i32, z: i32, reach: i32) -> bool {
        let gap = |d: i32| i64::from((d.abs() - reach).max(0));
        let (dx, dz) = (gap(x - self.center.0), gap(z - self.center.1));
        let r = i64::from(self.radius + 1);
        // The extra areas are squares: their finished square grown by `reach`.
        dx * dx + dz * dz <= r * r
            || self.in_extra(x, z, 1 + reach)
            || self.in_band(x, z, 1 + reach)
    }
    fn in_range(&self, x: i32, z: i32, margin: i32) -> bool {
        self.in_main_range(x, z, margin)
            || self.in_extra(x, z, margin)
            || self.in_band(x, z, margin)
    }
    /// Within `margin` of the pregeneration band.
    fn in_band(&self, x: i32, z: i32, margin: i32) -> bool {
        self.band.is_some_and(|b| b.within(x, z, margin))
    }
    /// Within `margin` of an extra area's square (Chebyshev distance, as
    /// vanilla's ticket levels spread).
    fn in_extra(&self, x: i32, z: i32, margin: i32) -> bool {
        if let Some(cover) = self.cover
            && (0..=ExtraCover::REACH).contains(&margin)
        {
            return cover.within(x, z, margin);
        }
        self.extra
            .iter()
            .any(|&(c, radius)| (x - c.0).abs().max((z - c.1).abs()) <= radius + margin)
    }
    /// The columns meshed for drawing: those vanilla draws around the
    /// player (`in_view_distance`), and forced ones.
    fn in_view(&self, x: i32, z: i32) -> bool {
        in_view_distance(self.center, self.radius, x, z) || self.forced.contains(&(x, z))
    }
    fn in_main_range(&self, x: i32, z: i32, margin: i32) -> bool {
        let r = i64::from(self.radius + margin);
        let (dx, dz) = (i64::from(x - self.center.0), i64::from(z - self.center.1));
        if dx * dx + dz * dz <= r * r {
            return true;
        }
        let reach = (margin - 1).max(0);
        self.forced
            .iter()
            .any(|f| (x - f.0).abs() <= reach && (z - f.1).abs() <= reach)
    }
}
impl Drop for Streaming {
    fn drop(&mut self) {
        #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
        {
            // Release even if a receipt remains externally owned. Do this before
            // save draining or joining: a paused worker may be needed by either.
            let gate = self
                .shared
                .native_column_load_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(gate) = gate {
                gate.release();
            }
        }
        // Saves in flight hold proto chunks nothing else would save.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while self.saving > 0 {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.results.recv_timeout(left) {
                Ok(done) => {
                    let ReceivedWork { done, mut owner } = self.received(done);
                    if let Done::Saved(protos, outcome) = done {
                        if let Some(owner) = &mut owner {
                            owner.advance(WorkPhase::Reconciling);
                        }
                        let _reconciliation = Reconciliation(owner);
                        self.receive_saved(protos, outcome);
                    }
                    // Other payloads retain an unresolved disposal fact.
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.result_disconnected();
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
            }
        }
        // The stop predicate and notification use the worker's queue mutex.
        // Otherwise notification can fall between its empty-queue check and
        // Condvar::wait, leaving an idle worker asleep while Drop joins it.
        let queue = match self.shared.queue.try_lock() {
            Ok(queue) => queue,
            Err(std::sync::TryLockError::WouldBlock) => {
                #[cfg(test)]
                self.shared.shutdown_event(ShutdownEvent::QueueContended);
                self.shared.queue.lock().unwrap_or_else(|e| e.into_inner())
            }
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        };
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.ready.notify_all();
        drop(queue);
        #[cfg(test)]
        self.shared.shutdown_event(ShutdownEvent::Published);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        // Stop owns the remaining queue/result/deferred destruction. None of
        // those payloads was normally reconciled, so lease Drop records faults.
        self.shared
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.shared
            .queued_owners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.shared
            .returned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.deferred_owners.clear();
    }
}
#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
impl Streaming {
    #[doc(hidden)]
    pub fn arm_native_column_load_test_control(
        &self,
        column: (i32, i32),
    ) -> crate::Result<NativeColumnLoadTestControl> {
        let mut slot = self
            .shared
            .native_column_load_gate
            .lock()
            .map_err(|_| "native column gate poisoned")?;
        if slot.is_some() {
            return Err("native column gate occupied".into());
        }
        let (entered, receiver) = mpsc::channel();
        let gate = Arc::new(NativeColumnLoadGate {
            column,
            entered,
            entered_once: AtomicBool::new(false),
            resumed: AtomicBool::new(false),
            released: Mutex::new(false),
            ready: Condvar::new(),
        });
        *slot = Some(gate.clone());
        Ok(NativeColumnLoadTestControl {
            shared: Arc::downgrade(&self.shared),
            gate,
            entered: receiver,
            entered_seen: false,
        })
    }
}
/// The section indexes set in a column's bits, lowest first.
fn section_indexes(mut bits: u32) -> impl Iterator<Item = i32> {
    std::iter::from_fn(move || {
        (bits != 0).then(|| {
            let sy = bits.trailing_zeros() as i32;
            bits &= bits - 1;
            sy
        })
    })
}
/// The queued jobs' priorities computed again from each one's column
/// `distance` (`Streaming::submit`).
fn rank_again(queue: &mut BinaryHeap<Queued>, distance: impl Fn(i32, i32) -> i64) {
    if queue.is_empty() {
        return;
    }
    let mut jobs = std::mem::take(queue).into_vec();
    for job in &mut jobs {
        if let Some((x, z)) = job.at {
            job.priority = distance(x, z) * 4 + job.bias;
        }
    }
    *queue = BinaryHeap::from(jobs);
}
#[cfg(test)]
mod tests {
    use super::*;

    struct SaveProbe {
        gate: Mutex<Option<mpsc::Receiver<()>>>,
        started: mpsc::Sender<()>,
        writes: Mutex<Vec<u64>>,
        failure: Mutex<Option<String>>,
    }
    impl ChunkGenerator for SaveProbe {
        type Worker = ();
        fn worker(&self) {}
        fn proto(&self, _: &mut (), _: i32, _: i32) -> Proto {
            unreachable!()
        }
        fn finish(&self, _: Proto) -> Chunk {
            unreachable!()
        }
        fn save_protos(&self, protos: &[&Proto]) -> Result<(), String> {
            if let Some(gate) = self.gate.lock().unwrap().take() {
                self.started.send(()).unwrap();
                gate.recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
            }
            self.writes
                .lock()
                .unwrap()
                .extend(protos.iter().map(|p| *p.downcast_ref::<u64>().unwrap()));
            match &*self.failure.lock().unwrap() {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            }
        }
    }
    fn save_probe(
        gated: bool,
    ) -> (
        Streaming,
        Arc<SaveProbe>,
        mpsc::Receiver<()>,
        mpsc::Sender<()>,
    ) {
        use crate::{assets::Assets, biome::Biomes, blocks::BlockFacts, models};
        static CONTEXT: std::sync::OnceLock<Arc<WorkerContext>> = std::sync::OnceLock::new();
        let context = CONTEXT
            .get_or_init(|| {
                let root = std::env::var_os("HORIZON_TEST_RESOURCES")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| {
                        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                            .join("../../.local/horizon-client/26.3")
                    });
                assert!(
                    root.join("items.json").is_file(),
                    "owned prepared resources required"
                );
                let assets = Arc::new(Assets::load(&root).unwrap());
                let render = Arc::new(assets.reload(&root).unwrap());
                let facts = BlockFacts::load(&root, assets.registry).unwrap();
                let visibility = models::load_visibility(&root, assets.registry).unwrap();
                Arc::new(WorkerContext {
                    tables: Arc::new(
                        MeshTables::new(&assets, &render, &facts, visibility).unwrap(),
                    ),
                    light: Arc::new(LightTables::load(&root, assets.registry, &facts).unwrap()),
                    biomes: Arc::new(Biomes::load(&root).unwrap()),
                    assets,
                    render,
                })
            })
            .clone();
        let (started, began) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let probe = Arc::new(SaveProbe {
            gate: Mutex::new(gated.then_some(gate)),
            started,
            writes: Mutex::new(Vec::new()),
            failure: Mutex::new(None),
        });
        (
            Streaming::new(probe.clone(), context, 1),
            probe,
            began,
            release,
        )
    }
    fn save_world() -> World {
        World::new(horizon_world::registry::BlockStateId(0), 0)
    }
    struct FinishedProbe {
        context: Arc<WorkerContext>,
        save: Option<Arc<crate::save::SaveDir>>,
        failure: Mutex<Option<String>>,
        seen: Mutex<Vec<std::sync::Weak<Chunk>>>,
    }
    impl ChunkGenerator for FinishedProbe {
        type Worker = ();
        fn worker(&self) {}
        fn proto(&self, _: &mut (), _: i32, _: i32) -> Proto {
            unreachable!()
        }
        fn finish(&self, _: Proto) -> Chunk {
            unreachable!()
        }
        fn save_finished(&self, chunk: &Arc<Chunk>) -> Result<(), String> {
            self.seen.lock().unwrap().push(Arc::downgrade(chunk));
            if let Some(save) = &self.save {
                save.save_later(
                    std::slice::from_ref(chunk),
                    self.context.assets.registry,
                    &self.context.biomes,
                )
                .map_err(|e| e.to_string())?;
            }
            // With save set, inject failure after real region/POI acceptance
            // but before the separate entity enqueue owns anything.
            if let Some(error) = &*self.failure.lock().unwrap() {
                return Err(error.clone());
            }
            if let Some(save) = &self.save {
                save.save_finished_entities_later(chunk)
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        }
    }
    fn finished_chunk(context: &WorkerContext, key: (i32, i32)) -> Chunk {
        use horizon_world::nbt::Tag;
        let mut chunk = Chunk::empty(key.0, key.1);
        chunk.fresh = true;
        let chest = context
            .assets
            .registry
            .default_state("minecraft:chest")
            .unwrap()
            .0;
        chunk.set(1, 70, 1, chest);
        let mut slots = vec![None; 27];
        slots[0] = Some(crate::items::ItemStack {
            item: "minecraft:stone".into(),
            count: 3,
            damage: 0,
            extra: None,
        });
        chunk.block_entities.push((
            (key.0 * 16 + 1, 70, key.1 * 16 + 1),
            crate::containers::BlockEntity::Chest(slots),
        ));
        chunk.block_names.push((
            (key.0 * 16 + 1, 70, key.1 * 16 + 1),
            "Retained chest".into(),
        ));
        chunk.entities.push(Tag::compound([
            ("id", Tag::String("minecraft:pig".into())),
            ("Sentinel", Tag::Int(99)),
        ]));
        let mut extras = crate::chunk::ChunkExtras::default();
        extras.root.insert("diagnostic_extra".into(), Tag::Int(123));
        extras
            .root
            .insert("entities".into(), Tag::List(chunk.entities.clone()));
        chunk.saved = Some(Arc::new(extras));
        chunk.postprocess.push([key.0 * 16 + 1, 70, key.1 * 16 + 1]);
        chunk
    }
    fn finished_scratch(label: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "horizon-finished-{}-{label}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).unwrap();
        dir
    }
    fn persistent_finished(
        save: Arc<crate::save::SaveDir>,
        context: &Arc<WorkerContext>,
    ) -> crate::save::Persistent<crate::testgen::TestTerrain> {
        crate::save::Persistent {
            inner: Arc::new(
                crate::testgen::TestTerrain::new(context.assets.registry, &context.biomes).unwrap(),
            ),
            save,
            registry: context.assets.clone(),
            biomes: context.biomes.clone(),
        }
    }
    fn finish_polling(streaming: &mut Streaming, world: &mut World) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !streaming.poll_save(world).unwrap() {
            assert!(
                std::time::Instant::now() < deadline,
                "save polling did not finish"
            );
            thread::yield_now();
        }
    }

    #[test]
    #[ignore = "owned prepared resources: controlled generation-save worker"]
    fn save_completion_timeout_preserves_checkout_and_deferred_retry() {
        let (mut streaming, probe, started, release) = save_probe(true);
        let mut world = save_world();
        world.modified.insert((3, 4));
        streaming.protos.insert((0, 0), Some(Box::new(7u64)));
        streaming.written.insert((0, 0));
        streaming.save(&mut world, false).unwrap(); // Returns while the worker is gated.
        started
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        streaming.protos.insert((1, 0), Some(Box::new(11u64)));
        streaming.written.insert((1, 0));
        let error = streaming
            .save_until(&mut world, std::time::Instant::now())
            .unwrap_err();
        assert!(error.to_string().contains("deadline exceeded"));
        assert_eq!(streaming.saving, 1);
        assert!(streaming.protos[&(0, 0)].is_none());
        assert!(streaming.written.contains(&(1, 0)));
        assert_eq!(
            *streaming.protos[&(1, 0)]
                .as_ref()
                .unwrap()
                .downcast_ref::<u64>()
                .unwrap(),
            11
        );
        assert_eq!(world.modified, HashSet::from([(3, 4)]));
        assert!(probe.writes.lock().unwrap().is_empty());
        release.send(()).unwrap();
        streaming.wait(std::time::Duration::from_secs(5));
        assert!(matches!(
            streaming.deferred.as_slice(),
            [Done::Saved(_, Ok(()))]
        ));
        assert_eq!(streaming.saving, 1); // Deferred result still owns the proto.
        streaming
            .save_until(&mut world, std::time::Instant::now())
            .unwrap();
        assert_eq!(streaming.saving, 0);
        assert_eq!(
            *streaming.protos[&(0, 0)]
                .as_ref()
                .unwrap()
                .downcast_ref::<u64>()
                .unwrap(),
            7
        );
        assert!(streaming.deferred.is_empty());
        assert!(streaming.poll_save(&mut world).unwrap());
        assert_eq!(*probe.writes.lock().unwrap(), [7, 11]);
        assert_eq!(world.modified, HashSet::from([(3, 4)]));
    }

    #[test]
    #[ignore = "owned prepared resources: queued generation-save completions"]
    fn save_completion_queued_and_deferred_results_preserve_unrelated_order() {
        let (mut streaming, probe, _, _) = save_probe(false);
        let mut world = save_world();
        streaming.protos.insert((0, 0), Some(Box::new(8u64)));
        streaming.written.insert((0, 0));
        streaming.deferred.extend([
            Done::Proto(1, 0, Box::new(1u64), false),
            Done::Proto(2, 0, Box::new(2u64), false),
        ]);
        streaming.save(&mut world, false).unwrap();
        streaming
            .save_until(
                &mut world,
                std::time::Instant::now() + std::time::Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(streaming.saving, 0);
        streaming
            .deferred
            .push(Done::Proto(3, 0, Box::new(3u64), false));
        finish_polling(&mut streaming, &mut world);
        let unrelated: Vec<_> = streaming
            .deferred
            .iter()
            .map(|done| match done {
                Done::Proto(x, _, payload, _) => (*x, *payload.downcast_ref::<u64>().unwrap()),
                _ => panic!("save completion was left deferred"),
            })
            .collect();
        assert_eq!(unrelated, [(1, 1), (2, 2), (3, 3)]);
        assert_eq!(*probe.writes.lock().unwrap(), [8]);
        // Incremental reconciliation must consume deferred decoration/finish too.
        streaming.decorating = 1;
        streaming.finishing.insert((0, 0));
        streaming.deferred.extend([
            Done::Decorated(
                0,
                0,
                Box::new(std::array::from_fn(|i| Box::new(i as u64) as Proto)),
            ),
            Done::Generated(Chunk::empty(0, 0)),
        ]);
        finish_polling(&mut streaming, &mut world);
        assert_eq!(streaming.decorating, 0);
        assert!(streaming.finishing.is_empty());
        assert!(world.chunks.contains_key(&(0, 0)));
        assert_eq!(streaming.deferred.len(), 3);
        let mut writes = probe.writes.lock().unwrap()[1..].to_vec();
        writes.sort_unstable();
        assert_eq!(writes, (0..9).collect::<Vec<_>>());
    }

    #[test]
    #[ignore = "owned prepared resources: disconnected generation-save worker"]
    fn save_completion_disconnection_fails_required_work_but_idle_succeeds() {
        let (mut streaming, probe, _, _) = save_probe(false);
        let mut world = save_world();
        world.modified.insert((3, 4));
        let (send, results) = mpsc::channel();
        drop(send);
        streaming.results = results;
        assert!(streaming.poll_save(&mut world).unwrap());
        streaming
            .save_until(&mut world, std::time::Instant::now())
            .unwrap();
        streaming.saving = 1;
        streaming.protos.insert((0, 0), None);
        let error = streaming
            .save_until(
                &mut world,
                std::time::Instant::now() + std::time::Duration::from_secs(5),
            )
            .unwrap_err();
        assert!(error.to_string().contains("workers disconnected"));
        assert!(
            streaming
                .poll_save(&mut world)
                .unwrap_err()
                .to_string()
                .contains("workers disconnected")
        );
        assert_eq!(streaming.saving, 1);
        assert!(streaming.protos[&(0, 0)].is_none());
        assert_eq!(world.modified, HashSet::from([(3, 4)]));
        // No worker is needed for the synchronous path's direct proto callback.
        streaming.saving = 0;
        streaming.protos.insert((0, 0), Some(Box::new(9u64)));
        streaming.written.insert((0, 0));
        assert!(streaming.poll_save(&mut world).is_err());
        assert!(streaming.written.contains(&(0, 0)));
        assert!(streaming.protos[&(0, 0)].is_some());
        streaming
            .save_until(&mut world, std::time::Instant::now())
            .unwrap();
        assert_eq!(*probe.writes.lock().unwrap(), [9]);
        assert!(streaming.written.is_empty());
        assert_eq!(world.modified, HashSet::from([(3, 4)]));
    }

    #[test]
    #[ignore = "owned prepared resources: direct proto callback failure"]
    fn proto_save_direct_failure_retains_dirty_keys_and_refuses_retry() {
        let (mut streaming, probe, _, _) = save_probe(false);
        let mut world = save_world();
        world.modified.insert((3, 4));
        streaming.protos.insert((0, 0), Some(Box::new(17u64)));
        streaming.written.insert((0, 0));
        *probe.failure.lock().unwrap() = Some("direct proto failure".into());
        let error = streaming.save(&mut world, true).unwrap_err().to_string();
        assert_eq!(error, "direct proto failure");
        assert_eq!(streaming.generation_save_error(), Some(error.as_str()));
        assert!(streaming.written.contains(&(0, 0)));
        assert_eq!(
            *streaming.protos[&(0, 0)]
                .as_ref()
                .unwrap()
                .downcast_ref::<u64>()
                .unwrap(),
            17
        );
        *probe.failure.lock().unwrap() = None;
        for wait in [false, true] {
            assert_eq!(
                streaming.save(&mut world, wait).unwrap_err().to_string(),
                error
            );
        }
        assert_eq!(
            streaming.poll_save(&mut world).unwrap_err().to_string(),
            error
        );
        streaming.save_checked_out([(0, 0)]);
        assert!(streaming.protos[&(0, 0)].is_some());
        assert!(streaming.written.contains(&(0, 0)));
        assert_eq!(*probe.writes.lock().unwrap(), [17]);
        assert_eq!(world.modified, HashSet::from([(3, 4)]));
    }

    #[test]
    #[ignore = "owned prepared resources: failed Saved batch tail ownership"]
    fn proto_save_fault_wins_poll_budget_without_losing_deferred_tail() {
        let (mut streaming, _, _, _) = save_probe(false);
        let mut world = save_world();
        streaming.saving = 2;
        streaming.protos.insert((0, 0), None);
        streaming.protos.insert((1, 0), None);
        streaming.deferred.extend([
            Done::Saved(vec![((0, 0), Box::new(21u64))], Err("first failure".into())),
            Done::Proto(7, 0, Box::new(7u64), false),
            Done::Saved(vec![((1, 0), Box::new(22u64))], Ok(())),
            Done::Proto(8, 0, Box::new(8u64), false),
        ]);
        let error = streaming
            .poll_save_budget(&mut world, std::time::Duration::ZERO)
            .unwrap_err();
        assert_eq!(error.to_string(), "first failure");
        assert_eq!(streaming.saving, 1);
        assert!(streaming.written.contains(&(0, 0)));
        assert!(streaming.protos[&(0, 0)].is_some());
        assert!(streaming.protos[&(1, 0)].is_none());
        assert!(matches!(
            streaming.deferred.as_slice(),
            [
                Done::Proto(7, ..),
                Done::Saved(_, Ok(())),
                Done::Proto(8, ..)
            ]
        ));
        // Reconciliation still runs before the existing sticky fault guard.
        assert_eq!(
            streaming.save(&mut world, false).unwrap_err().to_string(),
            "first failure"
        );
        assert_eq!(streaming.saving, 0);
        assert_eq!(
            *streaming.protos[&(1, 0)]
                .as_ref()
                .unwrap()
                .downcast_ref::<u64>()
                .unwrap(),
            22
        );
        assert!(matches!(
            streaming.deferred.as_slice(),
            [Done::Proto(7, ..), Done::Proto(8, ..)]
        ));
        assert!(!streaming.written.contains(&(1, 0)));
        streaming.saving = 1;
        streaming.receive_saved(vec![((2, 0), Box::new(23u64))], Err("later failure".into()));
        assert_eq!(streaming.generation_save_error(), Some("first failure"));
        assert!(streaming.written.contains(&(2, 0)));
    }

    #[test]
    #[ignore = "owned prepared resources: failed proto ownership during normal updates"]
    fn proto_save_worker_failure_halts_producers_but_reconciles_existing_work() {
        let (mut streaming, probe, started, release) = save_probe(true);
        let mut world = save_world();
        *probe.failure.lock().unwrap() = Some("worker proto failure".into());
        let payload = Box::new(42u64);
        let address = (&*payload as *const u64) as usize;
        streaming.protos.insert((0, 0), Some(payload));
        streaming.written.insert((0, 0));
        streaming.save(&mut world, false).unwrap();
        started
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(streaming.protos[&(0, 0)].is_none());
        release.send(()).unwrap();
        streaming.wait(std::time::Duration::from_secs(5));
        assert!(matches!(
            streaming.deferred.as_slice(),
            [Done::Saved(_, Err(_))]
        ));
        assert_eq!(
            streaming.poll_save(&mut world).unwrap_err().to_string(),
            "worker proto failure"
        );
        assert_eq!(streaming.saving, 0);
        assert_eq!(
            (streaming.protos[&(0, 0)]
                .as_ref()
                .unwrap()
                .downcast_ref::<u64>()
                .unwrap() as *const u64) as usize,
            address
        );
        assert!(streaming.written.contains(&(0, 0)));
        *probe.failure.lock().unwrap() = None;
        streaming.mesh = false;
        // These neighbors would make the failed proto eligible for Finish.
        for z in -1..=1 {
            for x in -1..=1 {
                streaming.decorated.insert((x, z));
                if (x, z) != (0, 0) {
                    streaming.protos.insert((x, z), Some(Box::new(0u64)));
                }
            }
        }
        // Already-submitted results must drain even after production halts.
        streaming.generating.insert((5, 0));
        streaming.decorating = 1;
        streaming.finishing.insert((6, 0));
        streaming.lighting.insert((6, 0), 77);
        streaming.meshing.insert((6, 0, 0), 88);
        streaming.deferred.extend([
            Done::Proto(5, 0, Box::new(50u64), false),
            Done::Decorated(
                10,
                0,
                Box::new(std::array::from_fn(|i| Box::new(100 + i as u64) as Proto)),
            ),
            Done::Generated(Chunk::empty(6, 0)),
            Done::Lit(6, 0, Vec::new(), 77),
            Done::Meshed(6, 0, 0, SectionMesh::default(), 88),
        ]);
        world
            .chunks
            .insert((1000, 1000), Arc::new(Chunk::empty(1000, 1000)));
        let (uploads, removed) = streaming.update(&mut world);
        assert_eq!(uploads.len(), 1);
        assert_eq!(removed.len(), 1);
        assert!(world.chunks[&(6, 0)].lit);
        assert!(streaming.lighting.is_empty());
        assert!(streaming.meshing.is_empty());
        assert_eq!(streaming.decorating, 0);
        assert!(streaming.finishing.is_empty());
        assert!(streaming.generating.is_empty());
        // A neighboring decoration would borrow the failed key as part of its
        // 3x3, even if finalization of that key itself were blocked.
        streaming.decorates = true;
        streaming.decorated.remove(&(1, 0));
        for z in -1..=1 {
            streaming.protos.insert((2, z), Some(Box::new(0u64)));
        }
        streaming.update(&mut world);
        assert!(streaming.protos[&(0, 0)].is_some());
        assert_eq!(streaming.decorating, 0);
        for center in [(100, 100), (0, 0), (-100, -100)] {
            streaming.center = center;
            streaming.decorates = !streaming.decorates;
            streaming.update(&mut world);
            assert_eq!(
                (streaming.protos[&(0, 0)]
                    .as_ref()
                    .unwrap()
                    .downcast_ref::<u64>()
                    .unwrap() as *const u64) as usize,
                address
            );
            assert!(streaming.written.contains(&(0, 0)));
            assert!(streaming.protos[&(10, 0)].is_some());
            assert!(streaming.written.contains(&(10, 0)));
            assert!(streaming.generating.is_empty());
            assert!(streaming.finishing.is_empty());
            assert_eq!(streaming.decorating, 0);
            assert_eq!(streaming.saving, 0);
            assert!(
                !streaming
                    .shared
                    .queue
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|q| matches!(
                        &q.job,
                        Job::Generate(..) | Job::Decorate(..) | Job::Finish(..) | Job::Save(..)
                    ))
            );
        }
        assert_eq!(*probe.writes.lock().unwrap(), [42]);
        assert_eq!(
            streaming.save(&mut world, false).unwrap_err().to_string(),
            "worker proto failure"
        );
    }

    #[test]
    #[ignore = "owned prepared resources: immutable failed finished handoff"]
    fn finished_save_failure_retains_exact_payload_before_or_after_partial_enqueue() {
        for partial in [false, true] {
            let (mut streaming, _, _, _) = save_probe(false);
            let context = streaming.shared.context.read().unwrap().clone();
            let dir = finished_scratch(if partial { "partial" } else { "before" });
            let save = Arc::new(crate::save::SaveDir::new(dir.clone()));
            // Initialize the existing POI tables without enqueueing any data.
            ChunkGenerator::save_protos(&persistent_finished(save.clone(), &context), &[]).unwrap();
            let probe = Arc::new(FinishedProbe {
                context: context.clone(),
                save: partial.then(|| save.clone()),
                failure: Mutex::new(Some("finished enqueue failed".into())),
                seen: Mutex::new(Vec::new()),
            });
            streaming.saves = probe.clone();
            streaming.mesh = false;
            let mut world = save_world();
            let key = (40, 40);
            #[cfg(feature = "paired-packages")]
            world
                .enable_residency_observer(
                    crate::mutation_observer::AuthorityScope::new(7, "minecraft:overworld")
                        .unwrap(),
                    [key, (41, 40)],
                    8,
                )
                .unwrap();
            let chunk = finished_chunk(&context, key);
            let extras = chunk.saved.as_ref().unwrap().clone();
            let chest_id = chunk.get(1, 70, 1);
            streaming.receive_generated(chunk, &mut world);
            assert_eq!(streaming.failed_finished.len(), 1);
            let held = &streaming.failed_finished[0];
            assert!(std::sync::Weak::ptr_eq(
                &probe.seen.lock().unwrap()[0],
                &Arc::downgrade(held)
            ));
            assert_eq!(held.get(1, 70, 1), chest_id);
            assert_eq!(held.entities.len(), 1);
            assert_eq!(held.block_names[0].1, "Retained chest");
            let crate::containers::BlockEntity::Chest(slots) = &held.block_entities[0].1 else {
                panic!("native chest was lost")
            };
            assert_eq!(slots[0].as_ref().unwrap().count, 3);
            assert!(Arc::ptr_eq(held.saved.as_ref().unwrap(), &extras));
            assert_eq!(held.postprocess, [[641, 70, 641]]);
            assert!(world.chunks.is_empty());
            assert!(world.loaded_entities.is_empty());
            assert_eq!(world.block_entities.iter_mut().count(), 0);
            save.flush().unwrap();
            assert_eq!(
                save.load(key.0, key.1, context.assets.registry, &context.biomes)
                    .is_some(),
                partial
            );
            assert!(save.load_entities(key.0, key.1).is_empty());
            *probe.failure.lock().unwrap() = None;
            streaming.finishing.insert((41, 40));
            streaming.saving = 1;
            streaming.protos.insert((3, 3), None);
            streaming.deferred.extend([
                Done::Generated(finished_chunk(&context, (41, 40))),
                Done::Saved(
                    vec![((3, 3), Box::new(22u64))],
                    Err("later proto failure".into()),
                ),
            ]);
            streaming.update(&mut world);
            assert_eq!(probe.seen.lock().unwrap().len(), 1);
            assert_eq!(streaming.failed_finished.len(), 2);
            assert_eq!(streaming.saving, 0);
            assert!(streaming.finishing.is_empty());
            assert!(streaming.protos[&(3, 3)].is_some());
            assert!(streaming.written.contains(&(3, 3)));
            assert!(streaming.generating.is_empty());
            assert_eq!(streaming.decorating, 0);
            assert!(world.chunks.is_empty());
            assert_eq!(
                streaming.generation_save_error(),
                Some("finished enqueue failed")
            );
            assert!(streaming.save(&mut world, true).is_err());
            assert!(streaming.save(&mut world, false).is_err());
            assert!(streaming.poll_save(&mut world).is_err());
            #[cfg(feature = "paired-packages")]
            assert!(world.drain_residency().unwrap().unwrap().is_empty());
            drop(streaming);
            drop(probe);
            drop(save);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    #[ignore = "owned prepared resources: prior generation fault keeps finished snapshots"]
    fn finished_save_prior_fault_skips_callback_without_changing_first_error() {
        let (mut streaming, _, _, _) = save_probe(false);
        let context = streaming.shared.context.read().unwrap().clone();
        let probe = Arc::new(FinishedProbe {
            context: context.clone(),
            save: None,
            failure: Mutex::new(None),
            seen: Mutex::new(Vec::new()),
        });
        streaming.saves = probe.clone();
        streaming.fail_generation_save("prior proto failure".into());
        let mut world = save_world();
        streaming.receive_generated(finished_chunk(&context, (40, 40)), &mut world);
        assert_eq!(streaming.failed_finished.len(), 1);
        assert!(probe.seen.lock().unwrap().is_empty());
        assert!(world.chunks.is_empty());
        assert_eq!(
            streaming.generation_save_error(),
            Some("prior proto failure")
        );
    }

    #[test]
    #[ignore = "owned prepared resources: finished Persistent handoff"]
    fn finished_save_success_preserves_entities_and_extras_in_existing_formats() {
        let (mut streaming, _, _, _) = save_probe(false);
        let context = streaming.shared.context.read().unwrap().clone();
        let dir = finished_scratch("success");
        let save = Arc::new(crate::save::SaveDir::new(dir.clone()));
        streaming.saves = Arc::new(persistent_finished(save.clone(), &context));
        let mut world = save_world();
        let key = (40, 40);
        let chunk = finished_chunk(&context, key);
        let entities = chunk.entities.clone();
        streaming.receive_generated(chunk, &mut world);
        assert!(streaming.failed_finished.is_empty());
        assert!(streaming.generation_save_error().is_none());
        assert!(world.chunks.is_empty());
        assert_eq!(save.load_entities(key.0, key.1), entities);
        save.flush().unwrap();
        let reopened = crate::save::SaveDir::new(dir.clone());
        assert_eq!(reopened.load_entities(key.0, key.1), entities);
        // Check the actual record: decoding a Chunk filters legacy keys and
        // cannot establish that duplicate entity data was absent on disk.
        let raw = {
            let path = dir
                .join("region")
                .join(format!("r.{}.{}.mca", key.0 >> 5, key.1 >> 5));
            let mut file = horizon_world::region::RegionFile::open(path).unwrap();
            file.read(horizon_core::position::ChunkPos::new(key.0, key.1))
                .unwrap()
                .unwrap()
                .nbt(horizon_world::nbt::Limits::default())
                .unwrap()
        };
        assert!(
            raw.tag.get("entities").is_none(),
            "legacy entities were duplicated into the raw region record"
        );
        assert_eq!(
            raw.tag.get("diagnostic_extra"),
            Some(&horizon_world::nbt::Tag::Int(123))
        );
        let region = reopened
            .load(key.0, key.1, context.assets.registry, &context.biomes)
            .unwrap();
        assert_eq!(
            region
                .saved
                .as_ref()
                .unwrap()
                .root
                .get(&"diagnostic_extra".into()),
            Some(&horizon_world::nbt::Tag::Int(123))
        );
        assert_eq!(region.block_entities.len(), 1);
        drop(region);
        drop(reopened);
        drop(streaming);
        drop(save);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(feature = "paired-packages")]
    #[test]
    #[ignore = "owned prepared resources: explicit authoritative residency seams"]
    fn residency_streaming_admission_unload_and_replica_separation() {
        use crate::{
            assets::Assets, biome::Biomes, blocks::BlockFacts, models,
            mutation_observer::AuthorityScope, residency_observer::Kind,
        };
        use horizon_world::registry::BlockStateId;
        let root = std::env::var_os("HORIZON_TEST_RESOURCES")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../.local/horizon-client/26.3")
            });
        assert!(
            root.join("items.json").is_file(),
            "owned prepared resources required"
        );
        let assets = Arc::new(Assets::load(&root).unwrap());
        let render = Arc::new(assets.reload(&root).unwrap());
        let facts = BlockFacts::load(&root, assets.registry).unwrap();
        let biomes = Arc::new(Biomes::load(&root).unwrap());
        let generator =
            Arc::new(crate::testgen::TestTerrain::new(assets.registry, &biomes).unwrap());
        let visibility = models::load_visibility(&root, assets.registry).unwrap();
        let context = Arc::new(WorkerContext {
            tables: Arc::new(MeshTables::new(&assets, &render, &facts, visibility).unwrap()),
            light: Arc::new(LightTables::load(&root, assets.registry, &facts).unwrap()),
            biomes,
            assets,
            render,
        });
        let mut streaming = Streaming::new(generator, context, 1);
        streaming.radius = 2;
        streaming.mesh = false;
        // Fill the in-flight budget so updates request no unrelated generation.
        streaming
            .generating
            .extend([(1000, 0), (1001, 0), (1002, 0)]);
        let mut world = World::new(BlockStateId(0), 0);
        world
            .enable_residency_observer(
                AuthorityScope::new(7, "minecraft:overworld").unwrap(),
                [(0, 0)],
                8,
            )
            .unwrap();
        let mut generated = Chunk::empty(0, 0);
        generated.fresh = true;
        let mut saved = Chunk::empty(0, 0);
        saved.light_loaded = true;
        saved.lit = true;
        world.game_time = 42;
        streaming
            .deferred
            .extend([Done::Generated(generated), Done::Generated(saved.clone())]);
        assert!(streaming.update(&mut world).1.is_empty());
        assert!(
            world.modified.contains(&(0, 0)),
            "fresh admission keeps normal save marking"
        );
        world.game_time = 43;
        streaming.center = (100, 100);
        let removed = streaming.update(&mut world).1;
        assert_eq!(removed.len(), 1);
        assert_eq!((removed[0].x, removed[0].z), (0, 0));
        assert!(!world.chunks.contains_key(&(0, 0)));
        world.game_time = 44;
        streaming.center = (0, 0);
        streaming.deferred.push(Done::Generated(saved));
        streaming.update(&mut world);
        let events = world.drain_residency().unwrap().unwrap();
        assert_eq!(
            events
                .iter()
                .map(|e| (e.kind(), e.game_time(), e.sequence()))
                .collect::<Vec<_>>(),
            [
                (Kind::Loaded, 42, 1),
                (Kind::Replaced, 42, 2),
                (Kind::Unloaded, 43, 3),
                (Kind::Loaded, 44, 4)
            ]
        );
        for event in events {
            assert_eq!(event.coordinate(), (0, 0));
            assert_eq!(event.scope().dimension(), "minecraft:overworld");
        }
        streaming.remote = true;
        streaming.insert_remote(&mut world, Chunk::empty(0, 0));
        assert!(streaming.forget(&mut world, 0, 0).is_some());
        assert!(world.drain_residency().unwrap().unwrap().is_empty());
    }

    #[test]
    fn queued_jobs_follow_the_centre_and_the_loading_screen() {
        let forced = HashSet::new();
        let ranges = |center| Ranges {
            center,
            radius: 32,
            extra: &[],
            forced: &forced,
            cover: None,
            band: None,
        };
        let generate = |x, z, priority, order| Queued {
            priority,
            at: Some((x, z)),
            bias: 0,
            order,
            job: Job::Generate(x, z),
        };
        let loading = ranges((0, 0));
        let queued = || {
            BinaryHeap::from(vec![
                // Queued while the loading screen was up: ring order.
                generate(3, 1, loading.priority(3, 1, true) * 4, 1),
                // Queued after it closed: plain distances, far below any ring past the first.
                generate(20, 0, loading.priority(20, 0, false) * 4, 2),
                Queued {
                    priority: i64::MIN,
                    at: None,
                    bias: 0,
                    order: 3,
                    job: Job::Save(Vec::new()),
                },
            ])
        };
        let pops = |mut queue: BinaryHeap<Queued>| {
            std::iter::from_fn(move || queue.pop())
                .map(|q| q.at)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            pops(queued()),
            [None, Some((20, 0)), Some((3, 1))],
            "the stale order"
        );
        let mut queue = queued();
        rank_again(&mut queue, |x, z| loading.priority(x, z, false));
        assert_eq!(pops(queue), [None, Some((3, 1)), Some((20, 0))]);
        // The player moved: the job near the new centre goes first.
        let mut queue = BinaryHeap::from(vec![generate(3, 1, 0, 1), generate(20, 0, 1, 2)]);
        let moved = ranges((19, 0));
        rank_again(&mut queue, |x, z| moved.priority(x, z, false));
        assert_eq!(queue.pop().and_then(|q| q.at), Some((20, 0)));
    }

    #[test]
    fn view_distance_is_vanillas_shape() {
        // ChunkTrackingView.isInViewDistance at render distance 12: a column
        // of slack on each axis, so 533 columns rather than the 441 of a
        // circle of radius 12.
        let count = |r: i32| {
            (-40..=40)
                .flat_map(|x| (-40..=40).map(move |z| (x, z)))
                .filter(|&(x, z)| in_view_distance((5, -7), r, x + 5, z - 7))
                .count()
        };
        assert_eq!(count(12), 533);
        assert_eq!(count(2), 25);
        for (x, z, inside) in [(12, 0, true), (13, 0, false), (9, 9, true), (10, 9, false)] {
            assert_eq!(in_view_distance((0, 0), 12, x, z), inside, "({x}, {z})");
            assert_eq!(
                in_view_distance((0, 0), 12, -z, -x),
                inside,
                "({}, {})",
                -z,
                -x
            );
        }
    }

    struct GenerationFixture {
        world: World,
        center: (i32, i32),
        radius: i32,
        extra: Vec<((i32, i32), i32)>,
        forced: HashSet<(i32, i32)>,
        generating: FxHashSet<(i32, i32)>,
        protos: FxHashMap<(i32, i32), Option<Proto>>,
        finishing: FxHashSet<(i32, i32)>,
        decorates: bool,
        remote: bool,
    }
    impl GenerationFixture {
        fn new(radius: i32, decorates: bool) -> Self {
            Self {
                world: World::new(horizon_world::registry::BlockStateId(0), 0),
                center: (0, 0),
                radius,
                extra: Vec::new(),
                forced: HashSet::new(),
                generating: Default::default(),
                protos: Default::default(),
                finishing: Default::default(),
                decorates,
                remote: false,
            }
        }
        fn input(&self) -> GenerationInputs<'_> {
            GenerationInputs {
                ranges: Ranges {
                    center: self.center,
                    radius: self.radius,
                    extra: &self.extra,
                    forced: &self.forced,
                    cover: None,
                    band: None,
                },
                world: &self.world,
                generating: &self.generating,
                protos: &self.protos,
                finishing: &self.finishing,
                decorates: self.decorates,
                remote: self.remote,
                loading: false,
            }
        }
        fn fill(&mut self) {
            // Scheduler only reads membership. Share one real empty column to
            // keep a radius-32 fixture small; chunk content is never consulted.
            let chunk = Arc::new(Chunk::empty(0, 0));
            for (_, x, z) in baseline_candidates(self.input(), usize::MAX) {
                self.world.chunks.insert((x, z), chunk.clone());
            }
        }
        fn fill_steady(&mut self) {
            let chunk = Arc::new(Chunk::empty(0, 0));
            for (_, x, z) in baseline_candidates(self.input(), usize::MAX) {
                if self.input().ranges.in_range(x, z, 1) {
                    self.world.chunks.insert((x, z), chunk.clone());
                } else {
                    // Generation's outer neighbour margin remains proto;
                    // only columns in the finish range become resident.
                    self.protos.insert((x, z), Some(Box::new(())));
                }
            }
        }
        fn assert_same(&self, gate: &mut GenerationScan, capacity: usize) -> bool {
            let expected = baseline_candidates(self.input(), capacity);
            let result = gate.candidates(self.input(), capacity);
            assert_eq!(result.as_deref().unwrap_or(&[]), expected);
            result.is_some()
        }
    }

    // Original scheduling scan, without snapshot bookkeeping, for differential
    // correctness and timing. Keep this independent of the production gate.
    fn baseline_candidates(input: GenerationInputs<'_>, capacity: usize) -> Vec<(i64, i32, i32)> {
        if input.remote || input.generating.len() >= capacity {
            return Vec::new();
        }
        let margin: i32 = if input.decorates { 3 } else { 1 };
        let reach = if input.decorates { 2 } else { 1 };
        let mut wanted = Vec::new();
        let areas: Vec<_> = std::iter::once((input.ranges.center, input.ranges.radius))
            .chain(input.ranges.extra.iter().copied())
            .collect();
        let missing = |x, z| {
            !input.world.chunks.contains_key(&(x, z))
                && !input.generating.contains(&(x, z))
                && !input.protos.contains_key(&(x, z))
                && !input.finishing.contains(&(x, z))
        };
        for (center, radius) in areas {
            let r = radius + margin.max(reach + 1);
            for dz in -r..=r {
                for dx in -r..=r {
                    let (x, z) = (center.0 + dx, center.1 + dz);
                    if (input.ranges.in_range(x, z, margin)
                        || input.ranges.near_finished(x, z, reach))
                        && missing(x, z)
                    {
                        wanted.push((input.ranges.distance(x, z), x, z));
                    }
                }
            }
        }
        for &(fx, fz) in input.ranges.forced {
            for dz in -(margin - 1)..=(margin - 1) {
                for dx in -(margin - 1)..=(margin - 1) {
                    let (x, z) = (fx + dx, fz + dz);
                    if missing(x, z) {
                        wanted.push((input.ranges.distance(x, z), x, z));
                    }
                }
            }
        }
        wanted.sort_unstable();
        wanted.dedup();
        wanted.truncate(capacity - input.generating.len());
        wanted
    }

    #[test]
    fn generation_gate_tracks_public_ranges_and_exact_membership() {
        for decorates in [false, true] {
            let mut f = GenerationFixture::new(2, decorates);
            f.fill();
            let mut gate = GenerationScan::default();
            assert!(f.assert_same(&mut gate, 6));
            assert!(!f.assert_same(&mut gate, 6));
            // Light/mesh state and block content do not affect generation.
            f.world.dirty_light.insert((0, 0));
            f.world.dirty_sections.insert((0, 0, 0));
            Arc::make_mut(f.world.chunks.get_mut(&(0, 0)).unwrap()).lit = true;
            assert!(!f.assert_same(&mut gate, 6));
            // Same-count removal/replacement must not leave a hole unnoticed.
            let chunk = f.world.chunks.remove(&(0, 0)).unwrap();
            f.world.chunks.insert((100, 100), chunk.clone());
            assert!(f.assert_same(&mut gate, 6));
            f.world.chunks.remove(&(100, 100));
            f.world.chunks.insert((0, 0), chunk);
            assert!(f.assert_same(&mut gate, 6));
            assert!(!f.assert_same(&mut gate, 6));
            f.center = (1, -1);
            assert!(f.assert_same(&mut gate, 6));
            f.fill();
            f.assert_same(&mut gate, 6);
            f.radius += 1;
            assert!(f.assert_same(&mut gate, 6));
            f.fill();
            f.assert_same(&mut gate, 6);
            f.extra.push(((20, -20), 2));
            assert!(f.assert_same(&mut gate, 6));
            f.fill();
            f.assert_same(&mut gate, 6);
            f.forced.insert((40, 40));
            assert!(f.assert_same(&mut gate, 6));
            f.fill();
            f.assert_same(&mut gate, 6);
            f.extra.clear();
            f.forced.clear();
            assert!(f.assert_same(&mut gate, 6));
            f.remote = true;
            assert!(!f.assert_same(&mut gate, 6));
            f.world.chunks.clear();
            f.remote = false;
            assert!(f.assert_same(&mut gate, 6));
            f.fill();
            f.assert_same(&mut gate, 6);
            f.world = World::new(horizon_world::registry::BlockStateId(0), 1);
            assert!(f.assert_same(&mut gate, 6));
        }
    }

    #[test]
    fn generation_gate_preserves_batches_stage_transitions_and_retries() {
        for decorates in [false, true] {
            let mut f = GenerationFixture::new(2, decorates);
            let mut gate = GenerationScan::default();
            let mut scheduled = FxHashSet::default();
            loop {
                let expected = baseline_candidates(f.input(), 3);
                let batch = gate.candidates(f.input(), 3).unwrap_or_default();
                assert_eq!(batch, expected);
                if batch.is_empty() {
                    break;
                }
                for (_, x, z) in batch {
                    assert!(scheduled.insert((x, z)), "duplicate scheduling");
                    f.generating.insert((x, z));
                }
                if f.generating.len() == 3 {
                    assert!(!f.assert_same(&mut gate, 3)); // capacity pressure
                } else {
                    f.assert_same(&mut gate, 3);
                }
                for key in std::mem::take(&mut f.generating) {
                    // load miss -> proto -> checked out decoration -> finish.
                    f.protos.insert(key, None);
                }
                gate.exhausted = false; // Done::Proto/Decorated invalidation
            }
            assert!(!scheduled.is_empty());
            assert!(!f.assert_same(&mut gate, 3));
            let key = (0, 0);
            f.protos.remove(&key);
            f.finishing.insert(key);
            assert!(!f.assert_same(&mut gate, 3)); // eligibility unchanged
            f.finishing.remove(&key);
            f.world.chunks.insert(key, Arc::new(Chunk::empty(0, 0)));
            gate.exhausted = false; // Done::Generated
            assert!(f.assert_same(&mut gate, 3));
            assert!(!f.assert_same(&mut gate, 3));
            // A discarded/pruned proto must be requested again, even without
            // any range or loaded-membership change.
            let key = scheduled.iter().copied().find(|k| *k != (0, 0)).unwrap();
            f.protos.remove(&key);
            gate.exhausted = false;
            assert!(f.assert_same(&mut gate, 3));
            assert_eq!(baseline_candidates(f.input(), 3).len(), 1);
        }
    }

    #[test]
    fn generation_scan_looks_at_overlapping_areas_once() {
        // A server's players: many areas around one spot, some apart, one
        // touching another only at a corner, and part of the land present.
        for decorates in [false, true] {
            let mut f = GenerationFixture::new(10, decorates);
            for i in 0..40 {
                f.extra.push(((i % 7 - 3, i / 7 - 3), 19));
            }
            f.extra.push(((200, -150), 19));
            f.extra.push(((243, -107), 19));
            f.extra.push(((-60, 5), 4));
            f.forced.insert((90, 90));
            let chunk = Arc::new(Chunk::empty(0, 0));
            for x in -40i32..40 {
                for z in -40i32..40 {
                    match (x * 7 + z * 13).rem_euclid(5) {
                        0 => drop(f.world.chunks.insert((x, z), chunk.clone())),
                        1 => drop(f.protos.insert((x, z), None)),
                        2 if x % 3 == 0 => drop(f.generating.insert((x, z))),
                        _ => {}
                    }
                }
            }
            assert!(baseline_candidates(f.input(), usize::MAX).len() > 1000);
            for capacity in [usize::MAX, f.generating.len() + 7] {
                let mut gate = GenerationScan::default();
                assert!(f.assert_same(&mut gate, capacity));
            }
        }
    }

    #[test]
    #[ignore = "reproducible actual generation scheduler benchmark; run with --ignored --nocapture"]
    fn generation_scan_benchmark() {
        use std::{hint::black_box, time::Instant};
        let mut f = GenerationFixture::new(32, true);
        f.fill_steady();
        let mut gate = GenerationScan::default();
        f.assert_same(&mut gate, 12);
        assert!(!f.assert_same(&mut gate, 12));
        const FRAMES: usize = 2000;
        let measure = |optimized: bool, dynamic: bool| {
            let mut samples = Vec::new();
            let mut scans = 0;
            let mut jobs = 0;
            for repetition in 0..9 {
                let mut fixture = GenerationFixture::new(32, true);
                fixture.fill_steady();
                let saved = fixture.world.chunks.get(&(0, 0)).unwrap().clone();
                let mut gate = GenerationScan::default();
                gate.candidates(fixture.input(), 12);
                let start = Instant::now();
                let mut count = 0;
                let mut scheduled = 0;
                for frame in 0..FRAMES {
                    // Remove a column, queue generation, then accept a saved
                    // load completion. Both schedulers see identical events.
                    if dynamic && frame % 32 == 0 {
                        fixture.world.chunks.remove(&(0, 0));
                    }
                    if dynamic && frame % 32 == 1 {
                        fixture.generating.remove(&(0, 0));
                        fixture.world.chunks.insert((0, 0), saved.clone());
                        gate.exhausted = false; // Done::Generated
                    }
                    let result = if optimized {
                        let result = gate.candidates(black_box(fixture.input()), 12);
                        count += usize::from(result.is_some());
                        result.unwrap_or_default()
                    } else {
                        count += 1;
                        baseline_candidates(black_box(fixture.input()), 12)
                    };
                    let expected_jobs = usize::from(dynamic && frame % 32 == 0);
                    assert_eq!(result.len(), expected_jobs);
                    for (_, x, z) in black_box(result) {
                        assert_eq!((x, z), (0, 0));
                        fixture.generating.insert((x, z));
                        scheduled += 1;
                    }
                }
                if repetition != 0 {
                    samples.push(start.elapsed().as_secs_f64() * 1e9 / FRAMES as f64);
                }
                assert!(fixture.generating.is_empty());
                assert_eq!(fixture.world.chunks.len(), f.world.chunks.len());
                scans = count;
                jobs = scheduled;
            }
            samples.sort_by(f64::total_cmp);
            (samples[samples.len() / 2], scans, jobs)
        };
        println!(
            "radius=32 decorates=true positions/frame=5041 loaded_keys={} proto_keys={} frames/sample={FRAMES} warmup=1 measured=8",
            f.world.chunks.len(),
            f.protos.len()
        );
        for dynamic in [false, true] {
            let (before, baseline_scans, before_jobs) = measure(false, dynamic);
            let (after, optimized_scans, after_jobs) = measure(true, dynamic);
            assert_eq!(before_jobs, after_jobs);
            println!(
                "dynamic={dynamic}: baseline={before:.1}ns/frame optimized={after:.1}ns/frame speedup={:.2}x scans={baseline_scans}/{optimized_scans} jobs={before_jobs}/{after_jobs}",
                before / after
            );
        }
    }

    /// The ranges `Streaming::update` checks at each step, from generation to
    /// light, must let every chunk the player spawns on finish and light: the
    /// 3x3 columns around the player need their own 3x3 neighbours finished,
    /// each of those its 3x3 neighbours decorated, and each of those its 3x3
    /// neighbours generated and kept. Render distance 2 once stopped short.
    #[test]
    fn spawn_neighbourhood_completes_at_every_distance() {
        let forced = HashSet::new();
        for decorates in [true, false] {
            let (margin, reach) = Ranges::margins(decorates);
            for radius in 2..=32 {
                let r = Ranges {
                    center: (5, -7),
                    radius,
                    extra: &[],
                    forced: &forced,
                    cover: None,
                    band: None,
                };
                let square = |x: i32, z: i32, n: i32| {
                    (-n..=n).flat_map(move |dz| (-n..=n).map(move |dx| (x + dx, z + dz)))
                };
                let generated = |x, z| {
                    r.requested(x, z, decorates)
                        && (x - r.center.0).abs() <= radius + margin.max(reach + 1)
                        && (z - r.center.1).abs() <= radius + margin.max(reach + 1)
                };
                let proto_kept = |x, z| r.in_range(x, z, 4) || r.near_finished(x, z, 2);
                let decorated =
                    |x, z| !decorates || r.in_range(x, z, 2) || r.near_finished(x, z, 1);
                let chunk_kept = |x, z| r.chunk_kept(x, z, decorates);
                let unloaded = |x, z| r.unloaded(x, z, decorates);
                let finishes = |x: i32, z: i32| {
                    r.in_range(x, z, 1)
                        && chunk_kept(x, z)
                        && !unloaded(x, z)
                        && square(x, z, 1).all(|(nx, nz)| {
                            decorated(nx, nz)
                                && (!decorates
                                    || square(nx, nz, 1)
                                        .all(|(gx, gz)| generated(gx, gz) && proto_kept(gx, gz)))
                                && generated(nx, nz)
                                && proto_kept(nx, nz)
                        })
                };
                let (cx, cz) = r.center;
                for (x, z) in square(cx, cz, 1) {
                    for (nx, nz) in square(x, z, 1) {
                        assert!(
                            finishes(nx, nz),
                            "radius {radius} (decorates: {decorates}): ({nx}, {nz}) never finishes"
                        );
                    }
                }
            }
        }
    }

    /// A column generation asks for is kept when it arrives finished (loaded
    /// from the save) and is not unloaded right after: either way it would be
    /// asked for and loaded again on the next frame, forever (8 such columns
    /// after a teleport at render distance 16 held 8 of the in-flight slots).
    /// #770: while the loading screen is up, chunks go ring by ring (a
    /// square front, as vanilla's ticket levels), nearest first in a ring.
    #[test]
    fn loading_goes_ring_by_ring() {
        let forced = HashSet::new();
        let r = Ranges {
            center: (2, -1),
            radius: 12,
            extra: &[],
            forced: &forced,
            cover: None,
            band: None,
        };
        let at = |dx: i32, dz: i32| (2 + dx, -1 + dz);
        let key = |(x, z): (i32, i32), loading| r.priority(x, z, loading);
        // The corner of ring 3 comes before the edge of ring 4 only while loading.
        assert!(key(at(3, 3), true) < key(at(0, 4), true));
        assert!(key(at(3, 3), false) > key(at(0, 4), false));
        // Inside a ring, nearest first; rings never interleave.
        assert!(key(at(0, 3), true) < key(at(2, -3), true));
        assert!(key(at(-3, -3), true) < key(at(4, 0), true));
        for dx in -20..=20 {
            for dz in -20..=20 {
                assert_eq!(key(at(dx, dz), false), r.distance(2 + dx, -1 + dz));
            }
        }
    }
    #[test]
    fn requested_columns_stay_loaded() {
        let forced = HashSet::new();
        for decorates in [true, false] {
            for radius in 2..=32 {
                let r = Ranges {
                    center: (5, -7),
                    radius,
                    extra: &[((radius * 2 + 9, 3), 4)],
                    forced: &forced,
                    cover: None,
                    band: None,
                };
                let span = radius * 3 + 20;
                for x in -span..=span {
                    for z in -span..=span {
                        if r.requested(x, z, decorates) {
                            assert!(
                                r.chunk_kept(x, z, decorates),
                                "radius {radius} (decorates: {decorates}): ({x}, {z}) is asked for but dropped"
                            );
                        }
                        if r.chunk_kept(x, z, decorates) {
                            assert!(
                                !r.unloaded(x, z, decorates),
                                "radius {radius} (decorates: {decorates}): ({x}, {z}) is kept then unloaded"
                            );
                        }
                    }
                }
            }
        }
    }

    /// A server player's area (`lan_update_streaming`: the server view
    /// distance + 1) finishes vanilla's FULL square: PLAYER_LOADING tickets at
    /// level 31 out to the view distance, FULL (33) two chunks past it. Every
    /// chunk the player tracks has its 3x3 neighbourhood finished, so it can
    /// be lit and sent, and generation reaches vanilla's FEATURES (34) and
    /// TERRAIN (35) rings.
    #[test]
    fn server_player_areas_finish_vanillas_full_square() {
        let forced = HashSet::new();
        for view in 2..=32 {
            let extra = [((40, -9), view + 1)];
            let r = Ranges {
                // The window's own area, far away.
                center: (-400, 300),
                radius: 2,
                extra: &extra,
                forced: &forced,
                cover: None,
                band: None,
            };
            let level = |x: i32, z: i32| 31 + ((x - 40).abs().max((z + 9).abs()) - view).max(0);
            for x in 40 - view - 8..=40 + view + 8 {
                for z in -9 - view - 8..=-9 + view + 8 {
                    assert_eq!(
                        r.in_range(x, z, 1),
                        level(x, z) <= 33,
                        "view {view}: ({x}, {z})"
                    );
                    assert_eq!(
                        r.requested(x, z, true),
                        level(x, z) <= 35,
                        "view {view}: ({x}, {z})"
                    );
                    assert_eq!(
                        r.unloaded(x, z, true),
                        level(x, z) > 35,
                        "view {view}: ({x}, {z})"
                    );
                    // ChunkTrackingView.contains, at any view distance the player may ask for.
                    let tracked = |v: i64| {
                        let gap = |d: i32| i64::from(((d).abs() - 2).max(0));
                        gap(x - 40).pow(2) + gap(z + 9).pow(2) < v * v
                    };
                    if tracked(i64::from(view)) {
                        for (dx, dz) in (-1..=1).flat_map(|dx| (-1..=1).map(move |dz| (dx, dz))) {
                            assert!(
                                r.in_range(x + dx, z + dz, 1),
                                "view {view}: ({x}, {z}) tracked, not lit"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod cover_tests {
    use super::*;

    fn areas(seed: &mut u64, n: usize, spread: i32) -> Vec<((i32, i32), i32)> {
        let mut next = |m: i32| {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 7;
            *seed ^= *seed << 17;
            (*seed % (2 * m as u64 + 1)) as i32 - m
        };
        (0..n)
            .map(|_| ((next(spread), next(spread)), 2 + next(14).abs()))
            .collect()
    }

    /// Every question streaming asks gets the same answer with the grid.
    ///
    /// A debug build checks every fourth cell of each row, shifted by row and case so the cases still cover every
    /// cell's position relative to the areas: the full check took 20 of the ~30 s of the crate's tests.
    /// `cargo test --release -p horizon-client --lib the_grid_answers_as_the_scan` checks every cell.
    #[test]
    fn the_grid_answers_as_the_scan() {
        let stride = if cfg!(debug_assertions) { 4 } else { 1 };
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let forced: HashSet<(i32, i32)> = [(40, -40)].into();
        for case in 0..60 {
            let extra = areas(&mut seed, 2 + case % 20, [4, 30, 120][case % 3]);
            let cover = ExtraCover::new(&extra).expect("a grid");
            let ranges = |cover| Ranges {
                center: (3, -2),
                radius: 10,
                extra: &extra,
                forced: &forced,
                cover,
                band: None,
            };
            let (scan, grid) = (ranges(None), ranges(Some(&cover)));
            let span = [4, 30, 120][case % 3] + 40;
            for z in -span..=span {
                let first = -span + (z + case as i32).rem_euclid(stride);
                for x in (first..=span).step_by(stride as usize) {
                    for m in 0..=ExtraCover::REACH + 2 {
                        assert_eq!(
                            scan.in_extra(x, z, m),
                            grid.in_extra(x, z, m),
                            "case {case} ({x}, {z}) {m}"
                        );
                    }
                    for d in [false, true] {
                        assert_eq!(scan.unloaded(x, z, d), grid.unloaded(x, z, d));
                        assert_eq!(scan.requested(x, z, d), grid.requested(x, z, d));
                        assert_eq!(scan.chunk_kept(x, z, d), grid.chunk_kept(x, z, d));
                    }
                }
            }
        }
    }

    #[test]
    fn a_stale_or_impossible_grid_is_not_used() {
        let mut cover = None;
        ExtraCover::refresh(&mut cover, &[((0, 0), 11), ((5, 5), 11)]);
        assert!(ExtraCover::matching(&cover, &[((0, 0), 11), ((5, 5), 11)]).is_some());
        assert!(ExtraCover::matching(&cover, &[((0, 0), 11), ((5, 6), 11)]).is_none());
        // Too far apart for a mebibyte of grid, and one area alone: scans.
        assert!(ExtraCover::new(&[((0, 0), 11), ((100_000, 0), 11)]).is_none());
        assert!(ExtraCover::new(&[((0, 0), 11)]).is_none());
        assert!(ExtraCover::new(&[((i32::MAX - 1, 0), 11), ((i32::MAX - 3, 0), 11)]).is_none());
        assert!(ExtraCover::new(&[((0, i32::MAX - 1), 11), ((0, i32::MAX - 3), 11)]).is_none());
        assert!(ExtraCover::new(&[((0, i32::MIN + 1), 11), ((0, i32::MIN + 3), 11)]).is_none());
    }

    /// cargo test --release -p horizon-client --lib cover_tests::cover_cost -- --ignored --nocapture
    #[test]
    #[ignore]
    fn cover_cost() {
        let forced = HashSet::new();
        for players in [10usize, 25, 50] {
            // The benchmark's bots: most near spawn, a fifth flying outwards.
            let extra: Vec<_> = (0..players)
                .map(|i| {
                    let a = i as f64 * 2.399;
                    let d = if i % 5 == 0 { 30.0 } else { 3.0 };
                    (((a.cos() * d) as i32, (a.sin() * d) as i32), 11)
                })
                .collect();
            let cover = ExtraCover::new(&extra).unwrap();
            let ranges = |cover| Ranges {
                center: (0, 0),
                radius: 10,
                extra: &extra,
                forced: &forced,
                cover,
                band: None,
            };
            // The columns streaming asks about: everything kept, and the
            // generation scan's ring past it.
            let columns: Vec<(i32, i32)> = (-60..=60)
                .flat_map(|z| (-60..=60).map(move |x| (x, z)))
                .filter(|&(x, z)| {
                    ranges(None).requested(x, z, true) || !ranges(None).unloaded(x, z, true)
                })
                .collect();
            let rounds = 100;
            let time = |r: &Ranges| {
                let t = std::time::Instant::now();
                let mut n = 0usize;
                for _ in 0..rounds {
                    for &(x, z) in &columns {
                        n += usize::from(r.unloaded(x, z, true));
                        n += usize::from(r.requested(x, z, true));
                        n += usize::from(r.in_range(x, z, 1));
                    }
                }
                std::hint::black_box(n);
                t.elapsed().as_secs_f64() * 1e3 / f64::from(rounds)
            };
            let t = std::time::Instant::now();
            for _ in 0..rounds {
                std::hint::black_box(ExtraCover::new(std::hint::black_box(&extra)));
            }
            let build = t.elapsed().as_secs_f64() * 1e3 / f64::from(rounds);
            println!(
                "{players} players, {} columns, 3 questions each: scan {:.3} ms, grid {:.3} ms (+ {build:.3} ms to build)",
                columns.len(),
                time(&ranges(None)),
                time(&ranges(Some(&cover))),
            );
        }
    }
}

/// Refusal from the bounded, fresh built-in fixture streamer's capture check.
#[cfg(feature = "paired-packages")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapturePristineError {
    PreviouslySubmitted,
    QueuePoisoned,
    Stopped,
    InconsistentState,
}

#[cfg(feature = "paired-packages")]
impl Streaming {
    /// Checks the named owners without receiving results or changing streaming state.
    ///
    /// This is only one premise of the fixed fresh built-in fixture's capture:
    /// its setup must establish zero native submissions and no outside producer.
    /// `submit` increments `order` before enqueuing every native job, and workers
    /// send results only after taking a job. Under that setup, no result can be
    /// hidden in `results`. This is neither a lease nor a certificate for arbitrary
    /// generators or live history: `order` can wrap after 2^64 submissions.
    pub fn require_pristine_fixture_capture(
        &self,
    ) -> std::result::Result<(), CapturePristineError> {
        use CapturePristineError as E;
        if self.order != 0 {
            return Err(E::PreviouslySubmitted);
        }
        let queue = self.shared.queue.lock().map_err(|_| E::QueuePoisoned)?;
        if self.shared.stop.load(Ordering::Relaxed) {
            return Err(E::Stopped);
        }
        if !queue.is_empty()
            || !self.generating.is_empty()
            || !self.finishing.is_empty()
            || self.decorating != 0
            || self.saving != 0
            || !self.protos.is_empty()
            || !self.written.is_empty()
            || !self.failed_finished.is_empty()
            || !self.deferred.is_empty()
            || self.generation_save_error.is_some()
            || !self.lighting.is_empty()
            || !self.meshing.is_empty()
            || !self.light_snapshots.is_empty()
        {
            return Err(E::InconsistentState);
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "paired-packages", target_os = "linux"))]
mod capture_pristine_tests {
    use super::*;
    use std::time::Duration;

    fn context() -> Arc<WorkerContext> {
        use crate::{assets::Assets, biome::Biomes, blocks::BlockFacts, models};
        static CONTEXT: std::sync::OnceLock<Arc<WorkerContext>> = std::sync::OnceLock::new();
        CONTEXT
            .get_or_init(|| {
                let root = std::env::var_os("HORIZON_TEST_RESOURCES")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| {
                        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                            .join("../../.local/horizon-client/26.3")
                    });
                assert!(
                    root.join("items.json").is_file(),
                    "owned prepared resources required"
                );
                let assets = Arc::new(Assets::load(&root).unwrap());
                let render = Arc::new(assets.reload(&root).unwrap());
                let facts = BlockFacts::load(&root, assets.registry).unwrap();
                let visibility = models::load_visibility(&root, assets.registry).unwrap();
                Arc::new(WorkerContext {
                    tables: Arc::new(
                        MeshTables::new(&assets, &render, &facts, visibility).unwrap(),
                    ),
                    light: Arc::new(LightTables::load(&root, assets.registry, &facts).unwrap()),
                    biomes: Arc::new(Biomes::load(&root).unwrap()),
                    assets,
                    render,
                })
            })
            .clone()
    }

    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    #[test]
    #[ignore = "owned prepared resources: real worker column gate cancellation, occupied slot, stream-drop release and complete worker join"]
    fn native_column_load_control_drop_before_entry_and_stream_drop_release_actual_workers() {
        for cancel_before_entry in [false, true] {
            let context = context();
            let generator = Arc::new(
                crate::testgen::TestTerrain::new(context.assets.registry, &context.biomes).unwrap(),
            );
            let mut streaming = Streaming::new(generator, context, 1);
            let mut control = streaming
                .arm_native_column_load_test_control((0, 0))
                .unwrap();
            let observer = control.observer();
            assert!(
                streaming
                    .arm_native_column_load_test_control((1, 0))
                    .is_err()
            );
            let book = streaming.shared.ownership.clone();
            if cancel_before_entry {
                control.release();
            }
            streaming.submit(Some((0, 0)), 0, Job::Generate(0, 0));
            if !cancel_before_entry {
                control.wait_entered(Duration::from_secs(5)).unwrap();
                assert_eq!(
                    book.lock().unwrap().counts[NativeWorkOrigin::Generate as usize].worker_held,
                    1
                );
                // The external receipt deliberately remains alive during Drop.
                drop(streaming);
                assert!(observer.entered() && observer.resumed());
            } else {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while book.lock().unwrap().counts[NativeWorkOrigin::Generate as usize].returned == 0
                {
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::yield_now();
                }
                drop(streaming);
                assert!(!observer.entered());
            }
            drop(control);
            let book = book.lock().unwrap();
            assert_eq!(
                book.counts[NativeWorkOrigin::Generate as usize].worker_held,
                0
            );
            assert!(
                !book.worker_initialization_fault
                    && !book.worker_exit_fault
                    && !book.accounting_fault
            );
        }
    }

    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    #[test]
    #[ignore = "owned prepared resources: old native worker/receipt cleanup cannot remove a newer column gate"]
    fn native_column_load_control_old_receipt_and_worker_preserve_new_gate_identity() {
        let context = context();
        let generator = Arc::new(
            crate::testgen::TestTerrain::new(context.assets.registry, &context.biomes).unwrap(),
        );
        let mut streaming = Streaming::new(generator, context, 1);
        let mut old = streaming
            .arm_native_column_load_test_control((0, 0))
            .unwrap();
        streaming.submit(Some((0, 0)), 0, Job::Generate(0, 0));
        old.wait_entered(Duration::from_secs(5)).unwrap();
        old.release();
        let newer = streaming
            .arm_native_column_load_test_control((99, 99))
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while streaming.shared.ownership.lock().unwrap().counts[NativeWorkOrigin::Generate as usize]
            .returned
            == 0
        {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        drop(old);
        assert!(
            streaming
                .arm_native_column_load_test_control((1, 0))
                .is_err()
        );
        drop(newer);
        let cancelled = streaming
            .arm_native_column_load_test_control((1, 0))
            .unwrap();
        drop(cancelled);
        drop(streaming);
    }

    #[test]
    #[ignore = "owned prepared resources: pristine built-in capture streamer"]
    fn capture_pristine_builtin_constructor_is_repeatable_without_mutation() {
        let context = context();
        let generator = Arc::new(
            crate::testgen::TestTerrain::new(context.assets.registry, &context.biomes).unwrap(),
        );
        let streaming = Streaming::new(generator, context, 1);
        for _ in 0..3 {
            assert_eq!(streaming.require_pristine_fixture_capture(), Ok(()));
            assert_eq!(streaming.order, 0);
            assert!(streaming.shared.queue.lock().unwrap().is_empty());
            assert!(streaming.generating.is_empty());
            assert!(streaming.finishing.is_empty());
            assert_eq!(streaming.decorating, 0);
            assert_eq!(streaming.saving, 0);
            assert!(streaming.protos.is_empty());
            assert!(streaming.written.is_empty());
            assert!(streaming.failed_finished.is_empty());
            assert!(streaming.deferred.is_empty());
            assert!(streaming.generation_save_error.is_none());
            assert!(streaming.lighting.is_empty());
            assert!(streaming.meshing.is_empty());
            assert!(streaming.light_snapshots.is_empty());
            assert!(!streaming.shared.stop.load(Ordering::Relaxed));
        }
    }

    // Only negative cases use this generator: its gates expose actual native
    // queue/worker/result transitions, and do not establish fixture provenance.
    struct SaveGate {
        worker_started: mpsc::Sender<()>,
        worker_gate: Mutex<Option<mpsc::Receiver<()>>>,
        save_started: mpsc::Sender<u64>,
        save_gates: Mutex<[Option<mpsc::Receiver<()>>; 2]>,
    }
    impl ChunkGenerator for SaveGate {
        type Worker = ();
        fn worker(&self) {
            self.worker_started.send(()).unwrap();
            self.worker_gate
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
        }
        fn proto(&self, _: &mut (), _: i32, _: i32) -> Proto {
            unreachable!("this negative fixture only submits native Save jobs")
        }
        fn finish(&self, _: Proto) -> Chunk {
            unreachable!("this negative fixture only submits native Save jobs")
        }
        fn save_protos(&self, protos: &[&Proto]) -> Result<(), String> {
            let id = *protos[0].downcast_ref::<u64>().unwrap();
            let index = match id {
                11 => 0,
                22 => 1,
                _ => unreachable!(),
            };
            let gate = self.save_gates.lock().unwrap()[index].take().unwrap();
            self.save_started.send(id).unwrap();
            gate.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(())
        }
    }

    struct ReleaseOnDrop(Option<mpsc::Sender<()>>);
    impl ReleaseOnDrop {
        fn release(&mut self) {
            if let Some(send) = self.0.take() {
                let _ = send.send(());
            }
        }
    }
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.release();
        }
    }

    fn returned_save(done: Done, expected: u64) {
        let Done::Saved(protos, outcome) = done else {
            panic!("native worker returned a non-save completion")
        };
        assert!(outcome.is_ok());
        assert_eq!(protos.len(), 1);
        assert_eq!(*protos[0].1.downcast_ref::<u64>().unwrap(), expected);
    }

    #[test]
    #[ignore = "owned prepared resources: native queued in-flight and returned capture refusal"]
    fn capture_pristine_refuses_native_queued_inflight_and_returned_work_without_draining() {
        let (worker_started, worker_began) = mpsc::channel();
        let (worker_release, worker_gate) = mpsc::channel();
        let (save_started, save_began) = mpsc::channel();
        let (first_release, first_gate) = mpsc::channel();
        let (second_release, second_gate) = mpsc::channel();
        let generator = Arc::new(SaveGate {
            worker_started,
            worker_gate: Mutex::new(Some(worker_gate)),
            save_started,
            save_gates: Mutex::new([Some(first_gate), Some(second_gate)]),
        });
        let mut streaming = Streaming::new(generator, context(), 1);
        // Declared after Streaming so assertion unwinding releases every gate
        // before Streaming's unchanged Drop joins the native worker.
        let mut worker_release = ReleaseOnDrop(Some(worker_release));
        let mut first_release = ReleaseOnDrop(Some(first_release));
        let mut second_release = ReleaseOnDrop(Some(second_release));
        worker_began.recv_timeout(Duration::from_secs(5)).unwrap();
        streaming.submit(None, 0, Job::Save(vec![((0, 0), Box::new(11_u64))]));
        streaming.submit(None, 0, Job::Save(vec![((1, 0), Box::new(22_u64))]));
        assert_eq!(streaming.shared.queue.lock().unwrap().len(), 2);
        assert_eq!(
            streaming.require_pristine_fixture_capture(),
            Err(CapturePristineError::PreviouslySubmitted)
        );
        assert_eq!(streaming.order, 2);
        assert_eq!(streaming.shared.queue.lock().unwrap().len(), 2);

        worker_release.release();
        assert_eq!(save_began.recv_timeout(Duration::from_secs(5)).unwrap(), 11);
        assert_eq!(streaming.shared.queue.lock().unwrap().len(), 1);
        assert_eq!(
            streaming.require_pristine_fixture_capture(),
            Err(CapturePristineError::PreviouslySubmitted)
        );
        assert_eq!(streaming.shared.queue.lock().unwrap().len(), 1);

        first_release.release();
        assert_eq!(save_began.recv_timeout(Duration::from_secs(5)).unwrap(), 22);
        // The worker starts the second job only AFTER sending the first Done.
        // The real first result is now waiting in Streaming's original channel.
        assert!(streaming.shared.queue.lock().unwrap().is_empty());
        assert_eq!(
            streaming.require_pristine_fixture_capture(),
            Err(CapturePristineError::PreviouslySubmitted)
        );
        returned_save(
            streaming
                .results
                .recv_timeout(Duration::from_secs(5))
                .unwrap(),
            11,
        );
        second_release.release();
        returned_save(
            streaming
                .results
                .recv_timeout(Duration::from_secs(5))
                .unwrap(),
            22,
        );
        // Even after the test observes both actual native completions, no reset
        // or successful admission is possible for this submitted streamer.
        assert_eq!(
            streaming.require_pristine_fixture_capture(),
            Err(CapturePristineError::PreviouslySubmitted)
        );
        assert_eq!(streaming.order, 2);
        assert!(streaming.deferred.is_empty());
    }
}

#[cfg(all(test, feature = "paired-packages", target_os = "linux"))]
mod native_work_tests {
    use super::*;
    use std::time::{Duration, Instant};
    const WAIT: Duration = Duration::from_secs(5);

    fn context() -> Arc<WorkerContext> {
        use crate::{assets::Assets, biome::Biomes, blocks::BlockFacts, models};
        static CONTEXT: std::sync::OnceLock<Arc<WorkerContext>> = std::sync::OnceLock::new();
        CONTEXT
            .get_or_init(|| {
                let root = std::env::var_os("HORIZON_TEST_RESOURCES")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| {
                        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                            .join("../../.local/horizon-client/26.3")
                    });
                assert!(
                    root.join("items.json").is_file(),
                    "owned prepared resources required"
                );
                let assets = Arc::new(Assets::load(&root).unwrap());
                let render = Arc::new(assets.reload(&root).unwrap());
                let facts = BlockFacts::load(&root, assets.registry).unwrap();
                let visibility = models::load_visibility(&root, assets.registry).unwrap();
                Arc::new(WorkerContext {
                    tables: Arc::new(
                        MeshTables::new(&assets, &render, &facts, visibility).unwrap(),
                    ),
                    light: Arc::new(LightTables::load(&root, assets.registry, &facts).unwrap()),
                    biomes: Arc::new(Biomes::load(&root).unwrap()),
                    assets,
                    render,
                })
            })
            .clone()
    }
    struct Release(Option<mpsc::Sender<()>>);
    impl Release {
        fn release(&mut self) {
            if let Some(send) = self.0.take() {
                let _ = send.send(());
            }
        }
    }
    impl Drop for Release {
        fn drop(&mut self) {
            self.release();
        }
    }
    struct Payload {
        value: u64,
        at: (i32, i32),
        alive: Arc<()>,
    }
    struct Probe {
        worker_started: mpsc::Sender<()>,
        worker_gate: Mutex<Option<mpsc::Receiver<()>>>,
        events: mpsc::Sender<NativeWorkOrigin>,
        gates: Mutex<VecDeque<(NativeWorkOrigin, mpsc::Receiver<()>)>>,
        failure: Mutex<Option<String>>,
        seen: Mutex<Vec<(u64, usize)>>,
        load: bool,
        panic_init: bool,
        panic_job: bool,
    }
    impl Probe {
        fn gate(&self, origin: NativeWorkOrigin) {
            self.events.send(origin).unwrap();
            let gate = {
                let mut gates = self.gates.lock().unwrap();
                gates
                    .iter()
                    .position(|(wanted, _)| *wanted == origin)
                    .and_then(|index| gates.remove(index).map(|(_, gate)| gate))
            };
            if let Some(gate) = gate {
                gate.recv_timeout(WAIT).unwrap();
            }
            assert!(!self.panic_job, "controlled native job panic");
        }
        fn outcome(&self) -> Result<(), String> {
            self.failure.lock().unwrap().clone().map_or(Ok(()), Err)
        }
    }
    impl ChunkGenerator for Probe {
        type Worker = ();
        fn worker(&self) {
            self.worker_started.send(()).unwrap();
            if let Some(gate) = self.worker_gate.lock().unwrap().take() {
                gate.recv_timeout(WAIT).unwrap();
            }
            assert!(
                !self.panic_init,
                "controlled native worker initialization panic"
            );
        }
        fn load(&self, x: i32, z: i32) -> Option<Chunk> {
            if self.load {
                self.gate(NativeWorkOrigin::Generate);
                Some(Chunk::empty(x, z))
            } else {
                None
            }
        }
        fn proto(&self, _: &mut (), x: i32, z: i32) -> Proto {
            self.gate(NativeWorkOrigin::Generate);
            Box::new(Payload {
                value: 91,
                at: (x, z),
                alive: Arc::new(()),
            })
        }
        fn finish(&self, proto: Proto) -> Chunk {
            self.gate(NativeWorkOrigin::Finish);
            let payload = proto
                .downcast::<Payload>()
                .unwrap_or_else(|_| panic!("native payload type"));
            assert_eq!(Arc::strong_count(&payload.alive), 1);
            Chunk::empty(payload.at.0, payload.at.1)
        }
        fn save_protos(&self, protos: &[&Proto]) -> Result<(), String> {
            self.gate(NativeWorkOrigin::Save);
            for proto in protos {
                let payload = proto.downcast_ref::<Payload>().unwrap();
                self.seen
                    .lock()
                    .unwrap()
                    .push((payload.value, payload as *const Payload as usize));
            }
            self.outcome()
        }
        fn save_finished(&self, chunk: &Arc<Chunk>) -> Result<(), String> {
            self.gate(NativeWorkOrigin::SaveFinishedCallback);
            assert_eq!(
                chunk.entities[0].get("payload"),
                Some(&horizon_world::nbt::Tag::Long(731))
            );
            self.outcome()
        }
    }
    fn fixture(
        threads: usize,
        load: bool,
        panic_init: bool,
        panic_job: bool,
    ) -> (
        Streaming,
        Arc<Probe>,
        mpsc::Receiver<()>,
        mpsc::Receiver<NativeWorkOrigin>,
        Release,
    ) {
        let (worker_started, workers) = mpsc::channel();
        let (release, worker_gate) = mpsc::channel();
        let (events, received) = mpsc::channel();
        let probe = Arc::new(Probe {
            worker_started,
            worker_gate: Mutex::new(Some(worker_gate)),
            events,
            gates: Mutex::new(VecDeque::new()),
            failure: Mutex::new(None),
            seen: Mutex::new(Vec::new()),
            load,
            panic_init,
            panic_job,
        });
        let mut streaming = Streaming::new(probe.clone(), context(), threads);
        streaming.remote = true;
        streaming.mesh = false;
        (streaming, probe, workers, received, Release(Some(release)))
    }
    fn gate(probe: &Probe, origin: NativeWorkOrigin) -> Release {
        let (send, receive) = mpsc::channel();
        probe.gates.lock().unwrap().push_back((origin, receive));
        Release(Some(send))
    }
    fn counts(streaming: &Streaming, origin: NativeWorkOrigin) -> NativeWorkCounts {
        streaming.observe_native_work().origins[origin as usize].1
    }
    fn payload(value: u64, at: (i32, i32)) -> (Proto, std::sync::Weak<()>, usize) {
        let alive = Arc::new(());
        let weak = Arc::downgrade(&alive);
        let boxed = Box::new(Payload { value, at, alive });
        let ptr = &*boxed as *const Payload as usize;
        (boxed, weak, ptr)
    }
    fn world() -> World {
        World::new(
            context()
                .assets
                .registry
                .default_state("minecraft:air")
                .unwrap(),
            0,
        )
    }

    #[test]
    #[ignore = "owned prepared resources: actual native handoffs and borrowed observation"]
    fn native_work_borrowed_observation_and_explicit_origin_handoffs() {
        let (mut streaming, probe, workers, events, mut worker_release) =
            fixture(1, false, false, false);
        workers.recv_timeout(WAIT).unwrap();
        let mut generate_release = gate(&probe, NativeWorkOrigin::Generate);
        let mut finish_release = gate(&probe, NativeWorkOrigin::Finish);
        let (finish, finish_alive, _) = payload(72, (0, 0));
        streaming.submit(None, 0, Job::Generate(1, 0));
        streaming.submit(None, 0, Job::Finish(finish));
        let before = streaming.observe_native_work();
        for _ in 0..3 {
            assert_eq!(streaming.observe_native_work(), before);
        }
        assert_eq!(streaming.shared.queue.lock().unwrap().len(), 2);
        assert_eq!(counts(&streaming, NativeWorkOrigin::Generate).queued, 1);
        assert_eq!(counts(&streaming, NativeWorkOrigin::Finish).queued, 1);
        assert!(finish_alive.upgrade().is_some());
        worker_release.release();
        assert_eq!(
            events.recv_timeout(WAIT).unwrap(),
            NativeWorkOrigin::Generate
        );
        assert_eq!(
            counts(&streaming, NativeWorkOrigin::Generate).worker_held,
            1
        );
        generate_release.release();
        assert_eq!(events.recv_timeout(WAIT).unwrap(), NativeWorkOrigin::Finish);
        assert_eq!(counts(&streaming, NativeWorkOrigin::Generate).returned, 1);
        assert_eq!(counts(&streaming, NativeWorkOrigin::Finish).worker_held, 1);
        streaming.wait(WAIT);
        let Done::Proto(1, 0, ref proto, false) = streaming.deferred[0] else {
            panic!("actual proto result")
        };
        assert_eq!(proto.downcast_ref::<Payload>().unwrap().value, 91);
        assert_eq!(
            streaming.deferred_owners[0].1.origin,
            NativeWorkOrigin::Generate
        );
        assert_eq!(
            streaming.require_pristine_fixture_capture(),
            Err(CapturePristineError::PreviouslySubmitted)
        );
        finish_release.release();
        streaming.wait(WAIT);
        assert!(finish_alive.upgrade().is_none());
        assert_eq!(
            streaming.deferred_owners[1].1.origin,
            NativeWorkOrigin::Finish
        );
        let mut world = world();
        streaming.update(&mut world);
        assert_eq!(counts(&streaming, NativeWorkOrigin::Generate).disposed, 1);
        assert_eq!(counts(&streaming, NativeWorkOrigin::Finish).disposed, 1);
        assert!(!streaming.observe_native_work().receipt_mismatch);
        assert_eq!(
            streaming.require_pristine_fixture_capture(),
            Err(CapturePristineError::PreviouslySubmitted)
        );
        let region: Box<[Proto; 9]> = Box::new(std::array::from_fn(|i| {
            Box::new(Payload {
                value: 100 + i as u64,
                at: (9 + (i % 3) as i32, 9 + (i / 3) as i32),
                alive: Arc::new(()),
            }) as Proto
        }));
        let expected: Vec<_> = region
            .iter()
            .map(|proto| {
                let payload = proto.downcast_ref::<Payload>().unwrap();
                (
                    payload.at,
                    payload.value,
                    payload as *const Payload as usize,
                )
            })
            .collect();
        streaming.decorating = 1;
        streaming.submit(None, 0, Job::Decorate(10, 10, region));
        streaming.wait(WAIT);
        assert_eq!(counts(&streaming, NativeWorkOrigin::Decorate).returned, 1);
        streaming.reconcile_ready_save_results(&mut world);
        assert_eq!(counts(&streaming, NativeWorkOrigin::Decorate).disposed, 1);
        for (at, value, address) in expected {
            let payload = streaming.protos[&at]
                .as_ref()
                .unwrap()
                .downcast_ref::<Payload>()
                .unwrap();
            assert_eq!(
                (payload.value, payload as *const Payload as usize),
                (value, address)
            );
        }
    }

    #[test]
    #[ignore = "owned prepared resources: native light and mesh cancelled indexes retain results"]
    fn native_work_forget_reload_and_stale_result_disposal_keep_ownership() {
        let (mut streaming, probe, workers, events, mut worker_release) =
            fixture(1, false, false, false);
        workers.recv_timeout(WAIT).unwrap();
        let mut save_release = gate(&probe, NativeWorkOrigin::Save);
        let (light_entered, light_started) = mpsc::channel();
        let (light_release, light_gate) = mpsc::channel();
        *streaming.shared.work_gate.lock().unwrap() = Some(WorkGate {
            origin: NativeWorkOrigin::Light,
            entered: light_entered,
            release: light_gate,
        });
        let mut light_release = Release(Some(light_release));
        let mut world = world();
        world.chunks.insert((0, 0), Arc::new(Chunk::empty(0, 0)));
        let region = || {
            Box::new(std::array::from_fn(|z| {
                std::array::from_fn(|x| Some(Arc::new(Chunk::empty(x as i32 - 1, z as i32 - 1))))
            }))
        };
        streaming.lighting.insert((0, 0), 41);
        streaming.meshing.insert((0, 0, 0), 42);
        streaming.submit(None, 0, Job::Light(0, 0, region(), 41, true));
        streaming.submit(
            None,
            0,
            Job::Mesh(
                0,
                0,
                0,
                region(),
                42,
                MeshSettings {
                    nether: false,
                    zoom: BiomeZoom::new(0),
                    blend_radius: 0,
                    smooth_lighting: true,
                    cutout_leaves: false,
                },
            ),
        );
        let (save, alive, address) = payload(115, (4, 4));
        streaming.submit(None, 0, Job::Save(vec![((4, 4), save)]));
        worker_release.release();
        light_started.recv_timeout(WAIT).unwrap();
        assert_eq!(counts(&streaming, NativeWorkOrigin::Light).worker_held, 1);
        assert_eq!(counts(&streaming, NativeWorkOrigin::Mesh).queued, 1);
        let held = streaming.forget(&mut world, 0, 0).unwrap();
        streaming.reload_resources(&mut world, context());
        assert!(streaming.lighting.is_empty() && streaming.meshing.is_empty());
        assert_eq!(counts(&streaming, NativeWorkOrigin::Light).worker_held, 1);
        assert_eq!(counts(&streaming, NativeWorkOrigin::Mesh).queued, 1);
        light_release.release();
        assert_eq!(events.recv_timeout(WAIT).unwrap(), NativeWorkOrigin::Save);
        assert_eq!(counts(&streaming, NativeWorkOrigin::Light).returned, 1);
        assert_eq!(counts(&streaming, NativeWorkOrigin::Mesh).returned, 1);
        let before = streaming.observe_native_work();
        assert_eq!(streaming.observe_native_work(), before);
        assert_eq!(
            before.origins[NativeWorkOrigin::Save as usize]
                .1
                .worker_held,
            1
        );
        assert!(alive.upgrade().is_some());
        streaming.wait(WAIT);
        streaming.wait(WAIT);
        assert_eq!(streaming.deferred.len(), 2);
        assert_eq!(counts(&streaming, NativeWorkOrigin::Light).returned, 1);
        streaming.update(&mut world); // Native stale sequence branches dispose both results.
        assert_eq!(counts(&streaming, NativeWorkOrigin::Light).disposed, 1);
        assert_eq!(counts(&streaming, NativeWorkOrigin::Mesh).disposed, 1);
        assert_eq!(held.x, 0);
        save_release.release();
        streaming.wait(WAIT);
        let Done::Saved(ref protos, Ok(())) = streaming.deferred[0] else {
            panic!("native save result")
        };
        assert_eq!(
            protos[0].1.downcast_ref::<Payload>().unwrap() as *const Payload as usize,
            address
        );
        assert_eq!(*probe.seen.lock().unwrap(), [(115, address)]);
        // This test submitted a bare save directly; restore its native checkout count.
        streaming.saving = 1;
        streaming.reconcile_ready_save_results(&mut world);
        assert_eq!(
            counts(&streaming, NativeWorkOrigin::Save).accepted_storage_handoffs,
            1
        );
        assert_eq!(counts(&streaming, NativeWorkOrigin::Save).disposed, 1);
        assert!(alive.upgrade().is_some());
    }

    #[test]
    #[ignore = "owned prepared resources: synchronous callbacks and native save failure retention"]
    fn native_work_storage_callbacks_report_enqueue_acceptance_and_partial_failure() {
        let (mut streaming, probe, workers, events, mut worker_release) =
            fixture(1, false, false, false);
        workers.recv_timeout(WAIT).unwrap();
        worker_release.release();
        let mut world = world();
        let (proto, alive, address) = payload(991, (0, 0));
        streaming.protos.insert((0, 0), Some(proto));
        streaming.written.insert((0, 0));
        streaming
            .save_until(&mut world, Instant::now() + WAIT)
            .unwrap();
        assert_eq!(events.recv_timeout(WAIT).unwrap(), NativeWorkOrigin::Save);
        assert_eq!(
            counts(&streaming, NativeWorkOrigin::SaveProtosCallback).accepted_storage_handoffs,
            1
        );
        assert_eq!(*probe.seen.lock().unwrap(), [(991, address)]);
        assert!(alive.upgrade().is_some());
        *probe.failure.lock().unwrap() =
            Some("controlled accepted-prefix/entity-enqueue failure".into());
        streaming.written.insert((0, 0));
        assert!(
            streaming
                .save_until(&mut world, Instant::now() + WAIT)
                .is_err()
        );
        assert_eq!(
            streaming.protos[&(0, 0)]
                .as_ref()
                .unwrap()
                .downcast_ref::<Payload>()
                .unwrap() as *const Payload as usize,
            address
        );
        assert!(streaming.written.contains(&(0, 0)));
        let c = counts(&streaming, NativeWorkOrigin::SaveProtosCallback);
        assert_eq!(
            (c.accepted_storage_handoffs, c.storage_errors, c.unresolved),
            (1, 1, 1)
        );
        assert!(streaming.generation_save_error().is_some());
        assert_eq!(events.recv_timeout(WAIT).unwrap(), NativeWorkOrigin::Save);
        // A separate real callback retains its complete failed column, including NBT.
        let (mut finished, finished_probe, started, _finished_events, mut release) =
            fixture(1, false, false, false);
        started.recv_timeout(WAIT).unwrap();
        release.release();
        *finished_probe.failure.lock().unwrap() = Some("partial native finished enqueue".into());
        let mut chunk = Chunk::empty(1000, 1000);
        chunk.fresh = true;
        chunk.entities.push(horizon_world::nbt::Tag::compound([(
            "payload",
            horizon_world::nbt::Tag::Long(731),
        )]));
        finished.receive_generated(chunk, &mut world);
        let saved = &finished.failed_finished[0];
        assert_eq!(
            saved.entities[0].get("payload"),
            Some(&horizon_world::nbt::Tag::Long(731))
        );
        let ptr = Arc::as_ptr(saved);
        let before = finished.observe_native_work();
        assert_eq!(finished.observe_native_work(), before);
        assert_eq!(Arc::as_ptr(&finished.failed_finished[0]), ptr);
        let c = counts(&finished, NativeWorkOrigin::SaveFinishedCallback);
        assert_eq!(
            (c.storage_errors, c.unresolved, c.accepted_storage_handoffs),
            (1, 1, 0)
        );
        let (mut asynchronous, async_probe, started, _async_events, mut release) =
            fixture(1, false, false, false);
        started.recv_timeout(WAIT).unwrap();
        *async_probe.failure.lock().unwrap() = Some("native worker storage refusal".into());
        let (proto, alive, address) = payload(882, (2, 3));
        asynchronous.protos.insert((2, 3), Some(proto));
        asynchronous.written.insert((2, 3));
        asynchronous.save(&mut world, false).unwrap();
        assert!(asynchronous.protos[&(2, 3)].is_none());
        release.release();
        asynchronous.wait(WAIT);
        assert_eq!(counts(&asynchronous, NativeWorkOrigin::Save).returned, 1);
        assert!(
            asynchronous
                .save_until(&mut world, Instant::now() + WAIT)
                .is_err()
        );
        let retained = asynchronous.protos[&(2, 3)]
            .as_ref()
            .unwrap()
            .downcast_ref::<Payload>()
            .unwrap();
        assert_eq!(
            (retained.value, retained as *const Payload as usize),
            (882, address)
        );
        assert!(alive.upgrade().is_some() && asynchronous.written.contains(&(2, 3)));
        let c = counts(&asynchronous, NativeWorkOrigin::Save);
        assert_eq!(
            (c.storage_errors, c.accepted_storage_handoffs, c.disposed),
            (1, 0, 1)
        );
    }

    #[test]
    #[ignore = "owned prepared resources: concurrent native workers pair send receipts in order"]
    fn native_work_concurrent_native_sends_preserve_origin_and_payload_pairing() {
        let (mut streaming, probe, workers, events, mut release) = fixture(2, true, false, false);
        workers.recv_timeout(WAIT).unwrap();
        workers.recv_timeout(WAIT).unwrap();
        let mut generate_release = gate(&probe, NativeWorkOrigin::Generate);
        let mut finish_release = gate(&probe, NativeWorkOrigin::Finish);
        let (finish, alive, _) = payload(19, (7, 8));
        streaming.submit(None, 0, Job::Generate(5, 6));
        streaming.submit(None, 0, Job::Finish(finish));
        release.release();
        let mut entered = [
            events.recv_timeout(WAIT).unwrap(),
            events.recv_timeout(WAIT).unwrap(),
        ];
        entered.sort_by_key(|origin| *origin as usize);
        assert_eq!(
            entered,
            [NativeWorkOrigin::Generate, NativeWorkOrigin::Finish]
        );
        assert_eq!(
            counts(&streaming, NativeWorkOrigin::Generate).worker_held,
            1
        );
        assert_eq!(counts(&streaming, NativeWorkOrigin::Finish).worker_held, 1);
        assert!(alive.upgrade().is_some());
        generate_release.release();
        finish_release.release();
        streaming.wait(WAIT);
        streaming.wait(WAIT);
        assert_eq!(streaming.deferred.len(), 2);
        for (index, owner) in &streaming.deferred_owners {
            let Done::Generated(ref chunk) = streaming.deferred[*index] else {
                panic!("native generated result")
            };
            assert_eq!(
                (chunk.x, chunk.z),
                match owner.origin {
                    NativeWorkOrigin::Generate => (5, 6),
                    NativeWorkOrigin::Finish => (7, 8),
                    _ => panic!("lost origin"),
                }
            );
        }
        assert!(alive.upgrade().is_none());
        let mut world = world();
        assert!(
            !streaming
                .poll_save_budget(&mut world, Duration::ZERO)
                .unwrap()
        );
        assert_eq!(
            streaming.deferred.len(),
            1,
            "early native receive preserves its owned tail"
        );
        assert_eq!(streaming.deferred_owners.len(), 1);
        let tail = streaming.deferred_owners[0].1.origin;
        assert_eq!(counts(&streaming, tail).returned, 1);
        assert_eq!(counts(&streaming, tail).disposed, 0);
        streaming.update(&mut world);
        assert_eq!(counts(&streaming, NativeWorkOrigin::Generate).disposed, 1);
        assert_eq!(counts(&streaming, NativeWorkOrigin::Finish).disposed, 1);
        assert!(!streaming.observe_native_work().receipt_mismatch);
    }

    #[test]
    #[ignore = "owned prepared resources: native panic disconnection and unprocessed Drop"]
    fn native_work_worker_faults_failed_send_and_unprocessed_drop_stay_named() {
        {
            let (streaming, _probe, workers, _events, mut release) =
                fixture(1, false, false, false);
            workers.recv_timeout(WAIT).unwrap();
            let shared = streaming.shared.clone();
            let (idle_entered, idle_seen) = mpsc::channel();
            let (idle_release, idle_gate) = mpsc::channel();
            let mut idle_release = Release(Some(idle_release));
            *shared.idle_wait_gate.lock().unwrap() = Some(IdleWaitGate {
                entered: idle_entered,
                release: idle_gate,
            });
            let (shutdown, shutdown_seen) = mpsc::channel();
            *shared.shutdown_events.lock().unwrap() = Some(shutdown);
            release.release();
            idle_seen.recv_timeout(WAIT).unwrap();
            // The actual worker has checked stop=false and an empty queue,
            // and still holds that same mutex immediately before its wait.
            let (completed, completion) = mpsc::channel();
            let dropping = thread::spawn(move || {
                drop(streaming);
                let _ = completed.send(());
            });
            let first = shutdown_seen.recv_timeout(WAIT);
            let stop_while_worker_holds_queue = shared.stop.load(Ordering::Relaxed);
            idle_release.release();
            let rescue = || {
                // Baseline publishes/notifies before this worker enters wait.
                // Acquire its queue mutex after releasing the gate, so this
                // failure-only wake cannot itself fall into the pre-wait gap.
                let queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                shared.ready.notify_all();
                drop(queue);
            };
            if first != Ok(ShutdownEvent::QueueContended) {
                rescue();
            }
            let mut finished = completion.recv_timeout(WAIT);
            let completed_before_rescue = finished.is_ok();
            if finished.is_err() {
                rescue();
                finished = completion.recv_timeout(WAIT);
            }
            if finished.is_err() {
                // A broken shutdown must fail this bounded test, not join
                // indefinitely. No such detachment occurs on the passing path.
                drop(dropping);
                panic!("native idle Streaming::drop did not finish after the rescue wake");
            }
            dropping.join().unwrap();
            assert_eq!(first, Ok(ShutdownEvent::QueueContended));
            assert!(
                completed_before_rescue,
                "normal idle shutdown failed; a cleanup rescue cannot qualify as success"
            );
            assert!(!stop_while_worker_holds_queue);
            assert_eq!(
                shutdown_seen.recv_timeout(WAIT),
                Ok(ShutdownEvent::Published)
            );
            assert!(shared.stop.load(Ordering::Relaxed));
            let book = shared.ownership.lock().unwrap();
            assert_eq!(book.initialized_workers, 1);
            assert!(!book.worker_initialization_fault && !book.worker_exit_fault);
            assert!(
                book.counts
                    .iter()
                    .all(|counts| *counts == NativeWorkCounts::default())
            );
        }
        for blocking in [false, true] {
            let (mut streaming, _probe, workers, _events, mut release) =
                fixture(1, false, false, false);
            workers.recv_timeout(WAIT).unwrap();
            release.release();
            let (send, receive) = mpsc::channel();
            drop(send);
            streaming.results = receive;
            assert!(!streaming.observe_native_work().result_disconnected);
            let mut world = world();
            if blocking {
                streaming
                    .save_until(&mut world, Instant::now() + WAIT)
                    .unwrap();
            } else {
                streaming.save(&mut world, false).unwrap();
            }
            assert!(
                streaming.observe_native_work().result_disconnected,
                "idle direct reconciliation must name disconnection"
            );
        }
        for (panic_init, panic_job, disconnect) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let (mut streaming, _probe, workers, _events, mut release) =
                fixture(1, false, panic_init, panic_job);
            workers.recv_timeout(WAIT).unwrap();
            let book = streaming.shared.ownership.clone();
            let (proto, alive, _) = payload(87, (0, 0));
            streaming.submit(None, 0, Job::Finish(proto));
            if disconnect {
                let (send, receive) = mpsc::channel();
                drop(send);
                streaming.results = receive;
            }
            release.release();
            for handle in streaming.threads.drain(..) {
                assert_eq!(handle.join().is_err(), panic_init || panic_job);
            }
            let observation = streaming.observe_native_work();
            assert_eq!(observation.worker_initialization_fault, panic_init);
            assert_eq!(observation.worker_exit_fault, !panic_init);
            assert_eq!(observation.result_disconnected, disconnect);
            if panic_init {
                assert!(alive.upgrade().is_some());
            }
            drop(streaming);
            assert!(alive.upgrade().is_none());
            assert_eq!(
                book.lock().unwrap().counts[NativeWorkOrigin::Finish as usize].unresolved,
                1
            );
        }
        let (mut streaming, _probe, workers, _events, mut release) =
            fixture(1, false, false, false);
        workers.recv_timeout(WAIT).unwrap();
        let book = streaming.shared.ownership.clone();
        streaming.submit(None, 0, Job::Generate(0, 0));
        release.release();
        streaming.wait(WAIT);
        let Done::Proto(_, _, ref proto, _) = streaming.deferred[0] else {
            panic!()
        };
        let alive = Arc::downgrade(&proto.downcast_ref::<Payload>().unwrap().alive);
        assert_eq!(counts(&streaming, NativeWorkOrigin::Generate).returned, 1);
        assert!(alive.upgrade().is_some());
        drop(streaming);
        assert!(alive.upgrade().is_none());
        assert_eq!(
            book.lock().unwrap().counts[NativeWorkOrigin::Generate as usize].unresolved,
            1
        );
    }

    mod write_fence {
        use super::*;

        fn initialized(streaming: &Streaming, count: u64) {
            let deadline = Instant::now() + WAIT;
            while streaming.observe_native_work().initialized_workers != count {
                assert!(Instant::now() < deadline, "native initialization handshake");
                thread::yield_now();
            }
        }
        fn short() -> Instant {
            Instant::now() + Duration::from_millis(30)
        }

        #[test]
        #[ignore = "owned prepared resources: native queued/held/returned producer closure"]
        fn native_write_fence_retains_timeout_payload_then_transfers_proto_without_rescheduling() {
            let (mut streaming, probe, workers, events, mut init) = fixture(1, false, false, false);
            workers.recv_timeout(WAIT).unwrap();
            let (proto, alive, _) = payload(81, (0, 0));
            let mut held = gate(&probe, NativeWorkOrigin::Finish);
            streaming.finishing.insert((0, 0));
            streaming.submit(None, 0, Job::Finish(proto));
            streaming.generating.insert((1, 0));
            streaming.submit(None, 0, Job::Generate(1, 0));
            let mut world = world();
            let mut polls = 0;
            let original = streaming.native_write_fence_until(&mut world, short(), || {
                polls += 1;
                Ok(true)
            });
            assert!(original.unwrap_err().to_string().contains("deadline"));
            assert_eq!(polls, 0);
            assert_eq!(counts(&streaming, NativeWorkOrigin::Finish).queued, 1);
            assert!(alive.upgrade().is_some());
            init.release();
            assert_eq!(events.recv_timeout(WAIT).unwrap(), NativeWorkOrigin::Finish);
            let original = streaming.native_write_fence_until(&mut world, short(), || {
                polls += 1;
                Ok(true)
            });
            assert!(original.unwrap_err().to_string().contains("deadline"));
            assert_eq!(polls, 0);
            assert_eq!(counts(&streaming, NativeWorkOrigin::Finish).worker_held, 1);
            assert!(alive.upgrade().is_some());
            held.release();
            streaming.wait(WAIT);
            streaming.wait(WAIT);
            let Done::Proto(1, 0, ref proto, _) = streaming.deferred[1] else {
                panic!("native proto")
            };
            let address = proto.downcast_ref::<Payload>().unwrap() as *const Payload as usize;
            let submitted = streaming.order;
            streaming
                .native_write_fence_until(&mut world, Instant::now() + WAIT, || {
                    polls += 1;
                    Ok(true)
                })
                .unwrap();
            assert_eq!(polls, 1);
            assert_eq!(streaming.order, submitted);
            assert_eq!(
                streaming.protos[&(1, 0)]
                    .as_ref()
                    .unwrap()
                    .downcast_ref::<Payload>()
                    .unwrap() as *const Payload as usize,
                address
            );
            assert_eq!(counts(&streaming, NativeWorkOrigin::Generate).disposed, 1);
            assert_eq!(counts(&streaming, NativeWorkOrigin::Finish).disposed, 1);
            assert_eq!(
                streaming.require_pristine_fixture_capture(),
                Err(CapturePristineError::PreviouslySubmitted)
            );
        }

        struct InitProbe {
            entered: mpsc::Sender<usize>,
            gates: Mutex<VecDeque<mpsc::Receiver<()>>>,
            next: std::sync::atomic::AtomicUsize,
            panic_second: bool,
        }
        impl ChunkGenerator for InitProbe {
            type Worker = ();
            fn worker(&self) {
                let (id, gate) = {
                    let mut gates = self.gates.lock().unwrap();
                    (
                        self.next.fetch_add(1, Ordering::SeqCst),
                        gates.pop_front().unwrap(),
                    )
                };
                self.entered.send(id).unwrap();
                gate.recv_timeout(WAIT).unwrap();
                assert!(!(self.panic_second && id == 1), "controlled init panic");
            }
            fn proto(&self, _: &mut (), _: i32, _: i32) -> Proto {
                Box::new(())
            }
            fn finish(&self, _: Proto) -> Chunk {
                unreachable!()
            }
        }
        #[test]
        #[ignore = "owned prepared resources: every actual worker must initialize"]
        fn native_write_fence_waits_for_all_workers_and_refuses_delayed_init_panic() {
            for panic_second in [false, true] {
                let (entered, seen) = mpsc::channel();
                let (first, first_gate) = mpsc::channel();
                let (second, second_gate) = mpsc::channel();
                let generator = Arc::new(InitProbe {
                    entered,
                    gates: Mutex::new(VecDeque::from([first_gate, second_gate])),
                    next: std::sync::atomic::AtomicUsize::new(0),
                    panic_second,
                });
                let mut streaming = Streaming::new(generator, context(), 2);
                let mut first = Release(Some(first));
                let mut second = Release(Some(second));
                let mut ids = [
                    seen.recv_timeout(WAIT).unwrap(),
                    seen.recv_timeout(WAIT).unwrap(),
                ];
                ids.sort();
                assert_eq!(ids, [0, 1]);
                first.release();
                initialized(&streaming, 1);
                let mut world = world();
                let mut polls = 0;
                let original = streaming.native_write_fence_until(&mut world, short(), || {
                    polls += 1;
                    Ok(true)
                });
                assert!(original.unwrap_err().to_string().contains("deadline"));
                assert_eq!(polls, 0, "one initialized worker is insufficient");
                second.release();
                let result =
                    streaming.native_write_fence_until(&mut world, Instant::now() + WAIT, || {
                        polls += 1;
                        Ok(true)
                    });
                assert_eq!(result.is_err(), panic_second);
                assert_eq!(polls, usize::from(!panic_second));
                assert_eq!(
                    streaming.observe_native_work().worker_initialization_fault,
                    panic_second
                );
                // Streaming::drop joins both actual pool threads, including the unwind.
            }
        }

        #[test]
        #[ignore = "owned prepared resources: unrelated native payload identity and order"]
        fn native_write_fence_preserves_light_mesh_payloads_and_revalidates_late_worker_fault() {
            let (mut streaming, _probe, workers, _events, mut init) =
                fixture(1, false, false, false);
            workers.recv_timeout(WAIT).unwrap();
            let mut world = world();
            let mut center = Chunk::empty(0, 0);
            let stone = context()
                .assets
                .registry
                .default_state("minecraft:stone")
                .unwrap();
            Arc::make_mut(&mut center.sections[4]).set(0, stone.0);
            let center = Arc::new(center);
            let region = || {
                Box::new(std::array::from_fn(|z| {
                    std::array::from_fn(|x| {
                        Some(if x == 1 && z == 1 {
                            center.clone()
                        } else {
                            Arc::new(Chunk::empty(x as i32 - 1, z as i32 - 1))
                        })
                    })
                }))
            };
            streaming.submit(None, 0, Job::Light(0, 0, region(), 51, true));
            streaming.submit(
                None,
                0,
                Job::Mesh(
                    0,
                    4,
                    0,
                    region(),
                    52,
                    MeshSettings {
                        nether: false,
                        zoom: BiomeZoom::new(0),
                        blend_radius: 0,
                        smooth_lighting: true,
                        cutout_leaves: false,
                    },
                ),
            );
            init.release();
            streaming.wait(WAIT);
            streaming.wait(WAIT);
            let identities = |streaming: &Streaming| {
                streaming
                    .deferred
                    .iter()
                    .map(|done| match done {
                        Done::Lit(_, _, sections, seq) => {
                            (0, *seq, sections.as_ptr() as usize, sections.len())
                        }
                        Done::Meshed(_, _, _, mesh, seq) => {
                            (1, *seq, mesh.solid.as_ptr() as usize, mesh.solid.len())
                        }
                        _ => panic!("unexpected deferred native payload"),
                    })
                    .collect::<Vec<_>>()
            };
            let before = identities(&streaming);
            assert!(before.iter().all(|row| row.3 > 0));
            streaming
                .native_write_fence_until(&mut world, Instant::now() + WAIT, || Ok(true))
                .unwrap();
            assert_eq!(identities(&streaming), before);
            assert_eq!(counts(&streaming, NativeWorkOrigin::Light).returned, 1);
            assert_eq!(counts(&streaming, NativeWorkOrigin::Mesh).returned, 1);

            let (entered, seen) = mpsc::channel();
            let (release, blocked) = mpsc::channel();
            *streaming.shared.work_gate.lock().unwrap() = Some(WorkGate {
                origin: NativeWorkOrigin::Light,
                entered,
                release: blocked,
            });
            streaming.submit(None, 0, Job::Light(1, 0, region(), 53, true));
            seen.recv_timeout(WAIT).unwrap();
            let book = streaming.shared.ownership.clone();
            let mut release = Some(release);
            let original =
                streaming.native_write_fence_until(&mut world, Instant::now() + WAIT, || {
                    // Deliberate channel disconnect unwinds the REAL worker during
                    // the final writer poll, after producer closure passed.
                    drop(release.take());
                    let deadline = Instant::now() + WAIT;
                    while !book.lock().unwrap().worker_exit_fault {
                        assert!(Instant::now() < deadline);
                        thread::yield_now();
                    }
                    Ok(true)
                });
            assert!(
                original.is_err(),
                "final post-writer validation must refuse"
            );
            let fault = streaming.observe_native_work();
            assert!(fault.worker_exit_fault);
            assert_eq!(
                fault.origins[NativeWorkOrigin::Light as usize].1.unresolved,
                1
            );
            assert_eq!(identities(&streaming), before);
        }

        struct LateFinished {
            save: Arc<crate::save::SaveDir>,
            entered: mpsc::Sender<()>,
            gate: Mutex<Option<mpsc::Receiver<()>>>,
            callback: mpsc::Sender<usize>,
            partial: bool,
            book: std::sync::OnceLock<Arc<Mutex<OwnershipBook>>>,
        }
        impl ChunkGenerator for LateFinished {
            type Worker = ();
            fn worker(&self) {}
            fn load(&self, x: i32, z: i32) -> Option<Chunk> {
                self.entered.send(()).unwrap();
                let gate = self.gate.lock().unwrap().take().unwrap();
                gate.recv_timeout(WAIT).unwrap();
                let mut chunk = Chunk::empty(x, z);
                chunk.fresh = true;
                chunk.entities.push(horizon_world::nbt::Tag::compound([(
                    "payload",
                    horizon_world::nbt::Tag::Long(731),
                )]));
                Some(chunk)
            }
            fn proto(&self, _: &mut (), _: i32, _: i32) -> Proto {
                unreachable!()
            }
            fn finish(&self, _: Proto) -> Chunk {
                unreachable!()
            }
            fn save_finished(&self, chunk: &Arc<Chunk>) -> Result<(), String> {
                {
                    let book = self.book.get().unwrap().lock().unwrap();
                    assert_eq!(
                        book.counts[NativeWorkOrigin::Generate as usize].reconciling,
                        1
                    );
                    assert_eq!(
                        book.counts[NativeWorkOrigin::SaveFinishedCallback as usize].reconciling,
                        1
                    );
                }
                self.callback.send(Arc::as_ptr(chunk) as usize).unwrap();
                self.save
                    .save_later(
                        std::slice::from_ref(chunk),
                        context().assets.registry,
                        &context().biomes,
                    )
                    .map_err(|e| e.to_string())?;
                if self.partial {
                    return Err("controlled failure after real region enqueue".into());
                }
                self.save
                    .save_finished_entities_later(chunk)
                    .map_err(|e| e.to_string())
            }
        }
        #[test]
        #[ignore = "owned prepared resources: late Generate finished callback and real writer ownership"]
        fn native_write_fence_late_finished_handoff_precedes_writer_and_retains_partial_failure() {
            for partial in [false, true] {
                let unique = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let root = std::env::temp_dir()
                    .join(format!("h08nf1-late-{}-{unique}", std::process::id()));
                std::fs::create_dir(&root).unwrap();
                let save = Arc::new(crate::save::SaveDir::new(root.clone()));
                let (entered, seen) = mpsc::channel();
                let (release, gate) = mpsc::channel();
                let mut release = Release(Some(release));
                let (callback, callbacks) = mpsc::channel();
                let generator = Arc::new(LateFinished {
                    save: save.clone(),
                    entered,
                    gate: Mutex::new(Some(gate)),
                    callback,
                    partial,
                    book: std::sync::OnceLock::new(),
                });
                let mut streaming = Streaming::new(generator.clone(), context(), 1);
                assert!(
                    generator
                        .book
                        .set(streaming.shared.ownership.clone())
                        .is_ok()
                );
                streaming.generating.insert((1000, 1000));
                streaming.submit(None, 0, Job::Generate(1000, 1000));
                seen.recv_timeout(WAIT).unwrap();
                assert!(
                    save.writes_finished().unwrap(),
                    "initial writer zero precedes the late callback"
                );
                let (writer_entered, writer_seen) = mpsc::channel();
                let (writer_release, writer_gate) = mpsc::channel();
                let mut writer_release = Release(Some(writer_release));
                save.native_fence_writer_step(move || {
                    writer_entered.send(()).unwrap();
                    writer_gate.recv_timeout(WAIT).unwrap();
                });
                release.release();
                streaming.wait(WAIT);
                let mut world = world();
                let mut polls = 0;
                let original = streaming.native_write_fence_until(&mut world, short(), || {
                    polls += 1;
                    save.writes_finished()
                });
                assert!(original.is_err());
                let pointer = callbacks.recv_timeout(WAIT).unwrap();
                writer_seen.recv_timeout(WAIT).unwrap();
                assert!(!save.writes_finished().unwrap());
                if partial {
                    assert_eq!(polls, 0);
                    assert_eq!(Arc::as_ptr(&streaming.failed_finished[0]) as usize, pointer);
                    assert_eq!(
                        streaming.failed_finished[0].entities[0].get("payload"),
                        Some(&horizon_world::nbt::Tag::Long(731))
                    );
                    assert!(streaming.generation_save_error().is_some());
                    assert_eq!(
                        counts(&streaming, NativeWorkOrigin::SaveFinishedCallback).unresolved,
                        1
                    );
                } else {
                    assert!(polls > 0);
                    assert_eq!(
                        save.load_entities(1000, 1000)[0].get("payload"),
                        Some(&horizon_world::nbt::Tag::Long(731))
                    );
                }
                // Preserve the original failure above; cleanup cannot qualify it.
                writer_release.release();
                let completed =
                    streaming.native_write_fence_until(&mut world, Instant::now() + WAIT, || {
                        save.writes_finished()
                    });
                assert_eq!(completed.is_err(), partial);
                drop(streaming);
                drop(generator);
                drop(save);
                std::fs::remove_dir_all(root).unwrap();
            }
        }

        #[test]
        #[ignore = "owned prepared resources: native accounting/disconnect/job-unwind refusal"]
        fn native_write_fence_refuses_accounting_disconnect_and_job_unwind() {
            for mode in 0..3 {
                let (mut streaming, _probe, workers, _events, mut init) =
                    fixture(1, false, false, mode == 2);
                workers.recv_timeout(WAIT).unwrap();
                let mut alive = None;
                if mode == 0 {
                    streaming.shared.ownership.lock().unwrap().accounting_fault = true;
                } else if mode == 1 {
                    let (send, receive) = mpsc::channel();
                    drop(send);
                    streaming.results = receive;
                } else {
                    let (proto, weak, _) = payload(93, (0, 0));
                    alive = Some(weak);
                    streaming.finishing.insert((0, 0));
                    streaming.submit(None, 0, Job::Finish(proto));
                }
                init.release();
                let original =
                    streaming.native_write_fence_until(&mut world(), Instant::now() + WAIT, || {
                        panic!("fault cannot reach writers")
                    });
                assert!(original.is_err());
                if mode == 2 {
                    let observation = streaming.observe_native_work();
                    assert!(observation.worker_exit_fault);
                    assert_eq!(counts(&streaming, NativeWorkOrigin::Finish).unresolved, 1);
                    // Unwind destroyed the worker-held proto; this proves a
                    // named fault/refusal, never invented payload recovery.
                    assert!(alive.unwrap().upgrade().is_none());
                }
            }
        }

        #[test]
        #[ignore = "owned prepared resources: decorated proto ownership and existing synchronous handoff"]
        fn native_write_fence_flushes_written_protos_and_keeps_failed_markers() {
            for failure in [false, true] {
                let (mut streaming, probe, workers, _events, mut init) =
                    fixture(1, false, false, false);
                workers.recv_timeout(WAIT).unwrap();
                if failure {
                    *probe.failure.lock().unwrap() =
                        Some("controlled proto enqueue failure".into());
                }
                let region: Box<[Proto; 9]> = Box::new(std::array::from_fn(|i| {
                    payload(201 + i as u64, (i as i32 % 3 - 1, i as i32 / 3 - 1)).0
                }));
                let mut expected: Vec<_> = region
                    .iter()
                    .map(|proto| {
                        let payload = proto.downcast_ref::<Payload>().unwrap();
                        (payload.value, payload as *const Payload as usize)
                    })
                    .collect();
                expected.sort();
                streaming.decorating = 1;
                streaming.submit(None, 0, Job::Decorate(0, 0, region));
                init.release();
                streaming.wait(WAIT);
                let mut polls = 0;
                let original =
                    streaming.native_write_fence_until(&mut world(), Instant::now() + WAIT, || {
                        polls += 1;
                        Ok(true)
                    });
                assert_eq!(original.is_err(), failure);
                assert_eq!(polls, usize::from(!failure));
                assert_eq!(streaming.protos.len(), 9);
                assert_eq!(streaming.written.len(), if failure { 9 } else { 0 });
                let mut actual: Vec<_> = streaming
                    .protos
                    .values()
                    .map(|proto| {
                        let payload = proto.as_ref().unwrap().downcast_ref::<Payload>().unwrap();
                        (payload.value, payload as *const Payload as usize)
                    })
                    .collect();
                actual.sort();
                assert_eq!(actual, expected);
                let mut seen = probe.seen.lock().unwrap().clone();
                seen.sort();
                assert_eq!(seen, expected);
                let callback = counts(&streaming, NativeWorkOrigin::SaveProtosCallback);
                assert_eq!(callback.accepted_storage_handoffs, u64::from(!failure));
                assert_eq!(callback.storage_errors, u64::from(failure));
                assert_eq!(callback.unresolved, u64::from(failure));
            }
        }

        #[test]
        #[ignore = "owned prepared resources: actual writer panic refuses the borrowed fence"]
        fn native_write_fence_writer_panic_times_out_without_discarding_pending_snapshot() {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "h08nf1-writer-panic-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir(&root).unwrap();
            let save = crate::save::SaveDir::new(root.clone());
            let (entered, seen) = mpsc::channel();
            save.native_fence_writer_step(move || {
                entered.send(()).unwrap();
                panic!("controlled actual writer panic");
            });
            let entity = horizon_world::nbt::Tag::compound([(
                "payload",
                horizon_world::nbt::Tag::Long(731),
            )]);
            save.save_entities_later(vec![((0, 0), vec![entity.clone()])])
                .unwrap();
            seen.recv_timeout(WAIT).unwrap();
            let (mut streaming, _probe, workers, _events, mut init) =
                fixture(1, false, false, false);
            workers.recv_timeout(WAIT).unwrap();
            init.release();
            initialized(&streaming, 1);
            let original = streaming
                .native_write_fence_until(&mut world(), short(), || save.writes_finished());
            assert!(original.unwrap_err().to_string().contains("deadline"));
            assert!(!save.writes_finished().unwrap());
            assert_eq!(save.load_entities(0, 0), [entity]);
            drop(streaming);
            drop(save); // Reap both native and writer threads.
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn native_work_checked_exhaustion_retains_fault_without_wrapping() {
        let book = Arc::new(Mutex::new(OwnershipBook::default()));
        {
            let mut state = book.lock().unwrap();
            let c = &mut state.counts[NativeWorkOrigin::Generate as usize];
            c.submitted = u64::MAX;
            c.queued = u64::MAX;
            c.accepted_storage_handoffs = u64::MAX;
            c.unresolved = u64::MAX;
        }
        let lease = WorkLease::new(&book, NativeWorkOrigin::Generate, WorkPhase::Queued);
        lease.storage(true);
        drop(lease);
        let state = book.lock().unwrap();
        assert!(state.accounting_fault);
        let c = state.counts[NativeWorkOrigin::Generate as usize];
        assert_eq!(
            (
                c.submitted,
                c.queued,
                c.accepted_storage_handoffs,
                c.unresolved
            ),
            (u64::MAX, u64::MAX, u64::MAX, u64::MAX)
        );
    }
}
