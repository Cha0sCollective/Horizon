//! What the configuration phase sends: the synchronized registries (names in
//! network order, with their data for clients that do not know the vanilla
//! pack) and the tags of every registry, loaded from the prepared resources
//! (`data/minecraft/...`) and the registries report.
use horizon_protocol::{
    packets::configuration::{RegistryData, RegistryEntry, RegistryTags, TagEntry, UpdateTags},
    registry::{Builtin, Dynamic},
};
use horizon_world::nbt::{JavaString, Tag};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
};

pub use compatibility::RegistrySnapshot;

/// Internal capture-time identity only. This module does not alter configuration
/// emission, admit peers, or freeze the tables from which it reads.
mod compatibility {
    use super::*;
    use horizon_world::{
        nbt::{Limits, write_network},
        registry::{BlockRegistry, BlockStateId},
    };
    use sha2::{Digest, Sha256};

    const MAX_BYTES: usize = 128 << 20;
    const MAX_TABLES: usize = 1_000_000;
    const MAX_COLLECTION: usize = 262_144;
    const MAX_NODES: usize = 8_000_000;
    const DOMAIN: &[u8] = b"horizon-peer-snapshot-1/";

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct RegistrySnapshot {
        pub builtin: [u8; 32],
        pub blocks: [u8; 32],
        pub synchronized: [u8; 32],
        pub tags: [u8; 32],
        pub features: [u8; 32],
    }

    fn checked_add(a: usize, b: usize) -> Result<usize, String> {
        a.checked_add(b)
            .ok_or_else(|| "snapshot counter overflow".into())
    }
    fn collection(n: usize) -> Result<(), String> {
        if n > MAX_COLLECTION {
            Err("snapshot collection limit".into())
        } else {
            Ok(())
        }
    }
    fn text(units: impl Iterator<Item = u16>) -> Result<usize, String> {
        let mut count = 0usize;
        let mut bytes = 0usize;
        for c in units {
            count = checked_add(count, 1)?;
            bytes = checked_add(
                bytes,
                match c {
                    1..=127 => 1,
                    0..=2047 => 2,
                    _ => 3,
                },
            )?;
            if count > 32_767 || bytes > 65_535 {
                return Err("snapshot NBT string limit".into());
            }
        }
        checked_add(2, bytes)
    }
    fn string(s: &str) -> Result<usize, String> {
        text(s.encode_utf16())
    }
    fn identifier(s: &str) -> Result<(), String> {
        if s.is_empty()
            || s.len() > 1_024
            || !s
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_:./-".contains(&c))
        {
            return Err("snapshot unsupported identifier".into());
        }
        Ok(())
    }
    fn resource(s: &str) -> Result<(), String> {
        identifier(s)?;
        let Some((namespace, path)) = s.split_once(':') else {
            return Err("snapshot namespaced identifier required".into());
        };
        if namespace.is_empty() || path.is_empty() || path.contains(':') || namespace.contains('/')
        {
            return Err("snapshot invalid namespaced identifier".into());
        }
        Ok(())
    }

    #[derive(Default)]
    struct Budget {
        bytes: usize,
        tables: usize,
        nodes: usize,
        encoded_bytes: usize,
    }
    impl Budget {
        fn bytes(&mut self, n: usize) -> Result<(), String> {
            self.bytes = checked_add(self.bytes, n)?;
            if self.bytes > MAX_BYTES {
                return Err("snapshot aggregate byte limit".into());
            }
            Ok(())
        }
        fn table(&mut self, n: usize) -> Result<(), String> {
            collection(n)?;
            self.tables = checked_add(self.tables, n)?;
            if self.tables > MAX_TABLES {
                return Err("snapshot aggregate table limit".into());
            }
            Ok(())
        }
        fn name(&mut self, s: &str) -> Result<(), String> {
            identifier(s)?;
            string(s)?;
            self.bytes(checked_add(8, s.len())?)
        }
        fn entry(&mut self, measured: Measure) -> Result<(), String> {
            let limits = Limits::default();
            if measured.bytes > limits.max_bytes
                || measured.nodes > limits.max_nodes
                || measured.depth >= limits.max_depth
            {
                return Err("snapshot native entry limit".into());
            }
            self.nodes = checked_add(self.nodes, measured.nodes)?;
            if self.nodes > MAX_NODES {
                return Err("snapshot aggregate native node limit".into());
            }
            self.encoded_bytes = checked_add(self.encoded_bytes, measured.bytes)?;
            self.bytes(checked_add(8, measured.bytes)?)
        }
    }

