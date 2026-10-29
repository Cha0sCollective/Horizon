//! Bounded native library slot install. No epoch, Game admission or item codec migration.
use super::{
    World,
    counter_restore::{CounterTarget, NativeBinding, WorldId},
};
use crate::{
    items::{ItemStack, Items},
    world_slot_trial::{self, RestoreAllocation, SlotKey, Trial},
};
use horizon_world::{
    nbt::{self, Compound, JavaString, Limits, NamedTag, Tag},
    registry::BlockStateId,
};
use std::fmt;

const BYTES: usize = 1 << 20;
const NODES: usize = 16384;
const DEPTH: usize = 32;
const STRING: usize = 4096;
const ARRAY: usize = 16384;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotRestoreError {
    InvalidFrame,
    UnsupportedStack,
    CodecLoss,
    Limit,
    IdentityMismatch,
    BindingMismatch,
    ClockMismatch,
    Occupied,
    NotResident,
    StateMismatch,
    PendingRetirement,
    Faulted,
    Detached,
    GenerationExhausted,
}
impl fmt::Display for SlotRestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "native slot restore: {self:?}")
    }
}
impl std::error::Error for SlotRestoreError {}
type Result<T> = std::result::Result<T, SlotRestoreError>;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotStatus {
    Healthy,
    Faulted,
    Detached,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotAllocation {
    Dormant,
    Active {
        slot_generation: u64,
        machine_generation: u64,
    },
    Pending {
        slot_generation: u64,
        machine_generation: u64,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotMetadataSnapshot {
    pub high_water: u64,
    pub allocation: SlotAllocation,
    pub status: SlotStatus,
}
pub(super) type SlotDomain = SlotMetadataSnapshot;
#[derive(Debug)]
struct Header {
    world: WorldId,
    target: CounterTarget,
    package: String,
    type_id: String,
    block_id: String,
    observed: String,
    time: i64,
    machine_head: u64,
    slot_head: u64,
    machine_generation: Option<u64>,
}
/// Unique decoded candidate; neither a native capability nor a cloneable owner.
#[derive(Debug)]
pub struct DecodedSlotFrame {
    header: Box<Header>,
    allocation: Option<RestoreAllocation>,
}
/// Immutable context borrowed from the sole decoded candidate, without runtime authority.
#[derive(Debug)]
pub struct SlotFrameView<'a> {
    pub world: WorldId,
    pub dimension: &'a str,
    pub position: [i32; 3],
    pub package_identity: &'a str,
    pub type_id: &'a str,
    pub block_id: &'a str,
    pub save_schema: u32,
    pub observed_state: &'a str,
    pub saved_game_time: i64,
    pub machine_high_water: u64,
    pub slot_high_water: u64,
    pub allocation: SlotFrameAllocation<'a>,
}
#[derive(Debug)]
pub enum SlotFrameAllocation<'a> {
    Dormant,
    Active {
        slot_generation: u64,
        machine_generation: u64,
        revision: u64,
        stack: Option<&'a ItemStack>,
    },
}
impl DecodedSlotFrame {
    pub fn view(&self) -> SlotFrameView<'_> {
        let allocation = match self.allocation.as_ref() {
            Some(value) => SlotFrameAllocation::Active {
                slot_generation: value.generation,
                // The private decoder validates this pair before constructing a frame.
                machine_generation: self
                    .header
                    .machine_generation
                    .expect("validated active slot generation"),
                revision: value.revision,
                stack: value.stack.as_ref(),
            },
            None => SlotFrameAllocation::Dormant,
        };
        SlotFrameView {
            world: self.header.world,
            dimension: &self.header.target.dimension,
            position: self.header.target.position,
            package_identity: &self.header.package,
            type_id: &self.header.type_id,
            block_id: &self.header.block_id,
            save_schema: 1,
            observed_state: &self.header.observed,
            saved_game_time: self.header.time,
            machine_high_water: self.header.machine_head,
            slot_high_water: self.header.slot_head,
            allocation,
        }
    }
}
#[derive(Debug)]
pub struct RejectedSlotFrame {
    pub error: SlotRestoreError,
    pub frame: DecodedSlotFrame,
}
impl fmt::Display for RejectedSlotFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for RejectedSlotFrame {}
fn limits() -> Limits {
    Limits {
        max_bytes: BYTES,
        max_nodes: NODES,
        max_depth: DEPTH,
    }
}
fn hex(bytes: [u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn text(value: &str) -> Tag {
    Tag::String(value.into())
}
fn unsigned(value: u64) -> Tag {
    Tag::ByteArray(value.to_be_bytes().to_vec())
}
fn fields<'a>(tag: &'a Tag, names: &[&str]) -> Result<&'a Compound> {
    let Tag::Compound(map) = tag else {
        return Err(SlotRestoreError::InvalidFrame);
    };
    if map.len() != names.len()
        || names
            .iter()
            .any(|name| !map.contains_key(&JavaString::from(*name)))
    {
        return Err(SlotRestoreError::InvalidFrame);
    }
    Ok(map)
}
fn field<'a>(tag: &'a Tag, name: &str) -> Result<&'a Tag> {
    tag.get(name).ok_or(SlotRestoreError::InvalidFrame)
}
fn read_text(tag: &Tag) -> Result<String> {
    let Tag::String(value) = tag else {
        return Err(SlotRestoreError::InvalidFrame);
    };
    value
        .to_string()
        .map_err(|_| SlotRestoreError::UnsupportedStack)
}
fn read_unsigned(tag: &Tag) -> Result<u64> {
    let Tag::ByteArray(bytes) = tag else {
        return Err(SlotRestoreError::InvalidFrame);
    };
    Ok(u64::from_be_bytes(
        bytes
            .as_slice()
            .try_into()
            .map_err(|_| SlotRestoreError::InvalidFrame)?,
    ))
}

