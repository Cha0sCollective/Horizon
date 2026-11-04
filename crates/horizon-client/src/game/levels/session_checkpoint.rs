//! Synchronous, bounded native session images. There are no ordinary lifecycle callers.
#![allow(dead_code)] // Deliberately dormant internal primitive; no startup/admission hook.
use crate::game::{
    Dim, Game,
    lan::{self, LanHost, SessionPlayerRow},
    raids::validate_session_empty_raids,
    world_loading::WorldRequest,
};
use horizon_client::{
    chunk::Chunk,
    save::{self, LevelShape},
    world::counter_restore::{
        CounterFrameV2, CounterTarget, GuestCounterFrameV3, GuestRuntimeState, GuestStatus,
        NativeBinding, NativeBindingProfile, NativeRetirementReason, Placement, RuntimeCounterKey,
        WorldId,
    },
    world::slot_restore::{
        DecodedSlotFrame, SlotFrameAllocation, decode_guest_slot_frame, decode_slot_frame,
    },
    world_slot_trial::SlotKey,
};
use horizon_core::position::ChunkPos;
use horizon_world::{
    DATA_VERSION,
    compression::Compression,
    nbt::{self, Limits, NamedTag, Tag},
    region::RegionFile,
};
use serde::{
    Deserialize, Serialize,
    de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Instant,
};

const KIND: &str = "horizon-native-session-checkpoint";
const GUEST_KIND: &str = "horizon-registered-guest-session-checkpoint";
const COUNTER: &str = "host/counter.json";
const SLOT: &str = "host/slot.nbt";
const PIN: &str = "horizon-paired-packages.json";
const RAIDS: &str = "dimensions/minecraft/overworld/data/minecraft/raids.dat";
const ENTRY_LIMIT: usize = 256;
const FILE_LIMIT: usize = 32 << 20;
const IMAGE_LIMIT: u64 = 64 << 20;
const MANIFEST_LIMIT: usize = 256 << 10;
const STRUCTURED_LIMIT: usize = 1 << 20;
const NODES: usize = 16_384;

pub(in crate::game) type SResult<T> = std::result::Result<T, SessionCheckpointError>;
#[path = "session_checkpoint_items.rs"]
pub(in crate::game) mod items;
use items::{CheckedSessionItems, ItemProjection, read_session_checkpoint_items};

/// Actual immutable native decoder owners, with no Game, renderer, UI or worker
/// pointer. Only a Game can capture this context; recovery wrappers use the same
/// complete validators as the owned close worker.
#[derive(Clone)]
pub(in crate::game) struct SessionValidationContext {
    res: Arc<crate::game::Resources>,
    assets: Arc<horizon_client::assets::Assets>,
    packages: crate::game::mod_packages::Packages,
    advancement_tree: Arc<horizon_client::advancements::Tree>,
}
impl SessionValidationContext {
    fn from_game(game: &Game<'_>) -> Self {
        Self {
            res: game.res.clone(),
            assets: game.pack.assets.clone(),
            packages: game.options.mod_packages.clone(),
            advancement_tree: game.adv.tree.clone(),
        }
    }
    fn same_owners(&self, game: &Game<'_>) -> bool {
        Arc::ptr_eq(&self.res, &game.res)
            && Arc::ptr_eq(&self.assets, &game.pack.assets)
            && Arc::ptr_eq(&self.advancement_tree, &game.adv.tree)
            && match (self.packages.catalog(), game.options.mod_packages.catalog()) {
                (Some(before), Some(now)) => std::ptr::eq(before, now),
                (None, None) => true,
                _ => false,
            }
    }
    pub(in crate::game) fn validate_column(
        &self,
        named: &NamedTag,
        position: (i32, i32),
        clock: i64,
    ) -> SResult<Chunk> {
        validate_column_context(self, named, position, clock)
    }
    pub(in crate::game) fn validate_item_tag(
        &self,
        tag: &Tag,
    ) -> SResult<([i32; 4], items::ItemRow)> {
        items::validate_item_tag_context(self, tag)
    }
}
impl Game<'_> {
    pub(in crate::game) fn capture_native_validation_context(&self) -> SessionValidationContext {
        SessionValidationContext::from_game(self)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::game) enum Publication {
    NotPublished,
    Uncertain,
}
#[derive(Debug)]
pub(in crate::game) struct SessionCheckpointError {
    pub publication: Publication,
    pub detail: String,
    /// Explicit UUID preparation can remain after a later failed capture.
    pub item_identities_assigned: usize,
}
impl fmt::Display for SessionCheckpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "session checkpoint {:?} ({} item identities assigned): {}",
            self.publication, self.item_identities_assigned, self.detail
        )
    }
}
impl std::error::Error for SessionCheckpointError {}
pub(in crate::game) fn fail(detail: impl fmt::Display) -> SessionCheckpointError {
    SessionCheckpointError {
        publication: Publication::NotPublished,
        detail: detail.to_string(),
        item_identities_assigned: 0,
    }
}
pub(in crate::game) fn checked<T>(value: std::result::Result<T, impl fmt::Display>) -> SResult<T> {
    value.map_err(fail)
}
pub(in crate::game) fn require(ok: bool, detail: &str) -> SResult<()> {
    if ok { Ok(()) } else { Err(fail(detail)) }
}
fn deadline(until: Instant) -> SResult<()> {
    require(Instant::now() < until, "deadline expired")
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Member {
    path: String,
    bytes: u64,
    sha256: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlayerDescriptor {
    profile_uuid: String,
    record_uuid: [i32; 4],
    integrated_owner: bool,
    dimension: String,
}

// The runtime target stays independent of persistence traits. Decode only the
// bounded scalar fields, then use its existing checked constructor.
mod target_fields {
    use super::*;

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Fields {
        dimension: String,
        position: [i32; 3],
    }

    pub(super) fn serialize<S: serde::Serializer>(
        target: &CounterTarget,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        Fields {
            dimension: target.dimension.clone(),
            position: target.position,
        }
        .serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<CounterTarget, D::Error> {
        let fields = Fields::deserialize(deserializer)?;
        CounterTarget::new(&fields.dimension, fields.position).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: u32,
    kind: String,
    epoch: u64,
    world_uuid: WorldId,
    dimension: String,
    saved_clock: i64,
    #[serde(with = "target_fields")]
    target: CounterTarget,
    type_id: String,
    counter: String,
    slot: String,
    resident: Vec<(i32, i32)>,
    players: Vec<PlayerDescriptor>,
    directories: Vec<String>,
    files: Vec<Member>,
}
#[derive(Clone, Debug)]
pub(in crate::game) struct CheckpointReceipt {
    pub epoch: u64,
    pub root: PathBuf,
}
pub(in crate::game) struct RecoveryRequest {
    pub expected_world: WorldId,
    pub expected_target: CounterTarget,
    pub expected_type: String,
    pub archive_root: PathBuf,
    pub destination: PathBuf,
    pub deadline: Instant,
}
pub(in crate::game) struct RecoveryReceipt {
    pub epoch: u64,
    pub counter: RuntimeCounterKey,
    pub slot: SlotKey,
}
/// New registered profile; Dormant has no fabricated runtime allocation keys.
#[derive(Clone)]
pub(in crate::game) enum RegisteredRecoveryState {
    Active {
        counter: RuntimeCounterKey,
        slot: SlotKey,
    },
    Dormant,
}
pub(in crate::game) struct RegisteredRecoveryReceipt {
    pub epoch: u64,
    pub state: RegisteredRecoveryState,
}
/// Trusted coordinator routing, not a decoded image or an expected-UUID assertion.
pub(in crate::game) struct ManagedRecoveryRequest {
    pub owner_root: PathBuf,
    pub expected_target: CounterTarget,
    pub expected_type: String,
    pub archive_root: PathBuf,
    pub destination: PathBuf,
    pub deadline: Instant,
}
/// Captured from the actual source Game while its constructor, native session,
/// package generation and registered domain still exist. Not decoded authority.
pub(in crate::game) struct OrdinaryRecoverySource {
    context: SessionValidationContext,
    startup: Arc<crate::game::mod_packages::FrozenHostInputs>,
    namespace: crate::game::world_loading::host_identity::session_authority::AuthorityNamespace,
    root: PathBuf,
    archive: PathBuf,
    continuation: PathBuf,
    world: WorldId,
    target: CounterTarget,
    type_id: String,
    guest: bool,
    role: crate::game::execution_role::ExecutionRole,
    native_player_uuid: Option<[i32; 4]>,
}
pub(in crate::game) struct OrdinaryRecoveryRequest {
    source: OrdinaryRecoverySource,
    _closed: crate::game::paired_close::OrdinaryClosedWorld,
    managed: ManagedRecoveryRequest,
}
impl OrdinaryRecoverySource {
    pub(in crate::game) fn capture(
        game: &Game<'_>,
        namespace: crate::game::world_loading::host_identity::session_authority::AuthorityNamespace,
        archive: &Path,
        continuation: &Path,
    ) -> SResult<Self> {
        let world = game.session_context_cpu()?;
        require(
            game.level.player_uuid.is_some() && game.level.world_spawn.is_some(),
            "ordinary source requires its actual established native record UUID and spawn",
        )?;
        require(
            game.native_close_authority
                .as_ref()
                .is_some_and(|authority| authority.retains_ordinary_source(game)),
            "ordinary domain source requires its actual original native construction",
        )?;
        let counter = checked(SessionCounter::from_world(&game.world))?;
        require(
            checked(counter.binding_profile())? == NativeBindingProfile::RegisteredCube,
            "ordinary recovery source requires the registered physical profile",
        )?;
        let _ = binding(game, &counter)?;
        let archive = directory(archive)?;
        let continuation = directory(continuation)?;
        checked(namespace.check_new_world(&continuation))?;
        require(
            fs::read_dir(&continuation).map_err(fail)?.next().is_none()
                && [&archive, &game.world_root].iter().all(|root| {
                    !continuation.starts_with(root) && !root.starts_with(&continuation)
                })
                && !archive.starts_with(&game.world_root)
                && !game.world_root.starts_with(&archive),
            "ordinary source requires its fresh separate continuation/archive routes",
        )?;
        Ok(Self {
            context: SessionValidationContext::from_game(game),
            startup: game
                .ordinary_startup
                .as_ref()
                .ok_or_else(|| fail("source startup absent"))?
                .clone(),
            namespace,
            root: game.world_root.clone(),
            archive,
            continuation,
            world,
            target: checked(CounterTarget::new(&counter.dimension, counter.position))?,
            type_id: counter.type_id.clone(),
            guest: counter.is_guest(),
            role: game.options.execution_role,
            native_player_uuid: game.level.player_uuid,
        })
    }
    #[allow(clippy::result_large_err)]
    pub(in crate::game) fn into_request(
        self,
        closed: crate::game::paired_close::OrdinaryClosedWorld,
        destination: PathBuf,
        until: Instant,
    ) -> std::result::Result<
        OrdinaryRecoveryRequest,
        (Self, crate::game::paired_close::OrdinaryClosedWorld, String),
    > {
        if !closed.is_coherent_source(&self.root, self.world, &self.startup)
            || until <= Instant::now()
        {
            return Err((
                self,
                closed,
                "actual source coherent completion/route association differs".into(),
            ));
        }
        let managed = ManagedRecoveryRequest {
            owner_root: self.root.clone(),
            expected_target: self.target.clone(),
            expected_type: self.type_id.clone(),
            archive_root: self.archive.clone(),
            destination,
            deadline: until,
        };
        Ok(OrdinaryRecoveryRequest {
            source: self,
            _closed: closed,
            managed,
        })
    }
}
impl OrdinaryRecoveryRequest {
    pub(in crate::game) fn continuation_archive(&self) -> &Path {
        &self.source.continuation
    }
    pub(in crate::game) fn deadline(&self) -> Instant {
        self.managed.deadline
    }
}
/// A decoder cannot grant player installation authority. Only `verify_image`,
/// after complete physical/native/package/frame/player validation, constructs this.
pub(in crate::game) struct CheckedSessionPlayers {
    rows: Vec<SessionPlayerRow>,
}
impl CheckedSessionPlayers {
    pub(in crate::game) fn rows(&self) -> &[SessionPlayerRow] {
        &self.rows
    }
}

fn relative(path: &str) -> SResult<()> {
    require(
        !path.is_empty()
            && path.len() <= 1024
            && !path.contains('\\')
            && !path.contains(':')
            && !path.contains('\0'),
        "invalid relative member",
    )?;
    require(
        path.split('/').all(|p| {
            !p.is_empty() && p != "." && p != ".." && p.bytes().all(|b| b.is_ascii_graphic())
        }),
        "invalid relative components",
    )?;
    require(
        Path::new(path)
            .components()
            .all(|p| matches!(p, Component::Normal(_))),
        "non-normal member",
    )
}
fn ancestors(path: &Path) -> SResult<()> {
    require(
        path.is_absolute() && !path.components().any(|p| matches!(p, Component::ParentDir)),
        "absolute path without traversal required",
    )?;
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) => require(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "path ancestor is not a real directory",
            )?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(fail(e)),
        }
    }
    Ok(())
}
fn directory(path: &Path) -> SResult<PathBuf> {
    ancestors(path)?;
    let meta = checked(fs::symlink_metadata(path))?;
    require(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "real directory required",
    )?;
    checked(fs::canonicalize(path))
}
fn read(path: &Path, limit: usize) -> SResult<Vec<u8>> {
    let meta = checked(fs::symlink_metadata(path))?;
    require(
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= limit as u64,
        "member type/byte bound",
    )?;
    let mut bytes = Vec::new();
    checked(
        checked(File::open(path))?
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes),
    )?;
    require(
        bytes.len() <= limit && bytes.len() as u64 == meta.len(),
        "member changed or exceeded bound",
    )?;
    Ok(bytes)
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn member(root: &Path, path: String) -> SResult<Member> {
    relative(&path)?;
    let bytes = read(&root.join(&path), FILE_LIMIT)?;
    Ok(Member {
        path,
        bytes: bytes.len() as u64,
        sha256: digest(&bytes),
    })
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Tree {
    directories: Vec<String>,
    files: Vec<Member>,
}
fn tree(root: &Path, exclude: &[&str]) -> SResult<Tree> {
    fn walk(
        root: &Path,
        at: &Path,
        exclude: &[&str],
        out: &mut Tree,
        count: &mut usize,
        total: &mut u64,
    ) -> SResult<()> {
        let mut entries = Vec::new();
        for entry in checked(fs::read_dir(at))? {
            *count = count
                .checked_add(1)
                .ok_or_else(|| fail("tree entry overflow"))?;
            require(*count <= ENTRY_LIMIT, "tree entry bound")?;
            entries.push(checked(entry)?);
        }
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let name = checked(path.strip_prefix(root))?
                .to_str()
                .ok_or_else(|| fail("non-UTF8 member"))?
                .replace('\\', "/");
            relative(&name)?;
            let meta = checked(fs::symlink_metadata(&path))?;
            require(!meta.file_type().is_symlink(), "linked tree member")?;
            if exclude.contains(&name.as_str()) {
                require(meta.is_file(), "excluded member must be regular")?;
                continue;
            }
            if meta.is_dir() {
                out.directories.push(name);
                walk(root, &path, exclude, out, count, total)?;
            } else {
                require(meta.is_file(), "nonregular tree member")?;
                let row = member(root, name)?;
                *total = total
                    .checked_add(row.bytes)
                    .ok_or_else(|| fail("image size overflow"))?;
                require(*total <= IMAGE_LIMIT, "image size bound")?;
                out.files.push(row);
            }
        }
        Ok(())
    }
    let _ = directory(root)?;
    let mut result = Tree {
        directories: Vec::new(),
        files: Vec::new(),
    };
    walk(root, root, exclude, &mut result, &mut 0, &mut 0)?;
    result.directories.sort();
    result.files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(result)
}
fn write_new(path: &Path, bytes: &[u8]) -> SResult<()> {
    require(bytes.len() <= FILE_LIMIT, "write byte bound")?;
    let mut file = checked(OpenOptions::new().write(true).create_new(true).open(path))?;
    checked(file.write_all(bytes))?;
    checked(file.sync_all())
}
fn overlay(path: &Path, bytes: &[u8]) -> SResult<()> {
    if path.try_exists().map_err(fail)? {
        let meta = checked(fs::symlink_metadata(path))?;
        require(
            meta.is_file() && !meta.file_type().is_symlink(),
            "overlay member type",
        )?;
        // This is the unpublished physical copy, never the working world.
        checked(fs::remove_file(path))?;
    }
    write_new(path, bytes)
}
fn parents(root: &Path, name: &str) -> SResult<PathBuf> {
    relative(name)?;
    let target = root.join(name);
    if let Some(parent) = target.parent() {
        let mut missing = Vec::new();
        let mut at = parent;
        while !at.try_exists().map_err(fail)? {
            missing.push(at.to_owned());
            at = at.parent().ok_or_else(|| fail("parent"))?;
        }
        let _ = directory(at)?;
        for path in missing.into_iter().rev() {
            checked(fs::create_dir(path))?;
        }
    }
    Ok(target)
}
fn copy_tree(source: &Path, target: &Path, inventory: &Tree, until: Instant) -> SResult<()> {
    checked(fs::create_dir(target))?;
    for name in &inventory.directories {
        deadline(until)?;
        checked(fs::create_dir(target.join(name)))?;
    }
    for row in &inventory.files {
        deadline(until)?;
        let bytes = read(&source.join(&row.path), FILE_LIMIT)?;
        require(
            bytes.len() as u64 == row.bytes && digest(&bytes) == row.sha256,
            "copy source changed",
        )?;
        write_new(&target.join(&row.path), &bytes)?;
        require(
            member(target, row.path.clone())? == *row,
            "copy member mismatch",
        )?;
    }
    Ok(())
}
fn sync_dir(path: &Path) -> SResult<()> {
    checked(checked(File::open(path))?.sync_all())
}
fn sync_tree(root: &Path) -> SResult<()> {
    let inventory = tree(root, &[])?;
    for row in &inventory.files {
        checked(checked(File::open(root.join(&row.path)))?.sync_all())?;
    }
    for name in inventory.directories.iter().rev() {
        sync_dir(&root.join(name))?;
    }
    sync_dir(root)
}

struct JsonBudget {
    nodes: usize,
}
struct JsonSeed<'a> {
    budget: &'a mut JsonBudget,
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for JsonSeed<'_> {
    type Value = Value;
    fn deserialize<D: de::Deserializer<'de>>(
        self,
        decoder: D,
    ) -> std::result::Result<Value, D::Error> {
        if self.depth > 32 || self.budget.nodes >= NODES {
            return Err(de::Error::custom("JSON structural bound"));
        }
        self.budget.nodes += 1;
        decoder.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for JsonSeed<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bounded duplicate-free JSON")
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_unit<E: de::Error>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Value, E> {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("nonfinite JSON"))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Value, E> {
        if v.len() > 4096 {
            Err(E::custom("JSON string bound"))
        } else {
            Ok(v.into())
        }
    }
    fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Value, E> {
        self.visit_str(&v)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Value, A::Error> {
        let mut out = Vec::new();
        while let Some(v) = seq.next_element_seed(JsonSeed {
            budget: &mut *self.budget,
            depth: self.depth + 1,
        })? {
            out.push(v);
        }
        Ok(Value::Array(out))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Value, A::Error> {
        let mut out = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if key.len() > 4096 || out.contains_key(&key) || self.budget.nodes >= NODES {
                return Err(de::Error::custom("JSON key/duplicate bound"));
            }
            self.budget.nodes += 1;
            out.insert(
                key,
                map.next_value_seed(JsonSeed {
                    budget: &mut *self.budget,
                    depth: self.depth + 1,
                })?,
            );
        }
        Ok(Value::Object(out))
    }
}
fn json<T: DeserializeOwned>(bytes: &[u8], cap: usize) -> SResult<T> {
    require(!bytes.is_empty() && bytes.len() <= cap, "JSON byte bound")?;
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let value = checked(
        JsonSeed {
            budget: &mut JsonBudget { nodes: 0 },
            depth: 0,
        }
        .deserialize(&mut decoder),
    )?;
    checked(decoder.end())?;
    checked(serde_json::from_value(value))
}
struct BoundedWriter {
    bytes: Vec<u8>,
    cap: usize,
}
impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.cap.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("serialization byte bound"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn json_bytes(value: &impl Serialize, cap: usize) -> SResult<Vec<u8>> {
    let mut writer = BoundedWriter {
        bytes: Vec::new(),
        cap,
    };
    checked(serde_json::to_writer(&mut writer, value))?;
    let _: Value = json(&writer.bytes, cap)?;
    Ok(writer.bytes)
}
fn limits() -> Limits {
    Limits {
        max_bytes: STRUCTURED_LIMIT,
        max_nodes: NODES,
        max_depth: 32,
    }
}
/// Validate the binary tree before the native reader's last-key-wins and
/// mixed-list compatibility rules can conceal duplicate or malformed structure.
struct NbtScan<'a> {
    bytes: &'a [u8],
    at: usize,
    nodes: usize,
    exact_items: bool,
    native_opaque: bool,
    entity_list_depth: Option<usize>,
}
impl NbtScan<'_> {
    fn take(&mut self, n: usize) -> SResult<&[u8]> {
        let end = self
            .at
            .checked_add(n)
            .ok_or_else(|| fail("NBT length overflow"))?;
        require(end <= self.bytes.len(), "truncated NBT")?;
        let old = self.at;
        self.at = end;
        Ok(&self.bytes[old..end])
    }
    fn byte(&mut self) -> SResult<u8> {
        Ok(self.take(1)?[0])
    }
    fn count(&mut self) -> SResult<usize> {
        let value = i32::from_be_bytes(self.take(4)?.try_into().unwrap());
        require((0..=NODES as i32).contains(&value), "NBT array/list bound")?;
        Ok(value as usize)
    }
    fn string(&mut self) -> SResult<nbt::JavaString> {
        let len = u16::from_be_bytes(self.take(2)?.try_into().unwrap()) as usize;
        require(
            len <= if self.exact_items || self.native_opaque {
                4096 * 3
            } else {
                4096
            },
            "NBT string bound",
        )?;
        let value = self.take(len)?;
        let mut encoded = vec![8, 0, 0];
        encoded.extend_from_slice(&(len as u16).to_be_bytes());
        encoded.extend_from_slice(value);
        let (named, used) = checked(nbt::read_named(&encoded, limits()))?;
        require(used == encoded.len(), "NBT string trailing bytes")?;
        if self.exact_items || self.native_opaque {
            require(
                checked(nbt::write_named(&named, limits()))? == encoded,
                "noncanonical/malformed native item modified UTF-8",
            )?;
        }
        let Tag::String(value) = named.tag else {
            return Err(fail("NBT string"));
        };
        require(
            value.0.len() <= 4096
                && (self.native_opaque
                    || char::decode_utf16(value.0.iter().copied()).all(|v| v.is_ok())),
            "invalid UTF16",
        )?;
        Ok(value)
    }
    fn payload(&mut self, id: u8, depth: usize) -> SResult<()> {
        self.nodes += 1;
        require(self.nodes <= NODES && depth <= 32, "NBT structural bound")?;
        match id {
            1 => {
                self.take(1)?;
            }
            2 => {
                self.take(2)?;
            }
            3 => {
                self.take(4)?;
            }
            4 => {
                self.take(8)?;
            }
            5 => {
                let bits = u32::from_be_bytes(self.take(4)?.try_into().unwrap());
                require(
                    self.native_opaque || f32::from_bits(bits).is_finite(),
                    "nonfinite NBT float",
                )?;
                require(
                    !self.exact_items || bits != (-0.0f32).to_bits(),
                    "native item float signed-zero coercion",
                )?;
            }
            6 => {
                let bits = u64::from_be_bytes(self.take(8)?.try_into().unwrap());
                require(
                    self.native_opaque || f64::from_bits(bits).is_finite(),
                    "nonfinite NBT double",
                )?;
                require(
                    !self.exact_items || bits != (-0.0f64).to_bits(),
                    "native item double signed-zero coercion",
                )?;
            }
            7 | 11 | 12 => {
                let n = self.count()?;
                self.take(
                    n * match id {
                        7 => 1,
                        11 => 4,
                        _ => 8,
                    },
                )?;
            }
            8 => {
                self.string()?;
            }
            9 => {
                let kind = self.byte()?;
                let n = self.count()?;
                require(kind <= 12 && (kind != 0 || n == 0), "NBT list type")?;
                if self.entity_list_depth == Some(depth) {
                    require(
                        kind == 10 || (n == 0 && kind == 0),
                        "native entity compound list type",
                    )?;
                }
                for _ in 0..n {
                    self.payload(kind, depth + 1)?;
                }
            }
            10 => {
                let mut names = BTreeSet::new();
                loop {
                    let kind = self.byte()?;
                    if kind == 0 {
                        break;
                    }
                    let key = self.string()?;
                    if self.entity_list_depth == depth.checked_sub(1)
                        && self.entity_list_depth.is_some()
                    {
                        require(
                            items::native_item_field(&key),
                            "unknown raw native entity field/wrapper",
                        )?;
                    }
                    let entity_list =
                        self.exact_items && depth == 0 && key == nbt::JavaString::from("Entities");
                    require(names.insert(key), "duplicate NBT key")?;
                    let previous = self.entity_list_depth;
                    if entity_list {
                        self.entity_list_depth = Some(depth + 1);
                    }
                    self.payload(kind, depth + 1)?;
                    self.entity_list_depth = previous;
                }
            }
            _ => return Err(fail("invalid NBT tag type")),
        }
        Ok(())
    }
}
pub(in crate::game) fn raw_nbt(raw: &[u8]) -> SResult<NamedTag> {
    raw_nbt_profile(raw, false)
}
fn raw_item_nbt(raw: &[u8]) -> SResult<NamedTag> {
    raw_nbt_profile(raw, true)
}
fn raw_nbt_profile(raw: &[u8], exact_items: bool) -> SResult<NamedTag> {
    raw_nbt_profile_native(raw, exact_items, false)
}
fn raw_native_nbt(raw: &[u8]) -> SResult<NamedTag> {
    raw_nbt_profile_native(raw, false, true)
}
fn raw_nbt_profile_native(raw: &[u8], exact_items: bool, native_opaque: bool) -> SResult<NamedTag> {
    require(
        !raw.is_empty() && raw.len() <= STRUCTURED_LIMIT,
        "NBT byte bound",
    )?;
    let mut scanner = NbtScan {
        bytes: raw,
        at: 0,
        nodes: 0,
        exact_items,
        native_opaque,
        entity_list_depth: None,
    };
    let kind = scanner.byte()?;
    require(kind == 10, "NBT compound root required")?;
    scanner.string()?;
    scanner.payload(kind, 0)?;
    require(scanner.at == raw.len(), "trailing NBT")?;
    let (named, used) = checked(nbt::read_named(raw, limits()))?;
    require(
        used == raw.len() && (native_opaque || named.name.0.is_empty()),
        "trailing or nonempty named NBT root",
    )?;
    Ok(named)
}
fn compressed_nbt(bytes: &[u8]) -> SResult<Tag> {
    Ok(raw_nbt(&checked(Compression::Gzip.decode(bytes, STRUCTURED_LIMIT))?)?.tag)
}
fn nbt_bytes(tag: &Tag) -> SResult<Vec<u8>> {
    let raw = checked(nbt::write_named(
        &NamedTag {
            name: "".into(),
            tag: tag.clone(),
        },
        limits(),
    ))?;
    let decoded = raw_nbt(&raw)?;
    require(
        lan::session_player_tags_equal(&decoded.tag, tag),
        "NBT encoding changed semantic values",
    )?;
    checked(Compression::Gzip.encode(&raw))
}
fn compound<'a>(tag: &'a Tag, allowed: &[&str]) -> SResult<&'a nbt::Compound> {
    let Tag::Compound(map) = tag else {
        return Err(fail("compound type required"));
    };
    require(
        map.keys()
            .all(|key| allowed.iter().any(|v| key == &nbt::JavaString::from(*v))),
        "unknown NBT field",
    )?;
    Ok(map)
}
fn text(tag: Option<&Tag>) -> SResult<String> {
    match tag {
        Some(Tag::String(value)) => checked(value.to_string()),
        _ => Err(fail("String type required")),
    }
}
fn list(tag: Option<&Tag>) -> SResult<&[Tag]> {
    match tag {
        Some(Tag::List(value)) => Ok(value),
        _ => Err(fail("List type required")),
    }
}
fn empty_list(tag: Option<&Tag>) -> SResult<()> {
    require(list(tag)?.is_empty(), "nonempty native owner/tick list")
}
fn uuid_text(value: [i32; 4]) -> String {
    let bytes = value
        .into_iter()
        .flat_map(i32::to_be_bytes)
        .collect::<Vec<_>>();
    bytes
        .iter()
        .enumerate()
        .fold(String::new(), |mut out, (i, b)| {
            use fmt::Write;
            if [4, 6, 8, 10].contains(&i) {
                out.push('-');
            }
            write!(&mut out, "{b:02x}").unwrap();
            out
        })
}
fn descriptor(row: &SessionPlayerRow) -> PlayerDescriptor {
    PlayerDescriptor {
        profile_uuid: format!("{:032x}", row.profile_uuid),
        record_uuid: row.record_uuid,
        integrated_owner: row.integrated_owner,
        dimension: row.dimension.clone(),
    }
}
fn player_path(row: &PlayerDescriptor, kind: &str) -> String {
    let name = uuid_text(row.record_uuid);
    let extension = if kind == "data" { "dat" } else { "json" };
    format!("world/players/{kind}/{name}.{extension}")
}

