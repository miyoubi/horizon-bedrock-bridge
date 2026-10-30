// Injected as `crate::net::bedrock` inside `crates/horizon-client/src/net.rs`.
//
// This is the Horizon-side of the bridge. It is a child module of `net`, so it
// can build a `Session` (whose fields are private) and reach the game thread
// through the same `Event` channel a real Java connection uses. For a Bedrock
// address it spawns a thread that runs the `bedrock_core` client and translates
// its packets into the Java clientbound packets Horizon already handles.
//
// The Bedrock stack lives in the bundle crate `bedrock_core`; this file is glue.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};

use horizon_protocol::packets::play;
use horizon_protocol::registry::Builtin;

use super::{Cookies, Event, Session};

/// Addresses the bridge handles: `bedrock:host[:port]` (default port 19132).
const PREFIX: &str = "bedrock:";
const DEFAULT_PORT: u16 = 19132;

/// Whether `address` names a Bedrock server rather than a Java one.
pub fn is_bedrock(address: &str) -> bool {
    address.starts_with(PREFIX)
}

fn split(address: &str) -> (String, u16) {
    let rest = address.strip_prefix(PREFIX).unwrap_or(address);
    match rest.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => {
            (host.to_owned(), port.parse().unwrap())
        }
        _ => (rest.to_owned(), DEFAULT_PORT),
    }
}

/// Shared state for a live Bedrock session, reachable from the game thread.
struct Bridge {
    /// Outbound Java packets translated to Bedrock on the game thread.
    outbound: mpsc::Sender<Vec<bedrock_core::protocol::Out>>,
    runtime_id: AtomicI64,
    /// Set when the game drops the session, so the thread ends.
    shutdown: AtomicBool,
}

static BRIDGES: OnceLock<Mutex<HashMap<String, Arc<Bridge>>>> = OnceLock::new();

fn bridges() -> &'static Mutex<HashMap<String, Arc<Bridge>>> {
    BRIDGES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A `Session` backed by a Bedrock connection, or `None` for a Java address.
pub fn try_connect(
    address: &str,
    name: &str,
    view_distance: i8,
    resources: std::path::PathBuf,
) -> Option<Session> {
    if !is_bedrock(address) {
        return None;
    }
    let (send, events) = mpsc::channel();
    let (out_tx, out_rx) = mpsc::channel::<Vec<bedrock_core::protocol::Out>>();
    let bridge = Arc::new(Bridge {
        outbound: out_tx,
        runtime_id: AtomicI64::new(0),
        shutdown: AtomicBool::new(false),
    });
    bridges()
        .lock()
        .unwrap()
        .insert(address.to_owned(), bridge.clone());

    let addr = address.to_owned();
    let name = name.to_owned();
    let thread_name = name.clone();
    let thread = std::thread::Builder::new()
        .name("horizon-bedrock".into())
        .spawn(move || {
            run(&addr, &thread_name, view_distance, &resources, &send, out_rx, &bridge);
        })
        .ok();

    // Build a Session exactly as `start` does, with no Java writer.
    let writer = Arc::new(Mutex::new(None));
    let configuring = Arc::new(AtomicBool::new(false));
    let counters = Arc::new(Mutex::new(None));
    let cookies: Cookies = Default::default();
    Some(Session {
        events,
        writer,
        configuring,
        thread,
        address: address.to_owned(),
        name,
        counters,
        cookies,
        debug: std::env::var_os("HORIZON_NET_DEBUG").is_some(),
    })
}

/// Translate and queue an outgoing Java packet. Returns whether the session is
/// the bridge's (so the hook can cancel the normal Java encode).
pub fn send(session: &Session, packet: &play::Serverbound) -> bool {
    let map = bridges().lock().unwrap();
    let Some(bridge) = map.get(&session.address) else {
        return false;
    };
    let out = translate::serverbound(bridge, packet);
    if !out.is_empty() {
        let _ = bridge.outbound.send(out);
    }
    true
}

