//! Bedrock ↔ Java mapping tables and the addon (custom) entity registry.
//!
//! The tables are the GeyserMC mappings (`references/mappings-master`), generated
//! for Java 26.3 ↔ Bedrock 1.26.60, and the entity identifiers come from
//! `AvailableEntityIdentifiers` (the same NBT a behaviour pack extends when it
//! defines a custom entity). The loader is optional: with no data directory the
//! bridge still runs and falls back to a neutral block/biome.

use std::collections::HashMap;
use std::path::Path;

use crate::crypto::B64;
use base64::Engine;

/// A Bedrock runtime block-id range that belongs to one Java block
/// (`items.json`'s `firstBlockRuntimeId`/`lastBlockRuntimeId`).
#[derive(Clone, Debug)]
pub struct BlockRange {
    pub first: i32,
    pub last: i32,
    pub java_id: String,
    pub bedrock_identifier: String,
}

/// The loaded mapping tables.
#[derive(Default)]
pub struct Mappings {
    blocks: Vec<BlockRange>,
    biomes_bedrock_to_java: HashMap<i32, String>,
    biomes_java_to_bedrock: HashMap<String, i32>,
    /// Bedrock entity identifier → preferred Java entity identifier.
    entities: HashMap<String, String>,
    /// Bedrock item identifier → Java item identifier.
    items: HashMap<String, String>,
}

/// The Bedrock block used when nothing maps (matches TunnelMC's fallback).
pub const FALLBACK_BLOCK: &str = "minecraft:stone";
/// The Bedrock biome used when nothing maps.
pub const FALLBACK_BIOME: i32 = 1;

impl Mappings {
    /// A mapping with only the built-in defaults (no files).
    pub fn defaults() -> Self {
        Mappings::default()
    }