/// Read ALL raw locations before `RegionFile::open` can pad/erase corrupt ones.
fn raw_region(bytes: &[u8]) -> SResult<Vec<usize>> {
    horizon_client::save::retained_region_locations(bytes).map_err(fail)
}
fn packed(
    tag: &Tag,
    count: usize,
    min_bits: usize,
    mut palette_value: impl FnMut(&Tag) -> SResult<()>,
) -> SResult<()> {
    compound(tag, &["palette", "data"])?;
    let palette = list(tag.get("palette"))?;
    require(
        !palette.is_empty() && palette.len() <= count,
        "palette size",
    )?;
    let mut unique = BTreeSet::new();
    for value in palette {
        palette_value(value)?;
        let encoded = checked(nbt::write_named(
            &NamedTag {
                name: "".into(),
                tag: Tag::compound([("value", value.clone())]),
            },
            limits(),
        ))?;
        require(unique.insert(encoded), "duplicate palette entry")?;
    }
    if palette.len() == 1 {
        return require(
            tag.get("data").is_none(),
            "singleton palette data must be absent",
        );
    }
    let bits = (usize::BITS - (palette.len() - 1).leading_zeros()) as usize;
    let bits = bits.max(min_bits);
    let per = 64 / bits;
    let words = count.div_ceil(per);
    let Some(Tag::LongArray(data)) = tag.get("data") else {
        return Err(fail("packed LongArray required"));
    };
    require(data.len() == words, "packed data length")?;
    let mask = (1u64 << bits) - 1;
    for (word_index, word) in data.iter().enumerate() {
        let mut value = *word as u64;
        let cells = per.min(count - word_index * per);
        for _ in 0..cells {
            require(
                (value & mask) < palette.len() as u64,
                "packed palette index",
            )?;
            value >>= bits;
        }
        require(value == 0, "nonzero unused packed bits")?;
    }
    Ok(())
}
pub(in crate::game) fn validate_column(
    game: &Game<'_>,
    named: &NamedTag,
    position: (i32, i32),
    clock: i64,
) -> SResult<Chunk> {
    validate_column_context(
        &SessionValidationContext::from_game(game),
        named,
        position,
        clock,
    )
}
fn validate_column_context(
    context: &SessionValidationContext,
    named: &NamedTag,
    position: (i32, i32),
    clock: i64,
) -> SResult<Chunk> {
    let tag = &named.tag;
    compound(
        tag,
        &[
            "DataVersion",
            "xPos",
            "zPos",
            "yPos",
            "Status",
            "LastUpdate",
            "isLightOn",
            "sections",
            "block_entities",
            "entities",
            "Heightmaps",
            "block_ticks",
            "fluid_ticks",
            "InhabitedTime",
            "structures",
            "PostProcessing",
        ],
    )?;
    require(
        tag.get("DataVersion") == Some(&Tag::Int(DATA_VERSION))
            && tag.get("xPos") == Some(&Tag::Int(position.0))
            && tag.get("zPos") == Some(&Tag::Int(position.1))
            && tag.get("yPos") == Some(&Tag::Int(-4)),
        "native column coordinates/version/types",
    )?;
    require(
        text(tag.get("Status"))? == "minecraft:full",
        "TestTerrain cannot restore saved proto status",
    )?;
    require(
        matches!(tag.get("LastUpdate"),Some(Tag::Long(v)) if *v>=0&&*v<=clock)
            && matches!(tag.get("InhabitedTime"),Some(Tag::Long(v)) if *v>=0),
        "native saved timestamps",
    )?;
    require(
        matches!(tag.get("isLightOn"), Some(Tag::Byte(0 | 1))),
        "native light flag type",
    )?;
    empty_list(tag.get("block_entities"))?;
    if tag.get("entities").is_some() {
        empty_list(tag.get("entities"))?;
    }
    match (tag.get("block_ticks"), tag.get("fluid_ticks")) {
        (None, None) => {}
        (Some(a), Some(b)) => {
            empty_list(Some(a))?;
            empty_list(Some(b))?;
        }
        _ => return Err(fail("native tick table pair")),
    }
    let structures = tag
        .get("structures")
        .ok_or_else(|| fail("native structures absent"))?;
    let values = compound(structures, &["References", "starts"])?;
    require(values.len() == 2, "native structure keys")?;
    for key in ["References", "starts"] {
        require(
            matches!(structures.get(key),Some(Tag::Compound(v)) if v.is_empty()),
            "native nonempty/malformed structure owner",
        )?;
    }
    let post = list(tag.get("PostProcessing"))?;
    require(post.len() == 24, "native postprocessing height")?;
    for row in post {
        empty_list(Some(row))?;
    }
    if let Some(heightmaps) = tag.get("Heightmaps") {
        let maps = compound(
            heightmaps,
            &[
                "WORLD_SURFACE",
                "OCEAN_FLOOR",
                "MOTION_BLOCKING",
                "MOTION_BLOCKING_NO_LEAVES",
            ],
        )?;
        require(maps.len() == 4, "native heightmap keys")?;
        for value in maps.values() {
            require(
                matches!(value,Tag::LongArray(v) if v.len()==37),
                "native heightmap LongArray size",
            )?;
        }
        require(
            tag.get("block_ticks").is_some(),
            "heightmaps require generated save tables",
        )?;
    } else {
        require(
            tag.get("block_ticks").is_none(),
            "table-less native encoder profile",
        )?;
    }
    let sections = list(tag.get("sections"))?;
    require(sections.len() == 24, "native full section count")?;
    let mut ys = BTreeSet::new();
    for section in sections {
        compound(
            section,
            &["Y", "block_states", "biomes", "SkyLight", "BlockLight"],
        )?;
        let Some(Tag::Byte(y)) = section.get("Y") else {
            return Err(fail("native section Y type"));
        };
        require(
            (-4..20).contains(y) && ys.insert(*y),
            "native section Y range/duplicate",
        )?;
        packed(
            section
                .get("block_states")
                .ok_or_else(|| fail("block states absent"))?,
            4096,
            4,
            |state| {
                let state = match state {
                    Tag::String(name) => {
                        let name = checked(name.to_string())?;
                        return require(
                            context.assets.registry.default_state(&name).is_some(),
                            "unresolved default native state",
                        );
                    }
                    Tag::Compound(_) => {
                        compound(state, &["id", "properties"])?;
                        let name = text(state.get("id"))?;
                        let mut properties = BTreeMap::new();
                        if let Some(Tag::Compound(values)) = state.get("properties") {
                            for (key, value) in values {
                                properties.insert(checked(key.to_string())?, text(Some(value))?);
                            }
                        } else {
                            require(state.get("properties").is_none(), "block properties type")?;
                        }
                        horizon_world::chunk::BlockState { name, properties }
                    }
                    _ => return Err(fail("block palette entry type")),
                };
                require(
                    context.assets.registry.id(&state).is_some(),
                    "unresolved native palette state",
                )
            },
        )?;
        packed(
            section.get("biomes").ok_or_else(|| fail("biomes absent"))?,
            64,
            1,
            |biome| {
                let name = text(Some(biome))?;
                let id = context.res.biomes().id(&name);
                require(
                    context.res.biomes().get(id).name == name,
                    "unresolved native biome",
                )
            },
        )?;
        for key in ["SkyLight", "BlockLight"] {
            if let Some(value) = section.get(key) {
                require(
                    matches!(value,Tag::ByteArray(v) if v.len()==2048),
                    "native nibble light size",
                )?;
                require(
                    tag.get("isLightOn") == Some(&Tag::Byte(1)),
                    "lightless native section cannot contain light",
                )?;
            }
        }
    }
    let mut chunk = save::decode(named, context.assets.registry, context.res.biomes())
        .ok_or_else(|| fail("native full decoder refused"))?;
    save::load_light(tag, &mut chunk, LevelShape::OVERWORLD.sky_light);
    require(
        chunk.x == position.0 && chunk.z == position.1,
        "native decoded coordinates",
    )?;
    // Palettes/indices were independently checked above. A fresh native re-encode
    // also proves native decoding did not silently drop a supported section value.
    let tables = save::SaveTables::new(&context.assets);
    let tables = tag.get("Heightmaps").is_some().then_some(&tables);
    let encoded = save::encode_with(
        &chunk,
        context.assets.registry,
        context.res.biomes(),
        tables,
        LevelShape::OVERWORLD,
    );
    if tag.get("Heightmaps").is_some() {
        require(
            tag.get("Heightmaps") == encoded.tag.get("Heightmaps"),
            "native heightmap semantic projection",
        )?;
    }
    let encoded_sections = list(encoded.tag.get("sections"))?;
    require(
        encoded_sections.len() == sections.len(),
        "native section roundtrip size",
    )?;
    for (before, after) in sections.iter().zip(encoded_sections) {
        for key in ["Y", "biomes", "SkyLight", "BlockLight"] {
            require(
                before.get(key) == after.get(key),
                "native section semantic roundtrip",
            )?;
        }
        // Default block states legitimately encode as strings with save tables
        // and compounds without them. Compare decoded physical cells instead of
        // requiring a single palette spelling; the raw table remains closed.
    }
    Ok(chunk)
}
fn columns(game: &Game<'_>, root: &Path, clock: i64) -> SResult<BTreeMap<(i32, i32), Arc<Chunk>>> {
    columns_context(&SessionValidationContext::from_game(game), root, clock)
}
fn columns_context(
    context: &SessionValidationContext,
    root: &Path,
    clock: i64,
) -> SResult<BTreeMap<(i32, i32), Arc<Chunk>>> {
    let inventory = tree(root, &["session.lock"])?;
    let mut result = BTreeMap::new();
    for row in &inventory.files {
        require(
            !row.path.ends_with(".mcc"),
            "external MCC members unsupported",
        )?;
        if !row.path.ends_with(".mca") {
            continue;
        }
        let (entities, region_position) = items::native_region(&row.path)?;
        if entities {
            continue;
        } // The complete item reader validates this family.
        let path = root.join(&row.path);
        let bytes = read(&path, FILE_LIMIT)?;
        let entries = raw_region(&bytes)?;
        // Only NOW may the native constructor/enumerator see this file.
        let mut region = checked(RegionFile::open(&path))?;
        let native: Vec<_> = region.chunks().map(|(at, _)| at).collect();
        require(
            native.len() == entries.len(),
            "native enumeration/raw header mismatch",
        )?;
        for index in entries {
            let at = (
                region_position.x * 32 + (index % 32) as i32,
                region_position.z * 32 + (index / 32) as i32,
            );
            let record = checked(region.read(ChunkPos::new(at.0, at.1)))?
                .ok_or_else(|| fail("raw native entry vanished"))?;
            let raw = checked(
                record
                    .compression
                    .decode(&record.compressed, STRUCTURED_LIMIT),
            )?;
            let named = raw_nbt(&raw)?;
            let chunk = validate_column_context(context, &named, at, clock)?;
            require(
                result.insert(at, Arc::new(chunk)).is_none(),
                "duplicate native column",
            )?;
        }
        require(
            member(root, row.path.clone())? == *row,
            "native region changed while reading",
        )?;
    }
    Ok(result)
}
fn binding(game: &Game<'_>, frame: &SessionCounter) -> SResult<NativeBinding> {
    binding_context(&SessionValidationContext::from_game(game), frame)
}
fn binding_context(
    context: &SessionValidationContext,
    frame: &SessionCounter,
) -> SResult<NativeBinding> {
    let catalog = context
        .packages
        .catalog()
        .ok_or_else(|| fail("captured package catalog missing"))?;
    let mut matches = catalog
        .prepared()
        .values()
        .filter(|p| p.content().machine_types().get(&frame.type_id).is_some());
    let prepared = matches
        .next()
        .ok_or_else(|| fail("prepared machine type absent"))?;
    require(matches.next().is_none(), "ambiguous prepared machine type")?;
    if let SessionCounter::Guest(guest) = frame {
        let binding = checked(NativeBinding::from_registered_guest_prepared(
            prepared,
            &frame.type_id,
            context.assets.registry,
        ))?;
        let identity = binding
            .package_identity()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        require(
            frame.package_identity == identity
                && guest.execution.primary_owner == prepared.bundle().content_owner().id
                && frame.binding.block_id == binding.block_id(),
            "Guest capture/binding mismatch",
        )?;
        return Ok(binding);
    }
    let binding = checked(match checked(frame.binding_profile())? {
        NativeBindingProfile::LegacyIron => {
            NativeBinding::from_prepared(prepared, &frame.type_id, context.assets.registry)
        }
        NativeBindingProfile::RegisteredCube => NativeBinding::from_registered_prepared(
            prepared,
            &frame.type_id,
            context.assets.registry,
        ),
    })?;
    let package = binding
        .package_identity()
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect::<String>();
    require(
        frame.package_identity == package
            && frame.type_id == binding.type_id()
            && frame.binding.block_id == binding.block_id()
            && frame.observed_state.properties.is_empty(),
        "physical binding/package mismatch",
    )?;
    Ok(binding)
}
fn correlate(
    manifest: &Manifest,
    counter: &SessionCounter,
    slot: &DecodedSlotFrame,
) -> SResult<()> {
    checked(counter.validate())?;
    require(
        counter.is_guest() == slot.is_guest()
            && (manifest.kind == GUEST_KIND) == counter.is_guest(),
        "counter/slot/envelope execution profiles differ",
    )?;
    let profile = checked(counter.binding_profile())?;
    require(
        slot.binding_profile() == profile,
        "counter/slot physical profiles differ",
    )?;
    let view = slot.view();
    match (counter.placement, &view.allocation) {
        (
            Placement::Active { generation, .. },
            SlotFrameAllocation::Active {
                machine_generation, ..
            },
        ) => {
            require(
                *machine_generation == generation,
                "machine/slot active generation mismatch",
            )?;
        }
        (Placement::Dormant, SlotFrameAllocation::Dormant)
            if profile == NativeBindingProfile::RegisteredCube => {}
        _ => return Err(fail("healthy paired allocation required")),
    }
    require(
        counter
            .last_retired
            .as_ref()
            .is_none_or(|retired| retired.reason == NativeRetirementReason::BlockChanged)
            && counter.world_uuid == manifest.world_uuid
            && counter.dimension == manifest.dimension
            && counter.position == manifest.target.position
            && counter.type_id == manifest.type_id
            && counter.saved_game_time == manifest.saved_clock,
        "counter/envelope correlation",
    )?;
    require(
        manifest.target.dimension == manifest.dimension
            && view.world == counter.world_uuid
            && view.dimension == counter.dimension
            && view.position == counter.position
            && view.package_identity == counter.package_identity
            && view.type_id == counter.type_id
            && view.block_id == counter.binding.block_id
            && view.save_schema == counter.save_schema
            && view.observed_state == counter.observed_state.name
            && view.saved_game_time == counter.saved_game_time
            && view.machine_high_water == counter.allocation_high_water,
        "slot/counter correlation",
    )
}
fn level_clock(root: &Path, expected: i64) -> SResult<()> {
    let level = compressed_nbt(&read(&root.join("level.dat"), STRUCTURED_LIMIT)?)?;
    require(
        level.get("Data").and_then(|data| data.get("Time")) == Some(&Tag::Long(expected)),
        "native typed saved clock mismatch",
    )?;
    require(
        save::Level::read(root)
            .is_some_and(|level| level.time == expected && level.dimension == Dim::Overworld.id()),
        "native level decode/dimension mismatch",
    )
}
fn raid_metadata(root: &Path) -> SResult<Option<Tag>> {
    let mut current = None;
    // Validate history independently before a native rotation can hide invalid
    // input. A backup is never a substitute for the current primary record.
    for name in [RAIDS.to_owned(), format!("{RAIDS}_old")] {
        let path = root.join(&name);
        if path.try_exists().map_err(fail)? {
            let tag = compressed_nbt(&read(&path, STRUCTURED_LIMIT)?)?;
            checked(validate_session_empty_raids(&tag))?;
            if name == RAIDS {
                current = Some(tag);
            }
        }
    }
    Ok(current)
}
fn sidecars(root: &Path, players: &[PlayerDescriptor]) -> SResult<()> {
    sidecars_with_initial_owner(root, players, None)
}
// Only the first pre-final read can supply this captured original owner UUID.
// Final images and recovery always enter through strict sidecars().
fn sidecars_with_initial_owner(
    root: &Path,
    players: &[PlayerDescriptor],
    initial_owner: Option<[i32; 4]>,
) -> SResult<()> {
    let local_uuid = native_local_uuid(root)?;
    let inventory = tree(root, &["session.lock"])?;
    for row in inventory.files {
        let name = row.path.strip_suffix("_old").unwrap_or(&row.path);
        // Only original region members are delegated to columns(). A normalized
        // backup name must still pass this closed sidecar profile and refuse.
        if row.path.ends_with(".mca") {
            continue;
        }
        let bytes = read(&root.join(&row.path), FILE_LIMIT)?;
        if name == PIN {
            let _: Value = json(&bytes, 64 << 10)?;
            continue;
        }
        if name == "horizon-world-identity.json" {
            let identity: Identity = json(&bytes, 1024)?;
            require(identity.format == 1, "native identity format")?;
            continue;
        }
        if name == "level.dat" {
            validate_level(&compressed_nbt(&bytes)?)?;
            continue;
        }
        if let Some(player) = players
            .iter()
            .find(|row| player_path(row, "data").strip_prefix("world/") == Some(name))
        {
            let tag = compressed_nbt(&bytes)?;
            validate_player_sidecar(
                &tag,
                row.path.ends_with("_old"),
                player,
                local_uuid,
                initial_owner,
            )?;
            continue;
        }
        if players.iter().any(|row| {
            ["stats", "advancements"]
                .iter()
                .any(|kind| player_path(row, kind).strip_prefix("world/") == Some(name))
        }) {
            let _: Value = json(&bytes, STRUCTURED_LIMIT)?;
            continue;
        }
        if players.iter().all(|row| !row.integrated_owner)
            && ["stats", "advancements"]
                .iter()
                .any(|kind| name == format!("players/{kind}/{}.json", uuid_text(local_uuid)))
        {
            let value: Value = json(&bytes, STRUCTURED_LIMIT)?;
            let empty = if name.starts_with("players/stats/") {
                crate::game::stats::Stats::default().to_json()
            } else {
                horizon_client::advancements::Progress::default()
                    .to_json(&horizon_client::advancements::Tree::default(), DATA_VERSION)
            };
            require(value == empty, "nonempty orphan native player JSON")?;
            continue;
        }
        if name.starts_with("players/data/") && players.iter().all(|row| !row.integrated_owner) {
            let uuid = native_local_uuid(root)?;
            require(
                name == format!("players/data/{}.dat", uuid_text(uuid)),
                "orphan native player file",
            )?;
            let tag = compressed_nbt(&bytes)?;
            require(
                tag.get("UUID") == Some(&Tag::IntArray(uuid.to_vec()))
                    && lan::session_empty_native_player(&tag),
                "orphan native player authority",
            )?;
            continue;
        }
        match name {
            RAIDS => {
                checked(validate_session_empty_raids(&compressed_nbt(&bytes)?))?;
            }
            "data/minecraft/weather.dat"
            | "data/minecraft/world_clocks.dat"
            | "data/minecraft/world_gen_settings.dat"
            | "data/minecraft/game_rules.dat" => {
                standard_sidecar(name, &compressed_nbt(&bytes)?)?;
            }
            "data/minecraft/random_sequences.dat" => {
                let tag = compressed_nbt(&bytes)?;
                compound(&tag, &["DataVersion", "data"])?;
                require(
                    tag.get("DataVersion") == Some(&Tag::Int(DATA_VERSION)),
                    "random sequence version",
                )?;
                let data = tag
                    .get("data")
                    .ok_or_else(|| fail("random sequences data missing"))?;
                let parsed = horizon_client::random_sequences::RandomSequences::from_nbt(data);
                require(
                    parsed.to_nbt() == *data,
                    "coercive or opaque random sequence data",
                )?;
            }
            "data/minecraft/chunk_tickets.dat" => {
                let tag = compressed_nbt(&bytes)?;
                compound(&tag, &["DataVersion", "data"])?;
                require(
                    tag.get("DataVersion") == Some(&Tag::Int(DATA_VERSION)),
                    "ticket version",
                )?;
                let data = tag.get("data").ok_or_else(|| fail("tickets data"))?;
                compound(data, &["tickets"])?;
                empty_list(data.get("tickets"))?;
            }
            // This operation does not claim support for arbitrary native saved
            // data, command stores or other dimensions/players hidden on disk.
            _ => return Err(fail(format!("unsupported native member: {name}"))),
        }
    }
    Ok(())
}
fn validate_player_sidecar(
    tag: &Tag,
    backup: bool,
    player: &PlayerDescriptor,
    local_uuid: [i32; 4],
    initial_owner: Option<[i32; 4]>,
) -> SResult<()> {
    let initial = player.integrated_owner
        && player.record_uuid == local_uuid
        && initial_owner == Some(local_uuid)
        && tag.get("UUID") == Some(&Tag::IntArray(local_uuid.to_vec()));
    if (backup || initial) && lan::session_empty_native_player(tag) {
        return Ok(());
    }
    checked(lan::validate_session_player_tag(tag))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    format: u32,
    world_uuid: WorldId,
}
fn identity(root: &Path, expected: WorldId) -> SResult<()> {
    let record: Identity = json(
        &read(&root.join("horizon-world-identity.json"), 1024)?,
        1024,
    )?;
    require(
        record.format == 1 && record.world_uuid == expected,
        "native world identity mismatch",
    )
}
fn player_rows(
    game: &Game<'_>,
    root: &Path,
    descriptors: &[PlayerDescriptor],
) -> SResult<Vec<SessionPlayerRow>> {
    player_rows_context(
        &SessionValidationContext::from_game(game),
        root,
        descriptors,
    )
}
fn player_rows_context(
    context: &SessionValidationContext,
    root: &Path,
    descriptors: &[PlayerDescriptor],
) -> SResult<Vec<SessionPlayerRow>> {
    require(descriptors.len() <= NODES, "participant structured bound")?;
    let mut profiles = BTreeSet::new();
    let mut records = BTreeSet::new();
    let mut owners = 0;
    let mut rows = Vec::new();
    for descriptor in descriptors {
        require(
            descriptor.profile_uuid.len() == 32
                && descriptor
                    .profile_uuid
                    .bytes()
                    .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v)),
            "profile UUID spelling",
        )?;
        let profile_uuid = checked(u128::from_str_radix(&descriptor.profile_uuid, 16))?;
        require(
            profiles.insert(profile_uuid)
                && records.insert(descriptor.record_uuid)
                && descriptor.dimension == Dim::Overworld.id(),
            "duplicate or wrong-dimension participant",
        )?;
        owners += usize::from(descriptor.integrated_owner);
        require(owners <= 1, "duplicate integrated owner")?;
        let player = compressed_nbt(&read(
            &root.join(player_path(descriptor, "data")),
            STRUCTURED_LIMIT,
        )?)?;
        checked(lan::validate_session_player_tag(&player))?;
        require(
            player.get("UUID") == Some(&Tag::IntArray(descriptor.record_uuid.to_vec()))
                && text(player.get("Dimension"))? == descriptor.dimension,
            "player record/dimension correlation",
        )?;
        let advancements = json(
            &read(
                &root.join(player_path(descriptor, "advancements")),
                STRUCTURED_LIMIT,
            )?,
            STRUCTURED_LIMIT,
        )?;
        let stats = json(
            &read(
                &root.join(player_path(descriptor, "stats")),
                STRUCTURED_LIMIT,
            )?,
            STRUCTURED_LIMIT,
        )?;
        validate_player_items_context(context, &player)?;
        validate_player_json_context(context, &advancements, &stats)?;
        rows.push(SessionPlayerRow {
            conn: 0,
            profile_uuid,
            record_uuid: descriptor.record_uuid,
            integrated_owner: descriptor.integrated_owner,
            dimension: descriptor.dimension.clone(),
            player,
            advancements,
            stats,
        });
    }
    Ok(rows)
}
struct Verified {
    manifest: Manifest,
    counter: SessionCounter,
    slot: DecodedSlotFrame,
    players: CheckedSessionPlayers,
    columns: BTreeMap<(i32, i32), Arc<Chunk>>,
    raids: Option<Tag>,
    items: CheckedSessionItems,
}
fn manifest_shape(manifest: &Manifest, epoch: u64) -> SResult<()> {
    require(
        manifest.format == 1
            && matches!(manifest.kind.as_str(), KIND | GUEST_KIND)
            && manifest.epoch == epoch
            && epoch > 0
            && manifest.dimension == Dim::Overworld.id()
            && manifest.counter == COUNTER
            && manifest.slot == SLOT
            && manifest.saved_clock >= 0,
        "manifest profile",
    )?;
    require(
        !manifest.resident.is_empty()
            && manifest.resident.len() <= NODES
            && manifest.resident.windows(2).all(|v| v[0] < v[1]),
        "resident coordinate ordering/bound",
    )?;
    require(
        manifest.directories.windows(2).all(|v| v[0] < v[1])
            && manifest.files.windows(2).all(|v| v[0].path < v[1].path)
            && manifest.directories.len() + manifest.files.len() <= ENTRY_LIMIT,
        "manifest inventory ordering/bound",
    )?;
    let mut paths = BTreeSet::new();
    let mut total = 0u64;
    for path in &manifest.directories {
        relative(path)?;
        require(
            (path == "world" || path.starts_with("world/") || path == "host")
                && paths.insert(path.as_str()),
            "unknown/duplicate directory",
        )?;
    }
    for row in &manifest.files {
        relative(&row.path)?;
        require(
            row.path != "manifest.json"
                && !row.path.ends_with("/session.lock")
                && paths.insert(&row.path)
                && row.bytes <= FILE_LIMIT as u64
                && row.sha256.len() == 64
                && row
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "manifest member shape",
        )?;
        total = total
            .checked_add(row.bytes)
            .ok_or_else(|| fail("manifest size overflow"))?;
        require(total <= IMAGE_LIMIT, "manifest image bound")?;
        require(
            row.path.starts_with("world/") || matches!(row.path.as_str(), COUNTER | SLOT),
            "unknown envelope member",
        )?;
        let mut parent = Path::new(&row.path).parent();
        while let Some(at) = parent {
            if at.as_os_str().is_empty() {
                break;
            }
            require(
                manifest
                    .directories
                    .binary_search(&at.to_string_lossy().replace('\\', "/"))
                    .is_ok(),
                "manifest parent missing",
            )?;
            parent = at.parent();
        }
    }
    require(
        manifest.files.iter().any(|r| r.path == COUNTER)
            && manifest.files.iter().any(|r| r.path == SLOT)
            && manifest
                .files
                .iter()
                .any(|r| r.path == format!("world/{PIN}")),
        "required envelope members missing",
    )
}
fn verify_image(
    game: &Game<'_>,
    root: &Path,
    epoch: u64,
    expected_world: WorldId,
    expected_target: &CounterTarget,
    expected_type: &str,
) -> SResult<Verified> {
    verify_image_context(
        &SessionValidationContext::from_game(game),
        root,
        epoch,
        expected_world,
        expected_target,
        expected_type,
    )
}
fn verify_image_context(
    context: &SessionValidationContext,
    root: &Path,
    epoch: u64,
    expected_world: WorldId,
    expected_target: &CounterTarget,
    expected_type: &str,
) -> SResult<Verified> {
    let manifest: Manifest = json(
        &read(&root.join("manifest.json"), MANIFEST_LIMIT)?,
        MANIFEST_LIMIT,
    )?;
    manifest_shape(&manifest, epoch)?;
    require(
        manifest.world_uuid == expected_world
            && &manifest.target == expected_target
            && manifest.type_id == expected_type,
        "trusted recovery identity/target/type mismatch",
    )?;
    let physical = tree(root, &["manifest.json"])?;
    require(
        physical
            == Tree {
                directories: manifest.directories.clone(),
                files: manifest.files.clone(),
            },
        "complete inventory/hash mismatch",
    )?;
    let world = root.join("world");
    identity(&world, expected_world)?;
    checked(context.packages.check_world(&world))?;
    // Require actual bounded pins even when an empty package setup would otherwise
    // let `check_world` accept their absence.
    let _: Value = json(&read(&world.join(PIN), 64 << 10)?, 64 << 10)?;
    level_clock(&world, manifest.saved_clock)?;
    let counter_bytes = read(&root.join(COUNTER), STRUCTURED_LIMIT)?;
    let _: Value = json(&counter_bytes, STRUCTURED_LIMIT)?;
    let counter = checked(SessionCounter::decode(
        &counter_bytes,
        manifest.kind == GUEST_KIND,
    ))?;
    let _ = binding_context(context, &counter)?;
    let slot_bytes = read(&root.join(SLOT), STRUCTURED_LIMIT)?;
    let _ = raw_nbt(&slot_bytes)?;
    let slot = checked(counter.decode_slot(&slot_bytes, context.res.items()))?;
    correlate(&manifest, &counter, &slot)?;
    let rows = player_rows_context(context, root, &manifest.players)?;
    sidecars(&world, &manifest.players)?;
    let raids = raid_metadata(&world)?;
    validate_native_local_player(&world, &manifest.players)?;
    let columns = columns_context(context, &world, manifest.saved_clock)?;
    for coordinate in &manifest.resident {
        require(
            columns.contains_key(coordinate),
            "resident native full record missing",
        )?;
    }
    let items = items::read_session_checkpoint_items_context(
        context,
        &world,
        &manifest.resident.iter().copied().collect(),
        &columns,
    )?;
    check_item_player_identities(&items.projection, &rows)?;
    let at = (
        expected_target.position[0] >> 4,
        expected_target.position[2] >> 4,
    );
    let target = columns
        .get(&at)
        .ok_or_else(|| fail("target full record absent"))?;
    let binding = binding_context(context, &counter)?;
    let current = target.block(
        expected_target.position[0] & 15,
        expected_target.position[1],
        expected_target.position[2] & 15,
    );
    let actual = context
        .assets
        .registry
        .state(current)
        .ok_or_else(|| fail("native target registry state absent"))?;
    require(
        actual.name == counter.observed_state.name
            && actual.properties == counter.observed_state.properties
            && match counter.placement {
                Placement::Active { .. } => current == binding.state_id(),
                Placement::Dormant => current != binding.state_id(),
            },
        "native physical target/frame mismatch",
    )?;
    // A checked player brand is issued only after every member, frame, native
    // record and package has passed, including off-resident native entries.
    Ok(Verified {
        manifest,
        counter,
        slot,
        players: CheckedSessionPlayers { rows },
        columns,
        raids,
        items,
    })
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::game) enum CheckpointTestFault {
    Copied,
    Promoted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Copied,
    Overlaid,
    MembersSynced,
    ManifestCompleted,
    BeforePromotion,
    Promoted,
    ParentSynced,
}
struct Snapshot {
    counter: SessionCounter,
    slot: Vec<u8>,
    players: Vec<SessionPlayerRow>,
    columns: BTreeMap<(i32, i32), NamedTag>,
    residents: Vec<(i32, i32)>,
    raids: Option<Tag>,
    items: ItemProjection,
    item_memory: crate::game::persist::SessionItemMemoryProjection,
    item_portals: crate::game::nether_portal::SessionItemPortalProjection,
    clocks: (i64, i64),
    random: u64,
    entity_head: u32,
}
fn player_item_reserved_identities(rows: &[SessionPlayerRow]) -> Vec<[i32; 4]> {
    rows.iter()
        .flat_map(|r| {
            let bytes = r.profile_uuid.to_be_bytes();
            let profile = std::array::from_fn(|i| {
                i32::from_be_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap())
            });
            [profile, r.record_uuid]
        })
        .collect()
}
fn check_item_player_identities(
    items: &ItemProjection,
    players: &[SessionPlayerRow],
) -> SResult<()> {
    require(
        player_item_reserved_identities(players)
            .iter()
            .all(|uuid| !items.rows.contains_key(uuid)),
        "item UUID collides with participant identity",
    )
}
fn same_players(a: &[SessionPlayerRow], b: &[SessionPlayerRow]) -> bool {
    a.len() == b.len()
        && a.iter().all(|a| {
            b.iter()
                .find(|b| b.profile_uuid == a.profile_uuid)
                .is_some_and(|b| {
                    descriptor(a) == descriptor(b)
                        && lan::session_player_tags_equal(&a.player, &b.player)
                        && a.advancements == b.advancements
                        && a.stats == b.stats
                })
        })
}

