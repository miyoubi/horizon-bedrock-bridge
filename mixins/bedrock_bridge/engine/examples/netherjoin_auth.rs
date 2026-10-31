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

/// Where this diagnostic example writes captures: `NETHER_CAPTURE_DIR`, else
/// the OS temp directory.
fn capture_dir() -> std::path::PathBuf {
    std::env::var_os("NETHER_CAPTURE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
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
    let _ = std::fs::write(capture_dir().join("our-token.txt"), &token);
    let _ = std::fs::write(capture_dir().join("our-skin.jwt"), &data.skin_json);
    let identity = ClientIdentity::external(nether_key, token);
    let Some(_server_id) = discover(&discovery_addr) else {
        return Err("no discovery reply".into());
    };
    // The guide: the POST path carries the *client's* NetworkID.
    let client_id = std::env::var("NETID").unwrap_or_else(|_| "1".to_string());
    let mut client = BedrockClient::connect_nethernet(&base, &client_id, &identity)?;
    println!("netherNet connected, data channel open");
    let skin_json = if std::env::var_os("NETHER_BPSKIN").is_some() {
        let payload = std::fs::read_to_string(capture_dir().join("bp-skin-payload.json"))?;
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

    // `BEDROCK_BLOCKS_NBT` names the block table; default to the reference dump.
    let blocks_path = std::env::var_os("BEDROCK_BLOCKS_NBT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new("references/mappings-master/blocks.nbt").to_path_buf()
        });
    let mappings = bedrock_core::mapping::BedrockBlockMappings::load(&blocks_path).ok();
    println!("block mappings: {:?}", mappings.as_ref().map(|m| m.len()));
    let states_path = std::env::var_os("BEDROCK_BLOCKSTATES")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new("mixins/bedrock_bridge/data/blockstates-1.26.60.txt.gz")
                .to_path_buf()
        });
    let table = bedrock_core::blockstates::BlockStates::load_gz(&states_path).ok();
    println!("block states: {:?}", table.as_ref().map(|t| t.len()));
    let mut chunk_seen = 0usize;
    let mut pub_seen = 0usize;

    // How long to hold the session; the ICE failure that kills an in-game
    // session shows up past half a minute, so it is worth waiting longer.
    let seconds = std::env::var("NETHER_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let deadline = Instant::now() + Duration::from_secs(seconds);
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
                    let cx = (g.position[0] / 16.0).floor() as i32;
                    let cz = (g.position[2] / 16.0).floor() as i32;
                    let offsets: Vec<[i8; 3]> = (-2..=4).map(|sy| [0i8, sy, 0]).collect();
                    let _ = client.send(&[bedrock_core::protocol::Out::SubChunkRequest {
                        dimension: 0,
                        center: [cx, 4, cz],
                        offsets,
                    }]);
                    client.send(&[bedrock_core::protocol::Out::ClientCacheStatus {
                        supported: false,
                    }])?;
                    client.request_chunks(8)?;
                }
                Packet::SubChunk(sc) => {
                    for e in &sc.entries {
                        if e.result != 1 || e.payload.is_empty() {
                            continue;
                        }
                        let _ = std::fs::write(
                            capture_dir().join(format!(
                                "sub-{}-{}-{}-{}-{}-{}.bin",
                                sc.position[0],
                                sc.position[1],
                                sc.position[2],
                                e.offset[0],
                                e.offset[1],
                                e.offset[2]
                            )),
                            &e.payload,
                        );
                        let mut r = bedrock_core::stream::Reader::new(&e.payload[1..]);
                        match bedrock_core::subchunk::SubChunk::read(&mut r, e.payload[0]) {
                            Ok(sub) => {
                                if let Some(s0) = sub.storages.first() {
                                    println!(
                                        "DBG bits={} words={} idx0={} idx1240={} idx2480={}",
                                        s0.storage.bits(),
                                        s0.storage.words().len(),
                                        s0.storage.get(0),
                                        s0.storage.get(1240),
                                        s0.storage.get(2480)
                                    );
                                    if let bedrock_core::subchunk::Palette::Runtime(ids) =
                                        &s0.palette
                                    {
                                        println!("DBG palette={:?}", &ids[..ids.len().min(12)]);
                                        if let Some(t) = table.as_ref() {
                                            for id in ids.iter().take(8) {
                                                let (n, st) = t.get(*id).unwrap_or_default();
                                                println!("DBG id={id} name={n} states={st}");
                                            }
                                        }
                                    }
                                }
                                let mut non_air = 0usize;
                                let mut sample = 0u32;
                                let mut rid_nonzero = 0usize;
                                let mut sample_rid = -1i32;
                                if let Some(t) = table.as_ref() {
                                    for y in 0..16 {
                                        for z in 0..16 {
                                            for x in 0..16 {
                                                let rid = sub.top_runtime_id(x, y, z).unwrap_or(0);
                                                if rid != 0 {
                                                    rid_nonzero += 1;
                                                    sample_rid = rid;
                                                }
                                                if t.get(rid)
                                                    .map(|(n, _)| n != "minecraft:air")
                                                    .unwrap_or(false)
                                                {
                                                    non_air += 1;
                                                    sample = rid as u32;
                                                }
                                            }
                                        }
                                    }
                                }
                                println!(
                                    "SUBCHUNK hdr={:02x?}",
                                    &e.payload[..e.payload.len().min(16)]
                                );
                                println!(
                                    "SUBCHUNK pos={:?} off={:?} bytes={} non_air={} rid_nonzero={} sample_rid={} name={:?} sample_java={}",
                                    sc.position,
                                    e.offset,
                                    e.payload.len(),
                                    non_air,
                                    rid_nonzero,
                                    sample_rid,
                                    table.as_ref().and_then(|t| t.get(sample_rid).map(|(n, _)| n)),
                                    sample
                                );
                            }
                            Err(err) => println!("SUBCHUNK decode err: {err}"),
                        }
                    }
                }
                // Ask for the centre column's sections once the server has
                // published the chunk area (a few times, then stop).
                Packet::NetworkChunkPublisherUpdate { position, .. } if pub_seen < 3 => {
                    pub_seen += 1;
                    let cx = position[0] >> 4;
                    let cz = position[2] >> 4;
                    let offsets: Vec<[i8; 3]> = (-2..=4).map(|sy| [0i8, sy, 0]).collect();
                    let _ = client.send(&[bedrock_core::protocol::Out::SubChunkRequest {
                        dimension: 0,
                        center: [cx, 4, cz],
                        offsets,
                    }]);
                }
                Packet::NetworkChunkPublisherUpdate { .. } => {}
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
                        if let Some(t) = table.as_ref() {
                            match bedrock_core::chunk::decode_sections(&c) {
                                Ok(sections) => {
                                    let total: usize =
                                        sections.iter().map(|s| s.blocks.len()).sum();
                                    let non_air = sections
                                        .iter()
                                        .flat_map(|s| s.blocks.iter())
                                        .filter(|b| {
                                            t.get(**b as i32)
                                                .map(|(n, _)| n != "minecraft:air")
                                                .unwrap_or(false)
                                        })
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
                                Err(e) => {
                                    // Keep the payload: a column that fails to
                                    // decode is never sent to the client, and the
                                    // player's own column is what ends the
                                    // loading screen.
                                    let path = capture_dir().join(format!(
                                        "chunk-fail-{}-{}.bin",
                                        c.chunk_x, c.chunk_z
                                    ));
                                    let _ = std::fs::write(&path, &c.data);
                                    println!("CHUNK decode error: {e} (payload at {path:?})");
                                }
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