/// Allocation-free preflight of the actual native wire, including mixed-list wrappers.
struct Wire<'a> {
    bytes: &'a [u8],
    offset: usize,
    nodes: usize,
}
impl<'a> Wire<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(SlotRestoreError::Limit)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(SlotRestoreError::InvalidFrame)?;
        self.offset = end;
        Ok(value)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn count(&mut self) -> Result<usize> {
        let count = i32::from_be_bytes(self.take(4)?.try_into().unwrap());
        usize::try_from(count).map_err(|_| SlotRestoreError::InvalidFrame)
    }
    fn string(&mut self) -> Result<()> {
        let len = u16::from_be_bytes(self.take(2)?.try_into().unwrap()) as usize;
        let bytes = self.take(len)?;
        let mut offset = 0;
        let mut units = 0;
        let mut high = false;
        while offset < bytes.len() {
            let first = bytes[offset];
            let (unit, width) = match first {
                1..=127 => (u16::from(first), 1),
                0xc0..=0xdf if offset + 1 < bytes.len() && bytes[offset + 1] & 0xc0 == 0x80 => (
                    ((u16::from(first & 31)) << 6) | u16::from(bytes[offset + 1] & 63),
                    2,
                ),
                0xe0..=0xef
                    if offset + 2 < bytes.len()
                        && bytes[offset + 1] & 0xc0 == 0x80
                        && bytes[offset + 2] & 0xc0 == 0x80 =>
                {
                    (
                        ((u16::from(first & 15)) << 12)
                            | ((u16::from(bytes[offset + 1] & 63)) << 6)
                            | u16::from(bytes[offset + 2] & 63),
                        3,
                    )
                }
                _ => return Err(SlotRestoreError::InvalidFrame),
            };
            if (width == 2 && unit != 0 && unit < 128) || (width == 3 && unit < 2048) {
                return Err(SlotRestoreError::InvalidFrame);
            }
            if high {
                if !(0xdc00..=0xdfff).contains(&unit) {
                    return Err(SlotRestoreError::UnsupportedStack);
                }
                high = false;
            } else if (0xd800..=0xdbff).contains(&unit) {
                high = true;
            } else if (0xdc00..=0xdfff).contains(&unit) {
                return Err(SlotRestoreError::UnsupportedStack);
            }
            units += 1;
            if units > STRING {
                return Err(SlotRestoreError::Limit);
            }
            offset += width;
        }
        if high {
            return Err(SlotRestoreError::UnsupportedStack);
        }
        Ok(())
    }
    fn payload(&mut self, id: u8, depth: usize) -> Result<()> {
        if self.nodes >= NODES || (matches!(id, 9 | 10) && depth >= DEPTH) {
            return Err(SlotRestoreError::Limit);
        }
        self.nodes += 1;
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
                if !f32::from_bits(bits).is_finite() || bits == 0x80000000 {
                    return Err(SlotRestoreError::UnsupportedStack);
                }
            }
            6 => {
                let bits = u64::from_be_bytes(self.take(8)?.try_into().unwrap());
                if !f64::from_bits(bits).is_finite() || bits == 0x8000000000000000 {
                    return Err(SlotRestoreError::UnsupportedStack);
                }
            }
            7 | 11 | 12 => {
                let count = self.count()?;
                if count > ARRAY {
                    return Err(SlotRestoreError::Limit);
                }
                let width = match id {
                    7 => 1,
                    11 => 4,
                    _ => 8,
                };
                self.take(count * width)?;
            }
            8 => self.string()?,
            9 => {
                let element = self.byte()?;
                let count = self.count()?;
                if count > NODES.saturating_sub(self.nodes) {
                    return Err(SlotRestoreError::Limit);
                }
                if count > 0 && !(1..=12).contains(&element) {
                    return Err(SlotRestoreError::InvalidFrame);
                }
                for _ in 0..count {
                    self.payload(element, depth + 1)?;
                }
            }
            10 => loop {
                let child = self.byte()?;
                if child == 0 {
                    break;
                }
                self.string()?;
                self.payload(child, depth + 1)?;
            },
            _ => return Err(SlotRestoreError::InvalidFrame),
        }
        Ok(())
    }
}
fn preflight(bytes: &[u8]) -> Result<()> {
    if bytes.len() > BYTES {
        return Err(SlotRestoreError::Limit);
    }
    let mut wire = Wire {
        bytes,
        offset: 0,
        nodes: 0,
    };
    if wire.byte()? != 10 {
        return Err(SlotRestoreError::InvalidFrame);
    }
    wire.string()?;
    wire.payload(10, 0)?;
    if wire.offset != bytes.len() {
        return Err(SlotRestoreError::InvalidFrame);
    }
    Ok(())
}