/// NativeWorld has a separate bounded readability profile. This is not the D
/// image profile, an entity decoder, or a claim of arbitrary-world semantics.
struct NativeReadLevel {
    dimension: Dim,
    save: Arc<save::SaveDir>,
    prefix: String,
    shape: LevelShape,
}
pub(in crate::game) struct NativeWorldReadContext {
    association: Arc<()>,
    validation: SessionValidationContext,
    root: PathBuf,
    active: Dim,
    had_lock: bool,
    expected_world: Option<WorldId>,
    levels: Vec<NativeReadLevel>,
}
#[derive(Clone)]
pub(in crate::game) struct NativeWorldReadInput {
    context: Arc<NativeWorldReadContext>,
    until: Instant,
    #[cfg(all(
        test,
        feature = "paired-packages",
        feature = "native-writer-test-control",
        debug_assertions
    ))]
    control: Option<CoherentImageTestControl>,
}
pub(in crate::game) struct NativeWorldReadPost {
    context: Arc<NativeWorldReadContext>,
    inventory: Tree,
    current_entities: BTreeMap<[i32; 4], (Dim, (i32, i32))>,
    embedded_entities: std::collections::HashSet<[i32; 4]>,
    player_reserved: std::collections::HashSet<[i32; 4]>,
}
impl NativeWorldReadContext {
    pub(in crate::game) fn game_matches(
        &self,
        game: &Game<'_>,
        levels: &super::RetainedNativeLevels,
    ) -> SResult<()> {
        checked(game.preflight_native_membership(levels))?;
        require(
            Arc::ptr_eq(&self.association, &levels.association)
                && self.root == game.world_root
                && self.active == game.dim
                && self.had_lock == game.session_lock.is_some()
                && self.expected_world
                    == game
                        .session_lock
                        .as_ref()
                        .and_then(|lock| lock.expected_world())
                        .copied()
                && Arc::ptr_eq(&self.validation.res, &game.res)
                && Arc::ptr_eq(&self.validation.assets, &game.pack.assets)
                && match (
                    self.validation.packages.catalog(),
                    game.options.mod_packages.catalog(),
                ) {
                    (Some(before), Some(now)) => std::ptr::eq(before, now),
                    (None, None) => true,
                    _ => false,
                },
            "native read original allocation, identity or resource context changed",
        )?;
        require(
            self.levels.len() == levels.levels.len()
                && self.levels.iter().zip(&levels.levels).all(|(a, b)| {
                    a.dimension == b.dimension
                        && Arc::ptr_eq(&a.save, &b.save)
                        && a.shape == b.save.shape
                        && b.save.root == save::dimension_dir(&self.root, b.dimension.id())
                }),
            "native read fixed dimensions or SaveDir changed",
        )
    }
    pub(in crate::game) fn read_preflight_input(
        self: &Arc<Self>,
        until: Instant,
    ) -> NativeWorldReadInput {
        NativeWorldReadInput {
            context: self.clone(),
            until,
            #[cfg(all(
                test,
                feature = "paired-packages",
                feature = "native-writer-test-control",
                debug_assertions
            ))]
            control: None,
        }
    }
}
impl NativeWorldReadPost {
    pub(in crate::game) fn reserved_uuids(&self) -> std::collections::HashSet<[i32; 4]> {
        native_uuid_reservoir(&self.current_entities, &self.player_reserved)
    }
    pub(in crate::game) fn preflight_final_payloads(
        &self,
        payloads: &super::RetainedNativePayloads,
    ) -> SResult<()> {
        require(
            Arc::ptr_eq(&self.context.association, &payloads.association)
                && payloads.levels().len() == self.context.levels.len()
                && payloads
                    .levels()
                    .iter()
                    .zip(&self.context.levels)
                    .all(|(a, b)| a.dimension() == b.dimension && Arc::ptr_eq(a.save(), &b.save)),
            "native source witness/final payload allocation association differs",
        )?;
        let covered: std::collections::HashSet<_> =
            payloads.overwritten_entity_rows().into_iter().collect();
        let covered_columns: std::collections::HashSet<_> = payloads
            .levels()
            .iter()
            .flat_map(|level| {
                level
                    .chunks()
                    .iter()
                    .map(move |chunk| (level.dimension(), (chunk.x, chunk.z)))
            })
            .collect();
        native_final_uuid_coverage(
            &self.current_entities,
            &self.embedded_entities,
            &self.player_reserved,
            &covered,
            &covered_columns,
            &checked(payloads.entity_uuid_rows())?,
        )
    }
}
fn native_uuid_reservoir(
    current: &BTreeMap<[i32; 4], (Dim, (i32, i32))>,
    players: &std::collections::HashSet<[i32; 4]>,
) -> std::collections::HashSet<[i32; 4]> {
    current
        .keys()
        .copied()
        .chain(players.iter().copied())
        .collect()
}
fn native_final_uuid_coverage(
    current: &BTreeMap<[i32; 4], (Dim, (i32, i32))>,
    embedded: &std::collections::HashSet<[i32; 4]>,
    players: &std::collections::HashSet<[i32; 4]>,
    covered: &std::collections::HashSet<(Dim, (i32, i32))>,
    covered_columns: &std::collections::HashSet<(Dim, (i32, i32))>,
    final_rows: &[(Dim, (i32, i32), [i32; 4])],
) -> SResult<()> {
    let mut seen = std::collections::HashSet::new();
    for (_, _, uuid) in final_rows {
        require(seen.insert(*uuid), "duplicate final native entity UUID")?;
        require(
            !players.contains(uuid),
            "final entity UUID collides with native player identity",
        )?;
        if let Some(old_row) = current.get(uuid) {
            // Full native column encoding removes its embedded legacy list;
            // rewriting only entities/ at that position does not remove it.
            let overwritten = if embedded.contains(uuid) {
                covered_columns.contains(old_row)
            } else {
                covered.contains(old_row)
            };
            require(
                overwritten,
                "final entity UUID collides with untouched original native entity row",
            )?;
        }
    }
    Ok(())
}
impl Game<'_> {
    pub(in crate::game) fn prepare_native_world_read_context(
        &self,
        levels: &super::RetainedNativeLevels,
    ) -> SResult<Arc<NativeWorldReadContext>> {
        checked(self.preflight_native_membership(levels))?;
        require(
            levels.error.is_none()
                && !levels.prepared
                && levels.levels.iter().all(|level| level.sealed.is_some()),
            "native old-source capture requires actual sealed original levels before identities",
        )?;
        let mut fixed = Vec::new();
        for level in &levels.levels {
            let prefix = checked(level.save.root.strip_prefix(&self.world_root))?
                .to_str()
                .ok_or_else(|| fail("native dimension root is not UTF8"))?
                .replace('\\', "/");
            relative(&prefix)?;
            fixed.push(NativeReadLevel {
                dimension: level.dimension,
                save: level.save.clone(),
                prefix,
                shape: level.save.shape,
            });
        }
        Ok(Arc::new(NativeWorldReadContext {
            association: levels.association.clone(),
            validation: self.capture_native_validation_context(),
            root: self.world_root.clone(),
            active: self.dim,
            had_lock: self.session_lock.is_some(),
            expected_world: self
                .session_lock
                .as_ref()
                .and_then(|lock| lock.expected_world())
                .copied(),
            levels: fixed,
        }))
    }
    pub(in crate::game) fn preflight_native_world_read(
        &self,
        context: &Arc<NativeWorldReadContext>,
        levels: &super::RetainedNativeLevels,
        post: &NativeWorldReadPost,
    ) -> SResult<()> {
        require(
            Arc::ptr_eq(context, &post.context),
            "foreign native source read witness",
        )?;
        let _ = &post.inventory;
        context.game_matches(self, levels)
    }
}
fn native_uuid(tag: Option<&Tag>) -> SResult<Option<[i32; 4]>> {
    match tag {
        None => Ok(None),
        Some(Tag::IntArray(uuid)) => Ok(Some(checked(uuid.as_slice().try_into())?)),
        _ => Err(fail("native UUID is not an exact four-int array")),
    }
}
fn native_uuid_name(name: &str) -> SResult<[i32; 4]> {
    let compact = name.replace('-', "");
    require(
        compact.len() == 32 && compact.bytes().all(|b| b.is_ascii_hexdigit()),
        "native player UUID filename",
    )?;
    let bytes = checked(u128::from_str_radix(&compact, 16))?.to_be_bytes();
    Ok(std::array::from_fn(|i| {
        i32::from_be_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap())
    }))
}
fn native_version(tag: &Tag) -> SResult<()> {
    require(
        matches!(tag.get("DataVersion"), None | Some(Tag::Int(_))),
        "native DataVersion type",
    )
}
fn native_player_record(
    name: &str,
    tag: &Tag,
    current: bool,
    reserved: &mut std::collections::HashSet<[i32; 4]>,
    current_records: &mut std::collections::HashSet<[i32; 4]>,
) -> SResult<()> {
    let uuid_name = name
        .strip_suffix(".dat")
        .ok_or_else(|| fail("native player filename"))?;
    let named_uuid = native_uuid_name(uuid_name)?;
    if current {
        reserved.insert(named_uuid);
    }
    if let Some(uuid) = native_uuid(tag.get("UUID"))?
        && current
    {
        require(
            current_records.insert(uuid),
            "duplicate current native player record UUID",
        )?;
        reserved.insert(uuid);
    }
    Ok(())
}
fn native_dat(bytes: &[u8]) -> SResult<Tag> {
    let codec = if bytes.starts_with(&[0x1f, 0x8b]) {
        Compression::Gzip
    } else {
        Compression::None
    };
    Ok(raw_native_nbt(&checked(codec.decode(bytes, STRUCTURED_LIMIT))?)?.tag)
}
fn native_entity_uuids(
    tags: &[Tag],
    row: (Dim, (i32, i32)),
    current: &mut BTreeMap<[i32; 4], (Dim, (i32, i32))>,
) -> SResult<()> {
    let mut work: Vec<_> = tags.iter().collect();
    while let Some(tag) = work.pop() {
        require(
            matches!(tag, Tag::Compound(_)),
            "native entity/passenger is not a compound",
        )?;
        if let Some(uuid) = native_uuid(tag.get("UUID"))? {
            require(
                current.insert(uuid, row).is_none(),
                "duplicate current native entity UUID",
            )?;
        }
        if let Some(passengers) = tag.get("Passengers") {
            work.extend(list(Some(passengers))?);
        }
    }
    Ok(())
}
fn native_column_envelope(tag: &Tag, at: (i32, i32), shape: LevelShape) -> SResult<()> {
    native_version(tag)?;
    require(
        tag.get("xPos") == Some(&Tag::Int(at.0)) && tag.get("zPos") == Some(&Tag::Int(at.1)),
        "native column/region coordinates differ",
    )?;
    require(
        matches!(tag.get("Status"), Some(Tag::String(_))),
        "native column Status type",
    )?;
    if let Some(y) = tag.get("yPos") {
        require(
            y == &Tag::Int(shape.min_y >> 4),
            "native column dimension height differs",
        )?;
    }
    let mut ys = BTreeSet::new();
    for section in list(tag.get("sections"))? {
        let Some(Tag::Byte(y)) = section.get("Y") else {
            return Err(fail("native section Y type"));
        };
        require(
            i32::from(*y) >= shape.min_y >> 4
                && i32::from(*y) < (shape.min_y + shape.height) >> 4
                && ys.insert(*y),
            "native section dimension range/duplicate",
        )?;
        for key in ["block_states", "biomes"] {
            if let Some(palette) = section.get(key) {
                require(
                    matches!(palette, Tag::Compound(_)),
                    "native palette envelope type",
                )?;
                let _ = list(palette.get("palette"))?;
                require(
                    matches!(palette.get("data"), None | Some(Tag::LongArray(_))),
                    "native palette storage type",
                )?;
            }
        }
    }
    for key in ["block_entities", "entities"] {
        if let Some(entries) = tag.get(key) {
            require(
                list(Some(entries))?
                    .iter()
                    .all(|entry| matches!(entry, Tag::Compound(_))),
                "native embedded owner list type",
            )?;
        }
    }
    Ok(())
}
fn native_poi_envelope(tag: &Tag, at: (i32, i32), shape: LevelShape) -> SResult<()> {
    native_version(tag)?;
    let Some(Tag::Compound(sections)) = tag.get("Sections") else {
        return Err(fail("native POI Sections type"));
    };
    for (key, section) in sections {
        let y: i32 = checked(checked(key.to_string())?.parse())?;
        require(
            y >= shape.min_y >> 4
                && y < (shape.min_y + shape.height) >> 4
                && matches!(section, Tag::Compound(_)),
            "native POI section range/type",
        )?;
        if let Some(records) = section.get("Records") {
            for record in list(Some(records))? {
                require(
                    matches!(record, Tag::Compound(_))
                        && matches!(record.get("pos"), Some(Tag::IntArray(pos)) if pos.len() == 3)
                        && matches!(record.get("type"), Some(Tag::String(_)))
                        && matches!(record.get("free_tickets"), None | Some(Tag::Int(_))),
                    "native POI record envelope",
                )?;
                if let Some(Tag::IntArray(pos)) = record.get("pos") {
                    require(
                        pos[0].div_euclid(16) == at.0
                            && pos[2].div_euclid(16) == at.1
                            && pos[1].div_euclid(16) == y
                            && pos[1] >= shape.min_y
                            && pos[1] < shape.min_y + shape.height,
                        "native POI record column/section/height differs",
                    )?;
                }
            }
        }
    }
    Ok(())
}
impl NativeWorldReadInput {
    #[cfg(all(
        test,
        feature = "paired-packages",
        feature = "native-writer-test-control",
        debug_assertions
    ))]
    pub(in crate::game) fn with_test_control(mut self, control: CoherentImageTestControl) -> Self {
        self.control = Some(control);
        self
    }
    pub(in crate::game) fn run(&self) -> SResult<NativeWorldReadPost> {
        deadline(self.until)?;
        #[cfg(all(
            test,
            feature = "paired-packages",
            feature = "native-writer-test-control",
            debug_assertions
        ))]
        if let Some(control) = &self.control {
            control.visit(CoherentImageTestPoint::Read)?;
        }
        let context = &self.context;
        let root = directory(&context.root)?;
        if let Some(world) = context.expected_world {
            identity(&root, world)?;
        }
        checked(context.validation.packages.check_world(&root))?;
        let inventory = tree(&root, &["session.lock"])?;
        let mut current_entities = BTreeMap::new();
        let mut embedded_entities = std::collections::HashSet::new();
        let mut player_reserved = std::collections::HashSet::new();
        let mut current_player_records = std::collections::HashSet::new();
        for member_row in &inventory.files {
            deadline(self.until)?;
            let name = &member_row.path;
            let base = name.strip_suffix("_old").unwrap_or(name);
            let current = base == name;
            let player = base
                .strip_prefix("players/data/")
                .or_else(|| base.strip_prefix("playerdata/"));
            let saved_data = (base.starts_with("data/minecraft/")
                || ["overworld", "the_nether", "the_end"]
                    .iter()
                    .any(|dimension| {
                        base.starts_with(&format!(
                            "dimensions/minecraft/{dimension}/data/minecraft/"
                        ))
                    }))
                && base.ends_with(".dat");
            if base == "level.dat"
                || player.is_some_and(|p| p.ends_with(".dat"))
                || saved_data
                || base == "data/horizon_portal_arrivals.dat"
            {
                let tag = native_dat(&read(&root.join(name), STRUCTURED_LIMIT)?)?;
                native_version(&tag)?;
                if base == "level.dat" {
                    let Some(Tag::Compound(data)) = tag.get("Data") else {
                        return Err(fail("native Level Data envelope"));
                    };
                    if let Some(uuid) = native_uuid(data.get(&"singleplayer_uuid".into()))?
                        && current
                    {
                        player_reserved.insert(uuid);
                    }
                    if let Some(player) = data.get(&"Player".into()) {
                        require(
                            matches!(player, Tag::Compound(_)),
                            "native legacy player envelope",
                        )?;
                        if let Some(uuid) = native_uuid(player.get("UUID"))?
                            && current
                        {
                            player_reserved.insert(uuid);
                        }
                    }
                } else if let Some(player) = player {
                    native_player_record(
                        player,
                        &tag,
                        current,
                        &mut player_reserved,
                        &mut current_player_records,
                    )?;
                } else if saved_data {
                    require(
                        matches!(tag.get("data"), Some(Tag::Compound(_))),
                        "native saved-data envelope",
                    )?;
                } else {
                    let _ = list(tag.get("arrivals"))?;
                }
            } else if base == "horizon-world-identity.json"
                || base == PIN
                || ((base.starts_with("players/advancements/")
                    || base.starts_with("players/stats/"))
                    && base.ends_with(".json"))
            {
                let bytes = read(&root.join(name), STRUCTURED_LIMIT)?;
                let _: Value = json(&bytes, STRUCTURED_LIMIT)?;
                if base == "horizon-world-identity.json" {
                    let record: Identity = json(&bytes, 1024)?;
                    require(record.format == 1, "native identity version")?;
                }
            }
            for level in &context.levels {
                let Some(within) = name.strip_prefix(&format!("{}/", level.prefix)) else {
                    continue;
                };
                let Some((folder, filename)) = within.split_once('/') else {
                    continue;
                };
                if !matches!(folder, "region" | "entities" | "poi") {
                    continue;
                }
                if !filename.ends_with(".mca") {
                    require(
                        !filename.contains(".mca")
                            && !filename.ends_with(".mcc")
                            && !filename.starts_with("r."),
                        "unsupported native region backup/external naming",
                    )?;
                    continue;
                }
                require(!filename.contains('/'), "nested native region member")?;
                let path = root.join(name);
                let position = checked(horizon_world::region::position_from_path(&path))?;
                require(
                    filename == format!("r.{}.{}.mca", position.x, position.z),
                    "noncanonical native region filename",
                )?;
                for coordinate in [position.x, position.z] {
                    require(
                        coordinate
                            .checked_mul(32)
                            .and_then(|v| v.checked_add(31))
                            .is_some(),
                        "native region coordinate overflow",
                    )?;
                }
                let bytes = read(&path, FILE_LIMIT)?;
                let entries = checked(save::retained_region_locations(&bytes))?;
                let mut region = checked(RegionFile::open(&path))?;
                let actual: BTreeSet<_> = region.chunks().map(|(p, _)| (p.x, p.z)).collect();
                let expected: BTreeSet<_> = entries
                    .iter()
                    .map(|i| {
                        (
                            position.x * 32 + (i % 32) as i32,
                            position.z * 32 + (i / 32) as i32,
                        )
                    })
                    .collect();
                require(
                    actual == expected,
                    "native region header/enumeration differs",
                )?;
                for at in expected {
                    deadline(self.until)?;
                    let record = checked(region.read(ChunkPos::new(at.0, at.1)))?
                        .ok_or_else(|| fail("native region slot vanished"))?;
                    let named = raw_native_nbt(&items::native_record_bytes(&record)?)?;
                    match folder {
                        "region" => {
                            native_column_envelope(&named.tag, at, level.shape)?;
                            if let Some(tags) = named.tag.get("entities") {
                                let mut embedded = BTreeMap::new();
                                native_entity_uuids(
                                    list(Some(tags))?,
                                    (level.dimension, at),
                                    &mut embedded,
                                )?;
                                for (uuid, row) in embedded {
                                    require(
                                        current_entities.insert(uuid, row).is_none(),
                                        "duplicate embedded/sidecar current entity UUID",
                                    )?;
                                    embedded_entities.insert(uuid);
                                }
                            }
                        }
                        "entities" => {
                            native_version(&named.tag)?;
                            require(
                                named.tag.get("Position") == Some(&Tag::IntArray(vec![at.0, at.1])),
                                "native entity sidecar Position differs",
                            )?;
                            native_entity_uuids(
                                list(named.tag.get("Entities"))?,
                                (level.dimension, at),
                                &mut current_entities,
                            )?;
                        }
                        _ => native_poi_envelope(&named.tag, at, level.shape)?,
                    }
                }
                require(
                    member(&root, name.clone())? == *member_row,
                    "native region changed while scanning",
                )?;
            }
        }
        require(
            current_entities
                .keys()
                .all(|uuid| !player_reserved.contains(uuid)),
            "current native entity/player UUID collision",
        )?;
        require(
            tree(&root, &["session.lock"])? == inventory,
            "native source inventory changed while scanning",
        )?;
        deadline(self.until)?;
        Ok(NativeWorldReadPost {
            context: context.clone(),
            inventory,
            current_entities,
            embedded_entities,
            player_reserved,
        })
    }
}

