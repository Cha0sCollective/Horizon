//! The bounded native ItemEntity profile used only by session checkpoints.
use super::*;
use horizon_client::{entities::ItemEntity, entity_nbt, items::ItemStack};

pub(in crate::game) const ITEM_LIMIT: usize = 1024;
const NATIVE_ITEM_FIELDS: &[&str] = &[
    "id",
    "UUID",
    "Pos",
    "Motion",
    "Rotation",
    "OnGround",
    "fall_distance",
    "Fire",
    "Air",
    "Invulnerable",
    "PortalCooldown",
    "Item",
    "Health",
    "Age",
    "PickupDelay",
    "Thrower",
    "Owner",
];
pub(super) fn native_item_field(key: &nbt::JavaString) -> bool {
    NATIVE_ITEM_FIELDS
        .iter()
        .any(|field| key == &nbt::JavaString::from(*field))
}
pub(in crate::game) fn exact_item_encoding(tag: &Tag) -> SResult<usize> {
    let raw = checked(nbt::write_named(
        &NamedTag {
            name: "".into(),
            tag: tag.clone(),
        },
        limits(),
    ))?;
    require(
        raw_item_nbt(&raw)?.tag == *tag,
        "native item NBT semantic coercion",
    )?;
    Ok(raw.len())
}
#[derive(Debug, PartialEq)]
pub(in crate::game) struct ItemRow {
    pub(in crate::game) column: (i32, i32),
    pub(in crate::game) tag: Tag,
}
#[derive(Debug, Default, PartialEq)]
pub(in crate::game) struct ItemProjection {
    pub(in crate::game) rows: BTreeMap<[i32; 4], ItemRow>,
}
/// Issued only after all physical native records have passed validation.
pub(in crate::game) struct CheckedSessionItems {
    pub(super) projection: ItemProjection,
    residents: BTreeSet<(i32, i32)>,
    native_columns: BTreeSet<(i32, i32)>,
}
impl CheckedSessionItems {
    pub(in crate::game) fn projection(&self) -> &ItemProjection {
        &self.projection
    }
    pub(in crate::game) fn columns(&self) -> BTreeSet<(i32, i32)> {
        self.projection.rows.values().map(|r| r.column).collect()
    }
    pub(in crate::game) fn check_arrivals(
        &self,
        game: &Game<'_>,
        resident: &BTreeSet<(i32, i32)>,
        queued: &[((i32, i32), Vec<Tag>)],
    ) -> SResult<()> {
        require(
            self.columns().is_subset(resident),
            "item columns not all resident",
        )?;
        require(
            self.columns().is_subset(&self.residents) && resident.is_subset(&self.native_columns),
            "unverified recovery resident column",
        )?;
        require(queued.len() <= NODES, "native entity arrival queue bound")?;
        let mut actual = ItemProjection::default();
        let expected_columns = self.columns();
        let mut columns = BTreeSet::new();
        for (at, tags) in queued {
            require(
                resident.contains(at)
                    && expected_columns.contains(at)
                    && !tags.is_empty()
                    && columns.insert(*at),
                "unexpected/duplicate entity arrival column",
            )?;
            for tag in tags {
                insert_item(game, &mut actual, tag, *at)?;
            }
        }
        require(
            actual == self.projection,
            "native entity arrival ownership mismatch",
        )
    }
}

