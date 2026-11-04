//! Dormant managed-world exclusion. All cooperating entries use one trusted namespace.
#![allow(dead_code)] // No ordinary startup, close, networking or item ingress caller.
use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

#[derive(Clone)]
pub(in crate::game) struct AuthorityNamespace {
    root: PathBuf,
}

/// No Clone, raw-UUID constructor, serializable grant or lock-file cleanup.
pub(in crate::game) struct WorldAuthority {
    world: WorldId,
    namespace: PathBuf,
    _authority_lock: SessionLock,
    _anchor_lock: Option<SessionLock>,
    owned: Option<OwnedAssociation>,
}

const OWN_FILE: &str = "owned.json";
const OWN_LIMIT: u64 = 16 << 10;
const PACKAGE_PIN: &str = "horizon-paired-packages.json";

/// Serializable policy, not the original actor, constructor or lock ownership.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub(in crate::game) enum OwnedProfile {
    Local,
    Integrated { owner: String },
    Dedicated,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedRecord {
    format: u32,
    world: WorldId,
    anchor: PathBuf,
    archive: PathBuf,
    destination_parent: PathBuf,
    profile: OwnedProfile,
    native_player: [i32; 4],
    terrain: String,
    spawn_mobs: bool,
    dimension: String,
    package_pin_sha256: String,
    package_identity: String,
    type_id: String,
    block_id: String,
    schema: u32,
}

/// Cloneable route observations cannot acquire a native world or mint admission.
#[derive(Clone)]
pub(in crate::game) struct OwnedAssociation {
    namespace: AuthorityNamespace,
    path: PathBuf,
    encoded: Vec<u8>,
    record: OwnedRecord,
}

/// Explicit trusted creation configuration, never decoded from a world image.
pub(in crate::game) struct NewOwnedRoute {
    namespace: AuthorityNamespace,
    archive_parent: PathBuf,
    destination_parent: PathBuf,
}

fn own_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn own_regular(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if linked(&metadata) || !metadata.is_file() || metadata.len() > limit {
        return Err("owned record requires a bounded regular unlinked file".into());
    }
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() as u64 > limit {
        return Err("owned record byte bound".into());
    }
    Ok(bytes)
}
fn own_sync_directory(path: &Path) -> Result<()> {
    File::open(directory_path(path, true)?)?.sync_all()?;
    Ok(())
}
/// Conservative layout for the explicit OWN profile. UUID components belong to
/// actual archives or internal namespace keys, never caller-configured roots.
/// This rejects nesting beneath a different world before constructor effects.
fn own_route(path: &Path, existing: bool) -> Result<PathBuf> {
    let path = directory_path(path, existing)?;
    if path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|text| WorldId::parse(&text.to_ascii_lowercase()).is_ok())
    }) {
        return Err("owned configured route cannot contain a logical UUID component".into());
    }
    Ok(path)
}
impl NewOwnedRoute {
    pub(in crate::game) fn namespace(&self) -> AuthorityNamespace {
        self.namespace.clone()
    }
    pub(in crate::game) fn matches_namespace(
        &self,
        namespace: Option<&AuthorityNamespace>,
    ) -> bool {
        namespace.is_some_and(|namespace| namespace.root == self.namespace.root)
    }
    pub(in crate::game) fn canonical_new_root(&self, root: &Path) -> Result<PathBuf> {
        self.check_new_root(root)?;
        directory_path(root, false)
    }
    pub(in crate::game) fn check_new_root(&self, root: &Path) -> Result<()> {
        self.namespace.check_new_world(root)?;
        let root = own_route(root, false)?;
        let archive = own_route(&self.archive_parent, true)?;
        let destination = own_route(&self.destination_parent, true)?;
        own_route(&self.namespace.root, true)?;
        for other in [&archive, &destination] {
            separate(&root, other)?;
            separate(&self.namespace.root, other)?;
        }
        separate(&archive, &destination)?;
        Ok(())
    }
}
impl OwnedAssociation {
    pub(in crate::game) fn same_route(&self, other: &Self) -> bool {
        self.namespace.root == other.namespace.root
            && self.path == other.path
            && self.encoded == other.encoded
    }
    pub(in crate::game) fn matches_working_route(&self, root: &Path) -> bool {
        root == self.record.anchor
            || root.parent() == Some(self.record.destination_parent.as_path())
    }
    pub(in crate::game) fn world(&self) -> WorldId {
        self.record.world
    }
    pub(in crate::game) fn anchor(&self) -> &Path {
        &self.record.anchor
    }
    pub(in crate::game) fn archive(&self) -> &Path {
        &self.record.archive
    }
    pub(in crate::game) fn destination_parent(&self) -> &Path {
        &self.record.destination_parent
    }
    pub(in crate::game) fn native_player(&self) -> [i32; 4] {
        self.record.native_player
    }
    pub(in crate::game) fn profile(&self) -> &OwnedProfile {
        &self.record.profile
    }
    pub(in crate::game) fn check_profile(
        &self,
        profile: &OwnedProfile,
        native_player: [i32; 4],
    ) -> Result<()> {
        if &self.record.profile != profile || self.record.native_player != native_player {
            return Err("owned native role/player mapping differs".into());
        }
        Ok(())
    }
    pub(in crate::game) fn type_id(&self) -> &str {
        &self.record.type_id
    }
    pub(in crate::game) fn recheck(&self) -> Result<()> {
        own_route(&self.namespace.root, true)?;
        if self.path
            != self
                .namespace
                .checked_key(self.record.world)?
                .join(OWN_FILE)
            || own_regular(&self.path, OWN_LIMIT)? != self.encoded
        {
            return Err("original owned association changed".into());
        }
        let anchor = own_route(&self.record.anchor, true)?;
        let archive = directory_path(&self.record.archive, true)?;
        own_route(archive.parent().ok_or("owned archive parent absent")?, true)?;
        let destination = own_route(&self.record.destination_parent, true)?;
        for root in [&anchor, &archive, &destination] {
            separate(&self.namespace.root, root)?;
        }
        separate(&anchor, &archive)?;
        separate(&anchor, &destination)?;
        separate(&archive, &destination)?;
        if anchor != self.record.anchor
            || archive != self.record.archive
            || destination != self.record.destination_parent
        {
            return Err("owned canonical route changed".into());
        }
        if archive.file_name().and_then(|name| name.to_str())
            != Some(self.record.world.to_string().as_str())
        {
            return Err("owned archive must be addressed by its actual logical UUID".into());
        }
        Ok(())
    }
    pub(in crate::game) fn check_pin(&self, root: &Path) -> Result<()> {
        if own_digest(&own_regular(&root.join(PACKAGE_PIN), 64 << 10)?)
            != self.record.package_pin_sha256
        {
            return Err("owned exact native executable/package pin differs".into());
        }
        Ok(())
    }
    pub(in crate::game) fn check_binding(
        &self,
        binding: &horizon_client::world::counter_restore::NativeBinding,
    ) -> Result<()> {
        use horizon_client::world::counter_restore::NativeBindingProfile;
        let package = binding
            .package_identity()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if binding.profile() != NativeBindingProfile::RegisteredCube
            || self.record.package_identity != package
            || self.record.type_id != binding.type_id()
            || self.record.block_id != binding.block_id()
            || self.record.schema != 1
        {
            return Err("owned registered Guest binding differs".into());
        }
        Ok(())
    }
    pub(in crate::game) fn check_working_root(&self, root: &Path) -> Result<()> {
        let root = own_route(root, true)?;
        if root != self.record.anchor
            && root.parent() != Some(self.record.destination_parent.as_path())
        {
            return Err("owned working root is outside its bound route".into());
        }
        Ok(())
    }
}

