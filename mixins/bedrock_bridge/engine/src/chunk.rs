//! Decode a Bedrock `LevelChunk` into per-section Java block-state ids.
//!
//! The chunk payload is a sequence of sub-chunks, each prefixed with its
//! section index (0 = the bottom of the world, y = -64). Every block is a
//! runtime palette id that [`StartGame::block`] resolves to a Bedrock
//! identifier + state, and [`BedrockBlockMappings`] then maps to the Java
//! block-state id Horizon expects.

use crate::error::Result;
use crate::mapping::BedrockBlockMappings;
use crate::protocol::packets::{LevelChunk, StartGame};
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

/// Resolve a Bedrock runtime id to a Java block-state id through the world's
/// palette and the Bedrock→Java table.
pub fn java_block_state(game: &StartGame, mappings: &BedrockBlockMappings, runtime_id: i32) -> u32 {
    game.block(runtime_id)
        .and_then(|(name, states)| mappings.by_bedrock_state(name, Some(states)))
        .map(|id| id as u32)
        .unwrap_or(0)
}

/// Decode every sub-chunk of `chunk` into Java block states.
pub fn decode_sections(
    game: &StartGame,
    chunk: &LevelChunk,
    mappings: &BedrockBlockMappings,
) -> Result<Vec<ChunkSection>> {
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
                    blocks[java_index(x, y, z)] = java_block_state(game, mappings, runtime_id);
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
    fn java_index_matches_minecraft_ordering() {
        assert_eq!(java_index(0, 0, 0), 0);
        assert_eq!(java_index(1, 0, 0), 1);
        assert_eq!(java_index(0, 0, 1), 16);
        assert_eq!(java_index(0, 1, 0), 256);
        assert_eq!(java_index(15, 15, 15), 4095);
    }
}
