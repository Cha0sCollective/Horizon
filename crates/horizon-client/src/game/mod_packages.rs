//! Startup-owned experimental packages, shared by the window and server thread.
use horizon_client::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
#[cfg(feature = "paired-packages")]
use {horizon_mods::paired::catalog::Catalog, std::sync::Arc};

const WORLD_PACKAGES: &str = "horizon-paired-packages.json";
const MAX_WORLD_PACKAGES: u64 = 64 << 10;

#[derive(Clone, Default)]
pub(super) struct Packages {
    #[cfg(feature = "paired-packages")]
    catalog: Option<Arc<Catalog>>,
    /// Exact executable, not a mutable source revision label.
    build: String,
    identities: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct WorldPackages {
    format: u32,
    build: String,
    packages: BTreeMap<String, String>,
}

impl Packages {
    #[cfg(not(feature = "paired-packages"))]
    pub fn read(_enabled: &BTreeSet<String>, _dev: &[PathBuf]) -> Result<Self> {
        Ok(Self::default())
    }

    #[cfg(feature = "paired-packages")]
    pub fn read(enabled: &BTreeSet<String>, dev: &[PathBuf]) -> Result<Self> {
        use horizon_mods::paired::catalog;
        let here = std::env::current_dir()?;
        let mut paths = catalog::find(&here.join(horizon_client::mod_list::DIR))?;
        paths.extend(dev.iter().cloned());
        let catalog = Catalog::read(paths, |path, manifest| {
            dev.iter().any(|p| p == path) || manifest.is_some_and(|m| enabled.contains(&m.id))
        })?;
        if !catalog.prepared().is_empty()
            && catalog.entries().iter().any(|entry| {
                entry.enabled
                    && entry.content.is_some()
                    && matches!(entry.kind, Some(catalog::Kind::Legacy))
            })
        {
            return Err("paired preview cannot mix mutable legacy content owners; disable legacy content packages and restart".into());
        }
        #[cfg(not(feature = "mods"))]
        if !catalog.prepared().is_empty() {
            return Err("paired execution requires a build with --features mods".into());
        }
        #[cfg(not(feature = "mods"))]
        let identities = BTreeMap::new();
        #[cfg(feature = "mods")]
        let mut identities = BTreeMap::new();
        #[cfg(feature = "mods")]
        let mut checker = horizon_mods::Mods::default();
        #[cfg(feature = "mods")]
        for prepared in catalog.prepared().values() {
            checker.preflight_pair(prepared)?;
            identities.insert(
                prepared.bundle().content_owner().id.clone(),
                prepared
                    .identity()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect(),
            );
        }
        let build = if identities.is_empty() {
            String::new()
        } else {
            use sha2::{Digest, Sha256};
            let mut executable = fs::File::open(std::env::current_exe()?)?;
            let mut digest = Sha256::new();
            let mut bytes = [0; 64 << 10];
            loop {
                let n = executable.read(&mut bytes)?;
                if n == 0 {
                    break;
                }
                digest.update(&bytes[..n]);
            }
            format!("sha256:{:x}", digest.finalize())
        };
        Ok(Self {
            catalog: Some(Arc::new(catalog)),
            build,
            identities,
        })
    }

    /// No packet negotiation exists yet. The in-process memory connection shares
    /// these same captured Arcs; external peers cannot establish that property.
    pub fn external_network(&self) -> Result<()> {
        if self.identities.is_empty() {
            Ok(())
        } else {
            Err("paired packages require identity negotiation before external connections; use local singleplayer".into())
        }
    }

    pub fn has_pairs(&self) -> bool {
        !self.identities.is_empty()
    }

    #[cfg(feature = "mods")]
    pub fn check_running(&self, mods: &horizon_mods::Mods) -> Result<()> {
        for id in self.identities.keys() {
            let running = mods.entries().iter().any(|entry| {
                entry.manifest.as_ref().is_some_and(|m| &m.id == id)
                    && entry.status == horizon_mods::Status::Running
            });
            if !running {
                return Err(format!("required paired package {id} is not running; restart after correcting its activation error").into());
            }
        }
        Ok(())
    }

