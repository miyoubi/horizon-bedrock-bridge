//! Bedrock's base wire encoding: little-endian primitives, LEB128 varints and
//! zig-zag signed integers. Everything above this file reads and writes through
//! [`Reader`] and [`Writer`].
//!
//! Bedrock differs from Java in almost every primitive: integers are
//! little-endian, lengths are unsigned varints, and signed values that travel as
//! varints are zig-zag encoded (so small magnitudes stay short for either sign).

use crate::error::{Error, Result};

/// A cursor over a byte slice with Bedrock's read helpers.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }
    pub fn position(&self) -> usize {
        self.pos
    }
    /// Rewind or skip to an absolute position (used to re-read a field whose
    /// layout can differ between protocol revisions).
    pub fn seek(&mut self, pos: usize) {
        self.pos = pos.min(self.data.len());
    }
    /// The bytes not yet read.
    pub fn rest(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }
    /// The whole backing slice.
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    fn need(&self, n: usize) -> Result<()> {
        if self.remaining() < n {
            return Err(Error::decode(format!(
                "need {n} bytes, {} remain",
                self.remaining()
            )));
        }
        Ok(())
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.need(n)?;
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }
    pub fn i8(&mut self) -> Result<i8> {
        Ok(self.u8()? as i8)
    }
    pub fn bool(&mut self) -> Result<bool> {
        Ok(self.u8()? != 0)
    }
    pub fn u16(&mut self) -> Result<u16> {
        let b = self.bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    pub fn i16(&mut self) -> Result<i16> {
        Ok(self.u16()? as i16)
    }
    pub fn u24(&mut self) -> Result<u32> {
        let b = self.bytes(3)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], 0]))
    }
    pub fn i32(&mut self) -> Result<i32> {
        let b = self.bytes(4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn i64(&mut self) -> Result<i64> {
        let b = self.bytes(8)?;
        Ok(i64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(self.i64()? as u64)
    }
    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.i32()? as u32))
    }
    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.i64()? as u64))
    }

    /// An unsigned LEB128 varint (32-bit, Bedrock's `VarInt`).
    pub fn varint(&mut self) -> Result<u32> {
        let mut result: u32 = 0;
        for shift in (0..35).step_by(7) {
            let byte = self.u8()?;
            result |= u32::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
        }
        Err(Error::decode("varint longer than 5 bytes"))
    }

    /// An unsigned LEB128 varint (64-bit, Bedrock's `VarLong`).
    pub fn varlong(&mut self) -> Result<u64> {
        let mut result: u64 = 0;
        for shift in (0..70).step_by(7) {
            let byte = self.u8()?;
            result |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
        }
        Err(Error::decode("varlong longer than 10 bytes"))
    }

    /// A ZigZag uint32 (Bedrock's signed `VarInt`, used by network NBT).
    pub fn zigzag32(&mut self) -> Result<i32> {
        let n = self.varint()?;
        Ok(((n >> 1) as i32) ^ -((n & 1) as i32))
    }

    /// A ZigZag uint64.
    pub fn zigzag64(&mut self) -> Result<i64> {
        let n = self.varlong()?;
        Ok(((n >> 1) as i64) ^ -((n & 1) as i64))
    }

    /// A varint-length-prefixed UTF-8 string.
    pub fn string(&mut self) -> Result<String> {
        let len = self.varint()? as usize;
        let bytes = self.bytes(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|e| Error::decode(format!("bad utf-8: {e}")))
    }

    /// A varint-length-prefixed byte blob (the common way Bedrock ships raw
    /// payloads such as NBT or login JWTs).
    pub fn byte_slice(&mut self) -> Result<&'a [u8]> {
        let len = self.varint()? as usize;
        self.bytes(len)
    }

    /// Read a `(x, z)` chunk coordinate pair as signed varints.
    pub fn chunk_pos(&mut self) -> Result<(i32, i32)> {
        Ok((self.zigzag32()?, self.zigzag32()?))
    }

    /// A network NBT compound, framed as a little-endian `u16` count of bytes
    /// (`0xffff` meaning "none") — the shape most Bedrock packets use.
    pub fn nbt_bytes(&mut self) -> Result<Option<&'a [u8]>> {
        let len = self.u16()?;
        if len == u16::MAX {
            // Some servers write a varint 0 after the sentinel.
            let _ = self.u8();
            return Ok(None);
        }
        Ok(Some(self.bytes(len as usize)?))
    }
}

