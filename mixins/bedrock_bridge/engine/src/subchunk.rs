//! Bedrock's paletted sub-chunk (16×16×16 section) format, as sent in
//! `LevelChunk` and updated by `UpdateBlock`/`SubChunk` packets.
//!
//! A sub-chunk holds up to eight "storages", layered (water over stone, and so
//! on). Each storage is a [`BitStorage`] of runtime-id or NBT-palette indices
//! plus the palette itself. Blocks are addressed with Bedrock's `x,z,y` packing:
//! `index = (x << 8) | (z << 4) | y`.

use crate::bitarray::BitStorage;
use crate::error::{Error, Result};
use crate::nbt::{self, Tag};
use crate::stream::{Reader, Writer};

/// One palette entry.
#[derive(Clone, Debug, PartialEq)]
pub enum Palette {
    /// Runtime ids, indices into the server's block palette.
    Runtime(Vec<i32>),
    /// Persistent (NBT) block descriptors: identifier + states.
    Persistent(Vec<Tag>),
}

/// One layered storage inside a sub-chunk.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockStorage {
    pub storage: BitStorage,
    pub palette: Palette,
    /// The layer (0 usually stone, 1 water/foliage).
    pub layer: u8,
}

impl BlockStorage {
    /// The raw palette index at `(x, y, z)` within the section.
    fn index(&self, x: usize, y: usize, z: usize) -> u32 {
        self.storage.get(Self::pack(x, y, z))
    }

    /// Bedrock's in-section ordering: `(x << 8) | (z << 4) | y`.
    pub fn pack(x: usize, y: usize, z: usize) -> usize {
        (x << 8) | (z << 4) | y
    }

    /// The runtime id, when this is a runtime storage.
    pub fn runtime_id_at(&self, x: usize, y: usize, z: usize) -> Option<i32> {
        match &self.palette {
            Palette::Runtime(ids) => {
                let idx = self.index(x, y, z) as usize;
                ids.get(idx).copied()
            }
            Palette::Persistent(_) => None,
        }
    }

    /// The persistent descriptor, when this is an NBT storage.
    pub fn descriptor_at(&self, x: usize, y: usize, z: usize) -> Option<&Tag> {
        match &self.palette {
            Palette::Persistent(entries) => entries.get(self.index(x, y, z) as usize),
            Palette::Runtime(_) => None,
        }
    }
}

/// A decoded 16×16×16 section.
#[derive(Clone, Debug, PartialEq)]
pub struct SubChunk {
    pub storages: Vec<BlockStorage>,
    /// Protocol ≥ 419's "all air, no storages".
    pub empty: bool,
}

impl SubChunk {
    /// An all-air section.
    pub fn air() -> Self {
        SubChunk {
            storages: Vec::new(),
            empty: true,
        }
    }

    /// Decode a serialized sub-chunk. `version_byte` is the first byte of the
    /// payload (the wire places it before the storages).
    pub fn read(r: &mut Reader, version_byte: u8) -> Result<Self> {
        let storage_count = match version_byte {
            8 => r.u8()? as usize,
            9 => {
                // 1-byte storage count, then the sub-chunk's Y index.
                let count = r.u8()? as usize;
                let _y = r.u8()?;
                count
            }
            n => n as usize,
        };
        // A sub-chunk has at most eight layers; anything else is a corrupt
        // packet rather than a bigger section.
        if storage_count > 8 {
            return Err(Error::decode(format!(
                "sub-chunk with {storage_count} storages"
            )));
        }
        let mut storages = Vec::with_capacity(storage_count);
        let mut layer = 0u8;
        for _ in 0..storage_count {
            let header = r.u8()?;
            let runtime = header & 1 == 1;
            let bits = header >> 1;
            // A zero-bit storage carries no packed words but still carries its
            // palette. Reading it as a lone id left the palette entry behind,
            // which drifted every following section of the chunk.
            let storage = if bits == 0 {
                BitStorage::new(1, 4096)?
            } else {
                BitStorage::read(r, bits, 4096)?
            };
            let palette_size = r.zigzag32()?;
            if palette_size < 0 {
                return Err(Error::decode("negative palette size"));
            }
            let palette = if runtime {
                let mut ids = Vec::with_capacity((palette_size as usize).min(4096));
                for _ in 0..palette_size {
                    ids.push(r.zigzag32()?);
                }
                Palette::Runtime(ids)
            } else {
                let mut entries = Vec::with_capacity((palette_size as usize).min(4096));
                for _ in 0..palette_size {
                    entries.push(nbt::read_named(r)?.1);
                }
                Palette::Persistent(entries)
            };
            storages.push(BlockStorage {
                storage,
                palette,
                layer,
            });
            layer = layer.saturating_add(1);
        }
        Ok(SubChunk {
            empty: storages.is_empty(),
            storages,
        })
    }