    // Exact wire-size accounting (including root ID and synthetic mixed-list
    // wrappers); this is a borrowed preflight, not an alternative NBT encoder.
    #[derive(Clone, Copy, Debug)]
    struct Measure {
        bytes: usize,
        nodes: usize,
        depth: usize,
    }
    impl Measure {
        fn scalar(bytes: usize, depth: usize) -> Self {
            Self {
                bytes,
                nodes: 1,
                depth,
            }
        }
        fn child(&mut self, child: Self) -> Result<(), String> {
            self.bytes = checked_add(self.bytes, child.bytes)?;
            self.nodes = checked_add(self.nodes, child.nodes)?;
            self.depth = self.depth.max(child.depth);
            Ok(())
        }
    }
    fn json_id(value: &Value) -> u8 {
        match value {
            Value::Null | Value::Object(_) => 10,
            Value::Bool(_) => 1,
            Value::Number(n) => match n.as_i64() {
                Some(n) if i32::try_from(n).is_ok() => 3,
                Some(_) => 4,
                None => 6,
            },
            Value::String(_) => 8,
            Value::Array(_) => 9,
        }
    }
    fn json_measure(
        value: &Value,
        source_depth: usize,
        wire_depth: usize,
        source_nodes: &mut usize,
    ) -> Result<Measure, String> {
        if source_depth > 64 {
            return Err("snapshot source depth limit".into());
        }
        *source_nodes = checked_add(*source_nodes, 1)?;
        if *source_nodes > 500_000 {
            return Err("snapshot source node limit".into());
        }
        let mut result = Measure::scalar(0, wire_depth);
        match value {
            Value::Null => result.bytes = 1,
            Value::Bool(_) => result.bytes = 1,
            Value::Number(_) => result.bytes = if json_id(value) == 3 { 4 } else { 8 },
            Value::String(s) => result.bytes = string(s)?,
            Value::Object(values) => {
                collection(values.len())?;
                result.bytes = 1;
                for (key, child) in values {
                    result.bytes = checked_add(result.bytes, checked_add(1, string(key)?)?)?;
                    result.child(json_measure(
                        child,
                        source_depth + 1,
                        wire_depth + 1,
                        source_nodes,
                    )?)?;
                }
            }
            Value::Array(values) => {
                collection(values.len())?;
                result.bytes = 5;
                let first = values.first().map_or(0, json_id);
                let compound = first == 10 || values.iter().any(|v| json_id(v) != first);
                for child in values {
                    let empty_key_wrapper =
                        matches!(child, Value::Object(o) if o.len() == 1 && o.contains_key(""));
                    let wrapped = compound && (json_id(child) != 10 || empty_key_wrapper);
                    if wrapped {
                        result.bytes = checked_add(result.bytes, 4)?;
                        result.nodes = checked_add(result.nodes, 1)?;
                    }
                    result.child(json_measure(
                        child,
                        source_depth + 1,
                        wire_depth + if wrapped { 2 } else { 1 },
                        source_nodes,
                    )?)?;
                }
            }
        }
        Ok(result)
    }
    fn native_measure(
        value: &Tag,
        source_depth: usize,
        wire_depth: usize,
        source_nodes: &mut usize,
    ) -> Result<Measure, String> {
        if source_depth > 64 {
            return Err("snapshot source depth limit".into());
        }
        *source_nodes = checked_add(*source_nodes, 1)?;
        if *source_nodes > 500_000 {
            return Err("snapshot source node limit".into());
        }
        let mut result = Measure::scalar(0, wire_depth);
        match value {
            Tag::End => return Err("snapshot unsupported End payload".into()),
            Tag::Byte(_) => result.bytes = 1,
            Tag::Short(_) => result.bytes = 2,
            Tag::Int(_) | Tag::Float(_) => result.bytes = 4,
            Tag::Long(_) | Tag::Double(_) => result.bytes = 8,
            Tag::String(s) => result.bytes = text(s.0.iter().copied())?,
            Tag::ByteArray(v) => {
                collection(v.len())?;
                *source_nodes = checked_add(*source_nodes, v.len())?;
                result.nodes = checked_add(result.nodes, v.len())?;
                result.bytes = checked_add(4, v.len())?;
            }
            Tag::IntArray(v) => {
                collection(v.len())?;
                *source_nodes = checked_add(*source_nodes, v.len())?;
                result.nodes = checked_add(result.nodes, v.len())?;
                result.bytes = checked_add(
                    4,
                    v.len().checked_mul(4).ok_or("snapshot counter overflow")?,
                )?;
            }
            Tag::LongArray(v) => {
                collection(v.len())?;
                *source_nodes = checked_add(*source_nodes, v.len())?;
                result.nodes = checked_add(result.nodes, v.len())?;
                result.bytes = checked_add(
                    4,
                    v.len().checked_mul(8).ok_or("snapshot counter overflow")?,
                )?;
            }
            Tag::Compound(values) => {
                collection(values.len())?;
                result.bytes = 1;
                for (key, child) in values {
                    result.bytes =
                        checked_add(result.bytes, checked_add(1, text(key.0.iter().copied())?)?)?;
                    result.child(native_measure(
                        child,
                        source_depth + 1,
                        wire_depth + 1,
                        source_nodes,
                    )?)?;
                }
            }
            Tag::List(values) => {
                collection(values.len())?;
                result.bytes = 5;
                let first = values.first().map_or(0, Tag::id);
                let compound = first == 10 || values.iter().any(|v| v.id() != first);
                for child in values {
                    let empty_key_wrapper = matches!(child, Tag::Compound(c) if c.len() == 1 && c.keys().next().is_some_and(|k| k.0.is_empty()));
                    let wrapped = compound && (child.id() != 10 || empty_key_wrapper);
                    if wrapped {
                        result.bytes = checked_add(result.bytes, 4)?;
                        result.nodes = checked_add(result.nodes, 1)?;
                    }
                    result.child(native_measure(
                        child,
                        source_depth + 1,
                        wire_depth + if wrapped { 2 } else { 1 },
                        source_nodes,
                    )?)?;
                }
            }
        }
        if *source_nodes > 500_000 {
            return Err("snapshot source node limit".into());
        }
        Ok(result)
    }
    fn measured_json(value: &Value) -> Result<Measure, String> {
        let mut m = json_measure(value, 1, 0, &mut 0)?;
        m.bytes = checked_add(m.bytes, 1)?;
        Ok(m)
    }
    fn measured_native(value: &Tag) -> Result<Measure, String> {
        let mut m = native_measure(value, 1, 0, &mut 0)?;
        m.bytes = checked_add(m.bytes, 1)?;
        Ok(m)
    }
    fn hasher(component: &[u8]) -> Sha256 {
        let mut hash = Sha256::new();
        hash.update(DOMAIN);
        hash.update(component);
        hash
    }
    fn number(hash: &mut Sha256, n: usize) {
        hash.update((n as u64).to_be_bytes());
    }
    fn name(hash: &mut Sha256, text: &str) {
        number(hash, text.len());
        hash.update(text.as_bytes());
    }
    fn encoded(value: &Tag, upper: Measure, remaining: &mut usize) -> Result<Vec<u8>, String> {
        let bytes =
            write_network(value, Limits::default()).map_err(|e| format!("snapshot NBT: {e}"))?;
        if bytes.len() > upper.bytes {
            return Err("snapshot preflight size mismatch".into());
        }
        *remaining = remaining
            .checked_sub(bytes.len())
            .ok_or("snapshot remaining aggregate byte limit")?;
        Ok(bytes)
    }

