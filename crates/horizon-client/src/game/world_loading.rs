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
    identity: Arc<()>,
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

/// Issued only with the successful cold factory's original native inputs.
/// This permits the three fixed C0d close projections, never world mutation or
/// guest/domain authority. Neither observations nor a Game can mint this owner.
#[cfg(feature = "paired-packages")]
pub(super) struct NativeCloseAuthority {
    save: std::sync::Weak<SaveDir>,
    world: horizon_client::world::counter_restore::WorldId,
    root: PathBuf,
    dimension: Dim,
    level_dimension: String,
    origin: std::sync::Weak<()>,
}

#[cfg(feature = "paired-packages")]
impl NativeCloseAuthority {
    pub(super) fn authorizes(&self, game: &Game<'_>) -> bool {
        game.options.execution_role.is_client()
            && game.options.test_terrain
            && !game.options.server_thread
            && game.options.server.is_none()
            && game.options.dedicated.is_none()
            && game.options.dimension.is_none()
            && game.remote.is_none()
            && game.integrated.is_none()
            && game.lan.is_none()
            && game.serving.is_none()
            && game.lan_acting.is_none()
            && game.world_root == self.root
            && game.dim == self.dimension
            && game.level.dimension == self.level_dimension
            && self.dimension == Dim::Overworld
            && game
                .session_lock
                .as_ref()
                .and_then(WorldSession::expected_world)
                == Some(&self.world)
            && self
                .save
                .upgrade()
                .is_some_and(|save| Arc::ptr_eq(&save, &game.save))
            && game
                .native_checkpoint_origin
                .as_ref()
                .is_some_and(|origin| {
                    self.origin
                        .upgrade()
                        .is_some_and(|identity| Arc::ptr_eq(&identity, &origin.identity))
                        && origin
                            .validate(&self.root, self.dimension, &game.save)
                            .is_ok()
                })
    }
}

/// Joint, non-Clone transfer into the private initial-Game recipe. Borrowing its
/// prepared view cannot separate or reconstruct the close authority.
#[cfg(feature = "paired-packages")]
pub(super) struct ColdPreparedWorld {
    prepared: PreparedWorld,
    authority: NativeCloseAuthority,
}

#[cfg(feature = "paired-packages")]
impl ColdPreparedWorld {
    pub(super) fn prepared(&self) -> &PreparedWorld {
        &self.prepared
    }

    pub(super) fn into_parts(self) -> (PreparedWorld, NativeCloseAuthority) {
        (self.prepared, self.authority)
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

/// The retained route is deliberately separate from ordinary world entry.
/// A failure and a ready result carry this same lexical resource owner.
#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
pub(super) struct PrepareOwner {
    armed: bool,
    request: WorldRequest,
    native_acquisition: Option<horizon_client::save::RetainedSessionLock>,
    preflight: Option<host_identity::IdentityPreflight>,
    authority_acquisition: Option<host_identity::session_authority::RetainedAuthorityAcquisition>,
    authority: Option<host_identity::session_authority::WorldAuthority>,
    session: Option<WorldSession>,
    save: Option<Arc<SaveDir>>,
    streaming: Option<Streaming>,
    level: Option<Level>,
    world: Option<Box<World>>,
    rand_sequences: Option<horizon_client::random_sequences::RandomSequences>,
    locator: Option<Arc<VanillaTerrain>>,
    native_checkpoint_origin: Option<NativeCheckpointOrigin>,
    original_error: Option<String>,
    read_refusals: Vec<String>,
    cancelled: bool,
    entry_token: Option<RetainedEntryIdentity>,
    close_deadline: Option<std::time::Instant>,
    close_error: Option<String>,
    sealed: Option<horizon_client::streaming::SealedStream>,
    producer_acknowledged: bool,
    writer: Option<horizon_client::save::NativeWriterClose>,
    writer_closed: Option<horizon_client::save::NativeWriterClosed>,
    accepted_item_uuids: std::collections::BTreeSet<[i32; 4]>,
    #[cfg(test)]
    control: Option<RetainedPrepareControl>,
}

#[cfg(feature = "paired-packages")]
impl Drop for PrepareOwner {
    fn drop(&mut self) {
        if self.armed {
            use std::io::Write;
            let _ = writeln!(
                std::io::stderr().lock(),
                "fatal: unresolved retained preparation owner"
            );
            // Before fields can drop a writer, native lock, or unfinished producer.
            std::process::abort();
        }
    }
}

#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
pub(super) struct RetainedPreparedWorld {
    owner: Box<PrepareOwner>,
}
#[cfg(feature = "paired-packages")]
impl RetainedPreparedWorld {
    pub(super) fn may_adopt(&self, entry: &RetainedEntryIdentity) -> bool {
        !self.owner.cancelled
            && self.owner.close_deadline.is_none()
            && self.owner.original_error.is_none()
            && self.owner.armed
            && self
                .owner
                .entry_token
                .as_ref()
                .is_some_and(|original| original.matches(entry))
    }
    /// Complete original-owner transfer shared by the fixed production cold
    /// constructor and its fixture. Neither caller replaces an existing Game.
    fn into_cold_parts(mut self) -> std::result::Result<PreparedWorld, Self> {
        let owner = &self.owner;
        if !owner.armed
            || owner.cancelled
            || owner.original_error.is_some()
            || owner.close_deadline.is_some()
            || owner.close_error.is_some()
            || owner.sealed.is_some()
            || owner.producer_acknowledged
            || owner.writer.is_some()
            || owner.writer_closed.is_some()
            || owner.authority.is_some()
            || owner.level.is_none()
            || owner.world.is_none()
            || owner.streaming.is_none()
            || owner.save.is_none()
            || owner.session.is_none()
            || owner.rand_sequences.is_none()
            || owner.native_checkpoint_origin.is_none()
        {
            return Err(self);
        }
        let level = owner.level.as_ref().unwrap();
        if owner
            .session
            .as_ref()
            .and_then(WorldSession::expected_world)
            .is_none()
            || owner
                .native_checkpoint_origin
                .as_ref()
                .unwrap()
                .validate(
                    &owner.request.save_root,
                    Dim::from_id(&level.dimension),
                    owner.save.as_ref().unwrap(),
                )
                .is_err()
        {
            return Err(self);
        }
        // Every refusal is above all takes. Construct the complete transfer from
        // SAME original owners before disarming the old preparation backstop.
        let prepared = PreparedWorld {
            save_root: self.owner.request.save_root.clone(),
            level: self.owner.level.take().unwrap(),
            world: *self.owner.world.take().unwrap(),
            streaming: self.owner.streaming.take().unwrap(),
            save: self.owner.save.take().unwrap(),
            session_lock: self.owner.session.take().unwrap(),
            rand_sequences: self.owner.rand_sequences.take().unwrap(),
            locator: self.owner.locator.take(),
            native_checkpoint_origin: self.owner.native_checkpoint_origin.take(),
        };
        self.owner.armed = false;
        Ok(prepared)
    }

    /// Existing fixture compatibility; production uses ColdPreparation instead.
    #[cfg(all(
        test,
        feature = "paired-packages",
        feature = "native-writer-test-control",
        debug_assertions
    ))]
    pub(in crate::game) fn into_cold_prepared_for_test(
        self,
    ) -> std::result::Result<PreparedWorld, Self> {
        self.into_cold_parts()
    }
}
#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
pub(super) struct PrepareFailure {
    owner: Box<PrepareOwner>,
}
#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
pub(super) enum PreparedOutcome {
    Ready(RetainedPreparedWorld),
    Failed(PrepareFailure),
}