    /// Load `biomes.json` and `items.json` from a directory of Geyser mappings,
    /// ignoring whichever files are absent.
    pub fn load(dir: &Path) -> Self {
        let mut m = Mappings::default();
        if let Ok(text) = std::fs::read_to_string(dir.join("biomes.json")) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                m.load_biomes(&json);
            }
        }
        if let Ok(text) = std::fs::read_to_string(dir.join("items.json")) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                m.load_items(&json);
            }
        }
        m.blocks.sort_by_key(|b| b.first);
        m
    }

    fn load_biomes(&mut self, json: &serde_json::Value) {
        if let Some(obj) = json.as_object() {
            for (java_id, entry) in obj {
                if let Some(id) = entry.get("bedrock_id").and_then(|v| v.as_i64()) {
                    self.biomes_bedrock_to_java
                        .insert(id as i32, java_id.clone());
                    self.biomes_java_to_bedrock
                        .insert(java_id.clone(), id as i32);
                }
            }
        }
    }

    fn load_items(&mut self, json: &serde_json::Value) {
        if let Some(obj) = json.as_object() {
            for (java_id, entry) in obj {
                let first = entry.get("firstBlockRuntimeId").and_then(|v| v.as_i64());
                let last = entry.get("lastBlockRuntimeId").and_then(|v| v.as_i64());
                let bedrock = entry
                    .get("bedrock_identifier")
                    .and_then(|v| v.as_str())
                    .unwrap_or(java_id)
                    .to_owned();
                self.items.insert(bedrock.clone(), java_id.clone());
                if let (Some(first), Some(last)) = (first, last) {
                    self.blocks.push(BlockRange {
                        first: first as i32,
                        last: last as i32,
                        java_id: java_id.clone(),
                        bedrock_identifier: bedrock,
                    });
                }
            }
        }
    }

    /// The Java block id for a Bedrock runtime block id, or the fallback.
    pub fn java_block(&self, runtime_id: i32) -> &str {
        // Ranges are sorted by `first`; binary search for the last range that
        // starts at or before the id, then range-check.
        let idx = self.blocks.partition_point(|b| b.first <= runtime_id);
        if idx > 0 {
            let r = &self.blocks[idx - 1];
            if runtime_id <= r.last {
                return &r.java_id;
            }
        }
        FALLBACK_BLOCK
    }

    /// The Bedrock identifier for a Bedrock runtime block id.
    pub fn bedrock_block_identifier(&self, runtime_id: i32) -> Option<&str> {
        let idx = self.blocks.partition_point(|b| b.first <= runtime_id);
        if idx > 0 {
            let r = &self.blocks[idx - 1];
            if runtime_id <= r.last {
                return Some(&r.bedrock_identifier);
            }
        }
        None
    }

    pub fn java_biome(&self, bedrock_id: i32) -> &str {
        self.biomes_bedrock_to_java
            .get(&bedrock_id)
            .map(String::as_str)
            .unwrap_or("minecraft:plains")
    }

    pub fn bedrock_biome(&self, java_id: &str) -> i32 {
        self.biomes_java_to_bedrock
            .get(java_id)
            .copied()
            .unwrap_or(FALLBACK_BIOME)
    }

    /// The Java item identifier for a Bedrock item identifier, or the input
    /// unchanged when nothing maps (the names agree for most items).
    pub fn java_item(&self, bedrock_id: &str) -> String {
        self.items
            .get(bedrock_id)
            .cloned()
            .unwrap_or_else(|| bedrock_id.to_owned())
    }

    /// Register (or override) a Bedrock entity identifier's Java counterpart.
    pub fn insert_entity(&mut self, bedrock: impl Into<String>, java: impl Into<String>) {
        self.entities.insert(bedrock.into(), java.into());
    }

    /// Register (or override) a Bedrock item identifier's Java counterpart.
    pub fn insert_item(&mut self, bedrock: impl Into<String>, java: impl Into<String>) {
        self.items.insert(bedrock.into(), java.into());
    }

    /// The Java item identifier for a Bedrock network id from the server's item
    /// palette, or `None` when the palette never named it.
    pub fn java_item_by_network_id(
        &self,
        palette: &[(String, i16)],
        network_id: i16,
    ) -> Option<String> {
        palette
            .iter()
            .find(|(_, id)| *id == network_id)
            .map(|(name, _)| self.java_item(name))
    }

    /// The Java entity identifier for a Bedrock one.
    ///
    /// A custom (addon) entity — anything outside `minecraft:` or a vanilla
    /// name — is passed through with its own namespace, so the bridge can still
    /// show it and a content mapping can give it a model.
    pub fn java_entity(&self, bedrock_id: &str) -> String {
        if let Some(java) = self.entities.get(bedrock_id) {
            return java.clone();
        }
        // A handful of Bedrock names differ from Java's.
        match bedrock_id {
            "minecraft:xp_orb" => "minecraft:experience_orb".into(),
            "minecraft:xp_bottle" => "minecraft:experience_bottle".into(),
            "minecraft:ender_crystal" => "minecraft:end_crystal".into(),
            "minecraft:zombie_pigman" => "minecraft:zombified_piglin".into(),
            // Java's thrown potions are entities of their own
            // (`minecraft:splash_potion`, id 108). Mapping them to the
            // `minecraft:potion` *item* made the id lookup fail and every witch
            // potion spawned as acacia boat (Java entity id 0).
            "minecraft:villager_v2" => "minecraft:villager".into(),
            "minecraft:tropicalfish" => "minecraft:tropical_fish".into(),
            other => other.into(),
        }
    }

    /// Parse the `AvailableEntityIdentifiers` compound, whose `idlist` holds one
    /// entry per identifier (vanilla and addon), and return each identifier.
    pub fn parse_entity_identifiers(nbt: &crate::nbt::Tag) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(list) = nbt.get("idlist").and_then(|t| t.as_list()) {
            for entry in list {
                if let Some(id) = entry.get_str("id") {
                    out.push(id.to_owned());
                }
            }
        }
        out
    }
}

/// One entry of the GeyserMC `blocks.nbt` table (`bedrock_mappings`), indexed by
/// the **Java block state id**.
#[derive(Clone, Debug, PartialEq)]
pub struct BedrockBlockEntry {
    /// The Bedrock identifier, when it differs from the Java one. Absent means
    /// the Bedrock name is the Java block's identifier (the table's default).
    pub bedrock_identifier: Option<String>,
    /// The Bedrock block-state properties (`state`).
    pub states: Option<crate::nbt::Tag>,
}

