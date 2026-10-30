//! Bedrock's **network NBT**: the binary NBT dialect Bedrock puts inside packets
//! (item stacks, block-entity data, `AvailableEntityIdentifiers`, ...).
//!
//! It is not Java's NBT. In the network dialect everything is little-endian and
//! the integer types are varints: `Int` is a zig-zag `VarInt`, `Long` is a
//! zig-zag `VarLong`, array and list lengths are zig-zag ints, and strings are
//! unsigned-varint length prefixed. `Short`, `Float` and `Double` stay fixed
//! width and little-endian. That is the shape Cloudburst's `NetworkDataInput`
//! stream produces, which is what Bedrock servers emit.

use crate::error::{Error, Result};
use crate::stream::{Reader, Writer};

/// Tag ids, matching vanilla.
pub mod tag_id {
    pub const END: u8 = 0;
    pub const BYTE: u8 = 1;
    pub const SHORT: u8 = 2;
    pub const INT: u8 = 3;
    pub const LONG: u8 = 4;
    pub const FLOAT: u8 = 5;
    pub const DOUBLE: u8 = 6;
    pub const BYTE_ARRAY: u8 = 7;
    pub const STRING: u8 = 8;
    pub const LIST: u8 = 9;
    pub const COMPOUND: u8 = 10;
    pub const INT_ARRAY: u8 = 11;
    pub const LONG_ARRAY: u8 = 12;
}

/// An NBT value.
#[derive(Clone, Debug, PartialEq)]
pub enum Tag {
    End,
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    ByteArray(Vec<u8>),
    String(String),
    List(Vec<Tag>),
    /// Fields keep their order: Bedrock sometimes relies on it, and it makes
    /// round-trip tests deterministic.
    Compound(Vec<(String, Tag)>),
    IntArray(Vec<i32>),
    LongArray(Vec<i64>),
}

impl Tag {
    pub fn type_id(&self) -> u8 {
        match self {
            Tag::End => tag_id::END,
            Tag::Byte(_) => tag_id::BYTE,
            Tag::Short(_) => tag_id::SHORT,
            Tag::Int(_) => tag_id::INT,
            Tag::Long(_) => tag_id::LONG,
            Tag::Float(_) => tag_id::FLOAT,
            Tag::Double(_) => tag_id::DOUBLE,
            Tag::ByteArray(_) => tag_id::BYTE_ARRAY,
            Tag::String(_) => tag_id::STRING,
            Tag::List(_) => tag_id::LIST,
            Tag::Compound(_) => tag_id::COMPOUND,
            Tag::IntArray(_) => tag_id::INT_ARRAY,
            Tag::LongArray(_) => tag_id::LONG_ARRAY,
        }
    }