/// The production cold constructor never exposes a streamer or admits a chunk
/// job. Its failed/ready outcome keeps the original acquisitions until either
/// the complete transfer or the actual preparation-close protocol finishes.
#[cfg(feature = "paired-packages")]
pub(super) struct ColdPreparation {
    outcome: PreparedOutcome,
}

#[cfg(feature = "paired-packages")]
impl ColdPreparation {
    pub(super) fn prepare(
        request: WorldRequest,
        res: &Resources,
        pack: &PackResources,
        options: &Options,
        settings: &Settings,
        entry: &RetainedEntryIdentity,
    ) -> Self {
        let mut owner = PrepareOwner::new(request);
        owner.entry_token = Some(RetainedEntryIdentity {
            token: entry.token,
            owner: entry.owner.clone(),
        });
        // This fixed profile produces initial NativeWorld provenance, not an
        // ordinary reopen, parked level, managed authority or vanilla provider.
        // Refuse those inputs before any native lock/storage acquisition.
        let result = if owner.request.identity_intent != IdentityIntent::CreateNewIdentity
            || owner.request.authority_namespace.is_some()
            || owner.request.expected_checkpoint_world.is_some()
            || !options.test_terrain
            || options.dimension.is_some()
            || !options.execution_role.is_client()
            || options.server_thread
        {
            Err("cold retained preparation requires a new identified in-frame native TestTerrain world".into())
        } else {
            owner.prepare(res, pack, options, settings)
        };
        let outcome = match result {
            Ok(()) => PreparedOutcome::Ready(RetainedPreparedWorld { owner }),
            Err(error) => {
                owner.original_error = Some(error.to_string());
                PreparedOutcome::Failed(PrepareFailure { owner })
            }
        };
        Self { outcome }
    }

    pub(super) fn original_error(&self) -> Option<&str> {
        self.outcome.owner().original_error.as_deref()
    }

    pub(super) fn into_prepared(
        self,
        entry: &RetainedEntryIdentity,
    ) -> std::result::Result<ColdPreparedWorld, Self> {
        let outcome = match self.outcome {
            PreparedOutcome::Ready(ready) if ready.may_adopt(entry) => {
                match ready.into_cold_parts() {
                    Ok(prepared) => {
                        // The complete original cold transfer and exact entry
                        // checks precede issuance. No other constructor issues
                        // this capability, including the raw fixture transfer.
                        let authority = NativeCloseAuthority {
                            save: Arc::downgrade(&prepared.save),
                            world: *prepared.session_lock.expected_world().unwrap(),
                            root: prepared.save_root.clone(),
                            dimension: Dim::from_id(&prepared.level.dimension),
                            level_dimension: prepared.level.dimension.clone(),
                            origin: Arc::downgrade(
                                &prepared.native_checkpoint_origin.as_ref().unwrap().identity,
                            ),
                        };
                        return Ok(ColdPreparedWorld {
                            prepared,
                            authority,
                        });
                    }
                    Err(ready) => PreparedOutcome::Ready(ready),
                }
            }
            outcome => outcome,
        };
        Err(Self { outcome })
    }

    pub(super) fn begin_close(&mut self, deadline: std::time::Instant) {
        self.outcome.begin_close(deadline);
    }

    pub(super) fn poll_close(&mut self) -> PreparationCloseStatus {
        // PrepareOwner::prepare starts original workers but never calls update
        // or queues a chunk job. This wrapper exposes no operation that can do
        // either; a surprise finished column is retained and blocks below.
        self.outcome.owner_mut().poll_close_with_game(None)
    }

    pub(super) fn retry_close(&mut self, deadline: std::time::Instant) -> Result<()> {
        self.outcome.retry_close(deadline)
    }

    pub(super) fn finish_close(self) -> std::result::Result<PreparationClosed, Self> {
        self.outcome
            .finish_close()
            .map_err(|outcome| Self { outcome })
    }
}
#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PreparationCloseStatus {
    Pending,
    Blocked(String),
    Complete,
}
#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
pub(super) struct PreparationClosed {
    pub original_error: Option<String>,
    /// Failed cleanup of actual read-only input, never a successful load/save.
    pub read_refusals: Vec<String>,
    pub cancelled: bool,
    entry_token: Option<RetainedEntryIdentity>,
}
#[cfg(feature = "paired-packages")]
impl PreparationClosed {
    pub(super) fn matches_entry(&self, entry: &RetainedEntryIdentity) -> bool {
        self.entry_token
            .as_ref()
            .is_some_and(|original| original.matches(entry))
    }
}
#[cfg(feature = "paired-packages")]
pub(super) struct RetainedEntryIdentity {
    pub token: u64,
    pub owner: Arc<()>,
}
#[cfg(feature = "paired-packages")]
impl RetainedEntryIdentity {
    fn matches(&self, other: &Self) -> bool {
        self.token == other.token && Arc::ptr_eq(&self.owner, &other.owner)
    }
}

#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
impl PreparedOutcome {
    pub(super) fn owner(&self) -> &PrepareOwner {
        match self {
            Self::Ready(ready) => &ready.owner,
            Self::Failed(failure) => &failure.owner,
        }
    }
    pub(super) fn owner_mut(&mut self) -> &mut PrepareOwner {
        match self {
            Self::Ready(ready) => &mut ready.owner,
            Self::Failed(failure) => &mut failure.owner,
        }
    }
    pub(super) fn request_cancel(&mut self) {
        self.owner_mut().cancelled = true;
    }
    pub(super) fn bind_entry_token(&mut self, token: u64, identity: Arc<()>) {
        self.owner_mut().entry_token = Some(RetainedEntryIdentity {
            token,
            owner: identity,
        });
    }
    pub(super) fn begin_close(&mut self, deadline: std::time::Instant) {
        let owner = self.owner_mut();
        // Repeated close/cancel intents do not constitute Retry.
        owner.close_deadline.get_or_insert(deadline);
    }
    pub(super) fn poll_close(&mut self, game: &Game<'_>) -> PreparationCloseStatus {
        self.owner_mut().poll_close(game)
    }
    pub(super) fn retry_close(&mut self, deadline: std::time::Instant) -> Result<()> {
        self.owner_mut().retry_close(deadline)
    }
    pub(super) fn finish_close(self) -> std::result::Result<PreparationClosed, Self> {
        if self.owner().armed {
            return Err(self);
        }
        Ok(PreparationClosed {
            original_error: self.owner().original_error.clone(),
            read_refusals: self.owner().read_refusals.clone(),
            cancelled: self.owner().cancelled,
            entry_token: self
                .owner()
                .entry_token
                .as_ref()
                .map(|entry| RetainedEntryIdentity {
                    token: entry.token,
                    owner: entry.owner.clone(),
                }),
        })
    }
}