/// The GeyserMC `blocks.nbt` table: `java_state_id → Bedrock state`.
///
/// This is the mapping's native direction (Java → Bedrock), which is what the
/// table stores. Chunk translation needs the reverse for each block in the
/// server's palette; the glue resolves the Java *name* to a state id with
/// Horizon's block registry, so only the entries whose Bedrock name differs
/// need the explicit identifier.
pub struct BedrockBlockMappings {
    pub entries: Vec<BedrockBlockEntry>,
}

impl BedrockBlockMappings {
    /// Load a gzip-compressed big-endian `blocks.nbt`.
    pub fn load(path: &Path) -> crate::Result<Self> {
        use std::io::Read;
        let compressed = std::fs::read(path)?;
        let mut data = Vec::new();
        flate2::read::GzDecoder::new(compressed.as_slice())
            .read_to_end(&mut data)
            .map_err(|e| crate::Error::other(format!("blocks.nbt gzip: {e}")))?;
        Self::parse(&data)
    }

    /// Parse uncompressed big-endian NBT.
    pub fn parse(data: &[u8]) -> crate::Result<Self> {
        let root = crate::nbt::from_bytes_be(data)?;
        let list = root
            .get("bedrock_mappings")
            .and_then(|t| t.as_list())
            .ok_or_else(|| crate::Error::decode("blocks.nbt has no bedrock_mappings list"))?;
        let entries = list
            .iter()
            .map(|e| BedrockBlockEntry {
                bedrock_identifier: e.get_str("bedrock_identifier").map(str::to_owned),
                states: e.get("state").cloned(),
            })
            .collect();
        Ok(Self { entries })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn entry(&self, java_state_id: usize) -> Option<&BedrockBlockEntry> {
        self.entries.get(java_state_id)
    }

    /// The first Java state id whose Bedrock identifier and state match.
    ///
    /// `states` is compared structurally; an entry with no `state` matches only
    /// an empty/absent query, and an entry with no `bedrock_identifier` is
    /// skipped (its Bedrock name is the Java name, unknown to this table alone).
    pub fn by_bedrock_state(&self, name: &str, states: Option<&crate::nbt::Tag>) -> Option<usize> {
        // NBT compound field order is not significant, so compare canonical forms.
        let want = states.map(canonical_states);
        self.entries.iter().position(|e| {
            e.bedrock_identifier.as_deref() == Some(name)
                && e.states.as_ref().map(canonical_states) == want
        })
    }
}

/// Sort every compound's fields so equivalent state maps compare equal.
fn canonical_states(tag: &crate::nbt::Tag) -> crate::nbt::Tag {
    use crate::nbt::Tag as B;
    match tag {
        B::Compound(fields) => {
            let mut sorted: Vec<(String, B)> = fields
                .iter()
                .map(|(k, v)| (k.clone(), canonical_states(v)))
                .collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            B::Compound(sorted)
        }
        B::List(items) => B::List(items.iter().map(canonical_states).collect()),
        other => other.clone(),
    }
}

/// A minimal, valid humanoid geometry so the client always renders.
pub const HUMANOID_GEOMETRY: &str = "{\"format_version\":\"1.12.0\",\"minecraft:geometry\":[{\"description\":{\"identifier\":\"geometry.humanoid.custom\",\"texture_width\":64,\"texture_height\":64},\"bones\":[]}]}";

/// A valid 64×64 RGBA skin, built once and base64 encoded.
///
/// The real client ships a Steve texture; the server only needs a well-formed
/// image, so a solid colour is generated instead of embedding a 44 KB literal.
pub fn steve_skin_b64() -> String {
    let mut pixels = vec![0u8; 64 * 64 * 4];
    for px in pixels.chunks_exact_mut(4) {
        px[0] = 0x3c;
        px[1] = 0x28;
        px[2] = 0x20;
        px[3] = 0xff;
    }
    B64.encode(pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_ranges_resolve_and_fall_back() {
        let mut m = Mappings::defaults();
        m.blocks = vec![
            BlockRange {
                first: 100,
                last: 110,
                java_id: "minecraft:stone".into(),
                bedrock_identifier: "minecraft:stone".into(),
            },
            BlockRange {
                first: 200,
                last: 205,
                java_id: "minecraft:dirt".into(),
                bedrock_identifier: "minecraft:dirt".into(),
            },
        ];
        m.blocks.sort_by_key(|b| b.first);
        assert_eq!(m.java_block(100), "minecraft:stone");
        assert_eq!(m.java_block(110), "minecraft:stone");
        assert_eq!(m.java_block(202), "minecraft:dirt");
        // A gap falls back.
        assert_eq!(m.java_block(150), FALLBACK_BLOCK);
        assert_eq!(m.java_block(5), FALLBACK_BLOCK);
        assert_eq!(m.bedrock_block_identifier(202), Some("minecraft:dirt"));
    }

    #[test]
    fn biomes_round_trip() {
        let json = serde_json::json!({
            "minecraft:ocean": { "bedrock_id": 0 },
            "minecraft:plains": { "bedrock_id": 1 },
        });
        let mut m = Mappings::defaults();
        m.load_biomes(&json);
        assert_eq!(m.java_biome(1), "minecraft:plains");
        assert_eq!(m.bedrock_biome("minecraft:ocean"), 0);
        assert_eq!(m.bedrock_biome("nope"), FALLBACK_BIOME);
    }

    #[test]
    fn custom_entities_are_passed_through() {
        let m = Mappings::defaults();
        assert_eq!(m.java_item("minecraft:apple"), "minecraft:apple");
        let mut named = Mappings::default();
        named.insert_item("minecraft:wooden_sword", "minecraft:wooden_sword");
        assert_eq!(
            named.java_item("minecraft:wooden_sword"),
            "minecraft:wooden_sword"
        );
        let palette = vec![("minecraft:apple".to_owned(), 257i16)];
        assert_eq!(
            m.java_item_by_network_id(&palette, 257).as_deref(),
            Some("minecraft:apple")
        );
        assert_eq!(m.java_item_by_network_id(&palette, 999), None);
        assert_eq!(m.java_entity("minecraft:cow"), "minecraft:cow");
        assert_eq!(
            m.java_entity("minecraft:xp_orb"),
            "minecraft:experience_orb"
        );
        // An addon entity keeps its namespace so it can be modelled on demand.
        assert_eq!(m.java_entity("mymod:robot"), "mymod:robot");
    }

    #[test]
    fn entity_identifier_list_is_read() {
        let nbt = crate::nbt::Tag::compound([(
            "idlist",
            crate::nbt::Tag::List(vec![
                crate::nbt::Tag::compound([(
                    "id",
                    crate::nbt::Tag::String("minecraft:cow".into()),
                )]),
                crate::nbt::Tag::compound([("id", crate::nbt::Tag::String("mymod:robot".into()))]),
            ]),
        )]);
        let ids = Mappings::parse_entity_identifiers(&nbt);
        assert_eq!(ids, vec!["minecraft:cow", "mymod:robot"]);
    }

    #[test]
    fn loads_geyser_blocks_nbt_when_present() {
        // Skips when the reference checkout is not beside this crate (e.g. in a
        // staged native build).
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../../references/mappings-master/blocks.nbt");
        if !path.is_file() {
            return;
        }
        let m = BedrockBlockMappings::load(&path).unwrap();
        assert_eq!(m.len(), 35723);
        // Every entry is a compound; at least one carries an explicit Bedrock
        // identifier (the rest default to the Java name).
        assert!(m.entries.iter().any(|e| e.bedrock_identifier.is_some()));
    }

    #[test]
    fn reverse_lookup_matches_identifier_and_state() {
        let m = BedrockBlockMappings {
            entries: vec![
                BedrockBlockEntry {
                    bedrock_identifier: None,
                    states: None,
                },
                BedrockBlockEntry {
                    bedrock_identifier: Some("minecraft:grass".into()),
                    states: Some(crate::nbt::Tag::compound([("x", crate::nbt::Tag::Int(1))])),
                },
            ],
        };
        let states = crate::nbt::Tag::compound([("x", crate::nbt::Tag::Int(1))]);
        assert_eq!(
            m.by_bedrock_state("minecraft:grass", Some(&states)),
            Some(1)
        );
        assert_eq!(m.by_bedrock_state("minecraft:stone", None), None);
    }

    #[test]
    fn skin_payload_is_a_64x64_rgba_image() {
        let bytes = B64.decode(steve_skin_b64()).unwrap();
        assert_eq!(bytes.len(), 64 * 64 * 4);
    }
}
