//! End-to-end NetherNet login.
//!
//!   cargo run -p bedrock-core --features nethernet-webrtc --example netherjoin_auth -- \
//!     http://127.0.0.1:19132 127.0.0.1:7551 127.0.0.1:19132
//!
//! Signs in (device code, cached), mints the multiplayer identity token, builds
//! the Bedrock chain from the same Microsoft account, joins over NetherNet
//! (WebRTC) and runs the login handshake to `StartGame`.

use base64::Engine as _;
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

    let nether_key = IdentityKey::generate()?;
    let data = login_with_identity(
        &msa.access_token,
        &server_address,
        bedrock_core::MINECRAFT_VERSION,
        nether_key.try_clone()?,
    )?;
    println!("built chain for {}", data.display_name);

    let (token, auth_info) = if std::env::var_os("NETHER_SELFSIGNED").is_some() {
        // bedrock-protocol's offline login (the guide says it joins BDS):
        // AuthenticationType 2, empty-chain Certificate, self-signed token.
        let t = bedrock_core::auth::self_signed_token(
            &data.identity,
            &data.xuid,
            &data.display_name,
            &bedrock_core::auth::uuid_string(data.uuid),
        )?;
        let cert = serde_json::json!({ "chain": [""] }).to_string();
        let ai = serde_json::json!({
            "AuthenticationType": 2,
            "Certificate": cert,
            "Token": t,
        })
        .to_string();
        (t, ai)
    } else {
        let xbl3 = playfab_xbl3(&msa.access_token)?;
        let ticket = auth_service::playfab_login(&xbl3)?;
        let auth_header = auth_service::session_start(&ticket, bedrock_core::MINECRAFT_VERSION)?;
        let t = auth_service::multiplayer_token(&auth_header, &data.identity.public_der())?;
        let ai = bedrock_core::auth::auth_info(&t, 0);
        (t, ai)
    };
    println!("AUTHINFO {}", &auth_info[..auth_info.len().min(160)]);
    let _ = std::fs::write("/mnt/our-token.txt", &token);
    let _ = std::fs::write("/mnt/our-skin.jwt", &data.skin_json);
    let identity = ClientIdentity::external(nether_key, token);
    let Some(_server_id) = discover(&discovery_addr) else {
        return Err("no discovery reply".into());
    };
    // The guide: the POST path carries the *client's* NetworkID.
    let client_id = std::env::var("NETID").unwrap_or_else(|_| "1".to_string());
    let mut client = BedrockClient::connect_nethernet(&base, &client_id, &identity)?;
    println!("netherNet connected, data channel open");
    let skin_json = if std::env::var_os("NETHER_BPSKIN").is_some() {
        let payload = std::fs::read_to_string("/mnt/bp-skin-payload.json")?;
        let header = serde_json::json!({
            "alg": "ES384",
            "x5u": bedrock_core::crypto::B64.encode(data.identity.public_der()),
        })
        .to_string();
        data.identity.sign_jwt(&header, &payload)?
    } else {
        data.skin_json.clone()
    };
    client.begin_login(data.identity, data.handshake, auth_info, skin_json)?;

    let mappings = bedrock_core::mapping::BedrockBlockMappings::load(std::path::Path::new(
        "/mnt/project/references/mappings-master/blocks.nbt",
    ))
    .ok();
    println!("block mappings: {:?}", mappings.as_ref().map(|m| m.len()));
    let mut game: Option<bedrock_core::protocol::packets::StartGame> = None;
    let mut chunk_seen = 0usize;

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        for packet in client.poll()? {
            let debug = format!("{packet:?}");
            println!("<- {}", &debug[..debug.len().min(1200)]);
            match packet {
                Packet::NetworkSettings(p) => println!(
                    "  compression threshold={} algorithm={}",
                    p.compression_threshold, p.compression_algorithm
                ),
                Packet::ResourcePacksInfo => {
                    client.send(&[bedrock_core::protocol::Out::ResourcePackClientResponse {
                        status: 2, // downloadingfinished
                        pack_ids: Vec::new(),
                    }])?
                }
                Packet::ResourcePackStack => {
                    client.send(&[bedrock_core::protocol::Out::ResourcePackClientResponse {
                        status: 3, // resourcepackstackfinished
                        pack_ids: Vec::new(),
                    }])?
                }
                Packet::StartGame(g) => {
                    game = Some(g);
                    client.send(&[bedrock_core::protocol::Out::ClientCacheStatus {
                        supported: false,
                    }])?;
                    client.request_chunks(8)?;
                }
                Packet::LevelChunk(c) => {
                    chunk_seen += 1;
                    if chunk_seen <= 3 {
                        println!(
                            "RAW CHUNK({},{}) dim={} sub_chunks={} data_len={} first={:02x?}",
                            c.chunk_x,
                            c.chunk_z,
                            c.dimension,
                            c.sub_chunk_count,
                            c.data.len(),
                            &c.data[..c.data.len().min(24)]
                        );
                    }
                    if chunk_seen <= 3 {
                        if let (Some(g), Some(m)) = (game.as_ref(), mappings.as_ref()) {
                            match bedrock_core::chunk::decode_sections(g, &c, m) {
                                Ok(sections) => {
                                    let total: usize =
                                        sections.iter().map(|s| s.blocks.len()).sum();
                                    let non_air = sections
                                        .iter()
                                        .flat_map(|s| s.blocks.iter())
                                        .filter(|b| **b != 0)
                                        .count();
                                    println!(
                                        "CHUNK({},{}) sections={} blocks={} non_air={}",
                                        c.chunk_x,
                                        c.chunk_z,
                                        sections.len(),
                                        total,
                                        non_air
                                    );
                                }
                                Err(e) => println!("CHUNK decode error: {e}"),
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    println!("logged in: {}", client.is_logged_in());
    Ok(())
}
