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
        CounterFrameV2, CounterTarget, NativeBinding, NativeRetirementReason, Placement,
        RuntimeCounterKey, WorldId,
    },
    world::slot_restore::{DecodedSlotFrame, SlotFrameAllocation, decode_slot_frame},
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

type SResult<T> = std::result::Result<T, SessionCheckpointError>;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::game) enum Publication {
    NotPublished,
    Uncertain,
}
#[derive(Debug)]
pub(in crate::game) struct SessionCheckpointError {
    pub publication: Publication,
    pub detail: String,
}
impl fmt::Display for SessionCheckpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "session checkpoint {:?}: {}",
            self.publication, self.detail
        )
    }
}
impl std::error::Error for SessionCheckpointError {}
fn fail(detail: impl fmt::Display) -> SessionCheckpointError {
    SessionCheckpointError {
        publication: Publication::NotPublished,
        detail: detail.to_string(),
    }
}
fn checked<T>(value: std::result::Result<T, impl fmt::Display>) -> SResult<T> {
    value.map_err(fail)
}
fn require(ok: bool, detail: &str) -> SResult<()> {
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
/// Trusted coordinator routing, not a decoded image or an expected-UUID assertion.
pub(in crate::game) struct ManagedRecoveryRequest {
    pub owner_root: PathBuf,
    pub expected_target: CounterTarget,
    pub expected_type: String,
    pub archive_root: PathBuf,
    pub destination: PathBuf,
    pub deadline: Instant,
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
        require(len <= 4096, "NBT string bound")?;
        let value = self.take(len)?;
        let mut encoded = vec![8, 0, 0];
        encoded.extend_from_slice(&(len as u16).to_be_bytes());
        encoded.extend_from_slice(value);
        let (named, used) = checked(nbt::read_named(&encoded, limits()))?;
        require(used == encoded.len(), "NBT string trailing bytes")?;
        let Tag::String(value) = named.tag else {
            return Err(fail("NBT string"));
        };
        require(
            char::decode_utf16(value.0.iter().copied()).all(|v| v.is_ok()),
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
                require(f32::from_bits(bits).is_finite(), "nonfinite NBT float")?;
            }
            6 => {
                let bits = u64::from_be_bytes(self.take(8)?.try_into().unwrap());
                require(f64::from_bits(bits).is_finite(), "nonfinite NBT double")?;
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
                    require(names.insert(key), "duplicate NBT key")?;
                    self.payload(kind, depth + 1)?;
                }
            }
            _ => return Err(fail("invalid NBT tag type")),
        }
        Ok(())
    }
}
fn raw_nbt(raw: &[u8]) -> SResult<NamedTag> {
    require(
        !raw.is_empty() && raw.len() <= STRUCTURED_LIMIT,
        "NBT byte bound",
    )?;
    let mut scanner = NbtScan {
        bytes: raw,
        at: 0,
        nodes: 0,
    };
    let kind = scanner.byte()?;
    require(kind == 10, "NBT compound root required")?;
    scanner.string()?;
    scanner.payload(kind, 0)?;
    require(scanner.at == raw.len(), "trailing NBT")?;
    let (named, used) = checked(nbt::read_named(raw, limits()))?;
    require(
        used == raw.len() && named.name.0.is_empty(),
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
    require(
        bytes.len() >= 8192 && bytes.len().is_multiple_of(4096),
        "truncated/unaligned native region",
    )?;
    let mut used = vec![false; bytes.len() / 4096];
    used[0] = true;
    used[1] = true;
    let mut present = Vec::new();
    for i in 0..1024 {
        let location = u32::from_be_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
        if location == 0 {
            continue;
        }
        let start = (location >> 8) as usize;
        let sectors = (location & 255) as usize;
        let end = start
            .checked_add(sectors)
            .ok_or_else(|| fail("region sector overflow"))?;
        require(
            start >= 2 && sectors > 0 && end <= used.len(),
            "invalid native region location/sector range",
        )?;
        require(
            used[start..end].iter().all(|v| !*v),
            "overlapping native region sectors",
        )?;
        used[start..end].fill(true);
        let at = start * 4096;
        let length = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        require(
            length >= 1 && length <= sectors * 4096 - 4,
            "native record payload length",
        )?;
        let compression = bytes[at + 4];
        require(
            compression & 128 == 0,
            "external native records unsupported",
        )?;
        checked(Compression::try_from(compression))?;
        present.push(i);
    }
    Ok(present)
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
fn validate_column(
    game: &Game<'_>,
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
                            game.pack.assets.registry.default_state(&name).is_some(),
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
                    game.pack.assets.registry.id(&state).is_some(),
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
                let id = game.res.biomes().id(&name);
                require(
                    game.res.biomes().get(id).name == name,
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
    let mut chunk = save::decode(named, game.pack.assets.registry, game.res.biomes())
        .ok_or_else(|| fail("native full decoder refused"))?;
    save::load_light(tag, &mut chunk, LevelShape::OVERWORLD.sky_light);
    require(
        chunk.x == position.0 && chunk.z == position.1,
        "native decoded coordinates",
    )?;
    // Palettes/indices were independently checked above. A fresh native re-encode
    // also proves native decoding did not silently drop a supported section value.
    let tables = save::SaveTables::new(&game.pack.assets);
    let tables = tag.get("Heightmaps").is_some().then_some(&tables);
    let encoded = save::encode_with(
        &chunk,
        game.pack.assets.registry,
        game.res.biomes(),
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
        require(
            row.path
                .starts_with("dimensions/minecraft/overworld/region/")
                && row.path.matches('/').count() == 4,
            "unsupported native region owner/dimension",
        )?;
        let path = root.join(&row.path);
        let bytes = read(&path, FILE_LIMIT)?;
        let entries = raw_region(&bytes)?;
        let region_position = checked(horizon_world::region::position_from_path(&path))?;
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
            let chunk = validate_column(game, &named, at, clock)?;
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
fn binding(game: &Game<'_>, frame: &CounterFrameV2) -> SResult<NativeBinding> {
    let catalog = game
        .options
        .mod_packages
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
    let binding = checked(NativeBinding::from_prepared(
        prepared,
        &frame.type_id,
        game.pack.assets.registry,
    ))?;
    let package = binding
        .package_identity()
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect::<String>();
    require(
        frame.package_identity == package
            && frame.type_id == binding.type_id()
            && frame.binding.block_id == binding.block_id()
            && frame.observed_state.name == "minecraft:iron_block"
            && frame.observed_state.properties.is_empty(),
        "physical binding/package mismatch",
    )?;
    Ok(binding)
}
fn correlate(
    manifest: &Manifest,
    counter: &CounterFrameV2,
    slot: &DecodedSlotFrame,
) -> SResult<()> {
    checked(counter.validate())?;
    let Placement::Active { generation, .. } = counter.placement else {
        return Err(fail("healthy active counter required"));
    };
    let view = slot.view();
    let SlotFrameAllocation::Active {
        machine_generation, ..
    } = view.allocation
    else {
        return Err(fail("healthy active slot required"));
    };
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
            && view.observed_state == "minecraft:iron_block"
            && view.saved_game_time == counter.saved_game_time
            && view.machine_high_water == counter.allocation_high_water
            && machine_generation == generation,
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
        if players
            .iter()
            .any(|row| player_path(row, "data").strip_prefix("world/") == Some(name))
        {
            let tag = compressed_nbt(&bytes)?;
            if !(row.path.ends_with("_old") && lan::session_empty_native_player(&tag)) {
                checked(lan::validate_session_player_tag(&tag))?;
            }
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
        validate_player_items(game, &player)?;
        validate_player_json(game, &advancements, &stats)?;
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
    counter: CounterFrameV2,
    slot: DecodedSlotFrame,
    players: CheckedSessionPlayers,
    columns: BTreeMap<(i32, i32), Arc<Chunk>>,
    raids: Option<Tag>,
}
fn manifest_shape(manifest: &Manifest, epoch: u64) -> SResult<()> {
    require(
        manifest.format == 1
            && manifest.kind == KIND
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
    checked(game.options.mod_packages.check_world(&world))?;
    // Require actual bounded pins even when an empty package setup would otherwise
    // let `check_world` accept their absence.
    let _: Value = json(&read(&world.join(PIN), 64 << 10)?, 64 << 10)?;
    level_clock(&world, manifest.saved_clock)?;
    let counter_bytes = read(&root.join(COUNTER), STRUCTURED_LIMIT)?;
    let _: Value = json(&counter_bytes, STRUCTURED_LIMIT)?;
    let counter = checked(CounterFrameV2::decode(&counter_bytes))?;
    let _ = binding(game, &counter)?;
    let slot_bytes = read(&root.join(SLOT), STRUCTURED_LIMIT)?;
    let _ = raw_nbt(&slot_bytes)?;
    let slot = checked(decode_slot_frame(&slot_bytes, game.res.items()))?;
    correlate(&manifest, &counter, &slot)?;
    let rows = player_rows(game, root, &manifest.players)?;
    sidecars(&world, &manifest.players)?;
    let raids = raid_metadata(&world)?;
    validate_native_local_player(&world, &manifest.players)?;
    let columns = columns(game, &world, manifest.saved_clock)?;
    for coordinate in &manifest.resident {
        require(
            columns.contains_key(coordinate),
            "resident native full record missing",
        )?;
    }
    let at = (
        expected_target.position[0] >> 4,
        expected_target.position[2] >> 4,
    );
    let target = columns
        .get(&at)
        .ok_or_else(|| fail("target full record absent"))?;
    let binding = binding(game, &counter)?;
    require(
        target.block(
            expected_target.position[0] & 15,
            expected_target.position[1],
            expected_target.position[2] & 15,
        ) == binding.state_id(),
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
    counter: CounterFrameV2,
    slot: Vec<u8>,
    players: Vec<SessionPlayerRow>,
    columns: BTreeMap<(i32, i32), NamedTag>,
    residents: Vec<(i32, i32)>,
    raids: Option<Tag>,
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
impl Game<'_> {
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
                .validate(&self.world_root, self.dim, &self.save),
        )?;
        identity(&self.world_root, world)?;
        checked(self.options.mod_packages.check_world(&self.world_root))?;
        let _: Value = json(&read(&self.world_root.join(PIN), 64 << 10)?, 64 << 10)?;
        self.session_running_packages()?;
        Ok(world)
    }
    fn session_fence(&mut self, until: Instant) -> SResult<()> {
        let save = self.save.clone();
        checked(
            self.streaming
                .native_write_fence_until(&mut self.world, until, || save.writes_finished()),
        )
    }
    fn session_snapshot(&self) -> SResult<Snapshot> {
        let world = self.session_context()?;
        let counter = checked(self.world.counter_checkpoint_frame(self.world.game_time))?;
        let slot = checked(
            self.world
                .slot_checkpoint_bytes(self.res.items(), self.world.game_time),
        )?;
        let parsed = checked(decode_slot_frame(&slot, self.res.items()))?;
        let roster = checked(
            self.observe_checkpoint_participants()
                .map_err(|e| format!("{e:?}")),
        )?;
        let players = checked(self.project_session_participant_rows(&roster))?;
        checked(self.inspect_session_checkpoint_owners())?;
        let raids = self.effective_session_raids()?;
        let descriptors = players.iter().map(descriptor).collect::<Vec<_>>();
        let local_uuid = validate_native_local_player(&self.world_root, &descriptors)?;
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
                self.save.tables.get().map(Arc::as_ref),
                LevelShape::OVERWORLD,
            );
            columns.insert((chunk.x, chunk.z), named);
        }
        let residents = columns.keys().copied().collect();
        let manifest = Manifest {
            format: 1,
            kind: KIND.into(),
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
        })
    }
    /// One continuous exclusive Game borrow spans both producer fences, native
    /// save(false), complete physical copying, semantic validation and promotion.
    pub(in crate::game) fn checkpoint_session(
        &mut self,
        archive_root: &Path,
        epoch: u64,
        until: Instant,
    ) -> SResult<CheckpointReceipt> {
        self.checkpoint_session_steps(archive_root, epoch, until, |_| Ok(()))
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
        mut step: impl FnMut(Step) -> SResult<()>,
    ) -> SResult<CheckpointReceipt> {
        let mut publication = Publication::NotPublished;
        let result = (|| {
            deadline(until)?;
            let world = self.session_context()?;
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
            let _ = columns(self, &source, self.world.game_time)?;
            let before = self.session_snapshot()?;
            checked(self.save_game_with(false))?;
            deadline(until)?;
            checked(self.write_session_checkpoint_raids())?;
            // Native autosave writes dirty columns. Checkpoint additionally queues
            // real snapshots for every resident so clean inhabited metadata and
            // the current saved clock cannot be lost.
            deadline(until)?;
            let residents = self.with_entities(self.world.chunks.values());
            checked(self.save.save_later(
                &residents,
                self.pack.assets.registry,
                self.res.biomes(),
            ))?;
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
                before.raids == after.raids,
                "native save changed empty raid metadata",
            )?;
            require(
                before.columns == after.columns,
                "native save changed full resident saved projection",
            )?;
            require(self.session_context()? == world, "native context changed")?;
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
                kind: KIND.into(),
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
                    self.save.tables.get().map(Arc::as_ref),
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
impl<'a> Game<'a> {
    fn fresh_session_candidate(&self) -> SResult<()> {
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

    /// Own the unpublished native candidate before admitting any transport.
    /// Participant installation and the final correlation remain separate gates.
    pub(in crate::game) fn prepare_session_checkpoint(
        mut self: Box<Self>,
        request: RecoveryRequest,
    ) -> SResult<NativeRecoveryParts<'a>> {
        self.fresh_session_candidate()?;
        deadline(request.deadline)?;
        let _ = self.advancement_tree();
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
            &self,
            &selected,
            epoch,
            request.expected_world,
            &request.expected_target,
            &request.expected_type,
        )?;
        let centers = player_centers(verified.players.rows(), &request.expected_target)?;
        let coverage = required_coverage(&centers)?;
        for at in &coverage {
            require(
                verified.columns.contains_key(at),
                "recovery required native full coverage missing before loader",
            )?;
        }
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
        let mut prepared = checked(
            open.with_expected_checkpoint_world(request.expected_world)
                .prepare(&self.res, &self.pack, &self.options, &self.settings),
        )?;
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
        checked(self.options.mod_packages.check_world(&request.destination))?;
        let under_lock = columns(&self, &request.destination, verified.manifest.saved_clock)?;
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
        let clocks = (prepared.world.game_time, prepared.world.day_time);
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
        for center in centers {
            prepared.streaming.center = center;
            loop {
                deadline(request.deadline)?;
                let _ = prepared.streaming.update(&mut prepared.world);
                require(
                    (prepared.world.game_time, prepared.world.day_time) == clocks,
                    "streaming advanced native clocks",
                )?;
                if prepared.world.chunks.contains_key(&center) {
                    break;
                }
                std::thread::yield_now();
            }
        }
        // Return center to the machine while real arrivals may enlarge residency.
        prepared.streaming.center = target;
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
        for (at, chunk) in &prepared.world.chunks {
            let expected = under_lock
                .get(at)
                .ok_or_else(|| fail("unexpected regenerated native arrival"))?;
            let a = save::encode(chunk, self.pack.assets.registry, self.res.biomes());
            let b = save::encode(expected, self.pack.assets.registry, self.res.biomes());
            require(
                lan::session_player_tags_equal(&a.tag, &b.tag),
                "native arrival changed saved semantics",
            )?;
        }
        let binding = binding(&self, &verified.counter)?;
        let counter = checked(prepared.world.restore_counter_frame(
            &binding,
            request.expected_world,
            &request.expected_target,
            &verified.counter,
        ))?
        .ok_or_else(|| fail("restored active counter key absent"))?;
        let slot = prepared
            .world
            .restore_persistent_world_slot(
                &binding,
                self.res.items(),
                request.expected_world,
                &request.expected_target,
                verified.slot,
            )
            .map_err(|e| fail(e.error))?
            .ok_or_else(|| fail("restored active slot key absent"))?;
        checked(self.finish_open_world(prepared))?;
        let random = self.world.random.state();
        checked(self.install_session_checkpoint_raids(verified.raids.as_ref()))?;
        require(
            self.world.random.state() == random,
            "native raid installation drew from world random",
        )?;
        // Opening resets progress and starts a fresh native tree prefetch.
        // Finish it before player restoration and its strict JSON re-projection.
        let _ = self.advancement_tree();
        Ok(NativeRecoveryParts {
            game: self,
            request,
            verified: NativeRecoveryImage {
                manifest: verified.manifest,
                counter: verified.counter,
                players: verified.players,
                raids: verified.raids,
            },
            selected,
            clocks,
            receipt: RecoveryReceipt {
                epoch,
                counter,
                slot,
            },
        })
    }
}

/// Non-clone ownership of a verified native recovery, not a ready Game.
struct NativeRecoveryImage {
    manifest: Manifest,
    counter: CounterFrameV2,
    players: CheckedSessionPlayers,
    raids: Option<Tag>,
}
pub(in crate::game) struct NativeRecoveryParts<'a> {
    game: Box<Game<'a>>,
    request: RecoveryRequest,
    verified: NativeRecoveryImage,
    selected: PathBuf,
    clocks: (i64, i64),
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
            ..
        } = self;
        game.session_fence(request.deadline)?;
        require(
            (game.world.game_time, game.world.day_time) == *clocks,
            "recovery advanced native clocks",
        )?;
        let snapshot = game.session_snapshot()?;
        require(
            snapshot.counter == verified.counter
                && same_players(&snapshot.players, verified.players.rows())
                && snapshot.raids == verified.raids
                && raid_metadata(&game.world_root)? == verified.raids,
            "final restored counter/player mismatch",
        )?;
        let recovered_slot = checked(decode_slot_frame(&snapshot.slot, game.res.items()))?;
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
}

#[cfg(all(test, feature = "paired-packages", target_os = "linux"))]
#[path = "session_checkpoint_tests.rs"]
mod tests;

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
                        game.res.items().get(&id).is_some() && id != "minecraft:air",
                        "unresolved player item ID",
                    )?;
                    require(
                        matches!(value.get("count"),Some(Tag::Int(v)) if *v>0),
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
    let progress = horizon_client::advancements::Progress::from_json(advancements);
    require(
        progress.to_json(&game.adv.tree, DATA_VERSION) == *advancements,
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