    pub fn compound(fields: impl IntoIterator<Item = (impl Into<String>, Tag)>) -> Tag {
        Tag::Compound(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    pub fn get(&self, key: &str) -> Option<&Tag> {
        match self {
            Tag::Compound(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn get_str(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Tag::String(s) => Some(s),
            _ => None,
        }
    }
    pub fn get_i32(&self, key: &str) -> Option<i32> {
        match self.get(key)? {
            Tag::Int(v) => Some(*v),
            Tag::Short(v) => Some(i32::from(*v)),
            Tag::Byte(v) => Some(i32::from(*v)),
            _ => None,
        }
    }
    pub fn get_i64(&self, key: &str) -> Option<i64> {
        match self.get(key)? {
            Tag::Long(v) => Some(*v),
            _ => None,
        }
    }
    pub fn get_f32(&self, key: &str) -> Option<f32> {
        match self.get(key)? {
            Tag::Float(v) => Some(*v),
            _ => None,
        }
    }
    pub fn as_list(&self) -> Option<&[Tag]> {
        match self {
            Tag::List(v) => Some(v),
            _ => None,
        }
    }
    pub fn as_compound(&self) -> Option<&[(String, Tag)]> {
        match self {
            Tag::Compound(v) => Some(v),
            _ => None,
        }
    }
}

/// A named tag, as it appears at the root or inside a compound.
pub type Named = (String, Tag);

/// Read a root/compound element: type byte, name, payload.
pub fn read_named(r: &mut Reader) -> Result<Named> {
    let id = r.u8()?;
    if id == tag_id::END {
        return Ok((String::new(), Tag::End));
    }
    let name = r.string()?;
    let value = read_payload(r, id)?;
    Ok((name, value))
}

/// Read a payload of a known type id (no leading type byte, no name).
pub fn read_payload(r: &mut Reader, id: u8) -> Result<Tag> {
    Ok(match id {
        tag_id::END => Tag::End,
        tag_id::BYTE => Tag::Byte(r.i8()?),
        tag_id::SHORT => Tag::Short(r.i16()?),
        tag_id::INT => Tag::Int(r.zigzag32()?),
        tag_id::LONG => Tag::Long(r.zigzag64()?),
        tag_id::FLOAT => Tag::Float(r.f32()?),
        tag_id::DOUBLE => Tag::Double(r.f64()?),
        tag_id::BYTE_ARRAY => {
            let len = r.zigzag32()?;
            if len < 0 {
                return Err(Error::decode("negative byte-array length"));
            }
            Tag::ByteArray(r.bytes(len as usize)?.to_vec())
        }
        tag_id::STRING => Tag::String(r.string()?),
        tag_id::LIST => {
            let elem = r.u8()?;
            let len = r.zigzag32()?;
            if len < 0 {
                return Err(Error::decode("negative list length"));
            }
            // Vanilla's guard against absurd allocations.
            if len as usize > r.remaining() + 16 {
                return Err(Error::decode("list longer than the packet"));
            }
            let mut items = Vec::with_capacity((len as usize).min(1024));
            for _ in 0..len {
                items.push(read_payload(r, elem)?);
            }
            Tag::List(items)
        }
        tag_id::COMPOUND => {
            let mut fields = Vec::new();
            loop {
                let id = r.u8()?;
                if id == tag_id::END {
                    break;
                }
                let name = r.string()?;
                let value = read_payload(r, id)?;
                fields.push((name, value));
            }
            Tag::Compound(fields)
        }
        tag_id::INT_ARRAY => {
            let len = r.zigzag32()?;
            if len < 0 {
                return Err(Error::decode("negative int-array length"));
            }
            let mut items = Vec::with_capacity((len as usize).min(4096));
            for _ in 0..len {
                items.push(r.zigzag32()?);
            }
            Tag::IntArray(items)
        }
        tag_id::LONG_ARRAY => {
            let len = r.zigzag32()?;
            if len < 0 {
                return Err(Error::decode("negative long-array length"));
            }
            let mut items = Vec::with_capacity((len as usize).min(4096));
            for _ in 0..len {
                items.push(r.zigzag64()?);
            }
            Tag::LongArray(items)
        }
        other => return Err(Error::decode(format!("unknown NBT tag id {other}"))),
    })
}

/// Write a root/compound element.
pub fn write_named(w: &mut Writer, name: &str, tag: &Tag) {
    w.u8(tag.type_id());
    if !matches!(tag, Tag::End) {
        w.string(name);
        write_payload(w, tag);
    }
}

/// Write a payload of the tag's own type.
pub fn write_payload(w: &mut Writer, tag: &Tag) {
    match tag {
        Tag::End => {}
        Tag::Byte(v) => {
            w.i8(*v);
        }
        Tag::Short(v) => {
            w.i16(*v);
        }
        Tag::Int(v) => {
            w.zigzag32(*v);
        }
        Tag::Long(v) => {
            w.zigzag64(*v);
        }
        Tag::Float(v) => {
            w.f32(*v);
        }
        Tag::Double(v) => {
            w.f64(*v);
        }
        Tag::ByteArray(v) => {
            w.zigzag32(v.len() as i32);
            w.bytes(v);
        }
        Tag::String(s) => {
            w.string(s);
        }
        Tag::List(items) => {
            // An empty list carries the vanilla END element type.
            let elem = items.first().map_or(tag_id::END, Tag::type_id);
            w.u8(elem);
            w.zigzag32(items.len() as i32);
            for item in items {
                write_payload(w, item);
            }
        }
        Tag::Compound(fields) => {
            for (name, value) in fields {
                if matches!(value, Tag::End) {
                    continue;
                }
                w.u8(value.type_id());
                w.string(name);
                write_payload(w, value);
            }
            w.u8(tag_id::END);
        }
        Tag::IntArray(items) => {
            w.zigzag32(items.len() as i32);
            for v in items {
                w.zigzag32(*v);
            }
        }
        Tag::LongArray(items) => {
            w.zigzag32(items.len() as i32);
            for v in items {
                w.zigzag64(*v);
            }
        }
    }
}

/// Serialize a whole compound (with an empty root name) to bytes.
pub fn to_bytes(tag: &Tag) -> Vec<u8> {
    let mut w = Writer::new();
    write_named(&mut w, "", tag);
    w.into_inner()
}

/// Parse a whole compound from bytes.
pub fn from_bytes(data: &[u8]) -> Result<Tag> {
    let mut r = Reader::new(data);
    let (_, tag) = read_named(&mut r)?;
    Ok(tag)
}

// -------------------------------------------------------------------------
// Big-endian NBT: Java's file format, used by the GeyserMC mapping tables
// (`blocks.nbt` and friends, gzip-compressed). Network NBT above is little
// endian and varint-based; this is the classic big-endian form with fixed-width
// ints and `u16`-length strings.
// -------------------------------------------------------------------------

/// Read a big-endian NBT document (type byte, name, payload).
pub fn from_bytes_be(data: &[u8]) -> Result<Tag> {
    let mut r = BeReader { data, pos: 0 };
    let id = r.u8()?;
    if id == tag_id::END {
        return Ok(Tag::End);
    }
    let _name = r.string()?;
    r.payload(id)
}

struct BeReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BeReader<'a> {
    fn need(&self, n: usize) -> Result<()> {
        if self.data.len() - self.pos < n {
            return Err(Error::decode("big-endian NBT truncated"));
        }
        Ok(())
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        self.need(n)?;
        let b = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(b)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn i16(&mut self) -> Result<i16> {
        let b = self.take(2)?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }
    fn i32(&mut self) -> Result<i32> {
        let b = self.take(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn i64(&mut self) -> Result<i64> {
        let b = self.take(8)?;
        Ok(i64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    fn string(&mut self) -> Result<String> {
        let len = u16::from_be_bytes(self.take(2)?.try_into().unwrap()) as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|e| Error::decode(format!("bad utf-8: {e}")))
    }
    fn payload(&mut self, id: u8) -> Result<Tag> {
        Ok(match id {
            tag_id::END => Tag::End,
            tag_id::BYTE => Tag::Byte(self.u8()? as i8),
            tag_id::SHORT => Tag::Short(self.i16()?),
            tag_id::INT => Tag::Int(self.i32()?),
            tag_id::LONG => Tag::Long(self.i64()?),
            tag_id::FLOAT => Tag::Float(f32::from_bits(self.i32()? as u32)),
            tag_id::DOUBLE => Tag::Double(f64::from_bits(self.i64()? as u64)),
            tag_id::BYTE_ARRAY => {
                let len = self.i32()?;
                if len < 0 {
                    return Err(Error::decode("negative byte-array length"));
                }
                Tag::ByteArray(self.take(len as usize)?.to_vec())
            }
            tag_id::STRING => Tag::String(self.string()?),
            tag_id::LIST => {
                let elem = self.u8()?;
                let len = self.i32()?;
                if len < 0 {
                    return Err(Error::decode("negative list length"));
                }
                let mut items = Vec::with_capacity((len as usize).min(1024));
                for _ in 0..len {
                    items.push(self.payload(elem)?);
                }
                Tag::List(items)
            }
            tag_id::COMPOUND => {
                let mut fields = Vec::new();
                loop {
                    let id = self.u8()?;
                    if id == tag_id::END {
                        break;
                    }
                    let name = self.string()?;
                    fields.push((name, self.payload(id)?));
                }
                Tag::Compound(fields)
            }
            tag_id::INT_ARRAY => {
                let len = self.i32()?;
                if len < 0 {
                    return Err(Error::decode("negative int-array length"));
                }
                let mut items = Vec::with_capacity((len as usize).min(4096));
                for _ in 0..len {
                    items.push(self.i32()?);
                }
                Tag::IntArray(items)
            }
            tag_id::LONG_ARRAY => {
                let len = self.i32()?;
                if len < 0 {
                    return Err(Error::decode("negative long-array length"));
                }
                let mut items = Vec::with_capacity((len as usize).min(4096));
                for _ in 0..len {
                    items.push(self.i64()?);
                }
                Tag::LongArray(items)
            }
            other => return Err(Error::decode(format!("unknown NBT tag id {other}"))),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tag_type_round_trips() {
        let tag = Tag::compound([
            ("byte", Tag::Byte(-8)),
            ("short", Tag::Short(-300)),
            ("int", Tag::Int(-70_000)),
            ("long", Tag::Long(i64::MIN)),
            ("float", Tag::Float(1.25)),
            ("double", Tag::Double(-1.5e9)),
            ("bytes", Tag::ByteArray(vec![1, 2, 3])),
            ("string", Tag::String("hi".into())),
            (
                "list",
                Tag::List(vec![Tag::Int(1), Tag::Int(2), Tag::Int(3)]),
            ),
            (
                "nested",
                Tag::compound([("inner", Tag::String("x".into()))]),
            ),
            ("ints", Tag::IntArray(vec![-1, 0, 1])),
            ("longs", Tag::LongArray(vec![i64::MAX, -5])),
        ]);
        let bytes = to_bytes(&tag);
        let back = from_bytes(&bytes).unwrap();
        assert_eq!(back, tag);
    }

    #[test]
    fn int_is_a_zigzag_varint_not_fixed_width() {
        let mut w = Writer::new();
        write_named(&mut w, "", &Tag::Int(1));
        // type byte, empty name, then zigzag(1) == 2.
        assert_eq!(w.data, vec![tag_id::INT, 0x00, 0x02]);
    }

    #[test]
    fn empty_compound_is_just_the_end_byte_payload() {
        let mut w = Writer::new();
        write_named(&mut w, "", &Tag::End);
        assert_eq!(w.data, vec![tag_id::END]);
    }

    #[test]
    fn big_endian_root_decodes() {
        // compound{} { int x = 5 }
        let bytes = [10, 0, 0, 3, 0, 1, b'x', 0, 0, 0, 5, 0];
        assert_eq!(
            from_bytes_be(&bytes).unwrap(),
            Tag::compound([("x", Tag::Int(5))])
        );
    }

    #[test]
    fn accessors_read_nested_values() {
        let tag = Tag::compound([
            ("name", Tag::String("cow".into())),
            ("id", Tag::Int(11)),
            ("visible", Tag::compound([("v", Tag::Byte(1))])),
        ]);
        assert_eq!(tag.get_str("name"), Some("cow"));
        assert_eq!(tag.get_i32("id"), Some(11));
        assert_eq!(tag.get("visible").and_then(|t| t.get_i32("v")), Some(1));
        assert_eq!(tag.get("missing"), None);
    }
}
