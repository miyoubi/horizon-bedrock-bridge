//! NetherNet LAN discovery: encrypted UDP datagrams carrying a `ServerData`
//! record, so a client can find servers on the local network without WebRTC.
//!
//! Datagram = 32-byte HMAC-SHA256, then the AES-256-ECB-encrypted payload. The
//! payload is `u16 length | header | body`, where the header is the packet id
//! (`u16`), the sender id (`u64`) and eight zero bytes. The key is
//! `SHA-256(0xdeadbeef_u64.to_le_bytes())`, the same for every client.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

use crate::aes::{pad, unpad, Aes256};
use crate::error::{Error, Result};
use crate::nethernet::{ID_REQUEST_PACKET, ID_RESPONSE_PACKET, MAX_BYTES};
use crate::stream::{Reader, Writer};

type HmacSha256 = Hmac<Sha256>;

fn key() -> &'static [u8; 32] {
    static KEY: OnceLock<[u8; 32]> = OnceLock::new();
    KEY.get_or_init(|| {
        let mut h = Sha256::new();
        h.update(0xdeadbeef_u64.to_le_bytes());
        let out = h.finalize();
        let mut k = [0u8; 32];
        k.copy_from_slice(&out);
        k
    })
}

fn cipher() -> &'static Aes256 {
    static CIPHER: OnceLock<Aes256> = OnceLock::new();
    CIPHER.get_or_init(|| Aes256::new(key()))
}

fn checksum(data: &[u8]) -> [u8; 32] {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key()).expect("hmac key");
    mac.update(data);
    let out = mac.finalize().into_bytes();
    let mut c = [0u8; 32];
    c.copy_from_slice(&out);
    c
}

fn checksum_ok(data: &[u8], expected: &[u8]) -> bool {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key()).expect("hmac key");
    mac.update(data);
    mac.verify_slice(expected).is_ok()
}

/// Server details advertised over discovery.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServerData {
    pub server_name: String,
    pub level_name: String,
    pub game_type: i32,
    pub player_count: i32,
    pub max_player_count: i32,
    pub hardcore: bool,
    pub session_id: String,
    pub protocol_version: i32,
    pub game_version: String,
}

impl ServerData {
    /// Decode the binary record. Versions 4-6 are accepted structurally, but
    /// only version 7 (the current layout) is read in full.
    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut r = Reader::new(data);
        let version = r.u8()?;
        if version != 7 {
            return Err(Error::decode(format!(
                "unsupported NetherNet server-data version {version}"
            )));
        }
        let var_string = |r: &mut Reader| -> Result<String> {
            let len = r.varint()? as usize;
            Ok(String::from_utf8_lossy(r.bytes(len)?).into_owned())
        };
        let server_name = var_string(&mut r)?;
        let protocol_version = r.zigzag32()?;
        let game_version = var_string(&mut r)?;
        let level_name = var_string(&mut r)?;
        let player_count = r.zigzag32()?;
        let max_player_count = r.zigzag32()?;
        let game_type = r.zigzag32()?;
        let _editor = r.u8()? != 0;
        let hardcore = r.u8()? != 0;
        let _flag_a = r.u8()? != 0;
        let _flag_b = r.u8()? != 0;
        let session_id = var_string(&mut r)?;
        let _connection_type = r.zigzag32()?;
        Ok(ServerData {
            server_name,
            level_name,
            game_type,
            player_count,
            max_player_count,
            hardcore,
            session_id,
            protocol_version,
            game_version,
        })
    }
}

/// Build a discovery request datagram.
pub fn encode_request(sender_id: u64) -> Vec<u8> {
    let mut body = Writer::new();
    body.data.resize(2, 0); // u16 length, filled below
    body.u16(ID_REQUEST_PACKET).u64(sender_id);
    body.bytes(&[0u8; 8]);
    let len = (body.data.len() - 2) as u16;
    body.data[..2].copy_from_slice(&len.to_le_bytes());

    let mut payload = body.into_inner();
    let mac = checksum(&payload);
    pad(&mut payload);
    cipher().encrypt_ecb(&mut payload);

    let mut datagram = Vec::with_capacity(32 + payload.len());
    datagram.extend_from_slice(&mac);
    datagram.extend_from_slice(&payload);
    datagram
}

