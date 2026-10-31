//! Bedrock block-state id → `(identifier, states)`.
//!
//! With `block-network-ids-are-hashes=false` the runtime ids inside chunks are
//! indices into the version's block-state palette (0..≈24k). This table is that
//! palette, generated from the version's data; the id is the index.

use std::path::Path;

use crate::error::Result;
use crate::nbt::Tag;

/// The block-state palette of the Bedrock version this crate targets.
const BUNDLED: &[u8] = include_bytes!("../data/blockstates-1.26.60.txt.gz");

/// The version's block-state palette, indexed by runtime id.
pub struct BlockStates {
    entries: Vec<(String, serde_json::Value)>,
}

impl BlockStates {
    /// Parse the one-line-per-id `name<TAB>statesJSON` layout.
    pub fn parse(text: &str) -> Self {
        let mut entries = Vec::new();
        for line in text.lines() {
            let Some((name, json)) = line.split_once('\t') else {
                continue;
            };
            let states = serde_json::from_str(json).unwrap_or(serde_json::Value::Null);
            entries.push((name.to_string(), states));
        }
        Self { entries }
    }

    /// Load a gzip-compressed table.
    pub fn load_gz(path: &Path) -> Result<Self> {
        Self::parse_gz(&std::fs::read(path)?)
    }

    /// Parse a gzip-compressed table from memory.
    pub fn parse_gz(compressed: &[u8]) -> Result<Self> {
        use std::io::Read;
        let mut data = Vec::new();
        flate2::read::GzDecoder::new(compressed)
            .read_to_end(&mut data)
            .map_err(|e| crate::Error::other(format!("blockstates gzip: {e}")))?;
        Ok(Self::parse(&String::from_utf8_lossy(&data)))
    }

    /// The palette for the pinned Bedrock version, compiled into the binary so
    /// the mod does not have to find it on disk.
    pub fn bundled() -> Self {
        Self::parse_gz(BUNDLED).expect("bundled block-state palette")
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The Bedrock identifier (with its `minecraft:` prefix) and states.
    pub fn get(&self, runtime_id: i32) -> Option<(String, &serde_json::Value)> {
        let index = usize::try_from(runtime_id).ok()?;
        let (name, states) = self.entries.get(index)?;
        Some((qualify(name), states))
    }
}

/// Bedrock identifiers in the table omit the namespace.
fn qualify(name: &str) -> String {
    if name.contains(':') {
        name.to_string()
    } else {
        format!("minecraft:{name}")
    }
}

/// Convert a JSON state object into the NBT shape the mapping table compares
/// against (`blocks.nbt` stores Bedrock properties as NBT).
pub fn states_to_tag(value: &serde_json::Value) -> Option<Tag> {
    use serde_json::Value as J;
    Some(match value {
        J::Null => return None,
        J::Bool(b) => Tag::Byte(*b as i8),
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                Tag::Int(i as i32)
            } else {
                Tag::Float(n.as_f64()? as f32)
            }
        }
        J::String(s) => Tag::String(s.clone()),
        J::Array(items) => Tag::List(items.iter().filter_map(states_to_tag).collect()),
        J::Object(map) => {
            // A block without properties has no state entry in the table; an
            // empty object must compare as "absent", not as an empty compound.
            if map.is_empty() {
                return None;
            }
            Tag::Compound(
                map.iter()
                    .map(|(k, v)| Some((k.clone(), states_to_tag(v)?)))
                    .collect::<Option<Vec<_>>>()?,
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lines_and_qualifies_names() {
        let table = BlockStates::parse("air\t{}\nstone\t{}\noak_log\t{\"pillar_axis\":\"y\"}");
        assert_eq!(table.len(), 3);
        assert_eq!(table.get(0).unwrap().0, "minecraft:air");
        assert_eq!(table.get(2).unwrap().0, "minecraft:oak_log");
        assert!(table.get(3).is_none());
        assert_eq!(table.get(-1).map(|_| ()), None);
    }

    #[test]
    fn converts_json_states_to_nbt() {
        let v = serde_json::json!({"pillar_axis": "y", "level": 3});
        match states_to_tag(&v).unwrap() {
            Tag::Compound(fields) => {
                assert!(fields
                    .iter()
                    .any(|(k, t)| k == "pillar_axis" && *t == Tag::String("y".into())));
                assert!(fields
                    .iter()
                    .any(|(k, t)| k == "level" && *t == Tag::Int(3)));
            }
            other => panic!("expected compound, got {other:?}"),
        }
    }
}