#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
impl PrepareOwner {
    fn new(request: WorldRequest) -> Box<Self> {
        Box::new(Self {
            armed: true,
            request,
            native_acquisition: None,
            preflight: None,
            authority_acquisition: None,
            authority: None,
            session: None,
            save: None,
            streaming: None,
            level: None,
            world: None,
            rand_sequences: None,
            locator: None,
            native_checkpoint_origin: None,
            original_error: None,
            read_refusals: Vec::new(),
            cancelled: false,
            entry_token: None,
            close_deadline: None,
            close_error: None,
            sealed: None,
            producer_acknowledged: false,
            writer: None,
            writer_closed: None,
            accepted_item_uuids: Default::default(),
            #[cfg(test)]
            control: None,
        })
    }

    fn prepare(
        &mut self,
        res: &Resources,
        pack: &PackResources,
        options: &Options,
        settings: &Settings,
    ) -> Result<()> {
        let request = &self.request;
        if let Some(namespace) = &request.authority_namespace {
            if request.identity_intent != IdentityIntent::CreateNewIdentity {
                return Err("managed reopen requires authoritative archive recovery".into());
            }
            namespace.check_new_world(&request.save_root)?;
        }
        if request.expected_checkpoint_world.is_some()
            && request.identity_intent != IdentityIntent::RequireExistingIdentity
        {
            return Err("checkpoint recovery requires existing identified native world".into());
        }
        options.mod_packages.check_world(&request.save_root)?;
        self.preflight =
            host_identity::IdentityPreflight::read(&request.save_root, request.identity_intent)?;
        // Never starts or relinquishes global PRELOAD on this dormant route.
        horizon_client::structure_blocks::loot_data(&options.resources);
        if self.preflight.is_none() {
            horizon_client::save::upgrade_older_world(&request.save_root)?;
            horizon_client::save::migrate_legacy(&request.save_root)?;
        }
        self.native_acquisition = Some(horizon_client::save::RetainedSessionLock::new(
            &request.save_root,
        ));
        self.native_acquisition.as_mut().unwrap().acquire()?;
        #[cfg(test)]
        Self::test_stage(&mut self.control, RetainedPrepareStage::NativeAcquired)?;
        if let Some(preflight) = &self.preflight {
            preflight.recheck(&request.save_root)?;
            if let Some(namespace) = &request.authority_namespace {
                self.authority_acquisition =
                    Some(preflight.retained_authority_acquisition(namespace, &request.save_root)?);
                self.authority_acquisition.as_mut().unwrap().acquire()?;
                self.authority = Some(preflight.finish_retained_authority(
                    namespace,
                    self.authority_acquisition.as_mut().unwrap(),
                )?);
            }
            if let Some(expected) = request.expected_checkpoint_world {
                preflight.require_checkpoint_world(expected)?;
            }
            options.mod_packages.check_world(&request.save_root)?;
            options.mod_packages.record_world(&request.save_root)?;
            #[cfg(test)]
            Self::test_stage(&mut self.control, RetainedPrepareStage::IdentityFinish)?;
            self.session = Some(preflight.finish_retained(
                &request.save_root,
                self.native_acquisition.as_mut().unwrap(),
            )?);
            if self.authority.is_some() {
                self.session
                    .as_mut()
                    .unwrap()
                    .install_authority(&mut self.authority)?;
            }
        } else {
            options.mod_packages.record_world(&request.save_root)?;
            let lock = self
                .native_acquisition
                .as_mut()
                .unwrap()
                .finish(&request.save_root)?;
            self.session = Some(WorldSession::legacy(lock));
        }
        self.rand_sequences = Some(
            horizon_client::save::read_dat(
                &request
                    .save_root
                    .join("data/minecraft/random_sequences.dat"),
            )
            .and_then(|tag| tag.get("data").cloned())
            .map(|data| horizon_client::random_sequences::RandomSequences::from_nbt(&data))
            .unwrap_or_default(),
        );
        let existing = Level::read(&request.save_root);
        if request.identity_intent.requires_existing() && existing.is_none() {
            return Err("required host identity reopen: native Level could not be read; fresh-world fallback refused".into());
        }
        let created = existing.is_none();
        self.level = Some(existing.unwrap_or_else(|| {
            Level {
                name: request
                    .name
                    .as_deref()
                    .unwrap_or(&request.folder)
                    .to_owned(),
                seed: request.seed.unwrap_or_else(|| {
                    let time = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_nanos() as u64);
                    (time ^ (time >> 29)).wrapping_mul(0x9E37_79B9_7F4A_7C15) as i64
                }),
                creative: request.creative,
                day_time: 0,
                health: 20.0,
                food: 20,
                saturation: 5.0,
                ..Level::default()
            }
        }));
        let level = self.level.as_mut().unwrap();
        if created && let Some(create) = &request.create {
            level.hardcore = create.hardcore;
            level.difficulty = horizon_client::title::DIFFICULTIES[create.difficulty()].into();
            level.allow_commands = Some(create.commands(request.creative));
            horizon_client::save::write_world_gen_settings_with(
                &request.save_root,
                level.seed,
                create.world_gen(),
                create.flat_settings(),
            )?;
            level.game_rules = create.rules.by_name();
        }
        if created {
            level.allow_commands.get_or_insert(request.creative);
        }
        if let Some(dimension) = &options.dimension {
            level.dimension = dimension.clone();
            level.position = None;
        }
        let dim = Dim::from_id(&level.dimension);
        let seed = level.seed;
        self.save = Some(Arc::new(SaveDir::new_retained(
            horizon_client::save::dimension_dir(&request.save_root, dim.id()),
        )));
        #[cfg(test)]
        Self::test_stage(&mut self.control, RetainedPrepareStage::SaveAcquired)?;
        self.prepare_streaming(res, pack, options, settings, dim, seed)?;
        #[cfg(test)]
        Self::test_stage(&mut self.control, RetainedPrepareStage::StreamingAcquired)?;
        self.world = Some(Box::new(World::new(res.facts().air, seed)));
        let world = self.world.as_mut().unwrap();
        self.session
            .as_mut()
            .unwrap()
            .bind_initial_counter_world(world, dim.id())?;
        world.fast_lava = dim == Dim::Nether;
        world.game_time = self.level.as_ref().unwrap().time;
        world.day_time = options
            .time
            .unwrap_or(self.level.as_ref().unwrap().day_time);
        world.weather = self.level.as_ref().unwrap().weather;
        if let Some(weather) = options.weather.as_deref() {
            let (rain, thunder) = (weather != "clear", weather == "thunder");
            world.weather = horizon_client::world::Weather {
                clear_time: if weather == "clear" { 6000 } else { 0 },
                rain_time: 6000,
                thunder_time: 6000,
                raining: rain,
                thundering: thunder,
                rain_level: if rain { (1.0, 1.0) } else { (0.0, 0.0) },
                thunder_level: if thunder { (1.0, 1.0) } else { (0.0, 0.0) },
            };
        }
        growth::install_live_features(res, pack, options);
        if !self.request.save_root.join("level.dat").exists() {
            self.level
                .as_mut()
                .unwrap()
                .write(&self.request.save_root)?;
        }
        Ok(())
    }

    fn prepare_streaming(
        &mut self,
        res: &Resources,
        pack: &PackResources,
        options: &Options,
        settings: &Settings,
        dim: Dim,
        seed: i64,
    ) -> Result<()> {
        let save = self.save.as_ref().unwrap().clone();
        let flat = (dim == Dim::Overworld)
            .then(|| horizon_client::save::world_gen_settings(&self.request.save_root))
            .flatten()
            .and_then(|settings| {
                horizon_client::flatgen::FlatTerrain::from_settings(
                    &settings,
                    pack.assets.registry,
                    res.biomes(),
                )
            });
        let started = if let Some(inner) = flat {
            self.start_stream(
                Arc::new(Persistent {
                    inner: Arc::new(inner),
                    save: save.clone(),
                    registry: pack.assets.clone(),
                    biomes: res.biomes().clone(),
                }),
                res,
                pack,
                save.clone(),
            )
        } else if options.test_terrain && dim == Dim::Overworld {
            let inner = Arc::new(TestTerrain::new(pack.assets.registry, res.biomes())?);
            self.native_checkpoint_origin = self
                .session
                .as_ref()
                .is_some_and(|session| session.expected_world().is_some())
                .then(|| NativeCheckpointOrigin {
                    root: self.request.save_root.clone(),
                    dimension: dim,
                    save: Arc::downgrade(&save),
                    identity: Arc::new(()),
                });
            self.start_stream(
                Arc::new(Persistent {
                    inner,
                    save: save.clone(),
                    registry: pack.assets.clone(),
                    biomes: res.biomes().clone(),
                }),
                res,
                pack,
                save.clone(),
            )
        } else {
            let structures = horizon_client::save::WorldGen::read(&self.request.save_root)
                .is_none_or(|world| world.structures);
            let inner = Arc::new(VanillaTerrain::for_world(&options.resources.join("worldgen"), pack.assets.registry, res.biomes(), seed, dim.id(), structures)
                .map_err(|error| format!("{error}\nPrepare the worldgen bundle with the launcher, or: horizon-prepare --prepare"))?);
            // Deliberately omit only the optional detached Ring search here.
            self.locator = Some(inner.clone());
            self.start_stream(
                Arc::new(Persistent {
                    inner,
                    save: save.clone(),
                    registry: pack.assets.clone(),
                    biomes: res.biomes().clone(),
                }),
                res,
                pack,
                save.clone(),
            )
        };
        match started {
            Ok(streaming) => self.streaming = Some(streaming),
            Err(failure) => {
                self.streaming = Some(failure.streaming);
                return Err(failure.error.into());
            }
        }
        let streaming = self.streaming.as_mut().unwrap();
        streaming.forced = save.load_forced_chunks();
        streaming.radius = settings.render_distance;
        streaming.biome_blend = settings.biome_blend;
        streaming.smooth_lighting = settings.smooth_lighting;
        streaming.cutout_leaves = settings.cutout_leaves;
        streaming.biome_zoom = BiomeZoom::new(obfuscate_seed(seed));
        streaming.sky_light = dim != Dim::Nether;
        streaming.nether_shading = dim == Dim::Nether;
        streaming.mesh = options.execution_role.is_client();
        Ok(())
    }

    #[allow(clippy::result_large_err)] // Exact partial Streaming owner is the agreed failure API.
    fn start_stream<G: horizon_client::streaming::ChunkGenerator>(
        &mut self,
        generator: Arc<G>,
        res: &Resources,
        pack: &PackResources,
        save: Arc<SaveDir>,
    ) -> std::result::Result<Streaming, horizon_client::streaming::StreamingStartFailure> {
        #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
        if let Some(control) = self
            .control
            .as_mut()
            .and_then(|control| control.streaming.take())
        {
            let threads = self
                .control
                .as_ref()
                .and_then(|control| control.streaming_threads)
                .unwrap_or_else(|| res.threads());
            return Streaming::start_retained_with_test_control(
                generator,
                pack.context.clone(),
                threads,
                save,
                control,
            );
        }
        Streaming::start_retained(generator, pack.context.clone(), res.threads(), save)
    }

    fn blocked(&mut self, error: impl Into<String>) -> PreparationCloseStatus {
        let error = error.into();
        self.close_error.get_or_insert_with(|| error.clone());
        PreparationCloseStatus::Blocked(error)
    }

    fn poll_close(&mut self, game: &Game<'_>) -> PreparationCloseStatus {
        self.poll_close_with_game(Some(game))
    }

    fn poll_close_with_game(&mut self, game: Option<&Game<'_>>) -> PreparationCloseStatus {
        use horizon_client::{save::RetainedWriterStatus, streaming::RetainedStreamStatus};
        if !self.armed {
            return PreparationCloseStatus::Complete;
        }
        let Some(deadline) = self.close_deadline else {
            return PreparationCloseStatus::Pending;
        };
        if !self.producer_acknowledged
            && let Some(streaming) = &mut self.streaming
        {
            if let Err(error) = streaming.begin_retained_close(self.world.as_deref(), deadline) {
                return self.blocked(error);
            }
            if self.sealed.is_none() {
                match streaming.poll_retained_close(self.world.as_deref_mut()) {
                    RetainedStreamStatus::Pending => return PreparationCloseStatus::Pending,
                    RetainedStreamStatus::Blocked(error) => return self.blocked(error),
                    RetainedStreamStatus::NeedsRefusedRead => {
                        let read = streaming
                            .retained_refused_read()
                            .expect("retained refused read requested");
                        let error = read.error().to_owned();
                        let ticket = read.into_ticket();
                        match streaming.resolve_retained_refused_read(ticket, error) {
                            Ok(_) => return PreparationCloseStatus::Pending,
                            Err(error) => return self.blocked(error),
                        }
                    }
                    RetainedStreamStatus::NeedsFinishedColumn => {
                        let Some(game) = game else {
                            // No ticket is taken or payload discarded. A cold
                            // preparation cannot silently treat unexpected work
                            // as a validated native column or forge a Game.
                            return self.blocked(
                                "cold retained preparation has an unexpected finished column",
                            );
                        };
                        let view = streaming
                            .retained_finished_column()
                            .expect("retained column requested");
                        let staged = validate_retained_column(
                            game,
                            &self.request.save_root,
                            self.level.as_ref(),
                            self.world.as_deref(),
                            self.session.as_ref(),
                            self.save.as_ref().unwrap(),
                            self.native_checkpoint_origin.as_ref(),
                            &self.accepted_item_uuids,
                            view.chunk(),
                            view.evidence(),
                        );
                        let (disposition, identities) = match staged {
                            Ok(Some(value)) => value,
                            Ok(None) => return PreparationCloseStatus::Pending,
                            Err(error) => {
                                let error = error.to_string();
                                let read_only = view.evidence().is_some()
                                    && !view.chunk().fresh
                                    && view.origin()
                                        == horizon_client::streaming::NativeWorkOrigin::Generate;
                                let ticket = view.into_ticket();
                                if read_only {
                                    return match streaming
                                        .resolve_retained_refused_read(ticket, error)
                                    {
                                        Ok(_) => PreparationCloseStatus::Pending,
                                        Err(error) => self.blocked(error),
                                    };
                                }
                                if let Err(rejection) =
                                    streaming.reject_retained_finished_column(ticket, error.clone())
                                {
                                    return self.blocked(rejection);
                                }
                                return self.blocked(error);
                            }
                        };
                        let ticket = view.into_ticket();
                        match streaming.resolve_retained_finished_column(ticket, disposition) {
                            Ok(true) => self.accepted_item_uuids.extend(identities),
                            Ok(false) => return PreparationCloseStatus::Pending,
                            Err(error) => return self.blocked(error),
                        }
                        return PreparationCloseStatus::Pending;
                    }
                    RetainedStreamStatus::Complete => {
                        self.sealed = streaming.take_sealed();
                        if self.sealed.is_none() {
                            return self.blocked("retained producer completion witness missing");
                        }
                        self.read_refusals = self.sealed.as_ref().unwrap().refused_reads().to_vec();
                        if self.sealed.as_ref().unwrap().failed_initialization
                            && self.original_error.is_none()
                        {
                            self.original_error = Some(
                                streaming
                                    .retained_close_failures()
                                    .first()
                                    .cloned()
                                    .unwrap_or_else(|| {
                                        "retained native producer initialization failed".into()
                                    }),
                            );
                        }
                    }
                }
            }
        }
        if self.writer_closed.is_none()
            && let Some(save) = &self.save
        {
            if self.writer.is_none() {
                match save.begin_retained_writer_close(deadline) {
                    Ok(Some(writer)) => self.writer = Some(writer),
                    Ok(None) => return PreparationCloseStatus::Pending,
                    Err(error) => return self.blocked(error.to_string()),
                }
            }
            match self.writer.as_mut().unwrap().poll() {
                RetainedWriterStatus::Pending => return PreparationCloseStatus::Pending,
                RetainedWriterStatus::Blocked(error) => return self.blocked(error),
                RetainedWriterStatus::Complete => match self.writer.take().unwrap().finish() {
                    Ok(closed) => self.writer_closed = Some(closed),
                    Err(writer) => {
                        self.writer = Some(writer);
                        return self.blocked("writer finish refused after Complete");
                    }
                },
            }
        }
        if let Some(error) = &self.close_error {
            return PreparationCloseStatus::Blocked(error.clone());
        }
        if !self.producer_acknowledged
            && let Some(streaming) = &mut self.streaming
        {
            let Some(closed) = &self.writer_closed else {
                return self.blocked("producer has no matching native writer witness");
            };
            if !Arc::ptr_eq(closed.save(), self.save.as_ref().unwrap()) {
                return self.blocked("native writer allocation changed");
            }
            match streaming.poll_retained_writer_ack(closed) {
                RetainedStreamStatus::Pending => return PreparationCloseStatus::Pending,
                RetainedStreamStatus::Blocked(error) => return self.blocked(error),
                RetainedStreamStatus::Complete => {}
                RetainedStreamStatus::NeedsFinishedColumn
                | RetainedStreamStatus::NeedsRefusedRead => {
                    return self.blocked("writer acknowledgement found unsettled native input");
                }
            }
            let Some(sealed) = self.sealed.take() else {
                return self.blocked("producer witness missing at acknowledgement");
            };
            if let Err(sealed) = streaming.finish_retained_close(sealed, closed) {
                self.sealed = Some(sealed);
                return self.blocked("native producer/writer acknowledgement refused");
            }
            self.producer_acknowledged = true;
        }
        // No resource owner is released until the actual producers and writer
        // are terminal. These lock-only wrappers do not certify closure alone.
        if let Some(acquisition) = &mut self.authority_acquisition
            && let Err(error) = acquisition.release_lock_only()
        {
            return self.blocked(error.to_string());
        }
        if let Some(acquisition) = &mut self.native_acquisition
            && let Err(error) = acquisition.release_lock_only()
        {
            return self.blocked(error.to_string());
        }
        self.session = None;
        self.authority = None;
        self.armed = false;
        PreparationCloseStatus::Complete
    }

    fn retry_close(&mut self, deadline: std::time::Instant) -> Result<()> {
        if self.close_error.is_none() || deadline <= std::time::Instant::now() || !self.armed {
            return Err("preparation Retry requires a blocked owner and future deadline".into());
        }
        if let Some(writer) = &mut self.writer {
            writer.retry(deadline)?;
        } else if !self.producer_acknowledged
            && self.sealed.is_none()
            && let Some(streaming) = &mut self.streaming
        {
            streaming.retry_retained_close(deadline)?;
        }
        self.close_deadline = Some(deadline);
        self.close_error = None;
        Ok(())
    }
}