    impl RegistrySnapshot {
        fn preflight_blocks(blocks: &BlockRegistry, budget: &mut Budget) -> Result<(), String> {
            if blocks.is_empty() {
                return Err("snapshot empty block registry".into());
            }
            budget.table(blocks.len())?;
            budget.bytes(16)?; // State count and distinct default count.
            for index in 0..blocks.len() {
                let id =
                    BlockStateId(u32::try_from(index).map_err(|_| "snapshot state ID overflow")?);
                let state = blocks.state(id).ok_or("snapshot missing state ID")?;
                resource(&state.name)?;
                if blocks.id(state) != Some(id) {
                    return Err("snapshot block lookup roundtrip".into());
                }
                budget.bytes(8)?;
                budget.name(&state.name)?;
                if state.properties.len() > 64 {
                    return Err("snapshot block property limit".into());
                }
                budget.table(state.properties.len())?;
                budget.bytes(8)?;
                for (key, value) in &state.properties {
                    budget.name(key)?;
                    budget.name(value)?;
                }
                let default = blocks
                    .default_state(&state.name)
                    .ok_or("snapshot missing block default")?;
                if blocks
                    .state(default)
                    .is_none_or(|target| target.name != state.name)
                {
                    return Err("snapshot invalid block default target".into());
                }
                // The immutable public constructors guarantee exactly one
                // same-name default and no hidden extra reverse/default rows.
                // Its target is a unique representative, so count it once
                // without allocating a distinct-name set during preflight.
                if default == id {
                    budget.table(1)?;
                    budget.name(&state.name)?;
                    budget.bytes(8)?;
                }
            }
            Ok(())
        }
        fn hash_blocks(blocks: &BlockRegistry) -> Result<[u8; 32], String> {
            let mut hash = hasher(b"blocks");
            number(&mut hash, blocks.len());
            // Allocation/canonicalization follows the complete borrowed
            // preflight. Numeric state order remains meaningful.
            let mut defaults = std::collections::BTreeSet::new();
            for index in 0..blocks.len() {
                let state = blocks
                    .state(BlockStateId(index as u32))
                    .ok_or("snapshot missing state ID")?;
                number(&mut hash, index);
                name(&mut hash, &state.name);
                number(&mut hash, state.properties.len());
                for (key, value) in &state.properties {
                    name(&mut hash, key);
                    name(&mut hash, value);
                }
                defaults.insert(state.name.as_str());
            }
            number(&mut hash, defaults.len());
            for block in defaults {
                name(&mut hash, block);
                number(
                    &mut hash,
                    blocks
                        .default_state(block)
                        .ok_or("snapshot missing block default")?
                        .0 as usize,
                );
            }
            Ok(hash.finalize().into())
        }
        /// Internal data prerequisite only. Reservations account for the private
        /// package/build/schema component in the same aggregate resource policy.
        pub fn capture(
            sync: &SyncData,
            blocks: &BlockRegistry,
            reserved_bytes: usize,
            reserved_tables: usize,
        ) -> Result<Self, String> {
            let mut budget = Budget::default();
            budget.bytes(reserved_bytes)?;
            budget.table(reserved_tables)?;
            for component in [
                b"builtin".as_slice(),
                b"blocks",
                b"synchronized",
                b"tags",
                b"features",
            ] {
                budget.bytes(checked_add(DOMAIN.len(), component.len())?)?;
            }
            // This entire pass borrows inputs. No clone, sort, JSON conversion,
            // native encoding, or whole registry_packets allocation precedes it.
            budget.table(sync.builtin.registries.len())?;
            budget.bytes(8)?;
            for (key, ids) in &sync.builtin.registries {
                resource(key)?;
                budget.name(key)?;
                budget.table(ids.names.len())?;
                budget.table(ids.ids.len())?;
                if ids.names.len() != ids.ids.len() {
                    return Err("snapshot builtin reverse cardinality".into());
                }
                budget.bytes(8)?;
                for (index, value) in ids.names.iter().enumerate() {
                    resource(value)?;
                    budget.name(value)?;
                    budget.bytes(8)?;
                    if ids.id(value) != Some(index as i32)
                        || ids.name(index as i32) != Some(value.as_str())
                    {
                        return Err("snapshot builtin reverse lookup".into());
                    }
                }
                for (value, index) in &ids.ids {
                    resource(value)?;
                    if ids.name(*index) != Some(value.as_str()) {
                        return Err("snapshot builtin extra reverse lookup".into());
                    }
                }
            }
            budget.table(sync.registries.len())?;
            budget.table(sync.dynamic.registries.len())?;
            budget.bytes(8)?;
            if sync.registries.len() != sync.dynamic.registries.len() {
                return Err("snapshot dynamic registry cardinality".into());
            }
            for (key, entries) in &sync.registries {
                resource(key)?;
                budget.name(key)?;
                budget.table(entries.names.len())?;
                budget.table(entries.json.len())?;
                if entries.names.len() != entries.json.len() {
                    return Err("snapshot synchronized lengths differ".into());
                }
                let dynamic = sync
                    .dynamic
                    .registries
                    .get(key)
                    .ok_or("snapshot missing dynamic registry")?;
                budget.table(dynamic.len())?;
                if dynamic.len() != entries.names.len() {
                    return Err("snapshot dynamic entry cardinality".into());
                }
                budget.bytes(8)?;
                for index in 0..entries.names.len() {
                    let entry = &entries.names[index];
                    resource(entry)?;
                    budget.name(entry)?;
                    budget.bytes(8)?;
                    resource(&dynamic[index].0)?;
                    if dynamic[index].0 != *entry {
                        return Err("snapshot dynamic names/order differ".into());
                    }
                    if sync.dynamic.id(key, entry) != Some(index as i32)
                        || sync.dynamic.name(key, index as i32) != Some(entry.as_str())
                    {
                        return Err("snapshot duplicate/dynamic lookup".into());
                    }
                    budget.entry(measured_json(&entries.json[index])?)?;
                    if let Some(native) = &dynamic[index].1 {
                        budget.entry(measured_native(native)?)?;
                    }
                }
            }
            budget.table(sync.tags.registries.len())?;
            budget.bytes(8)?;
            for registry in &sync.tags.registries {
                resource(&registry.registry)?;
                if !sync.builtin.registries.contains_key(&registry.registry)
                    && !sync.dynamic.registries.contains_key(&registry.registry)
                {
                    return Err("snapshot unknown tag registry".into());
                }
                budget.name(&registry.registry)?;
                budget.table(registry.tags.len())?;
                budget.bytes(8)?;
                for tag in &registry.tags {
                    resource(&tag.name)?;
                    budget.name(&tag.name)?;
                    budget.table(tag.ids.len())?;
                    budget.bytes(8)?;
                    for id in &tag.ids {
                        budget.bytes(8)?;
                        let valid = sync.builtin.get(&registry.registry).map_or_else(
                            || sync.dynamic.name(&registry.registry, *id).is_some(),
                            |ids| ids.name(*id).is_some(),
                        );
                        if !valid {
                            return Err("snapshot invalid tag member".into());
                        }
                    }
                }
            }
            budget.table(sync.features.len())?;
            budget.bytes(8)?;
            for feature in &sync.features {
                resource(feature)?;
                budget.name(feature)?;
            }
            Self::preflight_blocks(blocks, &mut budget)?;
            let metadata = budget
                .bytes
                .checked_sub(budget.encoded_bytes)
                .ok_or("snapshot preflight accounting mismatch")?;
            let mut remaining = MAX_BYTES
                .checked_sub(metadata)
                .ok_or("snapshot remaining aggregate byte limit")?;

            let mut builtin = hasher(b"builtin");
            number(&mut builtin, sync.builtin.registries.len());
            let mut keys: Vec<_> = sync.builtin.registries.keys().collect();
            keys.sort();
            for key in keys {
                name(&mut builtin, key);
                let ids = &sync.builtin.registries[key];
                number(&mut builtin, ids.names.len());
                for (index, entry) in ids.names.iter().enumerate() {
                    number(&mut builtin, index);
                    name(&mut builtin, entry);
                }
            }
            let mut synchronized = hasher(b"synchronized");
            number(&mut synchronized, sync.registries.len());
            let mut seen = HashSet::new();
            for (key, entries) in &sync.registries {
                if !seen.insert(key) || sync.builtin.registries.contains_key(key) {
                    return Err("snapshot duplicate/ambiguous synchronized registry".into());
                }
                name(&mut synchronized, key);
                number(&mut synchronized, entries.names.len());
                let mut names = HashSet::new();
                let dynamic = &sync.dynamic.registries[key];
                for index in 0..entries.names.len() {
                    let entry = &entries.names[index];
                    if !names.insert(entry)
                        || sync.dynamic.id(key, entry) != Some(index as i32)
                        || sync.dynamic.name(key, index as i32) != Some(entry.as_str())
                    {
                        return Err("snapshot duplicate/dynamic lookup".into());
                    }
                    number(&mut synchronized, index);
                    name(&mut synchronized, entry);
                    let full = encoded(
                        &json_to_nbt(&entries.json[index]),
                        measured_json(&entries.json[index])?,
                        &mut remaining,
                    )?;
                    if let Some(native) = &dynamic[index].1 {
                        let cached = encoded(native, measured_native(native)?, &mut remaining)?;
                        if cached != full {
                            return Err("snapshot dynamic cached payload differs".into());
                        }
                    }
                    number(&mut synchronized, full.len());
                    synchronized.update(&full);
                }
            }
            let mut tags = hasher(b"tags");
            number(&mut tags, sync.tags.registries.len());
            let mut registries: Vec<_> = sync.tags.registries.iter().collect();
            registries.sort_by(|a, b| a.registry.cmp(&b.registry));
            for adjacent in registries.windows(2) {
                if adjacent[0].registry == adjacent[1].registry {
                    return Err("snapshot duplicate tag registry".into());
                }
            }
            for registry in registries {
                name(&mut tags, &registry.registry);
                number(&mut tags, registry.tags.len());
                let mut definitions: Vec<_> = registry.tags.iter().collect();
                definitions.sort_by(|a, b| a.name.cmp(&b.name));
                for adjacent in definitions.windows(2) {
                    if adjacent[0].name == adjacent[1].name {
                        return Err("snapshot duplicate tag name".into());
                    }
                }
                for tag in definitions {
                    name(&mut tags, &tag.name);
                    number(&mut tags, tag.ids.len());
                    let mut members = tag.ids.to_vec();
                    members.sort_unstable();
                    for adjacent in members.windows(2) {
                        if adjacent[0] == adjacent[1] {
                            return Err("snapshot duplicate tag member".into());
                        }
                    }
                    for member in members {
                        number(&mut tags, member as usize);
                    }
                }
            }
            let mut features = hasher(b"features");
            number(&mut features, sync.features.len());
            let mut enabled: Vec<_> = sync.features.iter().collect();
            enabled.sort();
            for adjacent in enabled.windows(2) {
                if adjacent[0] == adjacent[1] {
                    return Err("snapshot duplicate feature".into());
                }
            }
            for feature in enabled {
                name(&mut features, feature);
            }
            Ok(Self {
                builtin: builtin.finalize().into(),
                blocks: Self::hash_blocks(blocks)?,
                synchronized: synchronized.finalize().into(),
                tags: tags.finalize().into(),
                features: features.finalize().into(),
            })
        }
    }