pub(in crate::game) fn item_column(item: &ItemEntity) -> SResult<(i32, i32)> {
    let p = item.position;
    require(
        p.x.is_finite()
            && p.y.is_finite()
            && p.z.is_finite()
            && p.x.floor() >= f64::from(i32::MIN)
            && p.x.floor() <= f64::from(i32::MAX)
            && p.z.floor() >= f64::from(i32::MIN)
            && p.z.floor() <= f64::from(i32::MAX),
        "invalid item position/column",
    )?;
    Ok(((p.x.floor() as i32) >> 4, (p.z.floor() as i32) >> 4))
}
fn bounded_string(s: &str) -> SResult<()> {
    require(s.encode_utf16().count() <= 4096, "live item string bound")
}
/// Conservative pre-encoding accounting prevents an already-borrowed graph from
/// expanding into an unbounded temporary in the recursive native item encoder.
#[derive(Default)]
struct ItemBudget {
    nodes: usize,
    bytes: usize,
}
impl ItemBudget {
    fn charge(&mut self, nodes: usize, bytes: usize) -> SResult<()> {
        self.nodes = self
            .nodes
            .checked_add(nodes)
            .ok_or_else(|| fail("item node overflow"))?;
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| fail("item byte overflow"))?;
        require(
            self.nodes <= NODES && self.bytes <= STRUCTURED_LIMIT,
            "item aggregate structured bound",
        )
    }
    fn string(&mut self, value: &str) -> SResult<()> {
        bounded_string(value)?;
        self.charge(1, value.encode_utf16().count() * 3 + 8)
    }
    fn tag(&mut self, value: &Tag, depth: usize) -> SResult<()> {
        require(depth <= 32, "item component depth")?;
        self.charge(1, 16)?;
        match value {
            Tag::String(s) => {
                require(
                    s.0.len() <= 4096 && char::decode_utf16(s.0.iter().copied()).all(|c| c.is_ok()),
                    "item component string",
                )?;
                self.charge(0, s.0.len() * 3)?;
            }
            Tag::List(v) => {
                require(v.len() <= NODES, "item component list bound")?;
                for v in v {
                    self.tag(v, depth + 1)?;
                }
            }
            Tag::Compound(v) => {
                require(v.len() <= NODES, "item component map bound")?;
                for (key, v) in v {
                    require(
                        key.0.len() <= 4096
                            && char::decode_utf16(key.0.iter().copied()).all(|c| c.is_ok()),
                        "item component key",
                    )?;
                    self.charge(0, key.0.len() * 3 + 3)?;
                    self.tag(v, depth + 1)?;
                }
            }
            Tag::ByteArray(v) => self.charge(v.len(), v.len())?,
            Tag::IntArray(v) => self.charge(v.len(), v.len().saturating_mul(4))?,
            Tag::LongArray(v) => self.charge(v.len(), v.len().saturating_mul(8))?,
            _ => {}
        }
        Ok(())
    }
}
/// Bound the borrowed graph BEFORE invoking recursive native encoders. Runtime
/// bundle selection must be inspected explicitly: ItemStack::eq ignores it.
pub(in crate::game) fn validate_live_stack(game: &Game<'_>, stack: &ItemStack) -> SResult<usize> {
    let mut work = vec![(stack, 0usize)];
    let mut nodes = 0usize;
    let mut budget = ItemBudget::default();
    while let Some((stack, depth)) = work.pop() {
        nodes += 1;
        require(nodes <= NODES && depth <= 32, "live item graph bound")?;
        bounded_string(&stack.item)?;
        budget.charge(8, 256)?;
        budget.string(&stack.item)?;
        require(
            stack.count > 0
                && stack.count <= i32::MAX as u32
                && stack.damage <= i32::MAX as u32
                && &*stack.item != "minecraft:air"
                && game.res.items().get(&stack.item).is_some(),
            "live item ID/count/damage",
        )?;
        if let Some(data) = stack.data() {
            require(
                data.bundle_selected.0 == -1,
                "runtime item bundle selection",
            )?;
            require(
                [
                    data.enchantments.len(),
                    data.stored_enchantments.len(),
                    data.other.len(),
                    data.writable_pages.len(),
                    data.banner_patterns.len(),
                    data.stew_effects.len(),
                    data.container.len(),
                    data.charged.len(),
                    data.bundle_contents.len(),
                ]
                .into_iter()
                .all(|n| n <= NODES),
                "live item component bound",
            )?;
            let mut keys = BTreeSet::new();
            for (key, tag) in &data.other {
                budget.string(key)?;
                budget.tag(tag, depth + 2)?;
                require(
                    keys.insert(key) && lan::session_player_payload_supported(tag),
                    "duplicate/unsupported live item component",
                )?;
            }
            for (id, _) in data.enchantments.iter().chain(&data.stored_enchantments) {
                budget.string(id)?;
                budget.charge(2, 24)?;
            }
            for s in data.custom_name.iter().chain(data.writable_pages.iter()) {
                budget.string(s)?;
                budget.charge(2, 24)?;
            }
            for s in data.potion.iter().chain(data.base_color.iter()) {
                budget.string(s)?;
            }
            if let Some((a, b)) = &data.trim {
                budget.string(a)?;
                budget.string(b)?;
            }
            if let Some(book) = &data.written {
                budget.string(&book.title)?;
                budget.string(&book.author)?;
                require(
                    book.pages.len() <= NODES && book.page_tags.len() <= NODES,
                    "book bound",
                )?;
                for page in &book.pages {
                    budget.string(page)?;
                    budget.charge(2, 24)?;
                }
                for page in &book.page_tags {
                    budget.tag(page, depth + 5)?;
                    require(lan::session_player_payload_supported(page), "book payload")?;
                }
            }
            for layer in &data.banner_patterns {
                budget.string(&layer.pattern)?;
                budget.string(&layer.color)?;
                budget.charge(4, 128)?;
            }
            for (id, _) in &data.stew_effects {
                budget.string(id)?;
                budget.charge(4, 128)?;
            }
            let mut explosion = |e: &horizon_items::firework::FireworkExplosion| -> SResult<()> {
                require(
                    e.colors.len() <= NODES && e.fade.len() <= NODES,
                    "firework payload bound",
                )?;
                budget.charge(
                    e.colors.len() + e.fade.len() + 8,
                    (e.colors.len() + e.fade.len()) * 8 + 128,
                )
            };
            if let Some(e) = &data.firework_explosion {
                explosion(e)?;
            }
            if let Some((_, es)) = &data.fireworks {
                require(es.len() <= NODES, "firework payload bound")?;
                for e in es {
                    explosion(e)?;
                }
            }
            let mut slots = BTreeSet::new();
            require(
                data.container.iter().all(|(slot, _)| slots.insert(*slot)),
                "duplicate live item cell",
            )?;
            let children = data
                .container
                .len()
                .checked_add(data.charged.len())
                .and_then(|n| n.checked_add(data.bundle_contents.len()))
                .and_then(|n| {
                    n.checked_add(
                        data.pot_decorations
                            .as_ref()
                            .map_or(0, |p| p.iter().flatten().count()),
                    )
                })
                .ok_or_else(|| fail("live item graph overflow"))?;
            require(
                children <= NODES.saturating_sub(nodes + work.len()),
                "live item graph bound",
            )?;
            checked(work.try_reserve(children))?;
            budget.charge(children, children * 32)?;
            work.extend(data.container.iter().map(|(_, s)| (s, depth + 1)));
            work.extend(
                data.charged
                    .iter()
                    .chain(&data.bundle_contents)
                    .map(|s| (s, depth + 1)),
            );
            if let Some(pots) = &data.pot_decorations {
                work.extend(pots.iter().flatten().map(|s| (s, depth + 1)));
            }
        }
    }
    let tag = save::item_to_nbt(stack);
    validate_player_items(game, &tag)?;
    require(
        lan::session_player_payload_supported(&tag),
        "item encoded structural bound",
    )?;
    exact_item_encoding(&tag)?;
    let decoded = save::item_from_nbt(&tag).ok_or_else(|| fail("native item decoder refused"))?;
    require(
        decoded == *stack && save::item_to_nbt(&decoded) == tag,
        "native item encoding changes live fields/components",
    )?;
    Ok(budget.bytes)
}