/// Captured before identity preparation. Construction proves actual CPU owners;
/// its worker performs the complete old-source validation before final writes.
pub(in crate::game) struct CoherentImageContext {
    validation: SessionValidationContext,
    save: Arc<save::SaveDir>,
    source_root: PathBuf,
    archive_root: PathBuf,
    world: WorldId,
    counter: SessionCounter,
    slot: Vec<u8>,
    roster: lan::ParticipantRoster,
    players: Vec<SessionPlayerRow>,
    residents: Vec<(i32, i32)>,
    live_raids: Option<Tag>,
    local_uuid: Option<[i32; 4]>,
    initial_integrated_record: Option<[i32; 4]>,
    clocks: (i64, i64),
    random: u64,
    entity_head: u32,
}
#[derive(Clone)]
pub(in crate::game) struct CoherentImageReadInput {
    context: Arc<CoherentImageContext>,
    until: Instant,
    #[cfg(all(
        test,
        feature = "paired-packages",
        feature = "native-writer-test-control",
        debug_assertions
    ))]
    control: Option<CoherentImageTestControl>,
}
pub(in crate::game) struct CoherentImageReadPost {
    context: Arc<CoherentImageContext>,
    source: PathBuf,
    archive: PathBuf,
    raids: Option<Tag>,
    local_uuid: [i32; 4],
    next_epoch: u64,
    old_inventory: Tree,
    old_item_uuids: Vec<[i32; 4]>,
}
impl CoherentImageReadPost {
    pub(in crate::game) fn next_epoch(&self) -> u64 {
        self.next_epoch
    }
    pub(in crate::game) fn reserved_item_identities(&self) -> Vec<[i32; 4]> {
        player_item_reserved_identities(&self.context.players)
            .into_iter()
            .chain(self.old_item_uuids.iter().copied())
            .collect()
    }
    pub(in crate::game) fn reserved_uuids(&self) -> std::collections::HashSet<[i32; 4]> {
        self.reserved_item_identities().into_iter().collect()
    }
}
impl CoherentImageContext {
    pub(in crate::game) fn read_preflight_input(
        self: &Arc<Self>,
        until: Instant,
    ) -> CoherentImageReadInput {
        CoherentImageReadInput {
            context: self.clone(),
            until,
            #[cfg(all(
                test,
                feature = "paired-packages",
                feature = "native-writer-test-control",
                debug_assertions
            ))]
            control: None,
        }
    }
    pub(in crate::game) fn game_matches(&self, game: &Game<'_>) -> SResult<()> {
        self.matches_cpu(game)
    }
    fn matches_cpu(&self, game: &Game<'_>) -> SResult<()> {
        require(
            self.validation.same_owners(game),
            "coherent image native resource owners changed",
        )?;
        require(
            game.save
                .as_ref()
                .is_some_and(|save| Arc::ptr_eq(&self.save, save)),
            "coherent image native writer allocation changed",
        )?;
        require(
            game.session_context_cpu()? == self.world
                && game.world_root == self.source_root
                && game.level.player_uuid == self.local_uuid
                && (game.world.game_time, game.world.day_time) == self.clocks
                && game.world.random.state() == self.random
                && game.next_entity == self.entity_head
                && game.world.chunks.keys().copied().collect::<Vec<_>>() == self.residents
                && checked(SessionCounter::from_world(&game.world))? == self.counter
                && checked(
                    self.counter
                        .slot_bytes(&game.world, self.validation.res.items()),
                )? == self.slot
                && checked(game.session_checkpoint_raids())? == self.live_raids,
            "coherent image original frame, native route or owner state changed",
        )?;
        checked(
            game.preflight_checkpoint_participants(&self.roster)
                .map_err(|e| format!("{e:?}")),
        )?;
        let players = checked(game.project_session_participant_rows(&self.roster))?;
        require(
            same_players(&self.players, &players)
                && self.players.iter().all(|a| {
                    players
                        .iter()
                        .any(|b| a.profile_uuid == b.profile_uuid && a.conn == b.conn)
                }),
            "coherent image original participant state changed",
        )?;
        checked(game.inspect_session_checkpoint_owners())
    }
}
impl CoherentImageReadInput {
    #[cfg(all(
        test,
        feature = "paired-packages",
        feature = "native-writer-test-control",
        debug_assertions
    ))]
    pub(in crate::game) fn with_test_control(mut self, control: CoherentImageTestControl) -> Self {
        self.control = Some(control);
        self
    }
    pub(in crate::game) fn run(&self) -> SResult<CoherentImageReadPost> {
        deadline(self.until)?;
        #[cfg(all(
            test,
            feature = "paired-packages",
            feature = "native-writer-test-control",
            debug_assertions
        ))]
        if let Some(control) = &self.control {
            control.visit(CoherentImageTestPoint::Read)?;
        }
        let context = &self.context;
        let source = directory(&context.source_root)?;
        let archive = directory(&context.archive_root)?;
        require(
            !archive.starts_with(&source) && !source.starts_with(&archive),
            "overlapping archive/world",
        )?;
        identity(&source, context.world)?;
        checked(context.validation.packages.check_world(&source))?;
        let _: Value = json(&read(&source.join(PIN), 64 << 10)?, 64 << 10)?;
        let old_inventory = tree(&source, &["session.lock"])?;
        let old_columns = columns_context(&context.validation, &source, context.clocks.0)?;
        let old_items = items::read_session_checkpoint_items_context(
            &context.validation,
            &source,
            &context.residents.iter().copied().collect(),
            &old_columns,
        )?;
        check_item_player_identities(&old_items.projection, &context.players)?;
        let descriptors = context.players.iter().map(descriptor).collect::<Vec<_>>();
        // Validate every original sidecar, including native backups, before a
        // later final save can rotate a malformed predecessor out of view.
        let next_epoch = next_session_checkpoint_epoch(&archive)?;
        sidecars_with_initial_owner(
            &source,
            &descriptors,
            (next_epoch == 1)
                .then_some(context.initial_integrated_record)
                .flatten(),
        )?;
        let local_uuid = validate_native_local_player(&source, &descriptors)?;
        require(
            context.local_uuid == Some(local_uuid),
            "live/native singleplayer UUID mismatch",
        )?;
        // Validate current and backup even if the live set supplies the effective
        // value: a native rotation must not hide malformed old metadata.
        let raids = context.live_raids.clone().or(raid_metadata(&source)?);
        require(
            tree(&source, &["session.lock"])? == old_inventory,
            "old source changed during coherent read preflight",
        )?;
        deadline(self.until)?;
        Ok(CoherentImageReadPost {
            context: context.clone(),
            source,
            archive,
            raids,
            local_uuid,
            next_epoch,
            old_inventory,
            old_item_uuids: old_items.projection.rows.keys().copied().collect(),
        })
    }
}

struct CoherentImageProjection {
    context: Arc<CoherentImageContext>,
    source: PathBuf,
    archive: PathBuf,
    minimum_epoch: u64,
    last_attempt_epoch: std::sync::atomic::AtomicU64,
    snapshot: Snapshot,
}
#[derive(Clone)]
pub(in crate::game) struct CoherentImageBuildInput {
    projection: Arc<CoherentImageProjection>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(in crate::game) enum CoherentImagePhase {
    Prepared,
    Copying,
    Copied,
    Overlaid,
    MembersSynced,
    ManifestCompleted,
    Verified,
    BeforePromotion,
    Promoted,
    ParentSynced,
}
/// Original owner retains this before dispatch and before every filesystem
/// effect. Atomic observations never acquire a worker-held progress mutex.
pub(in crate::game) struct CoherentImageAttempt {
    association: Arc<()>,
    projection: Arc<CoherentImageProjection>,
    epoch: u64,
    pending: PathBuf,
    published: PathBuf,
    until: Instant,
    phase: std::sync::atomic::AtomicU8,
    uncertain: std::sync::atomic::AtomicBool,
    started: std::sync::atomic::AtomicBool,
    #[cfg(all(
        test,
        feature = "paired-packages",
        feature = "native-writer-test-control",
        debug_assertions
    ))]
    control: Option<CoherentImageTestControl>,
}
impl CoherentImageAttempt {
    pub(in crate::game) fn epoch(&self) -> u64 {
        self.epoch
    }
    pub(in crate::game) fn pending(&self) -> &Path {
        &self.pending
    }
    pub(in crate::game) fn published(&self) -> &Path {
        &self.published
    }
    pub(in crate::game) fn publication(&self) -> Publication {
        if self.uncertain.load(std::sync::atomic::Ordering::Acquire) {
            Publication::Uncertain
        } else {
            Publication::NotPublished
        }
    }
    pub(in crate::game) fn phase(&self) -> CoherentImagePhase {
        use CoherentImagePhase as P;
        match self.phase.load(std::sync::atomic::Ordering::Acquire) {
            0 => P::Prepared,
            1 => P::Copying,
            2 => P::Copied,
            3 => P::Overlaid,
            4 => P::MembersSynced,
            5 => P::ManifestCompleted,
            6 => P::Verified,
            7 => P::BeforePromotion,
            8 => P::Promoted,
            _ => P::ParentSynced,
        }
    }
    fn advance(&self, phase: CoherentImagePhase) {
        self.phase
            .store(phase as u8, std::sync::atomic::Ordering::Release);
    }
}
pub(in crate::game) struct CoherentImageWritten {
    projection: Arc<CoherentImageProjection>,
    association: Arc<()>,
    receipt: CheckpointReceipt,
}
impl CoherentImageWritten {
    pub(in crate::game) fn receipt(&self) -> &CheckpointReceipt {
        &self.receipt
    }
}
impl CoherentImageBuildInput {
    pub(in crate::game) fn attempt(
        &self,
        epoch: u64,
        until: Instant,
    ) -> SResult<Arc<CoherentImageAttempt>> {
        require(
            epoch >= self.projection.minimum_epoch && epoch > 0,
            "coherent image epoch precedes read preflight",
        )?;
        let mut previous = self
            .projection
            .last_attempt_epoch
            .load(std::sync::atomic::Ordering::Acquire);
        loop {
            require(
                epoch > previous,
                "coherent image attempt epoch must strictly increase",
            )?;
            match self.projection.last_attempt_epoch.compare_exchange_weak(
                previous,
                epoch,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => previous = observed,
            }
        }
        Ok(Arc::new(CoherentImageAttempt {
            association: Arc::new(()),
            projection: self.projection.clone(),
            epoch,
            pending: self
                .projection
                .archive
                .join(format!(".pending-{epoch:020}")),
            published: self.projection.archive.join(format!("epoch-{epoch:020}")),
            until,
            phase: std::sync::atomic::AtomicU8::new(CoherentImagePhase::Prepared as u8),
            uncertain: std::sync::atomic::AtomicBool::new(false),
            started: std::sync::atomic::AtomicBool::new(false),
            #[cfg(all(
                test,
                feature = "paired-packages",
                feature = "native-writer-test-control",
                debug_assertions
            ))]
            control: None,
        }))
    }
    #[cfg(all(
        test,
        feature = "paired-packages",
        feature = "native-writer-test-control",
        debug_assertions
    ))]
    pub(in crate::game) fn attempt_with_test_control(
        &self,
        epoch: u64,
        until: Instant,
        control: CoherentImageTestControl,
    ) -> SResult<Arc<CoherentImageAttempt>> {
        let mut attempt = self.attempt(epoch, until)?;
        Arc::get_mut(&mut attempt)
            .expect("new private image attempt has one owner")
            .control = Some(control);
        Ok(attempt)
    }
    pub(in crate::game) fn build_verify_publish(
        &self,
        attempt: &CoherentImageAttempt,
    ) -> SResult<CoherentImageWritten> {
        let result = self.build_verify_publish_inner(attempt);
        result.map_err(|mut error| {
            error.publication = attempt.publication();
            error
        })
    }
    fn build_verify_publish_inner(
        &self,
        attempt: &CoherentImageAttempt,
    ) -> SResult<CoherentImageWritten> {
        use CoherentImagePhase as P;
        require(
            Arc::ptr_eq(&self.projection, &attempt.projection),
            "foreign coherent image attempt",
        )?;
        deadline(attempt.until)?;
        require(
            !attempt
                .started
                .swap(true, std::sync::atomic::Ordering::AcqRel),
            "coherent image attempt already started",
        )?;
        let projection = &self.projection;
        let context = &projection.context;
        let validation = &context.validation;
        let before = &projection.snapshot;
        let source = directory(&projection.source)?;
        let archive = directory(&projection.archive)?;
        require(
            source == projection.source && archive == projection.archive,
            "coherent image canonical route changed",
        )?;
        let minimum_epoch = next_session_checkpoint_epoch(&archive)?;
        require(
            attempt.epoch >= minimum_epoch,
            "epoch must increase monotonically across published and pending artifacts",
        )?;
        require(
            !attempt.pending.try_exists().map_err(fail)?
                && !attempt.published.try_exists().map_err(fail)?,
            "epoch path already exists",
        )?;
        identity(&source, context.world)?;
        checked(validation.packages.check_world(&source))?;
        let _: Value = json(&read(&source.join(PIN), 64 << 10)?, 64 << 10)?;
        let saved_columns = columns_context(validation, &source, before.clocks.0)?;
        let saved_items = items::read_session_checkpoint_items_context(
            validation,
            &source,
            &before.residents.iter().copied().collect(),
            &saved_columns,
        )?;
        require(
            saved_items.projection == before.items,
            "saved native item projection mismatch",
        )?;
        let source_inventory = tree(&source, &["session.lock"])?;
        attempt.advance(P::Copying);
        checked(fs::create_dir(&attempt.pending))?;
        #[cfg(all(
            test,
            feature = "paired-packages",
            feature = "native-writer-test-control",
            debug_assertions
        ))]
        if let Some(control) = &attempt.control {
            control.visit(CoherentImageTestPoint::Copy)?;
        }
        copy_tree(
            &source,
            &attempt.pending.join("world"),
            &source_inventory,
            attempt.until,
        )?;
        require(
            tree(&source, &["session.lock"])? == source_inventory,
            "source inventory/hash changed during copy",
        )?;
        attempt.advance(P::Copied);
        #[cfg(all(
            test,
            feature = "paired-packages",
            feature = "native-writer-test-control",
            debug_assertions
        ))]
        if let Some(control) = &attempt.control {
            control.visit(CoherentImageTestPoint::Copied)?;
        }
        for row in &before.players {
            let descriptor = descriptor(row);
            for (kind, bytes) in [
                ("data", nbt_bytes(&row.player)?),
                (
                    "advancements",
                    json_bytes(&row.advancements, STRUCTURED_LIMIT)?,
                ),
                ("stats", json_bytes(&row.stats, STRUCTURED_LIMIT)?),
            ] {
                overlay(
                    &parents(&attempt.pending, &player_path(&descriptor, kind))?,
                    &bytes,
                )?;
            }
        }
        write_new(
            &parents(&attempt.pending, COUNTER)?,
            &checked(before.counter.encode())?,
        )?;
        write_new(&parents(&attempt.pending, SLOT)?, &before.slot)?;
        attempt.advance(P::Overlaid);
        let physical = tree(&attempt.pending, &[])?;
        let manifest = Manifest {
            format: 1,
            kind: before.counter.image_kind().into(),
            epoch: attempt.epoch,
            world_uuid: context.world,
            dimension: Dim::Overworld.id().into(),
            saved_clock: before.clocks.0,
            target: checked(CounterTarget::new(
                &before.counter.dimension,
                before.counter.position,
            ))?,
            type_id: before.counter.type_id.clone(),
            counter: COUNTER.into(),
            slot: SLOT.into(),
            resident: before.residents.clone(),
            players: before.players.iter().map(descriptor).collect(),
            directories: physical.directories,
            files: physical.files,
        };
        manifest_shape(&manifest, attempt.epoch)?;
        sync_tree(&attempt.pending)?;
        attempt.advance(P::MembersSynced);
        write_new(
            &attempt.pending.join("manifest.json"),
            &json_bytes(&manifest, MANIFEST_LIMIT)?,
        )?;
        sync_dir(&attempt.pending)?;
        attempt.advance(P::ManifestCompleted);
        #[cfg(all(
            test,
            feature = "paired-packages",
            feature = "native-writer-test-control",
            debug_assertions
        ))]
        if let Some(control) = &attempt.control {
            control.visit(CoherentImageTestPoint::Verify)?;
        }
        let verified = verify_image_context(
            validation,
            &attempt.pending,
            attempt.epoch,
            context.world,
            &manifest.target,
            &manifest.type_id,
        )?;
        require(
            same_players(&before.players, verified.players.rows()),
            "detached player image changed",
        )?;
        require(
            before.items == verified.items.projection
                && saved_items.projection == verified.items.projection,
            "copied native item projection mismatch",
        )?;
        require(
            before.raids == verified.raids && raid_metadata(&source)? == before.raids,
            "copied/current native raid metadata mismatch",
        )?;
        for (at, expected) in &before.columns {
            let actual = verified
                .columns
                .get(at)
                .ok_or_else(|| fail("native saved resident record missing"))?;
            let tables = expected
                .tag
                .get("Heightmaps")
                .is_some()
                .then(|| save::SaveTables::new(&validation.assets));
            let actual = save::encode_with(
                actual,
                validation.assets.registry,
                validation.res.biomes(),
                tables.as_ref(),
                LevelShape::OVERWORLD,
            );
            require(
                lan::session_player_tags_equal(&expected.tag, &actual.tag),
                "saved native resident projection mismatch",
            )?;
        }
        require(
            tree(&source, &["session.lock"])? == source_inventory,
            "source changed after candidate validation",
        )?;
        attempt.advance(P::Verified);
        deadline(attempt.until)?;
        attempt.advance(P::BeforePromotion);
        // Set BEFORE rename, including panics or uncertain OS return values.
        attempt
            .uncertain
            .store(true, std::sync::atomic::Ordering::Release);
        #[cfg(all(
            test,
            feature = "paired-packages",
            feature = "native-writer-test-control",
            debug_assertions
        ))]
        if let Some(control) = &attempt.control {
            control.visit(CoherentImageTestPoint::BeforeRename)?;
        }
        checked(fs::rename(&attempt.pending, &attempt.published))?;
        attempt.advance(P::Promoted);
        #[cfg(all(
            test,
            feature = "paired-packages",
            feature = "native-writer-test-control",
            debug_assertions
        ))]
        if let Some(control) = &attempt.control {
            control.visit(CoherentImageTestPoint::Promoted)?;
        }
        sync_dir(&archive)?;
        attempt.advance(P::ParentSynced);
        Ok(CoherentImageWritten {
            projection: self.projection.clone(),
            association: attempt.association.clone(),
            receipt: CheckpointReceipt {
                epoch: attempt.epoch,
                root: attempt.published.clone(),
            },
        })
    }
}

