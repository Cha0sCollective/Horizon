//! `CommandStorage`: named compounds for `/data ... storage <id>`, kept per
//! namespace in `data/<namespace>/command_storage.dat`.
use crate::save;
use horizon_world::nbt::{Compound, Tag};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommandStorage {
    /// namespace -> path -> contents.
    namespaces: BTreeMap<String, BTreeMap<String, Tag>>,
    dirty: bool,
}
fn split_id(id: &str) -> (&str, &str) {
    id.split_once(':').unwrap_or(("minecraft", id))
}
fn file(root: &Path, namespace: &str) -> PathBuf {
    root.join("data")
        .join(namespace)
        .join("command_storage.dat")
}
impl CommandStorage {
    /// Read every `data/<namespace>/command_storage.dat` under a world.
    pub fn load(root: &Path) -> Self {
        let mut s = Self::default();
        let Ok(dirs) = std::fs::read_dir(root.join("data")) else {
            return s;
        };
        for dir in dirs.flatten() {
            let namespace = dir.file_name().to_string_lossy().into_owned();
            let Some(data) =
                save::read_dat(&file(root, &namespace)).and_then(|t| t.get("data").cloned())
            else {
                continue;
            };
            if let Some(Tag::Compound(contents)) = data.get("contents") {
                let map = contents
                    .iter()
                    .map(|(k, v)| (k.to_string_lossy(), v.clone()))
                    .collect();
                s.namespaces.insert(namespace, map);
            }
        }
        s
    }
    pub fn save(&mut self, root: &Path) -> crate::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        for (namespace, map) in &self.namespaces {
            let contents: Compound = map
                .iter()
                .map(|(k, v)| (k.as_str().into(), v.clone()))
                .collect();
            save::write_dat(
                &file(root, namespace),
                Tag::compound([
                    (
                        "data",
                        Tag::compound([("contents", Tag::Compound(contents))]),
                    ),
                    ("DataVersion", Tag::Int(horizon_world::DATA_VERSION)),
                ]),
            )?;
        }
        // One global flag deliberately retries earlier namespaces after a later failure.
        self.dirty = false;
        Ok(())
    }
    /// `get`: an empty compound when unset.
    pub fn get(&self, id: &str) -> Tag {
        let (ns, path) = split_id(id);
        self.namespaces
            .get(ns)
            .and_then(|m| m.get(path))
            .cloned()
            .unwrap_or_else(|| Tag::Compound(Compound::new()))
    }
    /// `set`: empty contents remove the entry.
    pub fn set(&mut self, id: &str, contents: Tag) {
        let (ns, path) = split_id(id);
        let empty = matches!(&contents, Tag::Compound(c) if c.is_empty());
        let map = self.namespaces.entry(ns.to_owned()).or_default();
        if empty {
            map.remove(path);
        } else {
            map.insert(path.to_owned(), contents);
        }
        self.dirty = true;
    }
    /// Every stored id (`keys`).
    pub fn keys(&self) -> Vec<String> {
        self.namespaces
            .iter()
            .flat_map(|(ns, m)| m.keys().map(move |k| format!("{ns}:{k}")))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use horizon_world::nbt::parse_snbt;

    fn scratch(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "horizon-storage-retry-{name}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn failed_namespace_keeps_global_dirty_and_all_contents_for_partial_retry() {
        let root = scratch("namespaces");
        let mut storage = CommandStorage::default();
        for (namespace, value) in [("a", 1), ("b", 2), ("c", 3)] {
            storage.set(
                &format!("{namespace}:value"),
                Tag::compound([("value", Tag::Int(value))]),
            );
        }
        let before = storage.clone();
        let blocked = file(&root, "b").with_file_name("command_storage.dat_new");
        std::fs::create_dir_all(&blocked).unwrap();
        assert!(storage.save(&root).is_err());
        assert_eq!(storage, before);
        assert!(storage.dirty);
        assert_eq!(
            CommandStorage::load(&root).get("a:value"),
            storage.get("a:value")
        );
        assert!(!file(&root, "b").exists());
        assert!(!file(&root, "c").exists());
        std::fs::remove_dir(blocked).unwrap();
        storage.save(&root).unwrap();
        assert!(!storage.dirty);
        assert_eq!(storage.namespaces, before.namespaces);
        assert_eq!(CommandStorage::load(&root).namespaces, before.namespaces);
        let written = save::read_dat(&file(&root, "c")).unwrap();
        assert_eq!(
            written.get("DataVersion"),
            Some(&Tag::Int(horizon_world::DATA_VERSION))
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_last_storage_entry_removal_retries_empty_namespace() {
        let root = scratch("deletion");
        let mut storage = CommandStorage::default();
        storage.set("test:last", Tag::compound([("value", Tag::Int(7))]));
        storage.save(&root).unwrap();
        let path = file(&root, "test");
        let old = std::fs::read(&path).unwrap();
        let blocked = path.with_file_name("command_storage.dat_new");
        std::fs::create_dir(&blocked).unwrap();
        storage.set("test:last", Tag::Compound(Compound::new()));
        assert!(storage.save(&root).is_err());
        assert!(storage.dirty && storage.keys().is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), old);
        std::fs::remove_dir(blocked).unwrap();
        storage.save(&root).unwrap();
        assert!(!storage.dirty);
        let reloaded = CommandStorage::load(&root);
        assert!(reloaded.keys().is_empty());
        assert_eq!(reloaded.namespaces, storage.namespaces);
        let written = save::read_dat(&path).unwrap();
        assert_eq!(
            written.get("data").unwrap().get("contents"),
            Some(&Tag::Compound(Compound::new()))
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stores_per_namespace_and_persists() {
        let dir = std::env::temp_dir().join(format!("hz-storage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = CommandStorage::default();
        assert_eq!(s.get("minecraft:x"), Tag::Compound(Compound::new()));
        s.set("minecraft:x", parse_snbt("{a:1}").unwrap().0);
        s.set("my:pack/y", parse_snbt("{b:[1,2]}").unwrap().0);
        s.save(&dir).unwrap();
        let back = CommandStorage::load(&dir);
        assert_eq!(back.get("x"), parse_snbt("{a:1}").unwrap().0);
        assert_eq!(back.get("my:pack/y"), parse_snbt("{b:[1,2]}").unwrap().0);
        assert_eq!(back.keys(), vec!["minecraft:x", "my:pack/y"]);
        // Empty contents delete.
        let mut s = back;
        s.set("x", Tag::Compound(Compound::new()));
        assert_eq!(s.keys(), vec!["my:pack/y"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