    #[cfg(test)]
    mod snapshot_tests {
        use super::*;
        use horizon_protocol::registry::Ids;

        fn blocks(default: i32, reversed: bool) -> BlockRegistry {
            let mut states = vec![
                Tag::compound([("id", Tag::String("minecraft:air".into()))]),
                Tag::compound([
                    ("id", Tag::String("example:block".into())),
                    (
                        "properties",
                        Tag::compound([("axis", Tag::String("x".into()))]),
                    ),
                ]),
                Tag::compound([
                    ("id", Tag::String("example:block".into())),
                    (
                        "properties",
                        Tag::compound([("axis", Tag::String("y".into()))]),
                    ),
                ]),
            ];
            if reversed {
                states.swap(1, 2);
            }
            BlockRegistry::from_nbt(&Tag::compound([
                ("DataVersion", Tag::Int(horizon_world::DATA_VERSION)),
                ("states", Tag::List(states)),
                (
                    "defaults",
                    Tag::compound([
                        ("minecraft:air", Tag::Int(0)),
                        ("example:block", Tag::Int(default)),
                    ]),
                ),
            ]))
            .unwrap()
        }
        fn sync() -> SyncData {
            let mut builtin = Builtin::default();
            builtin.registries.insert(
                "minecraft:item".into(),
                Ids::from_names(vec!["minecraft:air".into(), "minecraft:stone".into()]),
            );
            let key = "minecraft:dimension_type".to_owned();
            let names = vec!["minecraft:overworld".into(), "minecraft:the_nether".into()];
            let mut dynamic = Dynamic::default();
            dynamic.registries.insert(
                key.clone(),
                names.iter().cloned().map(|n| (n, None)).collect(),
            );
            SyncData {
                builtin,
                dynamic,
                registries: vec![(
                    key,
                    Entries {
                        names,
                        json: vec![
                            serde_json::json!({"min_y":-64, "mixed":[1,{"text":"a"}]}),
                            serde_json::json!({"min_y":0}),
                        ],
                    },
                )],
                tags: UpdateTags {
                    registries: vec![RegistryTags {
                        registry: "minecraft:item".into(),
                        tags: vec![TagEntry {
                            name: "example:test".into(),
                            ids: vec![0, 1],
                        }],
                    }],
                },
                features: vec!["minecraft:vanilla".into(), "example:feature".into()],
                commands: None,
                recipes: vec![],
                advancements: super::super::super::advancements::ServerAdvancements::load(
                    Path::new("__snapshot_no_resources__"),
                ),
            }
        }
        fn capture(sync: &SyncData, blocks: &BlockRegistry) -> RegistrySnapshot {
            RegistrySnapshot::capture(sync, blocks, 0, 0).unwrap()
        }