/// Genuine retained construction for an additional native level. It borrows the
/// existing identified Game session; it never acquires/relabels a legacy owner
/// or issues initial-Overworld checkpoint provenance for a parked level.
#[cfg(feature = "paired-packages")]
#[allow(dead_code)] // Dormant retained factory, separate from ordinary activation.
pub(in crate::game) struct RetainedParkedConstruction {
    owner: Box<PrepareOwner>,
    dimension: Dim,
    parent_save: Arc<SaveDir>,
}
#[cfg(feature = "paired-packages")]
#[allow(dead_code)] // Owned transfer exists for the dormant retained factory only.
pub(in crate::game) struct RetainedParkedParts {
    pub world: World,
    pub streaming: Streaming,
    pub save: Arc<SaveDir>,
    pub locator: Option<Arc<VanillaTerrain>>,
}
#[cfg(feature = "paired-packages")]
impl std::fmt::Debug for RetainedParkedConstruction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RetainedParkedConstruction")
            .field("dimension", &self.dimension)
            .field("error", &self.error())
            .finish_non_exhaustive()
    }
}
#[cfg(feature = "paired-packages")]
#[allow(dead_code)] // Keep the retained factory available without ordinary wiring.
impl RetainedParkedConstruction {
    pub(in crate::game) fn start(game: &Game<'_>, dimension: Dim) -> Self {
        let request = WorldRequest {
            save_root: game.world_root.clone(),
            folder: "retained-native-level".into(),
            name: None,
            seed: Some(game.level.seed),
            creative: game.creative,
            create: None,
            identity_intent: IdentityIntent::Legacy,
            authority_namespace: None,
            expected_checkpoint_world: None,
        };
        let mut owner = PrepareOwner::new(request);
        owner.save = Some(Arc::new(SaveDir::new_retained(
            horizon_client::save::dimension_dir(&game.world_root, dimension.id()),
        )));
        let mut world = Box::new(World::new(game.res.facts().air, game.level.seed));
        world.fast_lava = dimension == Dim::Nether;
        world.game_time = game.world.game_time;
        world.day_time = game.world.day_time;
        world.weather = game.world.weather;
        owner.world = Some(world);
        let result = if game
            .session_lock
            .as_ref()
            .and_then(WorldSession::expected_world)
            .is_none()
        {
            Err(
                "retained parked native construction requires original identified Game session"
                    .into(),
            )
        } else {
            owner.prepare_streaming(
                &game.res,
                &game.pack,
                &game.options,
                &game.settings,
                dimension,
                game.level.seed,
            )
        };
        if let Err(error) = result {
            owner.original_error = Some(error.to_string());
        }
        Self {
            owner,
            dimension,
            parent_save: game.save.clone(),
        }
    }
    pub(in crate::game) fn error(&self) -> Option<&str> {
        self.owner.original_error.as_deref()
    }
    pub(in crate::game) fn dimension(&self) -> Dim {
        self.dimension
    }
    pub(in crate::game) fn matches_game(&self, game: &Game<'_>) -> bool {
        self.owner.request.save_root == game.world_root
            && Arc::ptr_eq(&self.parent_save, &game.save)
    }
    pub(in crate::game) fn begin_close(&mut self, deadline: std::time::Instant) {
        self.owner.close_deadline.get_or_insert(deadline);
    }
    pub(in crate::game) fn poll_close(&mut self, game: &Game<'_>) -> PreparationCloseStatus {
        self.owner.poll_close(game)
    }
    pub(in crate::game) fn retry_close(&mut self, deadline: std::time::Instant) -> Result<()> {
        self.owner.retry_close(deadline)
    }
    pub(in crate::game) fn into_parts(mut self) -> std::result::Result<RetainedParkedParts, Self> {
        if self.owner.original_error.is_some()
            || self.owner.streaming.is_none()
            || self.owner.world.is_none()
            || self.owner.save.is_none()
            || self.owner.close_deadline.is_some()
        {
            return Err(self);
        }
        // All checks precede every ownership take. Native retained owners remain
        // armed in their final Parked allocation, whose close has not begun yet.
        let world = *self.owner.world.take().unwrap();
        let streaming = self.owner.streaming.take().unwrap();
        let save = self.owner.save.take().unwrap();
        let locator = self.owner.locator.take();
        self.owner.armed = false;
        Ok(RetainedParkedParts {
            world,
            streaming,
            save,
            locator,
        })
    }
}

