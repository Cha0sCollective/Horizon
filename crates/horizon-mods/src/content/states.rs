//! Experimental, immutable state planning, without registration or behavior code.
//!
//! Format 1 is `{ "format": 1, "behavior": "static", "properties": [
//! { "name": "axis", "values": ["x", "y", "z"], "default": "y" } ] }`.
//! Property names and values sort lexicographically. The rightmost property
//! varies fastest; each state exposes its sorted `(property, value)` tuples.
//! This preview does not make that declaration admissible as block content.
use serde::Deserialize;
use std::collections::BTreeSet;

pub const MAX_DECLARATION_BYTES: usize = 64 << 10;
pub const MAX_PROPERTIES: usize = 16;
pub const MAX_VALUES: usize = 64;
pub const MAX_STATES: usize = 4096;
const MAX_TOKEN_BYTES: usize = 64;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Declaration {
    format: u32,
    behavior: Behavior,
    properties: Vec<PropertyDeclaration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PropertyDeclaration {
    name: String,
    values: Vec<String>,
    default: String,
}

/// The sole preview behavior: static content with no machine callback or record.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Behavior {
    Static,
}

/// One validated property. Private fields prevent bypassing its invariants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Property {
    name: String,
    values: Vec<String>,
    default: String,
}

impl Property {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn values(&self) -> &[String] {
        &self.values
    }

    pub fn default_value(&self) -> &str {
        &self.default
    }
}

/// One canonical state. Its position in the plan is a local ordinal, not an ID
/// suitable for a save or network message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    properties: Vec<(String, String)>,
}

impl State {
    pub fn properties(&self) -> &[(String, String)] {
        &self.properties
    }
}

/// A validated and bounded Cartesian plan. Neither this nor its properties
/// implement Deserialize: every public construction passes the full validator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatePlan {
    behavior: Behavior,
    properties: Vec<Property>,
    states: Vec<State>,
    default_index: usize,
}

