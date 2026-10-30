//! Prepare local worlds without blocking the window's event loop.
use super::*;
pub(super) mod host_identity;
use host_identity::{IdentityIntent, WorldSession};

pub(super) struct WorldRequest {
    pub save_root: PathBuf,
    pub folder: String,
    pub name: Option<String>,
    pub seed: Option<i64>,
    pub creative: bool,
    pub create: Option<horizon_client::title::WorldOptions>,
    identity_intent: IdentityIntent,
    #[cfg(feature = "paired-packages")]
    authority_namespace: Option<host_identity::session_authority::AuthorityNamespace>,
    #[cfg(feature = "paired-packages")]
    expected_checkpoint_world: Option<horizon_client::world::counter_restore::WorldId>,
}

pub(super) struct PreparedWorld {
    pub save_root: PathBuf,
    pub level: Level,
    pub world: World,
    pub streaming: Streaming,
    pub save: Arc<SaveDir>,
    pub session_lock: WorldSession,
    pub rand_sequences: horizon_client::random_sequences::RandomSequences,
    pub locator: Option<Arc<VanillaTerrain>>,
    #[cfg(feature = "paired-packages")]
    pub native_checkpoint_origin: Option<NativeCheckpointOrigin>,
}

/// Factory-issued provenance for the actual built-in native TestTerrain provider.
/// This owner cannot be cloned or reconstructed from options or identity bytes.
#[cfg(feature = "paired-packages")]
pub(in crate::game) struct NativeCheckpointOrigin {
    root: PathBuf,
    dimension: Dim,
    save: std::sync::Weak<SaveDir>,
}
#[cfg(feature = "paired-packages")]
impl NativeCheckpointOrigin {
    pub(in crate::game) fn validate(
        &self,
        root: &std::path::Path,
        dimension: Dim,
        save: &Arc<SaveDir>,
    ) -> Result<()> {
        if self.root != root
            || self.dimension != dimension
            || dimension != Dim::Overworld
            || !self
                .save
                .upgrade()
                .is_some_and(|owner| Arc::ptr_eq(&owner, save))
            || save.root != horizon_client::save::dimension_dir(root, dimension.id())
        {
            return Err("native session checkpoint provider provenance mismatch".into());
        }
        Ok(())
    }
}

impl WorldRequest {
    pub fn new(
        folder: &str,
        name: Option<&str>,
        seed: Option<i64>,
        creative: bool,
        create: Option<&horizon_client::title::WorldOptions>,
    ) -> Result<Self> {
        Ok(Self {
            save_root: std::env::current_dir()?.join("saves").join(folder),
            folder: folder.to_owned(),
            name: name.map(str::to_owned),
            seed,
            creative,
            create: create.cloned(),
            identity_intent: IdentityIntent::Legacy,
            #[cfg(feature = "paired-packages")]
            authority_namespace: None,
            #[cfg(feature = "paired-packages")]
            expected_checkpoint_world: None,
        })
    }

    pub(super) fn with_identity_intent(mut self, intent: IdentityIntent) -> Self {
        self.identity_intent = intent;
        self
    }

    /// Internal opt-in for genuinely new managed worlds. Reopen uses the archive.
    #[cfg(feature = "paired-packages")]
    #[allow(dead_code)]
    pub(in crate::game) fn with_session_authority(
        mut self,
        namespace: host_identity::session_authority::AuthorityNamespace,
    ) -> Self {
        self.authority_namespace = Some(namespace);
        self
    }

    #[cfg(feature = "paired-packages")]
    pub(in crate::game) fn with_expected_checkpoint_world(
        mut self,
        expected: horizon_client::world::counter_restore::WorldId,
    ) -> Self {
        self.identity_intent = IdentityIntent::RequireExistingIdentity;
        self.expected_checkpoint_world = Some(expected);
        self
    }

