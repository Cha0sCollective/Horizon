//! Experimental discovery and selection, before content registration or execution.
//! Legacy content retains its existing reader; paired payloads are captured once
//! and shared with the resource loader and each side's runtime.
use super::{Bundle, DESCRIPTOR, PreparedBundle, check_owners};
use crate::{Capability, Content, Manifest, Package, manifest::MANIFEST};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone)]
pub enum Kind {
    Legacy,
    /// Both executable declarations, including while the package is disabled.
    Paired(Bundle),
}

pub struct Entry {
    pub path: PathBuf,
    pub manifest: Option<Manifest>,
    pub kind: Option<Kind>,
    pub enabled: bool,
    /// Unselected invalid installs remain visible to the Mods page.
    pub error: Option<String>,
    /// Selected content only. Paired clones share the captured resource bytes.
    pub content: Option<Content>,
}

pub struct Catalog {
    entries: Vec<Entry>,
    prepared: BTreeMap<PathBuf, Arc<PreparedBundle>>,
}

fn present(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Candidate paths, sorted by path. Broken descriptor symlinks are candidates,
/// too: they produce a diagnostic instead of silently disappearing.
/// A missing mods directory is an ordinary empty installation.
pub fn find(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    let mut found = Vec::new();
    for entry in read {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().is_some_and(|ext| ext == "hmod")
            || (path.is_dir()
                && (present(&path.join(MANIFEST))? || present(&path.join(DESCRIPTOR))?))
        {
            found.push(path);
        }
    }
    found.sort();
    Ok(found)
}

fn declarations(path: &Path) -> Result<(Manifest, Kind), String> {
    if path.is_dir() {
        let legacy = present(&path.join(MANIFEST))?;
        let paired = present(&path.join(DESCRIPTOR))?;
        if legacy && paired {
            return Err(format!(
                "ambiguous package: contains both {MANIFEST} and {DESCRIPTOR}"
            ));
        }
        if paired {
            let bundle = Bundle::read_folder(path)?;
            return Ok((bundle.content_owner().clone(), Kind::Paired(bundle)));
        }
    } else if path.extension().is_some_and(|ext| ext == "hmod") {
        // Paired archives are not a supported format. Never interpret a zip
        // containing both descriptors as whichever happens to be read first.
        let file = fs::File::open(path).map_err(|e| e.to_string())?;
        let archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
        let legacy = archive.file_names().any(|name| name == MANIFEST);
        let paired = archive.file_names().any(|name| name == DESCRIPTOR);
        if paired {
            return Err(if legacy {
                format!("ambiguous package: contains both {MANIFEST} and {DESCRIPTOR}")
            } else {
                "paired packages must be folders, not .hmod archives".into()
            });
        }
    }
    let package = Package::read(path)?;
    Ok((package.manifest, Kind::Legacy))
}

impl Catalog {
    /// Select once, validate all selected owners, then expose content/snapshots.
    /// `None` lets a dev path select even an invalid declaration, in which case
    /// admission fails. Invalid unselected entries are retained as error rows.
    /// No component is compiled or instantiated here.
    pub fn read(
        paths: impl IntoIterator<Item = PathBuf>,
        enabled: impl Fn(&Path, Option<&Manifest>) -> bool,
    ) -> Result<Self, String> {
        let mut paths: Vec<_> = paths.into_iter().collect();
        paths.sort();
        paths.dedup();
        let mut entries = Vec::new();
        let mut prepared = BTreeMap::new();
        for path in paths {
            let (manifest, kind) = match declarations(&path) {
                Ok(declarations) => declarations,
                Err(error) => {
                    if enabled(&path, None) {
                        return Err(format!("{}: {error}", path.display()));
                    }
                    entries.push(Entry {
                        path,
                        manifest: None,
                        kind: None,
                        enabled: false,
                        error: Some(error),
                        content: None,
                    });
                    continue;
                }
            };
            let on = enabled(&path, Some(&manifest));
            let (manifest, kind, content) = if on {
                match kind {
                    Kind::Paired(_) => {
                        let snapshot = Arc::new(
                            PreparedBundle::read_folder(&path)
                                .map_err(|e| format!("{}: {e}", path.display()))?,
                        );
                        let owner = snapshot.bundle().content_owner().clone();
                        if !enabled(&path, Some(&owner)) {
                            return Err(format!(
                                "{}: package changed during selection",
                                path.display()
                            ));
                        }
                        let kind = Kind::Paired(snapshot.bundle().clone());
                        let content = snapshot.content().clone();
                        prepared.insert(path.clone(), snapshot);
                        (owner, kind, Some(content))
                    }
                    Kind::Legacy => {
                        let content = if manifest.capabilities.contains(&Capability::Registries) {
                            Some(
                                Content::read(&path, &manifest)
                                    .map_err(|e| format!("{}: {e}", path.display()))?,
                            )
                        } else {
                            None
                        };
                        (manifest, Kind::Legacy, content)
                    }
                }
            } else {
                (manifest, kind, None)
            };
            entries.push(Entry {
                path,
                manifest: Some(manifest),
                kind: Some(kind),
                enabled: on,
                error: None,
                content,
            });
        }
        let selected = || entries.iter().filter(|entry| entry.enabled);
        let mut ids = BTreeMap::new();
        for entry in selected() {
            let manifest = entry.manifest.as_ref().expect("selected declarations");
            if let Some(previous) = ids.insert(manifest.id.as_str(), &entry.path) {
                return Err(format!(
                    "duplicate package id {}: {} and {}",
                    manifest.id,
                    previous.display(),
                    entry.path.display()
                ));
            }
        }
        check_owners(selected().filter_map(|entry| {
            entry
                .manifest
                .as_ref()
                .filter(|manifest| manifest.capabilities.contains(&Capability::Registries))
        }))?;
        Ok(Self { entries, prepared })
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn prepared(&self) -> &BTreeMap<PathBuf, Arc<PreparedBundle>> {
        &self.prepared
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const PAIRED: &str = r#"
format = 1
host-contract = "paired-preview-1"
[content]
id = "counter"
name = "Counter"
version = "1"
api = "0.2"
side = "both"
capabilities = ["registries"]
namespaces = ["diagnostic_assets"]
[client]
entry = "client.wasm"
api = "0.1"
capabilities = ["hud"]
[server]
entry = "server.wasm"
api = "0.3"
capabilities = ["world"]
"#;
    const LEGACY: &str =
        "id='legacy'\nname='Legacy'\nversion='1'\napi='0.2'\ncapabilities=['registries']";

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "horizon-catalog-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn paired(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir(&path).unwrap();
            fs::write(path.join(DESCRIPTOR), PAIRED).unwrap();
            fs::write(path.join("client.wasm"), b"client bytes").unwrap();
            fs::write(path.join("server.wasm"), b"server bytes").unwrap();
            path
        }
        fn legacy(&self, name: &str, descriptor: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir(&path).unwrap();
            fs::write(path.join(MANIFEST), descriptor).unwrap();
            path
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            assert_eq!(self.0.parent(), Some(std::env::temp_dir().as_path()));
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn discovery_is_sorted_and_keeps_legacy_and_paired_candidates() {
        let scratch = Scratch::new();
        let z = scratch.paired("z");
        let a = scratch.legacy("a", LEGACY);
        let archive = scratch.0.join("b.hmod");
        fs::write(&archive, Package::write_files(LEGACY, None, &[]).unwrap()).unwrap();
        fs::create_dir(scratch.0.join("not_a_mod")).unwrap();
        fs::write(scratch.0.join("README.txt"), b"ignored").unwrap();
        assert_eq!(find(&scratch.0).unwrap(), [a, archive, z]);
        assert!(find(&scratch.0.join("missing")).unwrap().is_empty());
        assert!(find(&scratch.0.join("README.txt")).is_err());
    }

    #[test]
    fn disabled_paired_reads_only_declarations_and_retains_both_permissions() {
        let scratch = Scratch::new();
        let path = scratch.paired("paired");
        fs::remove_file(path.join("client.wasm")).unwrap();
        fs::remove_file(path.join("server.wasm")).unwrap();
        // Invalid resources would fail a selected capture.
        fs::create_dir(path.join("assets")).unwrap();
        fs::write(path.join("assets/INVALID"), b"not a resource").unwrap();
        let catalog = Catalog::read([path], |_, _| false).unwrap();
        assert!(catalog.prepared().is_empty());
        let entry = &catalog.entries()[0];
        assert!(!entry.enabled);
        assert!(entry.error.is_none());
        assert!(entry.content.is_none());
        let Some(Kind::Paired(bundle)) = &entry.kind else {
            panic!("paired header")
        };
        assert_eq!(
            bundle.plan(super::super::Role::Client).executables[0]
                .manifest()
                .capabilities,
            [Capability::Hud]
        );
        assert_eq!(
            bundle.plan(super::super::Role::Server).executables[0]
                .manifest()
                .capabilities,
            [Capability::World]
        );
    }

    #[test]
    fn selected_pair_requires_both_halves_even_for_client_selection() {
        let scratch = Scratch::new();
        let path = scratch.paired("paired");
        fs::remove_file(path.join("server.wasm")).unwrap();
        let error = Catalog::read([path], |_, _| true).err().unwrap();
        assert!(error.contains("server.wasm"), "{error}");
    }

    #[test]
    fn invalid_unselected_entry_is_visible_and_selected_dev_path_fails() {
        let scratch = Scratch::new();
        let path = scratch.paired("bad");
        fs::write(path.join(DESCRIPTOR), b"not toml").unwrap();
        let catalog = Catalog::read([path.clone()], |_, _| false).unwrap();
        assert!(catalog.entries()[0].manifest.is_none());
        assert!(
            catalog.entries()[0]
                .error
                .as_ref()
                .unwrap()
                .contains(DESCRIPTOR)
        );
        assert!(Catalog::read([path], |_, _| true).is_err());
    }

    #[test]
    fn ambiguous_folders_are_never_classified_as_either_kind() {
        let scratch = Scratch::new();
        let path = scratch.paired("ambiguous");
        fs::write(path.join(MANIFEST), LEGACY).unwrap();
        let catalog = Catalog::read([path.clone()], |_, _| false).unwrap();
        assert!(catalog.entries()[0].kind.is_none());
        assert!(
            catalog.entries()[0]
                .error
                .as_ref()
                .unwrap()
                .contains("ambiguous")
        );
        assert!(
            Catalog::read([path], |_, _| true)
                .err()
                .unwrap()
                .contains("ambiguous")
        );
    }

    #[test]
    fn paired_archives_and_ambiguous_archives_are_rejected() {
        use std::io::Write;
        for legacy in [false, true] {
            let scratch = Scratch::new();
            let path = scratch.0.join("paired.hmod");
            let mut archive = zip::ZipWriter::new(fs::File::create(&path).unwrap());
            archive
                .start_file(DESCRIPTOR, zip::write::SimpleFileOptions::default())
                .unwrap();
            archive.write_all(PAIRED.as_bytes()).unwrap();
            if legacy {
                archive
                    .start_file(MANIFEST, zip::write::SimpleFileOptions::default())
                    .unwrap();
                archive.write_all(LEGACY.as_bytes()).unwrap();
            }
            archive.finish().unwrap();
            let error = Catalog::read([path], |_, _| true).err().unwrap();
            assert!(
                error.contains(if legacy {
                    "ambiguous"
                } else {
                    "must be folders"
                }),
                "{error}"
            );
        }
    }

    #[test]
    fn mixed_selected_packages_reject_duplicate_ids_and_namespace_owners() {
        let scratch = Scratch::new();
        let pair = scratch.paired("paired");
        let legacy = scratch.legacy("legacy", &LEGACY.replace("id='legacy'", "id='counter'"));
        let error = Catalog::read([legacy.clone(), pair.clone()], |_, _| true)
            .err()
            .unwrap();
        assert!(error.contains("duplicate package id counter"), "{error}");
        fs::write(
            legacy.join(MANIFEST),
            format!("{LEGACY}\nnamespaces=['diagnostic_assets']"),
        )
        .unwrap();
        let error = Catalog::read([legacy.clone(), pair.clone()], |_, _| true)
            .err()
            .unwrap();
        assert!(error.contains("diagnostic_assets"), "{error}");
        // An installed, disabled owner does not reserve its namespace.
        assert!(
            Catalog::read([legacy, pair], |_, manifest| manifest
                .is_some_and(|m| m.id == "counter"))
            .is_ok()
        );
    }

    #[test]
    fn selected_paired_content_and_runtime_share_the_same_immutable_snapshot() {
        let scratch = Scratch::new();
        let path = scratch.paired("paired");
        let name = "assets/counter/lang/en_us.json";
        fs::create_dir_all(path.join("assets/counter/lang")).unwrap();
        fs::write(path.join(name), b"original").unwrap();
        let catalog = Catalog::read([path.clone(), path.clone()], |_, _| true).unwrap();
        assert_eq!(catalog.entries().len(), 1);
        let first = catalog.prepared().get(&path).unwrap();
        let cloned = catalog.prepared().clone();
        assert!(Arc::ptr_eq(first, &cloned[&path]));
        let content = catalog.entries()[0].content.as_ref().unwrap();
        assert_eq!(content.snapshot_identity(), Some(first.identity()));
        fs::remove_dir_all(&path).unwrap();
        assert_eq!(content.file(name).unwrap(), b"original");
        assert_eq!(first.code(crate::Side::Server).unwrap(), b"server bytes");
    }

    #[cfg(unix)]
    #[test]
    fn broken_descriptor_symlink_stays_visible_and_cannot_select() {
        let scratch = Scratch::new();
        let path = scratch.0.join("broken");
        fs::create_dir(&path).unwrap();
        std::os::unix::fs::symlink("missing", path.join(DESCRIPTOR)).unwrap();
        assert_eq!(find(&scratch.0).unwrap(), std::slice::from_ref(&path));
        let catalog = Catalog::read([path.clone()], |_, _| false).unwrap();
        assert!(catalog.entries()[0].error.is_some());
        assert!(Catalog::read([path], |_, _| true).is_err());
    }
}