/// Grows a byte buffer with Bedrock's write helpers.
#[derive(Clone, Debug, Default)]
pub struct Writer {
    pub data: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Writer { data: Vec::new() }
    }
    pub fn into_inner(self) -> Vec<u8> {
        self.data
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.data.push(v);
        self
    }
    pub fn i8(&mut self, v: i8) -> &mut Self {
        self.u8(v as u8)
    }
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(u8::from(v))
    }
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.data.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn i16(&mut self, v: i16) -> &mut Self {
        self.u16(v as u16)
    }
    pub fn u24(&mut self, v: u32) -> &mut Self {
        self.data.extend_from_slice(&v.to_le_bytes()[..3]);
        self
    }
    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.data.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.data.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.i64(v as i64)
    }
    pub fn f32(&mut self, v: f32) -> &mut Self {
        self.i32(v.to_bits() as i32)
    }
    pub fn f64(&mut self, v: f64) -> &mut Self {
        self.i64(v.to_bits() as i64)
    }

    pub fn varint(&mut self, mut v: u32) -> &mut Self {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.data.push(byte);
                return self;
            }
            self.data.push(byte | 0x80);
        }
    }
    pub fn varlong(&mut self, mut v: u64) -> &mut Self {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.data.push(byte);
                return self;
            }
            self.data.push(byte | 0x80);
        }
    }

    pub fn zigzag32(&mut self, v: i32) -> &mut Self {
        self.varint(((v << 1) ^ (v >> 31)) as u32)
    }
    pub fn zigzag64(&mut self, v: i64) -> &mut Self {
        self.varlong(((v << 1) ^ (v >> 63)) as u64)
    }

    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.data.extend_from_slice(b);
        self
    }
    pub fn string(&mut self, s: &str) -> &mut Self {
        self.varint(s.len() as u32).bytes(s.as_bytes())
    }
    /// A varint-length-prefixed byte blob.
    pub fn byte_slice(&mut self, b: &[u8]) -> &mut Self {
        self.varint(b.len() as u32).bytes(b)
    }
    pub fn chunk_pos(&mut self, x: i32, z: i32) -> &mut Self {
        self.zigzag32(x).zigzag32(z)
    }

    /// Writes a network NBT frame with a little-endian `u16` length prefix.
    pub fn nbt_bytes(&mut self, nbt: Option<&[u8]>) -> &mut Self {
        match nbt {
            None => self.u16(u16::MAX).u8(0), // sentinel + the trailing varint 0 servers expect
            Some(b) => self.u16(b.len() as u16).bytes(b),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn little_endian_round_trip() {
        let mut w = Writer::new();
        w.u8(0xab)
            .i8(-3)
            .bool(true)
            .u16(0x1234)
            .i16(-2)
            .i32(-123456)
            .i64(i64::MIN)
            .f32(1.5)
            .f64(-2.25);
        let bytes = w.into_inner();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.u8().unwrap(), 0xab);
        assert_eq!(r.i8().unwrap(), -3);
        assert!(r.bool().unwrap());
        assert_eq!(r.u16().unwrap(), 0x1234);
        assert_eq!(r.i16().unwrap(), -2);
        assert_eq!(r.i32().unwrap(), -123456);
        assert_eq!(r.i64().unwrap(), i64::MIN);
        assert_eq!(r.f32().unwrap(), 1.5);
        assert_eq!(r.f64().unwrap(), -2.25);
        assert!(r.is_empty());
    }

    #[test]
    fn varint_matches_the_wire_examples() {
        // 0, 1, 127, 128, 300, u32::MAX from the Bedrock/Java varint spec.
        let cases: &[(u32, &[u8])] = &[
            (0, &[0x00]),
            (1, &[0x01]),
            (127, &[0x7f]),
            (128, &[0x80, 0x01]),
            (300, &[0xac, 0x02]),
            (u32::MAX, &[0xff, 0xff, 0xff, 0xff, 0x0f]),
        ];
        for (value, wire) in cases {
            let mut w = Writer::new();
            w.varint(*value);
            assert_eq!(w.data, *wire, "encode {value}");
            let mut r = Reader::new(wire);
            assert_eq!(r.varint().unwrap(), *value, "decode {value}");
        }
    }

    #[test]
    fn zigzag_is_symmetric() {
        for v in [0i32, -1, 1, -64, 63, i32::MIN, i32::MAX] {
            let mut w = Writer::new();
            w.zigzag32(v);
            let mut r = Reader::new(&w.data);
            assert_eq!(r.zigzag32().unwrap(), v);
        }
        for v in [0i64, -1, 1, i64::MIN, i64::MAX] {
            let mut w = Writer::new();
            w.zigzag64(v);
            let mut r = Reader::new(&w.data);
            assert_eq!(r.zigzag64().unwrap(), v);
        }
    }

    #[test]
    fn truncated_input_errors_instead_of_panicking() {
        let mut r = Reader::new(&[0xff, 0xff]);
        // Four more continuation bytes are missing.
        assert!(r.varlong().is_err());
        let mut r = Reader::new(&[]);
        assert!(r.i32().is_err());
        let mut r = Reader::new(&[5]);
        assert!(r.string().is_err());
    }

    #[test]
    fn strings_and_blobs_round_trip() {
        let mut w = Writer::new();
        w.string("hello").byte_slice(&[1, 2, 3]);
        let mut r = Reader::new(&w.data);
        assert_eq!(r.string().unwrap(), "hello");
        assert_eq!(r.byte_slice().unwrap(), &[1, 2, 3]);
    }
}
