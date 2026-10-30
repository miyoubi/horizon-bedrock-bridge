//! NetherNet: the WebRTC-based transport newer Bedrock clients use (analysed in
//! `libminecraftpe.so` — JSON-RPC signalling over HTTP/LAN, then WebRTC with
//! DTLS and SCTP data channels).
//!
//! This module ports the **portable** half of NetherNet into `bedrock-core`: the
//! wire codec, the data-channel segmentation, the signaling message, and the
//! discovery constants. The connection half — ICE, DTLS, SCTP, the data
//! channels — needs a WebRTC stack (the reference uses the `rtc` crate, and the
//! upstream game links libwebrtc). A native-mod bundle crate may only depend on
//! crates already in Horizon's `Cargo.lock`, and no WebRTC stack is there, so
//! that half stays behind [`Transport`] until Horizon carries one. Everything
//! here is dependency-light and unit-tested, so it is ready to plug into a
//! session as soon as the media stack is available.

pub mod codec;
pub mod discovery;
pub mod http;
pub mod identity;
pub mod join;
pub mod message;
pub mod sdp;
pub mod signal;
#[cfg(feature = "nethernet-webrtc")]
pub mod webrtc;

pub use codec::NetherCodec;
pub use message::{Message, MessageSegment};
pub use signal::{Signal, SignalType};

/// UDP port LAN discovery uses.
pub const LAN_DISCOVERY_PORT: u16 = 7551;
/// The data-channel port NetherNet opens an SCTP stream on.
pub const SCTP_PORT: u16 = 5000;

/// Discovery packet ids.
pub const ID_REQUEST_PACKET: u16 = 0;
pub const ID_RESPONSE_PACKET: u16 = 1;
pub const ID_MESSAGE_PACKET: u16 = 2;

/// Largest payload the codec accepts for a u32-length field.
pub const MAX_BYTES: usize = 65535;
/// Payload bytes carried in one data-channel segment.
pub const MAX_MESSAGE_SIZE: usize = 10000;

/// The two data channels every NetherNet session opens.
pub const RELIABLE_CHANNEL: &str = "ReliableDataChannel";
pub const UNRELIABLE_CHANNEL: &str = "UnreliableDataChannel";

/// Which transport a Bedrock connection uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// RakNet over UDP (the classic transport; implemented in [`crate::raknet`]).
    RakNet,
    /// NetherNet over WebRTC (signalling implemented here; the media stack is
    /// not, see the module note).
    NetherNet,
}

impl Transport {
    /// Pick a transport from an address scheme: `nethernet:` forces NetherNet,
    /// anything else uses RakNet.
    pub fn from_address(address: &str) -> Self {
        if address.starts_with("nethernet:") {
            Transport::NetherNet
        } else {
            Transport::RakNet
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_from_scheme() {
        assert_eq!(
            Transport::from_address("nethernet:x:19132"),
            Transport::NetherNet
        );
        assert_eq!(Transport::from_address("host:19132"), Transport::RakNet);
    }
}
