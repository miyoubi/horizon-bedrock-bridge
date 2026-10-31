//! A minimal PNG writer.
//!
//! Java's player skins travel as a PNG behind a `textures` profile property,
//! and Bedrock hands over raw RGBA pixels, so the bridge has to make the image
//! itself. Only what a skin needs: 8-bit RGBA, one IDAT, no interlacing.

use std::io::Write;

/// Encode 8-bit RGBA pixels as a PNG.
pub fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let stride = width as usize * 4;
    let mut raw = Vec::with_capacity(rgba.len() + height as usize);
    for row in rgba.chunks(stride.max(1)) {
        raw.push(0); // filter: none
        raw.extend_from_slice(row);
    }

    let mut png = Vec::new();
    png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &header);

    let mut deflate = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    let _ = deflate.write_all(&raw);
    let idat = deflate.finish().unwrap_or_default();
    chunk(&mut png, b"IDAT", &idat);
    chunk(&mut png, b"IEND", &[]);
    png
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(data.len() + 4);
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    crc ^ 0xffff_ffff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_png_that_inflates_to_the_pixels() {
        let pixels = [255, 0, 0, 255, 0, 255, 0, 255];
        let png = encode_rgba(2, 1, &pixels);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        let mut at = 8;
        let mut raw = Vec::new();
        while at + 12 <= png.len() {
            let len = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
            let kind = &png[at + 4..at + 8];
            let data = &png[at + 8..at + 8 + len];
            if kind == b"IDAT" {
                use std::io::Read;
                let _ = flate2::read::ZlibDecoder::new(data).read_to_end(&mut raw);
            }
            at += 12 + len;
        }
        // One filter byte, then the RGBA of each pixel.
        assert_eq!(raw, [0, 255, 0, 0, 255, 0, 255, 0, 255]);
    }
}