    pub fn prepare(
        self,
        res: &Resources,
        pack: &PackResources,
        options: &Options,
        settings: &Settings,
    ) -> Result<PreparedWorld> {
        #[cfg(feature = "paired-packages")]
        if let Some(namespace) = &self.authority_namespace {
            if self.identity_intent != IdentityIntent::CreateNewIdentity {
                return Err("managed reopen requires authoritative archive recovery".into());
            }
            namespace.check_new_world(&self.save_root)?;
        }
        #[cfg(feature = "paired-packages")]
        if self.expected_checkpoint_world.is_some()
            && self.identity_intent != IdentityIntent::RequireExistingIdentity
        {
            return Err("checkpoint recovery requires existing identified native world".into());
        }
        options.mod_packages.check_world(&self.save_root)?;
        #[cfg(feature = "paired-packages")]
        let identity_preflight =
            host_identity::IdentityPreflight::read(&self.save_root, self.identity_intent)?;
        // Before any chunk is read: a loaded container with a loot table is rolled as its chunk
        // is decoded (`save::roll_loaded_loot`), and the spawn chunks are read while the world
        // opens. Every world opens here: in-frame, on the server thread, on a dedicated server.
        horizon_client::structure_blocks::loot_data(&options.resources);
        let Self {
            save_root,
            folder,
            name,
            seed,
            creative,
            create,
            identity_intent,
            #[cfg(feature = "paired-packages")]
            authority_namespace,
            #[cfg(feature = "paired-packages")]
            expected_checkpoint_world,
        } = self;
        let name = name.as_deref();
        let create = create.as_ref();
        #[allow(unused_mut)] // Binding exists only with paired-packages.
        let mut session_lock;
        #[cfg(feature = "paired-packages")]
        if let Some(preflight) = identity_preflight {
            // Opt-in preflight ran before native acquisition could follow or
            // truncate a lock target. The same facts are checked under lock.
            let lock = horizon_client::save::SessionLock::acquire(&save_root)?;
            preflight.recheck(&save_root)?;
            let mut authority = authority_namespace
                .map(|namespace| preflight.acquire_authority(namespace, &save_root))
                .transpose()?;
            if let Some(expected) = expected_checkpoint_world {
                preflight.require_checkpoint_world(expected)?;
            }
            options.mod_packages.check_world(&save_root)?;
            // Package v1 FIRST: an anchor alone would make a paired new root
            // look like an unmarked existing world. Failures retain artifacts.
            options.mod_packages.record_world(&save_root)?;
            session_lock = preflight.finish(&save_root, lock)?;
            if authority.is_some() {
                session_lock.install_authority(&mut authority)?;
            }
        } else {
            horizon_client::save::upgrade_older_world(&save_root)?;
            horizon_client::save::migrate_legacy(&save_root)?;
            let lock = horizon_client::save::SessionLock::acquire(&save_root)?;
            options.mod_packages.record_world(&save_root)?;
            session_lock = WorldSession::legacy(lock);
        }
        #[cfg(not(feature = "paired-packages"))]
        {
            horizon_client::save::upgrade_older_world(&save_root)?;
            horizon_client::save::migrate_legacy(&save_root)?;
            let lock = horizon_client::save::SessionLock::acquire(&save_root)?;
            options.mod_packages.record_world(&save_root)?;
            session_lock = WorldSession::legacy(lock);
        }
        // RandomSequences: the world's data/minecraft/random_sequences.dat.
        let rand_sequences =
            horizon_client::save::read_dat(&save_root.join("data/minecraft/random_sequences.dat"))
                .and_then(|t| t.get("data").cloned())
                .map(|d| horizon_client::random_sequences::RandomSequences::from_nbt(&d))
                .unwrap_or_default();
        let existing = Level::read(&save_root);
        if identity_intent.requires_existing() && existing.is_none() {
            return Err("required host identity reopen: native Level could not be read; fresh-world fallback refused".into());
        }
        let created = existing.is_none();
        let level = existing.unwrap_or_else(|| Level {
            name: name.unwrap_or(&folder).to_owned(),
            // WorldOptions.randomSeed when no seed was given.
            seed: seed.unwrap_or_else(|| {
                let t = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos() as u64);
                (t ^ (t >> 29)).wrapping_mul(0x9E37_79B9_7F4A_7C15) as i64
            }),
            creative,
            day_time: 0,
            health: 20.0,
            food: 20,
            saturation: 5.0,
            ..Level::default()
        });
        let mut level = level;
        // CreateWorldScreen's choices, before streaming reads the generator.
        if created && let Some(o) = create {
            level.hardcore = o.hardcore;
            level.difficulty = horizon_client::title::DIFFICULTIES[o.difficulty()].into();
            level.allow_commands = Some(o.commands(creative));
            horizon_client::save::write_world_gen_settings_with(
                &save_root,
                level.seed,
                o.world_gen(),
                o.flat_settings(),
            )?;
            // WorldCreationUiState.getGameRules, written with the level.
            level.game_rules = o.rules.by_name();
        }
        // Worlds made without the Create New World screen (--world, the
        // control socket) take its default: commands on for Creative only.
        if created {
            level.allow_commands.get_or_insert(creative);
        }
        if let Some(d) = &options.dimension {
            level.dimension = d.clone();
            level.position = None;
        }
        let dim = Dim::from_id(&level.dimension);
        let NativeStreaming {
            mut streaming,
            save,
            locator,
            #[cfg(feature = "paired-packages")]
            native_checkpoint_origin,
        } = make_native_streaming(res, pack, options, settings, &save_root, dim, level.seed)?;
        #[cfg(feature = "paired-packages")]
        let native_checkpoint_origin = native_checkpoint_origin
            .filter(|_| session_lock.expected_world().is_some() && dim == Dim::Overworld);
        streaming.radius = settings.render_distance;
        let mut world = World::new(res.facts().air, level.seed);
        #[cfg(feature = "paired-packages")]
        session_lock.bind_initial_counter_world(&mut world, dim.id())?;
        world.fast_lava = dim == Dim::Nether;
        world.game_time = level.time;
        world.day_time = options.time.unwrap_or(level.day_time);
        world.weather = level.weather;
        if let Some(w) = options.weather.as_deref() {
            // /weather: clear, rain or thunder for a long duration.
            let (rain, thunder) = (w != "clear", w == "thunder");
            world.weather = horizon_client::world::Weather {
                clear_time: if w == "clear" { 6000 } else { 0 },
                rain_time: 6000,
                thunder_time: 6000,
                raining: rain,
                thundering: thunder,
                rain_level: if rain { (1.0, 1.0) } else { (0.0, 0.0) },
                thunder_level: if thunder { (1.0, 1.0) } else { (0.0, 0.0) },
            };
        }

