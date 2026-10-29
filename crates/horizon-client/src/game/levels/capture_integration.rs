//! Fresh Linux fixture mechanism only. No runtime admission or live generation restore.
use super::*;
use crate::game::lan::CapturePlayerRow;
use horizon_client::save::{SessionLock, uuid_from_text, uuid_text};
use horizon_world::{
    compression::Compression,
    nbt::{self, Limits, NamedTag, Tag},
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
    io::{self, Read, Write},
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};

const ENTRY_LIMIT: usize = 256;
const FILE_LIMIT: u64 = 32 << 20;
const IMAGE_LIMIT: u64 = 64 << 20;
const JSON_LIMIT: usize = 1 << 20;
const MANIFEST_LIMIT: usize = 256 << 10;
const NODE_LIMIT: usize = 16_384;
const NBT_LIMIT: usize = 1 << 20;
const OWNER_RECORD: [i32; 4] = [-1, -2, 3, 4];
const SLOT_MEMBER: &str = "world/data/horizon/slot_checkpoint.nbt";
const SLOT_KIND: &str = "fresh-native-slot-capture-cell";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::game) struct Identity {
    pub engine_source: String,
    pub test_binary: String,
    pub resource_manifest: String,
    pub counter_content: String,
}
#[derive(Clone, Debug)]
pub(in crate::game) struct CaptureRequest {
    pub archive_root: PathBuf,
    pub epoch: u64,
    pub identity: Identity,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::game) struct CaptureReceipt {
    pub epoch: u64,
    pub manifest_sha256: [u8; 32],
    pub members: u32,
    pub image_bytes: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::game) enum Publication {
    NotPublished,
    Uncertain,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::game) enum Phase {
    Dispatch,
    Validate,
    Project,
    Save,
    Copy,
    Overlay,
    SyncMembers,
    Manifest,
    Promote,
    SyncArchive,
    Recover,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::game) enum Failure {
    CompletionLost,
    Context,
    Player,
    Entity,
    World,
    Streamer,
    Limit,
    Path,
    Identity,
    Profile,
    NativeSave,
    Io,
    InvalidImage,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::game) struct CaptureError {
    pub phase: Phase,
    pub publication: Publication,
    pub code: Failure,
    pub relative_path: Option<String>,
    pub detail: String,
}
pub(in crate::game) type CaptureCompletion = std::result::Result<CaptureReceipt, CaptureError>;
type CResult<T> = std::result::Result<T, CaptureError>;
impl CaptureError {
    fn new(phase: Phase, code: Failure, path: Option<&Path>, detail: impl fmt::Display) -> Self {
        let mut detail = detail.to_string();
        let mut end = detail.len().min(4096);
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        detail.truncate(end);
        Self {
            phase,
            publication: Publication::NotPublished,
            code,
            relative_path: path.map(|p| p.to_string_lossy().into_owned()),
            detail,
        }
    }
    pub(in crate::game) fn completion_lost(detail: impl Into<String>) -> Self {
        let mut error = Self::new(
            Phase::Dispatch,
            Failure::CompletionLost,
            None,
            detail.into(),
        );
        error.publication = Publication::Uncertain;
        error
    }
    fn uncertain(mut self) -> Self {
        self.publication = Publication::Uncertain;
        self
    }
}
fn io_error(phase: Phase, path: &Path, error: io::Error) -> CaptureError {
    CaptureError::new(phase, Failure::Io, Some(path), error)
}
fn invalid(phase: Phase, detail: impl fmt::Display) -> CaptureError {
    CaptureError::new(phase, Failure::InvalidImage, None, detail)
}
fn limit(phase: Phase) -> CaptureError {
    CaptureError::new(phase, Failure::Limit, None, "fixture bound exceeded")
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl Identity {
    fn validate(&self) -> CResult<()> {
        if [
            &self.engine_source,
            &self.test_binary,
            &self.resource_manifest,
            &self.counter_content,
        ]
        .into_iter()
        .all(|s| valid_hash(s))
        {
            Ok(())
        } else {
            Err(CaptureError::new(
                Phase::Validate,
                Failure::Identity,
                None,
                "identity must contain four lowercase SHA256 values",
            ))
        }
    }
}

/// Bounded while writing, not a check after an unlimited serialization.
struct ByteWriter {
    bytes: Vec<u8>,
    cap: usize,
}
impl Write for ByteWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.len() > self.cap.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("JSON byte bound"));
        }
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn json_bytes<T: Serialize>(value: &T, cap: usize) -> CResult<Vec<u8>> {
    let mut writer = ByteWriter {
        bytes: Vec::new(),
        cap,
    };
    serde_json::to_writer_pretty(&mut writer, value).map_err(|e| invalid(Phase::Project, e))?;
    // One recursive bounded reader validates generated values too.
    let _: Value = parse_json(&writer.bytes, cap)?;
    Ok(writer.bytes)
}
struct Budget {
    nodes: usize,
    probe: bool,
    duplicate: bool,
    slot_kind: bool,
    slot_schema: bool,
}
struct Seed<'a> {
    budget: &'a mut Budget,
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for Seed<'_> {
    type Value = Value;
    fn deserialize<D: de::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Value, D::Error> {
        if self.depth > 32 || self.budget.nodes >= NODE_LIMIT {
            return Err(de::Error::custom("JSON structural bound"));
        }
        self.budget.nodes += 1;
        deserializer.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Seed<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bounded integer JSON")
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Value, E> {
        Ok(Value::Bool(v))
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
    fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Value, E> {
        Err(E::custom("floating JSON is outside this cell"))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Value, E> {
        if v.len() > 4096 {
            Err(E::custom("JSON string bound"))
        } else {
            Ok(Value::String(v.into()))
        }
    }
    fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Value, E> {
        if v.len() > 4096 {
            Err(E::custom("JSON string bound"))
        } else {
            Ok(Value::String(v))
        }
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(Seed {
            budget: &mut *self.budget,
            depth: self.depth + 1,
        })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Value, A::Error> {
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if key.len() > 4096 || self.budget.nodes >= NODE_LIMIT {
                return Err(de::Error::custom("JSON key/duplicate bound"));
            }
            if values.contains_key(&key) {
                if !self.budget.probe {
                    return Err(de::Error::custom("JSON key/duplicate bound"));
                }
                self.budget.duplicate = true;
            }
            self.budget.nodes += 1;
            let value = map.next_value_seed(Seed {
                budget: &mut *self.budget,
                depth: self.depth + 1,
            })?;
            if self.depth == 0 && key == "kind" && value.as_str() == Some(SLOT_KIND) {
                self.budget.slot_kind = true;
            }
            if self.depth == 0 && key == "schema" && value.as_u64() == Some(2) {
                self.budget.slot_schema = true;
            }
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}
fn parse_json<T: DeserializeOwned>(bytes: &[u8], cap: usize) -> CResult<T> {
    if bytes.len() > cap {
        return Err(limit(Phase::Recover));
    }
    let mut budget = Budget {
        nodes: 0,
        probe: false,
        duplicate: false,
        slot_kind: false,
        slot_schema: false,
    };
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = Seed {
        budget: &mut budget,
        depth: 0,
    }
    .deserialize(&mut deserializer)
    .map_err(|e| invalid(Phase::Recover, e))?;
    deserializer.end().map_err(|e| invalid(Phase::Recover, e))?;
    serde_json::from_value(value).map_err(|e| invalid(Phase::Recover, e))
}
/// Same structural budget; duplicate retention is classification ONLY, never admission.
fn envelope_probe(bytes: &[u8]) -> CResult<(Value, bool, bool)> {
    if bytes.len() > MANIFEST_LIMIT {
        return Err(limit(Phase::Recover));
    }
    let mut budget = Budget {
        nodes: 0,
        probe: true,
        duplicate: false,
        slot_kind: false,
        slot_schema: false,
    };
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let value = Seed {
        budget: &mut budget,
        depth: 0,
    }
    .deserialize(&mut decoder)
    .map_err(|e| invalid(Phase::Recover, e))?;
    decoder.end().map_err(|e| invalid(Phase::Recover, e))?;
    let known_slot = budget.slot_kind || (budget.slot_schema && value.get("slot_path").is_some());
    Ok((value, budget.duplicate, known_slot))
}
fn profile_error(detail: impl fmt::Display) -> CaptureError {
    CaptureError::new(Phase::Recover, Failure::Profile, None, detail)
}
fn tag_bounds(root: &Tag) -> CResult<()> {
    let mut work = vec![(root, 0usize)];
    let mut nodes = 0usize;
    let string = |s: &nbt::JavaString| {
        s.0.len() <= 4096 && char::decode_utf16(s.0.iter().copied()).all(|c| c.is_ok())
    };
    while let Some((tag, depth)) = work.pop() {
        nodes += 1;
        if depth > 32 || nodes > NODE_LIMIT {
            return Err(limit(Phase::Project));
        }
        match tag {
            Tag::End => return Err(invalid(Phase::Project, "End is not a player payload")),
            Tag::Float(bits) if !f32::from_bits(*bits).is_finite() || *bits == 0x80000000 => {
                return Err(invalid(Phase::Project, "unsupported float"));
            }
            Tag::Double(bits)
                if !f64::from_bits(*bits).is_finite() || *bits == 0x8000000000000000 =>
            {
                return Err(invalid(Phase::Project, "unsupported double"));
            }
            Tag::String(s) if !string(s) => {
                return Err(invalid(Phase::Project, "unsupported UTF16"));
            }
            Tag::List(values) => {
                if values.len() > NODE_LIMIT.saturating_sub(nodes + work.len()) {
                    return Err(limit(Phase::Project));
                }
                work.extend(values.iter().map(|v| (v, depth + 1)));
            }
            Tag::Compound(values) => {
                if values.len() > NODE_LIMIT.saturating_sub(nodes + work.len())
                    || values.keys().any(|s| !string(s))
                {
                    return Err(limit(Phase::Project));
                }
                work.extend(values.values().map(|v| (v, depth + 1)));
            }
            Tag::ByteArray(v) if v.len() > NODE_LIMIT => return Err(limit(Phase::Project)),
            Tag::IntArray(v) if v.len() > NODE_LIMIT => return Err(limit(Phase::Project)),
            Tag::LongArray(v) if v.len() > NODE_LIMIT => return Err(limit(Phase::Project)),
            _ => {}
        }
    }
    Ok(())
}
fn nbt_limits() -> Limits {
    Limits {
        max_bytes: NBT_LIMIT,
        max_nodes: NODE_LIMIT,
        max_depth: 32,
    }
}
fn player_bytes(tag: &Tag) -> CResult<Vec<u8>> {
    tag_bounds(tag)?;
    let raw = nbt::write_named(
        &NamedTag {
            name: "".into(),
            tag: tag.clone(),
        },
        nbt_limits(),
    )
    .map_err(|e| invalid(Phase::Project, e))?;
    let (decoded, used) =
        nbt::read_named(&raw, nbt_limits()).map_err(|e| invalid(Phase::Project, e))?;
    if used != raw.len() || decoded.tag != *tag {
        return Err(invalid(
            Phase::Project,
            "player codec changed supported values",
        ));
    }
    let bytes = Compression::Gzip
        .encode(&raw)
        .map_err(|e| io_error(Phase::Project, Path::new("player"), e))?;
    if bytes.len() > 2 << 20 {
        return Err(limit(Phase::Project));
    }
    Ok(bytes)
}
fn strict_player(bytes: &[u8]) -> CResult<Tag> {
    if bytes.len() > 2 << 20 {
        return Err(limit(Phase::Recover));
    }
    let raw = Compression::Gzip
        .decode(bytes, NBT_LIMIT)
        .map_err(|e| io_error(Phase::Recover, Path::new("player"), e))?;
    let (value, used) =
        nbt::read_named(&raw, nbt_limits()).map_err(|e| invalid(Phase::Recover, e))?;
    if used != raw.len() {
        return Err(invalid(Phase::Recover, "trailing player NBT"));
    }
    tag_bounds(&value.tag)?;
    Ok(value.tag)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlayerDescriptor {
    profile_uuid: String,
    record_uuid: String,
    integrated_owner: bool,
    dimension: String,
}
struct PlayerImage {
    descriptor: PlayerDescriptor,
    nbt: Vec<u8>,
    advancements: Vec<u8>,
    stats: Vec<u8>,
}
impl PlayerImage {
    fn from_row(row: CapturePlayerRow) -> CResult<Self> {
        let _connection = row.conn;
        Ok(Self {
            descriptor: PlayerDescriptor {
                profile_uuid: uuid_text(horizon_client::lan::playerdata::uuid_ints(
                    row.profile_uuid,
                )),
                record_uuid: uuid_text(row.record_uuid),
                integrated_owner: row.integrated_owner,
                dimension: row.dimension,
            },
            nbt: player_bytes(&row.player)?,
            advancements: json_bytes(&row.advancements, JSON_LIMIT)?,
            stats: json_bytes(&row.stats, JSON_LIMIT)?,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveScope {
    world_instance: u64,
    dimension: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveState {
    name: String,
    properties: BTreeMap<String, String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CounterArchive {
    schema: u32,
    kind: String,
    scope: ArchiveScope,
    r#type: String,
    save_schema: u32,
    position: [i32; 3],
    state: ArchiveState,
    runtime_instance: u64,
    counter: u64,
    #[serde(deserialize_with = "required_optional_i64")]
    last_advanced: Option<i64>,
    resident: bool,
    live_generation_restore: bool,
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
struct Manifest {
    schema: u32,
    kind: String,
    epoch: u64,
    identity: Identity,
    players: Vec<PlayerDescriptor>,
    dimensions: Vec<String>,
    counter: String,
    directories: Vec<String>,
    files: Vec<Member>,
}
struct Material {
    source: PathBuf,
    request: CaptureRequest,
    players: Vec<PlayerImage>,
    counter: CounterArchive,
    dimensions: Vec<String>,
}
struct PromotedImage {
    archive_root: PathBuf,
    receipt: CaptureReceipt,
}
struct PendingImage {
    root: PathBuf,
    request: CaptureRequest,
    players: Vec<PlayerImage>,
    counter: CounterArchive,
    dimensions: Vec<String>,
    manifest: Option<Manifest>,
    manifest_bytes: Vec<u8>,
}
struct RecoveryReceipt {
    epoch: u64,
    counter: CounterArchive,
    rejected: Vec<(u64, String)>,
    _lock: SessionLock,
}
fn relative(path: &Path) -> CResult<String> {
    let text = path
        .to_str()
        .ok_or_else(|| invalid(Phase::Validate, "non-UTF8 path"))?;
    if text.is_empty()
        || text.len() > 240
        || !text.is_ascii()
        || text.contains(['\\', ':', '\0'])
        || text.split('/').count() > 12
        || text
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
        || path.is_absolute()
    {
        return Err(CaptureError::new(
            Phase::Validate,
            Failure::Path,
            Some(path),
            "unsupported relative path",
        ));
    }
    Ok(text.into())
}
fn ancestors(path: &Path) -> CResult<()> {
    if !path.is_absolute() {
        return Err(CaptureError::new(
            Phase::Validate,
            Failure::Path,
            Some(path),
            "absolute owned path required",
        ));
    }
    let mut at = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Normal(_) => at.push(component),
            _ => {
                return Err(CaptureError::new(
                    Phase::Validate,
                    Failure::Path,
                    Some(path),
                    "noncanonical component",
                ));
            }
        }
        match fs::symlink_metadata(&at) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(CaptureError::new(
                    Phase::Validate,
                    Failure::Path,
                    Some(&at),
                    "symlink ancestor",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(Phase::Validate, &at, error)),
        }
    }
    Ok(())
}
fn owned_dir(path: &Path) -> CResult<PathBuf> {
    ancestors(path)?;
    let meta = fs::symlink_metadata(path).map_err(|e| io_error(Phase::Validate, path, e))?;
    if !meta.is_dir() {
        return Err(CaptureError::new(
            Phase::Validate,
            Failure::Path,
            Some(path),
            "directory required",
        ));
    }
    fs::canonicalize(path).map_err(|e| io_error(Phase::Validate, path, e))
}
fn fresh_dir(path: &Path, phase: Phase) -> CResult<()> {
    ancestors(path)?;
    owned_dir(
        path.parent()
            .ok_or_else(|| invalid(phase, "parent required"))?,
    )?;
    fs::create_dir(path).map_err(|e| io_error(phase, path, e))
}
fn open_regular(path: &Path, cap: u64, phase: Phase) -> CResult<(File, u64)> {
    ancestors(path)?;
    let before = fs::symlink_metadata(path).map_err(|e| io_error(phase, path, e))?;
    if !before.is_file() || before.len() > cap {
        return Err(CaptureError::new(
            phase,
            Failure::Limit,
            Some(path),
            "regular file length bound",
        ));
    }
    let file = File::open(path).map_err(|e| io_error(phase, path, e))?;
    let opened = file.metadata().map_err(|e| io_error(phase, path, e))?;
    if !opened.is_file()
        || opened.len() != before.len()
        || opened.dev() != before.dev()
        || opened.ino() != before.ino()
    {
        return Err(invalid(phase, "opened file changed"));
    }
    Ok((file, opened.len()))
}
fn bounded_read(path: &Path, cap: usize, phase: Phase) -> CResult<Vec<u8>> {
    let (mut file, len) = open_regular(path, cap as u64, phase)?;
    let mut bytes = Vec::with_capacity(len as usize);
    (&mut file)
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_error(phase, path, e))?;
    if bytes.len() as u64 != len || bytes.len() > cap {
        return Err(invalid(phase, "file length changed"));
    }
    Ok(bytes)
}
fn file_hash(path: &Path, phase: Phase) -> CResult<(u64, String)> {
    let (mut file, len) = open_regular(path, FILE_LIMIT, phase)?;
    let mut digest = Sha256::new();
    let mut used = 0u64;
    let mut buffer = [0u8; 64 << 10];
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|e| io_error(phase, path, e))?;
        if n == 0 {
            break;
        }
        used = used.checked_add(n as u64).ok_or_else(|| limit(phase))?;
        if used > FILE_LIMIT || used > len {
            return Err(limit(phase));
        }
        digest.update(&buffer[..n]);
    }
    if used != len || file.metadata().map_err(|e| io_error(phase, path, e))?.len() != len {
        return Err(invalid(phase, "file changed during hash"));
    }
    Ok((used, format!("{:x}", digest.finalize())))
}
#[derive(Debug, PartialEq, Eq)]
struct Tree {
    directories: Vec<String>,
    files: Vec<Member>,
    bytes: u64,
}
fn tree(root: &Path, skip: &[&str], phase: Phase) -> CResult<Tree> {
    fn visit(
        root: &Path,
        dir: &Path,
        skip: &[&str],
        phase: Phase,
        out: &mut Tree,
        entries: &mut usize,
    ) -> CResult<()> {
        let mut children = Vec::new();
        for entry in fs::read_dir(dir).map_err(|e| io_error(phase, dir, e))? {
            if children.len() >= ENTRY_LIMIT {
                return Err(limit(phase));
            }
            children.push(entry.map_err(|e| io_error(phase, dir, e))?.path());
        }
        children.sort();
        for path in children {
            let rel = relative(path.strip_prefix(root).map_err(|e| invalid(phase, e))?)?;
            *entries += 1;
            if *entries > ENTRY_LIMIT {
                return Err(limit(phase));
            }
            let meta = fs::symlink_metadata(&path).map_err(|e| io_error(phase, &path, e))?;
            if meta.file_type().is_symlink() {
                return Err(invalid(phase, "symlink image member"));
            }
            if skip.contains(&rel.as_str()) {
                continue;
            }
            if meta.is_dir() {
                out.directories.push(rel);
                visit(root, &path, skip, phase, out, entries)?;
            } else if meta.is_file() {
                let (bytes, sha256) = file_hash(&path, phase)?;
                out.bytes = out.bytes.checked_add(bytes).ok_or_else(|| limit(phase))?;
                if out.bytes > IMAGE_LIMIT {
                    return Err(limit(phase));
                }
                out.files.push(Member {
                    path: rel,
                    bytes,
                    sha256,
                });
            } else {
                return Err(invalid(phase, "nonregular image member"));
            }
        }
        Ok(())
    }
    owned_dir(root)?;
    let mut out = Tree {
        directories: Vec::new(),
        files: Vec::new(),
        bytes: 0,
    };
    let mut entries = 1;
    visit(root, root, skip, phase, &mut out, &mut entries)?;
    out.directories.sort();
    out.files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}
fn physical_copy(source: &Path, target: &Path, member: &Member, phase: Phase) -> CResult<()> {
    let (mut input, len) = open_regular(source, FILE_LIMIT, phase)?;
    if len != member.bytes {
        return Err(invalid(phase, "source length differs"));
    }
    ancestors(target)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .map_err(|e| io_error(phase, target, e))?;
    let a = input.metadata().map_err(|e| io_error(phase, source, e))?;
    let b = output.metadata().map_err(|e| io_error(phase, target, e))?;
    if (a.dev(), a.ino()) == (b.dev(), b.ino()) {
        return Err(invalid(phase, "copy aliases source inode"));
    }
    let mut digest = Sha256::new();
    let mut used = 0u64;
    let mut buffer = [0u8; 64 << 10];
    loop {
        let n = input
            .read(&mut buffer)
            .map_err(|e| io_error(phase, source, e))?;
        if n == 0 {
            break;
        }
        used = used.checked_add(n as u64).ok_or_else(|| limit(phase))?;
        if used > len || used > FILE_LIMIT {
            return Err(limit(phase));
        }
        digest.update(&buffer[..n]);
        output
            .write_all(&buffer[..n])
            .map_err(|e| io_error(phase, target, e))?;
    }
    if used != len || format!("{:x}", digest.finalize()) != member.sha256 {
        return Err(invalid(phase, "source changed during copy"));
    }
    Ok(())
}
fn sync_dir(path: &Path, phase: Phase) -> CResult<()> {
    owned_dir(path)?;
    File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| io_error(phase, path, e))
}
fn sync_tree(root: &Path, phase: Phase) -> CResult<()> {
    let entries = tree(root, &[], phase)?;
    for member in entries.files {
        let path = root.join(member.path);
        File::open(&path)
            .and_then(|f| f.sync_all())
            .map_err(|e| io_error(phase, &path, e))?;
    }
    for dir in entries.directories.iter().rev() {
        sync_dir(&root.join(dir), phase)?;
    }
    sync_dir(root, phase)
}
fn write_bytes(path: &Path, bytes: &[u8], new: bool, phase: Phase) -> CResult<()> {
    ancestors(path)?;
    let mut options = OpenOptions::new();
    options.write(true);
    if new {
        options.create_new(true);
    } else {
        if let Ok(meta) = fs::symlink_metadata(path)
            && !meta.is_file()
        {
            return Err(invalid(phase, "overlay target not regular"));
        }
        options.create(true).truncate(true);
    }
    let mut file = options.open(path).map_err(|e| io_error(phase, path, e))?;
    file.write_all(bytes).map_err(|e| io_error(phase, path, e))
}
fn make_parents(root: &Path, relative_path: &str, phase: Phase) -> CResult<PathBuf> {
    relative(Path::new(relative_path))?;
    let target = root.join(relative_path);
    let parent = target
        .parent()
        .ok_or_else(|| invalid(phase, "overlay parent"))?;
    ancestors(parent)?;
    fs::create_dir_all(parent).map_err(|e| io_error(phase, parent, e))?;
    Ok(target)
}
fn player_target(row: &PlayerDescriptor, kind: &str) -> String {
    format!(
        "world/players/{kind}/{}.{}",
        row.record_uuid,
        if kind == "data" { "dat" } else { "json" }
    )
}
impl Material {
    fn copy(self) -> CResult<PendingImage> {
        let archive = owned_dir(&self.request.archive_root)?;
        let source = owned_dir(&self.source)?;
        if archive.starts_with(&source) || source.starts_with(&archive) {
            return Err(invalid(Phase::Copy, "overlapping roots"));
        }
        let published = archive.join(format!("epoch-{:020}", self.request.epoch));
        if published
            .try_exists()
            .map_err(|e| io_error(Phase::Copy, &published, e))?
        {
            return Err(invalid(Phase::Copy, "epoch already exists"));
        }
        let root = archive.join(format!(".pending-{:020}", self.request.epoch));
        let entries = tree(&source, &["session.lock"], Phase::Copy)?;
        fresh_dir(&root, Phase::Copy)?;
        let world = root.join("world");
        fresh_dir(&world, Phase::Copy)?;
        for dir in entries.directories {
            let at = world.join(dir);
            fresh_dir(&at, Phase::Copy)?;
        }
        for member in entries.files {
            physical_copy(
                &source.join(&member.path),
                &world.join(&member.path),
                &member,
                Phase::Copy,
            )?;
        }
        Ok(PendingImage {
            root,
            request: self.request,
            players: self.players,
            counter: self.counter,
            dimensions: self.dimensions,
            manifest: None,
            manifest_bytes: Vec::new(),
        })
    }
}
impl PendingImage {
    /// Compare both inherited inputs before touching either pending copy. Source
    /// files and the older immutable epoch are never overlay destinations.
    fn replace_inherited_slot_files(
        &self,
        source: &Path,
        inherited: &InheritedSlotFiles,
        counter: &[u8],
        slot: &[u8],
    ) -> CResult<()> {
        if counter.len() > 16 << 10 || slot.len() > 1 << 20 {
            return Err(limit(Phase::Overlay));
        }
        inherited.require_unchanged(source, Phase::Overlay)?;
        inherited.require_unchanged(&self.root.join("world"), Phase::Overlay)?;
        write_bytes(
            &self.root.join(counter_restore_integration::FRAME_PATH),
            counter,
            false,
            Phase::Overlay,
        )?;
        write_bytes(&self.root.join(SLOT_MEMBER), slot, false, Phase::Overlay)
    }
    fn overlay(&mut self) -> CResult<()> {
        for row in &self.players {
            for (kind, bytes) in [
                ("data", &row.nbt),
                ("advancements", &row.advancements),
                ("stats", &row.stats),
            ] {
                let target = make_parents(
                    &self.root,
                    &player_target(&row.descriptor, kind),
                    Phase::Overlay,
                )?;
                write_bytes(&target, bytes, false, Phase::Overlay)?;
            }
        }
        let bytes = json_bytes(&self.counter, JSON_LIMIT)?;
        let target = make_parents(&self.root, "research/counter.json", Phase::Overlay)?;
        write_bytes(&target, &bytes, true, Phase::Overlay)
    }
    fn sync_members(&self) -> CResult<()> {
        sync_tree(&self.root, Phase::SyncMembers)
    }
    fn write_manifest_temp(&mut self) -> CResult<()> {
        self.write_manifest_temp_for(RecoveryProfile::ArchiveOnly)
    }
    fn write_manifest_temp_for(&mut self, profile: RecoveryProfile) -> CResult<()> {
        let entries = tree(&self.root, &[], Phase::Manifest)?;
        let mut players: Vec<_> = self.players.iter().map(|r| r.descriptor.clone()).collect();
        players.sort_by(|a, b| a.record_uuid.cmp(&b.record_uuid));
        let manifest = Manifest {
            schema: 1,
            kind: "fresh-native-capture-cell".into(),
            epoch: self.request.epoch,
            identity: self.request.identity.clone(),
            players,
            dimensions: self.dimensions.clone(),
            counter: "research/counter.json".into(),
            directories: entries.directories,
            files: entries.files,
        };
        validate_manifest(&manifest, self.request.epoch, &self.request.identity)?;
        let bytes = if matches!(
            profile,
            RecoveryProfile::ActiveSlot | RecoveryProfile::AdvancedSlot
        ) {
            let slot =
                counter_restore_integration::slot_epoch_integration::SlotManifest::from_manifest(
                    manifest.clone(),
                );
            slot.validate(&self.request.identity, self.request.epoch)?;
            json_bytes(&slot, MANIFEST_LIMIT)?
        } else {
            if manifest
                .files
                .iter()
                .any(|member| member.path == SLOT_MEMBER)
            {
                return Err(profile_error("legacy publisher cannot ignore slot member"));
            }
            json_bytes(&manifest, MANIFEST_LIMIT)?
        };
        let path = self.root.join("manifest.json.tmp");
        write_bytes(&path, &bytes, true, Phase::Manifest)?;
        File::open(&path)
            .and_then(|f| f.sync_all())
            .map_err(|e| io_error(Phase::Manifest, &path, e))?;
        self.manifest = Some(manifest);
        self.manifest_bytes = bytes;
        Ok(())
    }
    fn finish_manifest(&self) -> CResult<()> {
        if self.manifest.is_none() {
            return Err(invalid(Phase::Manifest, "manifest not prepared"));
        }
        let target = self.root.join("manifest.json");
        if target
            .try_exists()
            .map_err(|e| io_error(Phase::Manifest, &target, e))?
        {
            return Err(invalid(Phase::Manifest, "manifest already exists"));
        }
        fs::rename(self.root.join("manifest.json.tmp"), &target)
            .map_err(|e| io_error(Phase::Manifest, &target, e))?;
        sync_dir(&self.root, Phase::Manifest)
    }
    fn rename_published(self) -> CResult<PromotedImage> {
        self.rename_published_for(RecoveryProfile::ArchiveOnly)
    }
    fn rename_published_for(self, profile: RecoveryProfile) -> CResult<PromotedImage> {
        let manifest = self
            .manifest
            .as_ref()
            .ok_or_else(|| invalid(Phase::Promote, "manifest absent"))?;
        let envelope =
            verify_complete_envelope(&self.root, self.request.epoch, &self.request.identity)?;
        check_recovery_profile(&envelope.manifest, envelope.slot_manifest.as_ref(), profile)?;
        let destination = self
            .request
            .archive_root
            .join(format!("epoch-{:020}", self.request.epoch));
        if destination
            .try_exists()
            .map_err(|e| io_error(Phase::Promote, &destination, e))?
        {
            return Err(invalid(Phase::Promote, "published target exists"));
        }
        // From the ATTEMPT onward an error cannot prove that publication is absent.
        fs::rename(&self.root, &destination)
            .map_err(|e| io_error(Phase::Promote, &destination, e).uncertain())?;
        let manifest_sha256: [u8; 32] = Sha256::digest(&self.manifest_bytes).into();
        let receipt = CaptureReceipt {
            epoch: manifest.epoch,
            manifest_sha256,
            members: manifest.files.len() as u32,
            image_bytes: manifest.files.iter().map(|m| m.bytes).sum(),
        };
        Ok(PromotedImage {
            archive_root: self.request.archive_root,
            receipt,
        })
    }
    fn promote(self) -> CaptureCompletion {
        self.rename_published()?.acknowledge()
    }
}

/// Exact archival bytes carried from an explicitly restored working world.
/// This is private input accounting, not a save lease or runtime authority.
struct InheritedSlotFiles {
    counter: Vec<u8>,
    slot: Vec<u8>,
}
impl InheritedSlotFiles {
    fn read(source: &Path, phase: Phase) -> CResult<Self> {
        Ok(Self {
            counter: bounded_read(
                &source.join("data/horizon/counter_checkpoint.json"),
                16 << 10,
                phase,
            )?,
            slot: bounded_read(
                &source.join("data/horizon/slot_checkpoint.nbt"),
                1 << 20,
                phase,
            )?,
        })
    }
    fn require_unchanged(&self, source: &Path, phase: Phase) -> CResult<()> {
        let current = Self::read(source, phase)?;
        if current.counter != self.counter || current.slot != self.slot {
            return Err(invalid(phase, "validated inherited sidecar bytes changed"));
        }
        Ok(())
    }
}
impl PromotedImage {
    fn acknowledge(self) -> CaptureCompletion {
        sync_dir(&self.archive_root, Phase::SyncArchive).map_err(CaptureError::uncertain)?;
        if let Some(parent) = self.archive_root.parent() {
            sync_dir(parent, Phase::SyncArchive).map_err(CaptureError::uncertain)?;
        }
        Ok(self.receipt)
    }
}
fn canonical_uuid(value: &str) -> bool {
    uuid_from_text(value).is_some_and(|uuid| uuid_text(uuid) == value)
}
fn validate_manifest(manifest: &Manifest, epoch: u64, identity: &Identity) -> CResult<()> {
    identity.validate()?;
    manifest.identity.validate()?;
    if &manifest.identity != identity {
        return Err(CaptureError::new(
            Phase::Recover,
            Failure::Identity,
            None,
            "epoch build/content mismatch",
        ));
    }
    if manifest.schema != 1
        || manifest.kind != "fresh-native-capture-cell"
        || manifest.epoch != epoch
        || epoch == 0
        || manifest.counter != "research/counter.json"
    {
        return Err(invalid(Phase::Recover, "manifest schema/epoch"));
    }
    if manifest.players.len() != 2
        || manifest
            .players
            .windows(2)
            .any(|p| p[0].record_uuid >= p[1].record_uuid)
    {
        return Err(invalid(Phase::Recover, "player ordering/population"));
    }
    let mut owner = false;
    let mut external = false;
    for player in &manifest.players {
        if !canonical_uuid(&player.profile_uuid)
            || !canonical_uuid(&player.record_uuid)
            || !manifest.dimensions.contains(&player.dimension)
            || ![Dim::Overworld.id(), Dim::Nether.id(), Dim::End.id()]
                .contains(&player.dimension.as_str())
        {
            return Err(invalid(Phase::Recover, "player identity"));
        }
        if player.integrated_owner {
            owner = player.profile_uuid == "00000000-0000-0000-0000-00000000000b"
                && player.record_uuid == uuid_text(OWNER_RECORD);
        } else {
            external = player.profile_uuid == "00000000-0000-0000-0000-00000000000c"
                && player.record_uuid == player.profile_uuid;
        }
    }
    if !owner || !external {
        return Err(invalid(Phase::Recover, "canonical owner mapping"));
    }
    let allowed = [Dim::Overworld.id(), Dim::Nether.id(), Dim::End.id()];
    if manifest.dimensions.is_empty() || manifest.dimensions.len() > 3 {
        return Err(invalid(Phase::Recover, "dimensions"));
    }
    let mut previous = None;
    for dim in &manifest.dimensions {
        let index = allowed
            .iter()
            .position(|d| *d == dim)
            .ok_or_else(|| invalid(Phase::Recover, "unknown dimension"))?;
        if previous.is_some_and(|p| p >= index) {
            return Err(invalid(Phase::Recover, "dimension ordering"));
        }
        previous = Some(index);
    }
    if manifest.files.len() + manifest.directories.len() > ENTRY_LIMIT {
        return Err(limit(Phase::Recover));
    }
    let mut names = BTreeSet::new();
    let mut total = 0u64;
    if manifest.directories.windows(2).any(|p| p[0] >= p[1])
        || manifest.files.windows(2).any(|p| p[0].path >= p[1].path)
    {
        return Err(invalid(Phase::Recover, "member ordering"));
    }
    for dir in &manifest.directories {
        relative(Path::new(dir))?;
        if !names.insert(dir.clone()) {
            return Err(invalid(Phase::Recover, "duplicate directory"));
        }
    }
    for member in &manifest.files {
        relative(Path::new(&member.path))?;
        if !(member.path.starts_with("world/") || member.path == "research/counter.json")
            || !names.insert(member.path.clone())
            || member.bytes > FILE_LIMIT
            || !valid_hash(&member.sha256)
        {
            return Err(invalid(Phase::Recover, "file descriptor"));
        }
        total = total
            .checked_add(member.bytes)
            .ok_or_else(|| limit(Phase::Recover))?;
        if total > IMAGE_LIMIT {
            return Err(limit(Phase::Recover));
        }
    }
    if manifest
        .directories
        .iter()
        .any(|d| !(d == "world" || d.starts_with("world/") || d == "research"))
        || !manifest.directories.iter().any(|d| d == "world")
        || !manifest.directories.iter().any(|d| d == "research")
    {
        return Err(invalid(Phase::Recover, "directory descriptor"));
    }
    for row in &manifest.players {
        for kind in ["data", "advancements", "stats"] {
            if !manifest
                .files
                .iter()
                .any(|m| m.path == player_target(row, kind))
            {
                return Err(invalid(Phase::Recover, "player row absent"));
            }
        }
    }
    if [
        "world/players/data/00000000-0000-0000-0000-00000000000b.dat",
        "world/players/advancements/00000000-0000-0000-0000-00000000000b.json",
        "world/players/stats/00000000-0000-0000-0000-00000000000b.json",
    ]
    .iter()
    .any(|path| names.contains(*path))
    {
        return Err(invalid(
            Phase::Recover,
            "owner profile alias conflicts with mapped row",
        ));
    }
    if names.contains("world/session.lock")
        || !names.contains("research/counter.json")
        || !names.contains("world/level.dat")
    {
        return Err(invalid(
            Phase::Recover,
            "lock/alias/native metadata conflict",
        ));
    }
    Ok(())
}
fn verify_image(root: &Path, epoch: u64, identity: &Identity) -> CResult<Manifest> {
    let bytes = bounded_read(&root.join("manifest.json"), MANIFEST_LIMIT, Phase::Recover)?;
    let manifest: Manifest = parse_json(&bytes, MANIFEST_LIMIT)?;
    validate_manifest(&manifest, epoch, identity)?;
    verify_manifest_members(root, &manifest)?;
    Ok(manifest)
}
fn verify_manifest_members(root: &Path, manifest: &Manifest) -> CResult<()> {
    verify_manifest_physical(root, manifest)?;
    verify_manifest_semantics(root, manifest)
}
fn verify_manifest_physical(root: &Path, manifest: &Manifest) -> CResult<()> {
    let actual = tree(root, &["manifest.json"], Phase::Recover)?;
    if actual.files != manifest.files || actual.directories != manifest.directories {
        return Err(invalid(Phase::Recover, "manifest entry/hash mismatch"));
    }
    Ok(())
}
fn verify_manifest_semantics(root: &Path, manifest: &Manifest) -> CResult<()> {
    for row in &manifest.players {
        let bytes = bounded_read(
            &root.join(player_target(row, "data")),
            2 << 20,
            Phase::Recover,
        )?;
        let tag = strict_player(&bytes)?;
        let uuid = uuid_from_text(&row.record_uuid)
            .ok_or_else(|| invalid(Phase::Recover, "record UUID"))?;
        if tag.get("UUID") != Some(&Tag::IntArray(uuid.to_vec())) {
            return Err(invalid(Phase::Recover, "player record UUID mismatch"));
        }
        for kind in ["advancements", "stats"] {
            let bytes = bounded_read(
                &root.join(player_target(row, kind)),
                JSON_LIMIT,
                Phase::Recover,
            )?;
            let _: Value = parse_json(&bytes, JSON_LIMIT)?;
        }
    }
    let bytes = bounded_read(
        &root.join("research/counter.json"),
        JSON_LIMIT,
        Phase::Recover,
    )?;
    let counter: CounterArchive = parse_json(&bytes, JSON_LIMIT)?;
    if counter.schema != 1
        || counter.kind != "item-free-counter-archive"
        || counter.live_generation_restore
        || !counter.resident
        || counter.runtime_instance == 0
        || counter.save_schema != 1
        || !manifest.dimensions.contains(&counter.scope.dimension)
    {
        return Err(invalid(Phase::Recover, "counter archive descriptor"));
    }
    Ok(())
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryProfile {
    ArchiveOnly,
    CounterOnly,
    ActiveSlot,
    AdvancedSlot,
}
struct CompleteEnvelope {
    manifest: Manifest,
    slot_manifest: Option<Value>,
}
fn check_recovery_profile(
    manifest: &Manifest,
    slot_manifest: Option<&Value>,
    profile: RecoveryProfile,
) -> CResult<()> {
    let has_slot = manifest
        .files
        .iter()
        .any(|member| member.path == SLOT_MEMBER);
    match profile {
        RecoveryProfile::ArchiveOnly | RecoveryProfile::CounterOnly
            if slot_manifest.is_some() || has_slot =>
        {
            return Err(profile_error(
                "selected slot owner requires slot recovery profile",
            ));
        }
        RecoveryProfile::ActiveSlot | RecoveryProfile::AdvancedSlot => {
            let value =
                slot_manifest.ok_or_else(|| profile_error("selected epoch has no slot profile"))?;
            let slot: counter_restore_integration::slot_epoch_integration::SlotManifest =
                serde_json::from_value(value.clone()).map_err(profile_error)?;
            slot.validate(&manifest.identity, manifest.epoch)?;
        }
        _ => {}
    }
    if matches!(
        profile,
        RecoveryProfile::CounterOnly | RecoveryProfile::ActiveSlot | RecoveryProfile::AdvancedSlot
    ) && !manifest
        .files
        .iter()
        .any(|member| member.path == counter_restore_integration::FRAME_PATH)
    {
        return Err(profile_error("selected epoch lacks native counter frame"));
    }
    Ok(())
}
fn verify_complete_envelope(
    root: &Path,
    epoch: u64,
    identity: &Identity,
) -> CResult<CompleteEnvelope> {
    let bytes = bounded_read(&root.join("manifest.json"), MANIFEST_LIMIT, Phase::Recover)?;
    let (mut value, duplicate, known_slot) = envelope_probe(&bytes)?;
    if !known_slot {
        let manifest: Manifest = parse_json(&bytes, MANIFEST_LIMIT)?;
        validate_manifest(&manifest, epoch, identity)?;
        if manifest
            .files
            .iter()
            .any(|member| member.path == SLOT_MEMBER)
        {
            return Err(profile_error(
                "legacy envelope contains reserved slot owner member",
            ));
        }
        verify_manifest_members(root, &manifest)?;
        return Ok(CompleteEnvelope {
            manifest,
            slot_manifest: None,
        });
    }
    // Identity remains a hard refusal even when known-profile fields are invalid.
    let actual: Identity = serde_json::from_value(
        value
            .get("identity")
            .cloned()
            .ok_or_else(|| profile_error("slot identity missing"))?,
    )
    .map_err(profile_error)?;
    actual.validate()?;
    if &actual != identity {
        return Err(CaptureError::new(
            Phase::Recover,
            Failure::Identity,
            None,
            "epoch build/content mismatch",
        ));
    }
    if duplicate {
        return Err(profile_error("duplicate known slot manifest field"));
    }
    if value.get("schema").and_then(Value::as_u64) != Some(2)
        || value.get("kind").and_then(Value::as_str) != Some(SLOT_KIND)
    {
        return Err(profile_error("strict slot schema/kind mismatch"));
    }
    if value.get("slot_path").and_then(Value::as_str) != Some(SLOT_MEMBER) {
        return Err(profile_error("slot_path required exact String"));
    }
    for required in [SLOT_MEMBER, counter_restore_integration::FRAME_PATH] {
        let declared = value
            .get("files")
            .and_then(Value::as_array)
            .is_some_and(|files| {
                files
                    .iter()
                    .any(|member| member.get("path").and_then(Value::as_str) == Some(required))
            });
        if !declared {
            return Err(profile_error(
                "required slot/counter member missing from profile declaration",
            ));
        }
    }
    let raw = value.clone();
    let map = value
        .as_object_mut()
        .ok_or_else(|| profile_error("slot manifest object"))?;
    map.remove("slot_path");
    map.insert("schema".into(), 1.into());
    map.insert("kind".into(), "fresh-native-capture-cell".into());
    let manifest: Manifest = serde_json::from_value(value).map_err(profile_error)?;
    validate_manifest(&manifest, epoch, identity).map_err(|error| {
        if error.code == Failure::Identity {
            error
        } else {
            profile_error(error.detail)
        }
    })?;
    // With coherent profile declarations, missing/hash-bad physical files use
    // ordinary whole-envelope corruption fallback. Once bytes match, PLAYER and
    // archive semantics are ranked refusals; they cannot select an older owner.
    verify_manifest_physical(root, &manifest)?;
    verify_manifest_semantics(root, &manifest).map_err(|error| profile_error(error.detail))?;
    Ok(CompleteEnvelope {
        manifest,
        slot_manifest: Some(raw),
    })
}
fn validate_selected_slot_scalar(root: &Path) -> CResult<()> {
    // Selection is complete. Semantic failure here refuses before copy and
    // cannot retry an older image; envelope verification retains its old contract.
    let frame = horizon_client::world::counter_restore::CounterFrameV2::decode(&bounded_read(
        &root.join(counter_restore_integration::FRAME_PATH),
        16 << 10,
        Phase::Recover,
    )?)
    .map_err(profile_error)?;
    let counter: CounterArchive = parse_json(
        &bounded_read(
            &root.join("research/counter.json"),
            JSON_LIMIT,
            Phase::Recover,
        )?,
        JSON_LIMIT,
    )
    .map_err(|error| profile_error(error.detail))?;
    use horizon_client::world::counter_restore::Placement;
    if !matches!(frame.placement,Placement::Active {counter:3|4,last_advanced:Some(time),..} if time==frame.saved_game_time)
        || frame.last_retired.is_some()
        || !matches!(frame.placement,Placement::Active {counter:value,last_advanced,..} if value==counter.counter&&last_advanced==counter.last_advanced)
    {
        return Err(profile_error("known slot3/4 scalar/diagnostic mismatch"));
    }
    Ok(())
}
fn epoch_name(value: &str, prefix: &str) -> CResult<u64> {
    let suffix = value
        .strip_prefix(prefix)
        .ok_or_else(|| invalid(Phase::Recover, "epoch prefix"))?;
    if suffix.len() != 20 || !suffix.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid(Phase::Recover, "epoch name"));
    }
    let epoch: u64 = suffix.parse().map_err(|e| invalid(Phase::Recover, e))?;
    if epoch == 0 || format!("{epoch:020}") != suffix {
        return Err(invalid(Phase::Recover, "epoch sequence"));
    }
    Ok(epoch)
}
fn select_fixture_epoch(
    archive_root: &Path,
    identity: &Identity,
    destination: &Path,
) -> CResult<SelectedEpoch> {
    select_fixture_epoch_for(
        archive_root,
        identity,
        destination,
        RecoveryProfile::ArchiveOnly,
    )
}
fn select_fixture_epoch_for(
    archive_root: &Path,
    identity: &Identity,
    destination: &Path,
    profile: RecoveryProfile,
) -> CResult<SelectedEpoch> {
    identity.validate()?;
    let archive = owned_dir(archive_root)?;
    ancestors(destination)?;
    let parent = owned_dir(
        destination
            .parent()
            .ok_or_else(|| invalid(Phase::Recover, "destination parent"))?,
    )?;
    if destination
        .try_exists()
        .map_err(|e| io_error(Phase::Recover, destination, e))?
        || parent.starts_with(&archive)
    {
        return Err(invalid(
            Phase::Recover,
            "fresh nonarchive destination required",
        ));
    }
    let mut candidates = Vec::new();
    let mut entries = 0usize;
    for entry in fs::read_dir(&archive).map_err(|e| io_error(Phase::Recover, &archive, e))? {
        entries += 1;
        if entries > ENTRY_LIMIT {
            return Err(limit(Phase::Recover));
        }
        let entry = entry.map_err(|e| io_error(Phase::Recover, &archive, e))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid(Phase::Recover, "non-UTF8 epoch"))?;
        let meta = fs::symlink_metadata(entry.path())
            .map_err(|e| io_error(Phase::Recover, &entry.path(), e))?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(invalid(Phase::Recover, "non-directory epoch"));
        }
        if name.starts_with(".pending-") {
            epoch_name(&name, ".pending-")?;
            continue;
        }
        let epoch = epoch_name(&name, "epoch-")?;
        if candidates.len() >= 16 || candidates.iter().any(|(e, _)| *e == epoch) {
            return Err(limit(Phase::Recover));
        }
        candidates.push((epoch, entry.path()));
    }
    candidates.sort_by_key(|(epoch, _)| *epoch);
    let mut selected = None;
    let mut rejected = Vec::new();
    // Verify ALL published identities; a wrong-build epoch is not silently skipped.
    for (epoch, root) in candidates {
        match verify_complete_envelope(&root, epoch, identity) {
            Ok(envelope) => selected = Some(Ok((root, envelope))),
            Err(error) if error.code == Failure::Identity => return Err(error),
            Err(error) if error.code == Failure::Profile => selected = Some(Err(error)),
            Err(error) => rejected.push((epoch, error.detail)),
        }
    }
    let (root, envelope) =
        selected.ok_or_else(|| invalid(Phase::Recover, "no complete matching epoch"))??;
    check_recovery_profile(&envelope.manifest, envelope.slot_manifest.as_ref(), profile)?;
    if envelope.slot_manifest.is_some() {
        validate_selected_slot_scalar(&root)?;
    }
    Ok(SelectedEpoch {
        root,
        manifest: envelope.manifest,
        slot_manifest: envelope.slot_manifest,
        profile,
        rejected,
        parent,
    })
}
struct SelectedEpoch {
    root: PathBuf,
    manifest: Manifest,
    slot_manifest: Option<Value>,
    profile: RecoveryProfile,
    rejected: Vec<(u64, String)>,
    parent: PathBuf,
}
fn copy_selected_epoch(selected: SelectedEpoch, destination: &Path) -> CResult<RecoveryReceipt> {
    check_recovery_profile(
        &selected.manifest,
        selected.slot_manifest.as_ref(),
        selected.profile,
    )?;
    let SelectedEpoch {
        root,
        manifest,
        rejected,
        parent,
        slot_manifest: _,
        profile: _,
    } = selected;
    fresh_dir(destination, Phase::Recover)?;
    for dir in &manifest.directories {
        if let Some(relative) = dir.strip_prefix("world/") {
            fresh_dir(&destination.join(relative), Phase::Recover)?;
        }
    }
    for member in &manifest.files {
        if let Some(relative) = member.path.strip_prefix("world/") {
            let target = destination.join(relative);
            physical_copy(&root.join(&member.path), &target, member, Phase::Recover)?;
            if file_hash(&target, Phase::Recover)? != (member.bytes, member.sha256.clone()) {
                return Err(invalid(Phase::Recover, "recovery copy mismatch"));
            }
        }
    }
    sync_tree(destination, Phase::Recover)?;
    sync_dir(&parent, Phase::Recover)?;
    let counter: CounterArchive = parse_json(
        &bounded_read(
            &root.join("research/counter.json"),
            JSON_LIMIT,
            Phase::Recover,
        )?,
        JSON_LIMIT,
    )?;
    let lock = SessionLock::acquire(destination)
        .map_err(|e| CaptureError::new(Phase::Recover, Failure::Io, Some(destination), e))?;
    Ok(RecoveryReceipt {
        epoch: manifest.epoch,
        counter,
        rejected,
        _lock: lock,
    })
}
fn recover_fixture_epoch(
    archive_root: &Path,
    identity: &Identity,
    destination: &Path,
) -> CResult<RecoveryReceipt> {
    copy_selected_epoch(
        select_fixture_epoch(archive_root, identity, destination)?,
        destination,
    )
}
#[derive(Clone, Copy)]
enum CaptureProfile {
    ArchivalOneColumn,
    NativeCounterPatch,
    NativeSlotPatch,
    NativeSlotRecapture,
    NativeSlotAdvance,
}
impl Game<'_> {
    fn capture_cell_checks(&self) -> CResult<Vec<String>> {
        self.capture_profile_checks(CaptureProfile::ArchivalOneColumn)
    }
    fn capture_profile_checks(&self, profile: CaptureProfile) -> CResult<Vec<String>> {
        let roster = self.observe_checkpoint_participants().map_err(|e| {
            CaptureError::new(Phase::Validate, Failure::Player, None, format!("{e:?}"))
        })?;
        self.preflight_checkpoint_participants(&roster)
            .map_err(|e| {
                CaptureError::new(Phase::Validate, Failure::Player, None, format!("{e:?}"))
            })?;
        let host = self
            .lan
            .as_ref()
            .ok_or_else(|| invalid(Phase::Validate, "host missing"))?;
        if host.hub.guests.len() != 2
            || host.owner.as_ref().map(|o| o.uuid) != Some(11)
            || self.level.player_uuid != Some(OWNER_RECORD)
            || !host.hub.guests.iter().any(|g| g.uuid == 12)
        {
            return Err(CaptureError::new(
                Phase::Validate,
                Failure::Player,
                None,
                "fixed owner11/external12 cell required",
            ));
        }
        for guest in &host.hub.guests {
            if guest.armor.iter().any(Option::is_some) || guest.offhand.is_some() {
                return Err(CaptureError::new(
                    Phase::Validate,
                    Failure::Player,
                    None,
                    "equipment unsupported",
                ));
            }
            for stack in guest
                .inventory
                .iter()
                .chain(guest.ender_items.iter())
                .flatten()
            {
                if !matches!(
                    &*stack.item,
                    "minecraft:stone"
                        | "minecraft:dirt"
                        | "minecraft:cobblestone"
                        | "minecraft:gravel"
                ) || !(1..=64).contains(&stack.count)
                    || stack.damage != 0
                    || stack.extra.is_some()
                {
                    return Err(CaptureError::new(
                        Phase::Validate,
                        Failure::Player,
                        None,
                        "only authored positive plain roots are supported",
                    ));
                }
            }
            if let Some(tag) = &guest.saved {
                tag_bounds(tag)?;
            }
        }
        if let Some(tag) = &self.level.player_extra {
            tag_bounds(tag)?;
        }
        let entity_owners = match profile {
            CaptureProfile::NativeSlotPatch | CaptureProfile::NativeSlotRecapture => {
                self.observe_slot_epoch_entity_owners()
            }
            CaptureProfile::NativeSlotAdvance => self.observe_slot_advance_entity_owners(),
            _ => self.observe_checkpoint_entity_owners(),
        };
        entity_owners.map_err(|e| {
            CaptureError::new(Phase::Validate, Failure::Entity, None, format!("{e:?}"))
        })?;
        let world_owners = match profile {
            CaptureProfile::NativeSlotRecapture => self.observe_slot_recapture_world_owners(),
            CaptureProfile::NativeSlotAdvance => self.observe_slot_advance_world_owners(),
            _ => self.observe_checkpoint_world_owners(),
        };
        world_owners.map_err(|e| {
            CaptureError::new(Phase::Validate, Failure::World, None, format!("{e:?}"))
        })?;
        if matches!(profile, CaptureProfile::NativeSlotAdvance) && !self.world.modified.is_empty() {
            return Err(profile_error(
                "advanced fixture requires nine clean native columns",
            ));
        }
        let mut dimensions = Vec::new();
        for dimension in [Dim::Overworld, Dim::Nether, Dim::End] {
            let (world, streaming, dropped) = if dimension == self.dim {
                (&self.world, &self.streaming, &self.dropped)
            } else if let Some(level) = self.levels.parked.get(&dimension) {
                (&level.world, &level.streaming, &level.dropped)
            } else {
                continue;
            };
            let shape = match profile {
                CaptureProfile::ArchivalOneColumn => {
                    world.chunks.len() == 1 && world.chunks.contains_key(&(0, 0))
                }
                CaptureProfile::NativeCounterPatch
                | CaptureProfile::NativeSlotPatch
                | CaptureProfile::NativeSlotRecapture
                | CaptureProfile::NativeSlotAdvance => {
                    dimension == Dim::Overworld
                        && self.dim == Dim::Overworld
                        && world.chunks.len() == 9
                        && (-1..=1).all(|z| {
                            (-1..=1).all(|x| world.chunks.get(&(x, z)).is_some_and(|c| c.lit))
                        })
                }
            };
            if !shape || !dropped.is_empty() {
                return Err(CaptureError::new(
                    Phase::Validate,
                    Failure::Context,
                    None,
                    "one resident column and no drops required",
                ));
            }
            streaming.require_pristine_fixture_capture().map_err(|e| {
                CaptureError::new(Phase::Validate, Failure::Streamer, None, format!("{e:?}"))
            })?;
            dimensions.push(dimension.id().into());
        }
        if host
            .hub
            .guests
            .iter()
            .any(|guest| !dimensions.contains(&guest.dimension))
        {
            return Err(CaptureError::new(
                Phase::Validate,
                Failure::Player,
                None,
                "player dimension is not captured",
            ));
        }
        if matches!(
            profile,
            CaptureProfile::NativeCounterPatch
                | CaptureProfile::NativeSlotPatch
                | CaptureProfile::NativeSlotRecapture
                | CaptureProfile::NativeSlotAdvance
        ) {
            if dimensions != [Dim::Overworld.id()] {
                return Err(invalid(Phase::Validate, "only native OW patch supported"));
            }
            for guest in &host.hub.guests {
                let [x, _, z] = guest.position;
                if !x.is_finite()
                    || !z.is_finite()
                    || !self
                        .world
                        .chunks
                        .contains_key(&((x.floor() as i32) >> 4, (z.floor() as i32) >> 4))
                {
                    return Err(invalid(Phase::Validate, "player outside resident patch"));
                }
            }
        }
        Ok(dimensions)
    }
    fn fixture_material(&mut self, request: CaptureRequest) -> CResult<Material> {
        self.profile_material(request, CaptureProfile::ArchivalOneColumn)
    }
    fn profile_material(
        &mut self,
        request: CaptureRequest,
        profile: CaptureProfile,
    ) -> CResult<Material> {
        request.identity.validate()?;
        if request.epoch == 0 {
            return Err(limit(Phase::Validate));
        }
        let archive = owned_dir(&request.archive_root)?;
        let source = owned_dir(&self.world_root)?;
        if archive.starts_with(&source) || source.starts_with(&archive) {
            return Err(invalid(Phase::Validate, "overlapping roots"));
        }
        let dimensions = self.capture_profile_checks(profile)?;
        let mut counter = None;
        for dimension in [Dim::Overworld, Dim::Nether, Dim::End] {
            let world = if dimension == self.dim {
                &self.world
            } else if let Some(level) = self.levels.parked.get(&dimension) {
                &level.world
            } else {
                continue;
            };
            if let Some(snapshot) = world.counter_trial_snapshot() {
                if counter.is_some()
                    || snapshot.fault().is_some()
                    || snapshot.last_retirement().is_some()
                {
                    return Err(invalid(Phase::Project, "one healthy counter required"));
                }
                let config = snapshot.configuration();
                let record = snapshot
                    .record()
                    .ok_or_else(|| invalid(Phase::Project, "counter record absent"))?;
                if !record.resident() || config.scope().dimension() != dimension.id() {
                    return Err(invalid(Phase::Project, "counter scope/residency"));
                }
                counter = Some(CounterArchive {
                    schema: 1,
                    kind: "item-free-counter-archive".into(),
                    scope: ArchiveScope {
                        world_instance: config.scope().world_instance(),
                        dimension: dimension.id().into(),
                    },
                    r#type: config.machine_type().id().into(),
                    save_schema: config.machine_type().save_schema(),
                    position: config.position(),
                    state: ArchiveState {
                        name: config.state().name.clone(),
                        properties: config.state().properties.clone(),
                    },
                    runtime_instance: record.instance(),
                    counter: record.counter(),
                    last_advanced: record.last_advanced(),
                    resident: record.resident(),
                    live_generation_restore: false,
                });
            }
        }
        let counter = counter.ok_or_else(|| invalid(Phase::Project, "counter missing"))?;
        let roster = self.observe_checkpoint_participants().map_err(|e| {
            CaptureError::new(Phase::Project, Failure::Player, None, format!("{e:?}"))
        })?;
        let rows = self
            .project_checkpoint_participants(&roster)
            .map_err(|e| {
                CaptureError::new(Phase::Project, Failure::Player, None, format!("{e:?}"))
            })?
            .into_capture_rows();
        let players = rows
            .into_iter()
            .map(PlayerImage::from_row)
            .collect::<CResult<Vec<_>>>()?;
        let _ = json_bytes(&counter, JSON_LIMIT)?;

        for (folder, extension) in [("data", "dat"), ("advancements", "json"), ("stats", "json")] {
            let alias = self.world_root.join(format!(
                "players/{folder}/00000000-0000-0000-0000-00000000000b.{extension}"
            ));
            if alias
                .try_exists()
                .map_err(|e| io_error(Phase::Validate, &alias, e))?
            {
                return Err(invalid(Phase::Validate, "conflicting owner alias"));
            }
        }
        // Refuse oversized or linked source BEFORE native saving, not after reading it.
        let _ = tree(&source, &["session.lock"], Phase::Validate)?;
        self.save_game_with(true)
            .map_err(|e| CaptureError::new(Phase::Save, Failure::NativeSave, None, e))?;
        self.save
            .flush()
            .map_err(|e| CaptureError::new(Phase::Save, Failure::NativeSave, None, e))?;
        for level in self.levels.parked.values() {
            level
                .save
                .flush()
                .map_err(|e| CaptureError::new(Phase::Save, Failure::NativeSave, None, e))?;
        }
        if self.capture_profile_checks(profile)? != dimensions {
            return Err(invalid(Phase::Save, "loaded dimensions changed"));
        }
        Ok(Material {
            source,
            request,
            players,
            counter,
            dimensions,
        })
    }
    pub(in crate::game) fn capture_fixture_epoch(
        &mut self,
        request: CaptureRequest,
    ) -> CaptureCompletion {
        let mut image = self.fixture_material(request)?.copy()?;
        image.overlay()?;
        image.sync_members()?;
        image.write_manifest_temp()?;
        image.finish_manifest()?;
        image.rename_published()?.acknowledge()
    }
}
#[path = "counter_restore_integration.rs"]
pub(in crate::game) mod counter_restore_integration;
#[path = "capture_integration_tests.rs"]
mod tests;

impl fmt::Debug for RecoveryReceipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecoveryReceipt")
            .field("epoch", &self.epoch)
            .field("counter", &self.counter)
            .field("rejected", &self.rejected)
            .finish_non_exhaustive()
    }
}
fn required_optional_i64<'de, D: de::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<i64>, D::Error> {
    Option::<i64>::deserialize(deserializer)
}
