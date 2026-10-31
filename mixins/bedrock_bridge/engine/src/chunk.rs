//! Decode a Bedrock `LevelChunk` into per-section Java block-state ids.
//!
//! The chunk payload is a sequence of sub-chunks, each prefixed with its
//! section index (0 = the bottom of the world, y = -64). Every block is a
//! runtime palette id that [`StartGame::block`] resolves to a Bedrock
//! identifier + state, and [`BedrockBlockMappings`] then maps to the Java
//! block-state id Horizon expects.

use crate::error::Result;
use crate::protocol::packets::LevelChunk;
use crate::stream::Reader;
use crate::subchunk::SubChunk;

/// One 16³ section, in Java's block-state-id space.
pub struct ChunkSection {
    /// Section Y in Java's coordinates (Bedrock index − 4).
    pub y: i8,
    /// 4096 Java block-state ids.
    pub blocks: Vec<u32>,
}

/// A block's index within a section: `(y * 16 + z) * 16 + x` (Java order).
pub fn java_index(x: usize, y: usize, z: usize) -> usize {
    (y * 16 + z) * 16 + x
}

/// A runtime id's Bedrock identity. Turning this into the game's block-state id
/// is the mod's job: the Java name (and property spelling) comes from Horizon's
/// block registry, which the engine cannot see.
pub fn bedrock_block(
    table: &crate::blockstates::BlockStates,
    runtime_id: i32,
) -> Option<(String, crate::nbt::Tag)> {
    let (name, states) = table.get(runtime_id)?;
    let tag = crate::blockstates::states_to_tag(states);
    Some((name, tag.unwrap_or(crate::nbt::Tag::End)))
}

/// Decode every sub-chunk of `chunk` into Java block states.
pub fn decode_sections(chunk: &LevelChunk) -> Result<Vec<ChunkSection>> {
    // The serialized chunk data starts with its own sub-chunk count; the
    // packet's `sub_chunk_count` field reads 0 from a modern server.
    let (count, offset) = if chunk.sub_chunk_count == 0 && !chunk.data.is_empty() {
        (chunk.data[0] as u32, 1)
    } else {
        (chunk.sub_chunk_count, 0)
    };
    let mut r = Reader::new(&chunk.data[offset..]);
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let index = r.u8()?;
        let version = r.u8()?;
        let sub = SubChunk::read(&mut r, version)?;
        let mut blocks = vec![0u32; 4096];
        for y in 0..16 {
            for z in 0..16 {
                for x in 0..16 {
                    let runtime_id = sub.top_runtime_id(x, y, z).unwrap_or(0);
                    // Runtime ids are the Bedrock block identity; the mod maps
                    // them to the game's block-state ids.
                    blocks[java_index(x, y, z)] = u32::try_from(runtime_id).unwrap_or(0);
                }
            }
        }
        out.push(ChunkSection {
            y: index as i8 - 4,
            blocks,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_known_palette_ids() {
        let table = crate::blockstates::BlockStates::load_gz(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../data/blockstates-1.26.60.txt.gz"),
        )
        .unwrap();
        // Palette ids captured from a real section.
        assert_eq!(bedrock_block(&table, 18894).unwrap().0, "minecraft:air");
        assert_eq!(bedrock_block(&table, 1899).unwrap().0, "minecraft:oak_log");
        assert_eq!(
            bedrock_block(&table, 3596).unwrap().0,
            "minecraft:oak_leaves"
        );
        assert_eq!(
            bedrock_block(&table, 9659).unwrap().0,
            "minecraft:birch_leaves"
        );
        assert!(bedrock_block(&table, -1).is_none());
    }

    #[test]
    fn java_index_matches_minecraft_ordering() {
        assert_eq!(java_index(0, 0, 0), 0);
        assert_eq!(java_index(1, 0, 0), 1);
        assert_eq!(java_index(0, 0, 1), 16);
        assert_eq!(java_index(0, 1, 0), 256);
        assert_eq!(java_index(15, 15, 15), 4095);
    }
}