    #[cfg(feature = "paired-packages")]
    pub fn catalog(&self) -> Option<&Catalog> {
        self.catalog.as_deref()
    }

    /// Check before upgrade, migration, chunk decoding or world writes. Existing
    /// unmarked worlds cannot acquire paired content through implicit migration.
    pub fn check_world(&self, root: &Path) -> Result<()> {
        let path = root.join(WORLD_PACKAGES);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let existing = match fs::read_dir(root) {
                    Ok(mut entries) => entries.try_fold(false, |any, entry| {
                        let entry = entry?;
                        Ok::<_, std::io::Error>(any || entry.file_name() != "session.lock")
                    })?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                    Err(e) => return Err(e.into()),
                };
                if !self.identities.is_empty() && existing {
                    return Err("this existing world has no paired package pins; explicit migration is required".into());
                }
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        if !metadata.is_file() || metadata.len() > MAX_WORLD_PACKAGES {
            return Err("invalid paired world package record".into());
        }
        let mut bytes = Vec::new();
        fs::File::open(path)?
            .take(MAX_WORLD_PACKAGES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_WORLD_PACKAGES {
            return Err("paired world package record is too large".into());
        }
        let recorded: WorldPackages = serde_json::from_slice(&bytes)?;
        if recorded != self.world_record() {
            return Err("paired world packages or exact host executable differ; restore the recorded packages/build before opening this world".into());
        }
        Ok(())
    }

    fn world_record(&self) -> WorldPackages {
        WorldPackages {
            format: 1,
            build: self.build.clone(),
            packages: self.identities.clone(),
        }
    }

    /// Called only under the ordinary world's session lock and before its first
    /// level/chunk write. create_new never overwrites an existing admission pin.
    pub fn record_world(&self, root: &Path) -> Result<()> {
        self.check_world(root)?;
        if self.identities.is_empty() {
            return Ok(());
        }
        let path = root.join(WORLD_PACKAGES);
        if path.try_exists()? {
            return Ok(());
        }
        let bytes = serde_json::to_vec(&self.world_record())?;
        if bytes.len() as u64 > MAX_WORLD_PACKAGES {
            return Err("paired world package record is too large".into());
        }
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "horizon-package-admission-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn packages() -> Packages {
        Packages {
            #[cfg(feature = "paired-packages")]
            catalog: None,
            build: "test-executable".into(),
            identities: BTreeMap::from([("counter".into(), "test-package".into())]),
        }
    }
    #[test]
    fn world_pins_reject_missing_changed_packages_and_host_before_writes() {
        let root = Scratch::new();
        let selected = packages();
        selected.check_world(&root.0).unwrap();
        selected.record_world(&root.0).unwrap();
        fs::write(root.0.join("level.dat"), b"untouched").unwrap();
        selected.check_world(&root.0).unwrap();
        assert!(Packages::default().check_world(&root.0).is_err());
        let mut changed = packages();
        changed
            .identities
            .insert("counter".into(), "changed".into());
        assert!(changed.record_world(&root.0).is_err());
        changed = packages();
        changed.build = "other-executable".into();
        assert!(changed.check_world(&root.0).is_err());
        assert_eq!(fs::read(root.0.join("level.dat")).unwrap(), b"untouched");
    }
    #[test]
    fn unmarked_existing_world_requires_migration_and_invalid_record_refuses() {
        let root = Scratch::new();
        fs::write(root.0.join("level.dat"), b"untouched").unwrap();
        assert!(packages().check_world(&root.0).is_err());
        Packages::default().check_world(&root.0).unwrap();
        fs::write(root.0.join(WORLD_PACKAGES), b"{}").unwrap();
        assert!(Packages::default().check_world(&root.0).is_err());
    }
    #[test]
    fn external_peers_require_negotiation_only_with_selected_pairs() {
        Packages::default().external_network().unwrap();
        assert!(packages().external_network().is_err());
    }
}
