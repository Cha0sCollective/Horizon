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
    /// Retention is an explicit property of the actual provider allocation.
    /// Unknown generators must be refused before any sampler or worker factory.
    #[cfg(feature = "paired-packages")]
    fn retained_lod_capability(self: Arc<Self>) -> Option<crate::lod::retained::LodCapability>
    where
        Self: Sized,
    {
        None
    }
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
    #[cfg(feature = "paired-packages")]
    /// The actual native storage allocation used by this producer. Unknown
    /// providers cannot start retained workers merely by receiving a SaveDir.
    fn retained_save(&self) -> Option<&Arc<crate::save::SaveDir>> {
        None
    }
    #[cfg(feature = "paired-packages")]
    fn retained_load(&self, x: i32, z: i32) -> crate::save::RetainedNativeLoad {
        crate::save::RetainedNativeLoad::unavailable(x, z)
    }
    #[cfg(feature = "paired-packages")]
    fn retained_proto(&self, worker: &mut Self::Worker, x: i32, z: i32) -> Proto {
        self.proto(worker, x, z)
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
    #[cfg(feature = "paired-packages")]
    fn retained_lod_capability(self: Arc<Self>) -> Option<crate::lod::retained::LodCapability> {
        None
    }
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
    #[cfg(feature = "paired-packages")]
    fn retained_lod_capability(self: Arc<Self>) -> Option<crate::lod::retained::LodCapability> {
        ChunkGenerator::retained_lod_capability(self)
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

fn mesh_job(
    region: &Region,
    sy: i32,
    settings: MeshSettings,
    context: &WorkerContext,
    faces: &mut Option<(Arc<MeshTables>, FaceVisibility)>,
) -> SectionMesh {
    let visibility = match faces.take() {
        Some((tables, visibility)) if Arc::ptr_eq(&tables, &context.tables) => visibility,
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
    let mut mesh = mesher.mesh(&borrow(region), sy);
    mesh.sort_solid();
    *faces = Some((context.tables.clone(), mesher.visibility));
    mesh
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
#[cfg(feature = "paired-packages")]
impl WorkLease {
    // Retained polling has already obtained the book with try_lock. Never
    // reacquire it through the legacy blocking reconciliation helpers.
    fn advance_locked(&mut self, state: &mut OwnershipBook, phase: WorkPhase) {
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
    fn settle_locked(mut self, state: &mut OwnershipBook) {
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
    fn new_locked(
        book: &Arc<Mutex<OwnershipBook>>,
        state: &mut OwnershipBook,
        origin: NativeWorkOrigin,
    ) -> Self {
        let submitted = OwnershipBook::add(&mut state.counts[origin as usize].submitted);
        let counted = OwnershipBook::add(state.phase(origin, WorkPhase::Reconciling));
        state.accounting_fault |= !submitted || !counted;
        Self {
            book: book.clone(),
            origin,
            phase: WorkPhase::Reconciling,
            counted,
            settled: false,
        }
    }
    fn storage_locked(&self, state: &mut OwnershipBook, accepted: bool) {
        let c = &mut state.counts[self.origin as usize];
        let ok = OwnershipBook::add(if accepted {
            &mut c.accepted_storage_handoffs
        } else {
            &mut c.storage_errors
        });
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
    #[cfg(feature = "paired-packages")]
    Remote(u64),
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
            #[cfg(feature = "paired-packages")]
            Self::RetainedLoad(x, z, _) => ResultStamp::Generated(*x, *z),
            #[cfg(feature = "paired-packages")]
            Self::Remote(job) => ResultStamp::Remote(job.sequence),
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
    #[cfg(feature = "paired-packages")]
    Remote(Arc<retained_remote::JobOwner>),
}
pub enum Done {
    Generated(Chunk),
    /// A proto chunk, and whether it was decorated (loaded so from the save).
    Proto(i32, i32, Proto, bool),
    Decorated(i32, i32, Box<[Proto; 9]>),
    Lit(i32, i32, Vec<crate::chunk::Light>, u64),
    Meshed(i32, i32, i32, SectionMesh, u64),
    Saved(Vec<((i32, i32), Proto)>, Result<(), String>),
    #[cfg(feature = "paired-packages")]
    RetainedLoad(i32, i32, Box<crate::save::RetainedNativeLoad>),
    #[cfg(feature = "paired-packages")]
    Remote(Arc<retained_remote::JobOwner>),
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
    #[cfg(all(
        feature = "paired-packages",
        feature = "native-writer-test-control",
        debug_assertions
    ))]
    retained_work_gate: Mutex<Option<RetainedWorkGate>>,
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
    constructor_complete: bool,
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
    #[cfg(feature = "paired-packages")]
    retained: Option<RetainedStream>,
    #[cfg(feature = "paired-packages")]
    remote_mode: Option<Arc<retained_remote::State>>,
}

/// A failed spawn retains the allocation and every successfully started worker.
pub struct StreamingStartFailure {
    pub streaming: Streaming,
    pub error: String,
}
impl std::fmt::Debug for StreamingStartFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamingStartFailure")
            .field("error", &self.error)
            .field("workers", &self.streaming.threads.len())
            .finish()
    }
}

#[cfg(feature = "paired-packages")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetainedStreamStatus {
    Pending,
    NeedsFinishedColumn,
    NeedsRefusedRead,
    Blocked(String),
    Complete,
}

#[cfg(feature = "paired-packages")]
pub enum RetainedColumnDisposition {
    StoreFresh,
    AlreadyStored,
}

#[cfg(feature = "paired-packages")]
pub struct RetainedColumnTicket {
    allocation: Arc<Shared>,
    serial: u64,
}

/// Correlation for one original saved read while its native producer is open.
/// This type cannot be used as a close-column or completion ticket.
#[cfg(feature = "paired-packages")]
pub struct RetainedOpenLoadTicket {
    allocation: Arc<Shared>,
    serial: u64,
}
#[cfg(feature = "paired-packages")]
pub struct RetainedOpenLoad<'a> {
    load: &'a crate::save::RetainedNativeLoad,
    ticket: RetainedOpenLoadTicket,
}
#[cfg(feature = "paired-packages")]
impl<'a> RetainedOpenLoad<'a> {
    pub fn load(&self) -> &'a crate::save::RetainedNativeLoad {
        self.load
    }
    /// The caller must validate the original records and decoded projection.
    pub fn into_ticket(self) -> RetainedOpenLoadTicket {
        self.ticket
    }
}

#[cfg(feature = "paired-packages")]
pub struct RetainedFinishedColumn<'a> {
    chunk: &'a Arc<Chunk>,
    origin: NativeWorkOrigin,
    ticket: RetainedColumnTicket,
    evidence: Option<&'a crate::save::RetainedLoadedRecords>,
}

#[cfg(feature = "paired-packages")]
pub struct RetainedRefusedRead<'a> {
    load: &'a crate::save::RetainedNativeLoad,
    ticket: RetainedColumnTicket,
}
#[cfg(feature = "paired-packages")]
impl<'a> RetainedRefusedRead<'a> {
    pub fn position(&self) -> (i32, i32) {
        self.load.position()
    }
    pub fn error(&self) -> &str {
        self.load.error().unwrap_or("retained native input refused")
    }
    pub fn evidence(&self) -> Option<&'a crate::save::RetainedLoadedRecords> {
        self.load.evidence()
    }
    pub fn into_ticket(self) -> RetainedColumnTicket {
        self.ticket
    }
}

#[cfg(feature = "paired-packages")]
impl<'a> RetainedFinishedColumn<'a> {
    pub fn chunk(&self) -> &'a Arc<Chunk> {
        self.chunk
    }
    pub fn origin(&self) -> NativeWorkOrigin {
        self.origin
    }
    pub fn evidence(&self) -> Option<&'a crate::save::RetainedLoadedRecords> {
        self.evidence
    }
    /// Correlation only. The caller must perform strict native validation first.
    pub fn into_ticket(self) -> RetainedColumnTicket {
        self.ticket
    }
}

#[cfg(feature = "paired-packages")]
#[must_use = "retain the producer witness until the same native writer is closed"]
pub struct SealedStream {
    allocation: Arc<Shared>,
    save: Arc<crate::save::SaveDir>,
    workers: usize,
    pub failed_initialization: bool,
    refusals: Vec<String>,
}
#[cfg(feature = "paired-packages")]
impl SealedStream {
    pub fn refused_reads(&self) -> &[String] {
        &self.refusals
    }
}
#[cfg(feature = "paired-packages")]
impl std::fmt::Debug for SealedStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SealedStream")
            .field("workers", &self.workers)
            .field("failed_initialization", &self.failed_initialization)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "paired-packages")]
struct RetainedStream {
    save: Arc<crate::save::SaveDir>,
    armed: bool,
    workers: usize,
    close: Option<RetainedProducerClose>,
    open_load: Option<ReceivedWork>,
    open_serial: u64,
    startup_error: Option<String>,
    finished_backlog: Vec<(Arc<Chunk>, Option<WorkLease>)>,
    proto_batches: Vec<RetainedProtoBatch>,
    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    start_control: Option<RetainedStreamingStartTestControl>,
    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    handoff_gate: Option<Arc<RetainedHandoffGate>>,
    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    test_column: Option<(i32, i32)>,
}

#[cfg(feature = "paired-packages")]
struct RetainedProducerClose {
    world: Option<usize>,
    membership: Option<u64>,
    deadline: std::time::Instant,
    status: RetainedStreamStatus,
    history: Vec<String>,
    deferred: std::vec::IntoIter<Done>,
    owners: std::iter::Peekable<std::vec::IntoIter<(usize, WorkLease)>>,
    deferred_index: usize,
    incoming: Option<Done>,
    current: Option<ReceivedWork>,
    finished: Option<(Arc<Chunk>, WorkLease)>,
    callback: Option<WorkLease>,
    proto_callback: Option<WorkLease>,
    serial: u64,
    stopped: bool,
    joined: usize,
    failed_initialization: bool,
    witness_taken: bool,
    written: Option<std::collections::hash_set::IntoIter<(i32, i32)>>,
    proto_key: Option<(i32, i32)>,
    proto_outcome: Option<Result<(), String>>,
    finished_outcome: Option<Result<(), String>>,
    proto_scan: Option<std::collections::hash_map::IntoIter<(i32, i32), Option<Proto>>>,
    protos_checked: bool,
    join_scan: usize,
    finished_backlog: std::vec::IntoIter<(Arc<Chunk>, Option<WorkLease>)>,
    proto_batches: std::vec::IntoIter<RetainedProtoBatch>,
    proto_batch: Option<RetainedProtoBatch>,
    save_restore: Option<RetainedSaveRestore>,
    light_replay: Option<RetainedLightReplay>,
    loaded: Option<crate::save::RetainedLoadedRecords>,
    accepted_loaded: Vec<crate::save::RetainedLoadedRecords>,
    finished_stored: bool,
    ack_cursor: usize,
    ack_error: Option<String>,
    writer_acknowledged: bool,
    refused: Option<RetainedRefusedReadOwner>,
    refused_inputs: Vec<RetainedRefusedInput>,
    refusal_errors: Vec<String>,
}

#[cfg(feature = "paired-packages")]
struct RetainedRefusedReadOwner {
    position: (i32, i32),
    load: Box<crate::save::RetainedNativeLoad>,
    owner: WorkLease,
}

#[cfg(feature = "paired-packages")]
enum RetainedRefusedInput {
    Raw(Box<crate::save::RetainedNativeLoad>),
    Loaded {
        _chunk: Arc<Chunk>,
        records: crate::save::RetainedLoadedRecords,
    },
}
#[cfg(feature = "paired-packages")]
impl RetainedRefusedInput {
    fn records(&self) -> &crate::save::RetainedLoadedRecords {
        match self {
            Self::Raw(load) => load.evidence().unwrap(),
            Self::Loaded { records, .. } => records,
        }
    }
}

#[cfg(feature = "paired-packages")]
struct RetainedProtoBatch {
    keys: Vec<(i32, i32)>,
    owner: WorkLease,
    outcome: Option<Result<(), String>>,
    accepted: bool,
    cleared: usize,
}

#[cfg(feature = "paired-packages")]
struct RetainedSaveRestore {
    protos: std::vec::IntoIter<((i32, i32), Proto)>,
    current: Option<((i32, i32), Proto)>,
    owner: Option<WorkLease>,
    outcome: Result<(), String>,
}

#[cfg(feature = "paired-packages")]
struct RetainedLightReplay {
    x: i32,
    z: i32,
    snapshot: u64,
    index: usize,
    positions: Vec<(i32, i32, i32)>,
    owner: Option<WorkLease>,
}

enum StreamingConstruction {
    Legacy,
    #[cfg(feature = "paired-packages")]
    Retained(Arc<crate::save::SaveDir>),
    #[cfg(feature = "paired-packages")]
    Remote(Arc<retained_remote::State>),
    #[cfg(all(
        feature = "paired-packages",
        feature = "native-writer-test-control",
        debug_assertions
    ))]
    Controlled(Arc<crate::save::SaveDir>, RetainedStreamingStartTestControl),
}