        growth::install_live_features(res, pack, options);

        // Persist new metadata on the worker too, before publishing the world.
        if !save_root.join("level.dat").exists() {
            level.write(&save_root)?;
        }
        Ok(PreparedWorld {
            save_root,
            level,
            world,
            streaming,
            save,
            session_lock,
            rand_sequences,
            locator,
            #[cfg(feature = "paired-packages")]
            native_checkpoint_origin,
        })
    }
}

#[cfg(all(test, feature = "paired-packages", target_os = "linux"))]
#[path = "world_loading/host_identity_startup_tests.rs"]
mod host_identity_startup_tests;

pub(super) struct WorldLoader {
    pub cancelled: bool,
    handle: JoinHandle<std::result::Result<PreparedWorld, String>>,
}

impl WorldLoader {
    pub fn start(
        request: WorldRequest,
        res: Arc<Resources>,
        pack: PackResources,
        options: Options,
        settings: Settings,
    ) -> Result<Self> {
        let handle = std::thread::Builder::new()
            .name("World initialization".into())
            .spawn(move || {
                request
                    .prepare(&res, &pack, &options, &settings)
                    .map_err(|e| e.to_string())
            })?;
        Ok(Self {
            cancelled: false,
            handle,
        })
    }

    // Wait for construction and disposal off the window thread. Keeping this
    // handle prevents a queued reopen racing the old session lock.
    pub fn cancel(self) -> Self {
        Self {
            cancelled: true,
            handle: std::thread::spawn(move || {
                drop(self.finish());
                Err("world initialization cancelled".into())
            }),
        }
    }

    pub fn ready(&self) -> bool {
        self.handle.is_finished()
    }

    pub fn finish(self) -> Result<PreparedWorld> {
        self.handle
            .join()
            .map_err(|_| "world initialization worker panicked")?
            .map_err(Into::into)
    }
}

/// How long the preloaded world data outlives the world screens: a player
/// who goes back to the title or to Multiplayer and opens no world for this
/// long gets the memory back.
const PRELOAD_KEPT: std::time::Duration = std::time::Duration::from_secs(60);

