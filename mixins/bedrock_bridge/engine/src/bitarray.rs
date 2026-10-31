//! Bedrock's packed bit storage, used inside paletted sub-chunks and biome
//! sections. The wire format is a run of little-endian `u32` words holding
//! fixed-width entries packed back to back, so an entry can straddle two words.

use crate::error::{Error, Result};
use crate::stream::{Reader, Writer};

/// The bit widths Bedrock uses: 1, 2, 3, 4, 5, 6, 8 or 16 bits per entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitArrayVersion {
    pub bits: u8,
}

impl BitArrayVersion {
    pub fn from_bits(bits: u8) -> Result<Self> {
        match bits {
            1..=6 | 8 | 16 => Ok(BitArrayVersion { bits }),
            other => Err(Error::decode(format!("invalid bit-array width {other}"))),
        }
    }

    /// The number of `u32` words a storage of `size` entries occupies. For the
    /// sub-chunk size (4096) every supported width divides evenly.
    pub fn words_for_size(&self, size: usize) -> usize {
        // Bedrock packs each word with `32 / bits` entries (padded), not a
        // continuous bitstream, so the count rounds by that block size.
        let per_word = 32 / self.bits as usize;
        size.div_ceil(per_word)
    }

    pub fn max_entry(&self) -> u32 {
        (1u32 << self.bits) - 1
    }
}

/// A packed little-endian bit storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitStorage {
    version: BitArrayVersion,
    words: Vec<u32>,
    size: usize,
}

impl BitStorage {
    /// A zeroed storage of `size` entries.
    pub fn new(bits: u8, size: usize) -> Result<Self> {
        let version = BitArrayVersion::from_bits(bits)?;
        Ok(BitStorage {
            version,
            words: vec![0; version.words_for_size(size)],
            size,
        })
    }

    pub fn bits(&self) -> u8 {
        self.version.bits
    }
    pub fn size(&self) -> usize {
        self.size
    }
    pub fn words(&self) -> &[u32] {
        &self.words
    }

    /// Read `words_for_size(size)` little-endian words.
    pub fn read(r: &mut Reader, bits: u8, size: usize) -> Result<Self> {
        let version = BitArrayVersion::from_bits(bits)?;
        let count = version.words_for_size(size);
        let mut words = Vec::with_capacity(count);
        for _ in 0..count {
            words.push(r.i32()? as u32);
        }
        Ok(BitStorage {
            version,
            words,
            size,
        })
    }

    pub fn write(&self, w: &mut Writer) {
        for word in &self.words {
            w.i32(*word as i32);
        }
    }

    /// The entry at `index`, zero when out of range.
    pub fn get(&self, index: usize) -> u32 {
        if index >= self.size {
            return 0;
        }
        let bits = self.version.bits as usize;
        let per_word = 32 / bits;
        let word = index / per_word;
        let offset = (index % per_word) * bits;
        (self.words[word] >> offset) & self.version.max_entry()
    }

    /// Set the entry at `index`, ignoring out-of-range writes.
    pub fn set(&mut self, index: usize, value: u32) {
        if index >= self.size {
            return;
        }
        let bits = self.version.bits as usize;
        let mask = self.version.max_entry();
        let value = value & mask;
        let per_word = 32 / bits;
        let word = index / per_word;
        let offset = (index % per_word) * bits;
        self.words[word] = (self.words[word] & !(mask << offset)) | (value << offset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_counts_match_the_protocol() {
        for (bits, words) in [
            (1u8, 128),
            (2, 256),
            (3, 410),
            (4, 512),
            (5, 683),
            (6, 820),
            (8, 1024),
            (16, 2048),
        ] {
            let v = BitArrayVersion::from_bits(bits).unwrap();
            assert_eq!(v.words_for_size(4096), words, "{bits} bits");
        }
        assert!(BitArrayVersion::from_bits(7).is_err());
    }

    #[test]
    fn entries_round_trip_at_every_width() {
        for bits in [1u8, 2, 3, 4, 5, 6, 8, 16] {
            let mut s = BitStorage::new(bits, 4096).unwrap();
            let max = (1u32 << bits) - 1;
            let samples = [0, 1, max, max / 2, max];
            for (i, v) in samples.iter().enumerate() {
                s.set(i * 7, *v);
            }
            for (i, v) in samples.iter().enumerate() {
                assert_eq!(s.get(i * 7), *v, "{bits} bits at {}", i * 7);
            }
            // Unwritten entries stay zero.
            assert_eq!(s.get(3), 0);
        }
    }

    #[test]
    fn entries_straddling_a_word_boundary_survive() {
        // 5-bit entries: index 6 covers bits 30..35, spanning words 0 and 1.
        let mut s = BitStorage::new(5, 64).unwrap();
        s.set(6, 0b11111);
        assert_eq!(s.get(6), 0b11111);
        s.set(7, 0b10101);
        assert_eq!(s.get(6), 0b11111);
        assert_eq!(s.get(7), 0b10101);
    }

    #[test]
    fn read_write_is_byte_identical() {
        let mut s = BitStorage::new(4, 16).unwrap();
        s.set(1, 3);
        s.set(15, 12);
        let mut w = Writer::new();
        s.write(&mut w);
        let mut r = Reader::new(&w.data);
        let back = BitStorage::read(&mut r, 4, 16).unwrap();
        assert_eq!(back, s);
    }
}
