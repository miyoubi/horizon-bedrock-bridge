//! End-to-end NetherNet login.
//!
//!   cargo run -p bedrock-core --features nethernet-webrtc --example netherjoin_auth -- \
//!     http://127.0.0.1:19132 127.0.0.1:7551 127.0.0.1:19132
//!
//! Signs in (device code, cached), mints the multiplayer identity token, builds
//! the Bedrock chain from the same Microsoft account, joins over NetherNet
//! (WebRTC) and runs the login handshake to `StartGame`.

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use bedrock_core::auth::{begin_device_code, login_with_identity, playfab_xbl3, poll_device_code};
use bedrock_core::client::BedrockClient;
use bedrock_core::crypto::IdentityKey;
use bedrock_core::nethernet::auth_service;
use bedrock_core::nethernet::discovery;
use bedrock_core::nethernet::identity::ClientIdentity;
use bedrock_core::protocol::Packet;

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
    let server_address = std::env::args()
        .nth(3)
        .unwrap_or_else(|| "127.0.0.1:19132".into());

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

    // 2. NetherNet identity token: XSTS(PlayFab) -> PlayFab -> session ->
    //    multiplayer/session/start with our P-384 key.
    let xbl3 = playfab_xbl3(&msa.access_token)?;
    let ticket = auth_service::playfab_login(&xbl3)?;
    let auth_header = auth_service::session_start(&ticket, bedrock_core::MINECRAFT_VERSION)?;
    let nether_key = IdentityKey::generate()?;
    let token = auth_service::multiplayer_token(&auth_header, &nether_key.public_der())?;
    println!("got multiplayer identity token ({} bytes)", token.len());

    // 3. Bedrock chain from the same account + the modern Login authInfo, then
    //    log in over NetherNet.
    let data = login_with_identity(
        &msa.access_token,
        &server_address,
        bedrock_core::MINECRAFT_VERSION,
        nether_key.try_clone()?,
    )?;
    println!("built chain for {}", data.display_name);
    // The modern Login authInfo: the chain plus the multiplayer token, Full auth.
    let auth_info = bedrock_core::auth::auth_info(&data.chain_json, &token, 0);
    let identity = ClientIdentity::external(nether_key, token);
    let Some(network_id) = discover(&discovery_addr) else {
        return Err("no discovery reply".into());
    };
    let mut client = BedrockClient::connect_nethernet(&base, &network_id, &identity)?;
    println!("netherNet connected, data channel open");
    client.begin_login(data.identity, data.handshake, auth_info, data.skin_json)?;

    let deadline = Instant::now() + Duration::from_secs(40);
    while Instant::now() < deadline && !client.is_logged_in() {
        for packet in client.poll()? {
            let debug = format!("{packet:?}");
            println!("<- {}", &debug[..debug.len().min(220)]);
            match packet {
                Packet::NetworkSettings(p) => println!(
                    "  compression threshold={} algorithm={}",
                    p.compression_threshold, p.compression_algorithm
                ),
                Packet::ResourcePacksInfo => client.accept_resource_packs()?,
                Packet::StartGame(_) => client.request_chunks(8)?,
                _ => {}
            }
        }
    }
    println!("logged in: {}", client.is_logged_in());
    Ok(())
}