fn linked(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_type().is_symlink() || meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

/// Check every existing directory component before canonicalizing the deepest
/// existing ancestor. A nonexistent final world/destination remains nonexistent.
fn directory_path(path: &Path, existing: bool) -> Result<PathBuf> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(failure(
            "authority path",
            "absolute path without parent traversal required",
        )
        .into());
    }
    let normalized: PathBuf = path
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect();
    let mut nearest = None;
    for at in normalized.ancestors() {
        match fs::symlink_metadata(at) {
            Ok(meta) if meta.is_dir() && !linked(&meta) => {
                if nearest.is_none() {
                    nearest = Some(at);
                }
            }
            Ok(_) => {
                return Err(failure("authority path", "linked or non-directory component").into());
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(failure("authority path", e).into()),
        }
    }
    let nearest = nearest.ok_or_else(|| failure("authority path", "no real ancestor"))?;
    if existing && nearest != normalized {
        return Err(failure("authority path", "required directory is missing").into());
    }
    Ok(fs::canonicalize(nearest)?.join(normalized.strip_prefix(nearest)?))
}

fn lock_path(root: &Path) -> Result<()> {
    directory_path(root, false)?;
    match fs::symlink_metadata(root.join("session.lock")) {
        Ok(meta) if meta.is_file() && !linked(&meta) => Ok(()),
        Ok(_) => Err(failure("authority lock", "linked or nonregular lock target").into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(failure("authority lock", e).into()),
    }
}

fn separate(a: &Path, b: &Path) -> Result<()> {
    if a.starts_with(b) || b.starts_with(a) {
        return Err(failure("authority route", "overlapping trusted roots").into());
    }
    Ok(())
}

impl AuthorityNamespace {
    /// Private explicit coordinator configuration, never derived from an image.
    pub(in crate::game) fn new(root: &Path) -> Result<Self> {
        Ok(Self {
            root: directory_path(root, true)?,
        })
    }

    pub(in crate::game) fn new_owned_route(
        &self,
        archive_parent: &Path,
        destination_parent: &Path,
    ) -> Result<NewOwnedRoute> {
        own_route(&self.root, true)?;
        let archive_parent = own_route(archive_parent, true)?;
        let destination_parent = own_route(destination_parent, true)?;
        separate(&self.root, &archive_parent)?;
        separate(&self.root, &destination_parent)?;
        separate(&archive_parent, &destination_parent)?;
        Ok(NewOwnedRoute {
            namespace: self.clone(),
            archive_parent,
            destination_parent,
        })
    }

    /// This returns observations only. Acquisition below rechecks the same bytes
    /// while holding BOTH the actual anchor and namespace locks.
    fn read_owned(&self, anchor: &Path) -> Result<OwnedAssociation> {
        let anchor = directory_path(anchor, true)?;
        let identity = IdentityAnchor::read(&anchor)?;
        let path = self.checked_key(identity.world)?.join(OWN_FILE);
        let encoded = own_regular(&path, OWN_LIMIT)?;
        let record: OwnedRecord = serde_json::from_slice(&encoded)?;
        let digest = |value: &str| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        };
        let role = match &record.profile {
            OwnedProfile::Local | OwnedProfile::Dedicated => true,
            OwnedProfile::Integrated { owner } => {
                owner.len() == 32
                    && owner
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            }
        };
        if record.format != 1
            || record.world != identity.world
            || record.anchor != anchor
            || record.terrain != "test-terrain"
            || record.spawn_mobs
            || record.dimension != "minecraft:overworld"
            || record.schema != 1
            || !role
            || !digest(&record.package_pin_sha256)
            || !digest(&record.package_identity)
            || record.type_id.is_empty()
            || record.type_id.len() > 256
            || record.block_id.is_empty()
            || record.block_id.len() > 256
        {
            return Err("unsupported or inconsistent owned association".into());
        }
        let association = OwnedAssociation {
            namespace: self.clone(),
            path,
            encoded,
            record,
        };
        association.recheck()?;
        Ok(association)
    }

    /// No image or caller-supplied UUID constructs this ownership. The selected
    /// anchor identifies the immutable route; original locks seal its recheck.
    pub(in crate::game) fn acquire_owned_recovery(
        &self,
        anchor: &Path,
        destination: &Path,
    ) -> Result<WorldAuthority> {
        own_route(destination, false)?;
        let association = self.read_owned(anchor)?;
        let destination = directory_path(destination, false)?;
        if destination.parent() != Some(association.destination_parent()) {
            return Err("owned recovery destination is outside its bound parent".into());
        }
        let mut authority = self.acquire_recovery(anchor, association.archive(), &destination)?;
        association.recheck()?;
        if authority.world != association.world() || authority.namespace != self.root {
            return Err("owned recovery original lock association changed".into());
        }
        authority.owned = Some(association);
        Ok(authority)
    }

    pub(in crate::game) fn check_new_world(&self, root: &Path) -> Result<()> {
        directory_path(&self.root, true)?;
        let root = directory_path(root, false)?;
        separate(&self.root, &root)?;
        lock_path(&root)
    }

    fn checked_key(&self, world: WorldId) -> Result<PathBuf> {
        directory_path(&self.root, true)?;
        let key = self.root.join(world.to_string());
        lock_path(&key)?;
        Ok(key)
    }

    // Only called with identity held under the actual native owner lock.
    fn acquire(&self, world: WorldId, anchor: Option<SessionLock>) -> Result<WorldAuthority> {
        let key = self.checked_key(world)?;
        let lock = SessionLock::acquire(&key)?;
        lock_path(&key)?;
        Ok(WorldAuthority {
            world,
            namespace: self.root.clone(),
            _authority_lock: lock,
            _anchor_lock: anchor,
            owned: None,
        })
    }

    /// Full route preflight MUST precede even the owner lock's first write.
    /// There is deliberately no anchor-only public acquisition function.
    pub(in crate::game) fn acquire_recovery(
        &self,
        owner: &Path,
        archive: &Path,
        destination: &Path,
    ) -> Result<WorldAuthority> {
        let namespace = directory_path(&self.root, true)?;
        let owner = directory_path(owner, true)?;
        let archive = directory_path(archive, true)?;
        let destination = directory_path(destination, false)?;
        for root in [&owner, &archive, &destination] {
            separate(&namespace, root)?;
        }
        separate(&owner, &archive)?;
        separate(&owner, &destination)?;
        separate(&archive, &destination)?;
        directory_path(
            destination.parent().ok_or("recovery destination parent")?,
            true,
        )?;
        if destination.try_exists()? {
            return Err(failure("authority route", "recovery destination exists").into());
        }
        lock_path(&owner)?;
        // Identity-only: a broken/missing native level must not defeat recovery.
        let anchor = IdentityAnchor::read(&owner)?;
        // Read the UUID before acquiring the owner lock, so a bad authority
        // child/lock target refuses without even creating owner/session.lock.
        self.checked_key(anchor.world)?;
        let native_lock = SessionLock::acquire(&owner)?;
        lock_path(&owner)?;
        anchor.recheck()?;
        self.acquire(anchor.world, Some(native_lock))
    }
}