/// Decode a discovery datagram: returns the server data and the sender id.
pub fn decode_response(datagram: &[u8]) -> Result<(ServerData, u64)> {
    if datagram.len() < 32 {
        return Err(Error::decode("discovery datagram too short"));
    }
    let mac = &datagram[..32];
    let mut payload = datagram[32..].to_vec();
    cipher().decrypt_ecb(&mut payload);
    unpad(&mut payload).ok_or_else(|| Error::decode("discovery padding is invalid"))?;
    if !checksum_ok(&payload, mac) {
        return Err(Error::decode("discovery checksum mismatch"));
    }

    let mut r = Reader::new(&payload);
    let _length = r.u16()?;
    let packet_id = r.u16()?;
    let sender_id = r.u64()?;
    let _padding = r.bytes(8)?;
    if packet_id != ID_RESPONSE_PACKET {
        return Err(Error::decode(format!(
            "expected response packet, got id {packet_id}"
        )));
    }
    let hex_len = r.i32()?;
    if hex_len < 0 || hex_len as usize > MAX_BYTES * 2 {
        return Err(Error::decode("bad discovery hex length"));
    }
    let hex = r.bytes(hex_len as usize)?;
    let app = hex_decode(hex)?;
    Ok((ServerData::decode(&app)?, sender_id))
}

fn hex_decode(hex: &[u8]) -> Result<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return Err(Error::decode("odd-length hex"));
    }
    let nib = |b: u8| -> Result<u8> {
        match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            b'A'..=b'F' => Ok(b - b'A' + 10),
            _ => Err(Error::decode("invalid hex digit")),
        }
    };
    let mut out = Vec::with_capacity(hex.len() / 2);
    for pair in hex.chunks_exact(2) {
        out.push((nib(pair[0])? << 4) | nib(pair[1])?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_through_the_cipher() {
        let datagram = encode_request(0x1122_3344_5566_7788);
        // Decrypt it back the way decode does, then read the header.
        let mut payload = datagram[32..].to_vec();
        cipher().decrypt_ecb(&mut payload);
        unpad(&mut payload).unwrap();
        assert!(checksum_ok(&payload, &datagram[..32]));
        let mut r = Reader::new(&payload);
        assert_eq!(r.u16().unwrap() as usize, payload.len() - 2);
        assert_eq!(r.u16().unwrap(), ID_REQUEST_PACKET);
        assert_eq!(r.u64().unwrap(), 0x1122_3344_5566_7788);
    }

    #[test]
    fn hex_decodes() {
        assert_eq!(hex_decode(b"00ff10").unwrap(), vec![0x00, 0xff, 0x10]);
        assert!(hex_decode(b"0").is_err());
        assert!(hex_decode(b"zz").is_err());
    }

    #[test]
    fn server_data_v7_decodes() {
        // version 7; "srv"; protocol 2225; "1.26.61"; "world"; 1/10; type 0;
        // editor 0, hardcore 0, flags 1 1; session "nonce"; connection 4.
        let mut w = Writer::new();
        w.u8(7)
            .string("srv")
            .zigzag32(2225)
            .string("1.26.61")
            .string("world");
        w.zigzag32(1).zigzag32(10).zigzag32(0);
        w.u8(0).u8(0).u8(1).u8(1).string("nonce").zigzag32(4);
        let data = ServerData::decode(&w.data).unwrap();
        assert_eq!(data.server_name, "srv");
        assert_eq!(data.protocol_version, 2225);
        assert_eq!(data.game_version, "1.26.61");
        assert_eq!(data.level_name, "world");
        assert_eq!(data.player_count, 1);
        assert_eq!(data.max_player_count, 10);
        assert_eq!(data.session_id, "nonce");
    }
}