/// Conservative native-object budget before any item encoder builds a temporary Tag.
/// The subsequent wire budget counts exact synthetic wrappers and bytes too.
struct NativeBudget {
    nodes: usize,
    bytes: usize,
}
impl NativeBudget {
    fn charge(&mut self, nodes: usize, bytes: usize) -> Result<()> {
        self.nodes = self
            .nodes
            .checked_add(nodes)
            .ok_or(SlotRestoreError::Limit)?;
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or(SlotRestoreError::Limit)?;
        if self.nodes > NODES || self.bytes > BYTES {
            Err(SlotRestoreError::Limit)
        } else {
            Ok(())
        }
    }
    fn string(&mut self, value: &str) -> Result<()> {
        let units = value.encode_utf16().count();
        if units > STRING {
            return Err(SlotRestoreError::Limit);
        }
        self.charge(1, units * 3 + 4)
    }
    fn tag(&mut self, value: &Tag, depth: usize) -> Result<()> {
        if depth >= DEPTH {
            return Err(SlotRestoreError::Limit);
        }
        self.charge(1, 16)?;
        match value {
            Tag::End => return Err(SlotRestoreError::UnsupportedStack),
            Tag::Float(bits) if !f32::from_bits(*bits).is_finite() || *bits == 0x80000000 => {
                return Err(SlotRestoreError::UnsupportedStack);
            }
            Tag::Double(bits)
                if !f64::from_bits(*bits).is_finite() || *bits == 0x8000000000000000 =>
            {
                return Err(SlotRestoreError::UnsupportedStack);
            }
            Tag::String(value) => {
                if value.0.len() > STRING {
                    return Err(SlotRestoreError::Limit);
                }
                if char::decode_utf16(value.0.iter().copied()).any(|c| c.is_err()) {
                    return Err(SlotRestoreError::UnsupportedStack);
                }
                self.charge(0, value.0.len() * 3)?;
            }
            Tag::ByteArray(values) => {
                if values.len() > ARRAY {
                    return Err(SlotRestoreError::Limit);
                }
                self.charge(0, values.len())?;
            }
            Tag::IntArray(values) => {
                if values.len() > ARRAY {
                    return Err(SlotRestoreError::Limit);
                }
                self.charge(0, values.len() * 4)?;
            }
            Tag::LongArray(values) => {
                if values.len() > ARRAY {
                    return Err(SlotRestoreError::Limit);
                }
                self.charge(0, values.len() * 8)?;
            }
            Tag::List(values) => {
                if values.len() > NODES {
                    return Err(SlotRestoreError::Limit);
                }
                for value in values {
                    // Mixed native lists may insert a compound wrapper for every element.
                    self.charge(1, 4)?;
                    self.tag(value, depth + 2)?;
                }
            }
            Tag::Compound(values) => {
                if values.len() > NODES {
                    return Err(SlotRestoreError::Limit);
                }
                for (key, value) in values {
                    if key.0.len() > STRING {
                        return Err(SlotRestoreError::Limit);
                    }
                    if char::decode_utf16(key.0.iter().copied()).any(|c| c.is_err()) {
                        return Err(SlotRestoreError::UnsupportedStack);
                    }
                    self.charge(0, key.0.len() * 3 + 3)?;
                    self.tag(value, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn explosion(&mut self, value: &horizon_items::firework::FireworkExplosion) -> Result<()> {
        for colors in [&value.colors, &value.fade] {
            if colors.len() > ARRAY {
                return Err(SlotRestoreError::Limit);
            }
            self.charge(colors.len() + 8, colors.len() * 8 + 128)?;
        }
        Ok(())
    }
    fn stack(&mut self, stack: &ItemStack, items: &Items, depth: usize) -> Result<()> {
        if depth + 1 >= DEPTH {
            return Err(SlotRestoreError::Limit);
        }
        if stack.count == 0
            || stack.count > i32::MAX as u32
            || stack.damage > i32::MAX as u32
            || &*stack.item == "minecraft:air"
            || items.get(&stack.item).is_none()
            || CounterTarget::new(&stack.item, [0, 0, 0]).is_err()
        {
            return Err(SlotRestoreError::UnsupportedStack);
        }
        self.charge(128, 4096)?;
        self.string(&stack.item)?;
        let Some(data) = stack.data() else {
            return Ok(());
        };
        if data.bundle_selected.0 != -1 || data.repair_cost > i32::MAX as u32 {
            return Err(SlotRestoreError::UnsupportedStack);
        }
        for levels in [&data.enchantments, &data.stored_enchantments] {
            if levels.len() > NODES {
                return Err(SlotRestoreError::Limit);
            }
            for (id, _) in levels {
                self.string(id)?;
                self.charge(2, 16)?;
            }
        }
        if let Some(name) = &data.custom_name {
            self.string(name)?;
        }
        if let Some(potion) = &data.potion {
            self.string(potion)?;
        }
        if let Some((material, pattern)) = &data.trim {
            self.string(material)?;
            self.string(pattern)?;
        }
        if let Some(color) = &data.base_color {
            self.string(color)?;
        }
        for pages in [
            data.writable_pages.as_slice(),
            data.written
                .as_ref()
                .map_or(&[][..], |book| book.pages.as_slice()),
        ] {
            if pages.len() > NODES {
                return Err(SlotRestoreError::Limit);
            }
            if !pages.is_empty() {
                // Native book content is a compound containing a pages list.
                // Each page is another compound { raw: String }, not just String.
                if depth + 4 >= DEPTH {
                    return Err(SlotRestoreError::Limit);
                }
                self.charge(2, 96)?;
            }
            for page in pages {
                self.charge(1, 8)?;
                self.string(page)?;
            }
        }
        if let Some(book) = &data.written {
            // Also cover the written-book compound/title filterable wrapper,
            // including the page_tags branch and books with no pages.
            self.charge(3, 96)?;
            self.string(&book.title)?;
            self.string(&book.author)?;
            if book.page_tags.len() > NODES {
                return Err(SlotRestoreError::Limit);
            }
            for tag in &book.page_tags {
                self.tag(tag, depth + 5)?;
            }
        }
        if data.banner_patterns.len() > NODES
            || data.stew_effects.len() > NODES
            || data.other.len() > NODES
        {
            return Err(SlotRestoreError::Limit);
        }
        for layer in &data.banner_patterns {
            self.string(&layer.pattern)?;
            self.string(&layer.color)?;
            self.charge(4, 128)?;
        }
        for (id, _) in &data.stew_effects {
            self.string(id)?;
            self.charge(4, 128)?;
        }
        if let Some((_, explosions)) = &data.fireworks {
            if explosions.len() > NODES {
                return Err(SlotRestoreError::Limit);
            }
            for value in explosions {
                self.explosion(value)?;
            }
        }
        if let Some(explosion) = &data.firework_explosion {
            self.explosion(explosion)?;
        }
        for (id, tag) in &data.other {
            self.string(id)?;
            self.tag(tag, depth + 2)?;
        }
        if data.container.len() > NODES
            || data.charged.len() > NODES
            || data.bundle_contents.len() > NODES
        {
            return Err(SlotRestoreError::Limit);
        }
        for (_, child) in &data.container {
            self.stack(child, items, depth + 4)?;
        }
        for child in data.charged.iter().chain(&data.bundle_contents) {
            self.stack(child, items, depth + 4)?;
        }
        if let Some(sides) = &data.pot_decorations {
            for child in sides.iter().flatten() {
                self.stack(child, items, depth + 4)?;
            }
        }
        Ok(())
    }
}
fn check_stack(stack: &ItemStack, items: &Items) -> Result<Tag> {
    NativeBudget { nodes: 0, bytes: 0 }.stack(stack, items, 3)?;
    let tag = horizon_items::item_nbt::item_to_nbt(stack);
    let bytes = write_tag(&tag)?;
    let decoded =
        horizon_items::item_nbt::item_from_nbt(&tag).ok_or(SlotRestoreError::CodecLoss)?;
    NativeBudget { nodes: 0, bytes: 0 }.stack(&decoded, items, 3)?;
    if &decoded != stack || horizon_items::item_nbt::item_to_nbt(&decoded) != tag {
        return Err(SlotRestoreError::CodecLoss);
    }
    let (roundtrip, used) =
        nbt::read_named(&bytes, limits()).map_err(|_| SlotRestoreError::InvalidFrame)?;
    if used != bytes.len() || roundtrip.tag != tag {
        return Err(SlotRestoreError::CodecLoss);
    }
    Ok(tag)
}
fn write_tag(tag: &Tag) -> Result<Vec<u8>> {
    let bytes = nbt::write_named(
        &NamedTag {
            name: JavaString::default(),
            tag: tag.clone(),
        },
        limits(),
    )
    .map_err(|_| SlotRestoreError::Limit)?;
    preflight(&bytes)?;
    Ok(bytes)
}

struct ValueRef<'a> {
    generation: u64,
    revision: u64,
    stack: Option<&'a ItemStack>,
}
fn encode_frame(header: &Header, value: Option<ValueRef<'_>>, items: &Items) -> Result<Vec<u8>> {
    let ownership = if let Some(value) = value {
        let stack = if let Some(stack) = value.stack {
            Tag::compound([
                ("kind", text("some")),
                ("value", check_stack(stack, items)?),
            ])
        } else {
            Tag::compound([("kind", text("none"))])
        };
        Tag::compound([
            ("kind", text("active")),
            ("slot_generation", unsigned(value.generation)),
            (
                "machine_generation",
                unsigned(
                    header
                        .machine_generation
                        .ok_or(SlotRestoreError::InvalidFrame)?,
                ),
            ),
            ("revision", unsigned(value.revision)),
            ("stack", stack),
        ])
    } else {
        Tag::compound([("kind", text("dormant"))])
    };
    let root = Tag::compound([
        ("format", Tag::Int(1)),
        ("kind", text("native-slot-checkpoint")),
        ("world_uuid", text(&header.world.to_string())),
        ("dimension", text(&header.target.dimension)),
        ("position", Tag::IntArray(header.target.position.to_vec())),
        ("package_identity", text(&header.package)),
        ("type_id", text(&header.type_id)),
        ("save_schema", Tag::Int(1)),
        ("block_id", text(&header.block_id)),
        ("template", text("minecraft:iron_block")),
        ("properties", Tag::compound([] as [(&str, Tag); 0])),
        (
            "observed_state",
            Tag::compound([
                ("name", text(&header.observed)),
                ("properties", Tag::compound([] as [(&str, Tag); 0])),
            ]),
        ),
        ("saved_game_time", Tag::Long(header.time)),
        ("machine_high_water", unsigned(header.machine_head)),
        ("slot_high_water", unsigned(header.slot_head)),
        ("ownership", ownership),
    ]);
    write_tag(&root)
}
impl Header {
    fn validate(&self, allocation: Option<&RestoreAllocation>) -> Result<()> {
        CounterTarget::new(&self.target.dimension, self.target.position)
            .map_err(|_| SlotRestoreError::InvalidFrame)?;
        for id in [&self.type_id, &self.block_id, &self.observed] {
            CounterTarget::new(id, [0, 0, 0]).map_err(|_| SlotRestoreError::InvalidFrame)?;
        }
        if self.time < 0
            || self.package.len() != 64
            || !self
                .package
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            || self.type_id.split_once(':').map(|(owner, _)| owner)
                != self.block_id.split_once(':').map(|(owner, _)| owner)
        {
            return Err(SlotRestoreError::InvalidFrame);
        }
        match (allocation, self.machine_generation) {
            (Some(value), Some(machine))
                if value.generation > 0
                    && value.generation <= self.slot_head
                    && machine > 0
                    && machine <= self.machine_head
                    && self.observed == "minecraft:iron_block" =>
            {
                Ok(())
            }
            (None, None) if self.observed != "minecraft:iron_block" => Ok(()),
            _ => Err(SlotRestoreError::InvalidFrame),
        }
    }
}
pub fn decode_slot_frame(bytes: &[u8], items: &Items) -> Result<DecodedSlotFrame> {
    preflight(bytes)?;
    let (root, used) =
        nbt::read_named(bytes, limits()).map_err(|_| SlotRestoreError::InvalidFrame)?;
    if used != bytes.len() || !root.name.0.is_empty() || write_tag(&root.tag)? != bytes {
        return Err(SlotRestoreError::InvalidFrame);
    }
    let tag = &root.tag;
    fields(
        tag,
        &[
            "format",
            "kind",
            "world_uuid",
            "dimension",
            "position",
            "package_identity",
            "type_id",
            "save_schema",
            "block_id",
            "template",
            "properties",
            "observed_state",
            "saved_game_time",
            "machine_high_water",
            "slot_high_water",
            "ownership",
        ],
    )?;
    if field(tag, "format")? != &Tag::Int(1)
        || read_text(field(tag, "kind")?)? != "native-slot-checkpoint"
        || field(tag, "save_schema")? != &Tag::Int(1)
        || read_text(field(tag, "template")?)? != "minecraft:iron_block"
    {
        return Err(SlotRestoreError::InvalidFrame);
    }
    fields(field(tag, "properties")?, &[])?;
    let observed = field(tag, "observed_state")?;
    fields(observed, &["name", "properties"])?;
    fields(field(observed, "properties")?, &[])?;
    let position = match field(tag, "position")? {
        Tag::IntArray(value) => value
            .as_slice()
            .try_into()
            .map_err(|_| SlotRestoreError::InvalidFrame)?,
        _ => return Err(SlotRestoreError::InvalidFrame),
    };
    let time = match field(tag, "saved_game_time")? {
        Tag::Long(value) => *value,
        _ => return Err(SlotRestoreError::InvalidFrame),
    };
    let mut header = Header {
        world: WorldId::parse(&read_text(field(tag, "world_uuid")?)?)
            .map_err(|_| SlotRestoreError::InvalidFrame)?,
        target: CounterTarget::new(&read_text(field(tag, "dimension")?)?, position)
            .map_err(|_| SlotRestoreError::InvalidFrame)?,
        package: read_text(field(tag, "package_identity")?)?,
        type_id: read_text(field(tag, "type_id")?)?,
        block_id: read_text(field(tag, "block_id")?)?,
        observed: read_text(field(observed, "name")?)?,
        time,
        machine_head: read_unsigned(field(tag, "machine_high_water")?)?,
        slot_head: read_unsigned(field(tag, "slot_high_water")?)?,
        machine_generation: None,
    };
    let ownership = field(tag, "ownership")?;
    let allocation = match read_text(field(ownership, "kind")?)?.as_str() {
        "dormant" => {
            fields(ownership, &["kind"])?;
            None
        }
        "pending" => return Err(SlotRestoreError::PendingRetirement),
        "active" => {
            fields(
                ownership,
                &[
                    "kind",
                    "slot_generation",
                    "machine_generation",
                    "revision",
                    "stack",
                ],
            )?;
            header.machine_generation =
                Some(read_unsigned(field(ownership, "machine_generation")?)?);
            let wrapper = field(ownership, "stack")?;
            let stack = match read_text(field(wrapper, "kind")?)?.as_str() {
                "none" => {
                    fields(wrapper, &["kind"])?;
                    None
                }
                "some" => {
                    fields(wrapper, &["kind", "value"])?;
                    let value = field(wrapper, "value")?;
                    let names = if value.get("components").is_some() {
                        &["id", "count", "components"][..]
                    } else {
                        &["id", "count"][..]
                    };
                    fields(value, names)?;
                    if !matches!(field(value, "id")?, Tag::String(_))
                        || !matches!(field(value,"count")?,Tag::Int(count) if *count>0)
                    {
                        return Err(SlotRestoreError::UnsupportedStack);
                    }
                    if let Some(components) = value.get("components")
                        && !matches!(components, Tag::Compound(_))
                    {
                        return Err(SlotRestoreError::InvalidFrame);
                    }
                    let stack = horizon_items::item_nbt::item_from_nbt(value)
                        .ok_or(SlotRestoreError::UnsupportedStack)?;
                    if check_stack(&stack, items)? != *value {
                        return Err(SlotRestoreError::CodecLoss);
                    }
                    Some(stack)
                }
                _ => return Err(SlotRestoreError::InvalidFrame),
            };
            Some(RestoreAllocation {
                generation: read_unsigned(field(ownership, "slot_generation")?)?,
                revision: read_unsigned(field(ownership, "revision")?)?,
                stack,
            })
        }
        _ => return Err(SlotRestoreError::InvalidFrame),
    };
    header.validate(allocation.as_ref())?;
    Ok(DecodedSlotFrame {
        header: Box::new(header),
        allocation,
    })
}

fn native_error(error: super::counter_restore::RestoreError) -> SlotRestoreError {
    use super::counter_restore::RestoreError as E;
    match error {
        E::Detached => SlotRestoreError::Detached,
        E::Faulted => SlotRestoreError::Faulted,
        E::NotResident => SlotRestoreError::NotResident,
        _ => SlotRestoreError::BindingMismatch,
    }
}
impl World {
    pub(crate) fn has_persistent_slot_mode(&self) -> bool {
        self.counter_domain
            .as_ref()
            .is_some_and(|domain| domain.slot.is_some())
    }
    pub fn persistent_world_slot_metadata(&self) -> Option<SlotMetadataSnapshot> {
        self.counter_domain.as_ref()?.slot
    }
    pub(crate) fn detach_persistent_slot(&mut self) {
        if let Some(slot) = self
            .counter_domain
            .as_mut()
            .and_then(|domain| domain.slot.as_mut())
        {
            slot.status = SlotStatus::Detached;
        }
    }
    fn checked_slot_anchor(&self) -> Result<super::counter_restore::SlotAnchor<'_>> {
        let domain = self
            .counter_domain
            .as_ref()
            .ok_or(SlotRestoreError::BindingMismatch)?;
        let anchor = domain.slot_anchor().map_err(native_error)?;
        let counter = self
            .counter_trial
            .as_ref()
            .ok_or(SlotRestoreError::Detached)?
            .snapshot();
        if counter.fault().is_some() {
            return Err(SlotRestoreError::Faulted);
        }
        if counter.record().is_some_and(|record| {
            record
                .last_advanced()
                .is_some_and(|time| time < 0 || time > self.game_time)
        }) || counter.last_retirement().is_some_and(|retired| {
            retired.game_time() < 0
                || retired.game_time() > self.game_time
                || retired
                    .record()
                    .last_advanced()
                    .is_some_and(|time| time < 0 || time > retired.game_time())
        }) {
            return Err(SlotRestoreError::ClockMismatch);
        }
        if !domain.configuration_matches(counter.configuration()) {
            return Err(SlotRestoreError::BindingMismatch);
        }
        let [x, y, z] = anchor.target.position;
        let current = self
            .chunks
            .get(&(x >> 4, z >> 4))
            .ok_or(SlotRestoreError::NotResident)?
            .block(x & 15, y, z & 15);
        let state = anchor
            .binding
            .physical_state(current)
            .ok_or(SlotRestoreError::StateMismatch)?;
        if !state.properties.is_empty() {
            return Err(SlotRestoreError::StateMismatch);
        }
        match (anchor.generation, counter.record()) {
            (Some(_), Some(record))
                if record.resident() && current == anchor.binding.state_id() => {}
            (Some(_), Some(record)) if !record.resident() => {
                return Err(SlotRestoreError::NotResident);
            }
            (None, None) if current != anchor.binding.state_id() => {}
            _ => return Err(SlotRestoreError::StateMismatch),
        }
        if self.game_time < 0 {
            return Err(SlotRestoreError::ClockMismatch);
        }
        Ok(anchor)
    }
    pub(crate) fn persistent_slot_guard(&self) -> std::result::Result<(), world_slot_trial::Error> {
        use world_slot_trial::{Error, Fault};
        let Some(metadata) = self.persistent_world_slot_metadata() else {
            return Ok(());
        };
        match metadata.status {
            SlotStatus::Detached => return Err(Error::Detached),
            SlotStatus::Faulted => return Err(Error::Fault(Fault::PersistentDomain)),
            _ => {}
        }
        let anchor = self.checked_slot_anchor().map_err(|error| match error {
            SlotRestoreError::Detached => Error::Detached,
            SlotRestoreError::NotResident => Error::NotResident,
            SlotRestoreError::StateMismatch => Error::StateMismatch,
            _ => Error::Fault(Fault::PersistentDomain),
        })?;
        let view = self
            .world_slot_trial
            .as_ref()
            .ok_or(Error::Detached)?
            .borrowed();
        if view.pending {
            return Err(Error::PendingRetirement);
        }
        if let Some(fault) = view.fault {
            return Err(Error::Fault(fault));
        }
        let domain = self.counter_domain.as_ref().unwrap();
        if view.high_water != metadata.high_water
            || !domain.configuration_matches(view.configuration)
            || view.configuration.scope()
                != self.counter_trial.as_ref().unwrap().configuration().scope()
        {
            return Err(Error::Fault(Fault::PersistentDomain));
        }
        match (metadata.allocation, view.active) {
            (SlotAllocation::Dormant, None) if anchor.generation.is_none() => Ok(()),
            (
                SlotAllocation::Active {
                    slot_generation,
                    machine_generation,
                },
                Some(value),
            ) if slot_generation > 0
                && slot_generation <= metadata.high_water
                && value.key.incarnation() == slot_generation
                && anchor.generation == Some(machine_generation) =>
            {
                if value.resident {
                    Ok(())
                } else {
                    Err(Error::NotResident)
                }
            }
            _ => Err(Error::Fault(Fault::PersistentDomain)),
        }
    }
    fn validate_slot_target(
        &self,
        binding: &NativeBinding,
        expected_world: WorldId,
        target: &CounterTarget,
    ) -> Result<super::counter_restore::SlotAnchor<'_>> {
        let anchor = self.checked_slot_anchor()?;
        CounterTarget::new(&target.dimension, target.position)
            .map_err(|_| SlotRestoreError::IdentityMismatch)?;
        if anchor.world != expected_world || anchor.target != target {
            return Err(SlotRestoreError::IdentityMismatch);
        }
        if anchor.binding.package_identity() != binding.package_identity()
            || anchor.binding.type_id() != binding.type_id()
            || anchor.binding.block_id() != binding.block_id()
            || anchor.binding.state_id() != binding.state_id()
        {
            return Err(SlotRestoreError::BindingMismatch);
        }
        Ok(anchor)
    }
    pub fn initialize_persistent_world_slot(
        &mut self,
        binding: &NativeBinding,
        target: &CounterTarget,
    ) -> Result<Option<SlotKey>> {
        if self.world_slot_trial.is_some() || self.has_persistent_slot_mode() {
            return Err(SlotRestoreError::Occupied);
        }
        let world = self.checked_slot_anchor()?.world;
        let anchor = self.validate_slot_target(binding, world, target)?;
        let allocation = anchor
            .generation
            .map(|machine| SlotAllocation::Active {
                slot_generation: 1,
                machine_generation: machine,
            })
            .unwrap_or(SlotAllocation::Dormant);
        let mut input = anchor.generation.map(|_| RestoreAllocation {
            generation: 1,
            revision: 0,
            stack: None,
        });
        let head = u64::from(input.is_some());
        let configuration = self.counter_trial.as_ref().unwrap().configuration().clone();
        let trial = Box::new(
            Trial::restore_checked(configuration, head, &mut input)
                .map_err(|_| SlotRestoreError::GenerationExhausted)?,
        );
        let key = trial.borrowed().active.map(|value| value.key.clone());
        self.counter_domain.as_mut().unwrap().slot = Some(SlotDomain {
            high_water: head,
            allocation,
            status: SlotStatus::Healthy,
        });
        self.world_slot_trial = Some(trial);
        Ok(key)
    }
    pub fn restore_persistent_world_slot(
        &mut self,
        binding: &NativeBinding,
        items: &Items,
        expected_world: WorldId,
        target: &CounterTarget,
        mut frame: DecodedSlotFrame,
    ) -> std::result::Result<Option<SlotKey>, RejectedSlotFrame> {
        let validate = || -> Result<()> {
            if self.world_slot_trial.is_some() || self.has_persistent_slot_mode() {
                return Err(SlotRestoreError::Occupied);
            }
            frame.header.validate(frame.allocation.as_ref())?;
            let anchor = self.validate_slot_target(binding, expected_world, target)?;
            let header = &frame.header;
            if header.world != expected_world
                || &header.target != target
                || header.machine_head != anchor.high_water
                || header.machine_generation != anchor.generation
            {
                return Err(SlotRestoreError::IdentityMismatch);
            }
            if header.package != hex(binding.package_identity())
                || header.type_id != binding.type_id()
                || header.block_id != binding.block_id()
            {
                return Err(SlotRestoreError::BindingMismatch);
            }
            if self.game_time != header.time {
                return Err(SlotRestoreError::ClockMismatch);
            }
            let [x, y, z] = target.position;
            let current = self
                .chunks
                .get(&(x >> 4, z >> 4))
                .ok_or(SlotRestoreError::NotResident)?
                .block(x & 15, y, z & 15);
            let state = anchor
                .binding
                .physical_state(current)
                .ok_or(SlotRestoreError::StateMismatch)?;
            if state.name != header.observed || !state.properties.is_empty() {
                return Err(SlotRestoreError::StateMismatch);
            }
            if let Some(stack) = frame
                .allocation
                .as_ref()
                .and_then(|value| value.stack.as_ref())
            {
                check_stack(stack, items)?;
            }
            Ok(())
        };
        if let Err(error) = validate() {
            return Err(RejectedSlotFrame { error, frame });
        }
        let metadata = SlotDomain {
            high_water: frame.header.slot_head,
            status: SlotStatus::Healthy,
            allocation: frame
                .allocation
                .as_ref()
                .map(|value| SlotAllocation::Active {
                    slot_generation: value.generation,
                    machine_generation: frame.header.machine_generation.unwrap(),
                })
                .unwrap_or(SlotAllocation::Dormant),
        };
        let configuration = self.counter_trial.as_ref().unwrap().configuration().clone();
        let trial =
            match Trial::restore_checked(configuration, metadata.high_water, &mut frame.allocation)
            {
                Ok(trial) => Box::new(trial),
                Err(_) => {
                    return Err(RejectedSlotFrame {
                        error: SlotRestoreError::GenerationExhausted,
                        frame,
                    });
                }
            };
        let key = trial.borrowed().active.map(|value| value.key.clone());
        self.counter_domain.as_mut().unwrap().slot = Some(metadata);
        self.world_slot_trial = Some(trial);
        Ok(key)
    }
    pub fn slot_checkpoint_bytes(&self, items: &Items, saved_game_time: i64) -> Result<Vec<u8>> {
        let metadata = self
            .persistent_world_slot_metadata()
            .ok_or(SlotRestoreError::BindingMismatch)?;
        match metadata.status {
            SlotStatus::Detached => return Err(SlotRestoreError::Detached),
            SlotStatus::Faulted => return Err(SlotRestoreError::Faulted),
            _ => {}
        }
        let trial = self
            .world_slot_trial
            .as_ref()
            .ok_or(SlotRestoreError::Detached)?
            .borrowed();
        if trial.pending {
            return Err(SlotRestoreError::PendingRetirement);
        }
        self.persistent_slot_guard().map_err(|error| match error {
            world_slot_trial::Error::NotResident => SlotRestoreError::NotResident,
            world_slot_trial::Error::Detached => SlotRestoreError::Detached,
            world_slot_trial::Error::PendingRetirement => SlotRestoreError::PendingRetirement,
            _ => SlotRestoreError::Faulted,
        })?;
        let anchor = self.checked_slot_anchor()?;
        if saved_game_time != self.game_time {
            return Err(SlotRestoreError::ClockMismatch);
        }
        let [x, y, z] = anchor.target.position;
        let current = self
            .chunks
            .get(&(x >> 4, z >> 4))
            .ok_or(SlotRestoreError::NotResident)?
            .block(x & 15, y, z & 15);
        let state = anchor
            .binding
            .physical_state(current)
            .ok_or(SlotRestoreError::StateMismatch)?;
        if !state.properties.is_empty() {
            return Err(SlotRestoreError::UnsupportedStack);
        }
        let header = Header {
            world: anchor.world,
            target: anchor.target.clone(),
            package: hex(anchor.binding.package_identity()),
            type_id: anchor.binding.type_id().into(),
            block_id: anchor.binding.block_id().into(),
            observed: state.name.clone(),
            time: saved_game_time,
            machine_head: anchor.high_water,
            slot_head: metadata.high_water,
            machine_generation: anchor.generation,
        };
        let value = trial.active.map(|value| ValueRef {
            generation: value.key.incarnation(),
            revision: value.revision,
            stack: value.stack,
        });
        encode_frame(&header, value, items)
    }
    fn slot_fault(&mut self) {
        if let Some(metadata) = self
            .counter_domain
            .as_mut()
            .and_then(|domain| domain.slot.as_mut())
            && metadata.status != SlotStatus::Detached
        {
            metadata.status = SlotStatus::Faulted;
        }
        if let Some(trial) = self.world_slot_trial.as_mut() {
            trial.persistent_fault();
        }
    }
    pub(super) fn persistent_slot_changed(&mut self, position: [i32; 3], state: BlockStateId) {
        let Some(metadata) = self.persistent_world_slot_metadata() else {
            return;
        };
        if metadata.status != SlotStatus::Healthy {
            return;
        }
        let Some(trial) = self.world_slot_trial.as_ref() else {
            self.slot_fault();
            return;
        };
        if trial.position() != position {
            return;
        }
        if trial.borrowed().pending {
            return;
        }
        if let SlotAllocation::Active {
            slot_generation,
            machine_generation,
        } = metadata.allocation
        {
            self.world_slot_trial.as_mut().unwrap().persistent_retire(
                world_slot_trial::RetirementReason::BlockChanged,
                self.game_time,
            );
            self.counter_domain
                .as_mut()
                .unwrap()
                .slot
                .as_mut()
                .unwrap()
                .allocation = SlotAllocation::Pending {
                slot_generation,
                machine_generation,
            };
            return;
        }
        if state != trial.borrowed().configuration.state_id() {
            return;
        }
        let machine = match self.checked_slot_anchor() {
            Ok(anchor) => anchor.generation,
            Err(_) => {
                self.slot_fault();
                return;
            }
        };
        let Some(machine) = machine else {
            self.slot_fault();
            return;
        };
        let Some(next) = metadata.high_water.checked_add(1) else {
            self.slot_fault();
            return;
        };
        let trial = self.world_slot_trial.as_mut().unwrap();
        trial.persistent_admit();
        if trial
            .borrowed()
            .active
            .as_ref()
            .is_none_or(|value| value.key.incarnation() != next)
        {
            self.slot_fault();
            return;
        }
        let slot = self.counter_domain.as_mut().unwrap().slot.as_mut().unwrap();
        slot.high_water = next;
        slot.allocation = SlotAllocation::Active {
            slot_generation: next,
            machine_generation: machine,
        };
    }
    pub(super) fn persistent_slot_inserted(&mut self, replaced: bool, state: BlockStateId) {
        let Some(metadata) = self.persistent_world_slot_metadata() else {
            return;
        };
        if metadata.status != SlotStatus::Healthy {
            return;
        }
        let Some(trial) = self.world_slot_trial.as_mut() else {
            self.slot_fault();
            return;
        };
        if trial.borrowed().pending {
            return;
        }
        if let SlotAllocation::Active {
            slot_generation,
            machine_generation,
        } = metadata.allocation
        {
            if replaced || state != trial.borrowed().configuration.state_id() {
                let reason = if replaced {
                    world_slot_trial::RetirementReason::ChunkReplaced
                } else {
                    world_slot_trial::RetirementReason::ReloadMismatch
                };
                trial.persistent_retire(reason, self.game_time);
                self.counter_domain
                    .as_mut()
                    .unwrap()
                    .slot
                    .as_mut()
                    .unwrap()
                    .allocation = SlotAllocation::Pending {
                    slot_generation,
                    machine_generation,
                };
            } else {
                trial.inserted(false, state, self.game_time);
            }
        }
    }
}

#[cfg(test)]
#[path = "slot_restore_tests.rs"]
mod tests;