/// Dormant lock-only partial acquisition. The caller's native world lock is a
/// separate owner and is neither consumed nor released by this custody.
/// Existing trusted regular/unlinked path checks are preserved; hostile external
/// rename/inode replacement is not newly hardened by this prerequisite.
pub(in crate::game) struct RetainedAuthorityAcquisition {
    namespace: AuthorityNamespace,
    key: PathBuf,
    world: WorldId,
    lock: horizon_client::save::RetainedSessionLock,
    ready: bool,
    first_failure: Option<String>,
    #[cfg(test)]
    hide_namespace_after_acquire: bool,
}
impl AuthorityNamespace {
    pub(in crate::game) fn retained_acquisition(
        &self,
        world: WorldId,
    ) -> Result<RetainedAuthorityAcquisition> {
        let key = self.checked_key(world)?;
        Ok(RetainedAuthorityAcquisition {
            namespace: self.clone(),
            lock: horizon_client::save::RetainedSessionLock::new(&key),
            key,
            world,
            ready: false,
            first_failure: None,
            #[cfg(test)]
            hide_namespace_after_acquire: false,
        })
    }
}
impl RetainedAuthorityAcquisition {
    pub(in crate::game) fn acquired(&self) -> bool {
        self.lock.acquired()
    }
    pub(in crate::game) fn ready(&self) -> bool {
        self.ready && self.lock.ready()
    }
    pub(in crate::game) fn first_failure(&self) -> Option<&str> {
        self.first_failure.as_deref()
    }
    fn remember<T>(&mut self, result: Result<T>) -> Result<T> {
        if let Err(error) = &result {
            self.first_failure.get_or_insert_with(|| error.to_string());
        }
        result
    }
    fn same_route(&self, namespace: &AuthorityNamespace, world: WorldId) -> Result<()> {
        if namespace.root != self.namespace.root || world != self.world {
            return Err(
                failure("retained authority", "namespace/world correlation mismatch").into(),
            );
        }
        Ok(())
    }
    fn recheck(&self) -> Result<()> {
        if self.namespace.checked_key(self.world)? != self.key {
            return Err(failure("retained authority", "checked key changed").into());
        }
        lock_path(&self.key)
    }
    pub(in crate::game) fn acquire(&mut self) -> Result<()> {
        if self.lock.acquired() {
            return Err("authority lock already acquired; repair same custody".into());
        }
        let result = (|| -> Result<()> {
            self.recheck()?;
            self.lock.acquire()?; // Actual partial File custody already installed.
            #[cfg(test)]
            if self.hide_namespace_after_acquire {
                self.hide_namespace_after_acquire = false;
                fs::rename(
                    &self.namespace.root,
                    self.namespace.root.with_extension("retained-held"),
                )?;
                fs::write(
                    self.namespace.root.components().collect::<PathBuf>(),
                    b"controlled post-lock non-directory namespace",
                )?;
            }
            self.recheck()?;
            self.ready = true;
            Ok(())
        })();
        self.remember(result)
    }
    pub(in crate::game) fn repair(
        &mut self,
        namespace: &AuthorityNamespace,
        world: WorldId,
    ) -> Result<()> {
        self.same_route(namespace, world)?;
        self.ready = false;
        let result = (|| -> Result<()> {
            self.lock.repair(&self.key)?; // Same File, never reopen/reacquire.
            self.recheck()?;
            self.ready = true;
            Ok(())
        })();
        self.remember(result)
    }
    pub(in crate::game) fn finish(
        &mut self,
        namespace: &AuthorityNamespace,
        world: WorldId,
    ) -> Result<WorldAuthority> {
        self.same_route(namespace, world)?;
        if !self.ready() {
            return Err("retained authority is not fully validated".into());
        }
        let result = self.recheck();
        if result.is_err() {
            self.ready = false;
        }
        self.remember(result)?;
        let lock = self.lock.finish(&self.key)?;
        self.ready = false;
        // No fallible operation after taking the actual ready lock.
        Ok(WorldAuthority {
            world: self.world,
            namespace: self.namespace.root.clone(),
            _authority_lock: lock,
            _anchor_lock: None,
            owned: None,
        })
    }
    /// Only the authority lock owned here. Never releases the caller's native
    /// owner lock or claims producer/writer/PreparationClosed completion.
    pub(in crate::game) fn release_lock_only(&mut self) -> Result<()> {
        self.ready = false;
        let result = self.lock.release_lock_only();
        self.remember(result)
    }
}
impl Drop for RetainedAuthorityAcquisition {
    fn drop(&mut self) {
        if self.lock.acquired() {
            use std::io::Write;
            let _ = writeln!(
                std::io::stderr().lock(),
                "fatal: unresolved retained world authority acquisition"
            );
            std::process::abort();
        }
    }
}
struct IdentityAnchor {
    root: PathBuf,
    identity: Vec<u8>,
    world: WorldId,
}
impl IdentityAnchor {
    fn read(root: &Path) -> Result<Self> {
        let meta = fs::symlink_metadata(root.join(IDENTITY_FILE))?;
        if linked(&meta) {
            return Err(failure("authority identity", "linked identity record").into());
        }
        let identity = bounded(&root.join(IDENTITY_FILE), IDENTITY_LIMIT)?;
        let world = decode_identity(&identity)?;
        Ok(Self {
            root: root.to_owned(),
            identity,
            world,
        })
    }
    fn recheck(&self) -> Result<()> {
        directory_path(&self.root, true)?;
        if Self::read(&self.root)?.identity != self.identity {
            return Err(failure("authority identity", "identity changed under native lock").into());
        }
        Ok(())
    }
}

