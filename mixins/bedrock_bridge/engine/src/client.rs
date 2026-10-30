//! The Bedrock client: RakNet transport + batch compression + the login
//! handshake state machine, producing decoded play packets.
//!
//! Layers on the wire, in order: the packet batch is zlib-compressed (after
//! `NetworkSettings` turns it on) and then encrypted (after
//! `ServerToClientHandshake` turns that on), before RakNet carries it.

use base64::Engine;
use std::io::{Read, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

use crate::crypto::{
    derive_key_iv, parse_handshake_jwt, parse_spki_x25519, Cipher, HandshakeKey, IdentityKey,
};
use crate::error::{Error, Result};
use crate::protocol::{self, Out, Packet};
use crate::raknet::RakNetClient;

/// A live Bedrock connection.
pub struct BedrockClient {
    transport: RakNetClient,
    cipher: Option<Cipher>,
    compression: bool,
    identity: Option<IdentityKey>,
    pending_login: Option<(String, String)>,
    pending_handshake: Option<HandshakeKey>,
    logged_in: bool,
    runtime_id: i64,
    unique_id: i64,
    server_address: String,
}

impl BedrockClient {
    /// Resolve `host:port`, bind a socket and complete the RakNet handshake.
    pub fn connect(host: &str, port: u16, mtu: u16) -> Result<Self> {
        let addr: SocketAddr = (host, port)
            .to_socket_addrs()
            .map_err(|e| Error::transport(format!("could not resolve {host}:{port}: {e}")))?
            .next()
            .ok_or_else(|| Error::transport(format!("no address for {host}:{port}")))?;
        let transport = RakNetClient::connect(addr, mtu)?;
        Ok(BedrockClient {
            transport,
            cipher: None,
            compression: false,
            identity: None,
            pending_login: None,
            pending_handshake: None,
            logged_in: false,
            runtime_id: 0,
            unique_id: 0,
            server_address: format!("{host}:{port}"),
        })
    }

    pub fn server_address(&self) -> &str {
        &self.server_address
    }
    pub fn runtime_id(&self) -> i64 {
        self.runtime_id
    }
    pub fn is_logged_in(&self) -> bool {
        self.logged_in
    }

    /// Begin a login: store the keys and ask for the network settings. The
    /// `Login` packet follows once the server answers with `NetworkSettings`.
    pub fn begin_login(
        &mut self,
        identity: IdentityKey,
        handshake: HandshakeKey,
        chain_json: String,
        skin_json: String,
    ) -> Result<()> {
        self.identity = Some(identity);
        self.pending_handshake = Some(handshake);
        self.pending_login = Some((chain_json, skin_json));
        self.send(&[Out::RequestNetworkSettings {
            protocol: crate::PROTOCOL_VERSION,
        }])
    }

    /// Send a batch of packets.
    pub fn send(&mut self, packets: &[Out]) -> Result<()> {
        let mut body = protocol::encode_batch(packets);
        if self.compression {
            body = zlib_compress(&body)?;
        }
        if let Some(cipher) = self.cipher.as_mut() {
            cipher.encrypt(&mut body)?;
        }
        self.transport.send(&body)
    }

    /// Receive and decode whatever is ready. Returns an empty vector on timeout.
    pub fn poll(&mut self) -> Result<Vec<Packet>> {
        let Some(mut body) = self.transport.recv()? else {
            return Ok(Vec::new());
        };
        if let Some(cipher) = self.cipher.as_mut() {
            cipher.decrypt(&mut body)?;
        }
        let batch = if self.compression {
            match zlib_decompress(&body) {
                Ok(plain) => plain,
                // The first NetworkSettings packet is not compressed.
                Err(_) => body,
            }
        } else {
            body
        };
        let packets = protocol::parse_batch(&batch)?;
        Ok(self.react(packets))
    }

    /// Apply handshake-side effects (enable compression/crypto, record ids) and
    /// return the packets for the caller.
    fn react(&mut self, packets: Vec<Packet>) -> Vec<Packet> {
        for packet in &packets {
            match packet {
                Packet::NetworkSettings(p) => {
                    self.compression = p.compression_threshold > 0;
                    if let Some((chain, skin)) = self.pending_login.take() {
                        let _ = self.send(&[Out::Login {
                            chain_json: chain,
                            skin_json: skin,
                        }]);
                    }
                }
                Packet::ServerToClientHandshake(jwt) => {
                    if let Err(e) = self.handle_encryption(jwt) {
                        log::warn!("bedrock: encryption handshake failed: {e}");
                    }
                }
                Packet::StartGame(g) => {
                    self.unique_id = g.entity_unique_id;
                    self.runtime_id = g.entity_runtime_id as i64;
                }
                Packet::PlayStatus(status) if *status == 3 => {
                    self.logged_in = true;
                }
                _ => {}
            }
        }
        packets
    }

    fn handle_encryption(&mut self, jwt: &str) -> Result<()> {
        let (x5u, salt) = parse_handshake_jwt(jwt)?;
        let server_key = crate::crypto::B64
            .decode(&x5u)
            .unwrap_or_else(|_| x5u.clone().into_bytes());
        let server_pub = parse_spki_x25519(&server_key)?;
        let handshake = self
            .pending_handshake
            .take()
            .ok_or_else(|| Error::auth("no X25519 key for the handshake"))?;
        let shared = handshake.agree(&server_pub)?;
        let (key, iv) = derive_key_iv(&shared, &salt);
        self.cipher = Some(Cipher::new(key, iv));
        // Tell the server we are ready.
        self.send(&[Out::ClientToServerHandshake])?;
        Ok(())
    }

    /// Respond to the resource-pack phase automatically.
    pub fn accept_resource_packs(&mut self) -> Result<()> {
        self.send(&[
            Out::ResourcePackClientResponse {
                status: 3,
                pack_ids: Vec::new(),
            },
            Out::ResourcePackClientResponse {
                status: 4,
                pack_ids: Vec::new(),
            },
        ])
    }

    /// Ask for chunks once the world has started, and report the player spawned.
    pub fn request_chunks(&mut self, radius: i32) -> Result<()> {
        self.send(&[
            Out::RequestChunkRadius { radius },
            Out::SetLocalPlayerAsInitialized {
                runtime_id: self.runtime_id as u64,
            },
        ])
    }
}

fn zlib_compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(4));
    enc.write_all(data)?;
    Ok(enc.finish()?)
}