enum Preload {
    Idle,
    /// Read, or being read, on the "World data preload" thread. `left`: when
    /// the player last left the world screens.
    Started {
        thread: std::thread::JoinHandle<()>,
        left: Option<std::time::Instant>,
    },
    /// A world opened: the data is the world's now, and stays.
    Used,
}

static PRELOAD: std::sync::Mutex<Preload> = std::sync::Mutex::new(Preload::Idle);

/// Each frame of the title: read what every world opening loads and keeps
/// for the rest of the process (the worldgen bundle, the loot tables and the
/// registries the server sends) on a helper thread while the player is on the
/// world screens (`on_world_screens`: Singleplayer, Create World and their
/// dialogs), so it overlaps the time spent choosing a world. A world opened
/// meanwhile waits for the part being read instead of reading it again. Not
/// at the title screen itself: many players only go to Multiplayer, and
/// small machines need the memory. Dropped after `PRELOAD_KEPT` away from
/// the world screens without a world opening.
pub(super) fn preload_world_data(
    on_world_screens: bool,
    resources: &std::path::Path,
    registry: &'static horizon_world::registry::BlockRegistry,
) {
    let mut preload = PRELOAD.lock().unwrap_or_else(|e| e.into_inner());
    match &mut *preload {
        Preload::Used => {}
        Preload::Idle if on_world_screens => {
            let resources = resources.to_owned();
            let spawned = std::thread::Builder::new()
                .name("World data preload".into())
                .spawn(move || {
                    horizon_threads::behind_server();
                    // Errors are left for the world opening to report.
                    let _ = horizon_client::vanillagen::shared_data(
                        &resources.join("worldgen"),
                        registry,
                    );
                    let _ = horizon_client::structure_blocks::loot_data(&resources);
                    let _ = super::lan::sync_data(&resources);
                });
            match spawned {
                Ok(thread) => *preload = Preload::Started { thread, left: None },
                Err(e) => log::warn!("Horizon: world data preload: {e}"),
            }
        }
        Preload::Idle => {}
        Preload::Started { left, .. } if on_world_screens => *left = None,
        Preload::Started { thread, left } => {
            let left = *left.get_or_insert_with(std::time::Instant::now);
            if left.elapsed() >= PRELOAD_KEPT && thread.is_finished() {
                horizon_client::vanillagen::forget_shared_data();
                horizon_client::structure_blocks::forget_loot_data();
                super::lan::forget_sync_data();
                horizon_threads::release_free_memory();
                *preload = Preload::Idle;
                log::info!("Horizon: preloaded world data dropped, no world opened");
            }
        }
    }
}

/// A world opens: what `preload_world_data` read is the world's from now on.
pub(super) fn world_data_used() {
    *PRELOAD.lock().unwrap_or_else(|e| e.into_inner()) = Preload::Used;
}

pub(super) fn make_streaming(
    res: &Resources,
    pack: &PackResources,
    options: &Options,
    settings: &Settings,
    world_root: &std::path::Path,
    dim: Dim,
    seed: i64,
) -> Result<(Streaming, Arc<SaveDir>, Option<Arc<VanillaTerrain>>)> {
    let native = make_native_streaming(res, pack, options, settings, world_root, dim, seed)?;
    Ok((native.streaming, native.save, native.locator))
}

struct NativeStreaming {
    streaming: Streaming,
    save: Arc<SaveDir>,
    locator: Option<Arc<VanillaTerrain>>,
    #[cfg(feature = "paired-packages")]
    native_checkpoint_origin: Option<NativeCheckpointOrigin>,
}

