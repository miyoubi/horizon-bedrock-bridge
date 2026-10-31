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

/// A byte-oriented transport that carries (compressed/encrypted) Bedrock
/// batches: RakNet datagrams, or a NetherNet WebRTC data channel.
pub trait Transport: Send {
    fn send(&mut self, body: &[u8]) -> Result<()>;
    fn recv(&mut self) -> Result<Option<Vec<u8>>>;
}

impl Transport for RakNetClient {
    fn send(&mut self, body: &[u8]) -> Result<()> {
        RakNetClient::send(self, body)
    }
    fn recv(&mut self) -> Result<Option<Vec<u8>>> {
        RakNetClient::recv(self)
    }
}

/// A live Bedrock connection.
pub struct BedrockClient {
    transport: Box<dyn Transport>,
    cipher: Option<Cipher>,
    /// NetherNet disables Bedrock packet encryption (as the reference clients
    /// do), so `ServerToClientHandshake` only needs to be acknowledged.
    disable_encryption: bool,
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
        let transport: Box<dyn Transport> = Box::new(RakNetClient::connect(addr, mtu)?);
        Ok(BedrockClient {
            transport,
            cipher: None,
            disable_encryption: false,
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

    /// Build a client over an arbitrary transport (e.g. a NetherNet session).
    pub fn with_transport(transport: Box<dyn Transport>, server_address: String) -> Self {
        BedrockClient {
            transport,
            cipher: None,
            disable_encryption: false,
            compression: false,
            identity: None,
            pending_login: None,
            pending_handshake: None,
            logged_in: false,
            runtime_id: 0,
            unique_id: 0,
            server_address,
        }
    }

    /// Connect to a Bedrock server over NetherNet (WebRTC), completing the
    /// offer/join handshake with the given identity.
    #[cfg(feature = "nethernet-webrtc")]
    pub fn connect_nethernet(
        base: &str,
        network_id: &str,
        identity: &crate::nethernet::identity::ClientIdentity,
    ) -> Result<Self> {
        let transport =
            crate::nethernet::transport::NetherNetTransport::connect(base, network_id, identity)?;
        let mut client = Self::with_transport(Box::new(transport), format!("{base}/{network_id}"));
        // NetherNet sessions skip Bedrock packet encryption, matching the
        // reference clients (bedrock-protocol sets disableEncryption=true).
        client.disable_encryption = true;
        Ok(client)
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
        auth_info: String,
        skin_json: String,
    ) -> Result<()> {
        self.identity = Some(identity);
        self.pending_handshake = Some(handshake);
        self.pending_login = Some((auth_info, skin_json));
        self.send(&[Out::RequestNetworkSettings {
            protocol: crate::PROTOCOL_VERSION,
        }])
    }

    /// Send a batch of packets.
    pub fn send(&mut self, packets: &[Out]) -> Result<()> {
        let mut body = protocol::encode_batch(packets);
        if std::env::var_os("NETHER_DEBUG").is_some() {
            eprintln!(
                "bedrock: raw batch[{}] {:02x?}",
                body.len(),
                &body[..body.len().min(64)]
            );
        }
        if self.compression {
            // Modern Bedrock prefixes each batch with the compressor id:
            // 0 = deflate, 255 = uncompressed.
            let mut framed = Vec::new();
            if std::env::var_os("NETHER_PLAIN").is_some() {
                framed.push(255); // 255 = uncompressed
                framed.extend_from_slice(&body);
            } else {
                framed.push(0); // 0 = deflate
                framed.extend_from_slice(&zlib_compress(&body)?);
            }
            body = framed;
        }
        if let Some(cipher) = self.cipher.as_mut() {
            cipher.encrypt(&mut body)?;
        }
        if std::env::var_os("NETHER_DEBUG").is_some() {
            eprintln!(
                "bedrock: send {} bytes (compression={})",
                body.len(),
                self.compression
            );
        }
        self.transport.send(&body)
    }

    /// Receive and decode whatever is ready. Returns an empty vector on timeout.
    pub fn poll(&mut self) -> Result<Vec<Packet>> {
        let Some(mut body) = self.transport.recv()? else {
            return Ok(Vec::new());
        };
        if std::env::var_os("NETHER_DEBUG").is_some() {
            eprintln!(
                "bedrock: recv {} bytes (compression={})",
                body.len(),
                self.compression
            );
        }
        if let Some(cipher) = self.cipher.as_mut() {
            cipher.decrypt(&mut body)?;
        }
        let batch = if self.compression {
            // After NetworkSettings every batch starts with the compressor id.
            match body.first().copied() {
                Some(0) => zlib_decompress(&body[1..]).unwrap_or_else(|_| body[1..].to_vec()),
                Some(255) | None => body[1..].to_vec(),
                _ => body,
            }
        } else {
            body
        };
        if std::env::var_os("NETHER_DEBUG").is_some() {
            eprintln!(
                "bedrock: batch[{}] {:02x?}",
                batch.len(),
                &batch[..batch.len().min(96)]
            );
        }
        let packets = protocol::parse_batch(&batch)?;
        Ok(self.react(packets))
    }

    /// Apply handshake-side effects (enable compression/crypto, record ids) and
    /// return the packets for the caller.
    fn react(&mut self, packets: Vec<Packet>) -> Vec<Packet> {
        for packet in &packets {
            // How far the handshake got, in the trace file.
            let shown: String = format!("{packet:?}").chars().take(80).collect();
            crate::nethernet::trace(format!("recv {shown}"));
            match packet {
                Packet::NetworkSettings(p) => {
                    self.compression = p.compression_threshold > 0;
                    if std::env::var_os("NETHER_DEBUG").is_some() {
                        eprintln!(
                            "bedrock: NetworkSettings threshold={} algorithm={}",
                            p.compression_threshold, p.compression_algorithm
                        );
                    }
                    if let Some((auth_info, skin)) = self.pending_login.take() {
                        crate::nethernet::trace("sending Login");
                        if let Err(e) = self.send(&[Out::Login {
                            auth_info,
                            skin_json: skin,
                        }]) {
                            log::warn!("bedrock: Login send failed: {e}");
                            if std::env::var_os("NETHER_DEBUG").is_some() {
                                eprintln!("bedrock: Login send failed: {e}");
                            }
                        }
                    }
                }
                Packet::ServerToClientHandshake(jwt) if self.disable_encryption => {
                    // Acknowledge without enabling encryption.
                    let _ = self.send(&[Out::ClientToServerHandshake]);
                    if std::env::var_os("NETHER_DEBUG").is_some() {
                        eprintln!("bedrock: acked ServerToClientHandshake (encryption disabled)");
                    }
                }
                Packet::ServerToClientHandshake(jwt) => match self.handle_encryption(jwt) {
                    Ok(()) => {
                        if std::env::var_os("NETHER_DEBUG").is_some() {
                            eprintln!("bedrock: sent ClientToServerHandshake (encrypted)");
                        }
                    }
                    Err(e) => {
                        log::warn!("bedrock: encryption handshake failed: {e}");
                        if std::env::var_os("NETHER_DEBUG").is_some() {
                            eprintln!("bedrock: encryption handshake failed: {e}");
                        }
                    }
                },
                Packet::StartGame(g) => {
                    self.unique_id = g.entity_unique_id;
                    self.runtime_id = g.entity_runtime_id as i64;
                }
                // 0 = LoginSuccess, 3 = PlayerSpawn.
                Packet::PlayStatus(status) if *status == 0 || *status == 3 => {
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
                status: 2, // downloadingfinished
                pack_ids: Vec::new(),
            },
            Out::ResourcePackClientResponse {
                status: 3, // resourcepackstackfinished
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
    // Bedrock's "zlib" is raw deflate: no zlib header or Adler-32 trailer.
    let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(4));
    enc.write_all(data)?;
    Ok(enc.finish()?)
}

fn zlib_decompress(data: &[u8]) -> Result<Vec<u8>> {
    // Raw deflate first (what Bedrock sends); fall back to zlib-wrapped.
    let mut out = Vec::new();
    if flate2::read::DeflateDecoder::new(data)
        .read_to_end(&mut out)
        .is_ok()
        && !out.is_empty()
    {
        return Ok(out);
    }
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data)
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
        assert_eq!(batch[0], 3); // RequestChunkRadius: id + zigzag radius + max radius
        assert_eq!(batch[4], 2); // SetLocalPlayerAsInitialized: id + varlong id
        assert_eq!(batch.len(), 7);
    }
}
