//! Explicit local host identity, separate from candidate machine frames.
#[cfg(feature = "paired-packages")]
use horizon_client::Result;
use horizon_client::save::SessionLock;
#[cfg(feature = "paired-packages")]
pub(in crate::game) mod session_authority;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)] // Explicit internal entry only; ordinary startup stays Legacy.
pub(in crate::game) enum IdentityIntent {
    #[default]
    Legacy,
    #[cfg(feature = "paired-packages")]
    CreateNewIdentity,
    #[cfg(feature = "paired-packages")]
    RequireExistingIdentity,
}
impl IdentityIntent {
    pub(super) fn requires_existing(self) -> bool {
        match self {
            Self::Legacy => false,
            #[cfg(feature = "paired-packages")]
            Self::CreateNewIdentity => false,
            #[cfg(feature = "paired-packages")]
            Self::RequireExistingIdentity => true,
        }
    }
}
/// Neither this context nor its native lock can be cloned. Identity is borrowed
/// from the same owner that retains the native lock through shutdown/cancellation.
pub(in crate::game) struct WorldSession {
    _lock: SessionLock,
    #[cfg(feature = "paired-packages")]
    authority: Option<session_authority::WorldAuthority>,
    #[cfg(feature = "paired-packages")]
    world: Option<horizon_client::world::counter_restore::WorldId>,
    #[cfg(feature = "paired-packages")]
    counter_bootstrap: Option<horizon_client::world::counter_restore::HostCounterBootstrap>,
}
impl WorldSession {
    pub(super) fn legacy(lock: SessionLock) -> Self {
        Self {
            _lock: lock,
            #[cfg(feature = "paired-packages")]
            authority: None,
            #[cfg(feature = "paired-packages")]
            world: None,
            #[cfg(feature = "paired-packages")]
            counter_bootstrap: None,
        }
    }
    #[cfg(feature = "paired-packages")]
    #[allow(dead_code)]
    pub(in crate::game) fn expected_world(
        &self,
    ) -> Option<&horizon_client::world::counter_restore::WorldId> {
        self.world.as_ref()
    }
    #[cfg(feature = "paired-packages")]
    pub(super) fn bind_initial_counter_world(
        &mut self,
        world: &mut horizon_client::world::World,
        actual_dimension: &str,
    ) -> Result<()> {
        if actual_dimension != "minecraft:overworld" {
            // An initially different dimension can never gain this eligibility
            // by returning later. There is no grant reconstruction from UUID.
            self.counter_bootstrap = None;
            return Ok(());
        }
        if let Some(grant) = &mut self.counter_bootstrap {
            world.bind_host_counter_bootstrap(grant, actual_dimension)?;
        }
        Ok(())
    }
    #[cfg(feature = "paired-packages")]
    pub(in crate::game) fn counter_bootstrap(
        &mut self,
    ) -> Option<&mut horizon_client::world::counter_restore::HostCounterBootstrap> {
        self.counter_bootstrap.as_mut()
    }
    #[cfg(feature = "paired-packages")]
    pub(in crate::game) fn validate_owned_unplaced(
        &self,
        world: &horizon_client::world::World,
        binding: &horizon_client::world::counter_restore::NativeBinding,
    ) -> Result<()> {
        let owned = self
            .owned()
            .ok_or("unused coherent source requires its actual owned namespace session")?;
        owned.check_binding(binding)?;
        let grant = self
            .counter_bootstrap
            .as_ref()
            .ok_or("unused source has no original session admission")?;
        world.validate_unplaced_host_admission(
            binding,
            self.world.ok_or("unused native identity absent")?,
            "minecraft:overworld",
            grant,
        )?;
        Ok(())
    }
    #[cfg(feature = "paired-packages")]
    pub(in crate::game) fn install_verified_unplaced_admission(
        &mut self,
        proof: &mut Option<crate::game::levels::session_checkpoint::VerifiedUnplacedAdmission>,
        world: &horizon_client::world::World,
    ) -> Result<()> {
        if self.counter_bootstrap.is_some() {
            return Err("native session already owns first-placement admission".into());
        }
        proof
            .as_ref()
            .ok_or("verified Unplaced admission absent")?
            .validate(self, world)?;
        self.counter_bootstrap = Some(
            proof
                .take()
                .expect("checked original Unplaced admission")
                .into_bootstrap(),
        );
        Ok(())
    }
}
#[cfg(feature = "paired-packages")]
use {
    horizon_client::world::counter_restore::WorldId,
    horizon_world::{
        DATA_VERSION,
        compression::Compression,
        nbt::{self, Limits, Tag},
    },
    serde::{Deserialize, Serialize},
    std::{
        fs::{self, File, OpenOptions},
        io::{self, Read, Write},
        path::{Component, Path},
    },
};
#[cfg(feature = "paired-packages")]
pub(super) const IDENTITY_FILE: &str = "horizon-world-identity.json";
#[cfg(feature = "paired-packages")]
const IDENTITY_LIMIT: usize = 1024;
#[cfg(feature = "paired-packages")]
const LEVEL_LIMIT: usize = 1 << 20;
#[cfg(feature = "paired-packages")]
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct IdentityRecord {
    format: u32,
    world_uuid: WorldId,
}
#[cfg(feature = "paired-packages")]
fn failure(stage: &str, error: impl std::fmt::Display) -> String {
    format!("host world identity {stage}: {error}")
}
#[cfg(feature = "paired-packages")]
fn decode_identity(bytes: &[u8]) -> Result<WorldId> {
    if bytes.is_empty() || bytes.len() > IDENTITY_LIMIT {
        return Err(failure("record decode", "byte bound").into());
    }
    let record: IdentityRecord =
        serde_json::from_slice(bytes).map_err(|e| failure("record decode", e))?;
    if record.format != 1 {
        return Err(failure("record decode", "unsupported format").into());
    }
    Ok(record.world_uuid)
}
#[cfg(feature = "paired-packages")]
fn allocate_identity() -> Result<(WorldId, Vec<u8>)> {
    let mut bytes = [0u8; 16];
    horizon_protocol::crypto::random_bytes(&mut bytes).map_err(|e| failure("entropy", e))?;
    bytes[6] = (bytes[6] & 15) | 0x40;
    bytes[8] = (bytes[8] & 63) | 0x80;
    let text = bytes
        .iter()
        .enumerate()
        .fold(String::new(), |mut text, (i, byte)| {
            use std::fmt::Write;
            if [4, 6, 8, 10].contains(&i) {
                text.push('-');
            }
            write!(&mut text, "{byte:02x}").expect("String formatting");
            text
        });
    let world = WorldId::parse(&text).map_err(|e| failure("entropy", e))?;
    let encoded = serde_json::to_vec(&IdentityRecord {
        format: 1,
        world_uuid: world,
    })
    .map_err(|e| failure("record encode", e))?;
    if encoded.len() > IDENTITY_LIMIT {
        return Err(failure("record encode", "byte bound").into());
    }
    Ok((world, encoded))
}
#[cfg(feature = "paired-packages")]
fn regular(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).map_err(|e| failure("path preflight", e))?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(failure(
            "path preflight",
            format!("{} must be a regular file", path.display()),
        )
        .into());
    }
    Ok(())
}
#[cfg(feature = "paired-packages")]
fn safe_root(root: &Path) -> Result<()> {
    if !root.is_absolute() || root.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(failure(
            "path preflight",
            "absolute root without parent traversal required",
        )
        .into());
    }
    for at in root.ancestors() {
        match fs::symlink_metadata(at) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(failure(
                    "path preflight",
                    format!("{} is not a real directory", at.display()),
                )
                .into());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(failure("path preflight", error).into()),
        }
    }
    match fs::symlink_metadata(root.join("session.lock")) {
        Ok(_) => regular(&root.join("session.lock"))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(failure("lock preflight", error).into()),
    }
    Ok(())
}
#[cfg(feature = "paired-packages")]
fn bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    regular(path)?;
    let meta = fs::symlink_metadata(path).map_err(|e| failure("bounded read", e))?;
    if meta.len() > limit as u64 {
        return Err(failure("bounded read", "file too large").into());
    }
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|e| failure("bounded read", e))?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| failure("bounded read", e))?;
    if bytes.len() > limit {
        return Err(failure("bounded read", "file grew past bound").into());
    }
    Ok(bytes)
}
#[cfg(feature = "paired-packages")]
fn new_root(root: &Path) -> Result<()> {
    safe_root(root)?;
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(failure("new root", error).into()),
    };
    for entry in entries {
        let entry = entry.map_err(|e| failure("new root", e))?;
        if entry.file_name() != "session.lock" {
            return Err(failure(
                "new root",
                "root is not new; creation never retries or regenerates",
            )
            .into());
        }
        regular(&entry.path())?;
    }
    Ok(())
}
#[cfg(feature = "paired-packages")]
fn native_level(root: &Path, bytes: &[u8]) -> Result<()> {
    for old in ["region", "entities", "poi", "DIM-1", "DIM1"] {
        match fs::symlink_metadata(root.join(old)) {
            Ok(_) => return Err(failure("native level", "legacy layout requires migration").into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(failure("native level", error).into()),
        }
    }
    if bytes.len() > LEVEL_LIMIT || !bytes.starts_with(&[0x1f, 0x8b]) {
        return Err(failure("native level", "bounded gzip required").into());
    }
    let raw = Compression::Gzip
        .decode(bytes, LEVEL_LIMIT)
        .map_err(|e| failure("native level gzip", e))?;
    let (named, consumed) = nbt::read_named(
        &raw,
        Limits {
            max_bytes: LEVEL_LIMIT,
            max_nodes: 16_384,
            max_depth: 64,
        },
    )
    .map_err(|e| failure("native level NBT", e))?;
    if consumed != raw.len() || !named.name.0.is_empty() || !matches!(&named.tag, Tag::Compound(_))
    {
        return Err(failure(
            "native level",
            "empty named Compound with no trailing data required",
        )
        .into());
    }
    let Some(data @ Tag::Compound(_)) = named.tag.get("Data") else {
        return Err(failure("native level", "Data Compound required").into());
    };
    if data.get("DataVersion") != Some(&Tag::Int(DATA_VERSION))
        || !matches!(data.get("Version"), Some(Tag::Compound(_)))
        || data.get("Version").and_then(|v| v.get("Id")) != Some(&Tag::Int(DATA_VERSION))
        || !matches!(data.get("Time"), Some(Tag::Long(_)))
        || !matches!(data.get("LevelName"), Some(Tag::String(_)))
        || !matches!(data.get("GameType"), Some(Tag::Int(_)))
    {
        return Err(failure("native level", "exact current typed header required").into());
    }
    for old in [
        "Player",
        "WorldGenSettings",
        "DayTime",
        "SpawnX",
        "SpawnY",
        "SpawnZ",
        "SpawnAngle",
        "clearWeatherTime",
        "rainTime",
        "thunderTime",
        "raining",
        "thundering",
        "Difficulty",
        "hardcore",
        "GameRules",
        "DimensionData",
        "WanderingTraderSpawnChance",
        "WanderingTraderSpawnDelay",
        "WanderingTraderId",
        "BorderCenterX",
        "BorderCenterZ",
        "BorderSize",
        "BorderSafeZone",
        "BorderWarningBlocks",
        "BorderWarningTime",
        "BorderSizeLerpTime",
        "BorderSizeLerpTarget",
        "BorderDamagePerBlock",
        "DifficultyLocked",
        "ScheduledEvents",
        "CustomBossEvents",
        "DragonFight",
    ] {
        if data.get(old).is_some() {
            return Err(failure(
                "native level",
                format!("legacy metadata {old} requires migration"),
            )
            .into());
        }
    }
    // Deliberately adopt native normalization and last-duplicate-wins. There
    // is no re-encoding equality or canonical/duplicate-free NBT claim here.
    Ok(())
}
#[cfg(feature = "paired-packages")]
enum PreflightKind {
    New { encoded: Vec<u8> },
    Existing { identity: Vec<u8>, level: Vec<u8> },
}
#[cfg(feature = "paired-packages")]
pub(super) struct IdentityPreflight {
    world: WorldId,
    kind: PreflightKind,
}
#[cfg(feature = "paired-packages")]
impl IdentityPreflight {
    /// Build custody before acquisition. No authority is acquired by this helper.
    pub(super) fn retained_authority_acquisition(
        &self,
        namespace: &session_authority::AuthorityNamespace,
        root: &Path,
    ) -> Result<session_authority::RetainedAuthorityAcquisition> {
        if !matches!(&self.kind, PreflightKind::New { .. }) {
            return Err("managed reopen requires authoritative archive recovery".into());
        }
        namespace.check_new_world(root)?;
        namespace.retained_acquisition(self.world)
    }

    pub(super) fn finish_retained_authority(
        &self,
        namespace: &session_authority::AuthorityNamespace,
        acquisition: &mut session_authority::RetainedAuthorityAcquisition,
    ) -> Result<session_authority::WorldAuthority> {
        if !matches!(&self.kind, PreflightKind::New { .. }) {
            return Err("managed reopen requires authoritative archive recovery".into());
        }
        acquisition.finish(namespace, self.world)
    }

    /// All identity IO precedes extraction of the original ready native lock.
    /// On any error the caller still owns this preflight and the acquisition.
    pub(super) fn finish_retained(
        &self,
        root: &Path,
        native: &mut horizon_client::save::RetainedSessionLock,
    ) -> Result<WorldSession> {
        if !native.ready() {
            return Err("retained native lock is not ready for identity finish".into());
        }
        if let PreflightKind::New { encoded } = &self.kind {
            create_identity(root, encoded)?;
        }
        let bootstrap = match &self.kind {
            PreflightKind::New { .. } => Some(
                horizon_client::world::counter_restore::HostCounterBootstrap::for_new_host_world(
                    self.world,
                ),
            ),
            PreflightKind::Existing { .. } => None,
        };
        let lock = native.finish(root)?;
        // No fallible work follows the take of the actual native SessionLock.
        Ok(WorldSession {
            _lock: lock,
            authority: None,
            world: Some(self.world),
            counter_bootstrap: bootstrap,
        })
    }

    /// Called after the exact identity/native-level under-lock recheck and before
    /// any world recording or native streaming construction for recovery.
    pub(super) fn require_checkpoint_world(&self, expected: WorldId) -> Result<()> {
        if self.world != expected || !matches!(&self.kind, PreflightKind::Existing { .. }) {
            return Err(failure("checkpoint world", "trusted expected UUID mismatch").into());
        }
        Ok(())
    }

    pub(super) fn read(root: &Path, intent: IdentityIntent) -> Result<Option<Self>> {
        match intent {
            IdentityIntent::Legacy => Ok(None),
            IdentityIntent::CreateNewIdentity => {
                new_root(root)?;
                let (world, encoded) = allocate_identity()?;
                Ok(Some(Self {
                    world,
                    kind: PreflightKind::New { encoded },
                }))
            }
            IdentityIntent::RequireExistingIdentity => {
                safe_root(root)?;
                let identity = bounded(&root.join(IDENTITY_FILE), IDENTITY_LIMIT)?;
                let world = decode_identity(&identity)?;
                let level = bounded(&root.join("level.dat"), LEVEL_LIMIT)?;
                native_level(root, &level)?;
                Ok(Some(Self {
                    world,
                    kind: PreflightKind::Existing { identity, level },
                }))
            }
        }
    }
    pub(super) fn recheck(&self, root: &Path) -> Result<()> {
        match &self.kind {
            PreflightKind::New { .. } => new_root(root),
            PreflightKind::Existing { identity, level } => {
                safe_root(root)?;
                let next_identity = bounded(&root.join(IDENTITY_FILE), IDENTITY_LIMIT)?;
                let next_level = bounded(&root.join("level.dat"), LEVEL_LIMIT)?;
                decode_identity(&next_identity)?;
                native_level(root, &next_level)?;
                if &next_identity != identity || &next_level != level {
                    return Err(failure(
                        "under-lock recheck",
                        "original identity/native-level bytes changed",
                    )
                    .into());
                }
                Ok(())
            }
        }
    }
    pub(super) fn finish(self, root: &Path, lock: SessionLock) -> Result<WorldSession> {
        if let PreflightKind::New { encoded } = &self.kind {
            create_identity(root, encoded)?;
        }
        Ok(WorldSession {
            _lock: lock,
            authority: None,
            world: Some(self.world),
            counter_bootstrap: match self.kind {
                PreflightKind::New { .. } => Some(horizon_client::world::counter_restore::HostCounterBootstrap::for_new_host_world(self.world)),
                PreflightKind::Existing { .. } => None,
            },
        })
    }
}
#[cfg(feature = "paired-packages")]
trait RecordFile: Write {
    fn sync_record(&self) -> io::Result<()>;
}
#[cfg(feature = "paired-packages")]
impl RecordFile for File {
    fn sync_record(&self) -> io::Result<()> {
        self.sync_all()
    }
}
#[cfg(feature = "paired-packages")]
fn finish_identity(root: &Path, file: &mut impl RecordFile, bytes: &[u8]) -> Result<()> {
    file.write_all(bytes)
        .map_err(|e| failure("identity write (partial file retained)", e))?;
    file.sync_record()
        .map_err(|e| failure("identity file sync (file retained)", e))?;
    #[cfg(target_os = "linux")]
    File::open(root)
        .and_then(|directory| directory.sync_all())
        .map_err(|e| failure("identity parent sync (file retained)", e))?;
    // Only Linux parent-directory sync is qualified. Other platforms retain
    // native file-sync semantics without a directory-durability promise.
    Ok(())
}
#[cfg(feature = "paired-packages")]
fn create_identity(root: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join(IDENTITY_FILE))
        .map_err(|e| failure("exclusive identity create (prior artifacts retained)", e))?;
    finish_identity(root, &mut file, bytes)
}

