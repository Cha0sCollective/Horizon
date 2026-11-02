//! Speculative decoration is isolated from gameplay and never writes normal saves.
use super::*;
use crate::{
    fast_hash::{FxHashMap, FxHashSet},
    lod::{Column, Span, Surface},
    streaming::LodDetail,
};
use horizon_world::{
    heightmap::HeightmapState,
    registry::BlockStateId,
    worldgen::{decoration_order::vanilla_order, generator::GeneratorError},
};
use std::{
    collections::VecDeque,
    sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

mod vegetation;
use vegetation::Vegetation;
const CACHE: usize = 1024;
const REQUESTS: usize = 64;
const BATCH: i32 = 4;
fn batch_origin((x, z): (i32, i32)) -> (i32, i32) {
    (x.div_euclid(BATCH) * BATCH, z.div_euclid(BATCH) * BATCH)
}
#[derive(Default)]
struct Requests {
    chunks: FxHashSet<(i32, i32)>,
    center: [i32; 2],
    current: Option<(i32, i32)>,
    finished: FxHashSet<(i32, i32)>,
}
struct Shared {
    requests: Mutex<Requests>,
    wake: Condvar,
    stop: AtomicBool,
}
fn distance((x, z): (i32, i32), center: [i32; 2]) -> i64 {
    let dx = i64::from(x * 16 + 8 - center[0]);
    let dz = i64::from(z * 16 + 8 - center[1]);
    dx * dx + dz * dz
}
struct Builder {
    shared: Arc<Shared>,
    results: mpsc::Receiver<LodDetail>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Builder {
    fn new(terrain: Arc<VanillaTerrain>) -> Self {
        let shared = Arc::new(Shared {
            requests: Mutex::new(Requests::default()),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        let worker = shared.clone();
        let (tx, results) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("Far decoration".into())
            .spawn(move || {
                // Structure pieces have mutable one-shot placement flags. Sharing
                // gameplay's seeded pipeline would consume its chests/spawners.
                let generator = match OverworldGenerator::for_dimension(
                    terrain.generator.data().clone(),
                    terrain.seed,
                    "minecraft:normal",
                    &terrain.dimension,
                ) {
                    Ok(generator) => generator,
                    Err(error) => {
                        log::warn!("Horizon: far decoration unavailable: {error}");
                        worker.stop.store(true, Ordering::Relaxed);
                        return;
                    }
                };
                let mut state = generator.worker();
                loop {
                    let key = {
                        let mut requests = worker.requests.lock().unwrap();
                        while requests.chunks.is_empty() && !worker.stop.load(Ordering::Relaxed) {
                            requests = worker.wake.wait(requests).unwrap();
                        }
                        if worker.stop.load(Ordering::Relaxed) {
                            break;
                        }
                        let key = *requests
                            .chunks
                            .iter()
                            .min_by_key(|&&key| (distance(key, requests.center), key))
                            .unwrap();
                        let origin = batch_origin(key);
                        requests.chunks.retain(|&key| batch_origin(key) != origin);
                        requests.current = Some(origin);
                        origin
                    };
                    let result = decorated_batch(&mut state, key, BATCH, &worker.stop, |chunk| {
                        let pos = chunk.position();
                        let key = (pos.x, pos.z);
                        worker.requests.lock().unwrap().finished.insert(key);
                        let columns = columns(chunk, &terrain.biome_map, &terrain.dimension);
                        if tx.send((key, columns)).is_err() {
                            worker.stop.store(true, Ordering::Relaxed);
                        }
                    });
                    worker.requests.lock().unwrap().current = None;
                    match result {
                        Ok(_) => {}
                        Err(error) => {
                            log::warn!(
                                "Horizon: far decoration failed at {} {}: {error}",
                                key.0,
                                key.1
                            );
                            worker.stop.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                }
            })
            .expect("far decoration worker");
        Self {
            shared,
            results,
            thread: Some(thread),
        }
    }
    fn request(&self, chunks: &[(i32, i32)], center: [i32; 2]) {
        if self.shared.stop.load(Ordering::Relaxed) {
            return;
        }
        let mut requests = self.shared.requests.lock().unwrap();
        requests.center = center;
        for &key in chunks {
            if requests.current != Some(batch_origin(key)) && !requests.finished.contains(&key) {
                requests.chunks.insert(key);
            }
        }
        if requests.chunks.len() > REQUESTS {
            let mut sorted: Vec<_> = requests.chunks.iter().copied().collect();
            sorted.sort_unstable_by_key(|&key| (distance(key, center), key));
            requests.chunks = sorted.into_iter().take(REQUESTS).collect();
        }
        self.shared.wake.notify_one();
    }
}
impl Drop for Builder {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.wake.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Share the two-chunk terrain halo and one-chunk feature halo across adjacent
/// targets. Emit each target only after all nine feature centers have run.
/// These proto chunks never enter World, lighting or normal save queues.
fn decorated_batch(
    worker: &mut GeneratorWorker,
    origin: (i32, i32),
    size: i32,
    stop: &AtomicBool,
    mut emit: impl FnMut(&ProtoChunk),
) -> Result<FxHashMap<(i32, i32), ProtoChunk>, GeneratorError> {
    // Each region starts afresh; flags from a previous speculative structure
    // placement must not suppress blocks in this region.
    worker.forget_structure_starts();
    let mut chunks = FxHashMap::default();
    for dx in -2..size + 2 {
        for dz in -2..size + 2 {
            if stop.load(Ordering::Relaxed) {
                return Ok(FxHashMap::default());
            }
            let key = (origin.0 + dx, origin.1 + dz);
            chunks.insert(key, worker.generate_proto(ChunkPos::new(key.0, key.1))?);
        }
    }
    let mut remaining: FxHashMap<_, u8> = (0..size)
        .flat_map(|x| (0..size).map(move |z| ((origin.0 + x, origin.1 + z), 9)))
        .collect();
    let center = (origin.0 + size / 2, origin.1 + size / 2);
    for (x, z) in vanilla_order(center, size / 2)
        .into_iter()
        .filter(|&(x, z)| {
            x >= origin.0 - 1 && x <= origin.0 + size && z >= origin.1 - 1 && z <= origin.1 + size
        })
    {
        if stop.load(Ordering::Relaxed) {
            return Ok(FxHashMap::default());
        }
        let mut region: [ProtoChunk; 9] = std::array::from_fn(|i| {
            chunks
                .remove(&(x + i as i32 % 3 - 1, z + i as i32 / 3 - 1))
                .expect("generated neighborhood")
        });
        worker.decorate(&mut region)?;
        for chunk in region {
            let pos = chunk.position();
            chunks.insert((pos.x, pos.z), chunk);
        }
        for dx in -1..=1 {
            for dz in -1..=1 {
                let key = (x + dx, z + dz);
                if let Some(left) = remaining.get_mut(&key) {
                    *left -= 1;
                    if *left == 0 {
                        emit(&chunks[&key]);
                    }
                }
            }
        }
    }
    assert!(
        remaining.values().all(|&left| left == 0),
        "incomplete far decoration halo"
    );
    Ok(chunks)
}

#[cfg(test)]
fn decorated_region(
    worker: &mut GeneratorWorker,
    center: (i32, i32),
    stop: &AtomicBool,
) -> Result<Option<ProtoChunk>, GeneratorError> {
    Ok(decorated_batch(worker, center, 1, stop, |_| {})?.remove(&center))
}
fn columns(chunk: &ProtoChunk, biome_map: &[u16], dimension: &str) -> Arc<[Column; 256]> {
    let states = chunk.data().block_states();
    let registry = chunk.data().registry();
    let min = chunk.terrain.min_y().max(MIN_Y);
    let max = (chunk.terrain.min_y() + chunk.terrain.height()).min(MIN_Y + HEIGHT);
    Arc::new(std::array::from_fn(|index| {
        let x = (index & 15) as i32;
        let z = (index >> 4) as i32;
        let mut column = Column::default();
        for y in min..max {
            let state = chunk.block(x, y, z);
            let properties = states.get(state);
            if properties.is_air()
                || (!properties.blocks_motion_in_heightmap() && !properties.has_fluid())
            {
                continue;
            }
            let name = registry.state(state).map_or("", |s| s.name.as_str());
            let fluid = matches!(name, "minecraft:water" | "minecraft:lava");
            let biome = biome_map
                .get(chunk.biome_id_at(x, y, z).0 as usize)
                .copied()
                .unwrap_or(0);
            // shortcut: no propagated far lighting, use the normal lighting
            // engine if accurate distant block-light propagation is required.
            let light = if dimension == "minecraft:the_nether" {
                0
            } else {
                240
            } | if name == "minecraft:lava" {
                15 << 12
            } else {
                0
            };
            column.push(Span {
                min: y as i16,
                max: (y + 1) as i16,
                state,
                biome,
                light,
            });
            if fluid {
                let surface = column.surface.get_or_insert(Surface {
                    height: MIN_Y,
                    state: BlockStateId(0),
                    water: None,
                    biome,
                });
                surface.water = Some((y + 1, state));
            } else {
                column.surface = Some(Surface {
                    height: y + 1,
                    state,
                    water: None,
                    biome,
                });
            }
        }
        column
    }))
}

pub(super) struct VanillaLodSampler {
    generator: Arc<VanillaTerrain>,
    worker: GeneratorWorker,
    builder: Option<Builder>,
    vegetation: Arc<Vegetation>,
    cache: FxHashMap<(i32, i32), Arc<[Column; 256]>>,
    order: VecDeque<(i32, i32)>,
    details: Vec<LodDetail>,
}
impl VanillaLodSampler {
    pub(super) fn new(generator: Arc<VanillaTerrain>) -> Self {
        let worker = generator.generator.worker();
        let builder = Some(Builder::new(generator.clone()));
        let vegetation = Arc::new(Vegetation::new(
            generator.generator.data(),
            &generator.biome_map,
            generator.seed,
        ));
        Self {
            generator,
            worker,
            builder,
            vegetation,
            cache: FxHashMap::default(),
            order: VecDeque::new(),
            details: Vec::new(),
        }
    }
}
impl LodSampler for VanillaLodSampler {
    fn fork(&self) -> Option<Box<dyn LodSampler>> {
        Some(Box::new(Self {
            generator: self.generator.clone(),
            worker: self.generator.generator.worker(),
            builder: None,
            vegetation: self.vegetation.clone(),
            cache: FxHashMap::default(),
            order: VecDeque::new(),
            details: Vec::new(),
        }))
    }
    fn request_detail(&mut self, chunks: &[(i32, i32)], center: [i32; 2]) {
        let mut missing = Vec::new();
        for &key in chunks {
            if let Some(columns) = self.cache.get(&key) {
                self.details.push((key, columns.clone()));
            } else {
                missing.push(key);
            }
        }
        if let Some(builder) = &self.builder {
            builder.request(&missing, center);
        }
    }
    fn drain_details(&mut self) -> Vec<LodDetail> {
        let Some(builder) = &self.builder else {
            return std::mem::take(&mut self.details);
        };
        while let Ok((key, columns)) = builder.results.try_recv() {
            if !self.cache.contains_key(&key) {
                self.order.push_back(key);
            }
            self.cache.insert(key, columns.clone());
            while self.cache.len() > CACHE {
                if let Some(old) = self.order.pop_front() {
                    self.cache.remove(&old);
                    builder
                        .shared
                        .requests
                        .lock()
                        .unwrap()
                        .finished
                        .remove(&old);
                }
            }
            self.details.push((key, columns));
        }
        std::mem::take(&mut self.details)
    }
    fn sample(&mut self, x: i32, z: i32) -> Option<Surface> {
        self.column(x, z).surface
    }
    fn column(&mut self, x: i32, z: i32) -> Column {
        self.reduced_column(x, z, 1)
    }
    fn reduced_column(&mut self, x: i32, z: i32, width: i32) -> Column {
        if width <= 16
            && let Some(columns) = self.cache.get(&(x >> 4, z >> 4))
        {
            return Column::reduced(
                columns,
                (x & 15) as usize,
                (z & 15) as usize,
                width as usize,
            );
        }
        self.grid(x, z, width, 1).pop().unwrap_or_default()
    }
    fn grid(&mut self, x: i32, z: i32, width: i32, side: usize) -> Vec<Column> {
        let samples = match self
            .worker
            .lod_grid(x + width / 2, z + width / 2, width, side)
        {
            Ok(samples) => samples,
            Err(error) => {
                log::warn!("Horizon: direct far sampling failed: {error}");
                return vec![Column::default(); side * side];
            }
        };
        samples
            .into_iter()
            .enumerate()
            .map(|(index, (surface, spans))| {
                let (wx, wz) = (
                    x + (index % side) as i32 * width,
                    z + (index / side) as i32 * width,
                );
                if width <= 16
                    && let Some(columns) = self.cache.get(&(wx >> 4, wz >> 4))
                {
                    return Column::reduced(
                        columns,
                        (wx & 15) as usize,
                        (wz & 15) as usize,
                        width as usize,
                    );
                }
                self.preview_column(wx + width / 2, wz + width / 2, width, surface, spans)
            })
            .collect()
    }
}
impl VanillaLodSampler {
    fn preview_column(
        &self,
        x: i32,
        z: i32,
        width: i32,
        surface: Option<SurfaceSample>,
        spans: Vec<(i32, i32, BlockStateId)>,
    ) -> Column {
        let surface = surface.map(|sample| self.generator.client_surface(sample));
        let biome = surface.map_or(0, |surface| surface.biome);
        let registry = self.generator.generator.data().registry();
        let mut column = Column {
            surface,
            spans: Vec::new(),
        };
        for (min, max, state) in spans.into_iter().rev() {
            column.push(Span {
                min: min as i16,
                max: max as i16,
                state,
                biome,
                light: if self.generator.dimension == "minecraft:the_nether" {
                    0
                } else {
                    240
                } | if registry
                    .state(state)
                    .is_some_and(|s| s.name == "minecraft:lava")
                {
                    15 << 12
                } else {
                    0
                },
            });
        }
        let light = if self.generator.dimension == "minecraft:the_nether" {
            0
        } else {
            240
        };
        self.vegetation.apply(&mut column, x, z, width, light);
        column
    }
}

/// The retained route never constructs the legacy Builder, including temporarily.
/// Its native decorator and every generated neighborhood remain group-owned.
#[cfg(feature = "paired-packages")]
mod retained {
    use super::*;
    use crate::lod::retained::{
        CheckedLodSampler, LodFailure, LodGroup, LodRole, LodStage, RetainedLodProvider,
    };

    struct Backend {
        requests: Mutex<Requests>,
        stop: Arc<AtomicBool>,
        wake: Arc<Condvar>,
        results: Mutex<VecDeque<LodDetail>>,
        error: Mutex<Option<String>>,
        /// Requests retired by the existing bounded nearest-first policy.
        retired: Mutex<Vec<(i32, i32)>>,
        #[cfg(test)]
        fail_after_proto: Mutex<Option<usize>>,
        #[cfg(test)]
        panic_after_proto: Mutex<Option<usize>>,
    }
    struct Batch {
        origin: (i32, i32),
        requested: Vec<(i32, i32)>,
        chunks: FxHashMap<(i32, i32), ProtoChunk>,
        /// Keep the actual mutable neighborhood outside the unwind frame.
        region: Vec<ProtoChunk>,
        remaining: FxHashMap<(i32, i32), u8>,
        emitted: Vec<LodDetail>,
    }
    impl Batch {
        fn new(origin: (i32, i32), requested: Vec<(i32, i32)>) -> Self {
            Self {
                origin,
                requested,
                chunks: FxHashMap::default(),
                region: Vec::with_capacity(9),
                remaining: (0..BATCH)
                    .flat_map(|x| (0..BATCH).map(move |z| ((origin.0 + x, origin.1 + z), 9)))
                    .collect(),
                emitted: Vec::new(),
            }
        }
    }
    struct Work {
        terrain: Arc<VanillaTerrain>,
        backend: Arc<Backend>,
        generator: Option<OverworldGenerator>,
        worker: Option<GeneratorWorker>,
        active: Option<Batch>,
        completed: Vec<((i32, i32), Vec<(i32, i32)>, usize)>,
    }
    struct Sampler {
        /// Always builder=None. The checked route never calls tolerant grid().
        inner: VanillaLodSampler,
        backend: Option<Arc<Backend>>,
        stop: Arc<AtomicBool>,
        #[cfg(test)]
        work: Option<Arc<Mutex<Work>>>,
        grid_input: Option<(i32, i32, i32, usize)>,
        raw_grid: Option<Vec<horizon_world::worldgen::generator::LodColumn>>,
        partial_grid: Vec<Column>,
    }
    impl RetainedLodProvider for VanillaTerrain {
        fn retained_sampler(
            self: Arc<Self>,
            group: Arc<LodGroup>,
        ) -> Result<Box<dyn CheckedLodSampler>, LodFailure> {
            Ok(Box::new(Sampler::new(self, group)?))
        }
    }
    impl Sampler {
        fn new(terrain: Arc<VanillaTerrain>, group: Arc<LodGroup>) -> Result<Self, LodFailure> {
            // One permanent admission fence is shared with every actor and
            // fork. Registry contention may delay a wakeup, never the fence.
            let stop = group.admission_fence();
            let wake = Arc::new(Condvar::new());
            group.register_stop(stop.clone(), wake.clone());
            let backend = Arc::new(Backend {
                requests: Mutex::new(Requests::default()),
                stop: stop.clone(),
                wake,
                results: Mutex::new(VecDeque::new()),
                error: Mutex::new(None),
                retired: Mutex::new(Vec::new()),
                #[cfg(test)]
                fail_after_proto: Mutex::new(None),
                #[cfg(test)]
                panic_after_proto: Mutex::new(None),
            });
            let work = Arc::new(Mutex::new(Work {
                terrain: terrain.clone(),
                backend: backend.clone(),
                generator: None,
                worker: None,
                active: None,
                completed: Vec::new(),
            }));
            // The group retains this original state even if native spawn fails.
            group.spawn(LodRole::Decoration, work.clone(), run)?;
            let inner = bare_sampler(terrain, None);
            Ok(Self {
                inner,
                backend: Some(backend),
                stop,
                #[cfg(test)]
                work: Some(work),
                grid_input: None,
                raw_grid: None,
                partial_grid: Vec::new(),
            })
        }
    }
    fn bare_sampler(
        generator: Arc<VanillaTerrain>,
        vegetation: Option<Arc<Vegetation>>,
    ) -> VanillaLodSampler {
        let worker = generator.generator.worker();
        let vegetation = vegetation.unwrap_or_else(|| {
            Arc::new(Vegetation::new(
                generator.generator.data(),
                &generator.biome_map,
                generator.seed,
            ))
        });
        VanillaLodSampler {
            generator,
            worker,
            builder: None,
            vegetation,
            cache: FxHashMap::default(),
            order: VecDeque::new(),
            details: Vec::new(),
        }
    }
    fn failure(message: impl Into<String>) -> LodFailure {
        LodFailure::new(LodStage::Decorate, message)
    }
    fn run(state: Arc<Mutex<Work>>) -> Result<(), LodFailure> {
        let backend = state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .backend
            .clone();
        let outcome = run_inner(&state, &backend);
        if let Err(error) = &outcome {
            *backend.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.message.clone());
            backend.stop.store(true, Ordering::Release);
            backend.wake.notify_all();
        }
        outcome
    }
    fn run_inner(state: &Arc<Mutex<Work>>, backend: &Backend) -> Result<(), LodFailure> {
        {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            if backend.stop.load(Ordering::Acquire) {
                return Ok(());
            }
            // This independent pipeline preserves structure one-shot flags in
            // gameplay's original generator. It is never reconstructed on Retry.
            state.generator = Some(
                OverworldGenerator::for_dimension(
                    state.terrain.generator.data().clone(),
                    state.terrain.seed,
                    "minecraft:normal",
                    &state.terrain.dimension,
                )
                .map_err(|e| failure(format!("decoration initialization: {e}")))?,
            );
            state.worker = Some(state.generator.as_ref().unwrap().worker());
        }
        loop {
            let (origin, requested) = {
                let mut requests = backend.requests.lock().unwrap_or_else(|e| e.into_inner());
                while requests.chunks.is_empty() && !backend.stop.load(Ordering::Acquire) {
                    // The timeout only rechecks a stop racing notification. It
                    // never supplies a successful generation/close result.
                    requests = backend
                        .wake
                        .wait_timeout(requests, std::time::Duration::from_millis(100))
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
                if backend.stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                let key = *requests
                    .chunks
                    .iter()
                    .min_by_key(|&&key| (distance(key, requests.center), key))
                    .unwrap();
                let origin = batch_origin(key);
                let selected: Vec<_> = requests
                    .chunks
                    .iter()
                    .copied()
                    .filter(|&key| batch_origin(key) == origin)
                    .collect();
                // Copy the request identity into the durable active state
                // before removing any original queued request.
                state.lock().unwrap_or_else(|e| e.into_inner()).active =
                    Some(Batch::new(origin, selected.clone()));
                for key in &selected {
                    requests.chunks.remove(key);
                }
                requests.current = Some(origin);
                (origin, selected)
            };
            let _ = (origin, requested); // identities live in the original Batch.
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            let Work {
                terrain,
                worker,
                active,
                completed,
                ..
            } = &mut *state;
            let batch = active.as_mut().unwrap();
            retained_batch(worker.as_mut().unwrap(), batch, terrain, backend)?;
            if backend.stop.load(Ordering::Acquire) {
                // Active maps and partial neighborhood remain owned as-is.
                return Ok(());
            }
            // Normal consumption retires voxel neighborhoods after all their
            // derived columns are published. Stopped/failed batches never take
            // this path and retain their actual proto/neighbor state.
            let batch = active.take().unwrap();
            completed.push((batch.origin, batch.requested, batch.emitted.len()));
            backend
                .requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .current = None;
        }
    }
    fn retained_batch(
        worker: &mut GeneratorWorker,
        batch: &mut Batch,
        terrain: &VanillaTerrain,
        backend: &Backend,
    ) -> Result<(), LodFailure> {
        worker.forget_structure_starts();
        let origin = batch.origin;
        for dx in -2..BATCH + 2 {
            for dz in -2..BATCH + 2 {
                if backend.stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                let key = (origin.0 + dx, origin.1 + dz);
                let chunk = worker
                    .generate_proto(ChunkPos::new(key.0, key.1))
                    .map_err(|e| failure(format!("proto {} {}: {e}", key.0, key.1)))?;
                batch.chunks.insert(key, chunk);
                #[cfg(test)]
                if *backend
                    .panic_after_proto
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    == Some(batch.chunks.len())
                {
                    panic!("injected retained decoration panic after native proto");
                }
                #[cfg(test)]
                if *backend
                    .fail_after_proto
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    == Some(batch.chunks.len())
                {
                    return Err(failure(
                        "injected retained decoration failure after native proto",
                    ));
                }
            }
        }
        let center = (origin.0 + BATCH / 2, origin.1 + BATCH / 2);
        for (x, z) in vanilla_order(center, BATCH / 2)
            .into_iter()
            .filter(|&(x, z)| {
                x >= origin.0 - 1
                    && x <= origin.0 + BATCH
                    && z >= origin.1 - 1
                    && z <= origin.1 + BATCH
            })
        {
            if backend.stop.load(Ordering::Acquire) {
                return Ok(());
            }
            for i in 0..9 {
                batch.region.push(
                    batch
                        .chunks
                        .remove(&(x + i % 3 - 1, z + i / 3 - 1))
                        .expect("generated retained neighborhood"),
                );
            }
            let region: &mut [ProtoChunk; 9] = batch
                .region
                .as_mut_slice()
                .try_into()
                .unwrap_or_else(|_| panic!("nine retained neighbors"));
            worker
                .decorate(region)
                .map_err(|e| failure(format!("features {x} {z}: {e}")))?;
            for chunk in batch.region.drain(..) {
                let pos = chunk.position();
                batch.chunks.insert((pos.x, pos.z), chunk);
            }
            for dx in -1..=1 {
                for dz in -1..=1 {
                    let key = (x + dx, z + dz);
                    if let Some(left) = batch.remaining.get_mut(&key) {
                        *left -= 1;
                        if *left == 0 {
                            let detail = (
                                key,
                                columns(
                                    &batch.chunks[&key],
                                    &terrain.biome_map,
                                    &terrain.dimension,
                                ),
                            );
                            batch.emitted.push(detail.clone());
                            backend
                                .results
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push_back(detail);
                            backend
                                .requests
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .finished
                                .insert(key);
                        }
                    }
                }
            }
        }
        if batch.remaining.values().any(|&left| left != 0) {
            return Err(failure("incomplete retained decoration halo"));
        }
        Ok(())
    }
    impl Sampler {
        fn check(&self) -> Result<(), LodFailure> {
            if let Some(backend) = &self.backend
                && let Some(error) = &*backend.error.lock().unwrap_or_else(|e| e.into_inner())
            {
                return Err(failure(error.clone()));
            }
            if self.stop.load(Ordering::Acquire) {
                return Err(failure("retained decoration admission closed"));
            }
            Ok(())
        }
        fn receive(&mut self) -> Result<(), LodFailure> {
            self.check()?;
            if let Some(backend) = &self.backend {
                let mut results = backend.results.lock().unwrap_or_else(|e| e.into_inner());
                while let Some((key, columns)) = results.pop_front() {
                    if !self.inner.cache.contains_key(&key) {
                        self.inner.order.push_back(key);
                    }
                    self.inner.cache.insert(key, columns.clone());
                    while self.inner.cache.len() > CACHE {
                        if let Some(old) = self.inner.order.pop_front() {
                            self.inner.cache.remove(&old);
                            backend
                                .requests
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .finished
                                .remove(&old);
                        }
                    }
                    self.inner.details.push((key, columns));
                }
            }
            self.check()
        }
    }
    impl CheckedLodSampler for Sampler {
        fn fork(&self) -> Result<Option<Box<dyn CheckedLodSampler>>, LodFailure> {
            self.check()?;
            Ok(Some(Box::new(Self {
                inner: bare_sampler(
                    self.inner.generator.clone(),
                    Some(self.inner.vegetation.clone()),
                ),
                backend: None,
                stop: self.stop.clone(),
                #[cfg(test)]
                work: None,
                grid_input: None,
                raw_grid: None,
                partial_grid: Vec::new(),
            })))
        }
        fn request_detail(
            &mut self,
            chunks: &[(i32, i32)],
            center: [i32; 2],
        ) -> Result<(), LodFailure> {
            self.receive()?;
            let Some(backend) = &self.backend else {
                return Ok(());
            };
            let mut requests = backend.requests.lock().unwrap_or_else(|e| e.into_inner());
            self.check()?;
            requests.center = center;
            for &key in chunks {
                if let Some(columns) = self.inner.cache.get(&key) {
                    self.inner.details.push((key, columns.clone()));
                } else if requests.current != Some(batch_origin(key))
                    && !requests.finished.contains(&key)
                {
                    requests.chunks.insert(key);
                }
            }
            if requests.chunks.len() > REQUESTS {
                let mut sorted: Vec<_> = requests.chunks.iter().copied().collect();
                sorted.sort_unstable_by_key(|&key| (distance(key, center), key));
                backend
                    .retired
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(sorted.iter().skip(REQUESTS).copied());
                requests.chunks = sorted.into_iter().take(REQUESTS).collect();
            }
            backend.wake.notify_all();
            Ok(())
        }
        fn drain_details(&mut self) -> Result<Vec<LodDetail>, LodFailure> {
            self.receive()?;
            Ok(std::mem::take(&mut self.inner.details))
        }
        fn grid(
            &mut self,
            x: i32,
            z: i32,
            width: i32,
            side: usize,
        ) -> Result<Vec<Column>, LodFailure> {
            self.check()?;
            self.grid_input = Some((x, z, width, side));
            self.partial_grid.clear();
            self.raw_grid = Some(
                self.inner
                    .worker
                    .lod_grid(x + width / 2, z + width / 2, width, side)
                    .map_err(|e| {
                        LodFailure::new(LodStage::Sample, format!("direct far {x} {z}: {e}"))
                    })?,
            );
            for (index, (surface, spans)) in self.raw_grid.as_ref().unwrap().iter().enumerate() {
                let (wx, wz) = (
                    x + (index % side) as i32 * width,
                    z + (index / side) as i32 * width,
                );
                let column = if width <= 16
                    && let Some(columns) = self.inner.cache.get(&(wx >> 4, wz >> 4))
                {
                    Column::reduced(
                        columns,
                        (wx & 15) as usize,
                        (wz & 15) as usize,
                        width as usize,
                    )
                } else {
                    self.inner.preview_column(
                        wx + width / 2,
                        wz + width / 2,
                        width,
                        *surface,
                        spans.clone(),
                    )
                };
                self.partial_grid.push(column);
            }
            self.check()?;
            self.raw_grid = None;
            self.grid_input = None;
            Ok(std::mem::take(&mut self.partial_grid))
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use std::time::{Duration, Instant};

        fn terrain() -> Arc<VanillaTerrain> {
            let root = std::path::PathBuf::from(
                std::env::var_os("HORIZON_TEST_RESOURCES").expect("pinned resource directory"),
            );
            let assets = crate::assets::Assets::load(&root).expect("real prepared assets");
            let biomes = crate::biome::Biomes::load(&root).expect("real prepared biomes");
            Arc::new(
                VanillaTerrain::new(&root.join("worldgen"), assets.registry, &biomes, 1234)
                    .expect("real prepared worldgen"),
            )
        }
        fn finish(group: &LodGroup) -> crate::lod::retained::LodObservation {
            group.request_stop();
            let deadline = Instant::now() + Duration::from_secs(90);
            loop {
                group.poll_finished();
                let observation = group.observe_workers();
                if observation.ready {
                    return observation;
                }
                assert!(
                    Instant::now() < deadline,
                    "native decorator did not actually finish/join"
                );
                thread::sleep(Duration::from_millis(2));
            }
        }
        #[test]
        #[ignore = "requires pinned prepared worldgen in HORIZON_TEST_RESOURCES"]
        fn retained_real_vanilla_checked_grid_and_fork() {
            let terrain = terrain();
            let group = LodGroup::for_test();
            let mut sampler =
                Sampler::new(terrain.clone(), group.clone()).expect("retained sampler");
            let mut fork = sampler
                .fork()
                .expect("checked fork")
                .expect("actual independent worker");
            let sampled = sampler.grid(-1024, 0, 32, 2).expect("checked native grid");
            let forked = fork.grid(-1024, 0, 32, 2).expect("forked native grid");
            let (entered, entered_rx) = mpsc::channel();
            let (release, release_rx) = mpsc::channel();
            let registry_group = group.clone();
            let registry_holder = thread::spawn(move || {
                registry_group.hold_registry_for_test(entered, release_rx);
            });
            entered_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("actual registry held");
            group.request_stop();
            // No poll can forward backend signals while this actual lock is
            // held. All these calls must see the authoritative fence directly.
            let same_fence = Arc::ptr_eq(&sampler.stop, &group.admission_fence())
                && Arc::ptr_eq(&sampler.stop, &sampler.backend.as_ref().unwrap().stop);
            let sealed = sampler.stop.load(Ordering::Acquire);
            let grid_refused = sampler.grid(0, 0, 32, 2).is_err();
            let fork_grid_refused = fork.grid(0, 0, 32, 2).is_err();
            let request_refused = sampler.request_detail(&[(99, 99)], [0, 0]).is_err();
            let fork_refused = sampler.fork().is_err();
            release.send(()).expect("release actual registry holder");
            registry_holder
                .join()
                .expect("checked registry holder join");
            let observation = finish(&group);
            assert!(
                same_fence && sealed,
                "backend uses the exact permanent group fence"
            );
            assert!(
                grid_refused && fork_grid_refused && request_refused && fork_refused,
                "registry contention cannot admit new native sampling or detail work after close"
            );
            assert_eq!(sampled, forked);
            assert!(
                observation
                    .workers
                    .iter()
                    .all(|worker| worker.joined && worker.failure.is_none())
            );
            assert!(Arc::ptr_eq(&sampler.inner.generator, &terrain));
            assert!(sampler.inner.builder.is_none());
            assert!(
                sampler.grid(0, 0, 32, 2).is_err(),
                "closed route cannot publish a sentinel"
            );
        }
        #[test]
        #[ignore = "requires pinned prepared worldgen in HORIZON_TEST_RESOURCES"]
        fn retained_real_decoration_request_keeps_completed_detail() {
            let group = LodGroup::for_test();
            let mut sampler = Sampler::new(terrain(), group.clone()).expect("retained sampler");
            let key = (-64, 0);
            sampler
                .request_detail(&[key], [-1024, 0])
                .expect("real detail request");
            let deadline = Instant::now() + Duration::from_secs(90);
            let backend = sampler.backend.as_ref().unwrap().clone();
            let produced = loop {
                let present = backend
                    .results
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .any(|(at, _)| *at == key);
                if present {
                    break true;
                }
                if backend.stop.load(Ordering::Acquire) || Instant::now() >= deadline {
                    break false;
                }
                thread::sleep(Duration::from_millis(2));
            };
            let observation = finish(&group);
            assert!(
                produced,
                "real decorated columns missing or native worker failed"
            );
            assert!(
                observation
                    .workers
                    .iter()
                    .all(|worker| worker.joined && worker.failure.is_none())
            );
            let results = backend.results.lock().unwrap_or_else(|e| e.into_inner());
            let columns = &results.iter().find(|(at, _)| *at == key).unwrap().1;
            assert!(columns.iter().any(|column| !column.spans.is_empty()));
            assert!(
                sampler.drain_details().is_err(),
                "late results stay owned after close"
            );
            assert!(results.iter().any(|(at, _)| *at == key));
        }
        #[test]
        #[ignore = "requires pinned prepared worldgen in HORIZON_TEST_RESOURCES"]
        fn retained_real_decoration_failure_preserves_original_active_batch() {
            for panic in [false, true] {
                let group = LodGroup::for_test();
                let mut sampler = Sampler::new(terrain(), group.clone()).expect("retained sampler");
                let backend = sampler.backend.as_ref().unwrap().clone();
                if panic {
                    *backend.panic_after_proto.lock().unwrap() = Some(1);
                } else {
                    *backend.fail_after_proto.lock().unwrap() = Some(1);
                }
                sampler
                    .request_detail(&[(0, 0)], [0, 0])
                    .expect("native request before failure");
                let deadline = Instant::now() + Duration::from_secs(90);
                while !group.is_stopping() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(2));
                }
                let had_failed = group.is_stopping();
                let observation = finish(&group);
                assert!(
                    had_failed,
                    "failure injection did not reach original native worker"
                );
                assert!(
                    observation
                        .workers
                        .iter()
                        .any(|worker| worker.failure.is_some())
                );
                {
                    let work = sampler
                        .work
                        .as_ref()
                        .unwrap()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let batch = work.active.as_ref().expect("failed active batch retained");
                    assert_eq!(
                        batch.chunks.len(),
                        1,
                        "actual first native proto remains owned"
                    );
                    assert_eq!(batch.requested, vec![(0, 0)]);
                    assert_eq!(batch.remaining.len(), (BATCH * BATCH) as usize);
                }
                assert!(
                    sampler.drain_details().is_err(),
                    "failed backend cannot become successful empty terrain"
                );
            }
        }
        #[test]
        #[ignore = "requires pinned prepared worldgen in HORIZON_TEST_RESOURCES"]
        fn retained_real_decorator_spawn_refusal_keeps_original_factory_input() {
            let terrain = terrain();
            let group = LodGroup::for_test();
            group.refuse_spawn(LodRole::Decoration);
            let outcome = Sampler::new(terrain.clone(), group.clone());
            let observation = finish(&group);
            let error = match outcome {
                Err(error) => error,
                Ok(_) => panic!("controlled actual decoration spawn must refuse"),
            };
            assert_eq!(error.stage, LodStage::Spawn);
            assert!(observation.ready);
            assert_eq!(observation.workers.len(), 1);
            assert_eq!(observation.workers[0].role, LodRole::Decoration);
            assert!(observation.workers[0].joined);
            assert_eq!(observation.workers[0].failure.as_ref(), Some(&error));
            let payloads = group.retained_payloads_for_test::<Work>();
            assert_eq!(payloads.len(), 1, "original actual decorator slot payload");
            let work = payloads[0].lock().unwrap_or_else(|e| e.into_inner());
            assert!(Arc::ptr_eq(&work.terrain, &terrain));
            assert!(work.generator.is_none() && work.worker.is_none());
            assert!(work.active.is_none() && work.completed.is_empty());
            assert!(work.backend.results.lock().unwrap().is_empty());
            assert!(work.backend.requests.lock().unwrap().chunks.is_empty());
        }
        #[test]
        #[ignore = "requires pinned prepared worldgen in HORIZON_TEST_RESOURCES"]
        fn retained_real_decorator_initialization_failure_is_checked() {
            let mut terrain = terrain();
            Arc::get_mut(&mut terrain).unwrap().dimension = "minecraft:missing_dimension".into();
            let group = LodGroup::for_test();
            let mut sampler =
                Sampler::new(terrain, group.clone()).expect("registered native decorator");
            let deadline = Instant::now() + Duration::from_secs(90);
            while !group.is_stopping() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(2));
            }
            let failed = group.is_stopping();
            let observation = finish(&group);
            assert!(failed, "actual invalid dimension must fail initialization");
            let message = observation
                .workers
                .iter()
                .find_map(|worker| worker.failure.as_ref())
                .expect("checked native initialization failure")
                .message
                .clone();
            assert!(message.contains("minecraft:missing_dimension"));
            assert!(sampler.drain_details().is_err());
            assert!(sampler.grid(0, 0, 32, 2).is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires prepared worldgen data in HORIZON_TEST_RESOURCES"]
    fn decorated_lod_refines_real_seed_canopies_and_all_dimensions() -> crate::Result<()> {
        let root = std::path::PathBuf::from(
            std::env::var_os("HORIZON_TEST_RESOURCES").expect("resources"),
        );
        let assets = crate::assets::Assets::load(&root)?;
        let biomes = crate::biome::Biomes::load(&root)?;
        let terrain = Arc::new(VanillaTerrain::new(
            &root.join("worldgen"),
            assets.registry,
            &biomes,
            1234,
        )?);
        let mut sampler = VanillaLodSampler::new(terrain.clone());
        let mut fork = sampler.fork().expect("independent far sampler");
        let direct = sampler.grid(-1024, 0, 32, 16);
        assert_eq!(
            direct,
            fork.grid(-1024, 0, 32, 16),
            "sampling workers must agree on seed/biome vegetation"
        );
        let grass = assets
            .registry
            .default_state("minecraft:grass_block")
            .unwrap();
        let planted = |biome, width, x, z| {
            let mut column = Column::from_surface(Some(Surface {
                height: 80,
                state: grass,
                water: None,
                biome,
            }));
            sampler.vegetation.apply(&mut column, x, z, width, 240);
            column
        };
        let forest = biomes.id("minecraft:forest");
        let plains = biomes.id("minecraft:plains");
        let leaves = |column: &Column| {
            column.spans.iter().any(|span| {
                assets
                    .registry
                    .state(span.state)
                    .unwrap()
                    .name
                    .ends_with("_leaves")
            })
        };
        let mut forest_count = 0;
        let mut plains_count = 0;
        for z in 0..64 {
            for x in 0..64 {
                forest_count += usize::from(leaves(&planted(forest, 1, x, z)));
                plains_count += usize::from(leaves(&planted(plains, 1, x, z)));
            }
        }
        assert!(
            forest_count > 1000 && forest_count > plains_count * 4,
            "feature-derived vegetation density: forest={forest_count}, plains={plains_count}"
        );
        for width in [16, 32, 128, 512, 1024] {
            assert!(
                leaves(&planted(forest, width, 0, 0)),
                "forest canopy missing at scale {width}"
            );
        }
        let key = (-64, 0);
        sampler.request_detail(&[key], [-1024, 0]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let generated = loop {
            if let Some((_, columns)) = sampler.drain_details().into_iter().find(|(k, _)| *k == key)
            {
                break columns;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "decoration worker stalled"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let count = |suffix: &str| {
            generated
                .iter()
                .flat_map(|column| &column.spans)
                .filter(|span| {
                    assets
                        .registry
                        .state(span.state)
                        .unwrap()
                        .name
                        .ends_with(suffix)
                })
                .map(|span| i32::from(span.max - span.min))
                .sum::<i32>()
        };
        assert!(count("_leaves") > 0, "unexplored forest has a real canopy");
        assert!(count("_log") > 0, "canopy retains its trunks");
        assert!(generated.iter().all(|column| {
            column
                .spans
                .windows(2)
                .all(|pair| pair[0].max <= pair[1].min)
        }));
        for z in 0..16 {
            for x in 0..16 {
                assert_eq!(
                    sampler.column(key.0 * 16 + x, key.1 * 16 + z),
                    generated[(z * 16 + x) as usize]
                );
            }
        }
        sampler.request_detail(&[key], [-1024, 0]);
        let cached = sampler.drain_details();
        assert!(
            Arc::ptr_eq(&cached[0].1, &generated),
            "reuses completed compact data"
        );
        drop(sampler);
        let mut worker = terrain.generator.worker();
        let started = std::time::Instant::now();
        for x in 0..BATCH {
            for z in 0..BATCH {
                decorated_region(&mut worker, (key.0 + x, key.1 + z), &AtomicBool::new(false))?
                    .unwrap();
            }
        }
        let separate = started.elapsed();
        let started = std::time::Instant::now();
        let mut emitted = FxHashMap::default();
        let region = decorated_batch(&mut worker, key, BATCH, &AtomicBool::new(false), |chunk| {
            let pos = chunk.position();
            assert!(
                emitted
                    .insert(
                        (pos.x, pos.z),
                        columns(chunk, &terrain.biome_map, "minecraft:overworld")
                    )
                    .is_none()
            );
        })?;
        eprintln!(
            "far generation: {} separate targets={separate:?}, shared targets={:?}",
            emitted.len(),
            started.elapsed()
        );
        assert_eq!(emitted.len(), (BATCH * BATCH) as usize);
        for (key, data) in emitted {
            assert_eq!(
                data,
                columns(&region[&key], &terrain.biome_map, "minecraft:overworld"),
                "emitted before all neighbouring features completed"
            );
        }
        assert_eq!(batch_origin((-1, -5)), (-4, -8));
        let stop = AtomicBool::new(true);
        assert!(decorated_region(&mut terrain.generator.worker(), key, &stop)?.is_none());
        for (dimension, center) in [
            ("minecraft:the_nether", (0, 0)),
            ("minecraft:the_end", (16, 0)),
        ] {
            let generator = OverworldGenerator::for_dimension(
                terrain.generator.data().clone(),
                1234,
                "minecraft:normal",
                dimension,
            )?;
            let mut emitted = 0;
            let mut region = decorated_batch(
                &mut generator.worker(),
                center,
                BATCH,
                &AtomicBool::new(false),
                |_| emitted += 1,
            )?;
            assert_eq!(emitted, (BATCH * BATCH) as usize);
            let chunk = region.remove(&center).unwrap();
            let data = columns(&chunk, &terrain.biome_map, dimension);
            if dimension == "minecraft:the_nether" {
                assert!(
                    data.iter()
                        .flat_map(|c| &c.spans)
                        .all(|s| s.light & 0xf0 == 0)
                );
                assert!(data.iter().flat_map(|c| &c.spans).any(|s| {
                    assets.registry.state(s.state).unwrap().name == "minecraft:lava"
                        && s.light >> 12 == 15
                }));
            } else {
                assert!(
                    data.iter().all(|c| c.spans.is_empty()),
                    "End void stays empty"
                );
            }
        }
        // Reuse the saved-proto bastion fixture. Repeated speculative regions
        // must reconstruct structures after their one-shot placement flags ran.
        let generator = OverworldGenerator::for_dimension(
            terrain.generator.data().clone(),
            -1860902098141295193,
            "minecraft:normal",
            "minecraft:the_nether",
        )?;
        let mut worker = generator.worker();
        let mut previous = None;
        for _ in 0..2 {
            let chunk = decorated_region(&mut worker, (3, 7), &AtomicBool::new(false))?.unwrap();
            assert!(
                chunk
                    .structures()
                    .iter()
                    .any(|(name, _, _, _)| name.contains("bastion"))
            );
            let data = columns(&chunk, &terrain.biome_map, "minecraft:the_nether");
            let snapshot = (data, chunk.block_entities);
            if let Some(previous) = &previous {
                assert_eq!(
                    &snapshot, previous,
                    "speculative structure state leaked between regions"
                );
            }
            previous = Some(snapshot);
        }
        Ok(())
    }
}
