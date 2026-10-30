//! NetherNet's little-endian primitives: length-prefixed blobs and varints, on
//! top of the crate's [`Reader`]/[`Writer`].
//!
//! Not to be confused with [`crate::stream`]'s MCPE varints — same LEB128
//! shape, but NetherNet caps a length field at [`MAX_BYTES`].

use crate::error::{Error, Result};
use crate::nethernet::MAX_BYTES;
use crate::stream::{Reader, Writer};

/// Types that can be written to and read from a NetherNet stream.
pub trait NetherCodec: Sized {
    fn serialize(&self, w: &mut Writer);
    fn deserialize(r: &mut Reader) -> Result<Self>;
}

pub fn write_varuint(w: &mut Writer, value: u32) {
    w.varint(value);
}
pub fn read_varuint(r: &mut Reader) -> Result<u32> {
    r.varint()
}

pub fn write_varint(w: &mut Writer, value: i32) {
    w.zigzag32(value);
}
pub fn read_varint(r: &mut Reader) -> Result<i32> {
    r.zigzag32()
}

/// A `u8`-length blob; fails above 255 bytes.
pub fn write_bytes_u8(w: &mut Writer, data: &[u8]) -> Result<()> {
    let len = u8::try_from(data.len())
        .map_err(|_| Error::other(format!("blob {} exceeds 255 bytes", data.len())))?;
    w.u8(len).bytes(data);
    Ok(())
}
pub fn read_bytes_u8(r: &mut Reader) -> Result<Vec<u8>> {
    let len = r.u8()? as usize;
    Ok(r.bytes(len)?.to_vec())
}

/// A little-endian `u32`-length blob; fails above [`MAX_BYTES`].
pub fn write_bytes_u32(w: &mut Writer, data: &[u8]) -> Result<()> {
    if data.len() > MAX_BYTES {
        return Err(Error::other(format!(
            "blob {} exceeds {MAX_BYTES}",
            data.len()
        )));
    }
    w.i32(data.len() as i32).bytes(data);
    Ok(())
}
pub fn read_bytes_u32(r: &mut Reader) -> Result<Vec<u8>> {
    let len = r.i32()? as usize;
    if len > MAX_BYTES {
        return Err(Error::decode(format!(
            "blob length {len} exceeds {MAX_BYTES}"
        )));
    }
    Ok(r.bytes(len)?.to_vec())
}

/// A varint-length blob.
pub fn write_bytes_varuint(w: &mut Writer, data: &[u8]) -> Result<()> {
    write_bytes_u32_checked(data)?;
    w.varint(data.len() as u32).bytes(data);
    Ok(())
}
pub fn read_bytes_varuint(r: &mut Reader) -> Result<Vec<u8>> {
    let len = r.varint()? as usize;
    if len > MAX_BYTES {
        return Err(Error::decode(format!(
            "blob length {len} exceeds {MAX_BYTES}"
        )));
    }
    Ok(r.bytes(len)?.to_vec())
}

fn write_bytes_u32_checked(data: &[u8]) -> Result<()> {
    if data.len() > MAX_BYTES {
        return Err(Error::other(format!(
            "blob {} exceeds {MAX_BYTES}",
            data.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u8_blob_round_trips_and_rejects_oversize() {
        let mut w = Writer::new();
        write_bytes_u8(&mut w, b"hello").unwrap();
        assert_eq!(read_bytes_u8(&mut Reader::new(&w.data)).unwrap(), b"hello");
        let mut w = Writer::new();
        assert!(write_bytes_u8(&mut w, &[0u8; 256]).is_err());
    }

    #[test]
    fn u32_and_varuint_blobs_round_trip() {
        let data = vec![7u8; 300];
        let mut w = Writer::new();
        write_bytes_u32(&mut w, &data).unwrap();
        write_bytes_varuint(&mut w, &data).unwrap();
        let mut r = Reader::new(&w.data);
        assert_eq!(read_bytes_u32(&mut r).unwrap(), data);
        assert_eq!(read_bytes_varuint(&mut r).unwrap(), data);
    }

    #[test]
    fn varints_round_trip() {
        let mut w = Writer::new();
        write_varuint(&mut w, 300);
        write_varint(&mut w, -5);
        let mut r = Reader::new(&w.data);
        assert_eq!(read_varuint(&mut r).unwrap(), 300);
        assert_eq!(read_varint(&mut r).unwrap(), -5);
    }
}
