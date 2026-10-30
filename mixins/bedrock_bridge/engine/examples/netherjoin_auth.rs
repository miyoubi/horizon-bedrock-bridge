//! End-to-end NetherNet join using a Minecraft-authorization-service identity.
//!
//!   cargo run -p bedrock-core --features nethernet-webrtc --example netherjoin_auth -- \
//!     http://127.0.0.1:19132 127.0.0.1:7551
//!
//! Signs in with a Microsoft account via device code, mints the multiplayer
//! identity token (PlayFab -> session/start -> multiplayer/session/start), then
//! discovers, joins over WebRTC, and sends RequestNetworkSettings.

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use bedrock_core::auth::{begin_device_code, playfab_xbl3, poll_device_code};
use bedrock_core::crypto::IdentityKey;
use bedrock_core::nethernet::auth_service;
use bedrock_core::nethernet::identity::ClientIdentity;
use bedrock_core::nethernet::{discovery, webrtc::Session};
use bedrock_core::protocol::{self, Out};

fn discover(addr: &str) -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    socket
        .send_to(&discovery::encode_request(0x1122_3344_5566_7788), addr)
        .ok()?;
    let mut buf = [0u8; 65535];
    let (n, _) = socket.recv_from(&mut buf).ok()?;
    let (data, sender) = discovery::decode_response(&buf[..n]).ok()?;
    println!(
        "discovered {:?} protocol {}",
        data.server_name, data.protocol_version
    );
    Some(sender.to_string())
}

fn sign_in() -> Result<bedrock_core::auth::MsaToken, Box<dyn std::error::Error>> {
    let code = begin_device_code()?;
    println!(
        "SIGN IN: open {} and enter code {}",
        code.verification_uri, code.user_code
    );
    loop {
        if let Some(token) = poll_device_code(&code)? {
            return Ok(token);
        }
        std::thread::sleep(Duration::from_secs(code.interval));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:19132".into());
    let discovery_addr = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "127.0.0.1:7551".into());

    // 1. Microsoft sign-in: reuse a cached refresh token, else device code.
    let cache = std::path::Path::new("/tmp/nnauth-msa.json");
    let cached = std::fs::read_to_string(cache)
        .ok()
        .and_then(|t| serde_json::from_str::<String>(&t).ok());
    let msa = if let Some(refresh) = cached {
        println!("using cached sign-in");
        bedrock_core::auth::refresh(&refresh).unwrap_or_else(|_| sign_in().unwrap())
    } else {
        sign_in()?
    };
    if let Some(refresh) = &msa.refresh_token {
        let _ = std::fs::write(cache, serde_json::to_string(refresh).unwrap_or_default());
    }
    println!("signed in");

    // 2. PlayFab -> session -> multiplayer token.
    let xbl3 = playfab_xbl3(&msa.access_token)?;
    println!("got PlayFab XSTS");
    let ticket = auth_service::playfab_login(&xbl3)?;
    println!("got PlayFab session ticket");
    let auth_header = auth_service::session_start(&ticket, bedrock_core::MINECRAFT_VERSION)?;
    println!("got session authorization header");
    let key = IdentityKey::generate()?;
    let token = auth_service::multiplayer_token(&auth_header, &key.public_der())?;
    println!("got multiplayer identity token ({} bytes)", token.len());
    let identity = ClientIdentity::external(key, token);

    // 3. Discover + join.
    let Some(network_id) = discover(&discovery_addr) else {
        return Err("no discovery reply".into());
    };
    let mut session = Session::connect(&base, &network_id, &identity)?;
    println!("offer sent, local udp {}", session.local_addr());
    let deadline = Instant::now() + Duration::from_secs(25);
    while !session.is_ready() && Instant::now() < deadline {
        session.pump(Duration::from_millis(250))?;
    }
    if !session.is_ready() {
        println!("data channel did not open");
        return Ok(());
    }
    println!("data channel open");

    let batch = protocol::encode_batch(&[Out::RequestNetworkSettings {
        protocol: bedrock_core::PROTOCOL_VERSION,
    }]);
    for segment in bedrock_core::nethernet::message::Message::split_into_segments(&batch)? {
        session.send_reliable(&segment.encode())?;
    }
    println!("sent RequestNetworkSettings");

    let mut reassembly = bedrock_core::nethernet::message::Message::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        for raw in session.pump(Duration::from_millis(250))? {
            let Ok(seg) = bedrock_core::nethernet::message::MessageSegment::decode(&raw) else {
                continue;
            };
            let Ok(Some(msg)) = reassembly.add_segment(seg) else {
                continue;
            };
            for packet in protocol::parse_batch(&msg)? {
                println!("<- {:?}", std::mem::discriminant(&packet));
            }
        }
    }
    session.close();
    Ok(())
}
