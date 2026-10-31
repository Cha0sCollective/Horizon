//! Startup-owned experimental packages, shared by the window and server thread.
use horizon_client::Result;
use horizon_client::lan::{
    data::SyncData,
    peer_contract::{WireClaims, WireContract},
};
#[cfg(feature = "paired-packages")]
use horizon_mods::paired::catalog::Catalog;
use horizon_world::registry::BlockRegistry;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

const WORLD_PACKAGES: &str = "horizon-paired-packages.json";
const MAX_WORLD_PACKAGES: u64 = 64 << 10;

/// Owned capture-time data, with no admission or writable-world authority.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PeerSnapshot {
    domain: &'static str,
    protocol: i32,
    game: String,
    build: String,
    package_scheme: String,
    package: (String, String),
    schema: (String, u32),
    registries: horizon_client::lan::data::RegistrySnapshot,
}

/// Retains the exact supplied local objects, but is not a live network freeze,
/// admission receipt or proof that any sender/hub uses them. Only the package
/// capture method constructs it; decoded claims are never constructor inputs.
#[allow(dead_code)]
pub(super) struct FrozenHostInputs {
    packages: Packages,
    sync: Arc<SyncData>,
    blocks: &'static BlockRegistry,
    snapshot: PeerSnapshot,
    wire: WireContract,
    expectation:
        std::sync::OnceLock<Arc<horizon_client::lan::login_negotiation::NegotiationExpectation>>,
}

#[allow(dead_code)]
impl FrozenHostInputs {
    /// Cache the one transport instance from this holder's actual retained owners.
    #[cfg(feature = "paired-packages")]
    pub(super) fn negotiation_expectation(
        &self,
    ) -> std::result::Result<
        Arc<horizon_client::lan::login_negotiation::NegotiationExpectation>,
        String,
    > {
        if let Some(expectation) = self.expectation.get() {
            return Ok(expectation.clone());
        }
        let actual = self
            .packages
            .peer_snapshot(&self.sync, self.blocks)?
            .ok_or("negotiation holder lost captured package")?;
        if actual != self.snapshot || actual.to_wire_contract()? != self.wire {
            return Err("negotiation frozen startup projections differ".into());
        }
        let prepared = self
            .packages
            .catalog
            .as_ref()
            .ok_or("negotiation missing catalog")?
            .prepared()
            .values()
            .next()
            .ok_or("negotiation missing prepared pair")?
            .clone();
        let expectation =
            horizon_client::lan::login_negotiation::NegotiationExpectation::from_retained(
                self.sync.clone(),
                self.blocks,
                prepared,
                self.wire.clone(),
            )?;
        // Concurrent immutable callers converge on the holder's single identity.
        let _ = self.expectation.set(expectation);
        Ok(self
            .expectation
            .get()
            .expect("expectation just initialized")
            .clone())
    }
    pub(super) fn wire_contract(&self) -> &WireContract {
        &self.wire
    }
    pub(super) fn sync(&self) -> &SyncData {
        &self.sync
    }
    pub(super) fn blocks(&self) -> &'static BlockRegistry {
        self.blocks
    }
}

fn captured_hex_digest(value: &str) -> std::result::Result<[u8; 32], String> {
    if value.len() != 64 {
        return Err("wire projection requires captured SHA256".into());
    }
    let nibble = |b| match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        _ => Err("wire projection requires lowercase captured SHA256".to_owned()),
    };
    let mut digest = [0; 32];
    for (out, pair) in digest
        .iter_mut()
        .zip(value.as_bytes().as_chunks::<2>().0.iter())
    {
        *out = nibble(pair[0])? * 16 + nibble(pair[1])?;
    }
    Ok(digest)
}

#[allow(dead_code)]
impl PeerSnapshot {
    /// Explicit projection to the separately defined wire v1 data format.
    fn to_wire_contract(&self) -> std::result::Result<WireContract, String> {
        if self.domain != "horizon-peer-snapshot-1/package"
            || self.package_scheme != "paired-folder-sha256-1"
        {
            return Err("unsupported captured snapshot wire projection".into());
        }
        let build = self
            .build
            .strip_prefix("sha256:")
            .ok_or("wire projection requires captured executable SHA256")?;
        WireContract::from_claims(WireClaims {
            protocol: self.protocol,
            game: &self.game,
            build: captured_hex_digest(build)?,
            package_id: &self.package.0,
            package: captured_hex_digest(&self.package.1)?,
            machine_id: &self.schema.0,
            schema: self.schema.1,
            registries: &self.registries,
        })
        .map_err(|e| e.to_string())
    }