        #[test]
        fn unordered_maps_tags_and_features_are_canonical_without_mutation() {
            let mut a = sync();
            a.builtin.registries.insert(
                "minecraft:entity_type".into(),
                Ids::from_names(vec!["minecraft:pig".into()]),
            );
            let expected = capture(&a, &blocks(1, false));
            let mut b = sync();
            let items = b.builtin.registries.remove("minecraft:item").unwrap();
            b.builtin.registries.insert(
                "minecraft:entity_type".into(),
                Ids::from_names(vec!["minecraft:pig".into()]),
            );
            b.builtin.registries.insert("minecraft:item".into(), items);
            b.features.reverse();
            b.tags.registries[0].tags[0].ids.reverse();
            assert_eq!(capture(&b, &blocks(1, false)), expected);
            assert_eq!(b.tags.registries[0].tags[0].ids, [1, 0]);
        }
        #[test]
        fn numeric_orders_and_defaults_are_meaningful() {
            let mut s = sync();
            let b = blocks(1, false);
            let original = capture(&s, &b);
            s.builtin.registries.insert(
                "minecraft:item".into(),
                Ids::from_names(vec!["minecraft:stone".into(), "minecraft:air".into()]),
            );
            assert_ne!(capture(&s, &b).builtin, original.builtin);
            assert_ne!(capture(&sync(), &blocks(2, false)).blocks, original.blocks);
            assert_ne!(capture(&sync(), &blocks(1, true)).blocks, original.blocks);
            let mut s = sync();
            s.registries[0].1.names.reverse();
            s.registries[0].1.json.reverse();
            s.dynamic
                .registries
                .get_mut("minecraft:dimension_type")
                .unwrap()
                .reverse();
            assert_ne!(capture(&s, &b).synchronized, original.synchronized);
        }
        #[test]
        fn public_block_constructors_refuse_duplicate_invalid_and_missing_defaults() {
            let air = Tag::compound([("id", Tag::String("minecraft:air".into()))]);
            let stone = Tag::compound([("id", Tag::String("minecraft:stone".into()))]);
            let input = |states, defaults| {
                Tag::compound([
                    ("DataVersion", Tag::Int(horizon_world::DATA_VERSION)),
                    ("states", Tag::List(states)),
                    ("defaults", Tag::compound(defaults)),
                ])
            };
            assert!(
                BlockRegistry::from_nbt(&input(
                    vec![air.clone(), air.clone()],
                    vec![("minecraft:air", Tag::Int(0))]
                ))
                .is_err()
            );
            assert!(
                BlockRegistry::from_nbt(&input(
                    vec![air.clone(), stone.clone()],
                    vec![("minecraft:air", Tag::Int(0))]
                ))
                .is_err()
            );
            assert!(
                BlockRegistry::from_nbt(&input(
                    vec![air.clone(), stone.clone()],
                    vec![
                        ("minecraft:air", Tag::Int(0)),
                        ("minecraft:stone", Tag::Int(0))
                    ]
                ))
                .is_err()
            );
            assert!(
                BlockRegistry::from_nbt(&input(
                    vec![air.clone()],
                    vec![
                        ("minecraft:air", Tag::Int(0)),
                        ("minecraft:ghost", Tag::Int(0))
                    ]
                ))
                .is_err()
            );
            assert!(
                BlockRegistry::from_nbt(&input(vec![air], vec![("minecraft:air", Tag::Int(1))]))
                    .is_err()
            );
            assert!(BlockRegistry::from_facts_json(&br#"{"defaults":{"minecraft:air":0},"states":[["air",true,false,"","minecraft:air"],["air",true,false,"","minecraft:air"]]}"#[..]).is_err());
        }
        #[test]
        fn complete_builtin_reverse_and_dynamic_lengths_refuse() {
            let b = blocks(1, false);
            let mut s = sync();
            s.builtin
                .registries
                .get_mut("minecraft:item")
                .unwrap()
                .ids
                .insert("minecraft:ghost".into(), 0);
            assert!(
                RegistrySnapshot::capture(&s, &b, 0, 0)
                    .unwrap_err()
                    .contains("cardinality")
            );
            let mut s = sync();
            s.builtin
                .registries
                .get_mut("minecraft:item")
                .unwrap()
                .ids
                .insert("minecraft:stone".into(), 0);
            assert!(RegistrySnapshot::capture(&s, &b, 0, 0).is_err());
            let mut s = sync();
            s.registries[0].1.json.pop();
            assert!(
                RegistrySnapshot::capture(&s, &b, 0, 0)
                    .unwrap_err()
                    .contains("lengths")
            );
            let mut s = sync();
            s.dynamic.registries.insert("example:extra".into(), vec![]);
            assert!(RegistrySnapshot::capture(&s, &b, 0, 0).is_err());
            let mut s = sync();
            s.dynamic
                .registries
                .get_mut("minecraft:dimension_type")
                .unwrap()
                .swap(0, 1);
            assert!(RegistrySnapshot::capture(&s, &b, 0, 0).is_err());
        }
        #[test]
        fn full_data_cached_payload_and_frozen_value_independence() {
            let mut s = sync();
            let b = blocks(1, false);
            let old = capture(&s, &b);
            let full = json_to_nbt(&s.registries[0].1.json[0]);
            s.dynamic
                .registries
                .get_mut("minecraft:dimension_type")
                .unwrap()[0]
                .1 = Some(full);
            assert_eq!(
                capture(&s, &b),
                old,
                "None is an omitted cache, not omitted identity data"
            );
            s.registries[0].1.json[0]["min_y"] = serde_json::json!(-32);
            assert!(
                RegistrySnapshot::capture(&s, &b, 0, 0)
                    .unwrap_err()
                    .contains("payload differs")
            );
            s.dynamic
                .registries
                .get_mut("minecraft:dimension_type")
                .unwrap()[0]
                .1 = None;
            assert_ne!(capture(&s, &b).synchronized, old.synchronized);
            assert_eq!(old, capture(&sync(), &b));
            s.tags.registries[0].tags[0].ids.pop();
            assert_ne!(capture(&s, &b).tags, old.tags);
            s.features.pop();
            assert_ne!(capture(&s, &b).features, old.features);
        }
        #[test]
        fn duplicates_and_bad_tag_members_refuse() {
            let b = blocks(1, false);
            let mut s = sync();
            s.features.push(s.features[0].clone());
            assert!(RegistrySnapshot::capture(&s, &b, 0, 0).is_err());
            let mut s = sync();
            s.tags.registries[0].tags[0].ids.push(0);
            assert!(RegistrySnapshot::capture(&s, &b, 0, 0).is_err());
            let mut s = sync();
            s.tags.registries[0].tags[0].ids.push(-1);
            assert!(RegistrySnapshot::capture(&s, &b, 0, 0).is_err());
            let mut s = sync();
            s.tags.registries.push(RegistryTags {
                registry: "minecraft:item".into(),
                tags: vec![],
            });
            assert!(RegistrySnapshot::capture(&s, &b, 0, 0).is_err());
            let mut s = sync();
            s.registries[0].1.names[1] = s.registries[0].1.names[0].clone();
            s.dynamic
                .registries
                .get_mut("minecraft:dimension_type")
                .unwrap()[1]
                .0 = s.registries[0].1.names[0].clone();
            assert!(RegistrySnapshot::capture(&s, &b, 0, 0).is_err());
        }
        #[test]
        fn borrowed_preflight_limits_and_checked_aggregate_exhaustion() {
            let b = blocks(1, false);
            let mut s = sync();
            s.registries[0].1.json[0] = Value::String("x".repeat(32_768));
            assert!(
                RegistrySnapshot::capture(&s, &b, 0, 0)
                    .unwrap_err()
                    .contains("string limit")
            );
            let mut s = sync();
            let mut deep = Value::Null;
            for _ in 0..64 {
                deep = Value::Array(vec![deep]);
            }
            s.registries[0].1.json[0] = deep;
            assert!(
                RegistrySnapshot::capture(&s, &b, 0, 0)
                    .unwrap_err()
                    .contains("depth")
            );
            assert!(RegistrySnapshot::capture(&sync(), &b, MAX_BYTES, 0).is_err());
            assert!(RegistrySnapshot::capture(&sync(), &b, usize::MAX, 0).is_err());
            let mut budget = Budget {
                tables: MAX_TABLES,
                ..Budget::default()
            };
            assert!(budget.table(1).is_err());
            let mut budget = Budget {
                nodes: MAX_NODES,
                ..Budget::default()
            };
            assert!(budget.entry(Measure::scalar(1, 0)).is_err());
            assert!(collection(MAX_COLLECTION + 1).is_err());
            assert!(
                Budget::default()
                    .entry(Measure {
                        bytes: (64 << 20) + 1,
                        nodes: 1,
                        depth: 0
                    })
                    .is_err()
            );
        }
        #[test]
        fn preflight_matches_native_wrappers_and_lossless_java_strings() {
            for value in [
                serde_json::json!([1,{"text":"x"}]),
                serde_json::json!([{ "": 1 }]),
                serde_json::json!({"nul":"\u{0}","astral":"\u{1f680}"}),
            ] {
                let tag = json_to_nbt(&value);
                let bytes = write_network(&tag, Limits::default()).unwrap();
                assert_eq!(bytes.len(), measured_json(&value).unwrap().bytes);
                assert_eq!(bytes.len(), measured_native(&tag).unwrap().bytes);
            }
            let tag = Tag::String(JavaString(vec![0, 0xd800, 0xdc00, 0xdfff]));
            let mut remaining = MAX_BYTES;
            let bytes = encoded(&tag, measured_native(&tag).unwrap(), &mut remaining).unwrap();
            assert_eq!(bytes.len(), 14);
            assert!(encoded(&tag, measured_native(&tag).unwrap(), &mut 13).is_err());
            assert!(measured_native(&Tag::String(JavaString(vec![0xd800; 21_846]))).is_err());
            assert!(measured_native(&Tag::List(vec![Tag::End])).is_err());
        }

        #[test]
        #[ignore = "pinned HORIZON_TEST_RESOURCES; read-only actual configuration tables"]
        fn actual_resource_full_emission_dynamic_and_data_controls() {
            let root = PathBuf::from(
                std::env::var_os("HORIZON_TEST_RESOURCES").expect("pinned resources required"),
            );
            let before =
                ["facts.json", "registries.json"].map(|p| std::fs::read(root.join(p)).unwrap());
            let blocks = BlockRegistry::from_facts_json(
                std::fs::File::open(root.join("facts.json")).unwrap(),
            )
            .unwrap();
            let a = SyncData::load(&root).unwrap();
            let mut b = SyncData::load(&root).unwrap();
            let first = capture(&a, &blocks);
            assert_eq!(first, capture(&b, &blocks));
            // Independently use the existing emitter after the positive snapshot;
            // its full data must agree with optional Dynamic cached entries.
            let packets = b.registry_packets(false);
            for packet in packets {
                b.dynamic.add(packet);
            }
            assert_eq!(capture(&b, &blocks), first);
            let index = b
                .registries
                .iter()
                .position(|(_, e)| !e.names.is_empty())
                .expect("actual synchronized entries");
            let key = b.registries[index].0.clone();
            b.registries[index].1.json[0] = serde_json::json!({"snapshot_control":true});
            assert!(
                RegistrySnapshot::capture(&b, &blocks, 0, 0)
                    .unwrap_err()
                    .contains("payload differs")
            );
            b.dynamic.registries.get_mut(&key).unwrap()[0].1 = None;
            assert_ne!(capture(&b, &blocks).synchronized, first.synchronized);
            let tag = b
                .tags
                .registries
                .iter_mut()
                .flat_map(|r| r.tags.iter_mut())
                .find(|t| !t.ids.is_empty())
                .expect("actual tag members");
            tag.ids.pop();
            assert_ne!(capture(&b, &blocks).tags, first.tags);
            assert_eq!(first, capture(&a, &blocks));
            assert_eq!(
                ["facts.json", "registries.json"].map(|p| std::fs::read(root.join(p)).unwrap()),
                before
            );
        }
    }
}

/// RegistryDataLoader.SYNCHRONIZED_REGISTRIES, in vanilla's order.
pub const SYNCHRONIZED: &[&str] = &[
    "minecraft:worldgen/biome",
    "minecraft:chat_type",
    "minecraft:trim_pattern",
    "minecraft:trim_material",
    "minecraft:wolf_variant",
    "minecraft:wolf_sound_variant",
    "minecraft:pig_variant",
    "minecraft:pig_sound_variant",
    "minecraft:frog_variant",
    "minecraft:cat_variant",
    "minecraft:cat_sound_variant",
    "minecraft:cow_sound_variant",
    "minecraft:cow_variant",
    "minecraft:chicken_sound_variant",
    "minecraft:chicken_variant",
    "minecraft:zombie_nautilus_variant",
    "minecraft:painting_variant",
    "minecraft:sulfur_cube_archetype",
    "minecraft:dimension_type",
    "minecraft:damage_type",
    "minecraft:banner_pattern",
    "minecraft:enchantment",
    "minecraft:jukebox_song",
    "minecraft:instrument",
    "minecraft:test_environment",
    "minecraft:test_instance",
    "minecraft:dialog",
    "minecraft:world_clock",
    "minecraft:timeline",
    "minecraft:decorated_pot_pattern",
    "minecraft:block_transformer",
    "minecraft:worldgen/block_state_provider",
];
/// KnownPack.vanilla("core") for this version.
pub const CORE_PACK: (&str, &str, &str) = ("minecraft", "core", horizon_protocol::GAME_VERSION);

/// A data-driven registry: entry names in network order and their JSON.
struct Entries {
    names: Vec<String>,
    json: Vec<Value>,
}

pub struct SyncData {
    pub builtin: Builtin,
    /// Entry names per synchronized registry (for id lookups).
    pub dynamic: Dynamic,
    registries: Vec<(String, Entries)>,
    pub tags: UpdateTags,
    pub features: Vec<String>,
    /// Vanilla's command tree (the commands report), when found.
    pub commands: Option<super::commands::CommandTree>,
    /// ClientboundUpdateRecipesPacket's payload.
    pub recipes: Vec<u8>,
    pub advancements: super::advancements::ServerAdvancements,
}

/// Every `*.json` under `dir`, as (path without extension relative to dir, value), sorted.
fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Value>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            walk(&path, &format!("{prefix}{name}/"), out);
        } else if let Some(stem) = name.strip_suffix(".json")
            && let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(value) = serde_json::from_str::<Value>(&text)
        {
            out.insert(format!("{prefix}{stem}"), value);
        }
    }
}