fn zlib_decompress(data: &[u8]) -> Result<Vec<u8>> {
    // Bedrock historically used raw deflate; try zlib first, then raw.
    let mut out = Vec::new();
    if flate2::read::ZlibDecoder::new(data)
        .read_to_end(&mut out)
        .is_ok()
        && !out.is_empty()
    {
        return Ok(out);
    }
    let mut out = Vec::new();
    flate2::read::DeflateDecoder::new(data)
        .read_to_end(&mut out)
        .map_err(|e| Error::decode(format!("zlib: {e}")))?;
    Ok(out)
}

/// A tiny convenience for callers that want a sleep loop.
pub const POLL_INTERVAL: Duration = Duration::from_millis(5);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zlib_round_trips() {
        let data = b"the quick brown fox".repeat(20);
        let compressed = zlib_compress(&data).unwrap();
        assert_ne!(compressed, data);
        assert_eq!(zlib_decompress(&compressed).unwrap(), data);
    }

    #[test]
    fn batch_of_client_packets_parses_back() {
        let batch = protocol::encode_batch(&[
            Out::RequestChunkRadius { radius: 8 },
            Out::SetLocalPlayerAsInitialized { runtime_id: 55 },
        ]);
        // Each entry is a uvarint length + `id, body`; two small packets here.
        assert_eq!(batch[0], 2); // RequestChunkRadius: id + zigzag radius
        assert_eq!(batch[3], 2); // SetLocalPlayerAsInitialized: id + varlong id
        assert_eq!(batch.len(), 6);
    }
}