#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::game) enum CoherentImageTestAction {
    HoldRead,
    HoldCopy,
    HoldVerify,
    HoldBeforeRename,
    FailCopied,
    FailPromoted,
    PanicBeforeRename,
    PanicPromoted,
    PanicRead,
}
#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
enum CoherentImageTestPoint {
    Read,
    Copy,
    Copied,
    Verify,
    BeforeRename,
    Promoted,
}
#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
struct CoherentImageTestGate {
    entered: std::sync::atomic::AtomicBool,
    released: std::sync::atomic::AtomicBool,
    fired: std::sync::atomic::AtomicBool,
    wait: std::sync::Mutex<()>,
    changed: std::sync::Condvar,
}
#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
#[derive(Clone)]
pub(in crate::game) struct CoherentImageTestControl {
    action: CoherentImageTestAction,
    gate: Arc<CoherentImageTestGate>,
}
#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
pub(in crate::game) struct CoherentImageTestReceipt {
    gate: Arc<CoherentImageTestGate>,
}
#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
impl CoherentImageTestControl {
    pub(in crate::game) fn new(
        action: CoherentImageTestAction,
    ) -> (Self, CoherentImageTestReceipt) {
        let gate = Arc::new(CoherentImageTestGate {
            entered: false.into(),
            released: false.into(),
            fired: false.into(),
            wait: std::sync::Mutex::new(()),
            changed: std::sync::Condvar::new(),
        });
        (
            Self {
                action,
                gate: gate.clone(),
            },
            CoherentImageTestReceipt { gate },
        )
    }
    fn visit(&self, point: CoherentImageTestPoint) -> SResult<()> {
        use CoherentImageTestAction as A;
        use CoherentImageTestPoint as P;
        let selected = matches!(
            (self.action, point),
            (A::HoldRead | A::PanicRead, P::Read)
                | (A::HoldCopy, P::Copy)
                | (A::FailCopied, P::Copied)
                | (A::HoldVerify, P::Verify)
                | (A::HoldBeforeRename | A::PanicBeforeRename, P::BeforeRename)
                | (A::FailPromoted | A::PanicPromoted, P::Promoted)
        );
        if !selected
            || self
                .gate
                .fired
                .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return Ok(());
        }
        self.gate
            .entered
            .store(true, std::sync::atomic::Ordering::Release);
        match self.action {
            A::FailCopied | A::FailPromoted => Err(fail("injected owned image phase failure")),
            A::PanicBeforeRename | A::PanicPromoted | A::PanicRead => {
                panic!("injected owned image phase panic")
            }
            _ => {
                let mut waiting = self
                    .gate
                    .wait
                    .lock()
                    .map_err(|_| fail("image test gate poisoned"))?;
                while !self
                    .gate
                    .released
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    // Release is atomic and never takes this worker-only mutex.
                    // A timed recheck also covers notification before wait entry.
                    waiting = self
                        .gate
                        .changed
                        .wait_timeout(waiting, std::time::Duration::from_millis(10))
                        .map_err(|_| fail("image test gate poisoned"))?
                        .0;
                }
                Ok(())
            }
        }
    }
}
#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
impl CoherentImageTestReceipt {
    pub(in crate::game) fn entered(&self) -> bool {
        self.gate.entered.load(std::sync::atomic::Ordering::Acquire)
    }
    pub(in crate::game) fn release(&self) {
        self.gate
            .released
            .store(true, std::sync::atomic::Ordering::Release);
        self.gate.changed.notify_all();
    }
}
#[cfg(all(
    test,
    feature = "paired-packages",
    feature = "native-writer-test-control",
    debug_assertions
))]
impl Drop for CoherentImageTestReceipt {
    fn drop(&mut self) {
        self.release();
    }
}
impl Game<'_> {
    pub(in crate::game) fn prepare_coherent_image_context(
        &self,
        archive_root: &Path,
    ) -> SResult<Arc<CoherentImageContext>> {
        let world = self.session_context_cpu()?;
        let validation = SessionValidationContext::from_game(self);
        let counter = checked(SessionCounter::from_world(&self.world))?;
        let _ = binding_context(&validation, &counter)?;
        let slot = checked(counter.slot_bytes(&self.world, validation.res.items()))?;
        let parsed = checked(counter.decode_slot(&slot, validation.res.items()))?;
        let roster = checked(
            self.observe_checkpoint_participants()
                .map_err(|e| format!("{e:?}")),
        )?;
        let players = checked(self.project_session_participant_rows(&roster))?;
        checked(self.inspect_session_checkpoint_owners())?;
        if players.iter().all(|row| !row.integrated_owner) {
            require(
                lan::session_empty_native_player(&self.level.player_tag()),
                "dedicated cached local player authority",
            )?;
        }
        let residents = self.world.chunks.keys().copied().collect::<Vec<_>>();
        let manifest = Manifest {
            format: 1,
            kind: counter.image_kind().into(),
            epoch: 1,
            world_uuid: world,
            dimension: self.dim.id().into(),
            saved_clock: self.world.game_time,
            target: checked(CounterTarget::new(&counter.dimension, counter.position))?,
            type_id: counter.type_id.clone(),
            counter: COUNTER.into(),
            slot: SLOT.into(),
            resident: residents.clone(),
            players: players.iter().map(descriptor).collect(),
            directories: Vec::new(),
            files: Vec::new(),
        };
        correlate(&manifest, &counter, &parsed)?;
        for row in &players {
            checked(lan::validate_session_player_tag(&row.player))?;
            validate_player_items_context(&validation, &row.player)?;
            validate_player_json_context(&validation, &row.advancements, &row.stats)?;
            let _ = nbt_bytes(&row.player)?;
            let _ = json_bytes(&row.advancements, STRUCTURED_LIMIT)?;
            let _ = json_bytes(&row.stats, STRUCTURED_LIMIT)?;
        }
        Ok(Arc::new(CoherentImageContext {
            validation,
            save: self
                .save
                .as_ref()
                .ok_or_else(|| fail("native storage owner absent"))?
                .clone(),
            source_root: self.world_root.clone(),
            archive_root: archive_root.to_owned(),
            world,
            counter,
            slot,
            roster,
            players,
            residents,
            live_raids: checked(self.session_checkpoint_raids())?,
            local_uuid: self.level.player_uuid,
            initial_integrated_record: self
                .native_close_authority
                .as_ref()
                .filter(|authority| authority.retains_ordinary_source(self))
                .and_then(|_| {
                    self.retained_server
                        .as_ref()?
                        .integrated_owner()
                        .map(|(_, record)| record)
                }),
            clocks: (self.world.game_time, self.world.day_time),
            random: self.world.random.state(),
            entity_head: self.next_entity,
        }))
    }
    pub(in crate::game) fn freeze_coherent_image_input(
        &self,
        context: &Arc<CoherentImageContext>,
        read: &CoherentImageReadPost,
    ) -> SResult<CoherentImageBuildInput> {
        require(
            Arc::ptr_eq(context, &read.context),
            "foreign coherent source preflight",
        )?;
        context.matches_cpu(self)?;
        // The returned private read witness retains its exact old inventory;
        // no owner-side filesystem operation reconstructs that evidence.
        let _ = &read.old_inventory;
        let raids = checked(self.session_checkpoint_raids())?.or_else(|| read.raids.clone());
        let snapshot = self.session_snapshot_cpu(raids, read.local_uuid)?;
        Ok(CoherentImageBuildInput {
            projection: Arc::new(CoherentImageProjection {
                context: context.clone(),
                source: read.source.clone(),
                archive: read.archive.clone(),
                minimum_epoch: read.next_epoch,
                last_attempt_epoch: std::sync::atomic::AtomicU64::new(read.next_epoch - 1),
                snapshot,
            }),
        })
    }
    pub(in crate::game) fn preflight_coherent_image(
        &self,
        input: &CoherentImageBuildInput,
        attempt: &CoherentImageAttempt,
        written: &CoherentImageWritten,
    ) -> SResult<()> {
        require(
            Arc::ptr_eq(&input.projection, &written.projection)
                && Arc::ptr_eq(&input.projection, &attempt.projection)
                && Arc::ptr_eq(&attempt.association, &written.association)
                && written.receipt.epoch == attempt.epoch
                && written.receipt.root == attempt.published
                && attempt.phase() == CoherentImagePhase::ParentSynced,
            "foreign or incomplete coherent image result",
        )?;
        let projection = &input.projection;
        projection.context.matches_cpu(self)?;
        let before = &projection.snapshot;
        let after = self.session_snapshot_cpu(
            before.raids.clone(),
            projection
                .context
                .local_uuid
                .ok_or_else(|| fail("coherent local UUID absent"))?,
        )?;
        require(
            before.counter == after.counter
                && before.slot == after.slot
                && same_players(&before.players, &after.players)
                && before.players.iter().all(|a| {
                    after
                        .players
                        .iter()
                        .any(|b| a.profile_uuid == b.profile_uuid && a.conn == b.conn)
                })
                && before.columns == after.columns
                && before.residents == after.residents
                && before.raids == after.raids
                && before.items == after.items
                && before.item_memory == after.item_memory
                && before.item_portals == after.item_portals
                && before.clocks == after.clocks
                && before.random == after.random
                && before.entity_head == after.entity_head,
            "coherent image original final projection changed",
        )
    }
}
impl Game<'_> {
    /// Full owner/player/profile preflight precedes the explicit UUID stage.
    pub(in crate::game) fn session_checkpoint_identity_preflight(&self) -> SResult<Vec<[i32; 4]>> {
        let world = self.session_context()?;
        let counter = checked(SessionCounter::from_world(&self.world))?;
        let _ = binding(self, &counter)?;
        let slot = checked(counter.slot_bytes(&self.world, self.res.items()))?;
        let parsed = checked(counter.decode_slot(&slot, self.res.items()))?;
        let roster = checked(
            self.observe_checkpoint_participants()
                .map_err(|e| format!("{e:?}")),
        )?;
        let players = checked(self.project_session_participant_rows(&roster))?;
        checked(self.inspect_session_checkpoint_owners())?;
        let descriptors = players.iter().map(descriptor).collect::<Vec<_>>();
        require(
            self.level.player_uuid
                == Some(validate_native_local_player(
                    &self.world_root,
                    &descriptors,
                )?),
            "live/native singleplayer UUID mismatch",
        )?;
        if descriptors.iter().all(|row| !row.integrated_owner) {
            require(
                lan::session_empty_native_player(&self.level.player_tag()),
                "dedicated cached local player authority",
            )?;
        }
        let manifest = Manifest {
            format: 1,
            kind: counter.image_kind().into(),
            epoch: 1,
            world_uuid: world,
            dimension: self.dim.id().into(),
            saved_clock: self.world.game_time,
            target: checked(CounterTarget::new(&counter.dimension, counter.position))?,
            type_id: counter.type_id.clone(),
            counter: COUNTER.into(),
            slot: SLOT.into(),
            resident: self.world.chunks.keys().copied().collect(),
            players: descriptors,
            directories: Vec::new(),
            files: Vec::new(),
        };
        correlate(&manifest, &counter, &parsed)?;
        for row in &players {
            checked(lan::validate_session_player_tag(&row.player))?;
            let _ = nbt_bytes(&row.player)?;
            let _ = json_bytes(&row.advancements, STRUCTURED_LIMIT)?;
            let _ = json_bytes(&row.stats, STRUCTURED_LIMIT)?;
        }
        let _ = self.effective_session_raids()?;
        Ok(player_item_reserved_identities(&players))
    }
    fn effective_session_raids(&self) -> SResult<Option<Tag>> {
        let live = checked(self.session_checkpoint_raids())?;
        let disk = raid_metadata(&self.world_root)?;
        // An unopened native raid set is not authority to erase its saved
        // metadata. Loaded state supplies its actual current clock and next ID.
        Ok(live.or(disk))
    }
    fn session_running_packages(&self) -> SResult<()> {
        #[cfg(feature = "mods")]
        {
            checked(self.options.mod_packages.check_running(&self.mods.mods))
        }
        #[cfg(not(feature = "mods"))]
        {
            Err(fail("native session checkpoint requires the mod runtime"))
        }
    }

    fn session_warden_tracker(&self) -> SResult<()> {
        require(
            self.sculk.tracker.warning_level == 0
                && self.sculk.tracker.cooldown_ticks == 0
                && self.sculk.tracker.ticks_since_last_warning >= 0,
            "unsupported native warden warning, cooldown or invalid tracker counter",
        )
    }
    fn session_context(&self) -> SResult<WorldId> {
        let world = self.session_context_cpu()?;
        identity(&self.world_root, world)?;
        checked(self.options.mod_packages.check_world(&self.world_root))?;
        let _: Value = json(&read(&self.world_root.join(PIN), 64 << 10)?, 64 << 10)?;
        Ok(world)
    }
    fn session_context_cpu(&self) -> SResult<WorldId> {
        self.session_warden_tracker()?;
        require(
            self.remote.is_none()
                && self.integrated.is_none()
                && self.lan_acting.is_none()
                && !self.options.execution_role.is_client()
                && self.dim == Dim::Overworld
                && self.levels.parked.is_empty(),
            "single authoritative native Overworld required",
        )?;
        require(
            self.world_loader.is_none()
                && self.pending_entry.is_none()
                && self.rest_loader.is_none()
                && self.pending_save.is_none()
                && self.travelling.is_none()
                && self.respawn_travel.is_none()
                && self.end.locator().is_none()
                && self.cmds.pregen.is_none(),
            "native lifecycle/producer in progress",
        )?;
        require(
            self.container.is_none() && self.carried.is_none() && self.station.is_none(),
            "unsettled local menu/item owner",
        )?;
        let world = *self
            .session_lock
            .as_ref()
            .and_then(|s| s.expected_world())
            .ok_or_else(|| fail("identified retained native session lock required"))?;
        checked(
            self.native_checkpoint_origin
                .as_ref()
                .ok_or_else(|| fail("constructor-issued native origin absent"))?
                .validate(
                    &self.world_root,
                    self.dim,
                    self.save
                        .as_ref()
                        .ok_or_else(|| fail("native storage owner absent"))?,
                ),
        )?;
        if self.world.has_persistent_guest_counter() {
            let view = checked(self.world.guest_storage_view())?;
            let catalog = self
                .options
                .mod_packages
                .catalog()
                .ok_or_else(|| fail("Guest captured catalog absent"))?;
            require(
                catalog.prepared().len() == 1,
                "Guest storage requires exactly one captured pair",
            )?;
            let pair = catalog
                .prepared()
                .values()
                .next()
                .ok_or_else(|| fail("Guest pair absent"))?;
            let identity = pair
                .identity()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            require(
                view.counter().fields.package_identity == identity
                    && view.counter().execution.primary_owner == pair.bundle().content_owner().id,
                "Guest storage capture changed",
            )?;
            if view.counter().execution.status == GuestStatus::Ready {
                self.session_running_packages()?;
                #[cfg(feature = "mods")]
                {
                    checked(
                        self.mods
                            .mods
                            .counter_evaluator(&pair.bundle().content_owner().id, pair.identity()),
                    )?;
                }
                #[cfg(not(feature = "mods"))]
                {
                    return Err(fail("Guest runtime absent"));
                }
            }
        } else {
            self.session_running_packages()?;
        }
        Ok(world)
    }
    fn session_fence(&mut self, until: Instant) -> SResult<()> {
        let save = self
            .save
            .as_ref()
            .ok_or_else(|| fail("native storage owner absent"))?
            .clone();
        checked(
            self.streaming
                .expect_existing_mut()
                .native_write_fence_until(&mut self.world, until, || save.writes_finished()),
        )
    }
    fn session_snapshot(&self) -> SResult<Snapshot> {
        self.session_context()?;
        let raids = self.effective_session_raids()?;
        let local_uuid = native_local_uuid(&self.world_root)?;
        let snapshot = self.session_snapshot_cpu(raids, local_uuid)?;
        validate_native_local_player(
            &self.world_root,
            &snapshot.players.iter().map(descriptor).collect::<Vec<_>>(),
        )?;
        Ok(snapshot)
    }
    fn session_snapshot_cpu(&self, raids: Option<Tag>, local_uuid: [i32; 4]) -> SResult<Snapshot> {
        let roster = checked(
            self.observe_checkpoint_participants()
                .map_err(|e| format!("{e:?}")),
        )?;
        let players = checked(self.project_session_participant_rows(&roster))?;
        self.session_snapshot_from_players(raids, local_uuid, players)
    }
    fn session_snapshot_from_players(
        &self,
        raids: Option<Tag>,
        local_uuid: [i32; 4],
        players: Vec<SessionPlayerRow>,
    ) -> SResult<Snapshot> {
        let world = self.session_context_cpu()?;
        let counter = checked(SessionCounter::from_world(&self.world))?;
        let slot = checked(counter.slot_bytes(&self.world, self.res.items()))?;
        let parsed = checked(counter.decode_slot(&slot, self.res.items()))?;
        checked(self.inspect_session_checkpoint_owners())?;
        let items = self.project_session_checkpoint_items()?;
        check_item_player_identities(&items, &players)?;
        let item_memory = self.session_checkpoint_item_memory()?;
        let item_portals = checked(
            self.portals
                .project_session_item_portal_owners(&self.dropped),
        )?;
        let descriptors = players.iter().map(descriptor).collect::<Vec<_>>();
        require(
            self.level.player_uuid == Some(local_uuid),
            "live/native singleplayer UUID mismatch",
        )?;
        if descriptors.iter().all(|row| !row.integrated_owner) {
            require(
                lan::session_empty_native_player(&self.level.player_tag()),
                "dedicated cached local player authority",
            )?;
        }
        require(
            counter.world_uuid == world,
            "counter retained session identity",
        )?;
        let _ = binding(self, &counter)?;
        let mut columns = BTreeMap::new();
        for chunk in self.world.chunks.values() {
            // Every resident uses the exact current native save projection,
            // including ticks and inhabited time, without changing dirty markers.
            let saved = self.with_entities([chunk]).remove(0);
            let named = save::encode_with(
                &saved,
                self.pack.assets.registry,
                self.res.biomes(),
                self.save
                    .as_ref()
                    .ok_or_else(|| fail("native storage owner absent"))?
                    .tables
                    .get()
                    .map(Arc::as_ref),
                LevelShape::OVERWORLD,
            );
            columns.insert((chunk.x, chunk.z), named);
        }
        let residents = columns.keys().copied().collect();
        let manifest = Manifest {
            format: 1,
            kind: counter.image_kind().into(),
            epoch: 1,
            world_uuid: world,
            dimension: self.dim.id().into(),
            saved_clock: self.world.game_time,
            target: checked(CounterTarget::new(&counter.dimension, counter.position))?,
            type_id: counter.type_id.clone(),
            counter: COUNTER.into(),
            slot: SLOT.into(),
            resident: columns.keys().copied().collect(),
            players: players.iter().map(descriptor).collect(),
            directories: Vec::new(),
            files: Vec::new(),
        };
        correlate(&manifest, &counter, &parsed)?;
        for row in &players {
            checked(lan::validate_session_player_tag(&row.player))?;
            let _ = nbt_bytes(&row.player)?;
            let _ = json_bytes(&row.advancements, STRUCTURED_LIMIT)?;
            let _ = json_bytes(&row.stats, STRUCTURED_LIMIT)?;
        }
        Ok(Snapshot {
            counter,
            slot,
            players,
            columns,
            residents,
            raids,
            items,
            item_memory,
            item_portals,
            clocks: (self.world.game_time, self.world.day_time),
            random: self.world.random.state(),
            entity_head: self.next_entity,
        })
    }
    /// One continuous exclusive Game borrow spans both producer fences, native
    /// save(false), complete physical copying, semantic validation and promotion.
    /// UUID preparation is explicit: newly assigned identities survive a later
    /// failure and are reported separately from publication uncertainty.
    pub(in crate::game) fn checkpoint_session(
        &mut self,
        archive_root: &Path,
        epoch: u64,
        until: Instant,
    ) -> SResult<CheckpointReceipt> {
        self.checkpoint_session_steps(archive_root, epoch, until, |_| Ok(()))
    }
    pub(in crate::game) fn checkpoint_registered_session(
        &mut self,
        archive_root: &Path,
        epoch: u64,
        until: Instant,
    ) -> SResult<CheckpointReceipt> {
        self.checkpoint_session_profile_steps(
            NativeBindingProfile::RegisteredCube,
            false,
            archive_root,
            epoch,
            until,
            |_| Ok(()),
        )
    }
    #[cfg(test)]
    fn checkpoint_registered_session_with_test_fault(
        &mut self,
        archive: &Path,
        epoch: u64,
        until: Instant,
        fault: CheckpointTestFault,
    ) -> SResult<CheckpointReceipt> {
        self.checkpoint_session_profile_steps(
            NativeBindingProfile::RegisteredCube,
            false,
            archive,
            epoch,
            until,
            |step| {
                if matches!(
                    (fault, step),
                    (CheckpointTestFault::Copied, Step::Copied)
                        | (CheckpointTestFault::Promoted, Step::Promoted)
                ) {
                    Err(fail("injected registered checkpoint post-phase failure"))
                } else {
                    Ok(())
                }
            },
        )
    }
    #[cfg(test)]
    pub(in crate::game) fn checkpoint_session_with_test_fault(
        &mut self,
        archive: &Path,
        epoch: u64,
        until: Instant,
        fault: CheckpointTestFault,
    ) -> SResult<CheckpointReceipt> {
        self.checkpoint_session_steps(archive, epoch, until, |step| {
            if matches!(
                (fault, step),
                (CheckpointTestFault::Copied, Step::Copied)
                    | (CheckpointTestFault::Promoted, Step::Promoted)
            ) {
                Err(fail("injected checkpoint post-phase failure"))
            } else {
                Ok(())
            }
        })
    }
    fn checkpoint_session_steps(
        &mut self,
        archive_root: &Path,
        epoch: u64,
        until: Instant,
        step: impl FnMut(Step) -> SResult<()>,
    ) -> SResult<CheckpointReceipt> {
        self.checkpoint_session_profile_steps(
            NativeBindingProfile::LegacyIron,
            false,
            archive_root,
            epoch,
            until,
            step,
        )
    }
    fn checkpoint_session_profile_steps(
        &mut self,
        profile: NativeBindingProfile,
        guest: bool,
        archive_root: &Path,
        epoch: u64,
        until: Instant,
        mut step: impl FnMut(Step) -> SResult<()>,
    ) -> SResult<CheckpointReceipt> {
        let mut publication = Publication::NotPublished;
        let mut item_identities_assigned = 0;
        let result = (|| {
            deadline(until)?;
            let world = self.session_context()?;
            let frame = checked(SessionCounter::from_world(&self.world))?;
            require(
                frame.is_guest() == guest,
                "checkpoint adapter execution mismatch",
            )?;
            require(
                checked(frame.binding_profile())? == profile,
                "checkpoint adapter physical profile mismatch",
            )?;
            require(epoch > 0, "zero epoch")?;
            let archive = directory(archive_root)?;
            let source = directory(&self.world_root)?;
            require(
                !archive.starts_with(&source) && !source.starts_with(&archive),
                "overlapping archive/world",
            )?;
            let existing = epochs(&archive)?;
            require(
                existing.last().is_none_or(|(e, _)| epoch > *e),
                "epoch must increase monotonically",
            )?;
            let pending = archive.join(format!(".pending-{epoch:020}"));
            let published = archive.join(format!("epoch-{epoch:020}"));
            require(
                !pending.try_exists().map_err(fail)? && !published.try_exists().map_err(fail)?,
                "epoch path already exists",
            )?;
            // Bound and reject links before native saving touches the source tree.
            let _ = tree(&source, &["session.lock"])?;
            self.session_fence(until)?;
            // Preflight every old source record before this checkpoint submits
            // writes through native constructors that can normalize bad headers.
            // Dirty target/frame equality is checked only after its real save.
            let old_columns = columns(self, &source, self.world.game_time)?;
            let old_items = read_session_checkpoint_items(
                self,
                &source,
                &self.world.chunks.keys().copied().collect(),
                &old_columns,
            )?;
            let reserved = self.session_checkpoint_identity_preflight()?;
            require(
                reserved
                    .iter()
                    .all(|uuid| !old_items.projection.rows.contains_key(uuid)),
                "saved source item/player UUID collision",
            )?;
            item_identities_assigned = self.prepare_session_checkpoint_item_identities()?;
            let before = self.session_snapshot()?;
            checked(self.save_game_with(false))?;
            deadline(until)?;
            checked(self.write_session_checkpoint_raids())?;
            // Native autosave writes dirty columns. Checkpoint additionally queues
            // real snapshots for every resident so clean inhabited metadata and
            // the current saved clock cannot be lost.
            deadline(until)?;
            let residents = self.with_entities(self.world.chunks.values());
            checked(
                self.save
                    .as_ref()
                    .ok_or_else(|| fail("native storage owner absent"))?
                    .save_later(&residents, self.pack.assets.registry, self.res.biomes()),
            )?;
            self.session_fence(until)?;
            let after = self.session_snapshot()?;
            require(
                before.counter == after.counter
                    && before.slot == after.slot
                    && same_players(&before.players, &after.players)
                    && before.players.iter().all(|a| {
                        after
                            .players
                            .iter()
                            .any(|b| a.profile_uuid == b.profile_uuid && a.conn == b.conn)
                    })
                    && before.residents == after.residents,
                "native save changed session semantic owners/residency",
            )?;
            require(
                before.items == after.items
                    && before.item_memory == after.item_memory
                    && before.item_portals == after.item_portals
                    && before.clocks == after.clocks
                    && before.random == after.random
                    && before.entity_head == after.entity_head,
                "native save changed item/memory/portal/clock/RNG projection",
            )?;
            require(
                before.raids == after.raids,
                "native save changed empty raid metadata",
            )?;
            require(
                before.columns == after.columns,
                "native save changed full resident saved projection",
            )?;
            require(self.session_context()? == world, "native context changed")?;
            let saved_columns = columns(self, &source, self.world.game_time)?;
            let saved_items = read_session_checkpoint_items(
                self,
                &source,
                &before.residents.iter().copied().collect(),
                &saved_columns,
            )?;
            require(
                saved_items.projection == before.items,
                "saved native item projection mismatch",
            )?;
            let source_inventory = tree(&source, &["session.lock"])?;
            checked(fs::create_dir(&pending))?;
            copy_tree(&source, &pending.join("world"), &source_inventory, until)?;
            require(
                tree(&source, &["session.lock"])? == source_inventory,
                "source inventory/hash changed during copy",
            )?;
            step(Step::Copied)?;
            for row in &before.players {
                let descriptor = descriptor(row);
                for (kind, bytes) in [
                    ("data", nbt_bytes(&row.player)?),
                    (
                        "advancements",
                        json_bytes(&row.advancements, STRUCTURED_LIMIT)?,
                    ),
                    ("stats", json_bytes(&row.stats, STRUCTURED_LIMIT)?),
                ] {
                    overlay(&parents(&pending, &player_path(&descriptor, kind))?, &bytes)?;
                }
            }
            write_new(
                &parents(&pending, COUNTER)?,
                &checked(before.counter.encode())?,
            )?;
            write_new(&parents(&pending, SLOT)?, &before.slot)?;
            step(Step::Overlaid)?;
            let physical = tree(&pending, &[])?;
            let manifest = Manifest {
                format: 1,
                kind: before.counter.image_kind().into(),
                epoch,
                world_uuid: world,
                dimension: self.dim.id().into(),
                saved_clock: self.world.game_time,
                target: checked(CounterTarget::new(
                    &before.counter.dimension,
                    before.counter.position,
                ))?,
                type_id: before.counter.type_id.clone(),
                counter: COUNTER.into(),
                slot: SLOT.into(),
                resident: before.residents.clone(),
                players: before.players.iter().map(descriptor).collect(),
                directories: physical.directories,
                files: physical.files,
            };
            manifest_shape(&manifest, epoch)?;
            sync_tree(&pending)?;
            step(Step::MembersSynced)?;
            write_new(
                &pending.join("manifest.json"),
                &json_bytes(&manifest, MANIFEST_LIMIT)?,
            )?;
            sync_dir(&pending)?;
            step(Step::ManifestCompleted)?;
            let verified = verify_image(
                self,
                &pending,
                epoch,
                world,
                &manifest.target,
                &manifest.type_id,
            )?;
            require(
                same_players(&before.players, verified.players.rows()),
                "detached player image changed",
            )?;
            require(
                before.items == verified.items.projection
                    && saved_items.projection == verified.items.projection,
                "copied native item projection mismatch",
            )?;
            require(
                before.raids == verified.raids && self.effective_session_raids()? == before.raids,
                "copied/current native raid metadata mismatch",
            )?;
            for (at, expected) in &before.columns {
                let actual = verified
                    .columns
                    .get(at)
                    .ok_or_else(|| fail("native saved resident record missing"))?;
                let actual = save::encode_with(
                    actual,
                    self.pack.assets.registry,
                    self.res.biomes(),
                    self.save
                        .as_ref()
                        .ok_or_else(|| fail("native storage owner absent"))?
                        .tables
                        .get()
                        .map(Arc::as_ref),
                    LevelShape::OVERWORLD,
                );
                require(
                    lan::session_player_tags_equal(&expected.tag, &actual.tag),
                    "saved native resident projection mismatch",
                )?;
            }
            require(
                tree(&source, &["session.lock"])? == source_inventory,
                "source changed after candidate validation",
            )?;
            require(
                self.session_context()? == world,
                "final native context changed",
            )?;
            deadline(until)?;
            step(Step::BeforePromotion)?;
            // A failing rename can have taken effect. Definite rollback is never
            // inferred from its return value, and pending evidence is never erased.
            publication = Publication::Uncertain;
            checked(fs::rename(&pending, &published))?;
            step(Step::Promoted)?;
            sync_dir(&archive)?;
            step(Step::ParentSynced)?;
            Ok(CheckpointReceipt {
                epoch,
                root: published,
            })
        })();
        result.map_err(|mut error: SessionCheckpointError| {
            error.publication = publication;
            error.item_identities_assigned = item_identities_assigned;
            error
        })
    }
}
fn epoch_name(name: &str, prefix: &str) -> SResult<u64> {
    let suffix = name
        .strip_prefix(prefix)
        .ok_or_else(|| fail("unexpected archive entry"))?;
    let number: u64 = checked(suffix.parse())?;
    require(
        number > 0 && format!("{number:020}") == suffix,
        "epoch spelling",
    )?;
    Ok(number)
}
/// Inspect every published and pending epoch without reserving or changing it.
/// Keep one archive entry available for the next checkpoint's pending image.
pub(in crate::game) fn next_session_checkpoint_epoch(archive: &Path) -> SResult<u64> {
    let archive = directory(archive)?;
    let mut count = 0;
    let mut latest = 0;
    for entry in checked(fs::read_dir(&archive))? {
        count += 1;
        require(count < ENTRY_LIMIT, "archive entry capacity exhausted")?;
        let entry = checked(entry)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| fail("non-UTF8 epoch"))?;
        let meta = checked(fs::symlink_metadata(entry.path()))?;
        require(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "non-directory/linked archive entry",
        )?;
        let prefix = if name.starts_with(".pending-") {
            ".pending-"
        } else {
            "epoch-"
        };
        latest = latest.max(epoch_name(&name, prefix)?);
    }
    latest
        .checked_add(1)
        .ok_or_else(|| fail("checkpoint epoch overflow"))
}
fn epochs(archive: &Path) -> SResult<Vec<(u64, PathBuf)>> {
    let mut result = Vec::new();
    let mut count = 0;
    for entry in checked(fs::read_dir(archive))? {
        count += 1;
        require(count <= ENTRY_LIMIT, "archive entry bound")?;
        let entry = checked(entry)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| fail("non-UTF8 epoch"))?;
        let meta = checked(fs::symlink_metadata(entry.path()))?;
        require(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "non-directory/linked archive entry",
        )?;
        if name.starts_with(".pending-") {
            epoch_name(&name, ".pending-")?;
            continue;
        }
        let epoch = epoch_name(&name, "epoch-")?;
        result.push((epoch, entry.path()));
    }
    result.sort_by_key(|(epoch, _)| *epoch);
    Ok(result)
}
/// Exact requested set for native nondecorating streaming with radius=0, no
/// forced/band, and radius-zero extras retaining every center: native finished
/// square grown by the nondecorating provider reach (`Ranges::requested/near_finished`). Main-circle coverage is a subset. No fixed resident set.
fn required_coverage(centers: &BTreeSet<(i32, i32)>) -> SResult<BTreeSet<(i32, i32)>> {
    let mut required = BTreeSet::new();
    for &(x, z) in centers {
        for dz in -2i32..=2 {
            for dx in -2i32..=2 {
                if dx.abs() <= 2 && dz.abs() <= 2 {
                    required.insert((
                        x.checked_add(dx)
                            .ok_or_else(|| fail("coverage coordinate overflow"))?,
                        z.checked_add(dz)
                            .ok_or_else(|| fail("coverage coordinate overflow"))?,
                    ));
                }
            }
        }
    }
    require(required.len() <= NODES, "recovery coverage bound")?;
    Ok(required)
}
fn player_centers(
    rows: &[SessionPlayerRow],
    target: &CounterTarget,
) -> SResult<BTreeSet<(i32, i32)>> {
    let mut centers = BTreeSet::from([(target.position[0] >> 4, target.position[2] >> 4)]);
    for row in rows {
        let position = list(row.player.get("Pos"))?;
        require(position.len() == 3, "participant position shape")?;
        let coordinate = |i| -> SResult<i32> {
            let Tag::Double(bits) = position[i] else {
                return Err(fail("participant position type"));
            };
            let value = f64::from_bits(bits);
            require(
                value.is_finite()
                    && value.floor() >= i32::MIN as f64
                    && value.floor() <= i32::MAX as f64,
                "participant position range",
            )?;
            Ok((value.floor() as i32) >> 4)
        };
        centers.insert((coordinate(0)?, coordinate(2)?));
    }
    Ok(centers)
}
#[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
enum SessionItemLoadTestMode {
    HoldToDeadline,
    ReleaseAfterEntry,
    DropBeforeEntry,
}
#[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
struct SessionItemLoadTest {
    column: (i32, i32),
    mode: SessionItemLoadTestMode,
    observer: Option<horizon_client::streaming::NativeColumnLoadTestObservation>,
}
impl<'a> Game<'a> {
    fn fresh_session_candidate(&self) -> SResult<()> {
        if self.ordinary_startup.is_some() {
            require(
                self.save.is_none() && self.streaming.is_vacant(),
                "ordinary recovery requires actual no-world/no-save/no-worker vacancy",
            )?;
        }
        self.session_warden_tracker()?;
        require(
            checked(self.session_checkpoint_raids())?.is_none(),
            "candidate already owns loaded raid metadata",
        )?;
        require(
            self.renderer.is_none()
                && self.remote.is_none()
                && self.integrated.is_none()
                && self.lan.is_none()
                && self.lan_acting.is_none()
                && self.serving.is_none()
                && !self.options.execution_role.is_client(),
            "fresh headless candidate required",
        )?;
        require(
            self.session_lock.is_none()
                && self.native_checkpoint_origin.is_none()
                && self.world_root.as_os_str().is_empty()
                && self.world.chunks.is_empty()
                && self.world.counter_trial_snapshot().is_none()
                && !self.world.has_world_slot_trial()
                && self.levels.parked.is_empty(),
            "candidate already owns a world/session",
        )?;
        require(
            self.player.is_none()
                && self.inventory.iter().all(Option::is_none)
                && self.armor.iter().all(Option::is_none)
                && self.offhand.is_none()
                && self.ender_items.iter().all(Option::is_none)
                && self.carried.is_none()
                && self.container.is_none()
                && self.station.is_none(),
            "candidate local player/item owner",
        )?;
        require(
            self.world_loader.is_none()
                && self.pending_entry.is_none()
                && self.rest_loader.is_none()
                && self.pending_save.is_none()
                && self.travelling.is_none()
                && self.respawn_travel.is_none()
                && self.end.locator().is_none()
                && self.cmds.pregen.is_none(),
            "candidate lifecycle operation",
        )?;
        require(
            self.options.dimension.is_none()
                && self.options.time.is_none()
                && self.options.weather.is_none()
                && self.options.look.is_none(),
            "recovery native overrides refused",
        )?;
        checked(self.inspect_session_checkpoint_owners())
    }
    /// Failure consumes and drops the entire unpublished Game. There is no
    /// rollback claim, partial Game return, reused live host, tick or as_guest.
    /// Dormant managed entry: acquire exclusion BEFORE selecting any image and
    /// transfer it into the recovered native session BEFORE returning that Game.
    pub(in crate::game) fn recover_managed_session_checkpoint(
        self,
        namespace: &crate::game::world_loading::host_identity::session_authority::AuthorityNamespace,
        request: ManagedRecoveryRequest,
        host: LanHost,
    ) -> SResult<(Self, RecoveryReceipt)> {
        deadline(request.deadline)?;
        let mut lease = Some(checked(namespace.acquire_recovery(
            &request.owner_root,
            &request.archive_root,
            &request.destination,
        ))?);
        let expected_world = lease.as_ref().expect("just acquired authority").world();
        let (mut game, receipt) = self.recover_session_checkpoint(
            RecoveryRequest {
                expected_world,
                expected_target: request.expected_target,
                expected_type: request.expected_type,
                archive_root: request.archive_root,
                destination: request.destination,
                deadline: request.deadline,
            },
            host,
        )?;
        let session = game
            .session_lock
            .as_mut()
            .ok_or_else(|| fail("missing recovered native session"))?;
        checked(session.install_authority(&mut lease))?;
        Ok((game, receipt))
    }

