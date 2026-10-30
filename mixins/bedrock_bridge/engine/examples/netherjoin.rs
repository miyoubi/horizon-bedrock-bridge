//! Live NetherNet probe: discover a server, join over WebRTC, and send a Bedrock
//! `RequestNetworkSettings` batch, printing anything that comes back.
//!
//!   cargo run -p bedrock-core --features nethernet-webrtc --example netherjoin -- \
//!     http://127.0.0.1:19132 127.0.0.1:7551
//!
//! Only works against a NetherNet server (HTTP signalling on the first arg,
//! discovery on the second).

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use bedrock_core::crypto::IdentityKey;
use bedrock_core::nethernet::identity::ClientIdentity;
use bedrock_core::nethernet::message::{Message, MessageSegment};
use bedrock_core::nethernet::{discovery, webrtc::Session};
use bedrock_core::protocol::{self, Out};

fn discover(addr: &str) -> Option<(String, bedrock_core::nethernet::discovery::ServerData)> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let request = discovery::encode_request(0x1122_3344_5566_7788);
    socket.send_to(&request, addr).ok()?;
    let mut buf = [0u8; 65535];
    let (n, _) = socket.recv_from(&mut buf).ok()?;
    let (data, sender) = discovery::decode_response(&buf[..n]).ok()?;
    Some((sender.to_string(), data))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:19132".into());
    let discovery_addr = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "127.0.0.1:7551".into());

    let (network_id, data) = discover(&discovery_addr)
        .ok_or("no discovery reply (is the NetherNet server on UDP 7551?)")?;
    println!(
        "discovered {:?} (protocol {}) network_id={network_id}",
        data.server_name, data.protocol_version
    );

    let identity = ClientIdentity::new(IdentityKey::generate()?).with_domain("horizon-bedrock");
    let mut session = Session::connect(&base, &network_id, &identity)?;
    println!("offer sent, local udp {}", session.local_addr());

    // Wait for the data channel.
    let deadline = Instant::now() + Duration::from_secs(20);
    while !session.is_ready() && Instant::now() < deadline {
        session.pump(Duration::from_millis(250))?;
    }
    if !session.is_ready() {
        println!("data channel did not open within 20s");
        session.close();
        return Ok(());
    }
    println!("data channel open");

    // Send RequestNetworkSettings as one (unsegmented) message.
    let batch = protocol::encode_batch(&[Out::RequestNetworkSettings {
        protocol: bedrock_core::PROTOCOL_VERSION,
    }]);
    let segments = Message::split_into_segments(&batch)?;
    for segment in segments {
        session.send_reliable(&segment.encode())?;
    }
    println!("sent RequestNetworkSettings");

    let mut reassembly = Message::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut packets = 0usize;
    while Instant::now() < deadline && packets < 20 {
        for raw in session.pump(Duration::from_millis(250))? {
            let Ok(segment) = MessageSegment::decode(&raw) else {
                continue;
            };
            if let Ok(Some(message)) = reassembly.add_segment(segment) {
                for packet in protocol::parse_batch(&message)? {
                    println!("<- {:?}", std::mem::discriminant(&packet));
                    packets += 1;
                }
            }
        }
    }
    println!("done, {packets} packets");
    session.close();
    Ok(())
}