#[cfg(all(test, feature = "paired-packages", target_os = "linux"))]
mod tests {
    use super::*;
    use horizon_client::save::Level;
    use std::{
        collections::BTreeMap,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "horizon-host-id-{}-{}-{label}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn native(root: &Path) -> Vec<u8> {
        let mut level = Level {
            name: "Host Identity".into(),
            seed: 17,
            time: 73,
            world_spawn: Some([0, 64, 0]),
            world_spawn_angle: [-0.0, -0.0],
            ..Level::default()
        };
        level.write(root).unwrap();
        fs::read(root.join("level.dat")).unwrap()
    }
    fn encode(named: &nbt::NamedTag) -> Vec<u8> {
        let raw = nbt::write_named(
            named,
            Limits {
                max_bytes: 2 << 20,
                max_nodes: 50_000,
                max_depth: 128,
            },
        )
        .unwrap();
        Compression::Gzip.encode(&raw).unwrap()
    }
    fn recorded(root: &Path) -> WorldSession {
        let preflight = IdentityPreflight::read(root, IdentityIntent::CreateNewIdentity)
            .unwrap()
            .unwrap();
        let lock = SessionLock::acquire(root).unwrap();
        preflight.recheck(root).unwrap();
        crate::game::mod_packages::Packages::default()
            .record_world(root)
            .unwrap();
        preflight.finish(root, lock).unwrap()
    }
    fn disk(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn visit(root: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in fs::read_dir(root).unwrap() {
                let path = entry.unwrap().path();
                let meta = fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    visit(&path, out);
                } else {
                    assert!(meta.is_file());
                    out.insert(path.clone(), fs::read(path).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        visit(root, &mut out);
        out
    }
    #[test]
    fn host_identity_json_closed_bounded_and_canonical_uuid() {
        let (world, encoded) = allocate_identity().unwrap();
        assert_eq!(decode_identity(&encoded).unwrap(), world);
        let id = world.to_string();
        for bytes in [
            format!("{{\"format\":1,\"format\":1,\"world_uuid\":\"{id}\"}}"),
            format!("{{\"format\":1,\"world_uuid\":\"{id}\",\"world_uuid\":\"{id}\"}}"),
            format!("{{\"format\":1,\"world_uuid\":\"{id}\",\"other\":0}}"),
            format!("{{\"format\":2,\"world_uuid\":\"{id}\"}}"),
            format!("{{\"format\":1.0,\"world_uuid\":\"{id}\"}}"),
            format!("{{\"format\":\"1\",\"world_uuid\":\"{id}\"}}"),
            "{}".into(),
            "null".into(),
            "[]".into(),
            "{\"format\":1,\"world_uuid\":0}".into(),
            "{\"format\":1,\"world_uuid\":\"00000000-0000-0000-0000-000000000000\"}".into(),
            "{\"format\":1,\"world_uuid\":\"ABCDEF12-3456-4789-8123-456789abcdef\"}".into(),
            "{\"format\":1,\"world_uuid\":\"00000000-0000-1000-8000-000000000001\"}".into(),
        ] {
            assert!(decode_identity(bytes.as_bytes()).is_err(), "{bytes}");
        }
        assert!(decode_identity(&encoded[..encoded.len() - 1]).is_err());
        assert!(decode_identity(&vec![b' '; IDENTITY_LIMIT + 1]).is_err());
        assert!(decode_identity(&[encoded.clone(), b"true".to_vec()].concat()).is_err());
    }
    #[test]
    fn host_identity_native_header_bounds_types_and_native_negative_zero() {
        let scratch = Scratch::new("native-header");
        let bytes = native(&scratch.0);
        let raw = Compression::Gzip.decode(&bytes, LEVEL_LIMIT).unwrap();
        assert!(
            raw.windows(10)
                .any(|w| w == [5, 0, 3, b'y', b'a', b'w', 0x80, 0, 0, 0]),
            "native writer preserves negative-zero yaw"
        );
        native_level(&scratch.0, &bytes).unwrap();
        assert_eq!(fs::read(scratch.0.join("level.dat")).unwrap(), bytes);
        assert_eq!(DATA_VERSION, 5023);
        let (base, _) = nbt::read_named(&raw, Limits::default()).unwrap();
        for (key, value) in [
            ("DataVersion", Tag::Long(i64::from(DATA_VERSION))),
            ("DataVersion", Tag::Int(DATA_VERSION - 1)),
            ("DataVersion", Tag::Int(DATA_VERSION + 1)),
            ("Version", Tag::Int(DATA_VERSION)),
            (
                "Version",
                Tag::compound([("Id", Tag::Long(i64::from(DATA_VERSION)))]),
            ),
            (
                "Version",
                Tag::compound([("Id", Tag::Int(DATA_VERSION - 1))]),
            ),
            ("Time", Tag::Int(73)),
            ("LevelName", Tag::Int(1)),
            ("GameType", Tag::Long(0)),
            ("Player", Tag::Compound(nbt::Compound::new())),
            ("WorldGenSettings", Tag::Compound(nbt::Compound::new())),
            ("DayTime", Tag::Long(1)),
        ] {
            let mut named = base.clone();
            let Some(Tag::Compound(data)) = named.tag.get_mut("Data") else {
                panic!()
            };
            data.insert(key.into(), value);
            assert!(native_level(&scratch.0, &encode(&named)).is_err(), "{key}");
        }
        for key in ["DataVersion", "Version", "Time", "LevelName", "GameType"] {
            let mut named = base.clone();
            let Some(Tag::Compound(data)) = named.tag.get_mut("Data") else {
                panic!()
            };
            data.remove(&nbt::JavaString::from(key));
            assert!(native_level(&scratch.0, &encode(&named)).is_err());
        }
        let mut named = base.clone();
        named.tag = Tag::List(Vec::new());
        assert!(native_level(&scratch.0, &encode(&named)).is_err());
        let mut named = base.clone();
        named.name = "named".into();
        assert!(native_level(&scratch.0, &encode(&named)).is_err());
        let mut named = base.clone();
        *named.tag.get_mut("Data").unwrap() = Tag::Int(1);
        assert!(native_level(&scratch.0, &encode(&named)).is_err());
        assert!(native_level(&scratch.0, &raw).is_err());
        assert!(native_level(&scratch.0, &bytes[..bytes.len() - 4]).is_err());
        let mut trailing = raw.clone();
        trailing.push(0);
        assert!(native_level(&scratch.0, &Compression::Gzip.encode(&trailing).unwrap()).is_err());
        assert!(native_level(&scratch.0, &vec![0; LEVEL_LIMIT + 1]).is_err());
        assert!(
            native_level(
                &scratch.0,
                &Compression::Gzip.encode(&vec![0; LEVEL_LIMIT + 1]).unwrap()
            )
            .is_err()
        );
        let mut named = base.clone();
        let Some(Tag::Compound(data)) = named.tag.get_mut("Data") else {
            panic!()
        };
        data.insert("many".into(), Tag::List(vec![Tag::Int(1); 17_000]));
        assert!(native_level(&scratch.0, &encode(&named)).is_err());
        let mut deep = Tag::Int(1);
        for _ in 0..70 {
            deep = Tag::compound([("x", deep)]);
        }
        let Some(Tag::Compound(data)) = named.tag.get_mut("Data") else {
            panic!()
        };
        data.insert("many".into(), deep);
        assert!(native_level(&scratch.0, &encode(&named)).is_err());
        let mut malformed = raw[..raw.len() - 2].to_vec();
        malformed.extend_from_slice(&[7, 0, 1, b'x', 0x7f, 0xff, 0xff, 0xff]);
        malformed.extend_from_slice(&raw[raw.len() - 2..]);
        assert!(native_level(&scratch.0, &Compression::Gzip.encode(&malformed).unwrap()).is_err());
        // Native duplicate semantics are intentional: the final typed version wins.
        let mut field = vec![3, 0, 11];
        field.extend_from_slice(b"DataVersion");
        field.extend_from_slice(&DATA_VERSION.to_be_bytes());
        let at = raw.windows(field.len()).position(|w| w == field).unwrap();
        let mut old = field.clone();
        let len = old.len();
        old[len - 4..].copy_from_slice(&(DATA_VERSION - 1).to_be_bytes());
        let mut duplicate = raw[..at].to_vec();
        duplicate.extend_from_slice(&old);
        duplicate.extend_from_slice(&raw[at..]);
        native_level(&scratch.0, &Compression::Gzip.encode(&duplicate).unwrap()).unwrap();
        for directory in ["region", "entities", "poi", "DIM-1", "DIM1"] {
            let legacy = scratch.0.join(directory);
            fs::create_dir(&legacy).unwrap();
            assert!(native_level(&scratch.0, &bytes).is_err());
            fs::remove_dir(&legacy).unwrap();
        }
        assert_eq!(fs::read(scratch.0.join("level.dat")).unwrap(), bytes);
    }
    #[test]
    fn host_identity_paths_lock_targets_and_new_root_refusals_preserve_bytes() {
        use std::os::unix::{fs::symlink, net::UnixListener};
        let scratch = Scratch::new("paths");
        let target = scratch.0.join("target");
        fs::create_dir(&target).unwrap();
        let sentinel = target.join("sentinel");
        fs::write(&sentinel, b"untouched target").unwrap();
        let link = scratch.0.join("link");
        symlink(&target, &link).unwrap();
        assert!(IdentityPreflight::read(&link, IdentityIntent::CreateNewIdentity).is_err());
        assert!(
            IdentityPreflight::read(&link.join("new"), IdentityIntent::CreateNewIdentity).is_err()
        );
        for kind in [0, 1, 2] {
            let root = scratch.0.join(format!("lock-{kind}"));
            fs::create_dir(&root).unwrap();
            let lock = root.join("session.lock");
            let socket = match kind {
                0 => {
                    symlink(&sentinel, &lock).unwrap();
                    None
                }
                1 => {
                    fs::create_dir(&lock).unwrap();
                    None
                }
                _ => Some(UnixListener::bind(&lock).unwrap()),
            };
            assert!(IdentityPreflight::read(&root, IdentityIntent::CreateNewIdentity).is_err());
            assert_eq!(fs::read(&sentinel).unwrap(), b"untouched target");
            drop(socket);
        }
        let root = scratch.0.join("nonempty");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("other"), b"retained").unwrap();
        let before = disk(&root);
        assert!(IdentityPreflight::read(&root, IdentityIntent::CreateNewIdentity).is_err());
        assert_eq!(disk(&root), before);
        let fresh = scratch.0.join("fresh");
        fs::create_dir(&fresh).unwrap();
        fs::write(fresh.join("session.lock"), [0x26, 0x03]).unwrap();
        IdentityPreflight::read(&fresh, IdentityIntent::CreateNewIdentity)
            .unwrap()
            .unwrap();
        assert!(!fresh.join(IDENTITY_FILE).exists());
    }
    #[test]
    fn host_identity_exact_under_lock_rechecks_and_context_release() {
        let scratch = Scratch::new("rechecks");
        let session = recorded(&scratch.0);
        let world = session.expected_world().unwrap().to_string();
        native(&scratch.0);
        assert!(SessionLock::acquire(&scratch.0).is_err());
        drop(session);
        let preflight =
            IdentityPreflight::read(&scratch.0, IdentityIntent::RequireExistingIdentity)
                .unwrap()
                .unwrap();
        let original = disk(&scratch.0);
        let lock = SessionLock::acquire(&scratch.0).unwrap();
        preflight.recheck(&scratch.0).unwrap();
        let session = preflight.finish(&scratch.0, lock).unwrap();
        assert_eq!(session.expected_world().unwrap().to_string(), world);
        assert_eq!(disk(&scratch.0), original);
        drop(session);
        for changed in [IDENTITY_FILE, "level.dat"] {
            let preflight =
                IdentityPreflight::read(&scratch.0, IdentityIntent::RequireExistingIdentity)
                    .unwrap()
                    .unwrap();
            let path = scratch.0.join(changed);
            let before = fs::read(&path).unwrap();
            let bytes = if changed == IDENTITY_FILE {
                [before.clone(), b" ".to_vec()].concat()
            } else {
                let mut tag = nbt::read_named(
                    &Compression::Gzip.decode(&before, LEVEL_LIMIT).unwrap(),
                    Limits::default(),
                )
                .unwrap()
                .0;
                let Some(Tag::Compound(data)) = tag.tag.get_mut("Data") else {
                    panic!()
                };
                data.insert("Time".into(), Tag::Long(74));
                encode(&tag)
            };
            fs::write(&path, &bytes).unwrap();
            let lock = SessionLock::acquire(&scratch.0).unwrap();
            assert!(preflight.recheck(&scratch.0).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
            drop(lock);
            fs::write(path, before).unwrap();
        }
        fs::remove_file(scratch.0.join(IDENTITY_FILE)).unwrap();
        let before = disk(&scratch.0);
        assert!(
            IdentityPreflight::read(&scratch.0, IdentityIntent::RequireExistingIdentity).is_err()
        );
        assert_eq!(disk(&scratch.0), before);
    }
    struct FaultFile {
        file: File,
        remaining: usize,
        sync_failure: bool,
    }
    impl Write for FaultFile {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::other("controlled partial write refusal"));
            }
            let n = self.file.write(&bytes[..bytes.len().min(self.remaining)])?;
            self.remaining -= n;
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.file.flush()
        }
    }
    impl RecordFile for FaultFile {
        fn sync_record(&self) -> io::Result<()> {
            if self.sync_failure {
                Err(io::Error::other("controlled file sync refusal"))
            } else {
                self.file.sync_all()
            }
        }
    }
    #[test]
    fn host_identity_exclusive_partial_write_and_sync_errors_retain_artifacts() {
        let scratch = Scratch::new("io-failures");
        let (_, bytes) = allocate_identity().unwrap();
        for (name, remaining, sync_failure) in [("partial", 7, false), ("sync", usize::MAX, true)] {
            let root = scratch.0.join(name);
            fs::create_dir(&root).unwrap();
            let path = root.join(IDENTITY_FILE);
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            let mut fault = FaultFile {
                file,
                remaining,
                sync_failure,
            };
            let error = finish_identity(&root, &mut fault, &bytes).unwrap_err();
            assert!(error.to_string().contains(if sync_failure {
                "identity file sync"
            } else {
                "identity write"
            }));
            let retained = fs::read(&path).unwrap();
            assert_eq!(
                retained,
                if sync_failure {
                    bytes.clone()
                } else {
                    bytes[..7].to_vec()
                }
            );
            assert!(create_identity(&root, &bytes).is_err());
            assert_eq!(fs::read(&path).unwrap(), retained);
            assert!(IdentityPreflight::read(&root, IdentityIntent::CreateNewIdentity).is_err());
            assert!(
                IdentityPreflight::read(&root, IdentityIntent::RequireExistingIdentity).is_err()
            );
        }
        let root = scratch.0.join("parent-sync");
        fs::create_dir(&root).unwrap();
        let path = root.join(IDENTITY_FILE);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let error = finish_identity(&root.join("absent-parent"), &mut file, &bytes).unwrap_err();
        assert!(error.to_string().contains("identity parent sync"));
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}
