//! Experimental paired-folder preflight, not a game loader or a new WIT API.
//! Each bundle has one content owner and two separately planned executables.
use crate::{Capability, Content, Manifest, Side};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path};

mod capture;

pub const DESCRIPTOR: &str = "horizon-paired.toml";
pub const CONTRACT: &str = "paired-preview-1";
pub const IDENTITY_SCHEME: &str = "paired-folder-sha256-1";
const MAX_DESCRIPTOR: u64 = 64 << 10;
const MAX_COMPONENT: u64 = 32 << 20;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    format: u32,
    #[serde(rename = "host-contract")]
    host_contract: String,
    content: Manifest,
    client: ComponentSpec,
    server: ComponentSpec,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ComponentSpec {
    entry: String,
    api: String,
    #[serde(default)]
    capabilities: Vec<Capability>,
    #[serde(default)]
    experimental: Vec<Capability>,
}

/// Validated declarations. Fields are private so plans cannot bypass validation.
pub struct Bundle {
    owner: Manifest,
    client: Executable,
    server: Executable,
}

pub struct Executable {
    entry: String,
    manifest: Manifest,
}
impl Executable {
    pub fn entry(&self) -> &str {
        &self.entry
    }
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Client,
    Server,
    /// The fallback running both sides in one process, still in separate sandboxes.
    InFrame,
}

/// Selection only: this does not instantiate, admit or register anything.
pub struct Plan<'a> {
    pub content_owner: &'a Manifest,
    pub executables: Vec<&'a Executable>,
}

fn validate_manifest(manifest: &Manifest) -> Result<Manifest, String> {
    // Reuse the legacy validator rather than maintaining another permission table.
    let text = toml::to_string(manifest).map_err(|e| e.to_string())?;
    Manifest::parse(&text)
}

fn executable(owner: &Manifest, spec: ComponentSpec, side: Side) -> Result<Executable, String> {
    let expected = if side == Side::Server {
        "server.wasm"
    } else {
        "client.wasm"
    };
    if spec.entry != expected {
        return Err(format!("paired {side:?} entry must be {expected}"));
    }
    if spec.capabilities.contains(&Capability::Registries) {
        return Err("paired executables cannot own content; use the content owner".into());
    }
    let mut manifest = owner.clone();
    manifest.api = spec.api;
    manifest.side = side;
    manifest.capabilities = spec.capabilities;
    manifest.experimental = spec.experimental;
    // Aliases belong to the single content owner, not two new registry owners.
    manifest.namespaces.clear();
    Ok(Executable {
        entry: spec.entry,
        manifest: validate_manifest(&manifest)?,
    })
}

impl Bundle {
    pub fn parse(text: &str) -> Result<Self, String> {
        let descriptor: Descriptor =
            toml::from_str(text).map_err(|e| format!("{DESCRIPTOR}: {e}"))?;
        if descriptor.format != 1 || descriptor.host_contract != CONTRACT {
            return Err(format!(
                "paired package needs format 1 and host-contract {CONTRACT}"
            ));
        }
        let owner = validate_manifest(&descriptor.content)?;
        check_owners([&owner])?;
        if owner.api != "0.2"
            || owner.side != Side::Both
            || owner.capabilities != [Capability::Registries]
            || !owner.experimental.is_empty()
        {
            return Err(
                "paired content must be API 0.2, side both, capability registries only".into(),
            );
        }
        let client = executable(&owner, descriptor.client, Side::Client)?;
        let server = executable(&owner, descriptor.server, Side::Server)?;
        Ok(Self {
            owner,
            client,
            server,
        })
    }
    pub fn content_owner(&self) -> &Manifest {
        &self.owner
    }

    pub fn plan(&self, role: Role) -> Plan<'_> {
        let executables = match role {
            Role::Client => vec![&self.client],
            Role::Server => vec![&self.server],
            Role::InFrame => vec![&self.client, &self.server],
        };
        Plan {
            content_owner: &self.owner,
            executables,
        }
    }
}

/// Reject namespace collisions across paired and legacy content owners.
/// Pass each package's owner once, regardless of its executable count.
pub fn check_owners<'a>(owners: impl IntoIterator<Item = &'a Manifest>) -> Result<(), String> {
    let mut claimed = BTreeMap::new();
    for owner in owners {
        for namespace in owner.owned_namespaces() {
            if crate::manifest::RESERVED_NAMESPACES.contains(&namespace) {
                return Err(format!(
                    "namespace {namespace} is reserved, not a package's to own"
                ));
            }
            if let Some(previous) = claimed.insert(namespace, owner.id.as_str()) {
                return Err(format!(
                    "packages {previous} and {} both own namespace {namespace}",
                    owner.id
                ));
            }
        }
    }
    Ok(())
}

/// Immutable captured code/content and parsed declarations. Code remains
/// unvalidated until typed preflight; identity is not runtime or peer admission.
pub struct PreparedBundle {
    bundle: Bundle,
    content: Content,
    client: Vec<u8>,
    server: Vec<u8>,
    descriptor: Vec<u8>,
    identity: [u8; 32],
}
impl PreparedBundle {
    /// Requires both halves even when only one side will execute.
    /// This folder-only prototype is intentionally absent from `manifest::find`.
    pub fn read_folder(path: &Path) -> Result<Self, String> {
        let folder = capture::Folder::open(path)?;
        let descriptor = folder.file(DESCRIPTOR, MAX_DESCRIPTOR)?;
        let text =
            std::str::from_utf8(&descriptor).map_err(|_| format!("{DESCRIPTOR}: not UTF-8"))?;
        let bundle = Bundle::parse(text)?;
        let client = folder.file("client.wasm", MAX_COMPONENT)?;
        let server = folder.file("server.wasm", MAX_COMPONENT)?;
        let files = folder.content()?;
        let mut digest = Sha256::new();
        digest.update(IDENTITY_SCHEME.as_bytes());
        digest.update([0]);
        digest.update((files.len() as u64 + 3).to_le_bytes());
        for (name, bytes) in [
            (DESCRIPTOR, descriptor.as_slice()),
            ("client.wasm", &client),
            ("server.wasm", &server),
        ]
        .into_iter()
        .chain(
            files
                .iter()
                .map(|(name, bytes)| (name.as_str(), bytes.as_slice())),
        ) {
            digest.update((name.len() as u64).to_le_bytes());
            digest.update(name.as_bytes());
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(bytes);
        }
        let identity = digest.finalize().into();
        let content = Content::captured(path.to_owned(), bundle.content_owner(), files, identity)?;
        Ok(Self {
            bundle,
            content,
            client,
            server,
            descriptor,
            identity,
        })
    }
    pub fn identity(&self) -> &[u8; 32] {
        &self.identity
    }
    pub fn descriptor(&self) -> &[u8] {
        &self.descriptor
    }
    pub fn bundle(&self) -> &Bundle {
        &self.bundle
    }
    pub fn content(&self) -> &Content {
        &self.content
    }
    pub fn code(&self, side: Side) -> Result<&[u8], String> {
        match side {
            Side::Client => Ok(&self.client),
            Side::Server => Ok(&self.server),
            Side::Both => Err("select one sandbox side, not both".into()),
        }
    }
}