    pub(in crate::game) fn recover_session_checkpoint(
        self,
        request: RecoveryRequest,
        host: LanHost,
    ) -> SResult<(Self, RecoveryReceipt)> {
        let mut parts = Box::new(self).prepare_session_checkpoint(request)?;
        checked(
            parts
                .game
                .install_session_checkpoint_participants(host, &parts.verified.players),
        )?;
        let (game, receipt) = parts.finish()?;
        Ok((*game, receipt))
    }

    pub(in crate::game) fn recover_managed_registered_session_checkpoint(
        self,
        namespace: &crate::game::world_loading::host_identity::session_authority::AuthorityNamespace,
        request: ManagedRecoveryRequest,
        host: LanHost,
    ) -> SResult<(Self, RegisteredRecoveryReceipt)> {
        deadline(request.deadline)?;
        let mut lease = Some(checked(namespace.acquire_recovery(
            &request.owner_root,
            &request.archive_root,
            &request.destination,
        ))?);
        let expected_world = lease.as_ref().expect("just acquired authority").world();
        let (mut game, receipt) = self.recover_registered_session_checkpoint(
            RecoveryRequest {
                expected_world,
                expected_target: request.expected_target,
                expected_type: request.expected_type,
                archive_root: request.archive_root,
                destination: request.destination,
                deadline: request.deadline,
            },
            host,
        )?;
        let session = game
            .session_lock
            .as_mut()
            .ok_or_else(|| fail("missing recovered native session"))?;
        checked(session.install_authority(&mut lease))?;
        Ok((game, receipt))
    }

    pub(in crate::game) fn recover_registered_session_checkpoint(
        self,
        request: RecoveryRequest,
        host: LanHost,
    ) -> SResult<(Self, RegisteredRecoveryReceipt)> {
        let mut parts = Box::new(self).prepare_registered_session_checkpoint(request)?;
        checked(
            parts
                .core
                .game
                .install_session_checkpoint_participants(host, &parts.core.verified.players),
        )?;
        let (game, receipt) = parts.finish()?;
        Ok((*game, receipt))
    }

    pub(in crate::game) fn prepare_registered_session_checkpoint(
        self: Box<Self>,
        request: RecoveryRequest,
    ) -> SResult<RegisteredNativeRecoveryParts<'a>> {
        Ok(RegisteredNativeRecoveryParts {
            core: self.prepare_session_checkpoint_profile_steps(
                NativeBindingProfile::RegisteredCube,
                false,
                request,
                |_, _| Ok(()),
                #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
                None,
            )?,
        })
    }

    /// Own the unpublished native candidate before admitting any transport.
    /// Participant installation and the final correlation remain separate gates.
    pub(in crate::game) fn prepare_session_checkpoint(
        self: Box<Self>,
        request: RecoveryRequest,
    ) -> SResult<NativeRecoveryParts<'a>> {
        self.prepare_session_checkpoint_before_items(request, |_, _| Ok(()))
    }
    // Private defining-test seam, before item staging/publication. No callback
    // runs in the concrete infallible installer commit.
    fn prepare_session_checkpoint_before_items(
        self: Box<Self>,
        request: RecoveryRequest,
        before_items: impl FnOnce(&mut Game<'a>, &CheckedSessionItems) -> SResult<()>,
    ) -> SResult<NativeRecoveryParts<'a>> {
        self.prepare_session_checkpoint_steps(
            request,
            before_items,
            #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
            None,
        )
    }
    fn prepare_session_checkpoint_steps(
        self: Box<Self>,
        request: RecoveryRequest,
        before_items: impl FnOnce(&mut Game<'a>, &CheckedSessionItems) -> SResult<()>,
        #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
        item_load_test: Option<&mut SessionItemLoadTest>,
    ) -> SResult<NativeRecoveryParts<'a>> {
        let core = self.prepare_session_checkpoint_profile_steps(
            NativeBindingProfile::LegacyIron,
            false,
            request,
            before_items,
            #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
            item_load_test,
        )?;
        let CoreRecoveryState::Native(RegisteredRecoveryState::Active { counter, slot }) =
            core.receipt.state
        else {
            return Err(fail("legacy recovery requires active keys"));
        };
        Ok(NativeRecoveryParts {
            game: core.game,
            request: core.request,
            verified: core.verified,
            selected: core.selected,
            clocks: core.clocks,
            random: core.random,
            receipt: RecoveryReceipt {
                epoch: core.receipt.epoch,
                counter,
                slot,
            },
        })
    }
    fn prepare_session_checkpoint_profile_steps(
        self: Box<Self>,
        profile: NativeBindingProfile,
        guest: bool,
        request: RecoveryRequest,
        before_items: impl FnOnce(&mut Game<'a>, &CheckedSessionItems) -> SResult<()>,
        #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
        item_load_test: Option<&mut SessionItemLoadTest>,
    ) -> SResult<PreparedSessionRecovery<'a>> {
        // Legacy convenience callers retain their historical failure semantics.
        // The ordinary owner below instead retains this SAME work on refusal.
        let mut work = SessionRecoveryWork::legacy(self);
        let data = work.prepare_steps(
            profile,
            guest,
            request,
            before_items,
            #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
            item_load_test,
        )?;
        Ok(data.with_game(work.game.take().unwrap()))
    }
}