/// The retained initialization handle is never replaced by a disposal thread.
#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
pub(super) struct RetainedWorldLoader {
    handle: Option<JoinHandle<PreparedOutcome>>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
}
#[cfg(feature = "paired-packages")]
impl Drop for RetainedWorldLoader {
    fn drop(&mut self) {
        if self.handle.is_some() {
            use std::io::Write;
            let _ = writeln!(
                std::io::stderr().lock(),
                "fatal: unresolved retained initialization handle"
            );
            std::process::abort();
        }
    }
}
#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
impl RetainedWorldLoader {
    pub(super) fn start(
        request: WorldRequest,
        res: Arc<Resources>,
        pack: PackResources,
        options: Options,
        settings: Settings,
    ) -> Result<Self> {
        #[cfg(test)]
        {
            Self::start_inner(request, res, pack, options, settings, None)
        }
        #[cfg(not(test))]
        {
            Self::start_inner(request, res, pack, options, settings)
        }
    }
    fn start_inner(
        request: WorldRequest,
        res: Arc<Resources>,
        pack: PackResources,
        options: Options,
        settings: Settings,
        #[cfg(test)] control: Option<RetainedPrepareControl>,
    ) -> Result<Self> {
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let intent = cancel.clone();
        let handle = std::thread::Builder::new()
            .name("Retained world initialization".into())
            .spawn(move || {
                let mut owner = PrepareOwner::new(request);
                #[cfg(test)]
                {
                    owner.control = control;
                }
                #[cfg(test)]
                let gate = PrepareOwner::test_stage(
                    &mut owner.control,
                    RetainedPrepareStage::BeforeAcquire,
                );
                #[cfg(not(test))]
                let gate: Result<()> = Ok(());
                let result = if let Err(error) = gate {
                    Err(error)
                } else if intent.load(Ordering::Acquire) {
                    Err("world initialization cancelled before acquisition".into())
                } else {
                    owner.prepare(&res, &pack, &options, &settings)
                };
                owner.cancelled = intent.load(Ordering::Acquire);
                match result {
                    Ok(()) => PreparedOutcome::Ready(RetainedPreparedWorld { owner }),
                    Err(error) => {
                        owner.original_error = Some(error.to_string());
                        PreparedOutcome::Failed(PrepareFailure { owner })
                    }
                }
            })?;
        Ok(Self {
            handle: Some(handle),
            cancel,
        })
    }
    pub(super) fn request_cancel(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
    pub(super) fn poll_retained_outcome(
        &mut self,
    ) -> Option<std::result::Result<PreparedOutcome, String>> {
        if !self
            .handle
            .as_ref()
            .is_some_and(|handle| handle.is_finished())
        {
            return None;
        }
        let outcome = self
            .handle
            .take()
            .unwrap()
            .join()
            .map_err(|_| "retained initialization worker panicked".to_owned());
        Some(outcome.map(|mut outcome| {
            outcome.owner_mut().cancelled |= self.cancel.load(Ordering::Acquire);
            outcome
        }))
    }
}

/// Validate without publishing a native column or activating any entity. The
/// original Arc and work lease remain inside Streaming throughout this function.
#[cfg(feature = "paired-packages")]
#[allow(clippy::too_many_arguments)] // Borrow disjoint guarded fields while Streaming retains the payload.
fn validate_retained_column(
    game: &Game<'_>,
    root: &std::path::Path,
    level: Option<&Level>,
    world: Option<&World>,
    session: Option<&WorldSession>,
    save: &Arc<SaveDir>,
    origin: Option<&NativeCheckpointOrigin>,
    accepted: &std::collections::BTreeSet<[i32; 4]>,
    chunk: &Arc<horizon_client::chunk::Chunk>,
    evidence: Option<&horizon_client::save::RetainedLoadedRecords>,
) -> Result<
    Option<(
        horizon_client::streaming::RetainedColumnDisposition,
        std::collections::BTreeSet<[i32; 4]>,
    )>,
> {
    validate_retained_column_context(
        &game.capture_native_validation_context(),
        &game.pack,
        &game.res,
        &game.persist.uuids,
        root,
        level,
        world,
        session,
        save,
        origin,
        accepted,
        chunk,
        evidence,
    )
}

#[cfg(feature = "paired-packages")]
#[allow(clippy::too_many_arguments)]
pub(in crate::game) fn validate_retained_column_context(
    context: &super::levels::session_checkpoint::SessionValidationContext,
    pack: &PackResources,
    res: &Resources,
    live_uuids: &std::collections::HashMap<u32, [i32; 4]>,
    root: &std::path::Path,
    level: Option<&Level>,
    world: Option<&World>,
    session: Option<&WorldSession>,
    save: &Arc<SaveDir>,
    origin: Option<&NativeCheckpointOrigin>,
    accepted: &std::collections::BTreeSet<[i32; 4]>,
    chunk: &Arc<horizon_client::chunk::Chunk>,
    evidence: Option<&horizon_client::save::RetainedLoadedRecords>,
) -> Result<
    Option<(
        horizon_client::streaming::RetainedColumnDisposition,
        std::collections::BTreeSet<[i32; 4]>,
    )>,
> {
    use super::levels::session_checkpoint::{checked, fail, items, require};
    use horizon_client::save::LevelShape;
    let validate = || -> super::levels::session_checkpoint::SResult<_> {
        let level = level.ok_or_else(|| fail("late native column without original Level"))?;
        let world = world.ok_or_else(|| fail("late native column without original World"))?;
        let dimension = Dim::from_id(&level.dimension);
        checked(
            origin
                .ok_or_else(|| fail("late column requires actual native TestTerrain provenance"))?
                .validate(root, dimension, save),
        )?;
        require(
            session.and_then(WorldSession::expected_world).is_some(),
            "late native column has no retained identified session",
        )?;
        let at = (chunk.x, chunk.z);
        require(
            !world.chunks.contains_key(&at),
            "late native column collides with frozen resident owner",
        )?;
        let tables = horizon_client::save::SaveTables::new(&pack.assets);
        let encoded = if chunk.fresh {
            require(
                evidence.is_none(),
                "fresh native column unexpectedly carries loaded provenance",
            )?;
            horizon_client::save::encode_with(
                chunk,
                pack.assets.registry,
                res.biomes(),
                Some(&tables),
                LevelShape::OVERWORLD,
            )
        } else {
            let evidence = evidence.ok_or_else(|| {
                fail("loaded late native column requires retained original raw storage evidence")
            })?;
            require(
                Arc::ptr_eq(evidence.save(), save)
                    && evidence.position() == at
                    && evidence.dimension_root() == save.root.as_path(),
                "late native record allocation/coordinate/dimension mismatch",
            )?;
            match checked(evidence.poll_current())? {
                None => return Ok(None),
                Some(false) => {
                    return Err(fail(
                        "late native original record superseded by newer relevant storage write",
                    ));
                }
                Some(true) => {}
            }
            let column =
                retained_record_tag_context(context, pack, res, evidence.column(), at, false)?
                    .ok_or_else(|| {
                        fail("loaded late native column has proven absent column source")
                    })?;
            let entities =
                retained_record_tag_context(context, pack, res, evidence.entities(), at, true)?;
            let tags = match &entities {
                None => &[][..],
                Some(named) => {
                    require(named.name.0.is_empty(), "native entity record named root")?;
                    let horizon_world::nbt::Tag::Compound(values) = &named.tag else {
                        return Err(fail("native entity record compound root"));
                    };
                    require(
                        values.len() == 3
                            && values.keys().all(|key| {
                                ["DataVersion", "Position", "Entities"]
                                    .iter()
                                    .any(|allowed| {
                                        key == &horizon_world::nbt::JavaString::from(*allowed)
                                    })
                            })
                            && named.tag.get("DataVersion")
                                == Some(&horizon_world::nbt::Tag::Int(horizon_world::DATA_VERSION))
                            && named.tag.get("Position")
                                == Some(&horizon_world::nbt::Tag::IntArray(vec![at.0, at.1])),
                        "native entity record root/version/position",
                    )?;
                    let Some(horizon_world::nbt::Tag::List(tags)) = named.tag.get("Entities")
                    else {
                        return Err(fail("native entity record List required"));
                    };
                    tags.as_slice()
                }
            };
            require(
                tags == chunk.entities.as_slice(),
                "late native entity source/loaded payload ownership mismatch",
            )?;
            if let horizon_client::save::RetainedRecordView::Pending {
                kind: horizon_client::save::RetainedPendingKind::Chunk,
                chunk: Some(original),
                ..
            } = evidence.column()
            {
                require(
                    original.entities == chunk.entities,
                    "original pending column entities were discarded by sidecar replacement",
                )?;
                let original_decoded = context.validate_column(&column, at, world.game_time)?;
                validate_retained_projection(original, &column, &original_decoded)?;
            }
            column
        };
        let decoded = context.validate_column(&encoded, at, world.game_time)?;
        validate_retained_projection(chunk, &encoded, &decoded)?;
        require(
            chunk.entities.len() <= items::ITEM_LIMIT,
            "late native item count bound",
        )?;
        let mut identities = std::collections::BTreeSet::new();
        let mut bytes = 0usize;
        for tag in &chunk.entities {
            let (uuid, row) = context.validate_item_tag(tag)?;
            require(row.column == at, "late native entity column mismatch")?;
            require(
                identities.insert(uuid) && !accepted.contains(&uuid),
                "duplicate late native item UUID",
            )?;
            require(
                level.player_uuid != Some(uuid) && !live_uuids.values().any(|known| *known == uuid),
                "late native item/player or live UUID collision",
            )?;
            for (_, tags) in &world.loaded_entities {
                for resident in tags {
                    let (known, _) = context.validate_item_tag(resident)?;
                    require(
                        known != uuid,
                        "late native item/frozen arrival UUID collision",
                    )?;
                }
            }
            bytes = bytes
                .checked_add(items::exact_item_encoding(tag)?)
                .ok_or_else(|| fail("late native item byte overflow"))?;
            require(bytes <= 64 << 20, "late native item aggregate byte bound")?;
        }
        require(
            accepted
                .len()
                .checked_add(identities.len())
                .is_some_and(|count| count <= items::ITEM_LIMIT),
            "retained native item aggregate count bound",
        )?;
        let disposition = if chunk.fresh {
            horizon_client::streaming::RetainedColumnDisposition::StoreFresh
        } else {
            horizon_client::streaming::RetainedColumnDisposition::AlreadyStored
        };
        Ok(Some((disposition, identities)))
    };
    validate().map_err(|error| error.to_string().into())
}

#[cfg(feature = "paired-packages")]
fn validate_retained_projection(
    original: &horizon_client::chunk::Chunk,
    encoded: &horizon_world::nbt::NamedTag,
    decoded: &horizon_client::chunk::Chunk,
) -> super::levels::session_checkpoint::SResult<()> {
    use super::levels::session_checkpoint::require;
    use horizon_client::chunk::SECTIONS;
    require(
        original.sections.len() == SECTIONS && decoded.sections.len() == SECTIONS,
        "late native section count",
    )?;
    require(
        original.block_entities.is_empty()
            && original.block_names.is_empty()
            && original.postprocess.is_empty()
            && original.structure_refs.is_empty()
            && original.structure_starts.is_empty(),
        "late native column contains unsupported unique payload",
    )?;
    if let Some(extras) = &original.saved {
        require(
            extras.block_entities.is_empty()
                && extras.block_entity_sources.is_empty()
                && extras.block_entity_order.is_empty()
                && extras.block_ticks.is_empty()
                && extras.fluid_ticks.is_empty()
                && extras.rolled_loot.is_empty()
                && !extras.root.contains_key(&"entities".into()),
            "late native saved payload projection would lose ownership",
        )?;
        for (key, value) in &extras.root {
            let horizon_world::nbt::Tag::Compound(values) = &encoded.tag else {
                return Err(super::levels::session_checkpoint::fail(
                    "late native projected root is not Compound",
                ));
            };
            require(
                values.get(key) == Some(value),
                "late native retained root projection changed original data",
            )?;
        }
    }
    require(
        original.lit == decoded.lit,
        "late native light flag projection changed",
    )?;
    for (before, after) in original.sections.iter().zip(&decoded.sections) {
        for index in 0..4096 {
            require(
                before.blocks.get(index) == after.blocks.get(index),
                "late native block projection changed a cell",
            )?;
            require(
                before.light.get(index) == after.light.get(index),
                "late native light projection changed a cell",
            )?;
        }
        for index in 0..64 {
            require(
                before.biomes.get(index) == after.biomes.get(index),
                "late native biome projection changed a cell",
            )?;
        }
    }
    Ok(())
}

/// Interpret only the original factory-issued storage evidence. IO and raw
/// header validation already ran on its native worker before any constructor.
#[cfg(feature = "paired-packages")]
fn retained_record_tag_context(
    _context: &super::levels::session_checkpoint::SessionValidationContext,
    pack: &PackResources,
    res: &Resources,
    record: horizon_client::save::RetainedRecordView<'_>,
    at: (i32, i32),
    entities: bool,
) -> super::levels::session_checkpoint::SResult<Option<horizon_world::nbt::NamedTag>> {
    use super::levels::session_checkpoint::{checked, fail, items, raw_nbt, require};
    use horizon_client::save::{LevelShape, RetainedPendingKind, RetainedRecordView};
    let slot = ((at.1 & 31) * 32 + (at.0 & 31)) as usize;
    match record {
        RetainedRecordView::Missing { region } => {
            if let Some(region) = region {
                require(
                    !region.present().contains(&slot),
                    "missing native slot contradicts original region locations",
                )?;
            }
            Ok(None)
        }
        RetainedRecordView::Disk { record, region } => {
            require(
                region.present().contains(&slot)
                    && region.header().len() == 8192
                    && region.file_len() >= 8192,
                "native disk record/header correlation",
            )?;
            if entities {
                return items::read_item_record(record).map(Some);
            }
            raw_nbt(&checked(
                record.compression.decode(&record.compressed, 1 << 20),
            )?)
            .map(Some)
        }
        RetainedRecordView::Refused { error, .. } => Err(fail(error)),
        RetainedRecordView::Pending {
            kind,
            tag,
            chunk,
            registry,
            biomes,
            shape,
            ..
        } => {
            if kind == RetainedPendingKind::Delete {
                require(
                    tag.is_none() && chunk.is_none(),
                    "native pending Delete owns an unexpected projection",
                )?;
                return Ok(None);
            }
            if entities {
                require(
                    matches!(
                        kind,
                        RetainedPendingKind::Tag | RetainedPendingKind::FinishedEntities
                    ),
                    "native entity pending kind mismatch",
                )?;
                if kind == RetainedPendingKind::FinishedEntities {
                    let source = chunk.ok_or_else(|| {
                        fail("native pending finished entities lost original Chunk")
                    })?;
                    if source.entities.is_empty() {
                        require(
                            tag.is_none(),
                            "empty native finished entities projected as present",
                        )?;
                        return Ok(None);
                    }
                    require(
                        matches!(tag.and_then(|tag| tag.tag.get("Entities")), Some(horizon_world::nbt::Tag::List(tags)) if tags == &source.entities),
                        "native pending finished entities projection changed original payload",
                    )?;
                }
            } else {
                require(
                    matches!(kind, RetainedPendingKind::Tag | RetainedPendingKind::Chunk),
                    "native column pending kind mismatch",
                )?;
                if kind == RetainedPendingKind::Chunk {
                    require(
                        chunk.is_some()
                            && registry.is_some_and(|registry| {
                                std::ptr::eq(registry, pack.assets.registry)
                            })
                            && biomes.is_some_and(|biomes| std::ptr::eq(biomes, &**res.biomes()))
                            && shape == Some(LevelShape::OVERWORLD),
                        "native pending Chunk encoding context mismatch",
                    )?;
                }
            }
            let tag = tag
                .ok_or_else(|| fail("native pending record lost original structured projection"))?;
            require(tag.name.0.is_empty(), "native pending record named root")?;
            Ok(Some(tag.clone()))
        }
    }
}

#[cfg(all(test, feature = "paired-packages"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetainedPrepareStage {
    BeforeAcquire,
    NativeAcquired,
    IdentityFinish,
    SaveAcquired,
    StreamingAcquired,
}
#[cfg(all(test, feature = "paired-packages"))]
struct RetainedPrepareControl {
    stage: RetainedPrepareStage,
    entered: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
    fail: bool,
    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    streaming: Option<horizon_client::streaming::RetainedStreamingStartTestControl>,
    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    streaming_threads: Option<usize>,
}
#[cfg(all(test, feature = "paired-packages"))]
impl PrepareOwner {
    fn test_stage(
        control: &mut Option<RetainedPrepareControl>,
        stage: RetainedPrepareStage,
    ) -> Result<()> {
        if let Some(control) = control
            && control.stage == stage
        {
            let _ = control.entered.send(());
            control
                .release
                .recv()
                .map_err(|_| "retained preparation test release disconnected")?;
            if control.fail {
                return Err(format!("controlled preparation failure after {stage:?}").into());
            }
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "paired-packages", target_os = "linux"))]
#[path = "world_loading/retained_prepare_tests.rs"]
mod retained_prepare_tests;

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
                identity: Arc::new(()),
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