    /// Matching data never relaxes Packages::external_network or world pins.
    pub fn compare(&self, other: &Self) -> std::result::Result<(), &'static str> {
        if self.domain != other.domain || self.protocol != other.protocol || self.game != other.game
        {
            return Err("protocol/game version");
        }
        if self.build != other.build {
            return Err("exact executable");
        }
        if self.package_scheme != other.package_scheme || self.package != other.package {
            return Err("captured package/content");
        }
        if self.schema != other.schema {
            return Err("machine save schema");
        }
        if self.registries.builtin != other.registries.builtin {
            return Err("builtin registry");
        }
        if self.registries.blocks != other.registries.blocks {
            return Err("runtime block states/defaults");
        }
        if self.registries.synchronized != other.registries.synchronized {
            return Err("synchronized registry data");
        }
        if self.registries.tags != other.registries.tags {
            return Err("configuration tags");
        }
        if self.registries.features != other.registries.features {
            return Err("enabled features");
        }
        Ok(())
    }
}

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
    /// Dormant local capture only: no claimed build/hash, decoded wire value or
    /// mutable path can be supplied in place of retained startup provenance.
    #[allow(dead_code)]
    pub(super) fn freeze_peer_inputs(
        &self,
        sync: Arc<SyncData>,
        blocks: &'static BlockRegistry,
    ) -> std::result::Result<Option<FrozenHostInputs>, String> {
        let Some(snapshot) = self.peer_snapshot(&sync, blocks)? else {
            return Ok(None);
        };
        let wire = snapshot.to_wire_contract()?;
        // Bounded package capture/preflight precedes cloning its one identity
        // map. The catalog and registry data retain the exact existing owners.
        Ok(Some(FrozenHostInputs {
            packages: self.clone(),
            sync,
            blocks,
            snapshot,
            wire,
            expectation: std::sync::OnceLock::new(),
        }))
    }

    /// Dormant bounded capture. It uses retained startup identities and never
    /// reopens a package path or accepts a caller's asserted executable digest.
    #[allow(dead_code)]
    pub fn peer_snapshot(
        &self,
        sync: &horizon_client::lan::data::SyncData,
        blocks: &horizon_world::registry::BlockRegistry,
    ) -> std::result::Result<Option<PeerSnapshot>, String> {
        if self.identities.is_empty() {
            #[cfg(feature = "paired-packages")]
            if self
                .catalog
                .as_ref()
                .is_some_and(|c| !c.prepared().is_empty())
            {
                return Err("snapshot missing captured package identity".into());
            }
            return Ok(None);
        }
        #[cfg(not(feature = "paired-packages"))]
        {
            let _ = (sync, blocks);
            Err("snapshot requires captured paired packages".into())
        }
        #[cfg(feature = "paired-packages")]
        {
            use horizon_mods::paired::{IDENTITY_SCHEME, catalog::Kind};
            let catalog = self
                .catalog
                .as_ref()
                .ok_or("snapshot missing captured catalog")?;
            if self.identities.len() != 1 || catalog.prepared().len() != 1 {
                return Err("snapshot supports exactly one captured pair".into());
            }
            let mut enabled = catalog.entries().iter().filter(|e| e.enabled);
            let selected = enabled.next().ok_or("snapshot missing selected pair")?;
            if enabled.next().is_some()
                || selected.error.is_some()
                || !matches!(selected.kind, Some(Kind::Paired(_)))
            {
                return Err(
                    "snapshot refuses enabled extra/legacy/unknown package profiles".into(),
                );
            }
            let (path, prepared) = catalog
                .prepared()
                .first_key_value()
                .ok_or("snapshot missing prepared pair")?;
            if selected.path != *path || selected.content.is_none() {
                return Err("snapshot selected/prepared package mismatch".into());
            }
            let (id, identity) = self
                .identities
                .first_key_value()
                .ok_or("snapshot missing package identity")?;
            if prepared.bundle().content_owner().id != *id {
                return Err("snapshot captured package owner mismatch".into());
            }
            // The identity scheme binds descriptor, both captured executables,
            // and captured content. Check its retained startup rendering too.
            if identity.len() != 64
                || !identity
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            {
                return Err("snapshot invalid captured package identity".into());
            }
            for (index, byte) in prepared.identity().iter().enumerate() {
                let digits = b"0123456789abcdef";
                if identity.as_bytes()[index * 2] != digits[(byte >> 4) as usize]
                    || identity.as_bytes()[index * 2 + 1] != digits[(byte & 15) as usize]
                {
                    return Err("snapshot captured package identity mismatch".into());
                }
            }
            if self.build.len() != 71
                || !self.build.starts_with("sha256:")
                || !self.build[7..]
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            {
                return Err("snapshot missing exact startup executable SHA256".into());
            }
            let machines = prepared.content().machine_types().types();
            if machines.len() != 1 || machines[0].save_schema() != 1 {
                return Err("snapshot requires one current machine save schema".into());
            }
            let machine = &machines[0];
            // All strings are checked before clone, registry conversion or hash
            // allocation. Fixed-width framing/domain/version fields count too.
            for value in [
                id.as_str(),
                machine.id(),
                IDENTITY_SCHEME,
                horizon_protocol::GAME_VERSION,
            ] {
                if value.is_empty() || value.len() > 1_024 || value.encode_utf16().count() > 32_767
                {
                    return Err("snapshot package/schema identifier limit".into());
                }
            }
            let reservation = [
                "horizon-peer-snapshot-1/package",
                horizon_protocol::GAME_VERSION,
                self.build.as_str(),
                IDENTITY_SCHEME,
                id,
                identity,
                machine.id(),
            ]
            .iter()
            .try_fold(16usize, |n, value| {
                n.checked_add(8)?.checked_add(value.len())
            })
            .ok_or("snapshot package framing overflow")?;
            // Include every retained selected/prepared/package/schema view.
            let registries =
                horizon_client::lan::data::RegistrySnapshot::capture(sync, blocks, reservation, 4)?;
            Ok(Some(PeerSnapshot {
                domain: "horizon-peer-snapshot-1/package",
                protocol: horizon_protocol::PROTOCOL_VERSION,
                game: horizon_protocol::GAME_VERSION.into(),
                build: self.build.clone(),
                package_scheme: IDENTITY_SCHEME.into(),
                package: (id.clone(), identity.clone()),
                schema: (machine.id().into(), machine.save_schema()),
                registries,
            }))
        }
    }
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

    /// External paired configuration and receipt consumption remain disabled.
    /// Compatibility agreement alone cannot authorize an external connection.
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
#[path = "peer_contract_snapshot_tests.rs"]
mod peer_contract_snapshot_tests;

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