/// Real partial recovery ownership. Ordinary callers must resolve its native
/// workers and original preparation allocation before dropping a failed attempt.
struct SessionRecoveryWork<'a> {
    game: Option<Box<Game<'a>>>,
    preparation: Option<RecoveryPreparation>,
    ordinary: Option<(
        crate::game::world_loading::RetainedEntryIdentity,
        Arc<crate::game::mod_packages::FrozenHostInputs>,
    )>,
    unpublished: Option<crate::game::world_loading::UnpublishedRecoveryNative>,
    armed: bool,
}
enum RecoveryPreparation {
    Legacy(crate::game::world_loading::PreparedWorld),
    Ordinary(crate::game::world_loading::OrdinaryPreparedWorld),
    Failed(crate::game::world_loading::OrdinaryPreparation),
}
impl RecoveryPreparation {
    fn prepared_mut(&mut self) -> &mut crate::game::world_loading::PreparedWorld {
        match self {
            Self::Legacy(prepared) => prepared,
            Self::Ordinary(prepared) => prepared.prepared_mut(),
            Self::Failed(_) => unreachable!("failed preparation cannot authorize recovery"),
        }
    }
}
impl Drop for SessionRecoveryWork<'_> {
    fn drop(&mut self) {
        if self.armed {
            eprintln!("fatal: unresolved original ordinary recovery work");
            std::process::abort();
        }
    }
}
struct RecoveryData {
    request: RecoveryRequest,
    verified: NativeRecoveryImage,
    selected: PathBuf,
    clocks: (i64, i64),
    random: u64,
    receipt: CoreRecoveryReceipt,
}
impl RecoveryData {
    fn with_game<'a>(self, game: Box<Game<'a>>) -> PreparedSessionRecovery<'a> {
        let Self {
            request,
            verified,
            selected,
            clocks,
            random,
            receipt,
        } = self;
        PreparedSessionRecovery {
            game,
            request,
            verified,
            selected,
            clocks,
            random,
            receipt,
        }
    }
}
impl<'a> SessionRecoveryWork<'a> {
    fn reconcile_ordinary_reads(
        game: &Game<'_>,
        prepared: &mut crate::game::world_loading::PreparedWorld,
        columns: &BTreeMap<(i32, i32), Arc<Chunk>>,
    ) -> SResult<()> {
        let context = game.capture_native_validation_context();
        let accepted = BTreeSet::new();
        while let Some(view) = checked(prepared.streaming.poll_retained_open_load())? {
            let load = view.load();
            if let Some(error) = load.error() {
                return Err(fail(error));
            }
            let chunk = load
                .chunk()
                .ok_or_else(|| fail("ordinary recovery read has no native column"))?;
            let result = checked(
                crate::game::world_loading::validate_retained_column_context(
                    &context,
                    &game.pack,
                    &game.res,
                    &game.persist.uuids,
                    &prepared.save_root,
                    Some(&prepared.level),
                    Some(&prepared.world),
                    Some(&prepared.session_lock),
                    &prepared.save,
                    prepared.native_checkpoint_origin.as_ref(),
                    &accepted,
                    chunk,
                    load.evidence(),
                ),
            )?;
            match result {
                None => break,
                Some((horizon_client::streaming::RetainedColumnDisposition::AlreadyStored, _)) => {}
                Some(_) => {
                    return Err(fail(
                        "ordinary recovery cannot adopt freshly generated saved input",
                    ));
                }
            }
            let expected = columns.get(&(chunk.x, chunk.z)).ok_or_else(|| {
                fail("ordinary recovered original read is outside verified native image")
            })?;
            let encoded = save::encode(expected, game.pack.assets.registry, game.res.biomes());
            checked(crate::game::world_loading::validate_retained_projection(
                chunk, &encoded, expected,
            ))?;
            let ticket = view.into_ticket();
            if !checked(
                prepared
                    .streaming
                    .accept_retained_open_load(ticket, &mut prepared.world),
            )? {
                break;
            }
        }
        Ok(())
    }
    fn legacy(game: Box<Game<'a>>) -> Self {
        Self {
            game: Some(game),
            preparation: None,
            ordinary: None,
            unpublished: None,
            armed: false,
        }
    }
    fn prepare_steps(
        &mut self,
        profile: NativeBindingProfile,
        guest: bool,
        request: RecoveryRequest,
        before_items: impl FnOnce(&mut Game<'a>, &CheckedSessionItems) -> SResult<()>,
        #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
        mut item_load_test: Option<&mut SessionItemLoadTest>,
    ) -> SResult<RecoveryData> {
        let game = self
            .game
            .as_mut()
            .expect("same original recovery candidate");
        game.fresh_session_candidate()?;
        deadline(request.deadline)?;
        let _ = game.advancement_tree();
        let archive = directory(&request.archive_root)?;
        ancestors(&request.destination)?;
        require(
            !request.destination.try_exists().map_err(fail)?,
            "recovery destination already exists",
        )?;
        let parent = directory(
            request
                .destination
                .parent()
                .ok_or_else(|| fail("destination parent"))?,
        )?;
        require(
            !parent.starts_with(&archive) && !archive.starts_with(&request.destination),
            "overlapping recovery/archive roots",
        )?;
        let (epoch, selected) = epochs(&archive)?
            .pop()
            .ok_or_else(|| fail("no complete published epoch"))?;
        // Newest complete name is authoritative: corrupt/incompatible successors
        // refuse. Never borrow older frames or silently fall back to an old epoch.
        let verified = verify_image(
            game,
            &selected,
            epoch,
            request.expected_world,
            &request.expected_target,
            &request.expected_type,
        )?;
        require(
            checked(verified.counter.binding_profile())? == profile,
            "recovery adapter physical profile mismatch",
        )?;
        require(
            verified.counter.is_guest() == guest,
            "recovery adapter execution profile mismatch",
        )?;
        if guest {
            game.prepare_guest_image_evaluator(&verified.counter)?;
        }
        let mut centers = player_centers(verified.players.rows(), &request.expected_target)?;
        let coverage = required_coverage(&centers)?;
        for at in &coverage {
            require(
                verified.columns.contains_key(at),
                "recovery required native full coverage missing before loader",
            )?;
        }
        centers.extend(verified.items.columns());
        require(centers.len() <= NODES, "recovery load center union bound")?;
        let world_inventory = tree(&selected.join("world"), &[])?;
        copy_tree(
            &selected.join("world"),
            &request.destination,
            &world_inventory,
            request.deadline,
        )?;
        require(
            tree(&selected.join("world"), &[])? == world_inventory,
            "selected image changed during recovery copy",
        )?;
        sync_tree(&request.destination)?;
        sync_dir(&parent)?;
        let mut open = checked(WorldRequest::new(
            request
                .destination
                .to_str()
                .ok_or_else(|| fail("destination UTF8"))?,
            None,
            None,
            false,
            None,
        ))?;
        open.save_root = request.destination.clone();
        let open = open.with_expected_checkpoint_world(request.expected_world);
        self.preparation = Some(if let Some((entry, startup)) = self.ordinary.as_ref() {
            let preparation = crate::game::world_loading::OrdinaryPreparation::prepare(
                open,
                &game.res,
                &game.pack,
                &game.options,
                &game.settings,
                entry,
                startup.clone(),
            );
            match preparation.into_prepared(entry) {
                Ok(prepared) => RecoveryPreparation::Ordinary(prepared),
                Err(preparation) => {
                    let reason = preparation
                        .original_error()
                        .unwrap_or("ordinary recovery preparation transfer refused")
                        .to_owned();
                    self.preparation = Some(RecoveryPreparation::Failed(preparation));
                    return Err(fail(reason));
                }
            }
        } else {
            RecoveryPreparation::Legacy(checked(open.prepare(
                &game.res,
                &game.pack,
                &game.options,
                &game.settings,
            ))?)
        });
        let prepared = self
            .preparation
            .as_mut()
            .expect("original acquired preparation")
            .prepared_mut();
        // Retain the newly acquired native lock while rechecking complete bytes,
        // identity and packages; native APIs have no permission to regenerate.
        require(
            prepared.session_lock.expected_world() == Some(&request.expected_world),
            "locked native identity",
        )?;
        identity(&request.destination, request.expected_world)?;
        require(
            tree(&request.destination, &["session.lock"])? == world_inventory,
            "recovery copy changed under native lock",
        )?;
        checked(game.options.mod_packages.check_world(&request.destination))?;
        let under_lock = columns(game, &request.destination, verified.manifest.saved_clock)?;
        let locked_items = read_session_checkpoint_items(
            game,
            &request.destination,
            &verified.manifest.resident.iter().copied().collect(),
            &under_lock,
        )?;
        require(
            locked_items.projection == verified.items.projection,
            "locked native entity projection changed",
        )?;
        for at in &coverage {
            require(
                under_lock.contains_key(at),
                "locked native coverage missing",
            )?;
        }
        require(
            prepared.native_checkpoint_origin.is_some() && prepared.locator.is_none(),
            "recovery requires witnessed TestTerrain native provider",
        )?;
        require(
            prepared.world.game_time == verified.manifest.saved_clock,
            "prepared native clock",
        )?;
        // Declared after `prepared`: on any failure the owned receipt releases
        // BEFORE prepared Streaming is dropped and its actual workers are joined.
        #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
        let mut item_load_receipt = None;
        #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
        if let Some(test) = item_load_test.as_deref_mut() {
            require(
                verified.items.columns().contains(&test.column)
                    && !player_centers(verified.players.rows(), &request.expected_target)?
                        .contains(&test.column)
                    && !prepared.world.chunks.contains_key(&test.column),
                "item load test requires an unarrived verified distant item column",
            )?;
            let receipt = checked(
                prepared
                    .streaming
                    .arm_native_column_load_test_control(test.column),
            )?;
            test.observer = Some(receipt.observer());
            if matches!(test.mode, SessionItemLoadTestMode::DropBeforeEntry) {
                drop(receipt);
            } else {
                item_load_receipt = Some(receipt);
            }
        }
        let clocks = (prepared.world.game_time, prepared.world.day_time);
        let recovery_random = prepared.world.random.state();
        prepared.streaming.radius = 0;
        prepared.streaming.extra = centers.iter().copied().map(|at| (at, 0)).collect();
        require(
            prepared.streaming.forced.is_empty(),
            "recovery forced native producer",
        )?;
        let target = (
            request.expected_target.position[0] >> 4,
            request.expected_target.position[2] >> 4,
        );
        for &center in &centers {
            prepared.streaming.center = center;
            loop {
                deadline(request.deadline)?;
                let _ = prepared.streaming.update(&mut prepared.world);
                if self.ordinary.is_some() {
                    Self::reconcile_ordinary_reads(game, prepared, &under_lock)?;
                }
                #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
                if item_load_test.as_ref().is_some_and(|test| {
                    matches!(test.mode, SessionItemLoadTestMode::ReleaseAfterEntry)
                        && test
                            .observer
                            .as_ref()
                            .is_some_and(|observer| observer.entered())
                }) && let Some(mut receipt) = item_load_receipt.take()
                {
                    checked(
                        receipt.wait_entered(
                            request.deadline.saturating_duration_since(Instant::now()),
                        ),
                    )?;
                    receipt.release();
                }
                require(
                    (prepared.world.game_time, prepared.world.day_time) == clocks,
                    "streaming advanced native clocks",
                )?;
                require(
                    prepared.world.random.state() == recovery_random,
                    "streaming drew from native world random",
                )?;
                if prepared.world.chunks.contains_key(&center) {
                    break;
                }
                std::thread::yield_now();
            }
        }
        // Return center to the machine while real arrivals may enlarge residency.
        prepared.streaming.center = target;
        if self.ordinary.is_some() {
            // Every outstanding original saved read must be consumed before the
            // existing native fence; that fence intentionally never adopts reads.
            loop {
                deadline(request.deadline)?;
                let _ = prepared.streaming.update(&mut prepared.world);
                Self::reconcile_ordinary_reads(game, prepared, &under_lock)?;
                let work = prepared.streaming.observe_native_work();
                require(
                    !work.accounting_fault
                        && !work.receipt_mismatch
                        && !work.worker_initialization_fault
                        && !work.worker_exit_fault
                        && !work.result_disconnected
                        && !work.stopped,
                    "ordinary recovery native read ownership fault",
                )?;
                let (_, generated) = work
                    .origins
                    .iter()
                    .find(|(origin, _)| {
                        *origin == horizon_client::streaming::NativeWorkOrigin::Generate
                    })
                    .unwrap();
                require(
                    generated.unresolved == 0,
                    "ordinary recovery has unresolved original reads",
                )?;
                if generated.queued == 0
                    && generated.worker_held == 0
                    && generated.returned == 0
                    && generated.reconciling == 0
                {
                    break;
                }
                std::thread::yield_now();
            }
        }
        let save = prepared.save.clone();
        checked(prepared.streaming.native_write_fence_until(
            &mut prepared.world,
            request.deadline,
            || save.writes_finished(),
        ))?;
        require(
            (prepared.world.game_time, prepared.world.day_time) == clocks,
            "fence advanced native clocks",
        )?;
        require(
            prepared.world.random.state() == recovery_random,
            "recovery fence drew from native world random",
        )?;
        for (at, chunk) in &prepared.world.chunks {
            let expected = under_lock
                .get(at)
                .ok_or_else(|| fail("unexpected regenerated native arrival"))?;
            let a = save::encode(chunk, game.pack.assets.registry, game.res.biomes());
            let b = save::encode(expected, game.pack.assets.registry, game.res.biomes());
            require(
                lan::session_player_tags_equal(&a.tag, &b.tag),
                "native arrival changed saved semantics",
            )?;
        }
        verified.items.check_arrivals(
            game,
            &prepared.world.chunks.keys().copied().collect(),
            &prepared.world.loaded_entities,
        )?;
        let binding = binding(game, &verified.counter)?;
        let state = if let SessionCounter::Guest(frame) = &verified.counter {
            checked(prepared.world.restore_guest_counter_frame(
                &binding,
                request.expected_world,
                &request.expected_target,
                frame,
            ))?;
            prepared
                .world
                .restore_guest_world_slot(
                    &binding,
                    game.res.items(),
                    request.expected_world,
                    &request.expected_target,
                    verified.slot,
                )
                .map_err(|e| fail(e.error))?;
            CoreRecoveryState::Guest(checked(prepared.world.guest_runtime_state())?)
        } else {
            let counter = checked(prepared.world.restore_counter_frame(
                &binding,
                request.expected_world,
                &request.expected_target,
                &verified.counter,
            ))?;
            let slot = prepared
                .world
                .restore_persistent_world_slot(
                    &binding,
                    game.res.items(),
                    request.expected_world,
                    &request.expected_target,
                    verified.slot,
                )
                .map_err(|e| fail(e.error))?;
            CoreRecoveryState::Native(match (counter, slot) {
                (Some(counter), Some(slot)) => RegisteredRecoveryState::Active { counter, slot },
                (None, None) if profile == NativeBindingProfile::RegisteredCube => {
                    RegisteredRecoveryState::Dormant
                }
                _ => return Err(fail("restored counter/slot allocation mismatch")),
            })
        };
        // All candidate/preparation refusals precede taking original native parts.
        // The working owner retains Game, failed preparation and unpublished
        // constructor token across every fallible step below.
        if self.ordinary.is_some() {
            game.fresh_session_candidate()?;
        }
        let prepared = match self
            .preparation
            .take()
            .expect("original native preparation")
        {
            RecoveryPreparation::Legacy(prepared) => prepared,
            RecoveryPreparation::Ordinary(prepared) => {
                let (prepared, original) = prepared.into_unpublished_recovery();
                game.retained_server = Some(
                    crate::game::retained_server::ServerGameState::from_ordinary(
                        original.server_profile(),
                        game.ordinary_startup
                            .as_ref()
                            .expect("same startup")
                            .clone(),
                    ),
                );
                self.unpublished = Some(original);
                prepared
            }
            RecoveryPreparation::Failed(_) => {
                unreachable!("failed preparation has no adopted native parts")
            }
        };
        checked(game.finish_open_world(prepared))?;
        let random = game.world.random.state();
        require(
            random == recovery_random,
            "native opening drew from world random",
        )?;
        before_items(game, &verified.items)?;
        game.install_session_checkpoint_items(&verified.items)?;
        checked(game.install_session_checkpoint_raids(verified.raids.as_ref()))?;
        require(
            game.world.random.state() == random,
            "native raid installation drew from world random",
        )?;
        // Legacy opening starts a native tree prefetch; ordinary recovery keeps
        // its actual preloaded tree. Player restoration needs that exact tree.
        let _ = game.advancement_tree();
        Ok(RecoveryData {
            request,
            verified: NativeRecoveryImage {
                manifest: verified.manifest,
                counter: verified.counter,
                players: verified.players,
                raids: verified.raids,
                items: verified.items,
            },
            selected,
            clocks,
            random: recovery_random,
            receipt: CoreRecoveryReceipt { epoch, state },
        })
    }
}

/// Same ordinary worker owns this attempt from source exclusion through either
/// verified activation or original transport/preparation closure. No error drops
/// a partial candidate, body, native writer, producer or source lease.
pub(in crate::game) struct OrdinaryRecoveryWork<'a> {
    work: SessionRecoveryWork<'a>,
    request: OrdinaryRecoveryRequest,
    lease: Option<crate::game::world_loading::host_identity::session_authority::WorldAuthority>,
    data: Option<RecoveryData>,
    players: Option<lan::OrdinaryRecoveryPlayers>,
    closed_host: Option<LanHost<horizon_client::lan::hub::ClosedHub>>,
    closed_native: Option<crate::game::world_loading::PreparationClosed>,
    failure: Option<String>,
    close_deadline: Option<Instant>,
    native_close_started: bool,
}
impl<'a> OrdinaryRecoveryWork<'a> {
    pub(in crate::game) fn prepare(
        game: Box<Game<'a>>,
        entry: crate::game::world_loading::RetainedEntryIdentity,
        request: OrdinaryRecoveryRequest,
    ) -> Self {
        let startup = game
            .ordinary_startup
            .as_ref()
            .expect("actual ordinary recovery seed")
            .clone();
        let mut work = SessionRecoveryWork::legacy(game);
        work.ordinary = Some((entry, startup));
        work.armed = true;
        let mut owner = Self {
            work,
            request,
            lease: None,
            data: None,
            players: None,
            closed_host: None,
            closed_native: None,
            failure: None,
            close_deadline: None,
            native_close_started: false,
        };
        if let Err(error) = owner.prepare_native() {
            owner.fail(error.to_string());
        }
        owner
    }
    fn prepare_native(&mut self) -> SResult<()> {
        let source = &self.request.source;
        let game = self.work.game.as_ref().unwrap();
        require(
            source.context.same_owners(game)
                && game
                    .ordinary_startup
                    .as_ref()
                    .is_some_and(|startup| Arc::ptr_eq(startup, &source.startup))
                && game.options.execution_role == source.role,
            "ordinary recovery changed its actual captured source generation/role",
        )?;
        deadline(self.request.managed.deadline)?;
        let continuation = directory(&source.continuation)?;
        require(
            fs::read_dir(&continuation).map_err(fail)?.next().is_none()
                && [
                    &source.root,
                    &source.archive,
                    &self.request.managed.destination,
                ]
                .iter()
                .all(|root| !continuation.starts_with(root) && !root.starts_with(&continuation)),
            "ordinary recovery continuation must remain fresh and separate",
        )?;
        self.lease = Some(checked(source.namespace.acquire_recovery(
            &self.request.managed.owner_root,
            &self.request.managed.archive_root,
            &self.request.managed.destination,
        ))?);
        let world = self.lease.as_ref().unwrap().world();
        require(
            world == source.world,
            "original source identity changed before managed recovery",
        )?;
        let request = RecoveryRequest {
            expected_world: world,
            expected_target: self.request.managed.expected_target.clone(),
            expected_type: self.request.managed.expected_type.clone(),
            archive_root: self.request.managed.archive_root.clone(),
            destination: self.request.managed.destination.clone(),
            deadline: self.request.managed.deadline,
        };
        self.data = Some(self.work.prepare_steps(
            NativeBindingProfile::RegisteredCube,
            source.guest,
            request,
            |_, _| Ok(()),
            #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
            None,
        )?);
        require(
            self.work.game.as_ref().unwrap().level.player_uuid == source.native_player_uuid
                && source.native_player_uuid.is_some(),
            "ordinary recovery changed or invented the original native record UUID",
        )?;
        let game = self.work.game.as_mut().unwrap();
        checked(
            game.session_lock
                .as_mut()
                .ok_or_else(|| fail("actual recovered session absent"))?
                .install_authority(&mut self.lease),
        )?;
        Ok(())
    }
    pub(in crate::game) fn game_mut(&mut self) -> &mut Game<'a> {
        self.work.game.as_mut().unwrap()
    }
    pub(in crate::game) fn original_error(&self) -> Option<&str> {
        self.failure.as_deref()
    }
    pub(in crate::game) fn fail(&mut self, reason: String) {
        self.failure.get_or_insert(reason);
    }
    pub(in crate::game) fn try_activate(&mut self) -> SResult<Option<Box<Game<'a>>>> {
        require(
            self.failure.is_none() && self.close_deadline.is_none(),
            "refused recovery cannot activate",
        )?;
        let data = self
            .data
            .as_ref()
            .ok_or_else(|| fail("actual recovered image absent"))?;
        let game = self.work.game.as_mut().unwrap();
        let expected_count = if game
            .retained_server
            .as_ref()
            .and_then(|server| server.integrated_owner())
            .is_some()
        {
            1
        } else {
            2
        };
        let count = game
            .lan
            .as_ref()
            .ok_or_else(|| fail("original recovery Hub absent"))?
            .hub
            .guests
            .len();
        if count < expected_count {
            return Ok(None);
        }
        require(count == expected_count, "extra actual recovery participant")?;
        if self.players.is_none() {
            self.players = Some(checked(lan::OrdinaryRecoveryPlayers::bind(
                game,
                &data.verified.players,
            ))?);
        }
        let players = self.players.as_mut().unwrap();
        checked(players.install(game))?;
        match &data.receipt.state {
            CoreRecoveryState::Native(RegisteredRecoveryState::Active { counter, slot }) => {
                checked(game.world.read_persistent_counter(counter))?;
                game.world
                    .read_world_slot(slot)
                    .map_err(|error| fail(format!("{error:?}")))?;
            }
            CoreRecoveryState::Native(RegisteredRecoveryState::Dormant) => {
                require(
                    checked(game.world.persistent_counter_key())?.is_none(),
                    "ordinary dormant counter allocated",
                )?;
                let slot = checked(decode_slot_frame(
                    &checked(
                        game.world
                            .slot_checkpoint_bytes(game.res.items(), game.world.game_time),
                    )?,
                    game.res.items(),
                ))?;
                require(
                    matches!(slot.view().allocation, SlotFrameAllocation::Dormant),
                    "ordinary dormant slot allocated",
                )?;
            }
            CoreRecoveryState::Guest(state) => {
                match state {
                    GuestRuntimeState::ReadyActive { counter, slot } => {
                        checked(game.world.read_persistent_counter(counter))?;
                        game.world
                            .read_world_slot(slot)
                            .map_err(|error| fail(format!("{error:?}")))?;
                    }
                    GuestRuntimeState::FaultedActive { receipt } => {
                        checked(game.world.validate_guest_storage_receipt(receipt))?
                    }
                    GuestRuntimeState::ReadyDormant => {
                        let view = checked(game.world.guest_storage_view())?;
                        require(
                            view.counter().execution.status == GuestStatus::Ready
                                && matches!(view.counter().fields.placement, Placement::Dormant),
                            "ordinary Guest dormant state changed",
                        )?;
                    }
                }
                game.prepare_guest_image_evaluator(&data.verified.counter)?;
            }
        }
        game.session_fence(data.request.deadline)?;
        game.session_context()?;
        let rows = checked(players.project(game))?;
        let local_uuid = native_local_uuid(&game.world_root)?;
        let snapshot =
            game.session_snapshot_from_players(game.effective_session_raids()?, local_uuid, rows)?;
        validate_native_local_player(
            &game.world_root,
            &snapshot.players.iter().map(descriptor).collect::<Vec<_>>(),
        )?;
        #[cfg(all(test, feature = "mods", target_os = "linux"))]
        let observation = tests::ordinary_fixture_observation(&snapshot)?;
        verify_recovery_snapshot(
            game,
            &data.request,
            &data.verified,
            &data.selected,
            data.clocks,
            data.random,
            snapshot,
        )?;
        let original = self
            .work
            .unpublished
            .take()
            .ok_or_else(|| fail("original recovered construction transfer absent"))?;
        if let Err(original) = original.publish(game) {
            self.work.unpublished = Some(original);
            return Err(fail(
                "verified recovered native construction association changed",
            ));
        }
        game.retained_server
            .as_mut()
            .unwrap()
            .set_continuation_archive(self.request.source.continuation.clone());
        #[cfg(all(test, feature = "mods", target_os = "linux"))]
        if let Some(control) = game
            .retained_server
            .as_ref()
            .unwrap()
            .test
            .ordinary
            .as_ref()
        {
            let control: &crate::game::retained_server::OrdinaryControl = control;
            control.record_recovered(observation);
        }
        self.work.armed = false;
        Ok(self.work.game.take())
    }
    pub(in crate::game) fn begin_close(&mut self, until: Instant) {
        self.close_deadline.get_or_insert(until);
        if let Some(game) = self.work.game.as_mut() {
            if let Some(server) = game.retained_server.as_mut() {
                server.freeze();
            }
            if let Some(host) = game.lan.as_mut() {
                host.hub
                    .begin_retained_close(until.saturating_duration_since(Instant::now()));
            }
        }
    }
    pub(in crate::game) fn poll_close(
        &mut self,
    ) -> crate::game::world_loading::PreparationCloseStatus {
        use crate::game::world_loading::PreparationCloseStatus as Status;
        use horizon_client::lan::hub::RetainedHubPhase;
        let Some(until) = self.close_deadline else {
            return Status::Pending;
        };
        let game = self.work.game.as_mut().unwrap();
        if let Some(host) = game.lan.as_mut() {
            let Some(view) = host.hub.poll_retained_close() else {
                return Status::Blocked("original recovery transport owner missing".into());
            };
            if view.phase == RetainedHubPhase::Blocked {
                return Status::Blocked("original recovery transport close blocked".into());
            }
            if view.phase != RetainedHubPhase::Ready {
                return Status::Pending;
            }
            let host = game.lan.take().unwrap();
            match host.try_retained_closed() {
                Ok(closed) => self.closed_host = Some(closed),
                Err(host) => {
                    game.lan = Some(host);
                    return Status::Blocked("original recovery Hub consuming close refused".into());
                }
            }
        }
        if let Some(original) = self.work.unpublished.take() {
            match original.refuse_game(
                game,
                self.failure
                    .clone()
                    .unwrap_or_else(|| "ordinary recovery cancelled before activation".into()),
            ) {
                Ok(preparation) => {
                    self.work.preparation = Some(RecoveryPreparation::Failed(preparation))
                }
                Err(original) => {
                    self.work.unpublished = Some(original);
                    return Status::Blocked("original unpublished native parts cannot yet return to preparation custody".into());
                }
            }
        }
        if let Some(RecoveryPreparation::Ordinary(_)) = self.work.preparation.as_ref() {
            let Some(RecoveryPreparation::Ordinary(prepared)) = self.work.preparation.take() else {
                unreachable!()
            };
            self.work.preparation = Some(RecoveryPreparation::Failed(
                prepared.refuse_unpublished(
                    self.failure
                        .clone()
                        .unwrap_or_else(|| "ordinary recovery preparation refused".into()),
                ),
            ));
        }
        if let Some(preparation) = self.work.preparation.as_mut() {
            let RecoveryPreparation::Failed(preparation) = preparation else {
                return Status::Blocked("ordinary recovery acquired a legacy owner".into());
            };
            if !self.native_close_started {
                preparation.begin_close(until);
                self.native_close_started = true;
            }
            let status = preparation.poll_close_with_context(game);
            if status != Status::Complete {
                return status;
            }
            let Some(RecoveryPreparation::Failed(preparation)) = self.work.preparation.take()
            else {
                unreachable!()
            };
            match preparation.finish_close() {
                Ok(closed) => self.closed_native = Some(closed),
                Err(preparation) => {
                    self.work.preparation = Some(RecoveryPreparation::Failed(preparation));
                    return Status::Blocked("original recovery preparation finish refused".into());
                }
            }
        }
        if self.closed_native.is_none()
            && (game.save.is_some()
                || !game.streaming.is_vacant()
                || game.session_lock.is_some()
                || game.adv.pending_tree.is_some())
        {
            return Status::Blocked(
                "unpublished candidate still owns unresolved native work".into(),
            );
        }
        self.lease = None;
        self.work.armed = false;
        Status::Complete
    }
    pub(in crate::game) fn retry_close(&mut self, until: Instant) -> SResult<()> {
        require(
            self.close_deadline.is_some() && until > Instant::now(),
            "original recovery close Retry requires a future deadline",
        )?;
        let game = self.work.game.as_mut().unwrap();
        if let Some(host) = game.lan.as_mut()
            && let Some(view) = host.hub.retained_close_view()
            && view.phase == horizon_client::lan::hub::RetainedHubPhase::Blocked
        {
            host.hub.retry_retained_close(
                view.attempt,
                until.saturating_duration_since(Instant::now()),
            );
        }
        if let Some(RecoveryPreparation::Failed(preparation)) = self.work.preparation.as_mut() {
            checked(preparation.retry_close(until))?;
        }
        self.close_deadline = Some(until);
        Ok(())
    }
}

/// Non-clone ownership of a verified native recovery, not a ready Game.
struct NativeRecoveryImage {
    manifest: Manifest,
    counter: SessionCounter,
    players: CheckedSessionPlayers,
    raids: Option<Tag>,
    items: CheckedSessionItems,
}
/// Shared concrete ownership only; legacy adapters keep their existing fields.
struct PreparedSessionRecovery<'a> {
    game: Box<Game<'a>>,
    request: RecoveryRequest,
    verified: NativeRecoveryImage,
    selected: PathBuf,
    clocks: (i64, i64),
    random: u64,
    receipt: CoreRecoveryReceipt,
}
pub(in crate::game) struct RegisteredNativeRecoveryParts<'a> {
    core: PreparedSessionRecovery<'a>,
}
impl<'a> RegisteredNativeRecoveryParts<'a> {
    pub(in crate::game) fn runtime_state(&self) -> RegisteredRecoveryState {
        self.core.receipt.native_state().clone()
    }
    pub(in crate::game) fn validate_runtime_keys(&self) -> SResult<()> {
        match self.core.receipt.native_state() {
            RegisteredRecoveryState::Active { counter, slot } => {
                checked(self.core.game.world.read_persistent_counter(counter))?;
                self.core
                    .game
                    .world
                    .read_world_slot(slot)
                    .map_err(|e| fail(format!("{e:?}")))?;
            }
            RegisteredRecoveryState::Dormant => {
                require(
                    checked(self.core.game.world.persistent_counter_key())?.is_none(),
                    "dormant counter allocated",
                )?;
                let slot = checked(decode_slot_frame(
                    &checked(self.core.game.world.slot_checkpoint_bytes(
                        self.core.game.res.items(),
                        self.core.game.world.game_time,
                    ))?,
                    self.core.game.res.items(),
                ))?;
                require(
                    matches!(slot.view().allocation, SlotFrameAllocation::Dormant),
                    "dormant slot allocated",
                )?;
            }
        }
        Ok(())
    }
    pub(in crate::game) fn game_mut(&mut self) -> &mut Game<'a> {
        &mut self.core.game
    }
    pub(in crate::game) fn players(&self) -> &CheckedSessionPlayers {
        &self.core.verified.players
    }
    pub(in crate::game) fn game_and_players(&mut self) -> (&mut Game<'a>, &CheckedSessionPlayers) {
        (&mut self.core.game, &self.core.verified.players)
    }
    pub(in crate::game) fn install_authority(
        &mut self,
        lease: &mut Option<
            crate::game::world_loading::host_identity::session_authority::WorldAuthority,
        >,
    ) -> SResult<()> {
        checked(
            self.core
                .game
                .session_lock
                .as_mut()
                .ok_or_else(|| fail("missing recovered native session"))?
                .install_authority(lease),
        )
    }
    pub(in crate::game) fn verify_final(&mut self) -> SResult<()> {
        self.validate_runtime_keys()?;
        let core = &mut self.core;
        verify_recovery_final(
            &mut core.game,
            &core.request,
            &core.verified,
            &core.selected,
            core.clocks,
            core.random,
        )
    }
    pub(in crate::game) fn finish(mut self) -> SResult<(Box<Game<'a>>, RegisteredRecoveryReceipt)> {
        self.verify_final()?;
        let CoreRecoveryState::Native(state) = self.core.receipt.state else {
            return Err(fail("Native recovery state mismatch"));
        };
        Ok((
            self.core.game,
            RegisteredRecoveryReceipt {
                epoch: self.core.receipt.epoch,
                state,
            },
        ))
    }
}
pub(in crate::game) struct NativeRecoveryParts<'a> {
    game: Box<Game<'a>>,
    request: RecoveryRequest,
    verified: NativeRecoveryImage,
    selected: PathBuf,
    clocks: (i64, i64),
    random: u64,
    receipt: RecoveryReceipt,
}
impl<'a> NativeRecoveryParts<'a> {
    pub(in crate::game) fn validate_runtime_keys(&self) -> SResult<()> {
        self.game
            .world
            .read_persistent_counter(&self.receipt.counter)
            .map_err(|error| fail(format!("{error:?}")))?;
        self.game
            .world
            .read_world_slot(&self.receipt.slot)
            .map_err(|error| fail(format!("{error:?}")))?;
        Ok(())
    }
    pub(in crate::game) fn runtime_keys(&self) -> (RuntimeCounterKey, SlotKey) {
        (self.receipt.counter.clone(), self.receipt.slot.clone())
    }
    pub(in crate::game) fn game_mut(&mut self) -> &mut Game<'a> {
        &mut self.game
    }
    pub(in crate::game) fn players(&self) -> &CheckedSessionPlayers {
        &self.verified.players
    }
    pub(in crate::game) fn game_and_players(&mut self) -> (&mut Game<'a>, &CheckedSessionPlayers) {
        (&mut self.game, &self.verified.players)
    }
    pub(in crate::game) fn install_authority(
        &mut self,
        lease: &mut Option<
            crate::game::world_loading::host_identity::session_authority::WorldAuthority,
        >,
    ) -> SResult<()> {
        let session = self
            .game
            .session_lock
            .as_mut()
            .ok_or_else(|| fail("missing recovered native session"))?;
        checked(session.install_authority(lease))
    }
    pub(in crate::game) fn finish(mut self) -> SResult<(Box<Game<'a>>, RecoveryReceipt)> {
        self.verify_final()?;
        Ok((self.game, self.receipt))
    }
    pub(in crate::game) fn verify_final(&mut self) -> SResult<()> {
        let Self {
            game,
            request,
            verified,
            selected,
            clocks,
            random,
            ..
        } = self;
        verify_recovery_final(game, request, verified, selected, *clocks, *random)
    }
}
fn verify_recovery_final(
    game: &mut Game<'_>,
    request: &RecoveryRequest,
    verified: &NativeRecoveryImage,
    selected: &Path,
    clocks: (i64, i64),
    random: u64,
) -> SResult<()> {
    game.session_fence(request.deadline)?;
    let snapshot = game.session_snapshot()?;
    verify_recovery_snapshot(game, request, verified, selected, clocks, random, snapshot)
}
fn verify_recovery_snapshot(
    game: &Game<'_>,
    request: &RecoveryRequest,
    verified: &NativeRecoveryImage,
    selected: &Path,
    clocks: (i64, i64),
    random: u64,
    snapshot: Snapshot,
) -> SResult<()> {
    require(
        (game.world.game_time, game.world.day_time) == clocks,
        "recovery advanced native clocks",
    )?;
    require(
        game.world.random.state() == random,
        "recovery drew from world random",
    )?;
    require(
        snapshot.counter == verified.counter
            && same_players(&snapshot.players, verified.players.rows())
            && snapshot.raids == verified.raids
            && snapshot.items == verified.items.projection
            && raid_metadata(&game.world_root)? == verified.raids,
        "final restored counter/player mismatch",
    )?;
    let recovered_slot = checked(
        snapshot
            .counter
            .decode_slot(&snapshot.slot, game.res.items()),
    )?;
    correlate(&verified.manifest, &snapshot.counter, &recovered_slot)?;
    // The slot bytes are deterministic current-schema output; compare the
    // original member rather than permitting a new revision/default value.
    require(
        snapshot.slot == read(&selected.join(SLOT), STRUCTURED_LIMIT)?,
        "final restored slot mismatch",
    )?;
    require(
        game.session_context()? == request.expected_world,
        "final native origin/identity",
    )?;
    checked(game.inspect_session_checkpoint_owners())?;
    deadline(request.deadline)?;
    Ok(())
}