#[cfg(all(
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
struct RetainedStartGate {
    entered: AtomicBool,
    released: Mutex<bool>,
    ready: Condvar,
}
#[cfg(all(
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
pub struct RetainedStreamingStartTestControl {
    hold_worker: Option<usize>,
    fail_spawn: Option<usize>,
    gate: Arc<RetainedStartGate>,
    panic_worker: Option<usize>,
}
#[cfg(all(
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
pub struct RetainedStreamingStartTestReceipt {
    gate: Arc<RetainedStartGate>,
}
#[cfg(all(
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
impl RetainedStreamingStartTestControl {
    pub fn new(
        hold_worker: Option<usize>,
        fail_spawn: Option<usize>,
    ) -> (Self, RetainedStreamingStartTestReceipt) {
        let gate = Arc::new(RetainedStartGate {
            entered: AtomicBool::new(false),
            released: Mutex::new(false),
            ready: Condvar::new(),
        });
        (
            Self {
                hold_worker,
                fail_spawn,
                gate: gate.clone(),
                panic_worker: None,
            },
            RetainedStreamingStartTestReceipt { gate },
        )
    }
    pub fn panic_initialization_at(&mut self, worker: usize) {
        self.panic_worker = Some(worker);
    }
}
#[cfg(all(
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
impl RetainedStreamingStartTestReceipt {
    pub fn entered(&self) -> bool {
        self.gate.entered.load(Ordering::Acquire)
    }
    pub fn release(&mut self) {
        *self.gate.released.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.gate.ready.notify_all();
    }
}
#[cfg(all(
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
impl Drop for RetainedStreamingStartTestReceipt {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(all(
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
struct RetainedHandoffGate {
    origin: NativeWorkOrigin,
    entered: AtomicBool,
    released: AtomicBool,
    report_error: AtomicBool,
}
#[cfg(all(
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
struct RetainedWorkGate {
    origin: NativeWorkOrigin,
    gate: Arc<RetainedStartGate>,
    panic_after_release: bool,
}
#[cfg(all(
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
pub struct RetainedHandoffTestReceipt {
    gate: Arc<RetainedHandoffGate>,
}
#[cfg(all(
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
impl RetainedHandoffTestReceipt {
    pub fn entered(&self) -> bool {
        self.gate.entered.load(Ordering::Acquire)
    }
    pub fn release(&mut self) {
        self.gate.released.store(true, Ordering::Release);
    }
    /// A fixed negative observation after the real native callback returned Ok.
    /// It never replaces the native storage action or manufactures success.
    pub fn report_error_once_after_native_acceptance(&mut self) {
        self.gate.report_error.store(true, Ordering::Release);
    }
}
#[cfg(all(
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
impl Drop for RetainedHandoffTestReceipt {
    fn drop(&mut self) {
        self.release();
    }
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
    fn retained_sealed(&self) -> bool {
        #[cfg(feature = "paired-packages")]
        {
            // Remote callers use only the checked wrapper, never native/legacy
            // void scheduling or storage APIs, even before remote close.
            self.remote_mode.is_some() || self.retained.as_ref().is_some_and(|r| r.close.is_some())
        }
        #[cfg(not(feature = "paired-packages"))]
        {
            false
        }
    }
    pub fn new<G: ChunkGenerator>(
        generator: Arc<G>,
        context: Arc<WorkerContext>,
        threads: usize,
    ) -> Self {
        match Self::construct(generator, context, threads, StreamingConstruction::Legacy) {
            Ok(streaming) => streaming,
            Err(failure) => panic!("spawn worker: {}", failure.error),
        }
    }
    #[cfg(feature = "paired-packages")]
    #[allow(clippy::result_large_err)] // The returned failure is the original partial native owner.
    pub fn start_retained<G: ChunkGenerator>(
        generator: Arc<G>,
        context: Arc<WorkerContext>,
        threads: usize,
        save: Arc<crate::save::SaveDir>,
    ) -> Result<Self, StreamingStartFailure> {
        Self::construct(
            generator,
            context,
            threads,
            StreamingConstruction::Retained(save),
        )
    }
    #[cfg(all(
        feature = "paired-packages",
        feature = "native-writer-test-control",
        debug_assertions
    ))]
    #[allow(clippy::result_large_err)]
    pub fn start_retained_with_test_control<G: ChunkGenerator>(
        generator: Arc<G>,
        context: Arc<WorkerContext>,
        threads: usize,
        save: Arc<crate::save::SaveDir>,
        control: RetainedStreamingStartTestControl,
    ) -> Result<Self, StreamingStartFailure> {
        Self::construct(
            generator,
            context,
            threads,
            StreamingConstruction::Controlled(save, control),
        )
    }
    #[allow(clippy::result_large_err)]
    fn construct<G: ChunkGenerator>(
        generator: Arc<G>,
        context: Arc<WorkerContext>,
        threads: usize,
        construction: StreamingConstruction,
    ) -> Result<Self, StreamingStartFailure> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(BinaryHeap::new()),
            ready: Condvar::new(),
            stop: AtomicBool::new(false),
            context: RwLock::new(context.clone()),
            ownership: Arc::new(Mutex::new(OwnershipBook::default())),
            returned: Mutex::new(VecDeque::new()),
            queued_owners: Mutex::new(FxHashMap::default()),
            #[cfg(all(
                feature = "paired-packages",
                feature = "native-writer-test-control",
                debug_assertions
            ))]
            retained_work_gate: Mutex::new(None),
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
        let light_view = {
            let generator = generator.clone();
            Arc::new(move |p: &Proto| generator.light_view(p))
        };
        #[cfg(feature = "paired-packages")]
        let remote_mode = match &construction {
            StreamingConstruction::Remote(mode) => Some(mode.clone()),
            _ => None,
        };
        // This owner exists before the first native thread can be acquired.
        let mut streaming = Self {
            shared,
            light_view,
            results,
            threads: Vec::new(),
            constructor_complete: false,
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
            #[cfg(feature = "paired-packages")]
            remote_mode,
            #[cfg(feature = "paired-packages")]
            retained: match construction {
                StreamingConstruction::Legacy => None,
                StreamingConstruction::Remote(_) => None,
                StreamingConstruction::Retained(save) => Some(RetainedStream {
                    save,
                    armed: true,
                    workers: 0,
                    close: None,
                    open_load: None,
                    open_serial: 0,
                    startup_error: None,
                    finished_backlog: Vec::new(),
                    proto_batches: Vec::new(),
                    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
                    start_control: None,
                    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
                    handoff_gate: None,
                    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
                    test_column: None,
                }),
                #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
                StreamingConstruction::Controlled(save, control) => Some(RetainedStream {
                    save,
                    armed: true,
                    workers: 0,
                    close: None,
                    open_load: None,
                    open_serial: 0,
                    startup_error: None,
                    finished_backlog: Vec::new(),
                    proto_batches: Vec::new(),
                    start_control: Some(control),
                    handoff_gate: None,
                    test_column: None,
                }),
            },
        };
        #[cfg(not(feature = "paired-packages"))]
        let _ = construction;
        #[cfg(feature = "paired-packages")]
        if let Some(retained) = &mut streaming.retained {
            let error = match generator.retained_save() {
                Some(actual) if Arc::ptr_eq(actual, &retained.save) => None,
                Some(_) => Some("retained generator SaveDir allocation mismatch".to_owned()),
                None => Some("generator has no retained SaveDir binding".to_owned()),
            };
            if let Some(error) = error {
                retained.startup_error = Some(error.clone());
                return Err(StreamingStartFailure { streaming, error });
            }
        }
        #[cfg(feature = "paired-packages")]
        if let Some(mode) = &streaming.remote_mode {
            // The opaque remote owner is fully installed before native start.
            // Keep this original result channel for its later borrowed start.
            *mode.sender.lock().unwrap_or_else(|e| e.into_inner()) = Some(send);
            streaming.remote = true;
            streaming.constructor_complete = true;
            return Ok(streaming);
        }
        if let Err(error) = Self::spawn_pool(&mut streaming, generator, context, threads, send) {
            return Err(StreamingStartFailure { streaming, error });
        }
        Ok(streaming)
    }

    fn spawn_pool<G: ChunkGenerator>(
        streaming: &mut Self,
        generator: Arc<G>,
        context: Arc<WorkerContext>,
        threads: usize,
        send: mpsc::Sender<Done>,
    ) -> Result<(), String> {
        for i in 0..threads.max(1) {
            #[cfg(feature = "paired-packages")]
            let remote_mode = streaming.remote_mode.clone();
            #[cfg(feature = "paired-packages")]
            let (remote_release, remote_wait) = if remote_mode.is_some() {
                let (release, wait) = mpsc::channel();
                (Some(release), Some(wait))
            } else {
                (None, None)
            };
            #[cfg(feature = "paired-packages")]
            let retained_mode = streaming.retained.is_some();
            #[cfg(feature = "paired-packages")]
            let retained_save = streaming.retained.as_ref().map(|r| r.save.clone());
            #[cfg(all(
                feature = "paired-packages",
                feature = "native-writer-test-control",
                debug_assertions
            ))]
            let start_gate = streaming
                .retained
                .as_ref()
                .and_then(|r| r.start_control.as_ref())
                .filter(|c| c.hold_worker == Some(i))
                .map(|c| c.gate.clone());
            #[cfg(all(
                feature = "paired-packages",
                feature = "native-writer-test-control",
                debug_assertions
            ))]
            let panic_initialization = streaming
                .retained
                .as_ref()
                .and_then(|r| r.start_control.as_ref())
                .is_some_and(|c| c.panic_worker == Some(i));
            #[cfg(all(
                feature = "paired-packages",
                feature = "native-writer-test-control",
                debug_assertions
            ))]
            let mut spawn_error = streaming
                .retained
                .as_ref()
                .and_then(|r| r.start_control.as_ref())
                .filter(|c| c.fail_spawn == Some(i))
                .map(|_| std::io::Error::other("controlled native worker spawn failure"));
            #[cfg(all(
                feature = "paired-packages",
                feature = "native-writer-test-control",
                debug_assertions
            ))]
            if remote_mode
                .as_ref()
                .is_some_and(|mode| mode.refuse_spawn(i))
            {
                spawn_error = Some(std::io::Error::other(
                    "controlled remote worker spawn refusal",
                ));
            }
            #[cfg(not(all(
                feature = "paired-packages",
                feature = "native-writer-test-control",
                debug_assertions
            )))]
            let spawn_error: Option<std::io::Error> = None;
            let shared = streaming.shared.clone();
            let send = send.clone();
            let generator = generator.clone();
            let light = context.light.clone();
            let spawned = if let Some(error) = spawn_error {
                Err(error)
            } else {
                thread::Builder::new()
                    .name(format!("horizon-worker-{i}"))
                    .spawn(move || {
                        #[cfg(feature = "paired-packages")]
                        let _remote_guard = remote_mode.as_ref().map(|mode| retained_remote::ActorGuard::new(mode.clone(), i));
                        #[cfg(feature = "paired-packages")]
                        if let Some(wait) = remote_wait && wait.recv().is_err() {
                            return;
                        }
                        let mut lifetime = WorkerLifetime {
                            book: shared.ownership.clone(),
                            initialized: false,
                            stopped: false,
                        };
                        // Ticks and frames come first (horizon_threads).
                        horizon_threads::behind_server();
                        #[cfg(all(feature = "paired-packages", feature = "native-writer-test-control", debug_assertions))]
                        if let Some(gate) = start_gate {
                            gate.entered.store(true,Ordering::Release);
                            let mut released = gate.released.lock().unwrap_or_else(|e| e.into_inner());
                            while !*released { released = gate.ready.wait(released).unwrap_or_else(|e| e.into_inner()); }
                        }
                        #[cfg(all(feature = "paired-packages", feature = "native-writer-test-control", debug_assertions))]
                        assert!(!panic_initialization,"controlled native initialization panic");
                        let mut worker = generator.worker();
                        #[cfg(all(feature = "paired-packages", feature = "native-writer-test-control", debug_assertions))]
                        if let Some(mode) = &remote_mode { mode.initialization_control(i); }
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
                                    #[cfg(feature = "paired-packages")]
                                    if let Some(mode) = &remote_mode {
                                        if mode.sealed() {
                                            lifetime.stopped = true;
                                            return;
                                        }
                                        if !queue.is_empty() {
                                            mode.before_claim();
                                            if !mode.claim() {
                                                lifetime.stopped = true;
                                                return;
                                            }
                                        }
                                    }
                                    if let Some(job) = queue.pop() {
                                        #[cfg(feature = "paired-packages")]
                                        if let Job::Remote(input) = &job.job { input.mark_active(); }
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
                                    #[cfg(feature = "paired-packages")]
                                    if remote_mode.is_some() {
                                        drop(queue);
                                        if let Some(mode) = &remote_mode { mode.before_park(); }
                                        thread::park();
                                        queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                                        continue;
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
                            #[cfg(all(feature = "paired-packages", feature = "native-writer-test-control", debug_assertions))]
                            {
                                let gate = shared.retained_work_gate.lock().unwrap_or_else(|e| e.into_inner()).as_ref().filter(|g| owner.as_ref().is_some_and(|o|o.origin==g.origin)).map(|g|(g.gate.clone(),g.panic_after_release));
                                if let Some((gate,panic_after_release))=gate {
                                    gate.entered.store(true,Ordering::Release);
                                    let mut released=gate.released.lock().unwrap_or_else(|e| e.into_inner());
                                    while !*released { released=gate.ready.wait(released).unwrap_or_else(|e|e.into_inner()); }
                                    drop(released);
                                    assert!(!panic_after_release,"controlled retained native held-work panic");
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
                                Job::Generate(x, z) => {
                                    #[cfg(feature = "paired-packages")]
                                    let done = if retained_mode {
                                        let load=generator.retained_load(x,z);
                                        let correlated=load.position()==(x,z) && load.evidence().is_some_and(|evidence| evidence.position()==(x,z) && retained_save.as_ref().is_some_and(|save| Arc::ptr_eq(evidence.save(),save)));
                                        if correlated && load.is_absent() && load.error().is_none() {
                                            let proto=generator.retained_proto(&mut worker,x,z);
                                            let decorated=generator.is_decorated(&proto);
                                            Done::Proto(x,z,proto,decorated)
                                        } else {
                                            if correlated && load.error().is_none() && let Some(chunk)=load.chunk() {
                                                generator.prepare_loaded(&mut worker,chunk);
                                            }
                                            Done::RetainedLoad(x,z,Box::new(load))
                                        }
                                    } else { match generator.load(x,z) {
                                        Some(chunk)=>{generator.prepare_loaded(&mut worker,&chunk);Done::Generated(chunk)}
                                        None=>{let proto=generator.proto(&mut worker,x,z);let decorated=generator.is_decorated(&proto);Done::Proto(x,z,proto,decorated)}
                                    }};
                                    #[cfg(not(feature = "paired-packages"))]
                                    let done = match generator.load(x, z) {
                                        Some(chunk) => {
                                            generator.prepare_loaded(&mut worker, &chunk);
                                            Done::Generated(chunk)
                                        }
                                        None => {
                                            let proto = generator.proto(&mut worker, x, z);
                                            let decorated = generator.is_decorated(&proto);
                                            Done::Proto(x, z, proto, decorated)
                                        }
                                    };
                                    done
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
                                    let mesh = mesh_job(&region, sy, settings, &context, &mut faces);
                                    Done::Meshed(x, sy, z, mesh, seq)
                                }
                                #[cfg(feature = "paired-packages")]
                                Job::Remote(input) => {
                                    input.execute(&mut faces);
                                    Done::Remote(input)
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
                            #[cfg(feature = "paired-packages")]
                            if let Some(mode) = &remote_mode { mode.release_claim(); }
                        }
                    })
            };
            match spawned {
                Ok(handle) => {
                    #[cfg(feature = "paired-packages")]
                    if let Some(mode) = &streaming.remote_mode {
                        mode.register_wake(handle.thread().clone());
                    }
                    streaming.threads.push(handle);
                    #[cfg(feature = "paired-packages")]
                    if let Some(release) = remote_release {
                        let _ = release.send(());
                    }
                    #[cfg(feature = "paired-packages")]
                    if let Some(retained) = &mut streaming.retained {
                        retained.workers += 1;
                    }
                }
                Err(error) => {
                    let error = error.to_string();
                    #[cfg(feature = "paired-packages")]
                    if let Some(retained) = &mut streaming.retained {
                        retained.startup_error = Some(error.clone());
                    }
                    return Err(error);
                }
            }
        }
        streaming.constructor_complete = true;
        Ok(())
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
        if self.retained_sealed() {
            return;
        }
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
        if self.retained_sealed() {
            return;
        }
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
        if self.retained_sealed() {
            return None;
        }
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
        // Existing native/legacy scheduling retains its successful behavior.
        // Their entry points exclude the fixed remote recipe before mutation.
        self.try_submit(at, bias, job)
            .unwrap_or_else(|_| unreachable!("remote input reached native scheduling"));
    }
    fn try_submit(&mut self, at: Option<(i32, i32)>, bias: i64, job: Job) -> Result<(), Job> {
        #[cfg(feature = "paired-packages")]
        if self.remote_mode.is_some() {
            return Err(job);
        }
        let origin = match &job {
            Job::Generate(..) => NativeWorkOrigin::Generate,
            Job::Decorate(..) => NativeWorkOrigin::Decorate,
            Job::Finish(..) => NativeWorkOrigin::Finish,
            Job::Light(..) => NativeWorkOrigin::Light,
            Job::Mesh(..) => NativeWorkOrigin::Mesh,
            Job::Save(..) => NativeWorkOrigin::Save,
            #[cfg(feature = "paired-packages")]
            Job::Remote(input) => input.origin,
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
        Ok(())
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
                    #[cfg(feature = "paired-packages")]
                    if let Some(retained) = &mut self.retained {
                        retained.finished_backlog.push((chunk, None));
                    } else {
                        self.failed_finished.push(chunk);
                    }
                    #[cfg(not(feature = "paired-packages"))]
                    self.failed_finished.push(chunk);
                } else {
                    let owner = WorkLease::new(
                        &self.shared.ownership,
                        NativeWorkOrigin::SaveFinishedCallback,
                        WorkPhase::Reconciling,
                    );
                    let outcome = self.saves.save_finished(&chunk);
                    #[cfg(feature = "paired-packages")]
                    let outcome = self
                        .retained_handoff_result(NativeWorkOrigin::SaveFinishedCallback, outcome);
                    owner.storage(outcome.is_ok());
                    match outcome {
                        Ok(()) => owner.settle(),
                        Err(error) => {
                            #[cfg(feature = "paired-packages")]
                            if let Some(retained) = &mut self.retained {
                                retained.finished_backlog.push((chunk, Some(owner)));
                            } else {
                                self.failed_finished.push(chunk);
                            }
                            #[cfg(not(feature = "paired-packages"))]
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
        if self.retained_sealed() {
            return (Vec::new(), Vec::new());
        }
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
            #[cfg(feature = "paired-packages")]
            if matches!(&done, Done::RetainedLoad(..)) {
                self.defer(ReceivedWork { done, owner });
                continue;
            }
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
                #[cfg(feature = "paired-packages")]
                Done::RetainedLoad(..) => {
                    unreachable!("retained native records remain owned for checked handoff")
                }
                #[cfg(feature = "paired-packages")]
                Done::Remote(_) => {
                    unreachable!("opaque remote results have their own checked handoff")
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
        #[cfg(feature = "paired-packages")]
        if self.remote_mode.is_some() {
            return;
        }
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
        #[cfg(feature = "paired-packages")]
        if self.remote_mode.is_some() {
            return;
        }
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
        if self.retained_sealed() {
            return Err("retained producer submissions are sealed".into());
        }
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
            #[cfg(feature = "paired-packages")]
            let outcome =
                self.retained_handoff_result(NativeWorkOrigin::SaveProtosCallback, outcome);
            owner.storage(outcome.is_ok());
            if outcome.is_ok() {
                owner.settle();
            } else {
                #[cfg(feature = "paired-packages")]
                if let Some(retained) = &mut self.retained {
                    let keys = self
                        .written
                        .iter()
                        .filter(|key| self.protos.get(*key).is_some_and(Option::is_some))
                        .copied()
                        .collect();
                    retained.proto_batches.push(RetainedProtoBatch {
                        keys,
                        owner,
                        outcome: None,
                        accepted: false,
                        cleared: 0,
                    });
                }
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
        if self.retained_sealed() {
            return Err("retained producer submissions are sealed".into());
        }
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
    /// Observe the current native producer/written predicate without servicing it.
    /// This neither fences work nor polls storage, and issues no transferable
    /// readiness or admission. A staged caller must retain the original native
    /// owners, exclude scheduling and poll every actual writer before this fresh
    /// observation under the same exclusive owner borrow.
    #[cfg(feature = "paired-packages")]
    pub fn native_write_fence_current(&self) -> crate::Result<bool> {
        if self.retained_sealed() {
            return Err("retained producer submissions are sealed".into());
        }
        Ok(self.native_fence_producers_closed()? && self.written.is_empty())
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
        if self.retained_sealed() {
            return Err("retained producer submissions are sealed".into());
        }
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
        if self.retained_sealed() {
            return;
        }
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
        if self.retained_sealed() {
            return;
        }
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
        if self.retained_sealed() {
            return [0; 3];
        }
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
        if self.retained_sealed() {
            return;
        }
        match self.results.recv_timeout(timeout) {
            Ok(done) => self.defer(self.received(done)),
            Err(mpsc::RecvTimeoutError::Disconnected) => self.result_disconnected(),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}
#[cfg(feature = "paired-packages")]
impl RetainedProducerClose {
    fn block(&mut self, reason: impl Into<String>) -> RetainedStreamStatus {
        let reason = reason.into();
        self.history.push(reason.clone());
        self.status = RetainedStreamStatus::Blocked(reason);
        self.status.clone()
    }
}

#[cfg(feature = "paired-packages")]
impl Streaming {
    /// After update collects results, borrow one original saved read without
    /// sealing the producer. Pending/refused input remains owned here for close.
    pub fn poll_retained_open_load(&mut self) -> Result<Option<RetainedOpenLoad<'_>>, String> {
        let retained = self
            .retained
            .as_ref()
            .ok_or("streaming was not started retained")?;
        if retained.close.is_some() {
            return Err("open saved-load handoff after close began".into());
        }
        if retained.open_load.is_none() {
            let serial = retained
                .open_serial
                .checked_add(1)
                .ok_or("open saved-load serial exhausted")?;
            let mut selected = None;
            for work in self.take_deferred() {
                if selected.is_none() && matches!(&work.done, Done::RetainedLoad(..)) {
                    selected = Some(work);
                } else {
                    self.defer(work);
                }
            }
            if let Some(work) = selected {
                let retained = self.retained.as_mut().unwrap();
                retained.open_serial = serial;
                retained.open_load = Some(work);
            }
        }
        let retained = self.retained.as_ref().unwrap();
        let Some(work) = &retained.open_load else {
            return Ok(None);
        };
        let Done::RetainedLoad(x, z, load) = &work.done else {
            unreachable!()
        };
        if !work.owner.as_ref().is_some_and(|owner| {
            owner.origin == NativeWorkOrigin::Generate
                && Arc::ptr_eq(&owner.book, &self.shared.ownership)
        }) || load.position() != (*x, *z)
        {
            return Err("open saved-load original work allocation/coordinate mismatch".into());
        }
        Ok(Some(RetainedOpenLoad {
            load,
            ticket: RetainedOpenLoadTicket {
                allocation: self.shared.clone(),
                serial: retained.open_serial,
            },
        }))
    }

    /// Consume only the saved read that the caller just validated. Normal
    /// loaded-column insertion retains native entity/light/residency behavior.
    pub fn accept_retained_open_load(
        &mut self,
        ticket: RetainedOpenLoadTicket,
        world: &mut World,
    ) -> Result<bool, String> {
        let retained = self
            .retained
            .as_ref()
            .ok_or("streaming was not started retained")?;
        if retained.close.is_some()
            || !Arc::ptr_eq(&ticket.allocation, &self.shared)
            || ticket.serial != retained.open_serial
        {
            return Err("foreign, stale or closing open saved-load ticket".into());
        }
        let work = retained
            .open_load
            .as_ref()
            .ok_or("open saved-load ticket already consumed")?;
        let Done::RetainedLoad(x, z, load) = &work.done else {
            unreachable!()
        };
        let chunk = load
            .chunk()
            .ok_or("open saved-load has no validated native column")?;
        let evidence = load
            .evidence()
            .ok_or("open saved-load has no original record evidence")?;
        if load.error().is_some()
            || load.is_absent()
            || chunk.fresh
            || (chunk.x, chunk.z) != (*x, *z)
            || load.position() != (*x, *z)
            || evidence.position() != (*x, *z)
            || !Arc::ptr_eq(evidence.save(), &retained.save)
            || evidence.dimension_root() != retained.save.root.as_path()
            || world.chunks.contains_key(&(*x, *z))
            || !work.owner.as_ref().is_some_and(|owner| {
                owner.origin == NativeWorkOrigin::Generate
                    && Arc::ptr_eq(&owner.book, &self.shared.ownership)
            })
        {
            return Err("open saved-load provenance or resident collision".into());
        }
        match evidence.poll_current().map_err(|error| error.to_string())? {
            None => return Ok(false),
            Some(false) => return Err("open saved-load source was superseded".into()),
            Some(true) => {}
        }
        let ReceivedWork { done, mut owner } =
            self.retained.as_mut().unwrap().open_load.take().unwrap();
        let Done::RetainedLoad(x, z, load) = done else {
            unreachable!()
        };
        let (chunk, _records) = match load.into_parts() {
            Ok(parts) => parts,
            Err(load) => {
                self.retained.as_mut().unwrap().open_load = Some(ReceivedWork {
                    done: Done::RetainedLoad(x, z, load),
                    owner,
                });
                return Err("open saved-load consuming projection refused".into());
            }
        };
        owner.as_mut().unwrap().advance(WorkPhase::Reconciling);
        let _reconciliation = Reconciliation(owner);
        self.receive_generated(chunk, world);
        Ok(true)
    }

    pub fn begin_retained_close(
        &mut self,
        world: Option<&World>,
        deadline: std::time::Instant,
    ) -> Result<(), String> {
        let retained = self
            .retained
            .as_mut()
            .ok_or("streaming was not started retained")?;
        let identity = world.map(|w| w as *const World as usize);
        if let Some(close) = &retained.close {
            return if close.world == identity {
                Ok(())
            } else {
                Err("retained close world changed".into())
            };
        }
        let pending = retained.open_load.take();
        if let Some(pending) = pending {
            self.defer(pending);
        }
        let retained = self.retained.as_mut().unwrap();
        retained.close = Some(RetainedProducerClose {
            world: identity,
            membership: world.map(|w| w.chunks.membership_revision()),
            deadline,
            status: RetainedStreamStatus::Pending,
            history: retained.startup_error.iter().cloned().collect(),
            deferred: std::mem::take(&mut self.deferred).into_iter(),
            owners: std::mem::take(&mut self.deferred_owners)
                .into_iter()
                .peekable(),
            deferred_index: 0,
            incoming: None,
            current: None,
            finished: None,
            callback: None,
            proto_callback: None,
            serial: 0,
            stopped: false,
            joined: 0,
            failed_initialization: retained.startup_error.is_some(),
            witness_taken: false,
            written: None,
            proto_key: None,
            proto_outcome: None,
            finished_outcome: None,
            proto_scan: None,
            protos_checked: false,
            join_scan: 0,
            finished_backlog: std::mem::take(&mut retained.finished_backlog).into_iter(),
            proto_batches: std::mem::take(&mut retained.proto_batches).into_iter(),
            proto_batch: None,
            save_restore: None,
            light_replay: None,
            loaded: None,
            accepted_loaded: Vec::new(),
            finished_stored: false,
            ack_cursor: 0,
            ack_error: None,
            writer_acknowledged: false,
            refused: None,
            refused_inputs: Vec::new(),
            refusal_errors: Vec::new(),
        });
        if let Some(error) = &self.generation_save_error {
            retained.close.as_mut().unwrap().block(error.clone());
        }
        Ok(())
    }

    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    pub fn arm_retained_handoff_test_control(
        &mut self,
        origin: NativeWorkOrigin,
    ) -> Result<RetainedHandoffTestReceipt, String> {
        if !matches!(
            origin,
            NativeWorkOrigin::SaveProtosCallback | NativeWorkOrigin::SaveFinishedCallback
        ) {
            return Err("retained handoff control requires a native callback origin".into());
        }
        let retained = self
            .retained
            .as_mut()
            .ok_or("streaming was not started retained")?;
        if retained
            .handoff_gate
            .as_ref()
            .is_some_and(|g| !g.released.load(Ordering::Acquire))
        {
            return Err("retained handoff control already armed".into());
        }
        let gate = Arc::new(RetainedHandoffGate {
            origin,
            entered: AtomicBool::new(false),
            released: AtomicBool::new(false),
            report_error: AtomicBool::new(false),
        });
        retained.handoff_gate = Some(gate.clone());
        Ok(RetainedHandoffTestReceipt { gate })
    }

    fn retained_handoff_held(&self, origin: NativeWorkOrigin) -> bool {
        #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
        if let Some(gate) = self
            .retained
            .as_ref()
            .and_then(|r| r.handoff_gate.as_ref())
            .filter(|g| g.origin == origin)
        {
            gate.entered.store(true, Ordering::Release);
            return !gate.released.load(Ordering::Acquire);
        }
        let _ = origin;
        false
    }

    fn retained_handoff_result(
        &self,
        origin: NativeWorkOrigin,
        outcome: Result<(), String>,
    ) -> Result<(), String> {
        #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
        if outcome.is_ok()
            && self
                .retained
                .as_ref()
                .and_then(|r| r.handoff_gate.as_ref())
                .filter(|g| g.origin == origin)
                .is_some_and(|g| g.report_error.swap(false, Ordering::AcqRel))
        {
            return Err("controlled error observed after actual native storage acceptance".into());
        }
        let _ = origin;
        outcome
    }

    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    pub fn arm_retained_native_work_test_control(
        &mut self,
        origin: NativeWorkOrigin,
    ) -> Result<RetainedStreamingStartTestReceipt, String> {
        if !matches!(
            origin,
            NativeWorkOrigin::Generate
                | NativeWorkOrigin::Decorate
                | NativeWorkOrigin::Finish
                | NativeWorkOrigin::Light
                | NativeWorkOrigin::Mesh
                | NativeWorkOrigin::Save
        ) {
            return Err("native worker control requires a queued work origin".into());
        }
        if self.retained.is_none() {
            return Err("streaming was not started retained".into());
        }
        let mut slot = self
            .shared
            .retained_work_gate
            .lock()
            .map_err(|_| "retained native control mutex poisoned")?;
        if slot
            .as_ref()
            .is_some_and(|g| !g.gate.released.lock().map(|v| *v).unwrap_or(false))
        {
            return Err("native worker control already armed".into());
        }
        let gate = Arc::new(RetainedStartGate {
            entered: AtomicBool::new(false),
            released: Mutex::new(false),
            ready: Condvar::new(),
        });
        *slot = Some(RetainedWorkGate {
            origin,
            gate: gate.clone(),
            panic_after_release: false,
        });
        Ok(RetainedStreamingStartTestReceipt { gate })
    }

    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    pub fn submit_retained_native_test_column(&mut self, x: i32, z: i32) -> Result<(), String> {
        let retained = self
            .retained
            .as_mut()
            .ok_or("streaming was not started retained")?;
        if retained.close.is_some()
            || retained.test_column.is_some()
            || self.generating.contains(&(x, z))
            || self.finishing.contains(&(x, z))
        {
            return Err("native column test submission unavailable".into());
        }
        retained.test_column = Some((x, z));
        self.generating.insert((x, z));
        self.submit(Some((x, z)), 0, Job::Generate(x, z));
        Ok(())
    }

    /// A fixed native TestTerrain stage advance for focused bin fixtures. Real
    /// decorated generators must use their existing neighbour protocol instead.
    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    pub fn poll_retained_native_test_finish(&mut self) -> Result<bool, String> {
        let retained = self
            .retained
            .as_ref()
            .ok_or("streaming was not started retained")?;
        if retained.close.is_some() {
            return Err("native column test stage is sealed".into());
        }
        let key = retained
            .test_column
            .ok_or("native column test has no original request")?;
        if self.decorates {
            return Err("native column test cannot skip decoration".into());
        }
        let done = match self.results.try_recv() {
            Ok(done) => done,
            Err(mpsc::TryRecvError::Empty) => return Ok(false),
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err("native column test producer disconnected".into());
            }
        };
        let work = self.received(done);
        match work {
            ReceivedWork {
                done: Done::Proto(x, z, proto, decorated),
                mut owner,
            } if (x, z) == key => {
                let valid = owner.as_ref().is_some_and(|o| {
                    o.origin == NativeWorkOrigin::Generate
                        && Arc::ptr_eq(&o.book, &self.shared.ownership)
                });
                if !valid {
                    self.defer(ReceivedWork {
                        done: Done::Proto(x, z, proto, decorated),
                        owner,
                    });
                    return Err("native column test lost its original receipt".into());
                }
                owner.as_mut().unwrap().advance(WorkPhase::Reconciling);
                let _reconciliation = Reconciliation(owner);
                self.generating.remove(&key);
                self.finishing.insert(key);
                self.submit(Some(key), 0, Job::Finish(proto));
                self.retained.as_mut().unwrap().test_column = None;
                Ok(true)
            }
            work => {
                let loaded = matches!(&work.done,Done::RetainedLoad(x,z,_) if (*x,*z)==key);
                self.defer(work);
                if loaded {
                    self.retained.as_mut().unwrap().test_column = None;
                    Ok(true)
                } else {
                    Err("native column test observed unrelated original work".into())
                }
            }
        }
    }

    pub fn retained_close_failures(&self) -> &[String] {
        self.retained
            .as_ref()
            .and_then(|r| r.close.as_ref())
            .map_or(&[], |c| c.history.as_slice())
    }

    pub fn retry_retained_close(&mut self, deadline: std::time::Instant) -> Result<(), String> {
        let close = self
            .retained
            .as_mut()
            .and_then(|r| r.close.as_mut())
            .ok_or("retained close has not begun")?;
        if !matches!(close.status, RetainedStreamStatus::Blocked(_)) {
            return Err("retained close retry requires a blocked attempt".into());
        }
        if deadline <= std::time::Instant::now() {
            return Err("retained close retry deadline expired".into());
        }
        close.deadline = deadline;
        close.status = RetainedStreamStatus::Pending;
        if close.proto_outcome.as_ref().is_some_and(Result::is_err) {
            close.proto_outcome = None;
        }
        if close.finished_outcome.as_ref().is_some_and(Result::is_err) {
            close.finished_outcome = None;
        }
        if let Some(batch) = &mut close.proto_batch
            && batch.outcome.as_ref().is_some_and(Result::is_err)
        {
            batch.outcome = None;
        }
        Ok(())
    }

    pub fn retained_finished_column(&self) -> Option<RetainedFinishedColumn<'_>> {
        let close = self.retained.as_ref()?.close.as_ref()?;
        let (chunk, owner) = close.finished.as_ref()?;
        Some(RetainedFinishedColumn {
            chunk,
            origin: owner.origin,
            evidence: close.loaded.as_ref(),
            ticket: RetainedColumnTicket {
                allocation: self.shared.clone(),
                serial: close.serial,
            },
        })
    }

    pub fn retained_refused_read(&self) -> Option<RetainedRefusedRead<'_>> {
        let close = self.retained.as_ref()?.close.as_ref()?;
        let refused = close.refused.as_ref()?;
        Some(RetainedRefusedRead {
            load: &refused.load,
            ticket: RetainedColumnTicket {
                allocation: self.shared.clone(),
                serial: close.serial,
            },
        })
    }

    pub fn retained_refusal_errors(&self) -> &[String] {
        self.retained
            .as_ref()
            .and_then(|r| r.close.as_ref())
            .map_or(&[], |c| c.refusal_errors.as_slice())
    }

    /// Account only an original native read which produced no new output.
    /// The input, including an optional tolerant decoded view, stays owned until
    /// the same native writer closes and its relevant versions are checked.
    pub fn resolve_retained_refused_read(
        &mut self,
        ticket: RetainedColumnTicket,
        error: String,
    ) -> Result<bool, String> {
        if !Arc::ptr_eq(&ticket.allocation, &self.shared) {
            return Err("retained read ticket allocation mismatch".into());
        }
        let shared = self.shared.clone();
        let retained = self
            .retained
            .as_mut()
            .ok_or("streaming was not started retained")?;
        let close = retained
            .close
            .as_mut()
            .ok_or("retained close has not begun")?;
        if ticket.serial != close.serial {
            return Err("retained read ticket is stale".into());
        }
        if matches!(
            close.status,
            RetainedStreamStatus::Blocked(_) | RetainedStreamStatus::Complete
        ) {
            return Err("retained refused read attempt is not open".into());
        }
        if std::time::Instant::now() >= close.deadline {
            close.block("retained producer close deadline exceeded");
            return Err("retained producer close deadline exceeded".into());
        }
        let (records, position, original_error) = if let Some(refused) = &close.refused {
            if refused.owner.origin != NativeWorkOrigin::Generate
                || !Arc::ptr_eq(&refused.owner.book, &shared.ownership)
                || refused.load.position() != refused.position
            {
                return Err("refused input has no original native read provenance".into());
            }
            let Some(records) = refused.load.evidence() else {
                close.block("unknown provider ownership cannot be disposed as a native read");
                return Err(
                    "unknown provider ownership cannot be disposed as a native read".into(),
                );
            };
            (
                records,
                refused.position,
                refused.load.error().map(str::to_owned),
            )
        } else if let (Some((chunk, owner)), Some(records)) = (&close.finished, &close.loaded) {
            if owner.origin != NativeWorkOrigin::Generate
                || !Arc::ptr_eq(&owner.book, &shared.ownership)
                || chunk.fresh
                || close.finished_stored
                || close.finished_outcome.is_some()
                || close.callback.is_some()
            {
                return Err(
                    "new output or callback cannot be disposed as a refused native read".into(),
                );
            }
            (records, (chunk.x, chunk.z), None)
        } else {
            return Err("refused read has no retained original input".into());
        };
        if !Arc::ptr_eq(records.save(), &retained.save) || records.position() != position {
            close.block("refused native read save/coordinate mismatch");
            return Err("refused native read save/coordinate mismatch".into());
        }
        match records.poll_current()? {
            None => return Ok(false),
            Some(false) => {
                close.block("refused native read conflicts with a newer accepted write");
                return Err("refused native read conflicts with a newer accepted write".into());
            }
            Some(true) => {}
        }
        let mut book = match shared.ownership.try_lock() {
            Ok(book) => book,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(false),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                close.block("retained ownership mutex poisoned");
                return Err("retained ownership mutex poisoned".into());
            }
        };
        if book.accounting_fault
            || book.receipt_mismatch
            || book.worker_exit_fault
            || book.result_disconnected
            || book.counts.iter().any(|c| c.unresolved != 0)
        {
            close.block("refused read cannot waive a native ownership fault");
            return Err("refused read cannot waive a native ownership fault".into());
        }
        let Some(next) = close.serial.checked_add(1) else {
            close.block("retained column serial exhausted");
            return Err("retained column serial exhausted".into());
        };
        close.serial = next;
        let stored = if let Some(refused) = close.refused.take() {
            refused.owner.settle_locked(&mut book);
            RetainedRefusedInput::Raw(refused.load)
        } else {
            let (chunk, owner) = close.finished.take().unwrap();
            owner.settle_locked(&mut book);
            RetainedRefusedInput::Loaded {
                _chunk: chunk,
                records: close.loaded.take().unwrap(),
            }
        };
        self.generating.remove(&position);
        let original_error = original_error.unwrap_or(error);
        close.refusal_errors.push(original_error.clone());
        close.history.push(original_error);
        close.refused_inputs.push(stored);
        close.status = RetainedStreamStatus::Pending;
        Ok(true)
    }

    pub fn reject_retained_finished_column(
        &mut self,
        ticket: RetainedColumnTicket,
        error: String,
    ) -> Result<(), String> {
        if !Arc::ptr_eq(&ticket.allocation, &self.shared) {
            return Err("retained column ticket allocation mismatch".into());
        }
        let close = self
            .retained
            .as_mut()
            .and_then(|r| r.close.as_mut())
            .ok_or("retained close has not begun")?;
        if ticket.serial != close.serial || close.finished.is_none() {
            return Err("retained column ticket is stale".into());
        }
        if let Some(next) = close.serial.checked_add(1) {
            close.serial = next;
        } else {
            close.block("retained column serial exhausted");
            return Err("retained column serial exhausted".into());
        }
        close.block(error);
        Ok(())
    }

    /// Resolve only after the caller validates the complete borrowed native
    /// column. No unique payload crosses this boundary, including on failure.
    pub fn resolve_retained_finished_column(
        &mut self,
        ticket: RetainedColumnTicket,
        disposition: RetainedColumnDisposition,
    ) -> Result<bool, String> {
        if !Arc::ptr_eq(&ticket.allocation, &self.shared) {
            return Err("retained column ticket allocation mismatch".into());
        }
        let expected_save = self
            .retained
            .as_ref()
            .ok_or("streaming was not started retained")?
            .save
            .clone();
        #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
        let handoff_gate = self
            .retained
            .as_ref()
            .and_then(|r| r.handoff_gate.as_ref())
            .cloned();
        let close = self
            .retained
            .as_mut()
            .and_then(|r| r.close.as_mut())
            .ok_or("retained close has not begun")?;
        if ticket.serial != close.serial || close.finished.is_none() {
            return Err("retained column ticket is stale".into());
        }
        if matches!(
            close.status,
            RetainedStreamStatus::Blocked(_) | RetainedStreamStatus::Complete
        ) {
            return Err("retained column attempt is not open".into());
        }
        if std::time::Instant::now() >= close.deadline {
            close.block("retained producer close deadline exceeded");
            return Err("retained producer close deadline exceeded".into());
        }
        let chunk = &close.finished.as_ref().unwrap().0;
        match disposition {
            RetainedColumnDisposition::AlreadyStored => {
                let Some(evidence) = &close.loaded else {
                    return Err("original raw native storage provenance required".into());
                };
                if chunk.fresh
                    || evidence.position() != (chunk.x, chunk.z)
                    || !Arc::ptr_eq(evidence.save(), &expected_save)
                {
                    return Err("loaded native provenance allocation/position mismatch".into());
                }
                match evidence.poll_current()? {
                    None => return Ok(false),
                    Some(false) => {
                        close.block("loaded native record changed after strict validation");
                        return Err("loaded native record changed after strict validation".into());
                    }
                    Some(true) => {}
                }
                let Some(next) = close.serial.checked_add(1) else {
                    close.block("retained column serial exhausted");
                    return Err("retained column serial exhausted".into());
                };
                close.serial = next;
                close.finished_stored = true;
                close.finished_outcome = Some(Ok(()));
                return Ok(true);
            }
            RetainedColumnDisposition::StoreFresh if !chunk.fresh => {
                return Err("loaded column cannot be classified fresh".into());
            }
            RetainedColumnDisposition::StoreFresh => {}
        }
        if close.finished_outcome.is_some() {
            return Ok(false);
        }
        if close.callback.is_none()
            && close.finished.as_ref().unwrap().1.origin != NativeWorkOrigin::SaveFinishedCallback
        {
            let mut book = match self.shared.ownership.try_lock() {
                Ok(book) => book,
                Err(std::sync::TryLockError::WouldBlock) => return Ok(false),
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    close.block("retained ownership mutex poisoned");
                    return Err("retained ownership mutex poisoned".into());
                }
            };
            close.callback = Some(WorkLease::new_locked(
                &self.shared.ownership,
                &mut book,
                NativeWorkOrigin::SaveFinishedCallback,
            ));
        }
        #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
        if let Some(gate) = handoff_gate
            .as_ref()
            .filter(|g| g.origin == NativeWorkOrigin::SaveFinishedCallback)
        {
            gate.entered.store(true, Ordering::Release);
            if !gate.released.load(Ordering::Acquire) {
                return Ok(false);
            }
        }
        let Some(next) = close.serial.checked_add(1) else {
            close.block("retained column serial exhausted");
            return Err("retained column serial exhausted".into());
        };
        close.serial = next;
        let outcome = self.saves.save_finished(chunk);
        #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
        let outcome = if outcome.is_ok()
            && handoff_gate
                .as_ref()
                .filter(|g| g.origin == NativeWorkOrigin::SaveFinishedCallback)
                .is_some_and(|g| g.report_error.swap(false, Ordering::AcqRel))
        {
            Err("controlled error observed after actual native storage acceptance".into())
        } else {
            outcome
        };
        let accepted = outcome.is_ok();
        close.finished_outcome = Some(outcome);
        Ok(accepted)
    }

    pub fn poll_retained_close(&mut self, world: Option<&mut World>) -> RetainedStreamStatus {
        let Some(retained) = &mut self.retained else {
            return RetainedStreamStatus::Blocked("streaming was not started retained".into());
        };
        let Some(mut close) = retained.close.take() else {
            return RetainedStreamStatus::Blocked("retained close has not begun".into());
        };
        let status = self.poll_retained_turn(&mut close, world);
        close.status = status.clone();
        self.retained.as_mut().unwrap().close = Some(close);
        status
    }

    fn poll_retained_turn(
        &mut self,
        close: &mut RetainedProducerClose,
        mut world: Option<&mut World>,
    ) -> RetainedStreamStatus {
        if matches!(
            close.status,
            RetainedStreamStatus::Blocked(_) | RetainedStreamStatus::Complete
        ) {
            return close.status.clone();
        }
        if close.world != world.as_deref().map(|w| w as *const World as usize)
            || close.membership != world.as_deref().map(|w| w.chunks.membership_revision())
        {
            return close.block("retained close original World changed");
        }
        let started = std::time::Instant::now();
        for _ in 0..16 {
            if std::time::Instant::now() >= close.deadline {
                return close.block("retained producer close deadline exceeded");
            }
            if started.elapsed() >= std::time::Duration::from_millis(2) {
                return RetainedStreamStatus::Pending;
            }
            let shared = self.shared.clone();
            let mut book = match shared.ownership.try_lock() {
                Ok(book) => book,
                Err(std::sync::TryLockError::WouldBlock) => return RetainedStreamStatus::Pending,
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return close.block("retained ownership mutex poisoned");
                }
            };
            let no_jobs = book.counts.iter().all(|c| c.submitted == 0);
            if book.accounting_fault
                || book.receipt_mismatch
                || book.worker_exit_fault
                || (book.result_disconnected && !no_jobs)
                || book.counts.iter().any(|c| c.unresolved != 0)
            {
                return close.block("retained native ownership fault; original owners retained");
            }
            close.failed_initialization |= book.result_disconnected && no_jobs;
            if close.refused.is_some() {
                return RetainedStreamStatus::NeedsRefusedRead;
            }
            if let Some(replay) = &mut close.light_replay {
                if let Some(&(seq, p)) = self.light_log.get(replay.index) {
                    replay.index += 1;
                    if seq > replay.snapshot
                        && ((p.0 >> 4) - replay.x).abs() <= 1
                        && ((p.2 >> 4) - replay.z).abs() <= 1
                    {
                        replay.positions.push(p);
                    }
                    continue;
                }
                if !replay.positions.is_empty() {
                    let Some(world) = world.as_deref_mut() else {
                        return close.block("retained light replay lost original World");
                    };
                    let mut changed = LightChanges::default();
                    crate::light::relight(
                        &mut world.chunks,
                        &self.light_tables,
                        self.sky_light,
                        &replay.positions,
                        true,
                        &mut changed,
                    );
                    Self::mark_light_changes(world, &changed);
                }
                replay.owner.take().unwrap().settle_locked(&mut book);
                close.light_replay = None;
                continue;
            }
            if let Some(restore) = &mut close.save_restore {
                if restore.current.is_none() {
                    restore.current = restore.protos.next();
                }
                if let Some((key, proto)) = restore.current.take() {
                    if self.protos.get(&key).is_some_and(Option::is_some) {
                        restore.current = Some((key, proto));
                        return close.block("retained save proto owner collision");
                    }
                    self.protos.insert(key, Some(proto));
                    if restore.outcome.is_err() {
                        self.written.insert(key);
                    }
                    continue;
                }
                restore.owner.take().unwrap().settle_locked(&mut book);
                close.save_restore = None;
                continue;
            }
            if close.finished.is_some() {
                let Some(outcome) = close.finished_outcome.take() else {
                    return RetainedStreamStatus::NeedsFinishedColumn;
                };
                if !close.finished_stored {
                    if let Some(callback) = &close.callback {
                        callback.storage_locked(&mut book, outcome.is_ok());
                    } else if let Some((_, owner)) = &close.finished {
                        owner.storage_locked(&mut book, outcome.is_ok());
                    }
                }
                if let Err(error) = outcome {
                    close.finished_outcome = Some(Err(error.clone()));
                    return close.block(error);
                }
                if let Some(callback) = close.callback.take() {
                    callback.settle_locked(&mut book);
                }
                let (chunk, owner) = close.finished.take().unwrap();
                if close.finished_stored {
                    close.accepted_loaded.push(close.loaded.take().unwrap());
                    close.finished_stored = false;
                }
                let key = (chunk.x, chunk.z);
                self.generating.remove(&key);
                self.finishing.remove(&key);
                owner.settle_locked(&mut book);
                continue;
            }
            if close.current.is_none() {
                if let Some(done) = close.deferred.next() {
                    let index = close.deferred_index;
                    close.deferred_index += 1;
                    let owner = if close.owners.peek().is_some_and(|(at, _)| *at == index) {
                        close.owners.next().map(|(_, owner)| owner)
                    } else {
                        None
                    };
                    close.current = Some(ReceivedWork { done, owner });
                } else {
                    if close.owners.peek().is_some() {
                        return close.block("retained deferred receipt without original payload");
                    }
                    if close.incoming.is_none() {
                        match self.results.try_recv() {
                            Ok(done) => close.incoming = Some(done),
                            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => {}
                        }
                    }
                    if let Some(done) = &close.incoming {
                        let mut returned = match shared.returned.try_lock() {
                            Ok(returned) => returned,
                            Err(std::sync::TryLockError::WouldBlock) => {
                                return RetainedStreamStatus::Pending;
                            }
                            Err(std::sync::TryLockError::Poisoned(_)) => {
                                return close.block("retained returned receipt mutex poisoned");
                            }
                        };
                        if !returned
                            .front()
                            .is_some_and(|(stamp, _)| *stamp == done.stamp())
                        {
                            book.receipt_mismatch = true;
                            return close.block("retained native result receipt mismatch");
                        }
                        let owner = returned.pop_front().map(|(_, owner)| owner);
                        close.current = Some(ReceivedWork {
                            done: close.incoming.take().unwrap(),
                            owner,
                        });
                    }
                }
            }
            if let Some(mut work) = close.current.take() {
                let Some(mut owner) = work.owner.take() else {
                    close.current = Some(work);
                    return close.block("retained native result has no original work lease");
                };
                let correct_origin = match &work.done {
                    Done::Generated(_) => matches!(
                        owner.origin,
                        NativeWorkOrigin::Generate | NativeWorkOrigin::Finish
                    ),
                    Done::RetainedLoad(..) => owner.origin == NativeWorkOrigin::Generate,
                    Done::Proto(..) => owner.origin == NativeWorkOrigin::Generate,
                    Done::Decorated(..) => owner.origin == NativeWorkOrigin::Decorate,
                    Done::Saved(..) => owner.origin == NativeWorkOrigin::Save,
                    Done::Lit(..) => owner.origin == NativeWorkOrigin::Light,
                    Done::Meshed(..) => owner.origin == NativeWorkOrigin::Mesh,
                    Done::Remote(_) => false,
                };
                if !correct_origin || !Arc::ptr_eq(&owner.book, &shared.ownership) {
                    work.owner = Some(owner);
                    close.current = Some(work);
                    book.receipt_mismatch = true;
                    return close.block("retained result origin/allocation mismatch");
                }
                owner.advance_locked(&mut book, WorkPhase::Reconciling);
                if world.is_none() {
                    work.owner = Some(owner);
                    close.current = Some(work);
                    return close
                        .block("retained partial constructor unexpectedly submitted native work");
                }
                let world = world.as_deref_mut().unwrap();
                match work.done {
                    Done::Remote(input) => {
                        close.current = Some(ReceivedWork {
                            done: Done::Remote(input),
                            owner: Some(owner),
                        });
                        return close
                            .block("native producer cannot consume a remote streaming result");
                    }
                    Done::RetainedLoad(x, z, load) => match load.into_parts() {
                        Ok((chunk, evidence)) => {
                            let collision = world.chunks.contains_key(&(chunk.x, chunk.z));
                            let valid = evidence.position() == (x, z)
                                && (chunk.x, chunk.z) == (x, z)
                                && Arc::ptr_eq(
                                    evidence.save(),
                                    &self.retained.as_ref().unwrap().save,
                                );
                            close.finished = Some((Arc::new(chunk), owner));
                            close.loaded = Some(evidence);
                            let Some(next) = close.serial.checked_add(1) else {
                                return close.block("retained column serial exhausted");
                            };
                            close.serial = next;
                            if !valid || collision {
                                return close.block("retained loaded native allocation/coordinate/resident ambiguity");
                            }
                            return RetainedStreamStatus::NeedsFinishedColumn;
                        }
                        Err(load) => {
                            close.refused = Some(RetainedRefusedReadOwner {
                                position: (x, z),
                                load,
                                owner,
                            });
                            let Some(next) = close.serial.checked_add(1) else {
                                return close.block("retained column serial exhausted");
                            };
                            close.serial = next;
                            return RetainedStreamStatus::NeedsRefusedRead;
                        }
                    },
                    Done::Generated(chunk) => {
                        let collision = world.chunks.contains_key(&(chunk.x, chunk.z));
                        close.finished = Some((Arc::new(chunk), owner));
                        let Some(next) = close.serial.checked_add(1) else {
                            return close.block("retained column serial exhausted");
                        };
                        close.serial = next;
                        if collision {
                            return close.block(
                                "retained finished column collides with original resident owner",
                            );
                        }
                        return RetainedStreamStatus::NeedsFinishedColumn;
                    }
                    Done::Proto(x, z, proto, decorated) => {
                        if self.protos.get(&(x, z)).is_some_and(Option::is_some) {
                            close.current = Some(ReceivedWork {
                                done: Done::Proto(x, z, proto, decorated),
                                owner: Some(owner),
                            });
                            return close.block("retained proto owner collision");
                        }
                        self.generating.remove(&(x, z));
                        self.protos.insert((x, z), Some(proto));
                        if decorated {
                            self.decorated.insert((x, z));
                            self.written.insert((x, z));
                        }
                    }
                    Done::Decorated(x, z, region) => {
                        let collision = (0..9).any(|i| {
                            self.protos
                                .get(&(x + i % 3 - 1, z + i / 3 - 1))
                                .is_some_and(Option::is_some)
                        });
                        if collision || self.decorating == 0 {
                            close.current = Some(ReceivedWork {
                                done: Done::Decorated(x, z, region),
                                owner: Some(owner),
                            });
                            return close.block("retained decoration owner/count mismatch");
                        }
                        self.receive_decorated(x, z, *region);
                    }
                    Done::Saved(protos, outcome) => {
                        if self.saving == 0 {
                            close.current = Some(ReceivedWork {
                                done: Done::Saved(protos, outcome),
                                owner: Some(owner),
                            });
                            return close.block("retained save owner/count mismatch");
                        }
                        self.saving -= 1;
                        let error = outcome.as_ref().err().cloned();
                        close.save_restore = Some(RetainedSaveRestore {
                            protos: protos.into_iter(),
                            current: None,
                            owner: Some(owner),
                            outcome,
                        });
                        if let Some(error) = error {
                            return close.block(error);
                        }
                        continue;
                    }
                    Done::Lit(x, z, sections, seq) => {
                        if self.lighting.get(&(x, z)) == Some(&seq) {
                            self.lighting.remove(&(x, z));
                            let snapshot = self.light_snapshots.remove(&(x, z)).unwrap_or(u64::MAX);
                            if let Some(chunk) = world.chunks.get_mut(&(x, z)) {
                                let chunk = Arc::make_mut(chunk);
                                let first = self.lit.insert((x, z));
                                let mut changed = LightChanges::default();
                                if first {
                                    apply_lighting(chunk, sections);
                                } else {
                                    apply_relighting(chunk, sections, &mut changed);
                                }
                                if (first && !chunk.light_loaded) || !changed.is_empty() {
                                    world.modified.insert((x, z));
                                }
                                chunk.lit = true;
                                Self::mark_light_changes(world, &changed);
                                close.light_replay = Some(RetainedLightReplay {
                                    x,
                                    z,
                                    snapshot,
                                    index: 0,
                                    positions: Vec::new(),
                                    owner: Some(owner),
                                });
                                continue;
                            }
                        }
                    }
                    Done::Meshed(x, sy, z, _mesh, seq) => {
                        if self.meshing.get(&(x, sy, z)) == Some(&seq) {
                            self.meshing.remove(&(x, sy, z));
                        }
                    }
                }
                owner.settle_locked(&mut book);
                continue;
            }
            if let Some((chunk, owner)) = close.finished_backlog.next() {
                let collision = world
                    .as_deref()
                    .is_none_or(|world| world.chunks.contains_key(&(chunk.x, chunk.z)));
                let owner = owner.unwrap_or_else(|| {
                    WorkLease::new_locked(
                        &shared.ownership,
                        &mut book,
                        NativeWorkOrigin::SaveFinishedCallback,
                    )
                });
                close.finished = Some((chunk, owner));
                let Some(next) = close.serial.checked_add(1) else {
                    return close.block("retained column serial exhausted");
                };
                close.serial = next;
                if collision {
                    return close.block(
                        "retained failed finished column resident/original-World ambiguity",
                    );
                }
                return RetainedStreamStatus::NeedsFinishedColumn;
            }
            if close.proto_batch.is_none() {
                close.proto_batch = close.proto_batches.next();
            }
            if let Some(batch) = &mut close.proto_batch {
                if batch.accepted {
                    if let Some(key) = batch.keys.get(batch.cleared) {
                        self.written.remove(key);
                        batch.cleared += 1;
                        continue;
                    }
                    close
                        .proto_batch
                        .take()
                        .unwrap()
                        .owner
                        .settle_locked(&mut book);
                    continue;
                }
                if let Some(outcome) = batch.outcome.take() {
                    batch.owner.storage_locked(&mut book, outcome.is_ok());
                    match outcome {
                        Ok(()) => {
                            batch.accepted = true;
                            continue;
                        }
                        Err(error) => {
                            batch.outcome = Some(Err(error.clone()));
                            return close.block(error);
                        }
                    }
                }
                // This is one original native callback batch, with its exact
                // original keys and proto owners. Native encoding is indivisible.
                let refs: Option<Vec<&Proto>> = batch
                    .keys
                    .iter()
                    .map(|key| self.protos.get(key).and_then(Option::as_ref))
                    .collect();
                let Some(refs) = refs else {
                    book.accounting_fault = true;
                    return close.block("retained original proto callback batch lost an owner");
                };
                if self.retained_handoff_held(NativeWorkOrigin::SaveProtosCallback) {
                    return RetainedStreamStatus::Pending;
                }
                drop(book);
                batch.outcome = Some(self.retained_handoff_result(
                    NativeWorkOrigin::SaveProtosCallback,
                    self.saves.save_protos(&refs),
                ));
                return RetainedStreamStatus::Pending;
            }
            let mut active = false;
            for counts in &book.counts {
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
                if total != Some(counts.submitted) {
                    return close.block("retained all-origin conservation mismatch");
                }
                active |= counts.queued != 0
                    || counts.worker_held != 0
                    || counts.returned != 0
                    || counts.reconciling != 0;
            }
            // The close-specific proto callback is itself a reconciling origin.
            if active && close.proto_callback.is_none() {
                return RetainedStreamStatus::Pending;
            }
            if !close.protos_checked {
                if close.proto_scan.is_none() {
                    close.proto_scan = Some(std::mem::take(&mut self.protos).into_iter());
                }
                if let Some((key, proto)) = close.proto_scan.as_mut().unwrap().next() {
                    let missing = proto.is_none();
                    self.protos.insert(key, proto);
                    if missing {
                        book.accounting_fault = true;
                        return close
                            .block("retained checked-out proto has no original completion");
                    }
                    continue;
                }
                close.proto_scan = None;
                close.protos_checked = true;
            }
            if close.proto_key.is_none() {
                if close.written.is_none() {
                    close.written = Some(std::mem::take(&mut self.written).into_iter());
                }
                close.proto_key = close.written.as_mut().unwrap().next();
            }
            if let Some(key) = close.proto_key {
                let Some(proto) = self.protos.get(&key).and_then(Option::as_ref) else {
                    return close.block("retained written proto is not owned");
                };
                if let Some(outcome) = close.proto_outcome.take() {
                    close
                        .proto_callback
                        .as_ref()
                        .unwrap()
                        .storage_locked(&mut book, outcome.is_ok());
                    if let Err(error) = outcome {
                        close.proto_outcome = Some(Err(error.clone()));
                        return close.block(error);
                    }
                    close
                        .proto_callback
                        .take()
                        .unwrap()
                        .settle_locked(&mut book);
                    close.proto_key = None;
                    continue;
                }
                if close.proto_callback.is_none() {
                    close.proto_callback = Some(WorkLease::new_locked(
                        &shared.ownership,
                        &mut book,
                        NativeWorkOrigin::SaveProtosCallback,
                    ));
                }
                if self.retained_handoff_held(NativeWorkOrigin::SaveProtosCallback) {
                    return RetainedStreamStatus::Pending;
                }
                drop(book);
                close.proto_outcome = Some(self.retained_handoff_result(
                    NativeWorkOrigin::SaveProtosCallback,
                    self.saves.save_protos(&[proto]),
                ));
                return RetainedStreamStatus::Pending;
            }
            if active {
                return RetainedStreamStatus::Pending;
            }
            if !self.generating.is_empty()
                || self.decorating != 0
                || !self.finishing.is_empty()
                || self.saving != 0
                || !self.lighting.is_empty()
                || !self.meshing.is_empty()
                || !self.failed_finished.is_empty()
            {
                return close.block("retained native owner has no correlated completion");
            }
            if !close.stopped {
                let queue = match shared.queue.try_lock() {
                    Ok(queue) => queue,
                    Err(std::sync::TryLockError::WouldBlock) => {
                        return RetainedStreamStatus::Pending;
                    }
                    Err(std::sync::TryLockError::Poisoned(_)) => {
                        return close.block("retained queue mutex poisoned");
                    }
                };
                let queued = match shared.queued_owners.try_lock() {
                    Ok(queued) => queued,
                    Err(std::sync::TryLockError::WouldBlock) => {
                        return RetainedStreamStatus::Pending;
                    }
                    Err(std::sync::TryLockError::Poisoned(_)) => {
                        return close.block("retained queued owner mutex poisoned");
                    }
                };
                let returned = match shared.returned.try_lock() {
                    Ok(returned) => returned,
                    Err(std::sync::TryLockError::WouldBlock) => {
                        return RetainedStreamStatus::Pending;
                    }
                    Err(std::sync::TryLockError::Poisoned(_)) => {
                        return close.block("retained returned receipt mutex poisoned");
                    }
                };
                if !queue.is_empty() || !queued.is_empty() || !returned.is_empty() {
                    return close.block("retained queue/receipt has unaccounted native work");
                }
                shared.stop.store(true, Ordering::Relaxed);
                shared.ready.notify_all();
                close.stopped = true;
                drop(returned);
                drop(queued);
                drop(queue);
            }
            // Release accounting before a finished thread's TLS/destructor
            // result is inspected. is_finished proves join cannot wait on it.
            let zero_jobs = book.counts.iter().all(|c| c.submitted == 0);
            close.failed_initialization |= book.worker_initialization_fault;
            drop(book);
            if !self.threads.is_empty() {
                let index = close.join_scan % self.threads.len();
                close.join_scan = close.join_scan.wrapping_add(1);
                if !self.threads[index].is_finished() {
                    return RetainedStreamStatus::Pending;
                }
                let handle = self.threads.swap_remove(index);
                let result = handle.join();
                close.joined += 1;
                if result.is_err() {
                    if zero_jobs {
                        close.failed_initialization = true;
                        close.history.push(
                            "native worker initialization panicked; actual handle reaped".into(),
                        );
                    } else {
                        return close.block("native worker panicked after submissions");
                    }
                }
                continue;
            }
            if close.joined != self.retained.as_ref().unwrap().workers {
                return close.block("retained worker membership mismatch");
            }
            return RetainedStreamStatus::Complete;
        }
        RetainedStreamStatus::Pending
    }

    pub fn take_sealed(&mut self) -> Option<SealedStream> {
        let retained = self.retained.as_mut()?;
        let close = retained.close.as_mut()?;
        if close.status != RetainedStreamStatus::Complete
            || close.witness_taken
            || !self.threads.is_empty()
        {
            return None;
        }
        close.witness_taken = true;
        Some(SealedStream {
            allocation: self.shared.clone(),
            save: retained.save.clone(),
            workers: retained.workers,
            failed_initialization: close.failed_initialization,
            refusals: close.refusal_errors.clone(),
        })
    }

    pub fn poll_retained_writer_ack(
        &mut self,
        writer: &crate::save::NativeWriterClosed,
    ) -> RetainedStreamStatus {
        let Some(retained) = &mut self.retained else {
            return RetainedStreamStatus::Blocked("streaming was not started retained".into());
        };
        let Some(close) = &mut retained.close else {
            return RetainedStreamStatus::Blocked("retained close has not begun".into());
        };
        if close.status != RetainedStreamStatus::Complete
            || !Arc::ptr_eq(&retained.save, writer.save())
        {
            return RetainedStreamStatus::Blocked(
                "retained writer acknowledgement allocation/producer mismatch".into(),
            );
        }
        if let Some(error) = &close.ack_error {
            return RetainedStreamStatus::Blocked(error.clone());
        }
        let started = std::time::Instant::now();
        for _ in 0..16 {
            let records = if close.ack_cursor < close.accepted_loaded.len() {
                close.accepted_loaded.get(close.ack_cursor)
            } else {
                close
                    .refused_inputs
                    .get(close.ack_cursor - close.accepted_loaded.len())
                    .map(RetainedRefusedInput::records)
            };
            let Some(records) = records else {
                close.writer_acknowledged = true;
                return RetainedStreamStatus::Complete;
            };
            let error = match records.poll_closed_current(writer) {
                Ok(None) => return RetainedStreamStatus::Pending,
                Ok(Some(true)) => {
                    close.ack_cursor += 1;
                    None
                }
                Ok(Some(false)) => {
                    Some("loaded native records changed before matching writer closure".to_owned())
                }
                Err(error) => Some(error),
            };
            if let Some(error) = error {
                close.history.push(error.clone());
                close.ack_error = Some(error.clone());
                return RetainedStreamStatus::Blocked(error);
            }
            if started.elapsed() >= std::time::Duration::from_millis(2) {
                return RetainedStreamStatus::Pending;
            }
        }
        RetainedStreamStatus::Pending
    }

    pub fn finish_retained_close(
        &mut self,
        witness: SealedStream,
        writer: &crate::save::NativeWriterClosed,
    ) -> Result<(), SealedStream> {
        let valid = self.retained.as_ref().is_some_and(|r| {
            r.armed
                && Arc::ptr_eq(&r.save, writer.save())
                && Arc::ptr_eq(&r.save, &witness.save)
                && r.workers == witness.workers
                && r.close.as_ref().is_some_and(|c| {
                    c.status == RetainedStreamStatus::Complete
                        && c.witness_taken
                        && ((c.accepted_loaded.is_empty() && c.refused_inputs.is_empty())
                            || c.writer_acknowledged)
                })
        }) && Arc::ptr_eq(&self.shared, &witness.allocation)
            && self.threads.is_empty();
        if !valid {
            return Err(witness);
        }
        self.retained.as_mut().unwrap().armed = false;
        Ok(())
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
        #[cfg(feature = "paired-packages")]
        if let Some(mode) = &self.remote_mode {
            if mode.armed.load(Ordering::Acquire) {
                use std::io::Write;
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "fatal: unresolved retained remote streaming owner"
                );
                std::process::abort();
            }
            // The opaque closed owner has checked every native join and owns
            // all remaining original payloads; never invoke legacy Drop joins.
            return;
        }
        #[cfg(feature = "paired-packages")]
        if let Some(retained) = &self.retained {
            if retained.armed {
                use std::io::Write as _;
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "Horizon: unresolved retained Streaming owner relinquished"
                );
                std::process::abort();
            }
            // Explicit producer and writer acknowledgement already reaped every
            // handle. The legacy destructor is not a retained cleanup algorithm.
            return;
        }
        // Legacy new() historically panics at spawn failure before constructing
        // a Streaming: its already-created handles are dropped, never joined.
        if !self.constructor_complete {
            return;
        }
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

#[cfg(feature = "paired-packages")]
pub use retained_remote::{
    ClosedRemoteStreaming, RemoteCloseStatus, RemoteHandoff, RemoteInput, RemoteLightOutcome,
    RemoteMeshProfile, RemoteObservation, RemoteOutput, RemotePrepareRefusal, RemoteRefusal,
    RemoteRefusedInput, RetainedRemoteStreaming,
};

/// A fixed policy in the original pool, not another generator or storage owner.
#[cfg(feature = "paired-packages")]
mod retained_remote {
    use super::*;
    use std::{
        fmt,
        sync::{
            MutexGuard, TryLockError,
            atomic::{AtomicU8, AtomicU64},
        },
        time::Instant,
    };
    const CLOSED: u64 = 1 << 63;
    const QUEUED: u8 = 0;
    const ACTIVE: u8 = 1;
    const STORED: u8 = 2;
    const RECONCILING: u8 = 3;
    const LIMIT: usize = 64;

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn attempt<T>(mutex: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
        match mutex.try_lock() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(error)) => Some(error.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }
    struct Provider;
    impl ChunkGenerator for Provider {
        type Worker = ();
        fn worker(&self) {}
        fn proto(&self, _: &mut (), _: i32, _: i32) -> Proto {
            unreachable!("fixed remote admission excludes generation")
        }
        fn finish(&self, _: Proto) -> Chunk {
            unreachable!("fixed remote admission excludes generation")
        }
        fn save_protos(&self, _: &[&Proto]) -> Result<(), String> {
            Err("remote streaming owns no native storage".into())
        }
        fn save_finished(&self, _: &Arc<Chunk>) -> Result<(), String> {
            Err("remote streaming owns no native storage".into())
        }
    }
    /// Fixed values selected from the actual Login dimension/obfuscated seed
    /// and client settings, before any original remote worker starts.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct RemoteMeshProfile {
        pub sky_light: bool,
        pub nether_shading: bool,
        pub obfuscated_biome_seed: i64,
        pub biome_blend: i32,
        pub smooth_lighting: bool,
        pub cutout_leaves: bool,
    }
    pub enum RemoteLightOutcome {
        Missing,
        Unchanged,
        Replaced {
            previous: Arc<Chunk>,
            sections: Vec<usize>,
        },
    }
    #[derive(Clone, Copy)]
    enum Kind {
        Light {
            x: i32,
            z: i32,
            sky: bool,
        },
        Mesh {
            x: i32,
            sy: i32,
            z: i32,
            settings: MeshSettings,
        },
    }
    pub struct RemoteInput {
        context: Arc<WorkerContext>,
        profile: RemoteMeshProfile,
        region: Box<Region>,
        kind: Kind,
    }
    impl RemoteInput {
        fn position(&self) -> (i32, i32) {
            match self.kind {
                Kind::Light { x, z, .. } | Kind::Mesh { x, z, .. } => (x, z),
            }
        }
        fn origin(&self) -> NativeWorkOrigin {
            match self.kind {
                Kind::Light { .. } => NativeWorkOrigin::Light,
                Kind::Mesh { .. } => NativeWorkOrigin::Mesh,
            }
        }
    }
    pub enum RemoteOutput {
        Light {
            position: (i32, i32),
            sequence: u64,
            sections: Vec<crate::chunk::Light>,
        },
        Mesh {
            section: (i32, i32, i32),
            sequence: u64,
            mesh: SectionMesh,
        },
    }
    pub struct RemoteHandoff {
        pub sequence: u64,
        pub input: RemoteInput,
        pub output: Option<RemoteOutput>,
        /// Current means the original snapshot/sequence still matches, not upload/application.
        pub current: bool,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum RemoteRefusal {
        InvalidWorkers,
        InvalidProfile,
        InvalidLight,
        InvalidSection,
        MissingColumn,
        NotRunning,
        Closed,
        Busy,
        Full,
        ForeignContext,
        ForeignProfile,
        Exhausted,
        Attempt,
        Deadline,
        State,
    }
    pub struct RemotePrepareRefusal {
        pub reason: RemoteRefusal,
        pub context: Arc<WorkerContext>,
        pub workers: usize,
    }
    impl fmt::Debug for RemotePrepareRefusal {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("RemotePrepareRefusal")
                .field("reason", &self.reason)
                .field("workers", &self.workers)
                .finish_non_exhaustive()
        }
    }
    pub struct RemoteRefusedInput {
        pub reason: RemoteRefusal,
        pub input: RemoteInput,
    }
    impl fmt::Debug for RemoteRefusedInput {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("RemoteRefusedInput")
                .field("reason", &self.reason)
                .finish_non_exhaustive()
        }
    }
    struct Payload {
        input: Option<RemoteInput>,
        output: Option<Done>,
        receipt: Option<WorkLease>,
    }
    pub struct JobOwner {
        pub(super) sequence: u64,
        pub(super) origin: NativeWorkOrigin,
        phase: AtomicU8,
        payload: Mutex<Payload>,
        mode: std::sync::Weak<State>,
    }
    impl JobOwner {
        pub(super) fn mark_active(&self) {
            self.phase.store(ACTIVE, Ordering::Release);
        }
        pub(super) fn execute(&self, faces: &mut Option<(Arc<MeshTables>, FaceVisibility)>) {
            let mut payload = lock(&self.payload);
            let mode = self
                .mode
                .upgrade()
                .expect("original remote owner remains registered");
            mode.control(Point::BeforeWork);
            let input = payload.input.as_ref().unwrap();
            let output = match input.kind {
                Kind::Light { x, z, sky } => Done::Lit(
                    x,
                    z,
                    light_chunk(&borrow(&input.region), &input.context.light, sky),
                    self.sequence,
                ),
                Kind::Mesh { x, sy, z, settings } => Done::Meshed(
                    x,
                    sy,
                    z,
                    mesh_job(&input.region, sy, settings, &input.context, faces),
                    self.sequence,
                ),
            };
            payload.output = Some(output);
            self.phase.store(STORED, Ordering::Release);
            drop(payload);
            mode.control(Point::AfterPublish);
        }
    }
    pub(super) struct State {
        pub(super) sender: Mutex<Option<mpsc::Sender<Done>>>,
        pub(super) armed: AtomicBool,
        gate: AtomicU64,
        context: Arc<WorkerContext>,
        slots: Mutex<VecDeque<Arc<JobOwner>>>,
        wakes: Mutex<Vec<thread::Thread>>,
        failures: Mutex<Vec<String>>,
        #[cfg(all(
            test,
            feature = "native-writer-test-control",
            debug_assertions,
            target_os = "linux"
        ))]
        test: Mutex<Option<Arc<Control>>>,
    }
    impl State {
        pub(super) fn sealed(&self) -> bool {
            self.gate.load(Ordering::Acquire) & CLOSED != 0
        }
        pub(super) fn claim(&self) -> bool {
            let mut observed = self.gate.load(Ordering::Acquire);
            loop {
                if observed & CLOSED != 0 {
                    return false;
                }
                match self.gate.compare_exchange(
                    observed,
                    observed + 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return true,
                    Err(next) => observed = next,
                }
            }
        }
        pub(super) fn release_claim(&self) {
            self.gate.fetch_sub(1, Ordering::AcqRel);
        }
        pub(super) fn register_wake(&self, wake: thread::Thread) {
            lock(&self.wakes).push(wake);
        }
        fn seal(&self) {
            self.gate.fetch_or(CLOSED, Ordering::AcqRel);
        }
        pub(super) fn before_claim(&self) {
            #[cfg(all(
                test,
                feature = "native-writer-test-control",
                debug_assertions,
                target_os = "linux"
            ))]
            let _slots = lock(&self.test)
                .as_ref()
                .is_some_and(|test| test.point == Point::BeforeClaim)
                .then(|| lock(&self.slots));
            self.control(Point::BeforeClaim);
        }
        pub(super) fn before_park(&self) {
            self.control(Point::BeforePark);
        }
        fn control(&self, point: Point) {
            #[cfg(all(
                test,
                feature = "native-writer-test-control",
                debug_assertions,
                target_os = "linux"
            ))]
            {
                let control = lock(&self.test).clone();
                if let Some(control) = control {
                    control.enter(point);
                }
            }
            #[cfg(not(all(
                test,
                feature = "native-writer-test-control",
                debug_assertions,
                target_os = "linux"
            )))]
            let _ = point;
        }
        #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
        pub(super) fn refuse_spawn(&self, worker: usize) -> bool {
            #[cfg(all(test, target_os = "linux"))]
            {
                lock(&self.test)
                    .as_ref()
                    .is_some_and(|test| test.fail_spawn == Some(worker))
            }
            #[cfg(not(all(test, target_os = "linux")))]
            {
                let _ = worker;
                false
            }
        }
        #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
        pub(super) fn initialization_control(&self, worker: usize) {
            #[cfg(all(test, target_os = "linux"))]
            if lock(&self.test)
                .as_ref()
                .is_some_and(|test| test.panic_init == Some(worker))
            {
                panic!("controlled remote initialization panic");
            }
            #[cfg(not(all(test, target_os = "linux")))]
            let _ = worker;
        }
    }
    pub(super) struct ActorGuard {
        state: Arc<State>,
        worker: usize,
    }
    impl ActorGuard {
        pub(super) fn new(state: Arc<State>, worker: usize) -> Self {
            Self { state, worker }
        }
    }
    impl Drop for ActorGuard {
        fn drop(&mut self) {
            if thread::panicking() {
                self.state.seal();
                lock(&self.state.failures)
                    .push(format!("original remote worker {} panicked", self.worker));
                for wake in lock(&self.state.wakes).iter() {
                    wake.unpark();
                }
            } else {
                self.state.control(Point::BeforeExit);
            }
        }
    }
    #[derive(Clone, Debug)]
    pub struct RemoteObservation {
        pub sealed: bool,
        pub started: bool,
        pub workers: usize,
        pub joined: usize,
        pub owned: usize,
        pub queued: usize,
        pub active: usize,
        pub stored: usize,
        pub reconciling: usize,
        pub failures: Vec<String>,
        pub accounting_fault: bool,
    }
    #[derive(Debug)]
    pub enum RemoteCloseStatus {
        Pending {
            attempt: u64,
            observation: Option<RemoteObservation>,
        },
        Blocked {
            attempt: u64,
            observation: Option<RemoteObservation>,
        },
        Quiescent {
            attempt: u64,
            observation: Option<RemoteObservation>,
        },
    }
    pub struct RetainedRemoteStreaming {
        inner: Option<Streaming>,
        mode: Arc<State>,
        workers: usize,
        profile: Option<RemoteMeshProfile>,
        started: bool,
        spawned: usize,
        joined: usize,
        deadline: Option<Instant>,
        attempt: u64,
        ready: VecDeque<Arc<JobOwner>>,
        incoming: Option<Done>,
        failure: Option<String>,
    }
    pub struct ClosedRemoteStreaming {
        inner: Streaming,
        mode: Arc<State>,
        started: bool,
        joined: usize,
        failure: Option<String>,
    }
    impl RetainedRemoteStreaming {
        pub fn prepare(
            context: Arc<WorkerContext>,
            workers: usize,
        ) -> Result<Self, RemotePrepareRefusal> {
            if !(1..=8).contains(&workers) {
                return Err(RemotePrepareRefusal {
                    reason: RemoteRefusal::InvalidWorkers,
                    context,
                    workers,
                });
            }
            let mode = Arc::new(State {
                sender: Mutex::new(None),
                armed: AtomicBool::new(true),
                gate: AtomicU64::new(0),
                context: context.clone(),
                slots: Mutex::new(VecDeque::new()),
                wakes: Mutex::new(Vec::new()),
                failures: Mutex::new(Vec::new()),
                #[cfg(all(
                    test,
                    feature = "native-writer-test-control",
                    debug_assertions,
                    target_os = "linux"
                ))]
                test: Mutex::new(None),
            });
            let inner = match Streaming::construct(
                Arc::new(Provider),
                context,
                0,
                StreamingConstruction::Remote(mode.clone()),
            ) {
                Ok(inner) => inner,
                Err(_) => unreachable!(
                    "inert fixed remote construction cannot spawn or validate a native writer"
                ),
            };
            Ok(Self {
                inner: Some(inner),
                mode,
                workers,
                profile: None,
                started: false,
                spawned: 0,
                joined: 0,
                deadline: None,
                attempt: 0,
                ready: VecDeque::new(),
                incoming: None,
                failure: None,
            })
        }
        pub fn start(&mut self, profile: RemoteMeshProfile) -> Result<(), RemoteRefusal> {
            if self.mode.sealed() {
                return Err(RemoteRefusal::Closed);
            }
            if self.started {
                return Err(RemoteRefusal::State);
            }
            if !(0..=7).contains(&profile.biome_blend) {
                return Err(RemoteRefusal::InvalidProfile);
            }
            self.profile = Some(profile);
            let inner = self.inner.as_mut().unwrap();
            inner.sky_light = profile.sky_light;
            inner.nether_shading = profile.nether_shading;
            inner.biome_zoom = BiomeZoom::new(profile.obfuscated_biome_seed);
            inner.biome_blend = profile.biome_blend;
            inner.smooth_lighting = profile.smooth_lighting;
            inner.cutout_leaves = profile.cutout_leaves;
            self.started = true;
            let sender = lock(&self.mode.sender).as_ref().unwrap().clone();
            let result = Streaming::spawn_pool(
                inner,
                Arc::new(Provider),
                self.mode.context.clone(),
                self.workers,
                sender,
            );
            self.spawned = inner.threads.len();
            if let Err(error) = result {
                self.failure = Some(error);
                self.mode.seal();
                for handle in &inner.threads {
                    handle.thread().unpark();
                }
                return Err(RemoteRefusal::State);
            }
            Ok(())
        }
        pub fn profile(&self) -> Option<RemoteMeshProfile> {
            self.profile
        }
        /// A received resident column with installed server light. This says
        /// nothing about meshes, rendering, native generation or checked close.
        pub fn received_ready(&self, world: &World, at: (i32, i32)) -> bool {
            self.admission().is_ok()
                && self.inner.as_ref().unwrap().lit.contains(&at)
                && world
                    .chunks
                    .get(&at)
                    .is_some_and(|chunk| chunk.lit && chunk.light_loaded)
        }
        /// Conservative latest-stamp scheduling state. A pending snapshot may
        /// already be stale; only the original handoff computes `current`.
        pub fn mesh_pending(&self, section: (i32, i32, i32)) -> bool {
            self.inner.as_ref().unwrap().meshing.contains_key(&section)
        }
        fn admission(&self) -> Result<(), RemoteRefusal> {
            if self.mode.sealed() {
                Err(RemoteRefusal::Closed)
            } else if !self.started {
                Err(RemoteRefusal::NotRunning)
            } else {
                Ok(())
            }
        }
        pub fn mesh_input(
            &self,
            world: &World,
            section: (i32, i32, i32),
        ) -> Result<RemoteInput, RemoteRefusal> {
            self.admission()?;
            if !(0..SECTIONS as i32).contains(&section.1) {
                return Err(RemoteRefusal::InvalidSection);
            }
            let inner = self.inner.as_ref().unwrap();
            let region = Streaming::region(world, section.0, section.2)
                .ok_or(RemoteRefusal::MissingColumn)?;
            Ok(RemoteInput {
                context: self.mode.context.clone(),
                profile: self
                    .profile
                    .expect("started remote owner has its original profile"),
                region,
                kind: Kind::Mesh {
                    x: section.0,
                    sy: section.1,
                    z: section.2,
                    settings: MeshSettings {
                        nether: inner.nether_shading,
                        zoom: inner.biome_zoom,
                        blend_radius: inner.biome_blend,
                        smooth_lighting: inner.smooth_lighting,
                        cutout_leaves: inner.cutout_leaves,
                    },
                },
            })
        }
        pub fn light_input(
            &self,
            world: &World,
            position: (i32, i32),
        ) -> Result<RemoteInput, RemoteRefusal> {
            self.admission()?;
            let region = Streaming::region(world, position.0, position.1)
                .ok_or(RemoteRefusal::MissingColumn)?;
            Ok(RemoteInput {
                context: self.mode.context.clone(),
                profile: self
                    .profile
                    .expect("started remote owner has its original profile"),
                region,
                kind: Kind::Light {
                    x: position.0,
                    z: position.1,
                    sky: self.inner.as_ref().unwrap().sky_light,
                },
            })
        }
        #[allow(clippy::result_large_err)] // Refusal returns the caller's original owned column.
        pub fn try_insert_column(
            &mut self,
            world: &mut World,
            chunk: Chunk,
        ) -> Result<Option<Arc<Chunk>>, (RemoteRefusal, Chunk)> {
            if let Err(reason) = self.admission() {
                return Err((reason, chunk));
            }
            let inner = self.inner.as_mut().unwrap();
            let Some(_queue) = attempt(&inner.shared.queue) else {
                return Err((RemoteRefusal::Busy, chunk));
            };
            let Some(slots) = attempt(&self.mode.slots) else {
                return Err((RemoteRefusal::Busy, chunk));
            };
            if slots.len() == LIMIT {
                return Err((RemoteRefusal::Full, chunk));
            }
            if self.mode.sealed() {
                return Err((RemoteRefusal::Closed, chunk));
            }
            let key = (chunk.x, chunk.z);
            inner.lighting.remove(&key);
            inner.lit.insert(key);
            let previous = world.chunks.insert(key, Arc::new(chunk));
            for dz in -1..=1 {
                for dx in -1..=1 {
                    for sy in 0..SECTIONS as i32 {
                        world.dirty_sections.insert((key.0 + dx, sy, key.1 + dz));
                    }
                }
            }
            Ok(previous)
        }
        pub fn try_forget_column(
            &mut self,
            world: &mut World,
            at: (i32, i32),
        ) -> Result<Option<Arc<Chunk>>, RemoteRefusal> {
            self.admission()?;
            let inner = self.inner.as_mut().unwrap();
            let _queue = attempt(&inner.shared.queue).ok_or(RemoteRefusal::Busy)?;
            let _slots = attempt(&self.mode.slots).ok_or(RemoteRefusal::Busy)?;
            if self.mode.sealed() {
                return Err(RemoteRefusal::Closed);
            }
            inner.lit.remove(&at);
            inner.meshed.remove(&at);
            inner.lighting.remove(&at);
            inner.light_snapshots.remove(&at);
            for sy in 0..SECTIONS as i32 {
                inner.meshing.remove(&(at.0, sy, at.1));
                inner.waiting.remove(&(at.0, sy, at.1));
            }
            world.dirty_sections.take_column(at.0, at.1);
            world.dirty_light.remove(&at);
            // Original JobOwners/receipts/snapshots remain in their existing
            // registry. This transfers only the original resident allocation.
            Ok(world.chunks.remove(&at))
        }
        pub fn try_apply_light(
            &mut self,
            world: &mut World,
            at: (i32, i32),
            layers: &[horizon_protocol::chunk::SectionLight],
        ) -> Result<RemoteLightOutcome, RemoteRefusal> {
            self.admission()?;
            if layers.len() != SECTIONS
                || layers.iter().any(|(sky, block)| {
                    sky.as_ref().is_some_and(|layer| layer.len() != 2048)
                        || block.as_ref().is_some_and(|layer| layer.len() != 2048)
                })
            {
                return Err(RemoteRefusal::InvalidLight);
            }
            let sky = self
                .profile
                .expect("started remote owner has its original profile")
                .sky_light;
            let inner = self.inner.as_mut().unwrap();
            let _queue = attempt(&inner.shared.queue).ok_or(RemoteRefusal::Busy)?;
            let _slots = attempt(&self.mode.slots).ok_or(RemoteRefusal::Busy)?;
            if self.mode.sealed() {
                return Err(RemoteRefusal::Closed);
            }
            let Some(original) = world.chunks.get(&at) else {
                return Ok(RemoteLightOutcome::Missing);
            };
            if original.sections.len() != SECTIONS {
                return Err(RemoteRefusal::InvalidLight);
            }
            // Work on detached original data. Arc::make_mut inside the real
            // algorithm can only modify the replacement's section allocations.
            let mut replacement = (**original).clone();
            let sections = crate::light::update_light_layers(&mut replacement, layers, sky);
            if sections.is_empty() {
                return Ok(RemoteLightOutcome::Unchanged);
            }
            let previous = world.chunks.insert(at, Arc::new(replacement)).unwrap();
            inner.lighting.remove(&at);
            inner.light_snapshots.remove(&at);
            Streaming::light_updated(world, at.0, at.1, &sections);
            Ok(RemoteLightOutcome::Replaced { previous, sections })
        }
        pub fn try_submit(&mut self, input: RemoteInput) -> Result<u64, RemoteRefusedInput> {
            if let Err(reason) = self.admission() {
                return Err(RemoteRefusedInput { reason, input });
            }
            if !Arc::ptr_eq(&input.context, &self.mode.context) {
                return Err(RemoteRefusedInput {
                    reason: RemoteRefusal::ForeignContext,
                    input,
                });
            }
            if self.profile != Some(input.profile) {
                return Err(RemoteRefusedInput {
                    reason: RemoteRefusal::ForeignProfile,
                    input,
                });
            }
            let inner = self.inner.as_mut().unwrap();
            let Some(mut queue) = attempt(&inner.shared.queue) else {
                return Err(RemoteRefusedInput {
                    reason: RemoteRefusal::Busy,
                    input,
                });
            };
            let Some(mut queued) = attempt(&inner.shared.queued_owners) else {
                return Err(RemoteRefusedInput {
                    reason: RemoteRefusal::Busy,
                    input,
                });
            };
            let Some(mut book) = attempt(&inner.shared.ownership) else {
                return Err(RemoteRefusedInput {
                    reason: RemoteRefusal::Busy,
                    input,
                });
            };
            let Some(mut slots) = attempt(&self.mode.slots) else {
                return Err(RemoteRefusedInput {
                    reason: RemoteRefusal::Busy,
                    input,
                });
            };
            if self.mode.sealed() {
                return Err(RemoteRefusedInput {
                    reason: RemoteRefusal::Closed,
                    input,
                });
            }
            if slots.len() == LIMIT {
                return Err(RemoteRefusedInput {
                    reason: RemoteRefusal::Full,
                    input,
                });
            }
            let Some(sequence) = inner.order.checked_add(1) else {
                return Err(RemoteRefusedInput {
                    reason: RemoteRefusal::Exhausted,
                    input,
                });
            };
            let position = input.position();
            let origin = input.origin();
            match input.kind {
                Kind::Light { x, z, .. } => {
                    inner.lighting.insert((x, z), sequence);
                }
                Kind::Mesh { x, sy, z, .. } => {
                    inner.meshing.insert((x, sy, z), sequence);
                }
            }
            let owner = Arc::new(JobOwner {
                sequence,
                origin,
                phase: AtomicU8::new(QUEUED),
                payload: Mutex::new(Payload {
                    input: Some(input),
                    output: None,
                    receipt: None,
                }),
                mode: Arc::downgrade(&self.mode),
            });
            let mut receipt = WorkLease::new_locked(&inner.shared.ownership, &mut book, origin);
            receipt.advance_locked(&mut book, WorkPhase::Queued);
            queued.insert(sequence, receipt);
            queue.push(Queued {
                priority: sequence as i64,
                at: Some(position),
                bias: 0,
                order: sequence,
                job: Job::Remote(owner.clone()),
            });
            slots.push_back(owner);
            inner.order = sequence;
            drop(slots);
            drop(book);
            drop(queued);
            drop(queue);
            for handle in &inner.threads {
                handle.thread().unpark();
            }
            Ok(sequence)
        }
        pub fn try_reconcile(&mut self) -> Result<bool, RemoteRefusal> {
            let inner = self.inner.as_mut().unwrap();
            let Some(mut returned) = attempt(&inner.shared.returned) else {
                return Err(RemoteRefusal::Busy);
            };
            let Some(mut book) = attempt(&inner.shared.ownership) else {
                return Err(RemoteRefusal::Busy);
            };
            if self.incoming.is_none() {
                match inner.results.try_recv() {
                    Ok(done) => self.incoming = Some(done),
                    Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => {
                        return Ok(false);
                    }
                }
            }
            let Some(Done::Remote(job)) = &self.incoming else {
                self.failure = Some("unexpected original remote result kind".into());
                self.mode.seal();
                return Err(RemoteRefusal::State);
            };
            let job = job.clone();
            if !returned
                .front()
                .is_some_and(|(stamp, _)| *stamp == ResultStamp::Remote(job.sequence))
            {
                self.failure = Some("original remote result receipt mismatch".into());
                self.mode.seal();
                return Err(RemoteRefusal::State);
            }
            let Some(mut payload) = attempt(&job.payload) else {
                // Keep the exact channel token for a later try, without
                // popping its native receipt or fabricating an empty census.
                return Err(RemoteRefusal::Busy);
            };
            let mut receipt = returned.pop_front().unwrap().1;
            receipt.advance_locked(&mut book, WorkPhase::Reconciling);
            payload.receipt = Some(receipt);
            job.phase.store(RECONCILING, Ordering::Release);
            drop(payload);
            self.incoming.take();
            self.ready.push_back(job);
            Ok(true)
        }
        pub fn try_take(&mut self, world: &World) -> Option<RemoteHandoff> {
            let inner = self.inner.as_mut().unwrap();
            let job = self.ready.front()?;
            let mut book = attempt(&inner.shared.ownership)?;
            let mut slots = attempt(&self.mode.slots)?;
            let mut payload = attempt(&job.payload)?;
            if payload.input.is_none() || payload.output.is_none() || payload.receipt.is_none() {
                return None;
            }
            let sequence = job.sequence;
            let current = current(inner, world, payload.input.as_ref().unwrap(), job.sequence);
            let input = payload.input.take().unwrap();
            let output = output(payload.output.take().unwrap());
            payload.receipt.take().unwrap().settle_locked(&mut book);
            slots.retain(|owned| owned.sequence != job.sequence);
            match input.kind {
                Kind::Light { x, z, .. } => {
                    if inner.lighting.get(&(x, z)) == Some(&sequence) {
                        inner.lighting.remove(&(x, z));
                    }
                }
                Kind::Mesh { x, sy, z, .. } => {
                    if inner.meshing.get(&(x, sy, z)) == Some(&sequence) {
                        inner.meshing.remove(&(x, sy, z));
                    }
                }
            }
            drop(payload);
            drop(slots);
            drop(book);
            self.ready.pop_front();
            Some(RemoteHandoff {
                sequence,
                input,
                output: Some(output),
                current,
            })
        }
        pub fn try_observe(&self) -> Option<RemoteObservation> {
            observation(
                self.inner.as_ref().unwrap(),
                &self.mode,
                self.started,
                self.joined,
                &self.failure,
            )
        }
        pub fn begin_close(&mut self, deadline: Instant) {
            self.mode.seal();
            if self.deadline.is_none() {
                self.deadline = Some(deadline);
                self.attempt = 1;
            }
            for handle in &self.inner.as_ref().unwrap().threads {
                handle.thread().unpark();
            }
        }
        pub fn retry_close(
            &mut self,
            expected: u64,
            deadline: Instant,
        ) -> Result<u64, RemoteRefusal> {
            let now = Instant::now();
            if self.deadline.is_none_or(|old| now < old)
                || self.inner.as_ref().unwrap().threads.is_empty()
            {
                return Err(RemoteRefusal::State);
            }
            if expected != self.attempt {
                return Err(RemoteRefusal::Attempt);
            }
            if deadline <= now {
                return Err(RemoteRefusal::Deadline);
            }
            self.attempt = self
                .attempt
                .checked_add(1)
                .ok_or(RemoteRefusal::Exhausted)?;
            self.deadline = Some(deadline);
            for handle in &self.inner.as_ref().unwrap().threads {
                handle.thread().unpark();
            }
            Ok(self.attempt)
        }
        pub fn poll_close(&mut self) -> RemoteCloseStatus {
            let inner = self.inner.as_mut().unwrap();
            let mut index = 0;
            while index < inner.threads.len() {
                if inner.threads[index].is_finished() {
                    let outcome = inner.threads.swap_remove(index).join();
                    self.joined += 1;
                    if outcome.is_err() {
                        self.failure.get_or_insert_with(|| {
                            "original remote worker checked join failed".into()
                        });
                    }
                } else {
                    index += 1;
                }
            }
            let observation = self.try_observe();
            if self.deadline.is_some()
                && self.mode.sealed()
                && self.joined == self.spawned
                && self.inner.as_ref().unwrap().threads.is_empty()
            {
                RemoteCloseStatus::Quiescent {
                    attempt: self.attempt,
                    observation,
                }
            } else if self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                RemoteCloseStatus::Blocked {
                    attempt: self.attempt,
                    observation,
                }
            } else {
                RemoteCloseStatus::Pending {
                    attempt: self.attempt,
                    observation,
                }
            }
        }
        #[allow(clippy::result_large_err)] // A refused handoff returns the same opaque original owner.
        pub fn try_closed(mut self) -> Result<ClosedRemoteStreaming, Self> {
            if !matches!(self.poll_close(), RemoteCloseStatus::Quiescent { .. }) {
                return Err(self);
            }
            let inner = self.inner.take().unwrap();
            // The same concrete Streaming and registry transfer together. Its
            // native handles are checked terminal; no inner recovery is exposed.
            let closed = ClosedRemoteStreaming {
                inner,
                mode: self.mode.clone(),
                started: self.started,
                joined: self.joined,
                failure: self.failure.take(),
            };
            self.mode.armed.store(false, Ordering::Release);
            Ok(closed)
        }
    }
    fn current(inner: &Streaming, world: &World, input: &RemoteInput, sequence: u64) -> bool {
        let position = input.position();
        for (z, row) in input.region.iter().enumerate() {
            for (x, original) in row.iter().enumerate() {
                let at = (position.0 + x as i32 - 1, position.1 + z as i32 - 1);
                match (original, world.chunks.get(&at)) {
                    (Some(original), Some(current)) if Arc::ptr_eq(original, current) => {}
                    (None, None) => {}
                    _ => return false,
                }
            }
        }
        match input.kind {
            Kind::Light { .. } => inner.lighting.get(&position) == Some(&sequence),
            Kind::Mesh { sy, .. } => {
                inner.meshing.get(&(position.0, sy, position.1)) == Some(&sequence)
            }
        }
    }
    fn output(done: Done) -> RemoteOutput {
        match done {
            Done::Lit(x, z, sections, sequence) => RemoteOutput::Light {
                position: (x, z),
                sequence,
                sections,
            },
            Done::Meshed(x, sy, z, mesh, sequence) => RemoteOutput::Mesh {
                section: (x, sy, z),
                sequence,
                mesh,
            },
            _ => unreachable!("fixed real remote Light/Mesh result"),
        }
    }
    fn observation(
        inner: &Streaming,
        mode: &State,
        started: bool,
        joined: usize,
        failure: &Option<String>,
    ) -> Option<RemoteObservation> {
        let slots = attempt(&mode.slots)?;
        let failures = attempt(&mode.failures)?;
        let book = attempt(&inner.shared.ownership)?;
        let mut errors = failures.clone();
        errors.extend(failure.iter().cloned());
        let mut phases = [0; 4];
        for slot in slots.iter() {
            let phase = slot.phase.load(Ordering::Acquire);
            *phases.get_mut(phase as usize)? += 1;
        }
        Some(RemoteObservation {
            sealed: mode.sealed(),
            started,
            workers: inner.threads.len() + joined,
            joined,
            owned: slots.len(),
            queued: phases[QUEUED as usize],
            active: phases[ACTIVE as usize],
            stored: phases[STORED as usize],
            reconciling: phases[RECONCILING as usize],
            failures: errors,
            accounting_fault: book.accounting_fault
                || book.receipt_mismatch
                || book.result_disconnected,
        })
    }
    impl ClosedRemoteStreaming {
        pub fn try_observe(&self) -> Option<RemoteObservation> {
            observation(
                &self.inner,
                &self.mode,
                self.started,
                self.joined,
                &self.failure,
            )
        }
        pub fn try_take(&mut self) -> Option<RemoteHandoff> {
            let mut queue = attempt(&self.inner.shared.queue)?;
            let mut queued = attempt(&self.inner.shared.queued_owners)?;
            let mut returned = attempt(&self.inner.shared.returned)?;
            let mut book = attempt(&self.inner.shared.ownership)?;
            let mut slots = attempt(&self.mode.slots)?;
            let job = slots.front()?;
            let sequence = job.sequence;
            let mut payload = attempt(&job.payload)?;
            let input = payload.input.take()?;
            let result = payload.output.take().map(output);
            if let Some(receipt) = payload
                .receipt
                .take()
                .or_else(|| queued.remove(&job.sequence))
            {
                receipt.settle_locked(&mut book);
            }
            if let Some(index) = returned
                .iter()
                .position(|(stamp, _)| *stamp == ResultStamp::Remote(job.sequence))
            {
                returned.remove(index).unwrap().1.settle_locked(&mut book);
            }
            queue.retain(|queued| queued.order != job.sequence);
            drop(payload);
            slots.pop_front();
            Some(RemoteHandoff {
                sequence,
                input,
                output: result,
                current: false,
            })
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Point {
        BeforeClaim,
        BeforeWork,
        AfterPublish,
        BeforePark,
        BeforeExit,
    }
    #[cfg(all(
        test,
        feature = "native-writer-test-control",
        debug_assertions,
        target_os = "linux"
    ))]
    struct Control {
        point: Point,
        occurrence: usize,
        visits: std::sync::atomic::AtomicUsize,
        entered: AtomicBool,
        released: AtomicBool,
        panic: bool,
        fail_spawn: Option<usize>,
        panic_init: Option<usize>,
    }
    #[cfg(all(
        test,
        feature = "native-writer-test-control",
        debug_assertions,
        target_os = "linux"
    ))]
    impl Control {
        fn enter(&self, point: Point) {
            if self.point != point
                || self.visits.fetch_add(1, Ordering::AcqRel) + 1 != self.occurrence
            {
                return;
            }
            self.entered.store(true, Ordering::Release);
            // Do not consume the production Thread::unpark token while testing
            // the exact empty-queue-to-park window.
            while !self.released.load(Ordering::Acquire) {
                thread::yield_now();
            }
            assert!(!self.panic, "controlled original remote work panic");
        }
    }

    #[cfg(all(
        test,
        feature = "native-writer-test-control",
        debug_assertions,
        target_os = "linux"
    ))]
    mod tests {
        use super::*;
        use std::time::Duration;
        fn until() -> Instant {
            Instant::now() + Duration::from_secs(15)
        }
        fn profile() -> RemoteMeshProfile {
            RemoteMeshProfile {
                sky_light: true,
                nether_shading: false,
                obfuscated_biome_seed: 0,
                biome_blend: 2,
                smooth_lighting: true,
                cutout_leaves: true,
            }
        }
        fn fixture() -> (Arc<WorkerContext>, World) {
            let context = super::super::native_work_tests::context();
            let mut world = World::new(
                context
                    .assets
                    .registry
                    .default_state("minecraft:air")
                    .unwrap(),
                0,
            );
            let stone = context
                .assets
                .registry
                .default_state("minecraft:stone")
                .unwrap();
            for z in -1..=1 {
                for x in -1..=1 {
                    let mut chunk = Chunk::empty(x, z);
                    if (x, z) == (0, 0) {
                        chunk.set(1, 0, 1, stone.0);
                    }
                    world.chunks.insert((x, z), Arc::new(chunk));
                }
            }
            (context, world)
        }
        fn control(point: Point, occurrence: usize) -> Arc<Control> {
            Arc::new(Control {
                point,
                occurrence,
                visits: std::sync::atomic::AtomicUsize::new(0),
                entered: AtomicBool::new(false),
                released: AtomicBool::new(false),
                panic: false,
                fail_spawn: None,
                panic_init: None,
            })
        }
        fn controlled(
            context: Arc<WorkerContext>,
            workers: usize,
            control: Arc<Control>,
        ) -> RetainedRemoteStreaming {
            let owner = RetainedRemoteStreaming::prepare(context, workers).unwrap();
            *lock(&owner.mode.test) = Some(control);
            owner
        }
        fn entered(control: &Control) {
            let deadline = until();
            while !control.entered.load(Ordering::Acquire) {
                assert!(
                    Instant::now() < deadline,
                    "actual original remote worker gate missing"
                );
                thread::yield_now();
            }
        }
        fn submit(owner: &mut RetainedRemoteStreaming, mut input: RemoteInput) -> u64 {
            let deadline = until();
            loop {
                match owner.try_submit(input) {
                    Ok(sequence) => return sequence,
                    Err(refused) => {
                        assert_eq!(refused.reason, RemoteRefusal::Busy);
                        input = refused.input;
                        assert!(Instant::now() < deadline);
                        thread::yield_now();
                    }
                }
            }
        }
        fn reconcile(owner: &mut RetainedRemoteStreaming) {
            let deadline = until();
            loop {
                match owner.try_reconcile() {
                    Ok(true) => return,
                    Ok(false) | Err(RemoteRefusal::Busy) => {
                        assert!(Instant::now() < deadline, "actual remote result missing");
                        thread::yield_now();
                    }
                    other => panic!("remote reconciliation failed: {other:?}"),
                }
            }
        }
        fn insert_received(
            owner: &mut RetainedRemoteStreaming,
            world: &mut World,
            mut incoming: Chunk,
        ) -> Option<Arc<Chunk>> {
            let deadline = until();
            loop {
                match owner.try_insert_column(world, incoming) {
                    Ok(previous) => return previous,
                    Err((RemoteRefusal::Busy, original)) => {
                        incoming = original;
                        assert!(Instant::now() < deadline, "received insertion stayed Busy");
                        thread::yield_now();
                    }
                    Err((reason, _)) => panic!("received insertion refused: {reason:?}"),
                }
            }
        }
        fn take_handoff(owner: &mut RetainedRemoteStreaming, world: &World) -> RemoteHandoff {
            let deadline = until();
            loop {
                if let Some(handoff) = owner.try_take(world) {
                    return handoff;
                }
                assert!(
                    Instant::now() < deadline,
                    "original reconciled remote handoff missing"
                );
                thread::yield_now();
            }
        }
        fn close(mut owner: RetainedRemoteStreaming) -> ClosedRemoteStreaming {
            owner.begin_close(until());
            let deadline = until();
            loop {
                if matches!(owner.poll_close(), RemoteCloseStatus::Quiescent { .. }) {
                    return match owner.try_closed() {
                        Ok(closed) => closed,
                        Err(_) => panic!("actual remote handoff refused"),
                    };
                }
                assert!(
                    Instant::now() < deadline,
                    "actual remote worker join missing"
                );
                thread::yield_now();
            }
        }
        #[test]
        #[ignore = "owned prepared resources: actual received-column geometry and light"]
        fn remote_stream_real_mesh_and_light() {
            let (context, world) = fixture();
            let mut owner = RetainedRemoteStreaming::prepare(context.clone(), 1).unwrap();
            let selected = RemoteMeshProfile {
                sky_light: false,
                nether_shading: true,
                obfuscated_biome_seed: 0x1234_5678,
                biome_blend: 0,
                smooth_lighting: false,
                cutout_leaves: false,
            };
            owner.start(selected).unwrap();
            assert_eq!(owner.profile(), Some(selected));
            assert_eq!(owner.start(profile()), Err(RemoteRefusal::State));
            assert_eq!(owner.profile(), Some(selected));
            // Sharing the actual resource allocation does not permit a foreign
            // profile to bypass the receiver's immutable worker settings.
            let mut other = RetainedRemoteStreaming::prepare(context.clone(), 1).unwrap();
            other.start(profile()).unwrap();
            for mesh in [true, false] {
                let input = if mesh {
                    owner.mesh_input(&world, (0, 4, 0)).unwrap()
                } else {
                    owner.light_input(&world, (0, 0)).unwrap()
                };
                let original = input.region[1][1].as_ref().unwrap().clone();
                let refused = other.try_submit(input).unwrap_err();
                assert_eq!(refused.reason, RemoteRefusal::ForeignProfile);
                assert_eq!(refused.input.profile, selected);
                assert!(Arc::ptr_eq(&refused.input.context, &context));
                assert!(Arc::ptr_eq(
                    &original,
                    refused.input.region[1][1].as_ref().unwrap()
                ));
                assert_eq!(other.try_observe().unwrap().owned, 0);
            }
            assert_eq!(close(other).try_observe().unwrap().joined, 1);
            let mesh = owner.mesh_input(&world, (0, 4, 0)).unwrap();
            let light = owner.light_input(&world, (0, 0)).unwrap();
            let sequences = [submit(&mut owner, mesh), submit(&mut owner, light)];
            for (mesh_expected, sequence) in [true, false].into_iter().zip(sequences) {
                reconcile(&mut owner);
                let handoff = take_handoff(&mut owner, &world);
                assert_eq!(handoff.sequence, sequence);
                assert!(handoff.current && Arc::ptr_eq(&handoff.input.context, &context));
                match handoff.output.unwrap() {
                    RemoteOutput::Mesh { section, mesh, .. } => {
                        assert!(mesh_expected);
                        assert_eq!(section, (0, 4, 0));
                        assert_eq!(mesh.solid.len(), 24);
                        assert!(mesh.translucent.is_empty());
                        assert!(mesh.solid.iter().all(|v| [1.0, 2.0].contains(&v.pos[0])
                            && [0.0, 1.0].contains(&v.pos[1])
                            && [1.0, 2.0].contains(&v.pos[2])));
                        let horizontal: Vec<_> = mesh
                            .solid
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .filter(|quad| {
                                quad.iter().all(|v| v.pos[1] == 0.0)
                                    || quad.iter().all(|v| v.pos[1] == 1.0)
                            })
                            .collect();
                        assert_eq!(horizontal.len(), 2);
                        assert!(
                            horizontal.iter().all(|quad| quad
                                .iter()
                                .all(|v| v.color & 0x00ff_ffff == 0x00e5_e5e5))
                        );
                        assert!(!owner.mesh_pending((0, 4, 0)));
                    }
                    RemoteOutput::Light {
                        position, sections, ..
                    } => {
                        assert!(!mesh_expected);
                        assert_eq!(position, (0, 0));
                        assert_eq!(sections.len(), SECTIONS);
                        assert_eq!(sections[SECTIONS - 1].get(0), 0);
                        assert_eq!(sections[4].get(crate::chunk::index(8, 0, 8)), 0);
                        assert_eq!(sections[4].get(crate::chunk::index(1, 0, 1)), 0);
                        assert!(!owner.inner.as_ref().unwrap().lighting.contains_key(&(0, 0)));
                    }
                }
            }
            // A newer received allocation invalidates a real stored mesh.
            let stale = owner.mesh_input(&world, (0, 4, 0)).unwrap();
            submit(&mut owner, stale);
            reconcile(&mut owner);
            let mut replacement = world;
            assert!(insert_received(&mut owner, &mut replacement, Chunk::empty(0, 0)).is_some());
            assert!(!take_handoff(&mut owner, &replacement).current);
            // Both real algorithms also retain a stale result when only a
            // neighboring snapshot changes; the center allocation is unchanged.
            for mesh in [true, false] {
                let center = replacement.chunks.get(&(0, 0)).unwrap().clone();
                let input = if mesh {
                    owner.mesh_input(&replacement, (0, 4, 0)).unwrap()
                } else {
                    owner.light_input(&replacement, (0, 0)).unwrap()
                };
                let sequence = submit(&mut owner, input);
                reconcile(&mut owner);
                assert!(
                    insert_received(&mut owner, &mut replacement, Chunk::empty(1, 0)).is_some()
                );
                assert!(Arc::ptr_eq(
                    &center,
                    replacement.chunks.get(&(0, 0)).unwrap()
                ));
                let handoff = take_handoff(&mut owner, &replacement);
                assert_eq!(handoff.sequence, sequence);
                assert!(!handoff.current);
                assert_eq!(
                    matches!(handoff.output.as_ref(), Some(RemoteOutput::Mesh { .. })),
                    mesh
                );
                assert!(handoff.output.is_some());
            }
            let observation = close(owner).try_observe().unwrap();
            assert_eq!(observation.owned, 0);
            assert_eq!(observation.joined, 1);
            assert!(observation.failures.is_empty());
        }
        #[test]
        #[ignore = "owned prepared resources: inert and partial native startup"]
        fn remote_stream_prepare_and_partial_start() {
            let (context, _) = fixture();
            for workers in [0, 9] {
                let refused = match RetainedRemoteStreaming::prepare(context.clone(), workers) {
                    Err(refused) => refused,
                    Ok(_) => panic!("invalid remote worker count accepted"),
                };
                assert!(Arc::ptr_eq(&refused.context, &context));
                assert_eq!(refused.workers, workers);
            }
            let mut owner = RetainedRemoteStreaming::prepare(context.clone(), 2).unwrap();
            assert!(!owner.try_observe().unwrap().started);
            assert_eq!(owner.try_observe().unwrap().workers, 0);
            assert_eq!(owner.profile(), None);
            for blend in [-1, 8] {
                let invalid = RemoteMeshProfile {
                    biome_blend: blend,
                    ..profile()
                };
                assert_eq!(owner.start(invalid), Err(RemoteRefusal::InvalidProfile));
                assert_eq!(owner.profile(), None);
                assert!(!owner.try_observe().unwrap().started);
                assert_eq!(owner.try_observe().unwrap().workers, 0);
            }
            assert!(!close(owner).try_observe().unwrap().started);
            let mut failure = control(Point::BeforeWork, 1);
            Arc::get_mut(&mut failure).unwrap().fail_spawn = Some(1);
            let mut owner = controlled(context.clone(), 2, failure);
            let selected = RemoteMeshProfile {
                sky_light: false,
                obfuscated_biome_seed: 77,
                ..profile()
            };
            assert_eq!(owner.start(selected), Err(RemoteRefusal::State));
            assert_eq!(owner.profile(), Some(selected));
            assert_eq!(owner.start(profile()), Err(RemoteRefusal::Closed));
            assert_eq!(owner.profile(), Some(selected));
            let observation = close(owner).try_observe().unwrap();
            assert_eq!((observation.workers, observation.joined), (1, 1));
            assert!(!observation.failures.is_empty());
            let mut failure = control(Point::BeforeWork, 1);
            Arc::get_mut(&mut failure).unwrap().panic_init = Some(0);
            let mut owner = controlled(context, 1, failure);
            owner.start(profile()).unwrap();
            let observation = close(owner).try_observe().unwrap();
            assert_eq!(observation.joined, 1);
            assert!(!observation.failures.is_empty());
        }
        #[test]
        #[ignore = "owned prepared resources: all64 mixed-phase slots and original refusal"]
        fn remote_stream_capacity_and_handoff() {
            let (context, mut world) = fixture();
            let gate = control(Point::BeforeWork, 3);
            let mut owner = controlled(context, 1, gate.clone());
            owner.start(profile()).unwrap();
            let first = owner.mesh_input(&world, (0, 4, 0)).unwrap();
            let first_sequence = submit(&mut owner, first);
            reconcile(&mut owner);
            let mut expected_sequences = Vec::new();
            for index in 1..64 {
                let input = if index % 2 == 0 {
                    owner.mesh_input(&world, (0, 4, 0)).unwrap()
                } else {
                    owner.light_input(&world, (0, 0)).unwrap()
                };
                expected_sequences.push(submit(&mut owner, input));
            }
            entered(&gate);
            let census = owner.try_observe().unwrap();
            assert_eq!(
                (
                    census.owned,
                    census.reconciling,
                    census.stored,
                    census.active,
                    census.queued
                ),
                (64, 1, 1, 1, 61)
            );
            assert_eq!(
                census.queued + census.active + census.stored + census.reconciling,
                census.owned
            );
            let extra = owner.mesh_input(&world, (0, 4, 0)).unwrap();
            let original = extra.region[1][1].as_ref().unwrap().clone();
            let refused = owner.try_submit(extra).unwrap_err();
            assert_eq!(refused.reason, RemoteRefusal::Full);
            assert!(Arc::ptr_eq(
                &original,
                refused.input.region[1][1].as_ref().unwrap()
            ));
            let membership = world.chunks.membership_revision();
            let refused_column = owner.try_insert_column(&mut world, Chunk::empty(5, 5));
            assert!(matches!(refused_column, Err((RemoteRefusal::Full, _))));
            assert_eq!(world.chunks.membership_revision(), membership);
            let newest_mesh = owner
                .inner
                .as_ref()
                .unwrap()
                .meshing
                .get(&(0, 4, 0))
                .copied();
            let first = owner.try_take(&world).expect("explicit first handoff");
            assert_eq!(first.sequence, first_sequence);
            assert!(!first.current);
            assert!(owner.mesh_pending((0, 4, 0)));
            assert_eq!(
                owner
                    .inner
                    .as_ref()
                    .unwrap()
                    .meshing
                    .get(&(0, 4, 0))
                    .copied(),
                newest_mesh
            );
            assert_eq!(owner.try_observe().unwrap().owned, 63);
            expected_sequences.push(submit(&mut owner, refused.input));
            assert_eq!(owner.try_observe().unwrap().owned, 64);
            owner.begin_close(Instant::now());
            gate.released.store(true, Ordering::Release);
            let mut closed = close(owner);
            assert_eq!(closed.try_observe().unwrap().owned, 64);
            let mut handed = 0;
            let mut outputs = 0;
            let mut actual_sequences = Vec::new();
            while let Some(handoff) = closed.try_take() {
                handed += 1;
                outputs += usize::from(handoff.output.is_some());
                actual_sequences.push(handoff.sequence);
            }
            assert_eq!(actual_sequences, expected_sequences);
            assert_eq!((handed, outputs), (64, 2));
            assert_eq!(closed.try_observe().unwrap().owned, 0);
        }
        #[test]
        #[ignore = "owned prepared resources: checked received light/replacement/forget and original allocation custody"]
        fn remote_stream_received_world_operations() {
            fn apply_received_light(
                owner: &mut RetainedRemoteStreaming,
                world: &mut World,
                at: (i32, i32),
                layers: &[horizon_protocol::chunk::SectionLight],
            ) -> RemoteLightOutcome {
                let deadline = until();
                loop {
                    match owner.try_apply_light(world, at, layers) {
                        Ok(outcome) => return outcome,
                        Err(RemoteRefusal::Busy) => {
                            assert!(Instant::now() < deadline, "received light stayed Busy");
                            thread::yield_now();
                        }
                        Err(reason) => panic!("received light refused: {reason:?}"),
                    }
                }
            }
            fn forget_received(
                owner: &mut RetainedRemoteStreaming,
                world: &mut World,
                at: (i32, i32),
            ) -> Option<Arc<Chunk>> {
                let deadline = until();
                loop {
                    match owner.try_forget_column(world, at) {
                        Ok(previous) => return previous,
                        Err(RemoteRefusal::Busy) => {
                            assert!(Instant::now() < deadline, "received forget stayed Busy");
                            thread::yield_now();
                        }
                        Err(reason) => panic!("received forget refused: {reason:?}"),
                    }
                }
            }
            let (context, mut world) = fixture();
            let at = (0, 0);
            let section = (0, 4, 0);
            {
                let initial = Arc::make_mut(world.chunks.get_mut(&at).unwrap());
                initial.lit = true;
                initial.light_loaded = true;
            }
            let gate = control(Point::BeforeWork, 1);
            let mut owner = controlled(context, 1, gate.clone());
            let empty: Vec<horizon_protocol::chunk::SectionLight> = vec![(None, None); SECTIONS];
            let initial = world.chunks.get(&at).unwrap().clone();
            assert!(matches!(
                owner.try_forget_column(&mut world, at),
                Err(RemoteRefusal::NotRunning)
            ));
            assert!(matches!(
                owner.try_apply_light(&mut world, at, &empty),
                Err(RemoteRefusal::NotRunning)
            ));
            assert!(Arc::ptr_eq(&initial, world.chunks.get(&at).unwrap()));
            assert!(!owner.received_ready(&world, at));
            owner.start(profile()).unwrap();
            assert!(
                !owner.received_ready(&world, at),
                "residency alone is not received tracking"
            );
            let mut retired = Vec::new();
            for (lit, loaded) in [(false, true), (true, false), (true, true)] {
                let before = world.chunks.get(&at).unwrap().clone();
                let mut incoming = (*before).clone();
                incoming.lit = lit;
                incoming.light_loaded = loaded;
                let previous = insert_received(&mut owner, &mut world, incoming)
                    .expect("received replacement returns original allocation");
                assert!(Arc::ptr_eq(&before, &previous));
                assert_eq!(owner.received_ready(&world, at), lit && loaded);
                retired.push(previous);
            }
            world.dirty_sections.clear();
            let before = world.chunks.get(&at).unwrap().clone();
            assert!(matches!(
                apply_received_light(&mut owner, &mut world, (99, 99), &empty),
                RemoteLightOutcome::Missing
            ));
            assert!(matches!(
                apply_received_light(&mut owner, &mut world, at, &empty),
                RemoteLightOutcome::Unchanged
            ));
            assert!(Arc::ptr_eq(&before, world.chunks.get(&at).unwrap()));
            assert!(world.dirty_sections.is_empty());
            let input = owner.mesh_input(&world, section).unwrap();
            let mesh_sequence = submit(&mut owner, input);
            entered(&gate); // Original worker holds its actual input payload.
            assert!(owner.mesh_pending(section));
            let shared = owner.inner.as_ref().unwrap().shared.clone();
            let held = lock(&shared.queue);
            assert!(matches!(
                owner.try_forget_column(&mut world, at),
                Err(RemoteRefusal::Busy)
            ));
            assert!(matches!(
                owner.try_apply_light(&mut world, at, &empty),
                Err(RemoteRefusal::Busy)
            ));
            let incoming = Chunk::empty(9, 9);
            let incoming_section = incoming.sections[0].clone();
            let incoming = match owner.try_insert_column(&mut world, incoming) {
                Err((RemoteRefusal::Busy, original)) => original,
                _ => panic!("busy received insertion did not return its original input"),
            };
            assert!(Arc::ptr_eq(&incoming_section, &incoming.sections[0]));
            drop(held);
            assert!(Arc::ptr_eq(&before, world.chunks.get(&at).unwrap()));
            assert!(world.dirty_sections.is_empty() && owner.received_ready(&world, at));
            assert_eq!(owner.try_observe().unwrap().owned, 1);
            let too_many = vec![(None, None); SECTIONS + 1];
            let mut bad_sky = empty.clone();
            bad_sky[0].0 = Some(vec![0; 2047]);
            let mut bad_block = empty.clone();
            bad_block[0].1 = Some(vec![0; 2049]);
            for invalid in [
                &empty[..SECTIONS - 1],
                too_many.as_slice(),
                bad_sky.as_slice(),
                bad_block.as_slice(),
            ] {
                assert!(matches!(
                    owner.try_apply_light(&mut world, at, invalid),
                    Err(RemoteRefusal::InvalidLight)
                ));
                assert!(Arc::ptr_eq(&before, world.chunks.get(&at).unwrap()));
                assert!(world.dirty_sections.is_empty() && owner.mesh_pending(section));
                assert_eq!(owner.try_observe().unwrap().owned, 1);
            }
            let mut layers = empty.clone();
            layers[4] = (Some(vec![0xaa; 2048]), Some(vec![0x33; 2048]));
            let (previous, sections) =
                match apply_received_light(&mut owner, &mut world, at, &layers) {
                    RemoteLightOutcome::Replaced { previous, sections } => (previous, sections),
                    _ => panic!("real changed server light did not replace its original column"),
                };
            assert_eq!(sections, vec![4]);
            assert!(Arc::ptr_eq(&before, &previous));
            assert_eq!(previous.sections[4].light.get(0), 0xf0);
            assert_eq!(
                world.chunks.get(&at).unwrap().sections[4].light.get(0),
                0xa3
            );
            assert_eq!(world.dirty_sections.len(), 27);
            for z in -1..=1 {
                for x in -1..=1 {
                    for sy in 3..=5 {
                        assert!(world.dirty_sections.contains(&(x, sy, z)));
                    }
                }
            }
            retired.push(previous);
            assert!(owner.received_ready(&world, at) && owner.mesh_pending(section));
            let installed = world.chunks.get(&at).unwrap().clone();
            world.dirty_sections.clear();
            assert!(matches!(
                apply_received_light(&mut owner, &mut world, at, &layers),
                RemoteLightOutcome::Unchanged
            ));
            assert!(Arc::ptr_eq(&installed, world.chunks.get(&at).unwrap()));
            assert!(world.dirty_sections.is_empty());
            let first_light = owner.light_input(&world, at).unwrap();
            let first_light_sequence = submit(&mut owner, first_light);
            let newer_light = owner.light_input(&world, at).unwrap();
            let newer_light_sequence = submit(&mut owner, newer_light);
            gate.released.store(true, Ordering::Release);
            reconcile(&mut owner);
            let late_mesh = take_handoff(&mut owner, &world);
            assert_eq!(late_mesh.sequence, mesh_sequence);
            assert!(
                !late_mesh.current && matches!(late_mesh.output, Some(RemoteOutput::Mesh { .. }))
            );
            assert!(Arc::ptr_eq(
                &before,
                late_mesh.input.region[1][1].as_ref().unwrap()
            ));
            assert!(!owner.mesh_pending(section));
            reconcile(&mut owner);
            let old_light = take_handoff(&mut owner, &world);
            assert_eq!(old_light.sequence, first_light_sequence);
            assert!(!old_light.current);
            assert_eq!(
                owner.inner.as_ref().unwrap().lighting.get(&at),
                Some(&newer_light_sequence)
            );
            reconcile(&mut owner);
            let latest_light = take_handoff(&mut owner, &world);
            assert_eq!(latest_light.sequence, newer_light_sequence);
            assert!(latest_light.current);
            assert!(!owner.inner.as_ref().unwrap().lighting.contains_key(&at));
            let input = owner.mesh_input(&world, section).unwrap();
            let forget_sequence = submit(&mut owner, input);
            reconcile(&mut owner);
            world.dirty_sections.insert(section);
            world.dirty_light.insert(at);
            let forgotten = forget_received(&mut owner, &mut world, at).unwrap();
            assert!(Arc::ptr_eq(&installed, &forgotten));
            assert!(!owner.received_ready(&world, at) && !owner.mesh_pending(section));
            assert!(!world.dirty_sections.contains(&section) && !world.dirty_light.contains(&at));
            assert_eq!(owner.try_observe().unwrap().owned, 1);
            assert!(insert_received(&mut owner, &mut world, (*forgotten).clone()).is_none());
            assert!(owner.received_ready(&world, at));
            assert!(!Arc::ptr_eq(&forgotten, world.chunks.get(&at).unwrap()));
            let late = take_handoff(&mut owner, &world);
            assert_eq!(late.sequence, forget_sequence);
            assert!(!late.current && late.output.is_some());
            assert!(Arc::ptr_eq(
                &forgotten,
                late.input.region[1][1].as_ref().unwrap()
            ));
            retired.push(forgotten);
            let current = world.chunks.get(&at).unwrap().clone();
            world.dirty_sections.clear();
            owner.begin_close(until());
            let incoming = match owner.try_insert_column(&mut world, incoming) {
                Err((RemoteRefusal::Closed, original)) => original,
                _ => panic!("closed insertion did not preserve original refused input"),
            };
            assert!(Arc::ptr_eq(&incoming_section, &incoming.sections[0]));
            assert!(matches!(
                owner.try_apply_light(&mut world, at, &layers),
                Err(RemoteRefusal::Closed)
            ));
            assert!(matches!(
                owner.try_forget_column(&mut world, at),
                Err(RemoteRefusal::Closed)
            ));
            assert!(Arc::ptr_eq(&current, world.chunks.get(&at).unwrap()));
            assert!(world.dirty_sections.is_empty() && !owner.received_ready(&world, at));
            assert_eq!(owner.profile(), Some(profile()));
            let observation = close(owner).try_observe().unwrap();
            assert_eq!((observation.owned, observation.joined), (0, 1));
            assert!(observation.failures.is_empty());
            drop(incoming);
            drop(retired);
        }
        #[test]
        #[ignore = "owned prepared resources: actual late work and deterministic fence/wake"]
        fn remote_stream_close_late_and_claim_races() {
            let (context, world) = fixture();
            for point in [
                Point::BeforeClaim,
                Point::BeforePark,
                Point::BeforeWork,
                Point::BeforeExit,
            ] {
                let gate = control(point, 1);
                let mut owner = controlled(context.clone(), 1, gate.clone());
                owner.start(profile()).unwrap();
                let mut spare = owner.mesh_input(&world, (0, 4, 0)).unwrap();
                let original = spare.region[1][1].as_ref().unwrap().clone();
                if point != Point::BeforePark && point != Point::BeforeExit {
                    let input = owner.mesh_input(&world, (0, 4, 0)).unwrap();
                    submit(&mut owner, input);
                }
                if point == Point::BeforeExit {
                    owner.begin_close(Instant::now());
                }
                entered(&gate);
                if point == Point::BeforeClaim {
                    let refused = owner.try_submit(spare).unwrap_err();
                    assert_eq!(refused.reason, RemoteRefusal::Busy);
                    assert!(Arc::ptr_eq(
                        &original,
                        refused.input.region[1][1].as_ref().unwrap()
                    ));
                    spare = refused.input;
                }
                owner.begin_close(Instant::now());
                owner.begin_close(until());
                let refused = owner.try_submit(spare).unwrap_err();
                assert_eq!(refused.reason, RemoteRefusal::Closed);
                assert!(Arc::ptr_eq(
                    &original,
                    refused.input.region[1][1].as_ref().unwrap()
                ));
                match owner.poll_close() {
                    RemoteCloseStatus::Blocked {
                        attempt: 1,
                        observation,
                    } => {
                        if point == Point::BeforeClaim {
                            assert!(observation.is_none());
                        }
                    }
                    _ => panic!("expired original remote close is not Blocked"),
                }
                assert_eq!(owner.retry_close(0, until()), Err(RemoteRefusal::Attempt));
                assert_eq!(owner.retry_close(1, until()), Ok(2));
                gate.released.store(true, Ordering::Release);
                let mut closed = close(owner);
                assert_eq!(closed.try_observe().unwrap().joined, 1);
                if matches!(point, Point::BeforeClaim | Point::BeforeWork) {
                    let handoff = closed
                        .try_take()
                        .expect("original queued or active input must return");
                    assert_eq!(handoff.sequence, 1);
                    assert_eq!(handoff.output.is_some(), point == Point::BeforeWork);
                }
                assert!(closed.try_take().is_none());
            }
        }
        #[test]
        #[ignore = "owned prepared resources: actual worker unwind keeps inputs and stored outputs"]
        fn remote_stream_panics_preserve_originals() {
            let (context, world) = fixture();
            for point in [Point::BeforeWork, Point::AfterPublish] {
                let mut gate = control(point, 2);
                Arc::get_mut(&mut gate).unwrap().panic = true;
                let mut owner = controlled(context.clone(), 1, gate.clone());
                owner.start(profile()).unwrap();
                let mut sequences = Vec::new();
                for _ in 0..3 {
                    let input = owner.mesh_input(&world, (0, 4, 0)).unwrap();
                    sequences.push(submit(&mut owner, input));
                }
                entered(&gate);
                owner.begin_close(until());
                gate.released.store(true, Ordering::Release);
                let mut closed = close(owner);
                let census = closed.try_observe().unwrap();
                assert_eq!(census.owned, 3);
                assert!(!census.failures.is_empty());
                assert_eq!(census.joined, 1);
                let mut outputs = 0;
                let mut inputs = 0;
                let mut actual_sequences = Vec::new();
                while let Some(handoff) = closed.try_take() {
                    inputs += 1;
                    outputs += usize::from(handoff.output.is_some());
                    actual_sequences.push(handoff.sequence);
                    assert!(Arc::ptr_eq(&handoff.input.context, &context));
                }
                assert_eq!(actual_sequences, sequences);
                assert_eq!(inputs, 3);
                assert_eq!(outputs, if point == Point::BeforeWork { 1 } else { 2 });
            }
        }
        #[test]
        #[ignore = "owned prepared resources: intrinsic mode excludes native generation/storage"]
        fn remote_stream_intrinsic_profile_refusals() {
            let (context, mut world) = fixture();
            let mut owner = RetainedRemoteStreaming::prepare(context, 1).unwrap();
            owner.start(profile()).unwrap();
            let inner = owner.inner.as_mut().unwrap();
            inner.remote = false;
            inner.forced.insert((100, 100));
            assert!(matches!(
                inner.try_submit(Some((8, 8)), 0, Job::Generate(8, 8)),
                Err(Job::Generate(8, 8))
            ));
            let submitted = inner.order;
            inner.produce_terrain(&world);
            inner.schedule_generation(&world, 1);
            assert_eq!(inner.order, submitted);
            assert!(inner.update(&mut world).0.is_empty());
            assert!(inner.save(&mut world, false).is_err());
            assert!(inner.poll_save(&mut world).is_err());
            assert!(
                inner
                    .native_write_fence_until(&mut world, until(), || panic!(
                        "remote mode invoked native callback"
                    ))
                    .is_err()
            );
            assert!(inner.begin_retained_close(Some(&world), until()).is_err());
            assert!(inner.take_sealed().is_none());
            let observation = close(owner).try_observe().unwrap();
            assert_eq!(observation.owned, 0);
            assert!(observation.failures.is_empty());
        }
        #[test]
        #[ignore = "isolated fatal child: owned resources and HORIZON_RETAINED_REMOTE_STREAM_FATAL=1"]
        fn remote_stream_unresolved_native_owner_aborts() {
            assert_eq!(
                std::env::var("HORIZON_RETAINED_REMOTE_STREAM_FATAL").as_deref(),
                Ok("1")
            );
            let (context, world) = fixture();
            let gate = control(Point::BeforeWork, 1);
            let mut owner = controlled(context, 1, gate.clone());
            owner.start(profile()).unwrap();
            let input = owner.mesh_input(&world, (0, 4, 0)).unwrap();
            submit(&mut owner, input);
            entered(&gate);
            assert_eq!(owner.try_observe().unwrap().active, 1);
            println!("S1RW1_REMOTE_WORKER_HELD");
            drop(owner);
            panic!("unresolved original remote owner drop returned");
        }
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

    pub(super) fn context() -> Arc<WorkerContext> {
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

        #[cfg(feature = "paired-packages")]
        #[test]
        #[ignore = "owned prepared resources: actual current native fence observation"]
        fn native_write_fence_current_observes_without_servicing() {
            let (mut streaming, probe, workers, events, mut init) = fixture(1, false, false, false);
            workers.recv_timeout(WAIT).unwrap();
            init.release();
            initialized(&streaming, 1);
            let before = format!("{:?}", streaming.observe_native_work());
            assert!(streaming.native_write_fence_current().unwrap());
            assert_eq!(format!("{:?}", streaming.observe_native_work()), before);
            let mut held = gate(&probe, NativeWorkOrigin::Generate);
            streaming.generating.insert((0, 0));
            streaming.submit(None, 0, Job::Generate(0, 0));
            assert_eq!(
                events.recv_timeout(WAIT).unwrap(),
                NativeWorkOrigin::Generate
            );
            let before = counts(&streaming, NativeWorkOrigin::Generate);
            let order = streaming.order;
            assert!(!streaming.native_write_fence_current().unwrap());
            assert_eq!(counts(&streaming, NativeWorkOrigin::Generate), before);
            assert_eq!(streaming.order, order);
            assert!(streaming.deferred.is_empty());
            held.release();
            streaming.wait(WAIT);
            assert!(!streaming.native_write_fence_current().unwrap());
            assert_eq!(streaming.deferred.len(), 1);
            assert_eq!(counts(&streaming, NativeWorkOrigin::Generate).returned, 1);
            streaming
                .native_write_fence_until(&mut world(), Instant::now() + WAIT, || Ok(true))
                .unwrap();
            assert!(streaming.native_write_fence_current().unwrap());

            for failure in [false, true] {
                let (mut streaming, probe, workers, _events, mut init) =
                    fixture(1, false, false, false);
                workers.recv_timeout(WAIT).unwrap();
                if failure {
                    *probe.failure.lock().unwrap() =
                        Some("controlled proto enqueue failure".into());
                }
                let region = Box::new(std::array::from_fn(|i| {
                    payload(301 + i as u64, (i as i32 % 3 - 1, i as i32 / 3 - 1)).0
                }));
                streaming.decorating = 1;
                streaming.submit(None, 0, Job::Decorate(0, 0, region));
                init.release();
                streaming.wait(WAIT);
                streaming.reconcile_native_fence_results(&mut world());
                assert_eq!(streaming.written.len(), 9);
                let before = format!("{:?}", streaming.observe_native_work());
                let written = streaming.written.clone();
                let order = streaming.order;
                assert!(!streaming.native_write_fence_current().unwrap());
                assert_eq!(streaming.written, written);
                assert_eq!(streaming.order, order);
                assert_eq!(format!("{:?}", streaming.observe_native_work()), before);
                assert!(probe.seen.lock().unwrap().is_empty());
                let closed =
                    streaming
                        .native_write_fence_until(&mut world(), Instant::now() + WAIT, || Ok(true));
                assert_eq!(closed.is_err(), failure);
                if failure {
                    assert!(streaming.native_write_fence_current().is_err());
                    assert_eq!(streaming.written, written);
                } else {
                    assert!(streaming.native_write_fence_current().unwrap());
                }
            }
            let (mut streaming, _probe, workers, _events, mut init) =
                fixture(1, false, true, false);
            workers.recv_timeout(WAIT).unwrap();
            init.release();
            for worker in streaming.threads.drain(..) {
                assert!(worker.join().is_err());
            }
            assert!(streaming.native_write_fence_current().is_err());
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

    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    #[allow(clippy::type_complexity)]
    fn retained_native_fixture(
        label: &str,
        hold: Option<usize>,
        fail: Option<usize>,
        workers: usize,
    ) -> (
        Result<Streaming, StreamingStartFailure>,
        Arc<crate::save::SaveDir>,
        Arc<crate::save::Persistent<crate::testgen::TestTerrain>>,
        RetainedStreamingStartTestReceipt,
        std::path::PathBuf,
    ) {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "retained-stream-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let context = context();
        let save = Arc::new(crate::save::SaveDir::new_retained(root.clone()));
        let generator = Arc::new(crate::save::Persistent {
            inner: Arc::new(
                crate::testgen::TestTerrain::new(context.assets.registry, &context.biomes).unwrap(),
            ),
            save: save.clone(),
            registry: context.assets.clone(),
            biomes: context.biomes.clone(),
        });
        let (control, receipt) = RetainedStreamingStartTestControl::new(hold, fail);
        let streaming = Streaming::start_retained_with_test_control(
            generator.clone(),
            context,
            workers,
            save.clone(),
            control,
        );
        (streaming, save, generator, receipt, root)
    }

    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    #[test]
    #[ignore = "isolated pinned resources: original saved native load, open ticket and real producer/writer close"]
    fn retained_open_saved_load_preserves_native_payload_and_close_custody() {
        let (started, save, generator, mut initialization, _root) =
            retained_native_fixture("open-saved", Some(0), None, 1);
        let mut streaming = started.unwrap();
        streaming.radius = 2;
        streaming.mesh = false;
        let mut world = Box::new(world());
        let mut columns = Vec::new();
        for x in [0, 1] {
            columns.push(Arc::new(generator.inner.finish(generator.inner.proto(
                &mut (),
                x,
                0,
            ))));
        }
        let entity = horizon_world::nbt::Tag::compound([
            (
                "id",
                horizon_world::nbt::Tag::String("test:opaque_native_entity".into()),
            ),
            ("marker", horizon_world::nbt::Tag::Long(731)),
        ]);
        // This codec/custody fixture preserves an opaque saved entity. It does
        // not assert that Game's stricter item-only profile accepts that entity.
        save.save_later(&columns, generator.registry.registry, &generator.biomes)
            .unwrap();
        save.save_entities_later(vec![((0, 0), vec![entity.clone()])])
            .unwrap();
        let until = Instant::now() + WAIT;
        while !save.writes_finished().unwrap() {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        streaming.generating.insert((0, 0));
        streaming.submit(None, 0, Job::Generate(0, 0));
        assert!(
            streaming.poll_retained_open_load().unwrap().is_none(),
            "original queued read has not arrived"
        );
        assert_eq!(counts(&streaming, NativeWorkOrigin::Generate).queued, 1);
        initialization.release();
        streaming.wait(WAIT);
        let view = streaming
            .poll_retained_open_load()
            .unwrap()
            .expect("actual original saved read");
        let load = view.load();
        assert!(load.error().is_none() && !load.is_absent());
        let original_section = Arc::as_ptr(&load.chunk().unwrap().sections[0]);
        let originally_lit = load.chunk().unwrap().light_loaded;
        assert!(Arc::ptr_eq(load.evidence().unwrap().save(), &save));
        assert_eq!(
            load.chunk().unwrap().entities.as_slice(),
            std::slice::from_ref(&entity)
        );
        let ticket = view.into_ticket();
        let replay = RetainedOpenLoadTicket {
            allocation: ticket.allocation.clone(),
            serial: ticket.serial,
        };
        let stale = RetainedOpenLoadTicket {
            allocation: ticket.allocation.clone(),
            serial: ticket.serial,
        };
        let before = streaming.observe_native_work();
        world.insert_streamed_chunk((*columns[0]).clone());
        assert!(
            streaming
                .accept_retained_open_load(ticket, &mut world)
                .is_err(),
            "resident collision retains original read"
        );
        assert_eq!(streaming.observe_native_work(), before);
        assert_eq!(
            Arc::as_ptr(
                &streaming
                    .poll_retained_open_load()
                    .unwrap()
                    .unwrap()
                    .load()
                    .chunk()
                    .unwrap()
                    .sections[0]
            ),
            original_section
        );
        world.remove_streamed_chunk(&(0, 0));
        let foreign = Streaming::new(generator.inner.clone(), context(), 1);
        let wrong = RetainedOpenLoadTicket {
            allocation: foreign.shared.clone(),
            serial: replay.serial,
        };
        assert!(
            streaming
                .accept_retained_open_load(wrong, &mut world)
                .is_err()
        );
        drop(foreign);
        let ticket = streaming
            .poll_retained_open_load()
            .unwrap()
            .unwrap()
            .into_ticket();
        assert!(
            streaming
                .accept_retained_open_load(ticket, &mut world)
                .unwrap()
        );
        assert_eq!(
            Arc::as_ptr(&world.chunks[&(0, 0)].sections[0]),
            original_section
        );
        assert_eq!(world.loaded_entities, [((0, 0), vec![entity])]);
        assert_eq!(streaming.is_lit(0, 0), originally_lit);
        assert!(
            streaming
                .accept_retained_open_load(replay, &mut world)
                .is_err()
        );
        assert!(!streaming.retained_sealed());
        assert_eq!(counts(&streaming, NativeWorkOrigin::Generate).disposed, 1);
        streaming.generating.insert((1, 0));
        streaming.submit(None, 0, Job::Generate(1, 0));
        streaming.wait(WAIT);
        let outstanding = streaming
            .poll_retained_open_load()
            .unwrap()
            .unwrap()
            .into_ticket();
        assert!(
            streaming
                .accept_retained_open_load(stale, &mut world)
                .is_err(),
            "old serial cannot consume the next actual read"
        );
        assert_eq!(counts(&streaming, NativeWorkOrigin::Generate).disposed, 1);
        streaming
            .begin_retained_close(Some(&world), Instant::now() + Duration::from_secs(30))
            .unwrap();
        assert!(
            streaming
                .accept_retained_open_load(outstanding, &mut world)
                .is_err()
        );
        assert!(streaming.poll_retained_open_load().is_err());
        let (observation, failed) = reap_retained(&mut streaming, Some(&mut world), &save);
        assert!(!failed);
        let (_, generated) = observation.origins[NativeWorkOrigin::Generate as usize];
        assert_eq!(generated.submitted, 2);
        assert_eq!(generated.disposed, 2);
        assert_eq!(generated.unresolved, 0);
        assert!(
            world.chunks.contains_key(&(0, 0)) && !world.chunks.contains_key(&(1, 0)),
            "outstanding saved read closed without fake residency"
        );
    }

    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    fn reap_retained(
        streaming: &mut Streaming,
        mut world: Option<&mut World>,
        save: &Arc<crate::save::SaveDir>,
    ) -> (NativeWorkObservation, bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match streaming.poll_retained_close(world.as_deref_mut()) {
                RetainedStreamStatus::Complete => break,
                RetainedStreamStatus::Pending => {}
                RetainedStreamStatus::NeedsFinishedColumn => {
                    let column = streaming.retained_finished_column().unwrap();
                    // This unit case owns genuine fresh TestTerrain native output.
                    // Game's strict D validation is qualified in the bin tests.
                    let disposition = if column.evidence().is_some() {
                        RetainedColumnDisposition::AlreadyStored
                    } else {
                        RetainedColumnDisposition::StoreFresh
                    };
                    let ticket = column.into_ticket();
                    streaming
                        .resolve_retained_finished_column(ticket, disposition)
                        .unwrap();
                }
                RetainedStreamStatus::NeedsRefusedRead => {
                    let read = streaming.retained_refused_read().unwrap();
                    let error = read.error().to_owned();
                    let ticket = read.into_ticket();
                    streaming
                        .resolve_retained_refused_read(ticket, error)
                        .unwrap();
                }
                RetainedStreamStatus::Blocked(error) => {
                    panic!("unexpected retained producer fault: {error}")
                }
            }
            assert!(
                Instant::now() < deadline,
                "retained producer did not finish after control release"
            );
            std::thread::yield_now();
        }
        let witness = streaming.take_sealed().unwrap();
        let failed = witness.failed_initialization;
        let mut writer = loop {
            if let Some(writer) = save.begin_retained_writer_close(deadline).unwrap() {
                break writer;
            }
            std::thread::yield_now();
        };
        loop {
            match writer.poll() {
                crate::save::RetainedWriterStatus::Complete => break,
                crate::save::RetainedWriterStatus::Pending => std::thread::yield_now(),
                crate::save::RetainedWriterStatus::Blocked(error) => {
                    panic!("unexpected retained writer fault: {error}")
                }
            }
        }
        let writer = writer.finish().unwrap();
        loop {
            match streaming.poll_retained_writer_ack(&writer) {
                RetainedStreamStatus::Complete => break,
                RetainedStreamStatus::Pending => std::thread::yield_now(),
                status => panic!("unexpected retained writer acknowledgement: {status:?}"),
            }
        }
        streaming.finish_retained_close(witness, &writer).unwrap();
        (streaming.observe_native_work(), failed)
    }

    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    #[test]
    #[ignore = "owned native resources: actual Persistent save mismatch and unknown provider rejected before normal/controlled worker spawn"]
    fn retained_stream_constructor_rejects_mismatched_and_unknown_native_save() {
        for unknown in [false, true] {
            for controlled in [false, true] {
                let unique = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let root = std::env::temp_dir().join(format!(
                    "retained-stream-binding-{}-{unique}",
                    std::process::id()
                ));
                std::fs::create_dir_all(&root).unwrap();
                let context = context();
                let supplied = Arc::new(crate::save::SaveDir::new_retained(root.join("a")));
                let actual = Arc::new(crate::save::SaveDir::new_retained(root.join("b")));
                let terrain = Arc::new(
                    crate::testgen::TestTerrain::new(context.assets.registry, &context.biomes)
                        .unwrap(),
                );
                let generator = Arc::new(crate::save::Persistent {
                    inner: terrain.clone(),
                    save: actual.clone(),
                    registry: context.assets.clone(),
                    biomes: context.biomes.clone(),
                });
                let (control, mut receipt) = RetainedStreamingStartTestControl::new(Some(0), None);
                let started = if unknown {
                    if controlled {
                        Streaming::start_retained_with_test_control(
                            terrain.clone(),
                            context,
                            2,
                            supplied.clone(),
                            control,
                        )
                    } else {
                        Streaming::start_retained(terrain.clone(), context, 2, supplied.clone())
                    }
                } else if controlled {
                    Streaming::start_retained_with_test_control(
                        generator.clone(),
                        context,
                        2,
                        supplied.clone(),
                        control,
                    )
                } else {
                    Streaming::start_retained(generator.clone(), context, 2, supplied.clone())
                };
                let (mut streaming, error) = match started {
                    Ok(streaming) => (streaming, None),
                    Err(failure) => (failure.streaming, Some(failure.error)),
                };
                let workers = streaming.threads.len();
                let entered = receipt.entered();
                let observation = streaming.observe_native_work();
                let no_queue = streaming.shared.queue.lock().unwrap().is_empty();
                receipt.release();
                streaming
                    .begin_retained_close(None, Instant::now() + WAIT)
                    .unwrap();
                reap_retained(&mut streaming, None, &supplied);
                let original_error_kept = error
                    .as_ref()
                    .is_some_and(|error| streaming.retained_close_failures().contains(error));
                let deadline = Instant::now() + WAIT;
                let mut actual_close = loop {
                    if let Some(close) = actual.begin_retained_writer_close(deadline).unwrap() {
                        break close;
                    }
                    std::thread::yield_now();
                };
                while actual_close.poll() != crate::save::RetainedWriterStatus::Complete {
                    std::thread::yield_now();
                }
                let actual_closed = actual_close.finish().unwrap();
                let actual_identity = Arc::ptr_eq(actual_closed.save(), &actual);
                drop(streaming);
                drop(generator);
                drop(terrain);
                drop(receipt);
                drop(supplied);
                drop(actual_closed);
                drop(actual);
                std::fs::remove_dir_all(root).unwrap();
                assert_eq!(workers, 0);
                assert!(!entered && no_queue && actual_identity && original_error_kept);
                assert_eq!(observation.initialized_workers, 0);
                assert!(
                    observation
                        .origins
                        .iter()
                        .all(|(_, counts)| counts.submitted == 0)
                );
                assert_eq!(
                    error.as_deref(),
                    Some(if unknown {
                        "generator has no retained SaveDir binding"
                    } else {
                        "retained generator SaveDir allocation mismatch"
                    })
                );
            }
        }
    }

    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    #[test]
    #[ignore = "owned native resources: first/later retained init, spawn failure, deadline and same-owner retry"]
    fn retained_stream_partial_start_deadline_and_explicit_retry() {
        for (hold, fail, workers) in [
            (None, Some(0), 2),
            (Some(0), Some(1), 2),
            (Some(1), None, 2),
        ] {
            let (started, save, generator, mut receipt, root) =
                retained_native_fixture("partial", hold, fail, workers);
            let (mut streaming, spawn_failed) = match started {
                Ok(streaming) => (streaming, false),
                Err(failure) => (failure.streaming, true),
            };
            let expected_workers = fail.unwrap_or(workers);
            let actual_workers = streaming.threads.len();
            streaming
                .begin_retained_close(None, Instant::now())
                .unwrap();
            let blocked = matches!(
                streaming.poll_retained_close(None),
                RetainedStreamStatus::Blocked(_)
            );
            streaming
                .begin_retained_close(None, Instant::now() + WAIT)
                .unwrap();
            let still_blocked = matches!(
                streaming.poll_retained_close(None),
                RetainedStreamStatus::Blocked(_)
            );
            receipt.release();
            streaming
                .retry_retained_close(Instant::now() + Duration::from_secs(30))
                .unwrap();
            let (_, failed) = reap_retained(&mut streaming, None, &save);
            let joined = streaming.threads.is_empty();
            drop(streaming);
            drop(generator);
            drop(save);
            drop(receipt);
            std::fs::remove_dir_all(root).unwrap();
            assert_eq!(actual_workers, expected_workers);
            assert_eq!(failed, spawn_failed);
            assert!(blocked && still_blocked && joined);
        }
    }

    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    #[test]
    #[ignore = "owned native resources: real queue/receipt/book contention and same allocation writer witness"]
    fn retained_stream_mutex_contention_and_writer_identity() {
        let (started, save, generator, mut receipt, root) =
            retained_native_fixture("locks", Some(0), None, 1);
        let mut streaming = started.unwrap();
        streaming
            .begin_retained_close(None, Instant::now() + Duration::from_secs(30))
            .unwrap();
        let shared = streaming.shared.clone();
        let mut pending = Vec::new();
        {
            let _lock = shared.ownership.lock().unwrap();
            pending.push(streaming.poll_retained_close(None) == RetainedStreamStatus::Pending);
        }
        {
            let _lock = shared.returned.lock().unwrap();
            pending.push(streaming.poll_retained_close(None) == RetainedStreamStatus::Pending);
        }
        {
            let _lock = shared.queue.lock().unwrap();
            pending.push(streaming.poll_retained_close(None) == RetainedStreamStatus::Pending);
        }
        receipt.release();
        let observation = reap_retained(&mut streaming, None, &save).0;
        let joined = streaming.threads.is_empty();
        drop(streaming);
        drop(generator);
        drop(save);
        drop(receipt);
        drop(shared);
        std::fs::remove_dir_all(root).unwrap();
        assert!(pending.into_iter().all(|value| value));
        assert!(joined && observation.stopped);
    }

    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    #[test]
    #[ignore = "owned native resources: actual initialization panic reaped and foreign writer witness refused"]
    fn retained_stream_failed_initialization_and_foreign_writer_witness() {
        let (started, save, generator, mut receipt, root) =
            retained_native_fixture("panic-base", Some(0), Some(0), 1);
        // Reap the genuine zero-worker partial allocation before starting the
        // separately controlled actual panicking native worker.
        let mut partial = match started {
            Err(failure) => failure.streaming,
            Ok(_) => panic!("controlled first spawn unexpectedly succeeded"),
        };
        partial
            .begin_retained_close(None, Instant::now() + WAIT)
            .unwrap();
        receipt.release();
        reap_retained(&mut partial, None, &save);
        drop(partial);
        drop(generator);
        drop(save);
        drop(receipt);
        let save = Arc::new(crate::save::SaveDir::new_retained(root.join("panic")));
        let context = context();
        let generator = Arc::new(crate::save::Persistent {
            inner: Arc::new(
                crate::testgen::TestTerrain::new(context.assets.registry, &context.biomes).unwrap(),
            ),
            save: save.clone(),
            registry: context.assets.clone(),
            biomes: context.biomes.clone(),
        });
        let (mut control, mut receipt) = RetainedStreamingStartTestControl::new(Some(0), None);
        control.panic_initialization_at(0);
        let mut streaming = Streaming::start_retained_with_test_control(
            generator.clone(),
            context,
            1,
            save.clone(),
            control,
        )
        .unwrap();
        streaming
            .begin_retained_close(None, Instant::now() + Duration::from_secs(30))
            .unwrap();
        receipt.release();
        while streaming.poll_retained_close(None) != RetainedStreamStatus::Complete {
            std::thread::yield_now();
        }
        let witness = streaming.take_sealed().unwrap();
        let failed = witness.failed_initialization;
        let other = Arc::new(crate::save::SaveDir::new_retained(root.join("other")));
        let mut other_close = other
            .begin_retained_writer_close(Instant::now() + WAIT)
            .unwrap()
            .unwrap();
        while other_close.poll() != crate::save::RetainedWriterStatus::Complete {
            std::thread::yield_now();
        }
        let other_closed = other_close.finish().unwrap();
        let witness = streaming
            .finish_retained_close(witness, &other_closed)
            .unwrap_err();
        let still_armed = streaming.retained.as_ref().unwrap().armed;
        let mut close = save
            .begin_retained_writer_close(Instant::now() + WAIT)
            .unwrap()
            .unwrap();
        while close.poll() != crate::save::RetainedWriterStatus::Complete {
            std::thread::yield_now();
        }
        let closed = close.finish().unwrap();
        streaming.finish_retained_close(witness, &closed).unwrap();
        let observation = streaming.observe_native_work();
        drop(streaming);
        drop(generator);
        drop(save);
        drop(receipt);
        drop(other_closed);
        drop(other);
        drop(closed);
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            failed && still_armed && observation.worker_initialization_fault && observation.stopped
        );
        assert_eq!(observation.initialized_workers, 0);
    }

    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    #[test]
    #[ignore = "owned native resources: genuine mixed all-origin jobs, stale light/mesh, native writer and disk decode"]
    fn retained_stream_all_origins_keep_native_payloads_and_reap() {
        let (started, save, generator, mut receipt, root) =
            retained_native_fixture("origins", Some(0), None, 1);
        let mut streaming = started.unwrap();
        let mut world = Box::new(world());
        for z in -1..=1 {
            for x in -1..=1 {
                let proto = generator.proto(&mut (), x, z);
                let chunk = generator.finish(proto);
                world.chunks.insert((x, z), Arc::new(chunk));
            }
        }
        let region = Streaming::region(&world, 0, 0).unwrap();
        streaming.generating.insert((40, 40));
        streaming.submit(None, 0, Job::Generate(40, 40));
        let finish = generator.proto(&mut (), 30, 30);
        streaming.finishing.insert((30, 30));
        streaming.submit(None, 0, Job::Finish(finish));
        let decoration: Box<[Proto; 9]> = Box::new(std::array::from_fn(|i| {
            let key = (10 + i as i32 % 3 - 1, 10 + i as i32 / 3 - 1);
            streaming.protos.insert(key, None);
            generator.proto(&mut (), key.0, key.1)
        }));
        streaming.decorating = 1;
        streaming.submit(None, 0, Job::Decorate(10, 10, decoration));
        let saved = generator.proto(&mut (), 20, 20);
        streaming.protos.insert((20, 20), None);
        streaming.saving = 1;
        streaming.submit(None, 0, Job::Save(vec![((20, 20), saved)]));
        streaming.lighting.insert((0, 0), 2);
        streaming.light_snapshots.insert((0, 0), 0);
        streaming.submit(None, 0, Job::Light(0, 0, region.clone(), 1, true));
        streaming.submit(None, 0, Job::Light(0, 0, region.clone(), 2, true));
        let settings = MeshSettings {
            nether: false,
            zoom: BiomeZoom::new(0),
            blend_radius: 2,
            smooth_lighting: true,
            cutout_leaves: true,
        };
        streaming.meshing.insert((0, 4, 0), 4);
        streaming.submit(None, 0, Job::Mesh(0, 4, 0, region.clone(), 3, settings));
        streaming.submit(None, 0, Job::Mesh(0, 4, 0, region, 4, settings));
        let original_membership = world.chunks.membership_revision();
        streaming
            .begin_retained_close(Some(&world), Instant::now() + Duration::from_secs(30))
            .unwrap();
        let sealed_updates = streaming.update(&mut world).0.is_empty();
        let sealed_save = streaming.save(&mut world, false).is_err();
        let before = streaming.observe_native_work();
        receipt.release();
        let observation = reap_retained(&mut streaming, Some(&mut world), &save).0;
        let loaded = save.load(30, 30, context().assets.registry, &context().biomes);
        let disk_coordinates = loaded.as_ref().map(|c| (c.x, c.z));
        let membership = world.chunks.membership_revision();
        let all_origins = observation.origins.iter().all(|(_, c)| {
            c.submitted > 0
                && c.queued == 0
                && c.worker_held == 0
                && c.returned == 0
                && c.reconciling == 0
                && c.unresolved == 0
                && c.disposed == c.submitted
        });
        drop(streaming);
        drop(generator);
        drop(save);
        drop(receipt);
        drop(world);
        std::fs::remove_dir_all(root).unwrap();
        assert!(sealed_updates && sealed_save && all_origins);
        assert_eq!(
            before.origins[NativeWorkOrigin::Generate as usize].1.queued,
            1
        );
        assert_eq!(disk_coordinates, Some((30, 30)));
        assert_eq!(membership, original_membership);
    }

    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    #[allow(clippy::type_complexity)]
    fn retained_one_job(
        origin: NativeWorkOrigin,
    ) -> (
        Streaming,
        Box<World>,
        Arc<crate::save::SaveDir>,
        Arc<crate::save::Persistent<crate::testgen::TestTerrain>>,
        RetainedStreamingStartTestReceipt,
        std::path::PathBuf,
    ) {
        let (started, save, generator, receipt, root) =
            retained_native_fixture("held-origin", Some(0), None, 1);
        let mut streaming = started.unwrap();
        let mut world = Box::new(world());
        let job = match origin {
            NativeWorkOrigin::Generate => {
                streaming.generating.insert((40, 40));
                Job::Generate(40, 40)
            }
            NativeWorkOrigin::Finish => {
                streaming.finishing.insert((30, 30));
                Job::Finish(generator.proto(&mut (), 30, 30))
            }
            NativeWorkOrigin::Save => {
                streaming.protos.insert((20, 20), None);
                streaming.saving = 1;
                Job::Save(vec![((20, 20), generator.proto(&mut (), 20, 20))])
            }
            NativeWorkOrigin::Decorate => {
                streaming.decorating = 1;
                let region = std::array::from_fn(|i| {
                    let key = (10 + i as i32 % 3 - 1, 10 + i as i32 / 3 - 1);
                    streaming.protos.insert(key, None);
                    generator.proto(&mut (), key.0, key.1)
                });
                Job::Decorate(10, 10, Box::new(region))
            }
            NativeWorkOrigin::Light | NativeWorkOrigin::Mesh => {
                for z in -1..=1 {
                    for x in -1..=1 {
                        world.chunks.insert(
                            (x, z),
                            Arc::new(generator.finish(generator.proto(&mut (), x, z))),
                        );
                    }
                }
                let region = Streaming::region(&world, 0, 0).unwrap();
                if origin == NativeWorkOrigin::Light {
                    streaming.lighting.insert((0, 0), 1);
                    streaming.light_snapshots.insert((0, 0), 0);
                    Job::Light(0, 0, region, 1, true)
                } else {
                    streaming.meshing.insert((0, 4, 0), 1);
                    Job::Mesh(
                        0,
                        4,
                        0,
                        region,
                        1,
                        MeshSettings {
                            nether: false,
                            zoom: BiomeZoom::new(0),
                            blend_radius: 2,
                            smooth_lighting: true,
                            cutout_leaves: true,
                        },
                    )
                }
            }
            _ => unreachable!(),
        };
        streaming.submit(None, 0, job);
        (streaming, world, save, generator, receipt, root)
    }

    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    #[test]
    #[ignore = "owned native worldgen resources: genuine Vanilla proto Save job and close callback survive actual writer closure and native reload/restore"]
    fn retained_stream_vanilla_proto_codec_survives_close_and_native_reload() {
        let context = context();
        let resources = std::env::var_os("HORIZON_TEST_RESOURCES")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../.local/horizon-client/26.3")
            });
        let terrain = Arc::new(
            crate::vanillagen::VanillaTerrain::for_world(
                &resources.join("worldgen"),
                context.assets.registry,
                &context.biomes,
                187,
                "minecraft:overworld",
                false,
            )
            .unwrap(),
        );
        // No optional detached Ring search. Both opaque payloads come from the
        // actual Vanilla worker and its real saved_proto projection.
        let keys = [(0, 0), (1, 0)];
        let mut worker = terrain.worker();
        let job_proto = terrain.proto(&mut worker, keys[0].0, keys[0].1);
        let close_proto = terrain.proto(&mut worker, keys[1].0, keys[1].1);
        let expected = [
            terrain.saved_proto(&job_proto).unwrap(),
            terrain.saved_proto(&close_proto).unwrap(),
        ];
        let address = |proto: &Proto| {
            proto.as_ref() as *const (dyn std::any::Any + Send) as *const () as usize
        };
        let original_addresses = [address(&job_proto), address(&close_proto)];
        let tables = Arc::new(crate::save::SaveTables::new(&context.assets));
        let encode = |proto: &crate::save::SavedProto| {
            crate::save::encode_proto(
                proto,
                context.assets.registry,
                &context.biomes,
                Some(&tables),
                crate::save::LevelShape::OVERWORLD,
            )
        };
        let expected_native = expected.iter().map(encode).collect::<Vec<_>>();
        drop(worker);
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "retained-stream-vanilla-proto-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let save = Arc::new(crate::save::SaveDir::new_retained(root.clone()));
        let generator = Arc::new(crate::save::Persistent {
            inner: terrain.clone(),
            save: save.clone(),
            registry: context.assets.clone(),
            biomes: context.biomes.clone(),
        });
        let (control, mut initialization) = RetainedStreamingStartTestControl::new(Some(0), None);
        let mut streaming = Streaming::start_retained_with_test_control(
            generator.clone(),
            context.clone(),
            1,
            save.clone(),
            control,
        )
        .unwrap();
        streaming.protos.insert(keys[0], None);
        streaming.saving = 1;
        streaming.submit(None, 0, Job::Save(vec![(keys[0], job_proto)]));
        streaming.protos.insert(keys[1], Some(close_proto));
        streaming.written.insert(keys[1]);
        let queued_save = counts(&streaming, NativeWorkOrigin::Save).queued;
        let mut callback = streaming
            .arm_retained_handoff_test_control(NativeWorkOrigin::SaveProtosCallback)
            .unwrap();
        let mut world = Box::new(world());
        let deadline = Instant::now() + Duration::from_secs(30);
        streaming
            .begin_retained_close(Some(&world), deadline)
            .unwrap();
        initialization.release();
        let mut unexpected_status = false;
        while !callback.entered() && Instant::now() < deadline {
            if streaming.poll_retained_close(Some(&mut world)) != RetainedStreamStatus::Pending {
                unexpected_status = true;
                break;
            }
            std::thread::yield_now();
        }
        let callback_held = callback.entered();
        let callback_before_acceptance = counts(&streaming, NativeWorkOrigin::SaveProtosCallback);
        let marker_owned = streaming
            .retained
            .as_ref()
            .unwrap()
            .close
            .as_ref()
            .is_some_and(|close| {
                close.proto_key == Some(keys[1]) && close.proto_callback.is_some()
            });
        callback.release();
        let observation = reap_retained(&mut streaming, Some(&mut world), &save).0;
        let returned_addresses = keys.map(|key| {
            streaming
                .protos
                .get(&key)
                .and_then(Option::as_ref)
                .map(address)
        });
        let joined_and_acknowledged =
            streaming.threads.is_empty() && !streaming.retained.as_ref().unwrap().armed;
        let no_markers = streaming.written.is_empty()
            && streaming
                .retained
                .as_ref()
                .unwrap()
                .close
                .as_ref()
                .is_some_and(|close| close.proto_key.is_none() && close.proto_callback.is_none());
        drop(streaming);
        drop(world);
        drop(generator);
        drop(save);
        drop(initialization);
        drop(callback);
        // A new ordinary SaveDir has no pending snapshots from the producer.
        // These reads must use the actual native region files written above.
        let reopened = crate::save::SaveDir::new(root.clone());
        reopened.tables.get_or_init(|| tables.clone());
        let physical_matches = |a: &crate::save::SavedProto, b: &crate::save::SavedProto| {
            (a.column.x, a.column.z, a.decorated) == (b.column.x, b.column.z, b.decorated)
                && a.block_entities == b.block_entities
                && a.entities == b.entities
                && a.column.postprocess == b.column.postprocess
                && a.column.sections.len() == b.column.sections.len()
                && a.column
                    .sections
                    .iter()
                    .zip(&b.column.sections)
                    .all(|(a, b)| {
                        (0..4096).all(|i| a.get(i) == b.get(i) && a.light(i) == b.light(i))
                            && (0..64).all(|i| a.biomes.get(i) == b.biomes.get(i))
                    })
        };
        let mut disk_matches = Vec::new();
        let mut restored_matches = Vec::new();
        let mut worker = terrain.worker();
        for (index, key) in keys.into_iter().enumerate() {
            if let Some(saved) =
                reopened.load_proto(key.0, key.1, context.assets.registry, &context.biomes)
            {
                disk_matches.push(
                    physical_matches(&expected[index], &saved)
                        && encode(&saved).tag == expected_native[index].tag,
                );
                let mut restored = terrain.proto(&mut worker, key.0, key.1);
                terrain.restore_proto(&mut restored, saved);
                restored_matches.push(terrain.saved_proto(&restored).is_some_and(|saved| {
                    physical_matches(&expected[index], &saved)
                        && encode(&saved).tag == expected_native[index].tag
                }));
            } else {
                disk_matches.push(false);
                restored_matches.push(false);
            }
        }
        drop(worker);
        drop(reopened);
        drop(terrain);
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(queued_save, 1);
        assert!(
            !unexpected_status
                && callback_held
                && marker_owned
                && no_markers
                && joined_and_acknowledged
        );
        assert_eq!(returned_addresses, original_addresses.map(Some));
        assert_eq!(
            (
                callback_before_acceptance.reconciling,
                callback_before_acceptance.accepted_storage_handoffs
            ),
            (1, 0)
        );
        for origin in [NativeWorkOrigin::Save, NativeWorkOrigin::SaveProtosCallback] {
            let counts = observation.origins[origin as usize].1;
            assert_eq!(
                (
                    counts.submitted,
                    counts.disposed,
                    counts.accepted_storage_handoffs
                ),
                (1, 1, 1)
            );
            assert_eq!(
                (
                    counts.queued,
                    counts.worker_held,
                    counts.returned,
                    counts.reconciling,
                    counts.unresolved,
                    counts.storage_errors
                ),
                (0, 0, 0, 0, 0, 0)
            );
        }
        assert!(observation.stopped && observation.initialized_workers == 1);
        assert_eq!(disk_matches, [true, true]);
        assert_eq!(restored_matches, [true, true]);
    }

    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    #[test]
    #[ignore = "owned native resources: each real native job origin independently worker-held across nonwaiting close turns"]
    fn retained_stream_each_native_job_remains_worker_held() {
        for origin in [
            NativeWorkOrigin::Generate,
            NativeWorkOrigin::Decorate,
            NativeWorkOrigin::Finish,
            NativeWorkOrigin::Light,
            NativeWorkOrigin::Mesh,
            NativeWorkOrigin::Save,
        ] {
            let (mut streaming, mut world, save, generator, mut receipt, root) =
                retained_one_job(origin);
            let (entered, seen) = mpsc::channel();
            let (release, gate) = mpsc::channel();
            *streaming.shared.work_gate.lock().unwrap() = Some(WorkGate {
                origin,
                entered,
                release: gate,
            });
            streaming
                .begin_retained_close(Some(&world), Instant::now() + Duration::from_secs(30))
                .unwrap();
            receipt.release();
            let entered = seen.recv_timeout(WAIT).is_ok();
            let mut pending = true;
            for _ in 0..3 {
                pending &= streaming.poll_retained_close(Some(&mut world))
                    == RetainedStreamStatus::Pending;
            }
            let held = counts(&streaming, origin).worker_held;
            let _ = release.send(());
            let observation = reap_retained(&mut streaming, Some(&mut world), &save).0;
            drop(streaming);
            drop(world);
            drop(generator);
            drop(save);
            drop(receipt);
            std::fs::remove_dir_all(root).unwrap();
            assert!(entered && pending);
            assert_eq!(held, 1);
            assert_eq!(observation.origins[origin as usize].1.disposed, 1);
        }
    }

    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    #[test]
    #[ignore = "owned native resources: each exact native storage callback held with original payload/lease then actual writer closure"]
    fn retained_stream_native_callbacks_retain_payload_until_acceptance() {
        for origin in [
            NativeWorkOrigin::SaveProtosCallback,
            NativeWorkOrigin::SaveFinishedCallback,
        ] {
            let job = if origin == NativeWorkOrigin::SaveProtosCallback {
                NativeWorkOrigin::Decorate
            } else {
                NativeWorkOrigin::Finish
            };
            let (mut streaming, mut world, save, generator, mut receipt, root) =
                retained_one_job(job);
            let mut gate = streaming.arm_retained_handoff_test_control(origin).unwrap();
            streaming
                .begin_retained_close(Some(&world), Instant::now() + Duration::from_secs(30))
                .unwrap();
            receipt.release();
            let deadline = Instant::now() + Duration::from_secs(30);
            while !gate.entered() && Instant::now() < deadline {
                if streaming.poll_retained_close(Some(&mut world))
                    == RetainedStreamStatus::NeedsFinishedColumn
                {
                    let ticket = streaming.retained_finished_column().unwrap().into_ticket();
                    streaming
                        .resolve_retained_finished_column(
                            ticket,
                            RetainedColumnDisposition::StoreFresh,
                        )
                        .unwrap();
                }
                std::thread::yield_now();
            }
            let entered = gate.entered();
            let held = counts(&streaming, origin).reconciling;
            let before = streaming
                .retained_finished_column()
                .map(|v| Arc::as_ptr(v.chunk()));
            for _ in 0..3 {
                let _ = streaming.poll_retained_close(Some(&mut world));
            }
            let after = streaming
                .retained_finished_column()
                .map(|v| Arc::as_ptr(v.chunk()));
            gate.release();
            let observation = reap_retained(&mut streaming, Some(&mut world), &save).0;
            drop(streaming);
            drop(world);
            drop(generator);
            drop(save);
            drop(receipt);
            drop(gate);
            std::fs::remove_dir_all(root).unwrap();
            assert!(entered && held == 1);
            assert_eq!(before, after);
            assert!(
                observation.origins[origin as usize]
                    .1
                    .accepted_storage_handoffs
                    > 0
            );
        }
    }

    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    #[test]
    #[ignore = "owned native resources: pre-close native callback error keeps exact original receipt/batch and explicit retry"]
    fn retained_stream_failed_preclose_callbacks_keep_original_receipts() {
        for origin in [
            NativeWorkOrigin::SaveFinishedCallback,
            NativeWorkOrigin::SaveProtosCallback,
        ] {
            let (mut streaming, mut world, save, generator, mut receipt, root) =
                if origin == NativeWorkOrigin::SaveFinishedCallback {
                    retained_one_job(NativeWorkOrigin::Finish)
                } else {
                    let (started, save, generator, receipt, root) =
                        retained_native_fixture("old-proto", Some(0), None, 1);
                    (
                        started.unwrap(),
                        Box::new(world()),
                        save,
                        generator,
                        receipt,
                        root,
                    )
                };
            let mut control = streaming.arm_retained_handoff_test_control(origin).unwrap();
            control.report_error_once_after_native_acceptance();
            control.release();
            receipt.release();
            let failed = if origin == NativeWorkOrigin::SaveFinishedCallback {
                streaming.wait(WAIT);
                streaming.save(&mut world, false).is_err()
            } else {
                for key in [(10, 10), (11, 10), (12, 10)] {
                    streaming
                        .protos
                        .insert(key, Some(generator.proto(&mut (), key.0, key.1)));
                    streaming.written.insert(key);
                }
                streaming.save(&mut world, true).is_err()
            };
            let original_ptr = if origin == NativeWorkOrigin::SaveFinishedCallback {
                streaming
                    .retained
                    .as_ref()
                    .unwrap()
                    .finished_backlog
                    .first()
                    .map(|(chunk, _)| Arc::as_ptr(chunk) as usize)
            } else {
                streaming
                    .protos
                    .get(&(10, 10))
                    .and_then(Option::as_ref)
                    .and_then(|p| p.downcast_ref::<Chunk>())
                    .map(|p| p as *const Chunk as usize)
            };
            let old = counts(&streaming, origin);
            streaming
                .begin_retained_close(Some(&world), Instant::now() + Duration::from_secs(30))
                .unwrap();
            let blocked = matches!(
                streaming.poll_retained_close(Some(&mut world)),
                RetainedStreamStatus::Blocked(_)
            );
            streaming
                .retry_retained_close(Instant::now() + Duration::from_secs(30))
                .unwrap();
            let observation = reap_retained(&mut streaming, Some(&mut world), &save).0;
            let error_still_observable = streaming.generation_save_error().is_some()
                && !streaming.retained_close_failures().is_empty();
            let proto_ptr = streaming
                .protos
                .get(&(10, 10))
                .and_then(Option::as_ref)
                .and_then(|p| p.downcast_ref::<Chunk>())
                .map(|p| p as *const Chunk as usize);
            let native = if origin == NativeWorkOrigin::SaveFinishedCallback {
                save.load(30, 30, context().assets.registry, &context().biomes)
                    .map(|c| (c.x, c.z))
            } else {
                None
            };
            drop(streaming);
            drop(world);
            drop(generator);
            drop(save);
            drop(receipt);
            drop(control);
            std::fs::remove_dir_all(root).unwrap();
            assert!(failed && blocked && error_still_observable && original_ptr.is_some());
            assert_eq!(old.reconciling, 1);
            assert_eq!(old.unresolved, 0);
            let after = observation.origins[origin as usize].1;
            assert_eq!(after.storage_errors, 1);
            assert_eq!(after.unresolved, 0);
            assert!(after.accepted_storage_handoffs >= 1);
            if origin == NativeWorkOrigin::SaveProtosCallback {
                assert_eq!(original_ptr, proto_ptr);
            } else {
                assert_eq!(native, Some((30, 30)));
            }
        }
    }

    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    #[test]
    #[ignore = "direct fatal exception: unresolved retained Streaming dropped during actual held initialization; expected SIGABRT"]
    fn retained_stream_fatal_drop_while_native_initialization_held() {
        use std::io::Write as _;
        let (started, _save, _generator, receipt, _root) =
            retained_native_fixture("fatal-init", Some(0), None, 1);
        let mut streaming = started.unwrap();
        let deadline = Instant::now() + WAIT;
        while !receipt.entered() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        if !receipt.entered() || streaming.threads.len() != 1 || streaming.threads[0].is_finished()
        {
            std::process::exit(92);
        }
        streaming
            .begin_retained_close(None, Instant::now() + Duration::from_secs(30))
            .unwrap();
        let status = streaming.poll_retained_close(None);
        let observation = streaming.observe_native_work();
        if status != RetainedStreamStatus::Pending
            || observation.initialized_workers != 0
            || streaming.threads[0].is_finished()
        {
            std::process::exit(92);
        }
        let _ = writeln!(
            std::io::stderr().lock(),
            "HORIZON_RETAINED_STREAM_HELD_INIT_DROP_ARMED"
        );
        // Deliberate fatal fixture: the held native thread remains owned. A
        // broken Stream guard must not fall through into SaveDir's guard.
        drop(streaming);
        std::process::exit(91);
    }

    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    #[test]
    #[ignore = "direct fatal exception: genuine held native Finish panics, failed actual handle reaped, unresolved work stays Blocked; expected SIGABRT"]
    fn retained_stream_fatal_held_native_work_panic_stays_blocked() {
        use std::io::Write as _;
        let (mut streaming, mut world, _save, _generator, mut initialization, _root) =
            retained_one_job(NativeWorkOrigin::Finish);
        let mut held = streaming
            .arm_retained_native_work_test_control(NativeWorkOrigin::Finish)
            .unwrap();
        streaming
            .shared
            .retained_work_gate
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .panic_after_release = true;
        streaming
            .begin_retained_close(Some(&world), Instant::now() + Duration::from_secs(30))
            .unwrap();
        initialization.release();
        let deadline = Instant::now() + WAIT;
        while !held.entered() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        if !held.entered()
            || counts(&streaming, NativeWorkOrigin::Finish).worker_held != 1
            || streaming.threads.len() != 1
            || streaming.threads[0].is_finished()
        {
            std::process::exit(92);
        }
        if streaming.poll_retained_close(Some(&mut world)) != RetainedStreamStatus::Pending {
            std::process::exit(92);
        }
        held.release();
        let deadline = Instant::now() + WAIT;
        while !streaming.threads[0].is_finished() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        if !streaming.threads[0].is_finished() {
            std::process::exit(92);
        }
        // This exceptional fixture reaps only the proven-finished failed actual
        // handle. It never repairs/zeros the missing unique Finish payload.
        let reaped_failed = streaming.threads.pop().unwrap().join().is_err();
        let observation = streaming.observe_native_work();
        let finish = observation.origins[NativeWorkOrigin::Finish as usize].1;
        let mut blocked = true;
        for _ in 0..3 {
            blocked &= matches!(
                streaming.poll_retained_close(Some(&mut world)),
                RetainedStreamStatus::Blocked(_)
            );
        }
        if !reaped_failed
            || !blocked
            || !observation.worker_exit_fault
            || finish.submitted != 1
            || finish.unresolved != 1
            || finish.disposed != 0
            || finish.worker_held != 0
            || finish.returned != 0
            || !streaming.finishing.contains(&(30, 30))
            || streaming.take_sealed().is_some()
        {
            std::process::exit(92);
        }
        let _ = writeln!(
            std::io::stderr().lock(),
            "HORIZON_RETAINED_STREAM_HELD_WORK_PANIC_DROP_ARMED reaped_failed=true unresolved=1 blocked=true"
        );
        drop(streaming);
        std::process::exit(91);
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