pub(in crate::game) fn validate_live_item(
    game: &Game<'_>,
    item: &ItemEntity,
) -> SResult<((i32, i32), usize)> {
    let at = item_column(item)?;
    require(
        game.world.chunks.contains_key(&at),
        "off-resident live item",
    )?;
    require(
        [item.velocity.x, item.velocity.y, item.velocity.z]
            .into_iter()
            .all(f64::is_finite)
            && i16::try_from(item.health).is_ok()
            && i16::try_from(item.fire_ticks).is_ok()
            && item.age <= i16::MAX as u32
            && item.pickup_delay <= i16::MAX as u32,
        "unrepresentable live item scalar",
    )?;
    require(
        [
            item.position.x,
            item.position.y,
            item.position.z,
            item.velocity.x,
            item.velocity.y,
            item.velocity.z,
        ]
        .into_iter()
        .all(|v| v.to_bits() != (-0.0f64).to_bits()),
        "live item signed-zero coercion",
    )?;
    let bytes = validate_live_stack(game, &item.stack)?;
    Ok((at, bytes + 512))
}

fn decode_native_item_profile(tag: &Tag) -> SResult<([i32; 4], ItemEntity)> {
    let fields = compound(tag, NATIVE_ITEM_FIELDS)?;
    require(
        fields.len()
            == 15
                + usize::from(tag.get("Thrower").is_some())
                + usize::from(tag.get("Owner").is_some()),
        "missing native item fields",
    )?;
    require(
        text(tag.get("id"))? == "minecraft:item",
        "unsupported native entity kind",
    )?;
    let uuid = |name| -> SResult<[i32; 4]> {
        match tag.get(name) {
            Some(Tag::IntArray(v)) => checked(v.as_slice().try_into()),
            _ => Err(fail("native item UUID type")),
        }
    };
    let identity = uuid("UUID")?;
    for key in ["Thrower", "Owner"] {
        if tag.get(key).is_some() {
            let _ = uuid(key)?;
        }
    }
    for key in ["Pos", "Motion"] {
        require(
            list(tag.get(key))?.len() == 3
                && list(tag.get(key))?
                    .iter()
                    .all(|v| matches!(v, Tag::Double(bits) if f64::from_bits(*bits).is_finite())),
            "native item vector type/value",
        )?;
    }
    require(
        list(tag.get("Rotation"))? == [Tag::Float(0), Tag::Float(0)]
            && tag.get("fall_distance") == Some(&Tag::Double(0))
            && tag.get("Air") == Some(&Tag::Short(300))
            && tag.get("Invulnerable") == Some(&Tag::Byte(0))
            && tag.get("PortalCooldown") == Some(&Tag::Int(0))
            && matches!(tag.get("OnGround"), Some(Tag::Byte(0 | 1)))
            && matches!(tag.get("Health"), Some(Tag::Short(_)))
            && matches!(tag.get("Fire"), Some(Tag::Short(_)))
            && matches!(tag.get("Age"), Some(Tag::Short(v)) if *v >= 0)
            && matches!(tag.get("PickupDelay"), Some(Tag::Short(v)) if *v >= 0),
        "native item scalar profile",
    )?;
    let item = tag.get("Item").ok_or_else(|| fail("native Item missing"))?;
    compound(item, &["id", "count", "components"])?;
    require(
        lan::session_player_payload_supported(tag),
        "native item structured bound",
    )?;
    exact_item_encoding(tag)?;
    let decoded = entity_nbt::item_entity_from_nbt(tag, 0, 0.0)
        .ok_or_else(|| fail("native entity decoder refused"))?;
    require(
        entity_nbt::item_entity_to_nbt(&decoded, identity, Some(tag)) == *tag
            && save::item_to_nbt(&decoded.stack) == *item,
        "coercive native item record",
    )?;
    Ok((identity, decoded))
}
pub(in crate::game) fn validate_item_tag(
    game: &Game<'_>,
    tag: &Tag,
) -> SResult<([i32; 4], ItemRow)> {
    let (identity, decoded) = decode_native_item_profile(tag)?;
    validate_player_items(
        game,
        tag.get("Item").ok_or_else(|| fail("native Item missing"))?,
    )?;
    validate_live_stack(game, &decoded.stack)?;
    Ok((
        identity,
        ItemRow {
            column: item_column(&decoded)?,
            tag: tag.clone(),
        },
    ))
}
fn insert_item(
    game: &Game<'_>,
    projection: &mut ItemProjection,
    tag: &Tag,
    at: (i32, i32),
) -> SResult<()> {
    require(
        projection.rows.len() < ITEM_LIMIT,
        "session dropped item bound",
    )?;
    let (uuid, row) = validate_item_tag(game, tag)?;
    require(row.column == at, "native item/record column mismatch")?;
    require(
        projection.rows.insert(uuid, row).is_none(),
        "duplicate native item UUID",
    )
}

