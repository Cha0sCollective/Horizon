//! Experimental immutable machine type metadata from captured content only.
//! No instances, payloads, callbacks or save guarantees are established here.
use serde::Deserialize;

pub const MAX_DECLARATION_BYTES: usize = 64 << 10;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Declaration {
    format: u32,
    #[serde(rename = "save-schema")]
    save_schema: u32,
}

/// Package-owned identity and the declared schema version, not live state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineType {
    id: String,
    save_schema: u32,
}
impl MachineType {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn save_schema(&self) -> u32 {
        self.save_schema
    }
}

/// This preview admits at most one type. Construction is confined to content
/// validation; callers can inspect but cannot mutate declarations or bindings.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    types: Vec<MachineType>,
}
impl Catalog {
    pub fn types(&self) -> &[MachineType] {
        &self.types
    }
    pub fn get(&self, id: &str) -> Option<&MachineType> {
        self.types.iter().find(|machine| machine.id == id)
    }
    pub(super) fn declaration(owner: &str, name: &str, bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_DECLARATION_BYTES {
            return Err(format!(
                "machine declaration exceeds {MAX_DECLARATION_BYTES} bytes"
            ));
        }
        super::check_path(name)?;
        let prefix = format!("data/{owner}/horizon/machine/");
        let path = name
            .strip_prefix(&prefix)
            .and_then(|p| p.strip_suffix(".json"))
            .filter(|p| !p.is_empty())
            .ok_or("machine declaration needs a primary-owner path")?;
        super::check_path(&format!("data/{owner}/{path}"))?;
        let declaration: Declaration =
            serde_json::from_slice(bytes).map_err(|e| format!("machine declaration: {e}"))?;
        if declaration.format != 1 || declaration.save_schema != 1 {
            return Err("machine declaration requires format 1 and save-schema 1".into());
        }
        Ok(Self {
            types: vec![MachineType {
                id: format!("{owner}:{path}"),
                save_schema: declaration.save_schema,
            }],
        })
    }
    pub(super) fn binding(&self, owner: &str, id: &str) -> Result<(), String> {
        let (namespace, path) = id
            .split_once(':')
            .ok_or("machine-type needs a namespaced ID")?;
        if namespace != owner || path.is_empty() {
            return Err("machine-type must name a primary-owner declaration".into());
        }
        super::check_path(&format!("data/{owner}/{path}"))?;
        self.get(id)
            .ok_or_else(|| format!("machine-type {id} has no declaration"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const PATH: &str = "data/ruby/horizon/machine/counter.json";
    const DECLARATION: &[u8] = br#"{"format":1,"save-schema":1}"#;

    #[test]
    fn malformed_and_unsupported_machine_schemas_refuse() {
        for invalid in [
            "",
            "null",
            "[]",
            "{}",
            "{",
            r#"{"format":1}"#,
            r#"{"format":1,"save-schema":null}"#,
            r#"{"format":1,"save-schema":1,"payload":{}}"#,
            r#"{"format":1,"format":1,"save-schema":1}"#,
            r#"{"format":1,"save-schema":1,"save-schema":1}"#,
            r#"{"format":1,"save-schema":1,"owner":"other"}"#,
            r#"{"format":1,"save-schema":1,"id":"other:counter"}"#,
            r#"{"format":1,"save-schema":1,"hash":"abc"}"#,
            r#"{"format":2,"save-schema":1}"#,
            r#"{"format":1,"save-schema":2}"#,
            r#"{"format":1,"save-schema":0}"#,
            r#"{"format":1,"save-schema":-1}"#,
            r#"{"format":1,"save-schema":1.0}"#,
        ] {
            assert!(
                Catalog::declaration("ruby", PATH, invalid.as_bytes()).is_err(),
                "{invalid}"
            );
        }
        let mut boundary = DECLARATION.to_vec();
        boundary.resize(MAX_DECLARATION_BYTES, b' ');
        assert!(Catalog::declaration("ruby", PATH, &boundary).is_ok());
        boundary.push(b' ');
        assert!(
            Catalog::declaration("ruby", PATH, &boundary)
                .unwrap_err()
                .contains("exceeds")
        );
    }

    #[test]
    fn machine_ids_derive_only_from_checked_primary_owner_paths() {
        for invalid in [
            "data/alias/horizon/machine/counter.json",
            "data/ruby/horizon/machine/.json",
            "data/ruby/horizon/machine/..json",
            "data/ruby/horizon/machine/...json",
            "data/ruby/horizon/machine/../counter.json",
            "data/ruby/horizon/machine/a//b.json",
            "data/ruby/horizon/machine/Counter.json",
            "data/ruby/horizon/machine/a:b.json",
        ] {
            assert!(
                Catalog::declaration("ruby", invalid, DECLARATION).is_err(),
                "{invalid}"
            );
        }
        let catalog = Catalog::declaration("ruby", PATH, DECLARATION).unwrap();
        for invalid in [
            "",
            "counter",
            "other:counter",
            "ruby:",
            "ruby:.",
            "ruby:..",
            "ruby:a//b",
            "ruby:a:b",
            "ruby:Counter",
            "ruby:missing",
        ] {
            assert!(catalog.binding("ruby", invalid).is_err(), "{invalid}");
        }
        catalog.binding("ruby", "ruby:counter").unwrap();
        assert_eq!(catalog.get("ruby:counter").unwrap().save_schema(), 1);
    }
}