/// A JSON value as the NBT that vanilla's NbtOps would produce from the codec
/// (integers are ints, other numbers doubles, booleans bytes).
pub fn json_to_nbt(v: &Value) -> Tag {
    match v {
        Value::Null => Tag::compound(Vec::<(&str, Tag)>::new()),
        Value::Bool(b) => Tag::Byte(i8::from(*b)),
        Value::Number(n) => match n.as_i64() {
            Some(i) => match i32::try_from(i) {
                Ok(i) => Tag::Int(i),
                Err(_) => Tag::Long(i),
            },
            None => Tag::Double(n.as_f64().unwrap_or(0.0).to_bits()),
        },
        Value::String(s) => Tag::String(JavaString::from(s.as_str())),
        Value::Array(a) => Tag::List(a.iter().map(json_to_nbt).collect()),
        Value::Object(o) => Tag::compound(o.iter().map(|(k, v)| (k.as_str(), json_to_nbt(v)))),
    }
}

/// A component's JSON as NbtOps writes it. A list whose elements differ in type
/// (translation arguments mixing numbers and components) stays one list: the NBT
/// writer wraps each element that isn't a compound under the empty key, as
/// `ListTag.wrapIfNeeded` does, so wrapping it here too would leave the client a
/// `{"": 1}` argument it cannot read.
pub fn component_nbt(v: &Value) -> Tag {
    match v {
        Value::Array(a) => Tag::List(a.iter().map(component_nbt).collect()),
        Value::Object(o) => Tag::compound(o.iter().map(|(k, v)| (k.as_str(), component_nbt(v)))),
        other => json_to_nbt(other),
    }
}

