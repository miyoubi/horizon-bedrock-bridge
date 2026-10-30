//! `bedrock-core`: a Minecraft: Bedrock Edition **client** stack, written for the
//! Horizon bedrock-bridge native mod.
//!
//! Horizon speaks Java Edition protocol 777 to servers through
//! `horizon_protocol`. Bedrock servers speak an entirely different protocol over
//! RakNet (and increasingly over NetherNet). This crate is the other half: it
//! dials a Bedrock server, authenticates with the player's Xbox Live account,
//! logs in, and decodes the Bedrock world so a translator can turn it into the
//! Java packets Horizon already understands.
//!
//! The crate is deliberately free of Horizon dependencies: it only uses crates
//! already present in Horizon's `Cargo.lock`, so it stages as a native-mod bundle
//! crate and still compiles for `cargo test` on its own.
//!
//! Layers, bottom to top:
//!
//! * [`stream`] — little-endian primitives, varints and zigzag (Bedrock's base
//!   wire encoding).
//! * [`nbt`] — network NBT (little-endian, varint int/long, Bedrock flavour).
//! * [`bitarray`] / [`subchunk`] — the paletted sub-chunk format Bedrock sends
//!   chunks in.
//! * [`raknet`] — the RakNet reliability layer (offline handshake, datagrams,
//!   ACK/NACK, ordering, fragmentation).
//! * [`protocol`] — Bedrock packet ids and codecs, on top of [`stream`].
//! * [`crypto`] — the `ServerToClientHandshake` ECDH + AES stream cipher.
//! * [`auth`] — Xbox Live device-code sign-in and the Bedrock login JWT chain.
//! * [`mapping`] — Bedrock↔Java block/biome/item tables and the addon entity
//!   registry (custom entities).
//! * [`client`] — ties it together: connect, authenticate, log in, then feed
//!   decoded packets to a caller.

#![forbid(unsafe_code)]

pub mod auth;
pub mod bitarray;
pub mod client;
pub mod crypto;
pub mod error;
pub mod mapping;
pub mod nbt;
pub mod protocol;
pub mod raknet;
pub mod stream;
pub mod subchunk;

pub use error::{Error, Result};

/// The Bedrock protocol version this build speaks (Minecraft Bedrock 1.26.60,
/// which matches the Java↔Bedrock mappings in `references/mappings-master`).
pub const PROTOCOL_VERSION: i32 = 2225;

/// The Minecraft version string advertised in `RequestNetworkSettings`.
pub const MINECRAFT_VERSION: &str = "1.26.60";

/// `ProtocolInfo.VersionInfo` version id used inside the login JWT chain.
pub const CHAIN_VERSION: i32 = 2;

/// The name Horizon's launcher gives client data / the default device model.
pub const DEVICE_MODEL: &str = "Horizon";
pub const DEVICE_OS: i32 = 7; // Android is 7 in Bedrock's DeviceOS enum; a neutral value.