/// The session is going away: tell the thread to stop.
pub fn on_disconnect(address: &str) {
    if let Some(bridge) = bridges().lock().unwrap().remove(address) {
        bridge.shutdown.store(true, Ordering::Relaxed);
    }
}

fn run(
    address: &str,
    name: &str,
    view_distance: i8,
    resources: &std::path::Path,
    events: &mpsc::Sender<Event>,
    outbound: mpsc::Receiver<Vec<bedrock_core::protocol::Out>>,
    bridge: &Arc<Bridge>,
) {
    let status = |text: String| {
        let _ = events.send(Event::Status(text));
    };
    let (host, port) = split(address);
    status(format!("Connecting to Bedrock server {host}:{port}"));

    let mut client = match bedrock_core::client::BedrockClient::connect(&host, port, 1400) {
        Ok(c) => c,
        Err(e) => {
            let _ = events.send(Event::Disconnected {
                reason: format!("Bedrock connection failed: {e}"),
                from_server: false,
            });
            return;
        }
    };

    // Xbox sign-in (device code), then the Minecraft JWT chain.
    let login = match auth::sign_in(&host, port) {
        Ok(l) => l,
        Err(e) => {
            let _ = events.send(Event::Disconnected {
                reason: format!("Xbox sign-in failed: {e}"),
                from_server: false,
            });
            return;
        }
    };
    if let Err(e) = client.begin_login(
        login.identity,
        login.handshake,
        login.chain_json,
        login.skin_json,
    ) {
        let _ = events.send(Event::Disconnected {
            reason: format!("Bedrock login failed: {e}"),
            from_server: false,
        });
        return;
    }

    // The builtin registries give Java entity-type ids for the Bedrock
    // identifiers (the registers report beside prepared resources).
    let builtin = Builtin::find(resources)
        .and_then(|p| Builtin::load(&p).ok())
        .unwrap_or_default();
    let mappings = bedrock_core::mapping::Mappings::defaults();

    let mut ctx = translate::Context {
        builtin: &builtin,
        mappings: &mappings,
        runtime_id: 0,
        unique_id: 0,
    };

    loop {
        // Outbound Java → Bedrock. The channel closing (or the game signalling
        // shutdown) ends the thread.
        let mut alive = true;
        loop {
            match outbound.try_recv() {
                Ok(batch) => {
                    if let Err(e) = client.send(&batch) {
                        log::warn!("bedrock: send failed: {e}");
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    alive = false;
                    break;
                }
            }
        }
        if !alive || bridge.shutdown.load(Ordering::Relaxed) {
            break;
        }
        // Bedrock → Java.
        match client.poll() {
            Ok(packets) => {
                for packet in packets {
                    for event in translate::clientbound(&mut ctx, &packet) {
                        let _ = events.send(event);
                    }
                    // React to the resource-pack phase automatically.
                    if let bedrock_core::protocol::Packet::PlayStatus(s) = &packet {
                        if *s == 0 {
                            let _ = client.accept_resource_packs();
                        }
                    }
                    if let bedrock_core::protocol::Packet::StartGame(g) = &packet {
                        ctx.runtime_id = g.entity_runtime_id;
                        ctx.unique_id = g.entity_unique_id;
                        bridge.runtime_id.store(g.entity_runtime_id as i64, Ordering::Relaxed);
                        let _ = client.request_chunks(i32::from(view_distance));
                    }
                }
            }
            Err(e) => {
                let _ = events.send(Event::Disconnected {
                    reason: format!("Bedrock read failed: {e}"),
                    from_server: false,
                });
                return;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    let _ = events.send(Event::Disconnected {
        reason: "Bedrock session closed".into(),
        from_server: false,
    });
    let _ = name;
}

// -------------------------------------------------------------------------
// Translation: Bedrock packets → Horizon's Java clientbound packets.
// -------------------------------------------------------------------------
mod translate {
    use super::{Bridge, Event};
    use bedrock_core::protocol as bp;
    use horizon_protocol::packets::play;
    use horizon_protocol::registry::{Builtin, Dynamic};
    use horizon_world::nbt::{JavaString, Tag as JTag};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    pub struct Context<'a> {
        pub builtin: &'a Builtin,
        pub mappings: &'a bedrock_core::mapping::Mappings,
        pub runtime_id: u64,
        pub unique_id: i64,
    }

    fn entity_type_id(ctx: &Context, java_name: &str) -> i32 {
        ctx.builtin
            .get("minecraft:entity_type")
            .and_then(|ids| ids.id(java_name))
            .unwrap_or(0)
    }

    /// Bedrock network NBT → Java NBT (used once block entities are translated).
    #[allow(dead_code)]
    pub fn nbt(tag: &bedrock_core::nbt::Tag) -> JTag {
        use bedrock_core::nbt::Tag as B;
        match tag {
            B::End => JTag::End,
            B::Byte(v) => JTag::Byte(*v),
            B::Short(v) => JTag::Short(*v),
            B::Int(v) => JTag::Int(*v),
            B::Long(v) => JTag::Long(*v),
            B::Float(v) => JTag::Float(v.to_bits()),
            B::Double(v) => JTag::Double(v.to_bits()),
            B::ByteArray(v) => JTag::ByteArray(v.clone()),
            B::String(s) => JTag::String(JavaString::from(s.as_str())),
            B::List(items) => JTag::List(items.iter().map(nbt).collect()),
            B::Compound(fields) => JTag::Compound(
                fields
                    .iter()
                    .map(|(k, v)| (JavaString::from(k.as_str()), nbt(v)))
                    .collect(),
            ),
            B::IntArray(v) => JTag::IntArray(v.clone()),
            B::LongArray(v) => JTag::LongArray(v.clone()),
        }
    }

    fn text_component(message: &str) -> JTag {
        JTag::compound([("text", JTag::String(JavaString::from(message)))])
    }

    pub fn clientbound(ctx: &mut Context, packet: &bp::Packet) -> Vec<Event> {
        match packet {
            bp::Packet::StartGame(g) => {
                let spawn = play::SpawnInfo {
                    dimension_type: g.dimension,
                    dimension: "minecraft:overworld".into(),
                    seed: g.seed,
                    game_type: g.player_gamemode,
                    previous_game_type: None,
                    debug: false,
                    flat: false,
                    last_death: None,
                    portal_cooldown: 0,
                    sea_level: 63,
                };
                vec![
                    Event::Status("Bedrock login accepted".into()),
                    Event::Joined {
                        registries: Arc::new(Dynamic::default()),
                        uuid: 0,
                        name: String::new(),
                    },
                    Event::Packet(Box::new(play::Clientbound::Login(play::Login {
                        player_id: g.entity_runtime_id as i32,
                        hardcore: false,
                        levels: vec!["minecraft:overworld".into()],
                        max_players: 100,
                        chunk_radius: 8,
                        simulation_distance: 8,
                        reduced_debug_info: false,
                        show_death_screen: true,
                        limited_crafting: false,
                        spawn,
                        online_mode: true,
                        enforces_secure_chat: false,
                    }))),
                    Event::Packet(Box::new(play::Clientbound::PlayerPosition(
                        play::PlayerPosition {
                            teleport: 0,
                            position: [g.position[0] as f64, g.position[1] as f64, g.position[2] as f64],
                            delta: [0.0; 3],
                            yaw: g.yaw,
                            pitch: g.pitch,
                            relatives: 0,
                        },
                    ))),
                    Event::Packet(Box::new(play::Clientbound::SetChunkCacheCenter(
                        play::SetChunkCacheCenter { x: 0, z: 0 },
                    ))),
                ]
            }
            bp::Packet::Text(t) => {
                let message = if t.parameters.is_empty() {
                    t.message.clone()
                } else {
                    t.parameters.join(" ")
                };
                vec![Event::Packet(Box::new(play::Clientbound::SystemChat(
                    play::SystemChat {
                        content: text_component(message.trim()),
                        overlay: false,
                    },
                )))]
            }
            bp::Packet::SetTime(time) => vec![Event::Packet(Box::new(
                play::Clientbound::SetTime(play::SetTime {
                    game_time: i64::from(*time),
                    clocks: Vec::new(),
                }),
            ))],
            bp::Packet::SetDifficulty(difficulty) => vec![Event::Packet(Box::new(
                play::Clientbound::ChangeDifficulty(play::ChangeDifficulty {
                    difficulty: *difficulty,
                    locked: true,
                }),
            ))],
            bp::Packet::SetHealth(health) => vec![Event::Packet(Box::new(
                play::Clientbound::SetHealth(play::SetHealth {
                    health: *health,
                    food: 20,
                    saturation: 5.0,
                }),
            ))],
            bp::Packet::AddEntity(entity) => {
                let java_name = ctx.mappings.java_entity(&entity.identifier);
                let kind = entity_type_id(ctx, &java_name);
                vec![Event::Packet(Box::new(play::Clientbound::AddEntity(
                    play::AddEntity {
                        id: entity.runtime_id as i32,
                        uuid: entity.unique_id as u128,
                        kind,
                        position: [
                            entity.position[0] as f64,
                            entity.position[1] as f64,
                            entity.position[2] as f64,
                        ],
                        velocity: [
                            entity.velocity[0] as f64,
                            entity.velocity[1] as f64,
                            entity.velocity[2] as f64,
                        ],
                        pitch: (entity.pitch / (360.0 / 256.0)) as i8,
                        yaw: (entity.yaw / (360.0 / 256.0)) as i8,
                        head_yaw: (entity.head_yaw / (360.0 / 256.0)) as i8,
                        data: 0,
                    },
                )))]
            }
            bp::Packet::AddPlayer(player) => {
                let uuid = u128::from_be_bytes(player.uuid);
                vec![Event::Packet(Box::new(play::Clientbound::AddEntity(
                    play::AddEntity {
                        id: player.runtime_id as i32,
                        uuid,
                        kind: entity_type_id(ctx, "minecraft:player"),
                        position: [
                            player.position[0] as f64,
                            player.position[1] as f64,
                            player.position[2] as f64,
                        ],
                        velocity: [0.0; 3],
                        pitch: (player.pitch / (360.0 / 256.0)) as i8,
                        yaw: (player.yaw / (360.0 / 256.0)) as i8,
                        head_yaw: 0,
                        data: 0,
                    },
                )))]
            }
            bp::Packet::MovePlayer(m) => {
                if m.runtime_id as i64 == ctx.runtime_id as i64 {
                    vec![Event::Packet(Box::new(play::Clientbound::PlayerPosition(
                        play::PlayerPosition {
                            teleport: 0,
                            position: [m.position[0] as f64, m.position[1] as f64, m.position[2] as f64],
                            delta: [0.0; 3],
                            yaw: m.yaw,
                            pitch: m.pitch,
                            relatives: 0,
                        },
                    )))]
                } else {
                    vec![Event::Packet(Box::new(play::Clientbound::TeleportEntity(
                        play::TeleportEntity {
                            id: m.runtime_id as i32,
                            position: [m.position[0] as f64, m.position[1] as f64, m.position[2] as f64],
                            delta: [0.0; 3],
                            yaw: m.yaw,
                            pitch: m.pitch,
                            relatives: 0,
                            on_ground: m.on_ground,
                        },
                    )))]
                }
            }
            bp::Packet::MoveEntityAbsolute(m) => vec![Event::Packet(Box::new(
                play::Clientbound::TeleportEntity(play::TeleportEntity {
                    id: m.runtime_id as i32,
                    position: [m.position[0] as f64, m.position[1] as f64, m.position[2] as f64],
                    delta: [0.0; 3],
                    yaw: m.yaw,
                    pitch: m.pitch,
                    relatives: 0,
                    on_ground: m.on_ground,
                }),
            ))],
            bp::Packet::RemoveEntity(id) => vec![Event::Packet(Box::new(
                play::Clientbound::RemoveEntities(play::RemoveEntities {
                    ids: vec![*id as i32],
                }),
            ))],
            bp::Packet::Disconnect(d) => vec![Event::Disconnected {
                reason: if d.message.is_empty() {
                    format!("Bedrock disconnect reason {}", d.reason)
                } else {
                    d.message.clone()
                },
                from_server: true,
            }],
            // Chunks, block updates, inventories and the rest need the
            // Java↔Bedrock block-state resolver; see the README.
            _ => Vec::new(),
        }
    }

    /// Java serverbound → Bedrock. Only the movement/keepalive core for now.
    pub fn serverbound(bridge: &Bridge, packet: &play::Serverbound) -> Vec<bp::Out> {
        let runtime_id = bridge.runtime_id.load(Ordering::Relaxed) as u64;
        match packet {
            play::Serverbound::MovePlayerPos(p) => vec![bp::Out::MovePlayer {
                runtime_id,
                position: [p.x as f32, p.y as f32, p.z as f32],
                pitch: 0.0,
                yaw: 0.0,
                head_yaw: 0.0,
                mode: 0,
                on_ground: p.flags & 1 != 0,
            }],
            play::Serverbound::MovePlayerPosRot(p) => vec![bp::Out::MovePlayer {
                runtime_id,
                position: [p.x as f32, p.y as f32, p.z as f32],
                pitch: p.pitch,
                yaw: p.yaw,
                head_yaw: p.yaw,
                mode: 0,
                on_ground: p.flags & 1 != 0,
            }],
            play::Serverbound::Chat(c) => vec![bp::Out::Text {
                kind: 1,
                source: String::new(),
                message: c.message.clone(),
            }],
            play::Serverbound::ChatCommand(c) => vec![bp::Out::Text {
                kind: 1,
                source: String::new(),
                message: format!("/{}", c.command),
            }],
            _ => Vec::new(),
        }
    }
}

// The Xbox sign-in helper is kept beside the session glue so the game thread
// can show its device code as a status line.
mod auth {
    use bedrock_core::auth as core_auth;

    pub struct SignedIn {
        pub identity: bedrock_core::crypto::IdentityKey,
        pub handshake: bedrock_core::crypto::HandshakeKey,
        pub chain_json: String,
        pub skin_json: String,
    }

    fn token_path() -> std::path::PathBuf {
        std::env::temp_dir().join("horizon-bedrock-msa.json")
    }

    pub fn sign_in(host: &str, port: u16) -> Result<SignedIn, String> {
        let address = format!("{host}:{port}");
        // Reuse a refresh token when we have one.
        let cached = std::fs::read_to_string(token_path())
            .ok()
            .and_then(|t| serde_json::from_str::<String>(&t).ok());
        let msa = if let Some(refresh) = cached {
            core_auth::refresh(&refresh).map_err(|e| e.to_string())?
        } else {
            let code = core_auth::begin_device_code().map_err(|e| e.to_string())?;
            eprintln!(
                "[bedrock] sign in at {} with code {}",
                code.verification_uri, code.user_code
            );
            loop {
                match core_auth::poll_device_code(&code).map_err(|e| e.to_string())? {
                    Some(token) => break token,
                    None => std::thread::sleep(std::time::Duration::from_secs(code.interval)),
                }
            }
        };
        if let Some(refresh) = &msa.refresh_token {
            let _ = std::fs::write(token_path(), serde_json::to_string(refresh).unwrap_or_default());
        }
        let data = core_auth::login(&msa.access_token, &address, bedrock_core::MINECRAFT_VERSION)
            .map_err(|e| e.to_string())?;
        Ok(SignedIn {
            identity: data.identity,
            handshake: data.handshake,
            chain_json: data.chain_json,
            skin_json: data.skin_json,
        })
    }
}