impl SyncData {
    pub fn load(resources: &Path) -> Result<Self, String> {
        let builtin = Builtin::find(resources)
            .and_then(|p| Builtin::load(&p).ok())
            .ok_or("registries.json not found next to the resources")?;
        let root: PathBuf = resources.join("data/minecraft");
        let mut registries = Vec::new();
        let mut dynamic = Dynamic::default();
        for key in SYNCHRONIZED {
            let path = key.trim_start_matches("minecraft:");
            let mut found = BTreeMap::new();
            walk(&root.join(path), "", &mut found);
            let (names, json): (Vec<_>, Vec<_>) = found
                .into_iter()
                .map(|(n, v)| (format!("minecraft:{n}"), v))
                .unzip();
            dynamic.registries.insert(
                (*key).to_owned(),
                names.iter().map(|n| (n.clone(), None)).collect(),
            );
            registries.push(((*key).to_owned(), Entries { names, json }));
        }
        let tags = Self::load_tags(&root, &builtin, &dynamic);
        let mut sync = Self {
            builtin,
            dynamic,
            registries,
            tags,
            features: vec!["minecraft:vanilla".to_owned()],
            commands: super::commands::CommandTree::load(resources),
            recipes: Vec::new(),
            advancements: super::advancements::ServerAdvancements::load(resources),
        };
        sync.recipes = super::recipes::update_recipes(resources, &sync);
        Ok(sync)
    }