    /// Serialize this sub-chunk (the wire version byte must be written by the
    /// caller, since it also carries the layer count).
    pub fn write(&self, w: &mut Writer) {
        if self.empty {
            w.u8(9);
            return;
        }
        if self.storages.len() == 1 {
            w.u8(1);
        } else {
            w.u8(8).u8(self.storages.len() as u8);
        }
        for storage in &self.storages {
            match &storage.palette {
                Palette::Runtime(ids) => {
                    let header = (storage.storage.bits() << 1) | 1;
                    w.u8(header);
                    storage.storage.write(w);
                    w.zigzag32(ids.len() as i32);
                    for id in ids {
                        w.zigzag32(*id);
                    }
                }
                Palette::Persistent(entries) => {
                    let header = storage.storage.bits() << 1;
                    w.u8(header);
                    storage.storage.write(w);
                    w.zigzag32(entries.len() as i32);
                    for entry in entries {
                        nbt::write_named(w, "", entry);
                    }
                }
            }
        }
    }

    /// The runtime id of the top-most storage's block at `(x, y, z)`.
    pub fn top_runtime_id(&self, x: usize, y: usize, z: usize) -> Option<i32> {
        self.storages
            .iter()
            .rev()
            .find_map(|s| s.runtime_id_at(x, y, z))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_matches_bedrock_ordering() {
        assert_eq!(BlockStorage::pack(0, 0, 0), 0);
        assert_eq!(BlockStorage::pack(1, 0, 0), 256);
        assert_eq!(BlockStorage::pack(0, 1, 0), 1);
        assert_eq!(BlockStorage::pack(0, 0, 1), 16);
        assert_eq!(BlockStorage::pack(15, 15, 15), 4095);
    }

    #[test]
    fn single_layer_runtime_subchunk_round_trips() {
        let mut storage = BitStorage::new(4, 4096).unwrap();
        // three distinct blocks at known spots
        storage.set(BlockStorage::pack(1, 2, 3), 1);
        storage.set(BlockStorage::pack(5, 6, 7), 2);
        storage.set(BlockStorage::pack(10, 11, 12), 3);
        let section = SubChunk {
            empty: false,
            storages: vec![BlockStorage {
                storage,
                palette: Palette::Runtime(vec![0, 10, 20, 30]),
                layer: 0,
            }],
        };
        let mut w = Writer::new();
        section.write(&mut w);
        let bytes = w.into_inner();
        let mut r = Reader::new(&bytes);
        let version = r.u8().unwrap();
        let back = SubChunk::read(&mut r, version).unwrap();
        assert_eq!(back, section);
        assert_eq!(back.top_runtime_id(1, 2, 3), Some(10));
        assert_eq!(back.top_runtime_id(5, 6, 7), Some(20));
        assert_eq!(back.top_runtime_id(10, 11, 12), Some(30));
        assert_eq!(back.top_runtime_id(0, 0, 0), Some(0));
    }

    #[test]
    fn all_air_version_is_empty() {
        // Version 9: a storage count (0) then the sub-chunk Y index.
        let mut r = Reader::new(&[0u8, 0u8]);
        let section = SubChunk::read(&mut r, 9).unwrap();
        assert!(section.empty);
        assert_eq!(section.top_runtime_id(0, 0, 0), None);
    }

    #[test]
    fn persistent_nbt_palette_round_trips() {
        let storage = BitStorage::new(1, 4096).unwrap();
        let section = SubChunk {
            empty: false,
            storages: vec![BlockStorage {
                storage,
                palette: Palette::Persistent(vec![
                    Tag::compound([("name", Tag::String("minecraft:air".into()))]),
                    Tag::compound([("name", Tag::String("minecraft:stone".into()))]),
                ]),
                layer: 0,
            }],
        };
        let mut w = Writer::new();
        section.write(&mut w);
        let bytes = w.into_inner();
        let mut r = Reader::new(&bytes);
        let version = r.u8().unwrap();
        let back = SubChunk::read(&mut r, version).unwrap();
        assert_eq!(back, section);
    }
}