/// Parse exact canonical Overworld region families before any native constructor.
/// The returned region can safely be multiplied by 32, including every local slot.
pub(super) fn native_region(path: &str) -> SResult<(bool, ChunkPos)> {
    let parts: Vec<_> = path.split('/').collect();
    require(
        parts.len() == 5
            && parts[..3] == ["dimensions", "minecraft", "overworld"]
            && matches!(parts[3], "region" | "entities"),
        "unsupported native region owner/dimension",
    )?;
    let name: Vec<_> = parts[4].split('.').collect();
    require(
        name.len() == 4 && name[0] == "r" && name[3] == "mca",
        "native region filename",
    )?;
    let x: i32 = checked(name[1].parse())?;
    let z: i32 = checked(name[2].parse())?;
    require(
        parts[4] == format!("r.{x}.{z}.mca"),
        "noncanonical native region filename",
    )?;
    for v in [x, z] {
        require(
            v.checked_mul(32).and_then(|v| v.checked_add(31)).is_some(),
            "region coordinate overflow",
        )?;
    }
    Ok((parts[3] == "entities", ChunkPos::new(x, z)))
}
/// Retain the native decompressor and its bounds, but reject bytes it leaves
/// behind after an otherwise valid stream. No alternate native codec is written.
pub(in crate::game) fn read_item_record(
    record: &horizon_world::region::ChunkRecord,
) -> SResult<NamedTag> {
    let raw = checked(
        record
            .compression
            .decode(&record.compressed, STRUCTURED_LIMIT),
    )?;
    match record.compression {
        Compression::Zlib => {
            let mut decoder = flate2::Decompress::new(true);
            let mut block = [0u8; 8192];
            loop {
                let before = decoder.total_in();
                let before_out = decoder.total_out();
                let status = checked(decoder.decompress(
                    &record.compressed[before as usize..],
                    &mut block,
                    flate2::FlushDecompress::None,
                ))?;
                require(
                    decoder.total_out() <= STRUCTURED_LIMIT as u64,
                    "entity decompression bound",
                )?;
                if status == flate2::Status::StreamEnd {
                    require(
                        decoder.total_in() == record.compressed.len() as u64,
                        "trailing entity zlib bytes",
                    )?;
                    break;
                }
                require(
                    decoder.total_in() != before || decoder.total_out() != before_out,
                    "truncated entity zlib stream",
                )?;
            }
        }
        Compression::Lz4 => {
            let mut at = 0usize;
            loop {
                let end = at
                    .checked_add(21)
                    .ok_or_else(|| fail("entity LZ4 header overflow"))?;
                let header = record
                    .compressed
                    .get(at..end)
                    .ok_or_else(|| fail("entity LZ4 header"))?;
                let compressed = u32::from_le_bytes(header[9..13].try_into().unwrap()) as usize;
                let size = u32::from_le_bytes(header[13..17].try_into().unwrap());
                at = end
                    .checked_add(compressed)
                    .ok_or_else(|| fail("entity LZ4 record overflow"))?;
                require(at <= record.compressed.len(), "entity LZ4 length")?;
                if size == 0 {
                    require(at == record.compressed.len(), "trailing entity LZ4 bytes")?;
                    break;
                }
            }
        }
        _ => {}
    }
    raw_item_nbt(&raw)
}
pub(super) fn read_session_checkpoint_items(
    game: &Game<'_>,
    root: &Path,
    residents: &BTreeSet<(i32, i32)>,
    blocks: &BTreeMap<(i32, i32), Arc<Chunk>>,
) -> SResult<CheckedSessionItems> {
    let inventory = tree(root, &["session.lock"])?;
    let mut projection = ItemProjection::default();
    for row in &inventory.files {
        require(
            !row.path.ends_with(".mcc"),
            "external MCC members unsupported",
        )?;
        if !row.path.ends_with(".mca") {
            continue;
        }
        let (entities, at) = native_region(&row.path)?;
        if !entities {
            continue;
        }
        let path = root.join(&row.path);
        let bytes = read(&path, FILE_LIMIT)?;
        let entries = raw_region(&bytes)?;
        let mut region = checked(RegionFile::open(&path))?;
        require(
            region.chunks().count() == entries.len(),
            "entity raw/native header mismatch",
        )?;
        for index in entries {
            let key = (
                at.x * 32 + (index % 32) as i32,
                at.z * 32 + (index / 32) as i32,
            );
            let record = checked(region.read(ChunkPos::new(key.0, key.1)))?
                .ok_or_else(|| fail("entity record vanished"))?;
            let named = read_item_record(&record)?;
            require(
                compound(&named.tag, &["DataVersion", "Position", "Entities"])?.len() == 3
                    && named.tag.get("DataVersion") == Some(&Tag::Int(DATA_VERSION))
                    && named.tag.get("Position") == Some(&Tag::IntArray(vec![key.0, key.1])),
                "native entity record root/version/position",
            )?;
            let tags = list(named.tag.get("Entities"))?;
            if !tags.is_empty() {
                require(
                    residents.contains(&key) && blocks.contains_key(&key),
                    "off-resident/non-native entity column",
                )?;
            }
            for tag in tags {
                insert_item(game, &mut projection, tag, key)?;
            }
        }
        require(
            member(root, row.path.clone())? == *row,
            "entity region changed while reading",
        )?;
    }
    Ok(CheckedSessionItems {
        projection,
        residents: residents.clone(),
        native_columns: blocks.keys().copied().collect(),
    })
}

#[cfg(all(test, target_os = "linux"))]
#[path = "session_checkpoint_items_tests.rs"]
mod tests;