    /// The id of a name in a built-in or synchronized registry.
    pub fn id(&self, registry: &str, name: &str) -> Option<i32> {
        if let Some(ids) = self.builtin.get(registry) {
            return ids.id(name);
        }
        self.dynamic
            .registries
            .get(registry)?
            .iter()
            .position(|(n, _)| n == name)
            .map(|i| i as i32)
    }

    /// ClientboundRegistryDataPacket per registry. With `known_pack` the
    /// client rebuilds the entries from its own data, so none is sent.
    pub fn registry_packets(&self, known_pack: bool) -> Vec<RegistryData> {
        self.registries
            .iter()
            .map(|(key, e)| RegistryData {
                registry: key.clone(),
                entries: e
                    .names
                    .iter()
                    .zip(&e.json)
                    .map(|(name, json)| RegistryEntry {
                        id: name.clone(),
                        data: if known_pack {
                            None
                        } else {
                            Some(json_to_nbt(json))
                        },
                    })
                    .collect(),
            })
            .collect()
    }

    fn registry_names<'a>(
        builtin: &'a Builtin,
        dynamic: &'a Dynamic,
        key: &str,
    ) -> Option<HashMap<&'a str, i32>> {
        if let Some(ids) = builtin.get(key) {
            return Some(
                ids.names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| (n.as_str(), i as i32))
                    .collect(),
            );
        }
        dynamic.registries.get(key).map(|v| {
            v.iter()
                .enumerate()
                .map(|(i, (n, _))| (n.as_str(), i as i32))
                .collect()
        })
    }

    /// TagNetworkSerialization.serializeTagsToNetwork from the tag files.
    fn load_tags(root: &Path, builtin: &Builtin, dynamic: &Dynamic) -> UpdateTags {
        let tag_root = root.join("tags");
        let mut registries = Vec::new();
        let mut keys: Vec<String> = builtin
            .registries
            .keys()
            .chain(dynamic.registries.keys())
            .cloned()
            .collect();
        keys.sort();
        for key in keys {
            let dir = tag_root.join(key.trim_start_matches("minecraft:"));
            if !dir.is_dir() {
                continue;
            }
            let mut files = BTreeMap::new();
            walk(&dir, "", &mut files);
            if files.is_empty() {
                continue;
            }
            let Some(names) = Self::registry_names(builtin, dynamic, &key) else {
                continue;
            };
            let mut tags = Vec::new();
            for name in files.keys() {
                let mut ids = Vec::new();
                let mut seen = HashSet::new();
                Self::expand(&files, name, &names, &mut ids, &mut seen);
                tags.push(TagEntry {
                    name: format!("minecraft:{name}"),
                    ids,
                });
            }
            registries.push(RegistryTags {
                registry: key,
                tags,
            });
        }
        UpdateTags { registries }
    }

    fn expand(
        files: &BTreeMap<String, Value>,
        tag: &str,
        names: &HashMap<&str, i32>,
        out: &mut Vec<i32>,
        seen: &mut HashSet<String>,
    ) {
        if !seen.insert(tag.to_owned()) {
            return;
        }
        let Some(values) = files.get(tag).and_then(|v| v["values"].as_array()) else {
            return;
        };
        for value in values {
            let id = value
                .as_str()
                .or_else(|| value["id"].as_str())
                .unwrap_or_default();
            if let Some(reference) = id.strip_prefix('#') {
                let reference = reference.trim_start_matches("minecraft:");
                Self::expand(files, reference, names, out, seen);
            } else if let Some(&n) = names.get(id)
                && !out.contains(&n)
            {
                out.push(n);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_becomes_nbt() {
        let v: Value = serde_json::from_str(r#"{"a":1,"b":[true,2.5],"c":"x"}"#).unwrap();
        let t = json_to_nbt(&v);
        assert!(matches!(t.get("a"), Some(Tag::Int(1))));
        assert!(matches!(t.get("c"), Some(Tag::String(_))));
        assert!(matches!(t.get("b"), Some(Tag::List(l)) if l.len() == 2));
    }
}

#[cfg(test)]
mod component_tests {
    use super::*;
    use horizon_world::nbt::{Limits, read_network, write_network};

    /// `/give`'s reply: a count, then components. On the wire each element is wrapped
    /// once, and read back (ListTag.addAndUnwrap) it is the number again.
    #[test]
    fn mixed_translation_arguments_are_wrapped_once() {
        let json = serde_json::json!({
            "translate": "commands.give.success.single",
            "with": [1, {"text": "Raw Iron"}, {"text": "Tester"}],
        });
        let tag = component_nbt(&json);
        let Some(Tag::List(args)) = tag.get("with") else {
            panic!("no arguments: {tag:?}");
        };
        assert_eq!(args[0], Tag::Int(1));
        // Read back as ListTag.addAndUnwrap does: a wrapper wrapped twice stays a compound.
        let bytes = write_network(&tag, Limits::default()).unwrap();
        assert_eq!(read_network(&bytes, Limits::default()).unwrap().0, tag);
    }
}