#[cfg(all(test, feature = "paired-packages", target_os = "linux"))]
#[path = "session_checkpoint_tests.rs"]
pub(in crate::game) mod tests;

fn saved_data(tag: &Tag) -> SResult<&Tag> {
    let fields = compound(tag, &["DataVersion", "data"])?;
    require(
        fields.len() == 2 && tag.get("DataVersion") == Some(&Tag::Int(DATA_VERSION)),
        "native saved data root/version",
    )?;
    let data = tag
        .get("data")
        .ok_or_else(|| fail("native saved data absent"))?;
    require(
        matches!(data, Tag::Compound(_)),
        "native saved data compound",
    )?;
    Ok(data)
}
fn standard_sidecar(name: &str, tag: &Tag) -> SResult<()> {
    let data = saved_data(tag)?;
    match name {
        "data/minecraft/weather.dat" => {
            let fields = compound(
                data,
                &[
                    "clear_weather_time",
                    "rain_time",
                    "thunder_time",
                    "raining",
                    "thundering",
                ],
            )?;
            require(fields.len() == 5, "weather fields")?;
            for key in ["clear_weather_time", "rain_time", "thunder_time"] {
                require(
                    matches!(data.get(key),Some(Tag::Int(v)) if *v>=0),
                    "weather duration type/range",
                )?;
            }
            for key in ["raining", "thundering"] {
                require(
                    matches!(data.get(key), Some(Tag::Byte(0 | 1))),
                    "weather flag type",
                )?;
            }
        }
        "data/minecraft/world_clocks.dat" => {
            let fields = compound(data, &["minecraft:overworld", "minecraft:the_end"])?;
            require(fields.len() == 2, "native clock dimensions")?;
            for dimension in ["minecraft:overworld", "minecraft:the_end"] {
                let clock = data
                    .get(dimension)
                    .ok_or_else(|| fail("native clock missing"))?;
                compound(clock, &["total_ticks", "partial_tick", "rate", "paused"])?;
                require(
                    matches!(clock.get("total_ticks"), Some(Tag::Long(_))),
                    "native total clock type",
                )?;
                if let Some(value) = clock.get("partial_tick") {
                    require(
                        matches!(value,Tag::Float(v) if (0.0..1.0).contains(&f32::from_bits(*v))),
                        "native partial clock",
                    )?;
                }
                if let Some(value) = clock.get("rate") {
                    require(
                        matches!(value,Tag::Float(v) if f32::from_bits(*v)>=0.0),
                        "native clock rate",
                    )?;
                }
                if let Some(value) = clock.get("paused") {
                    require(matches!(value, Tag::Byte(0 | 1)), "native clock pause flag")?;
                }
            }
        }
        "data/minecraft/world_gen_settings.dat" => {
            let fields = compound(
                data,
                &["seed", "generate_structures", "bonus_chest", "dimensions"],
            )?;
            require(fields.len() == 4, "native generation keys")?;
            require(
                matches!(data.get("seed"), Some(Tag::Long(_))),
                "native seed type",
            )?;
            for key in ["generate_structures", "bonus_chest"] {
                require(
                    matches!(data.get(key), Some(Tag::Byte(0 | 1))),
                    "native generator flag",
                )?;
            }
            let dimensions = data
                .get("dimensions")
                .ok_or_else(|| fail("generation dimensions"))?;
            let fields = compound(
                dimensions,
                &[
                    "minecraft:overworld",
                    "minecraft:the_nether",
                    "minecraft:the_end",
                ],
            )?;
            require(fields.len() == 3, "generation dimension keys")?;
            for (dimension, settings) in [
                ("minecraft:overworld", "minecraft:overworld"),
                ("minecraft:the_nether", "minecraft:nether"),
                ("minecraft:the_end", "minecraft:end"),
            ] {
                let definition = dimensions
                    .get(dimension)
                    .ok_or_else(|| fail("native dimension missing"))?;
                require(
                    compound(definition, &["type", "generator"])?.len() == 2
                        && text(definition.get("type"))? == dimension,
                    "native dimension definition",
                )?;
                let generator = definition
                    .get("generator")
                    .ok_or_else(|| fail("generator absent"))?;
                require(
                    compound(generator, &["type", "settings", "biome_source"])?.len() == 3
                        && text(generator.get("type"))? == "minecraft:noise"
                        && text(generator.get("settings"))? == settings,
                    "unsupported generator settings",
                )?;
                let biome = generator
                    .get("biome_source")
                    .ok_or_else(|| fail("biome source missing"))?;
                if dimension == "minecraft:the_end" {
                    require(
                        compound(biome, &["type"])?.len() == 1
                            && text(biome.get("type"))? == "minecraft:the_end",
                        "native End biome source",
                    )?;
                } else {
                    require(
                        compound(biome, &["type", "preset"])?.len() == 2
                            && text(biome.get("type"))? == "minecraft:multi_noise"
                            && text(biome.get("preset"))? == settings,
                        "native biome source",
                    )?;
                }
            }
        }
        "data/minecraft/game_rules.dat" => {
            // Native gamerules are typed scalar metadata; names are resolved
            // against the same current native rule registry used by Level.
            let Tag::Compound(fields) = data else {
                return Err(fail("native rule compound"));
            };

            require(fields.len() == 58, "native rule count")?;
            for (key, value) in fields {
                let name = checked(key.to_string())?;
                let name = name
                    .strip_prefix("minecraft:")
                    .ok_or_else(|| fail("native rule namespace"))?;
                let rule = horizon_client::gamerules::rule(name)
                    .ok_or_else(|| fail("unknown native rule"))?;
                require(
                    match (rule.default, value) {
                        (horizon_client::gamerules::Value::Bool(_), Tag::Byte(0 | 1)) => true,
                        (horizon_client::gamerules::Value::Int(_), Tag::Int(v)) => {
                            *v >= rule.min && *v <= rule.max
                        }
                        _ => false,
                    },
                    "native rule type/range",
                )?;
            }
        }
        _ => return Err(fail("unsupported standard sidecar")),
    }
    Ok(())
}

fn native_local_uuid(root: &Path) -> SResult<[i32; 4]> {
    let level = compressed_nbt(&read(&root.join("level.dat"), STRUCTURED_LIMIT)?)?;
    let Some(Tag::IntArray(uuid)) = level.get("Data").and_then(|v| v.get("singleplayer_uuid"))
    else {
        return Err(fail("typed native singleplayer UUID absent"));
    };
    checked(uuid.as_slice().try_into())
}

fn validate_player_items(game: &Game<'_>, tag: &Tag) -> SResult<()> {
    validate_player_items_context(&SessionValidationContext::from_game(game), tag)
}
fn validate_player_items_context(context: &SessionValidationContext, tag: &Tag) -> SResult<()> {
    let mut work = vec![tag];
    let mut nodes = 0;
    while let Some(value) = work.pop() {
        nodes += 1;
        require(nodes <= NODES, "player item graph bound")?;
        match value {
            Tag::Compound(fields) => {
                if let Some(id) = value.get("id") {
                    let id = text(Some(id))?;
                    require(
                        context.res.items().get(&id).is_some() && id != "minecraft:air",
                        "unresolved player item ID",
                    )?;
                    require(
                        // Nested native ItemStackTemplate omits count==1. The
                        // enclosing native item/player roundtrip still requires
                        // exactly the canonical spelling at every location.
                        value.get("count").is_none()
                            || matches!(value.get("count"),Some(Tag::Int(v)) if *v>0),
                        "native player item count type/range",
                    )?;
                }
                for (name, value) in fields {
                    require(
                        !matches!(
                            name.to_string().as_deref(),
                            Ok("minecraft:attribute_modifiers"
                                | "!minecraft:attribute_modifiers"
                                | "attribute_modifiers")
                        ),
                        "unsupported item attribute profile",
                    )?;
                    if name == &nbt::JavaString::from("minecraft:container") {
                        let mut slots = BTreeSet::new();
                        for entry in list(Some(value))? {
                            require(
                                compound(entry, &["slot", "item"])?.len() == 2,
                                "native item container fields",
                            )?;
                            let Some(Tag::Int(slot)) = entry.get("slot") else {
                                return Err(fail("native item container slot type"));
                            };
                            require(
                                (0..=255).contains(slot) && slots.insert(*slot),
                                "native item container slot range/duplicate",
                            )?;
                        }
                    }
                    work.push(value);
                }
            }
            Tag::List(values) => work.extend(values),
            _ => {}
        }
    }
    Ok(())
}
fn validate_player_json(game: &Game<'_>, advancements: &Value, stats: &Value) -> SResult<()> {
    validate_player_json_context(
        &SessionValidationContext::from_game(game),
        advancements,
        stats,
    )
}
fn validate_player_json_context(
    context: &SessionValidationContext,
    advancements: &Value,
    stats: &Value,
) -> SResult<()> {
    let progress = horizon_client::advancements::Progress::from_json(advancements);
    require(
        progress.to_json(&context.advancement_tree, DATA_VERSION) == *advancements,
        "coercive native player advancements",
    )?;
    let values = stats
        .as_object()
        .ok_or_else(|| fail("statistics root type"))?;
    require(
        values.len() == 2
            && values.get("DataVersion").and_then(Value::as_i64) == Some(i64::from(DATA_VERSION)),
        "statistics version/fields",
    )?;
    let values = values
        .get("stats")
        .and_then(Value::as_object)
        .ok_or_else(|| fail("statistics values object"))?;
    let mut native = crate::game::stats::Stats::default();
    for (kind, entries) in values {
        require(kind.contains(':') && !kind.is_empty(), "statistics type ID")?;
        let entries = entries
            .as_object()
            .ok_or_else(|| fail("statistics entries object"))?;
        let mut target = BTreeMap::new();
        for (key, value) in entries {
            let value = value
                .as_i64()
                .and_then(|v| i32::try_from(v).ok())
                .ok_or_else(|| fail("statistics integer range/type"))?;
            require(key.contains(':'), "statistics key ID")?;
            target.insert(key.clone(), value);
        }
        native.values.insert(kind.clone(), target);
    }
    require(
        native.to_json() == *stats,
        "coercive native player statistics",
    )
}

fn validate_level(tag: &Tag) -> SResult<()> {
    require(
        compound(tag, &["Data"])?.len() == 1,
        "native level root fields",
    )?;
    let data = tag.get("Data").ok_or_else(|| fail("native Data absent"))?;
    let fields = compound(
        data,
        &[
            "DataVersion",
            "Version",
            "version_history",
            "version",
            "LevelName",
            "GameType",
            "Time",
            "LastPlayed",
            "spawn",
            "difficulty_settings",
            "initialized",
            "singleplayer_uuid",
            "ServerBrands",
            "allowCommands",
            "WasModded",
            "DataPacks",
        ],
    )?;
    require(
        fields.len() == 16
            && data.get("DataVersion") == Some(&Tag::Int(DATA_VERSION))
            && data.get("version") == Some(&Tag::Int(19133))
            && data.get("initialized") == Some(&Tag::Byte(1)),
        "native level generated profile",
    )?;
    let _ = text(data.get("LevelName"))?;
    require(
        matches!(data.get("GameType"), Some(Tag::Int(0..=3)))
            && matches!(data.get("Time"),Some(Tag::Long(v)) if *v>=0)
            && matches!(data.get("LastPlayed"),Some(Tag::Long(v)) if *v>=0)
            && matches!(data.get("singleplayer_uuid"),Some(Tag::IntArray(v)) if v.len()==4),
        "native level scalar types/ranges",
    )?;
    for key in ["allowCommands", "WasModded"] {
        require(
            matches!(data.get(key), Some(Tag::Byte(0 | 1))),
            "native level boolean",
        )?;
    }
    let history = list(data.get("version_history"))?;
    require(
        !history.is_empty() && history.iter().all(|v| *v == Tag::Int(DATA_VERSION)),
        "native same-version history",
    )?;
    let brands = list(data.get("ServerBrands"))?;
    require(!brands.is_empty(), "native brand history absent")?;
    for brand in brands {
        let _ = text(Some(brand))?;
    }
    let version = data
        .get("Version")
        .ok_or_else(|| fail("native Version absent"))?;
    require(
        compound(version, &["Name", "Id", "Snapshot", "Series"])?.len() == 4
            && version.get("Id") == Some(&Tag::Int(DATA_VERSION))
            && version.get("Snapshot") == Some(&Tag::Byte(0))
            && text(version.get("Series"))? == "main",
        "native version metadata",
    )?;
    let _ = text(version.get("Name"))?;
    let spawn = data
        .get("spawn")
        .ok_or_else(|| fail("native spawn absent"))?;
    require(
        compound(spawn, &["pos", "dimension", "yaw", "pitch"])?.len() == 4
            && matches!(spawn.get("pos"),Some(Tag::IntArray(v)) if v.len()==3)
            && text(spawn.get("dimension"))? == Dim::Overworld.id()
            && matches!(spawn.get("yaw"), Some(Tag::Float(_)))
            && matches!(spawn.get("pitch"), Some(Tag::Float(_))),
        "native spawn metadata",
    )?;
    let difficulty = data
        .get("difficulty_settings")
        .ok_or_else(|| fail("native difficulty absent"))?;
    require(
        compound(difficulty, &["difficulty", "hardcore", "locked"])?.len() == 3
            && matches!(
                text(difficulty.get("difficulty"))?.as_str(),
                "peaceful" | "easy" | "normal" | "hard"
            ),
        "native difficulty metadata",
    )?;
    for key in ["hardcore", "locked"] {
        require(
            matches!(difficulty.get(key), Some(Tag::Byte(0 | 1))),
            "native difficulty boolean",
        )?;
    }
    let packs = data
        .get("DataPacks")
        .ok_or_else(|| fail("native datapacks absent"))?;
    require(
        compound(packs, &["Enabled", "Disabled"])?.len() == 2
            && list(packs.get("Enabled"))? == [Tag::String("vanilla".into())],
        "unsupported enabled native datapacks",
    )?;
    for value in list(packs.get("Disabled"))? {
        require(
            matches!(
                text(Some(value))?.as_str(),
                "minecart_improvements" | "redstone_experiments" | "trade_rebalance"
            ),
            "unknown native disabled datapack",
        )?;
    }
    Ok(())
}

fn validate_native_local_player(root: &Path, players: &[PlayerDescriptor]) -> SResult<[i32; 4]> {
    let uuid = native_local_uuid(root)?;
    let local = compressed_nbt(&read(
        &root.join(format!("players/data/{}.dat", uuid_text(uuid))),
        STRUCTURED_LIMIT,
    )?)?;
    require(
        local.get("UUID") == Some(&Tag::IntArray(uuid.to_vec())),
        "native local record UUID mismatch",
    )?;
    let owners = players
        .iter()
        .filter(|row| row.integrated_owner)
        .collect::<Vec<_>>();
    require(owners.len() <= 1, "duplicate integrated local owner")?;
    require(
        players
            .iter()
            .filter(|row| !row.integrated_owner)
            .all(|row| row.record_uuid != uuid),
        "dedicated/external participant collides with native local record",
    )?;
    if let Some(owner) = owners.first() {
        require(
            owner.record_uuid == uuid,
            "integrated owner does not match native local UUID",
        )?;
    } else {
        require(
            lan::session_empty_native_player(&local),
            "dedicated native local record is not canonical inert metadata",
        )?;
    }
    Ok(uuid)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SessionCounter {
    Native(CounterFrameV2),
    Guest(GuestCounterFrameV3),
}
impl std::ops::Deref for SessionCounter {
    type Target = CounterFrameV2;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Native(frame) => frame,
            Self::Guest(frame) => &frame.fields,
        }
    }
}
impl SessionCounter {
    fn is_guest(&self) -> bool {
        matches!(self, Self::Guest(_))
    }
    fn image_kind(&self) -> &'static str {
        if self.is_guest() { GUEST_KIND } else { KIND }
    }
    fn binding_profile(
        &self,
    ) -> std::result::Result<
        NativeBindingProfile,
        horizon_client::world::counter_restore::RestoreError,
    > {
        match self {
            Self::Native(frame) => frame.binding_profile(),
            Self::Guest(frame) => {
                frame.validate()?;
                Ok(NativeBindingProfile::RegisteredCube)
            }
        }
    }
    fn validate(
        &self,
    ) -> std::result::Result<(), horizon_client::world::counter_restore::RestoreError> {
        match self {
            Self::Native(frame) => frame.validate(),
            Self::Guest(frame) => frame.validate(),
        }
    }
    fn encode(
        &self,
    ) -> std::result::Result<Vec<u8>, horizon_client::world::counter_restore::RestoreError> {
        match self {
            Self::Native(frame) => frame.encode(),
            Self::Guest(frame) => frame.encode(),
        }
    }
    fn decode(
        bytes: &[u8],
        guest: bool,
    ) -> std::result::Result<Self, horizon_client::world::counter_restore::RestoreError> {
        if guest {
            GuestCounterFrameV3::decode(bytes).map(Self::Guest)
        } else {
            CounterFrameV2::decode(bytes).map(Self::Native)
        }
    }
    fn from_world(
        world: &horizon_client::world::World,
    ) -> std::result::Result<Self, horizon_client::world::counter_restore::RestoreError> {
        if world.has_persistent_guest_counter() {
            Ok(Self::Guest(world.guest_storage_view()?.counter().clone()))
        } else {
            world
                .counter_checkpoint_frame(world.game_time)
                .map(Self::Native)
        }
    }
    fn slot_bytes(
        &self,
        world: &horizon_client::world::World,
        items: &horizon_client::items::Items,
    ) -> std::result::Result<Vec<u8>, horizon_client::world::slot_restore::SlotRestoreError> {
        if self.is_guest() {
            world.guest_slot_checkpoint_bytes(items, world.game_time)
        } else {
            world.slot_checkpoint_bytes(items, world.game_time)
        }
    }
    fn decode_slot(
        &self,
        bytes: &[u8],
        items: &horizon_client::items::Items,
    ) -> std::result::Result<DecodedSlotFrame, horizon_client::world::slot_restore::SlotRestoreError>
    {
        if self.is_guest() {
            decode_guest_slot_frame(bytes, items)
        } else {
            decode_slot_frame(bytes, items)
        }
    }
}
enum CoreRecoveryState {
    Native(RegisteredRecoveryState),
    Guest(GuestRuntimeState),
}
struct CoreRecoveryReceipt {
    epoch: u64,
    state: CoreRecoveryState,
}
impl CoreRecoveryReceipt {
    fn native_state(&self) -> &RegisteredRecoveryState {
        match &self.state {
            CoreRecoveryState::Native(state) => state,
            CoreRecoveryState::Guest(_) => {
                unreachable!("Native wrapper constructor rejects Guest execution")
            }
        }
    }
}
pub(in crate::game) struct GuestRecoveryReceipt {
    pub epoch: u64,
    pub state: GuestRuntimeState,
}
pub(in crate::game) struct GuestNativeRecoveryParts<'a> {
    core: PreparedSessionRecovery<'a>,
}
impl<'a> GuestNativeRecoveryParts<'a> {
    pub(in crate::game) fn game_mut(&mut self) -> &mut Game<'a> {
        &mut self.core.game
    }
    pub(in crate::game) fn players(&self) -> &CheckedSessionPlayers {
        &self.core.verified.players
    }
    pub(in crate::game) fn validate_storage_state(&self) -> SResult<()> {
        let CoreRecoveryState::Guest(state) = &self.core.receipt.state else {
            return Err(fail("Guest recovery state absent"));
        };
        match state {
            GuestRuntimeState::ReadyActive { counter, slot } => {
                checked(self.core.game.world.read_persistent_counter(counter))?;
                self.core
                    .game
                    .world
                    .read_world_slot(slot)
                    .map_err(|e| fail(format!("{e:?}")))?;
            }
            GuestRuntimeState::FaultedActive { receipt } => {
                checked(self.core.game.world.validate_guest_storage_receipt(receipt))?;
            }
            GuestRuntimeState::ReadyDormant => {
                let view = checked(self.core.game.world.guest_storage_view())?;
                require(
                    view.counter().execution.status == GuestStatus::Ready
                        && matches!(view.counter().fields.placement, Placement::Dormant),
                    "Guest dormant state changed",
                )?;
            }
        }
        Ok(())
    }
    pub(in crate::game) fn finish(mut self) -> SResult<(Box<Game<'a>>, GuestRecoveryReceipt)> {
        self.validate_storage_state()?;
        let counter = self.core.verified.counter.clone();
        self.core.game.prepare_guest_image_evaluator(&counter)?;
        verify_recovery_final(
            &mut self.core.game,
            &self.core.request,
            &self.core.verified,
            &self.core.selected,
            self.core.clocks,
            self.core.random,
        )?;
        let CoreRecoveryState::Guest(state) = self.core.receipt.state else {
            return Err(fail("Guest recovery state mismatch"));
        };
        Ok((
            self.core.game,
            GuestRecoveryReceipt {
                epoch: self.core.receipt.epoch,
                state,
            },
        ))
    }
}
impl<'a> Game<'a> {
    fn prepare_guest_image_evaluator(&mut self, counter: &SessionCounter) -> SResult<()> {
        #[cfg(not(feature = "mods"))]
        {
            let _ = counter;
            Err(fail("Guest recovery requires mod runtime"))
        }
        #[cfg(feature = "mods")]
        {
            let SessionCounter::Guest(frame) = counter else {
                return Err(fail("Guest image required"));
            };
            let catalog = self
                .options
                .mod_packages
                .catalog()
                .ok_or_else(|| fail("Guest captured catalog absent"))?;
            require(
                catalog.prepared().len() == 1,
                "Guest recovery requires one captured pair",
            )?;
            let pair = catalog
                .prepared()
                .values()
                .next()
                .ok_or_else(|| fail("Guest pair absent"))?
                .clone();
            let _ = binding(self, counter)?;
            require(
                frame.execution.primary_owner == pair.bundle().content_owner().id,
                "Guest recovery owner changed",
            )?;
            checked(
                self.mods
                    .mods
                    .prepare_counter_evaluator(&frame.execution.primary_owner, pair.identity()),
            )?;
            checked(
                self.mods
                    .mods
                    .counter_evaluator(&frame.execution.primary_owner, pair.identity()),
            )?;
            Ok(())
        }
    }
    pub(in crate::game) fn checkpoint_registered_guest_session(
        &mut self,
        archive: &Path,
        epoch: u64,
        until: Instant,
    ) -> SResult<CheckpointReceipt> {
        self.checkpoint_session_profile_steps(
            NativeBindingProfile::RegisteredCube,
            true,
            archive,
            epoch,
            until,
            |_| Ok(()),
        )
    }
    pub(in crate::game) fn prepare_registered_guest_session_checkpoint(
        self: Box<Self>,
        request: RecoveryRequest,
    ) -> SResult<GuestNativeRecoveryParts<'a>> {
        Ok(GuestNativeRecoveryParts {
            core: self.prepare_session_checkpoint_profile_steps(
                NativeBindingProfile::RegisteredCube,
                true,
                request,
                |_, _| Ok(()),
                #[cfg(all(test, feature = "native-writer-test-control", debug_assertions))]
                None,
            )?,
        })
    }
    pub(in crate::game) fn recover_registered_guest_session_checkpoint(
        self,
        request: RecoveryRequest,
        host: LanHost,
    ) -> SResult<(Self, GuestRecoveryReceipt)> {
        let mut parts = Box::new(self).prepare_registered_guest_session_checkpoint(request)?;
        checked(
            parts
                .core
                .game
                .install_session_checkpoint_participants(host, &parts.core.verified.players),
        )?;
        let (game, receipt) = parts.finish()?;
        Ok((*game, receipt))
    }
}