fn token(value: &str) -> bool {
    (1..=MAX_TOKEN_BYTES).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

impl StatePlan {
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_DECLARATION_BYTES {
            return Err(format!(
                "state declaration exceeds {MAX_DECLARATION_BYTES} bytes"
            ));
        }
        let declaration: Declaration =
            serde_json::from_str(text).map_err(|e| format!("state declaration: {e}"))?;
        if declaration.format != 1 {
            return Err("state declaration requires format 1".into());
        }
        if !(1..=MAX_PROPERTIES).contains(&declaration.properties.len()) {
            return Err(format!("state plan needs 1..={MAX_PROPERTIES} properties"));
        }
        let mut names = BTreeSet::new();
        let mut properties = Vec::new();
        let mut count = 1usize;
        for property in declaration.properties {
            if !token(&property.name) {
                return Err(format!("invalid state property name {:?}", property.name));
            }
            if !names.insert(property.name.clone()) {
                return Err(format!("duplicate state property {}", property.name));
            }
            if !(1..=MAX_VALUES).contains(&property.values.len()) {
                return Err(format!(
                    "property {} needs 1..={MAX_VALUES} values",
                    property.name
                ));
            }
            let mut values = property.values;
            if values.iter().any(|value| !token(value)) {
                return Err(format!("property {} has an invalid value", property.name));
            }
            values.sort();
            if values.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err(format!("property {} has duplicate values", property.name));
            }
            if values.binary_search(&property.default).is_err() {
                return Err(format!(
                    "property {} default is not an allowed value",
                    property.name
                ));
            }
            count = count
                .checked_mul(values.len())
                .filter(|count| *count <= MAX_STATES)
                .ok_or_else(|| format!("state expansion exceeds {MAX_STATES} states"))?;
            properties.push(Property {
                name: property.name,
                values,
                default: property.default,
            });
        }
        properties.sort_by(|a, b| a.name.cmp(&b.name));
        let default_index = properties.iter().fold(0, |index, property| {
            index * property.values.len()
                + property
                    .values
                    .binary_search(&property.default)
                    .expect("validated default")
        });
        let mut states = Vec::with_capacity(count);
        for ordinal in 0..count {
            let mut remainder = ordinal;
            let mut pairs = Vec::with_capacity(properties.len());
            for property in properties.iter().rev() {
                let value = &property.values[remainder % property.values.len()];
                remainder /= property.values.len();
                pairs.push((property.name.clone(), value.clone()));
            }
            pairs.reverse();
            states.push(State { properties: pairs });
        }
        Ok(Self {
            behavior: declaration.behavior,
            properties,
            states,
            default_index,
        })
    }

    pub fn behavior(&self) -> Behavior {
        self.behavior
    }

    pub fn properties(&self) -> &[Property] {
        &self.properties
    }

    pub fn states(&self) -> &[State] {
        &self.states
    }

    pub fn default_index(&self) -> usize {
        self.default_index
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn property(name: &str, values: &[&str], default: &str) -> Value {
        json!({"name": name, "values": values, "default": default})
    }

    fn declaration(properties: Vec<Value>) -> Value {
        json!({"format": 1, "behavior": "static", "properties": properties})
    }

    fn parse(value: &Value) -> Result<StatePlan, String> {
        StatePlan::parse(&value.to_string())
    }

    #[test]
    fn canonical_order_and_nonfirst_defaults_do_not_depend_on_input_order() {
        let first = parse(&declaration(vec![
            property("powered", &["true", "false"], "true"),
            property("axis", &["z", "x", "y"], "y"),
        ]))
        .unwrap();
        let reordered = parse(&declaration(vec![
            property("axis", &["y", "z", "x"], "y"),
            property("powered", &["false", "true"], "true"),
        ]))
        .unwrap();
        assert_eq!(first, reordered);
        assert_eq!(first.behavior(), Behavior::Static);
        assert_eq!(first.properties()[0].name(), "axis");
        assert_eq!(first.properties()[0].values(), ["x", "y", "z"]);
        assert_eq!(first.properties()[0].default_value(), "y");
        let expected = [
            [("axis", "x"), ("powered", "false")],
            [("axis", "x"), ("powered", "true")],
            [("axis", "y"), ("powered", "false")],
            [("axis", "y"), ("powered", "true")],
            [("axis", "z"), ("powered", "false")],
            [("axis", "z"), ("powered", "true")],
        ];
        for (state, expected) in first.states().iter().zip(expected) {
            let pairs: Vec<_> = state
                .properties()
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect();
            assert_eq!(pairs, expected);
        }
        assert_eq!(first.states().len(), 6);
        assert_eq!(first.default_index(), 3);
    }

    #[test]
    fn duplicate_properties_values_and_invalid_defaults_refuse() {
        for properties in [
            vec![property("axis", &["x"], "x"), property("axis", &["y"], "y")],
            vec![property("axis", &["x", "x"], "x")],
            vec![property("axis", &["x", "y"], "z")],
            vec![property("axis", &["x"], "")],
            vec![property("axis", &[], "x")],
        ] {
            assert!(parse(&declaration(properties)).is_err());
        }
    }

    #[test]
    fn tokens_reject_resource_paths_separators_unicode_and_oversize() {
        for invalid in [
            "", "Axis", "a/b", "a.b", "a:b", "a=b", "a b", "a-b", "é", "[x]",
        ] {
            assert!(parse(&declaration(vec![property(invalid, &["x"], "x")])).is_err());
            assert!(parse(&declaration(vec![property("axis", &[invalid], invalid)])).is_err());
        }
        assert!(parse(&declaration(vec![property(&"x".repeat(65), &["x"], "x")])).is_err());
        let long = "x".repeat(65);
        assert!(parse(&declaration(vec![property("axis", &[&long], &long)])).is_err());
        assert!(
            parse(&declaration(vec![property(
                "level_1",
                &["0", "1", "true"],
                "0"
            )]))
            .is_ok()
        );
    }

    #[test]
    fn declaration_versions_unknown_fields_and_duplicate_json_fields_refuse() {
        let good = declaration(vec![property("axis", &["x"], "x")]);
        for malformed in ["", "null", "{}", "[]"] {
            assert!(StatePlan::parse(malformed).is_err());
        }
        let mut wrong = good.clone();
        wrong["format"] = json!(2);
        assert!(parse(&wrong).is_err());
        wrong = good.clone();
        wrong["behavior"] = json!("machine");
        assert!(parse(&wrong).is_err());
        wrong = good.clone();
        wrong["unknown"] = json!(true);
        assert!(parse(&wrong).is_err());
        wrong = good.clone();
        wrong["properties"][0]["unknown"] = json!(true);
        assert!(parse(&wrong).is_err());
        let text = good.to_string();
        assert!(
            StatePlan::parse(&text.replacen("\"format\":1", "\"format\":1,\"format\":1", 1))
                .is_err()
        );
        assert!(
            StatePlan::parse(&text.replacen(
                "\"name\":\"axis\"",
                "\"name\":\"axis\",\"name\":\"axis\"",
                1
            ))
            .is_err()
        );
        assert!(StatePlan::parse(&" ".repeat(MAX_DECLARATION_BYTES + 1)).is_err());
    }

    #[test]
    fn expansion_and_each_dimension_are_bounded_before_state_allocation() {
        let values: Vec<_> = (0..64).map(|n| format!("v{n:02}")).collect();
        let names: Vec<_> = values.iter().map(String::as_str).collect();
        let full = parse(&declaration(vec![
            property("a", &names, "v63"),
            property("b", &names, "v63"),
        ]))
        .unwrap();
        assert_eq!(full.states().len(), MAX_STATES);
        assert_eq!(full.default_index(), MAX_STATES - 1);
        assert!(
            parse(&declaration(vec![
                property("a", &names, "v00"),
                property("b", &names, "v00"),
                property("c", &["false", "true"], "false"),
            ]))
            .is_err()
        );
        let mut too_many = names.clone();
        too_many.push("extra");
        assert!(parse(&declaration(vec![property("a", &too_many, "v00")])).is_err());
        assert!(parse(&declaration(Vec::new())).is_err());
        let properties: Vec<_> = (0..MAX_PROPERTIES)
            .map(|i| property(&format!("p{i}"), &["one"], "one"))
            .collect();
        assert_eq!(
            parse(&declaration(properties.clone()))
                .unwrap()
                .states()
                .len(),
            1
        );
        let mut too_many = properties;
        too_many.push(property("extra", &["one"], "one"));
        assert!(parse(&declaration(too_many)).is_err());
    }

    #[test]
    fn unsupported_states_remain_inadmissible_as_legacy_block_content() {
        let legacy = r#"{"template":"minecraft:iron_block"}"#;
        assert!(serde_json::from_str::<super::super::BlockFile>(legacy).is_ok());
        let mut custom: Value = serde_json::from_str(legacy).unwrap();
        custom["states"] = declaration(vec![property("axis", &["x", "y", "z"], "y")]);
        assert!(serde_json::from_value::<super::super::BlockFile>(custom).is_err());
    }
}