impl IdentityPreflight {
    pub(in crate::game::world_loading) fn acquire_authority(
        &self,
        namespace: AuthorityNamespace,
        root: &Path,
    ) -> Result<WorldAuthority> {
        if !matches!(self.kind, PreflightKind::New { .. }) {
            return Err(failure("authority creation", "archive reopen required").into());
        }
        namespace.check_new_world(root)?;
        self.recheck(root)?;
        namespace.acquire(self.world, None)
    }
}

impl WorldAuthority {
    pub(in crate::game) fn world(&self) -> WorldId {
        self.world
    }
    pub(in crate::game) fn owned(&self) -> Option<&OwnedAssociation> {
        self.owned.as_ref()
    }
}
impl WorldSession {
    /// Only an original new-world session can establish OWN. A recovered lease
    /// retains an anchor lock and cannot relabel its working copy as new.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::game) fn establish_owned(
        &mut self,
        plan: &NewOwnedRoute,
        root: &Path,
        profile: OwnedProfile,
        native_player: [i32; 4],
        binding: &horizon_client::world::counter_restore::NativeBinding,
        world: &horizon_client::world::World,
    ) -> Result<()> {
        plan.check_new_root(root)?;
        let authority = self
            .authority
            .as_mut()
            .ok_or("owned creation requires its original namespace lock")?;
        if authority._anchor_lock.is_some()
            || authority.owned.is_some()
            || authority.namespace != plan.namespace.root
            || self.world != Some(authority.world)
            || self.counter_bootstrap.is_none()
            || world.counter_domain_snapshot().is_some()
            || world.counter_trial_snapshot().is_some()
            || world.world_slot_snapshot().is_some()
        {
            return Err(
                "owned creation requires the original unused native new-world constructor".into(),
            );
        }
        let anchor = directory_path(root, true)?;
        let identity = IdentityAnchor::read(&anchor)?;
        if identity.world != authority.world {
            return Err("owned new native identity changed".into());
        }
        let package_pin_sha256 = own_digest(&own_regular(&anchor.join(PACKAGE_PIN), 64 << 10)?);
        // Actual constructor UUIDs address distinct archives even when two new
        // worlds use one configured parent before either first publication.
        let archive = directory_path(
            &plan.archive_parent.join(authority.world.to_string()),
            false,
        )?;
        fs::create_dir(&archive)?;
        own_sync_directory(&archive)?;
        own_sync_directory(&plan.archive_parent)?;
        let record = OwnedRecord {
            format: 1,
            world: authority.world,
            anchor,
            archive,
            destination_parent: plan.destination_parent.clone(),
            profile,
            native_player,
            terrain: "test-terrain".into(),
            spawn_mobs: false,
            dimension: "minecraft:overworld".into(),
            package_pin_sha256,
            package_identity: binding
                .package_identity()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            type_id: binding.type_id().into(),
            block_id: binding.block_id().into(),
            schema: 1,
        };
        let encoded = serde_json::to_vec(&record)?;
        if encoded.len() as u64 > OWN_LIMIT {
            return Err("owned record encoding bound".into());
        }
        let key = plan.namespace.checked_key(authority.world)?;
        let path = key.join(OWN_FILE);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        own_sync_directory(&key)?;
        own_sync_directory(&plan.namespace.root)?;
        let association = OwnedAssociation {
            namespace: plan.namespace.clone(),
            path,
            encoded,
            record,
        };
        association.recheck()?;
        association.check_binding(binding)?;
        authority.owned = Some(association);
        Ok(())
    }
    pub(in crate::game) fn owned(&self) -> Option<&OwnedAssociation> {
        self.authority.as_ref()?.owned()
    }
    /// Borrowing the option preserves the caller's lease until its unpublished
    /// Game has dropped on error. No failing consuming attachment can release it early.
    pub(in crate::game) fn install_authority(
        &mut self,
        lease: &mut Option<WorldAuthority>,
    ) -> Result<()> {
        let authority = lease.as_ref().ok_or("missing world authority")?;
        if self.world != Some(authority.world) || self.authority.is_some() {
            return Err(failure(
                "authority installation",
                "session identity or existing owner",
            )
            .into());
        }
        self.authority = lease.take();
        Ok(())
    }
    #[cfg(test)]
    pub(in crate::game) fn has_authority(&self) -> bool {
        self.authority.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "horizon-authority-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
        fn dir(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir(&path).unwrap();
            path
        }
        fn anchor(&self, name: &str, identity: &[u8]) -> PathBuf {
            let path = self.dir(name);
            fs::write(path.join(IDENTITY_FILE), identity).unwrap();
            path
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            if std::thread::panicking() {
                eprintln!("AUTHORITY_SCRATCH_RETAINED {}", self.0.display());
            } else {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
    }
    fn namespace(scratch: &Scratch) -> AuthorityNamespace {
        AuthorityNamespace::new(&scratch.dir("authority")).unwrap()
    }
    #[test]
    fn authority_copied_identities_contend_and_drop_releases_real_locks() {
        let scratch = Scratch::new();
        let ns = namespace(&scratch);
        let (_, identity) = allocate_identity().unwrap();
        let a = scratch.anchor("owner-a", &identity);
        let b = scratch.anchor("owner-b", &identity);
        let archive = scratch.dir("archive");
        let lease = ns
            .acquire_recovery(&a, &archive, &scratch.0.join("copy-a"))
            .unwrap();
        let alias = AuthorityNamespace::new(&ns.root.join(".")).unwrap();
        assert!(
            alias
                .acquire_recovery(&b, &archive, &scratch.0.join("copy-b"))
                .is_err()
        );
        assert!(SessionLock::acquire(&a).is_err());
        let (_, other_identity) = allocate_identity().unwrap();
        let c = scratch.anchor("owner-c", &other_identity);
        let other = ns
            .acquire_recovery(&c, &archive, &scratch.0.join("copy-c"))
            .unwrap();
        assert_ne!(lease.world, other.world);
        drop(lease);
        let next = alias
            .acquire_recovery(&b, &archive, &scratch.0.join("copy-b"))
            .unwrap();
        assert_eq!(fs::read(a.join(IDENTITY_FILE)).unwrap(), identity);
        assert_eq!(fs::read(b.join(IDENTITY_FILE)).unwrap(), identity);
        drop(next);
        assert!(a.join("session.lock").is_file());
        assert!(
            ns.root
                .join(decode_identity(&identity).unwrap().to_string())
                .join("session.lock")
                .is_file()
        );
    }

    #[test]
    fn authority_identity_only_anchor_never_requires_or_repairs_native_level() {
        let scratch = Scratch::new();
        let ns = namespace(&scratch);
        let (_, identity) = allocate_identity().unwrap();
        let owner = scratch.anchor("owner", &identity);
        let archive = scratch.dir("archive");
        let destination = scratch.0.join("recovered");
        drop(ns.acquire_recovery(&owner, &archive, &destination).unwrap());
        assert!(!owner.join("level.dat").exists());
        fs::write(owner.join("level.dat"), b"broken native state").unwrap();
        drop(ns.acquire_recovery(&owner, &archive, &destination).unwrap());
        assert_eq!(
            fs::read(owner.join("level.dat")).unwrap(),
            b"broken native state"
        );
        let snapshot = IdentityAnchor::read(&owner).unwrap();
        let (_, changed) = allocate_identity().unwrap();
        fs::write(owner.join(IDENTITY_FILE), &changed).unwrap();
        assert!(snapshot.recheck().is_err());
        assert_eq!(fs::read(owner.join(IDENTITY_FILE)).unwrap(), changed);
        let live = SessionLock::acquire(&owner).unwrap();
        assert!(ns.acquire_recovery(&owner, &archive, &destination).is_err());
        drop(live);
    }

    #[test]
    fn authority_invalid_anchor_records_refuse_without_creating_lock_or_identity() {
        let scratch = Scratch::new();
        let ns = namespace(&scratch);
        let archive = scratch.dir("archive");
        let (_, identity) = allocate_identity().unwrap();
        let mut unknown: serde_json::Value = serde_json::from_slice(&identity).unwrap();
        unknown
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), true.into());
        for (index, bytes) in [
            Vec::new(),
            b"broken".to_vec(),
            vec![b' '; IDENTITY_LIMIT + 1],
            serde_json::to_vec(&unknown).unwrap(),
        ]
        .iter()
        .enumerate()
        {
            let owner = scratch.anchor(&format!("bad-{index}"), bytes);
            assert!(
                ns.acquire_recovery(&owner, &archive, &scratch.0.join("copy"))
                    .is_err()
            );
            assert_eq!(fs::read(owner.join(IDENTITY_FILE)).unwrap(), *bytes);
            assert!(!owner.join("session.lock").exists());
        }
        let missing = scratch.dir("missing-identity");
        assert!(
            ns.acquire_recovery(&missing, &archive, &scratch.0.join("copy"))
                .is_err()
        );
        assert_eq!(fs::read_dir(missing).unwrap().count(), 0);
        assert_eq!(fs::read_dir(&ns.root).unwrap().count(), 0);
    }

    #[test]
    fn authority_route_refusal_precedes_all_lock_writes_including_archived_owner() {
        let scratch = Scratch::new();
        let ns = namespace(&scratch);
        let archive = scratch.dir("archive");
        let epoch = archive.join("epoch-00000000000000000001");
        fs::create_dir(&epoch).unwrap();
        let archived_owner = epoch.join("world");
        fs::create_dir(&archived_owner).unwrap();
        let (_, identity) = allocate_identity().unwrap();
        fs::write(archived_owner.join(IDENTITY_FILE), &identity).unwrap();
        fs::write(epoch.join("manifest.json"), b"immutable sentinel").unwrap();
        assert!(
            ns.acquire_recovery(&archived_owner, &archive, &scratch.0.join("copy"))
                .is_err()
        );
        assert_eq!(
            fs::read(archived_owner.join(IDENTITY_FILE)).unwrap(),
            identity
        );
        assert_eq!(fs::read_dir(&archived_owner).unwrap().count(), 1);
        assert_eq!(
            fs::read(epoch.join("manifest.json")).unwrap(),
            b"immutable sentinel"
        );
        assert_eq!(fs::read_dir(&ns.root).unwrap().count(), 0);
        let owner = scratch.anchor("owner", &identity);
        for destination in [
            archive.join("copy"),
            owner.join("copy"),
            ns.root.join("copy"),
            scratch.0.join("missing").join("..").join("copy"),
        ] {
            assert!(ns.acquire_recovery(&owner, &archive, &destination).is_err());
            assert!(!owner.join("session.lock").exists());
        }
        assert!(ns.check_new_world(&ns.root.join("world")).is_err());
        assert!(ns.check_new_world(&scratch.0).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn authority_linked_namespace_anchor_identity_child_and_lock_preserve_targets() {
        use std::os::unix::fs::symlink;
        let scratch = Scratch::new();
        let ns = namespace(&scratch);
        let archive = scratch.dir("archive");
        let (world, identity) = allocate_identity().unwrap();
        let owner = scratch.anchor("owner", &identity);
        let copy = scratch.0.join("copy");
        let alias = scratch.0.join("namespace-link");
        symlink(&ns.root, &alias).unwrap();
        assert!(AuthorityNamespace::new(&alias).is_err());
        let alias = scratch.0.join("owner-link");
        symlink(&owner, &alias).unwrap();
        assert!(ns.acquire_recovery(&alias, &archive, &copy).is_err());
        assert!(!owner.join("session.lock").exists());
        let other = scratch.dir("other");
        symlink(owner.join(IDENTITY_FILE), other.join(IDENTITY_FILE)).unwrap();
        assert!(ns.acquire_recovery(&other, &archive, &copy).is_err());
        assert!(!other.join("session.lock").exists());
        let target = scratch.dir("target");
        let key = ns.root.join(world.to_string());
        let child_owner = scratch.anchor("child-owner", &identity);
        symlink(&target, &key).unwrap();
        assert!(ns.acquire_recovery(&child_owner, &archive, &copy).is_err());
        assert_eq!(fs::read_dir(&child_owner).unwrap().count(), 1);
        assert_eq!(fs::read(child_owner.join(IDENTITY_FILE)).unwrap(), identity);
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        fs::remove_file(&key).unwrap();
        fs::create_dir(&key).unwrap();
        let sentinel = target.join("sentinel");
        fs::write(&sentinel, b"unchanged").unwrap();
        symlink(&sentinel, key.join("session.lock")).unwrap();
        let lock_owner = scratch.anchor("lock-owner", &identity);
        assert!(ns.acquire_recovery(&lock_owner, &archive, &copy).is_err());
        assert_eq!(fs::read_dir(&lock_owner).unwrap().count(), 1);
        assert_eq!(fs::read(lock_owner.join(IDENTITY_FILE)).unwrap(), identity);
        assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");
        assert_eq!(fs::read(owner.join(IDENTITY_FILE)).unwrap(), identity);
    }

    #[test]
    fn authority_new_identity_lock_failure_precedes_identity_publication() {
        let scratch = Scratch::new();
        let ns = namespace(&scratch);
        let root = scratch.0.join("new-world");
        let preflight = IdentityPreflight::read(&root, IdentityIntent::CreateNewIdentity)
            .unwrap()
            .unwrap();
        let native = SessionLock::acquire(&root).unwrap();
        preflight.recheck(&root).unwrap();
        let key = ns.root.join(preflight.world.to_string());
        let held = SessionLock::acquire(&key).unwrap();
        assert!(preflight.acquire_authority(ns.clone(), &root).is_err());
        assert!(!root.join(IDENTITY_FILE).exists());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        drop(held);
        let mut lease = Some(preflight.acquire_authority(ns.clone(), &root).unwrap());
        let mut session = preflight.finish(&root, native).unwrap();
        session.install_authority(&mut lease).unwrap();
        assert!(lease.is_none() && session.has_authority());
        assert!(SessionLock::acquire(&key).is_err());
        drop(session);
        assert!(SessionLock::acquire(&key).is_ok());
        assert!(root.join(IDENTITY_FILE).is_file());
    }
    #[test]
    fn retained_authority_route_finish_and_lock_only_release_preserve_native_owner() {
        let scratch = Scratch::new();
        let ns = namespace(&scratch);
        let other_ns = AuthorityNamespace::new(&scratch.dir("other-authority")).unwrap();
        let (world, _) = allocate_identity().unwrap();
        let (other_world, _) = allocate_identity().unwrap();
        let native_root = scratch.dir("native-owner");
        let native = SessionLock::acquire(&native_root).unwrap();
        let mut custody = ns.retained_acquisition(world).unwrap();
        custody.acquire().unwrap();
        let key = ns.checked_key(world).unwrap();
        let wrong_namespace = custody.finish(&other_ns, world).is_err();
        let wrong_world = custody.finish(&ns, other_world).is_err();
        let wrong_repair = custody.repair(&ns, other_world).is_err();
        let excluded = SessionLock::acquire(&key).is_err();
        custody.release_lock_only().unwrap();
        let authority_released = SessionLock::acquire(&key).is_ok();
        let native_retained = SessionLock::acquire(&native_root).is_err();
        drop(native);
        assert!(
            wrong_namespace
                && wrong_world
                && wrong_repair
                && excluded
                && authority_released
                && native_retained
        );
    }
    #[test]
    fn retained_authority_actual_postlock_path_failure_keeps_original_file_until_repair() {
        let scratch = Scratch::new();
        let ns = namespace(&scratch);
        let (world, _) = allocate_identity().unwrap();
        let mut custody = ns.retained_acquisition(world).unwrap();
        custody.hide_namespace_after_acquire = true;
        let failed = custody.acquire().is_err();
        let acquired_partial = custody.acquired() && !custody.ready();
        let backup = ns.root.with_extension("retained-held");
        let moved_key = backup.join(world.to_string());
        let excluded = SessionLock::acquire(&moved_key).is_err();
        let failed_finish = custody.finish(&ns, world).is_err();
        let first = custody.first_failure().unwrap().to_owned();
        fs::remove_file(ns.root.components().collect::<PathBuf>()).unwrap();
        fs::rename(&backup, &ns.root).unwrap();
        custody.repair(&ns, world).unwrap();
        let first_retained = custody.first_failure() == Some(first.as_str());
        let lease = custody.finish(&ns, world).unwrap();
        let key = ns.checked_key(world).unwrap();
        let transferred = SessionLock::acquire(&key).is_err();
        let consumed = !custody.acquired();
        drop(lease);
        assert!(
            failed
                && acquired_partial
                && excluded
                && failed_finish
                && first_retained
                && transferred
                && consumed
        );
        assert!(SessionLock::acquire(&key).is_ok());
        assert_eq!(fs::read(key.join("session.lock")).unwrap(), [0x26, 0x03]);
    }
    #[test]
    fn retained_authority_actual_finish_revalidation_failure_retains_custody() {
        let scratch = Scratch::new();
        let ns = namespace(&scratch);
        let (world, _) = allocate_identity().unwrap();
        let mut custody = ns.retained_acquisition(world).unwrap();
        custody.acquire().unwrap();
        let backup = ns.root.with_extension("retained-held");
        fs::rename(&ns.root, &backup).unwrap();
        fs::write(
            ns.root.components().collect::<PathBuf>(),
            b"actual non-directory before finish",
        )
        .unwrap();
        let failed = custody.finish(&ns, world).is_err();
        let retained = custody.acquired() && !custody.ready();
        let failed_repair = custody.repair(&ns, world).is_err();
        let excluded = SessionLock::acquire(&backup.join(world.to_string())).is_err();
        fs::remove_file(ns.root.components().collect::<PathBuf>()).unwrap();
        fs::rename(&backup, &ns.root).unwrap();
        custody.repair(&ns, world).unwrap();
        let lease = custody.finish(&ns, world).unwrap();
        assert_eq!(lease.world(), world);
        drop(lease);
        assert!(failed && retained && failed_repair && excluded);
    }
    #[test]
    fn retained_authority_prelock_path_refusal_never_creates_capability() {
        let scratch = Scratch::new();
        let ns = namespace(&scratch);
        let (world, _) = allocate_identity().unwrap();
        let mut custody = ns.retained_acquisition(world).unwrap();
        let backup = ns.root.with_extension("retained-held");
        fs::rename(&ns.root, &backup).unwrap();
        fs::write(
            ns.root.components().collect::<PathBuf>(),
            b"actual invalid namespace",
        )
        .unwrap();
        let failed = custody.acquire().is_err();
        let vacant = !custody.acquired() && !custody.ready();
        fs::remove_file(ns.root.components().collect::<PathBuf>()).unwrap();
        fs::rename(&backup, &ns.root).unwrap();
        let untouched = fs::read_dir(&ns.root).unwrap().count() == 0;
        custody.acquire().unwrap();
        custody.release_lock_only().unwrap();
        assert!(failed && vacant && untouched);
    }
    #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
    #[test]
    fn retained_authority_partial_native_header_is_visible_across_library_boundary() {
        use horizon_client::save::{RetainedLockTestFault, RetainedSessionLock};
        let scratch = Scratch::new();
        let ns = namespace(&scratch);
        let (world, _) = allocate_identity().unwrap();
        let mut custody = ns.retained_acquisition(world).unwrap();
        custody.lock = RetainedSessionLock::with_test_fault(
            &custody.key,
            RetainedLockTestFault::PartialHeader,
        );
        let failed = custody.acquire().is_err();
        let handle = custody.lock.test_handle();
        let partial = custody.acquired() && !custody.ready();
        let first = custody.first_failure().unwrap().to_owned();
        let excluded = SessionLock::acquire(&custody.key).is_err();
        custody.repair(&ns, world).unwrap();
        let same_file = custody.lock.test_handle() == handle;
        let original_error = custody.first_failure() == Some(first.as_str());
        let lease = custody.finish(&ns, world).unwrap();
        let header = fs::read(custody.key.join("session.lock")).unwrap();
        drop(lease);
        assert!(failed && partial && excluded && same_file && original_error);
        assert_eq!(header, [0x26, 0x03]);
    }
    #[test]
    #[ignore = "separate direct prebuilt process: marked fatal unresolved post-acquisition authority"]
    fn retained_authority_partial_unresolved_drop_fatal() {
        use std::io::Write;
        let scratch = Scratch::new();
        let ns = namespace(&scratch);
        let (world, _) = allocate_identity().unwrap();
        let mut custody = ns.retained_acquisition(world).unwrap();
        custody.hide_namespace_after_acquire = true;
        assert!(custody.acquire().is_err());
        assert!(custody.acquired() && !custody.ready());
        assert!(
            SessionLock::acquire(
                &ns.root
                    .with_extension("retained-held")
                    .join(world.to_string())
            )
            .is_err()
        );
        println!("HORIZON_RETAINED_AUTHORITY_PARTIAL_DROP_ARMED");
        std::io::stdout().flush().unwrap();
        drop(custody);
        panic!("unresolved authority unexpectedly dropped");
    }
    #[test]
    #[ignore = "separate direct prebuilt process: marked fatal unresolved ready authority"]
    fn retained_authority_ready_unresolved_drop_fatal() {
        use std::io::Write;
        let scratch = Scratch::new();
        let ns = namespace(&scratch);
        let (world, _) = allocate_identity().unwrap();
        let mut custody = ns.retained_acquisition(world).unwrap();
        custody.acquire().unwrap();
        assert!(custody.acquired() && custody.ready());
        assert!(SessionLock::acquire(&custody.key).is_err());
        println!("HORIZON_RETAINED_AUTHORITY_READY_DROP_ARMED");
        std::io::stdout().flush().unwrap();
        drop(custody);
        panic!("unresolved ready authority unexpectedly dropped");
    }
}