fn make_native_streaming(
    res: &Resources,
    pack: &PackResources,
    options: &Options,
    settings: &Settings,
    world_root: &std::path::Path,
    dim: Dim,
    seed: i64,
) -> Result<NativeStreaming> {
    let save = Arc::new(SaveDir::new(horizon_client::save::dimension_dir(
        world_root,
        dim.id(),
    )));
    let mut locator = None;
    #[cfg(feature = "paired-packages")]
    let mut native_checkpoint_origin = None;
    let flat = (dim == Dim::Overworld)
        .then(|| horizon_client::save::world_gen_settings(world_root))
        .flatten()
        .and_then(|s| {
            horizon_client::flatgen::FlatTerrain::from_settings(
                &s,
                pack.assets.registry,
                res.biomes(),
            )
        });
    let mut streaming = if let Some(inner) = flat {
        Streaming::new(
            Arc::new(Persistent {
                inner: Arc::new(inner),
                save: save.clone(),
                registry: pack.assets.clone(),
                biomes: res.biomes().clone(),
            }),
            pack.context.clone(),
            res.threads(),
        )
    } else if options.test_terrain && dim == Dim::Overworld {
        let inner = Arc::new(TestTerrain::new(pack.assets.registry, res.biomes())?);
        #[cfg(feature = "paired-packages")]
        {
            native_checkpoint_origin = Some(NativeCheckpointOrigin {
                root: world_root.to_path_buf(),
                dimension: dim,
                save: Arc::downgrade(&save),
            });
        }
        Streaming::new(
            Arc::new(Persistent {
                inner,
                save: save.clone(),
                registry: pack.assets.clone(),
                biomes: res.biomes().clone(),
            }),
            pack.context.clone(),
            res.threads(),
        )
    } else {
        // WorldOptions.generateStructures, from the world's settings.
        let structures =
            horizon_client::save::WorldGen::read(world_root).is_none_or(|w| w.structures);
        let inner = Arc::new(VanillaTerrain::for_world(&options.resources.join("worldgen"), pack.assets.registry, res.biomes(), seed, dim.id(), structures).map_err(|e| {
                format!("{e}\nPrepare the worldgen bundle with the launcher, or: horizon-prepare --prepare")
            })?);
        if dim == Dim::Overworld && structures {
            inner.search_rings_behind();
        }
        locator = Some(inner.clone());
        Streaming::new(
            Arc::new(Persistent {
                inner,
                save: save.clone(),
                registry: pack.assets.clone(),
                biomes: res.biomes().clone(),
            }),
            pack.context.clone(),
            res.threads(),
        )
    };
    streaming.forced = save.load_forced_chunks();
    streaming.radius = settings.render_distance;
    streaming.biome_blend = settings.biome_blend;
    streaming.smooth_lighting = settings.smooth_lighting;
    streaming.cutout_leaves = settings.cutout_leaves;
    streaming.biome_zoom = BiomeZoom::new(obfuscate_seed(seed));
    streaming.sky_light = dim != Dim::Nether;
    streaming.nether_shading = dim == Dim::Nether;
    // A server draws nothing: neither the integrated server nor the
    // dedicated one meshes sections.
    streaming.mesh = options.execution_role.is_client();
    Ok(NativeStreaming {
        streaming,
        save,
        locator,
        #[cfg(feature = "paired-packages")]
        native_checkpoint_origin,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polling_does_not_wait_for_world_initialization() {
        let (release, wait) = std::sync::mpsc::channel();
        let loader = WorldLoader {
            cancelled: false,
            handle: std::thread::spawn(move || {
                wait.recv().unwrap();
                Err("test world failure".into())
            }),
        };
        assert!(!loader.ready());
        release.send(()).unwrap();
        assert_eq!(
            loader.finish().err().unwrap().to_string(),
            "test world failure"
        );
    }

    #[test]
    fn cancellation_keeps_initialization_tracked_until_it_has_stopped() {
        let (release, wait) = std::sync::mpsc::channel();
        let loader = WorldLoader {
            cancelled: false,
            handle: std::thread::spawn(move || {
                wait.recv().unwrap();
                Err("cancelled test world".into())
            }),
        }
        .cancel();
        assert!(loader.cancelled);
        assert!(!loader.ready());
        release.send(()).unwrap();
        assert_eq!(
            loader.finish().err().unwrap().to_string(),
            "world initialization cancelled"
        );
    }

    #[test]
    fn worker_panics_reach_the_client() {
        let loader = WorldLoader {
            cancelled: false,
            handle: std::thread::spawn(|| panic!("broken world initialization")),
        };
        assert_eq!(
            loader.finish().err().unwrap().to_string(),
            "world initialization worker panicked"
        );
    }

    #[test]
    fn prepared_world_can_cross_threads() {
        fn send<T: Send>() {}
        send::<PreparedWorld>();
    }
}
