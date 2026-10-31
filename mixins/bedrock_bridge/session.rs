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
    /// Last position sent to the server; rotation-only updates reuse it, so a
    /// MovePlayerRot never looks like a teleport to (0,0,0).
    position: Mutex<[f32; 3]>,
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
        position: Mutex::new([0.0; 3]),
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
    let auth_info = bedrock_core::auth::auth_info(&login.multiplayer_token, 0);
    if let Err(e) = client.begin_login(login.identity, login.handshake, auth_info, login.skin_json) {
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

    let blocks = translate::Blocks::new(&resources);
    if blocks.is_none() {
        log::warn!("bedrock: no block registry at {}; chunks will be skipped", resources.display());
    }

    let mut ctx = translate::Context {
        builtin: &builtin,
        mappings: &mappings,
        blocks,
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
    use horizon_protocol::chunk::{ChunkData, LightData, Paletted, Section, Strategy};
    use horizon_protocol::packets::play;
    use horizon_protocol::registry::{Builtin, DimensionType, Dynamic};
    use horizon_world::nbt::{JavaString, Tag as JTag};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    pub struct Context<'a> {
        pub builtin: &'a Builtin,
        pub mappings: &'a bedrock_core::mapping::Mappings,
        /// Block-state resolution; absent if the game's registry could not be
        /// read, in which case chunks are skipped rather than sent as air.
        pub blocks: Option<Blocks>,
        pub runtime_id: u64,
        pub unique_id: i64,
    }

    /// Resolves Bedrock runtime ids to the game's block-state ids.
    ///
    /// The Bedrock side is the palette bundled with the engine; the Java side is
    /// the game's own block registry, read from the same prepared resources the
    /// session started with. Results are cached, since chunks repeat blocks
    /// heavily and a full column is 24 × 4096 lookups.
    pub struct Blocks {
        registry: horizon_world::registry::BlockRegistry,
        table: bedrock_core::blockstates::BlockStates,
        cache: std::collections::HashMap<i32, u32>,
        pub air: u32,
    }

    impl Blocks {
        pub fn new(resources: &std::path::Path) -> Option<Self> {
            let file = std::fs::File::open(resources.join("facts.json")).ok()?;
            let registry = horizon_world::registry::BlockRegistry::from_facts_json(file).ok()?;
            let air = registry.default_state("minecraft:air")?.0;
            Some(Self {
                registry,
                table: bedrock_core::blockstates::BlockStates::bundled(),
                cache: std::collections::HashMap::new(),
                air,
            })
        }

        /// The game's state id for a Bedrock runtime id.
        pub fn java(&mut self, runtime_id: i32) -> u32 {
            if let Some(&id) = self.cache.get(&runtime_id) {
                return id;
            }
            let id = self.resolve(runtime_id);
            self.cache.insert(runtime_id, id);
            id
        }

        fn resolve(&self, runtime_id: i32) -> u32 {
            let Some((name, states)) = self.table.get(runtime_id) else {
                return self.air;
            };
            let tag = bedrock_core::blockstates::states_to_tag(states);
            let java = bedrock_core::blockmap::to_java(&name, tag.as_ref());
            let Some(default) = self.registry.default_state(&java.name) else {
                return self.air;
            };
            // Apply each candidate only where the block declares that property
            // and the value is valid, so one wrong property cannot void the
            // rest of them.
            let mut current = default;
            for (key, value) in &java.properties {
                let Some(known) = self.registry.state(current) else {
                    break;
                };
                if !known.properties.contains_key(key) {
                    continue;
                }
                let mut candidate = known.clone();
                candidate.properties.insert(key.clone(), value.clone());
                if let Some(next) = self.registry.id(&candidate) {
                    current = next;
                }
            }
            current.0
        }
    }

    /// A little-endian bit set, as the packet's `BitSet` codec writes it.
    fn bitset(bits: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; bits.div_ceil(8)];
        for i in 0..bits {
            bytes[i / 8] |= 1 << (i % 8);
        }
        while bytes.last() == Some(&0) {
            bytes.pop();
        }
        bytes
    }

    /// One Bedrock chunk column as the Java client wants it: every section of
    /// the dimension in order, its heightmaps, and full sky light.
    ///
    /// Bedrock sends only the sections it has, at their own indices, and no
    /// light, so the column is padded with air and lit uniformly. Real light
    /// needs the server's light data, which NetherNet does not forward.
    fn column(
        ctx: &mut Context,
        chunk: &bedrock_core::protocol::packets::LevelChunk,
    ) -> Option<play::LevelChunkWithLight> {
        let decoded = bedrock_core::chunk::decode_sections(chunk).ok()?;
        if decoded.is_empty() {
            return None;
        }
        let blocks = ctx.blocks.as_mut()?;
        let dimension = DimensionType::vanilla("minecraft:overworld");
        let count = dimension.sections();
        let mut filled: Vec<Option<Section>> = (0..count).map(|_| None).collect();
        let mut heights = [0i32; 256];
        for section in &decoded {
            let base = i32::from(section.y) * 16;
            let Some(slot) = usize::try_from((base - dimension.min_y) / 16)
                .ok()
                .filter(|i| *i < count)
            else {
                continue;
            };
            let mut values = [0i32; 4096];
            let mut non_empty = 0i16;
            for (i, runtime_id) in section.blocks.iter().enumerate() {
                let id = blocks.java(*runtime_id as i32);
                values[i] = id as i32;
                if id == blocks.air {
                    continue;
                }
                non_empty += 1;
                // Column order matches `chunk::java_index`.
                let column = ((i >> 4) & 15) * 16 + (i & 15);
                let height = base + (i >> 8) as i32 - dimension.min_y + 1;
                if height > heights[column] {
                    heights[column] = height;
                }
            }
            filled[slot] = Some(Section {
                non_empty,
                fluids: 0,
                states: Paletted::from_values(&values, Strategy::Blocks, 16),
                biomes: Paletted::from_values(&[1i32; 64], Strategy::Biomes, 7),
            });
        }
        let air = blocks.air;
        let air_section = move || Section {
            non_empty: 0,
            fluids: 0,
            states: Paletted::from_values(&[air as i32; 4096], Strategy::Blocks, 16),
            biomes: Paletted::from_values(&[1i32; 64], Strategy::Biomes, 7),
        };
        let sections: Vec<Section> = filled
            .into_iter()
            .map(|section| section.unwrap_or_else(air_section))
            .collect();

        // WORLD_SURFACE (1) and MOTION_BLOCKING (4); this decoder does not
        // distinguish them, since it sees no fluids or leaf shapes.
        let bits = (dimension.height as u32 + 1).ilog2() as u8 + 1;
        let packed = |heights: &[i32; 256]| -> Vec<i64> {
            let per = 64 / bits as usize;
            heights
                .chunks(per)
                .map(|chunk| {
                    chunk
                        .iter()
                        .enumerate()
                        .fold(0u64, |word, (k, v)| {
                            word | ((*v as u64) << (k * bits as usize))
                        }) as i64
                })
                .collect()
        };
        let heightmaps = vec![(1, packed(&heights)), (4, packed(&heights))];

        // Light sections run from one below the lowest block section upwards.
        let light_sections = count + 2;
        let light = LightData {
            sky_mask: bitset(light_sections),
            block_mask: Vec::new(),
            empty_sky_mask: Vec::new(),
            // Block light everywhere is zero, which the empty mask conveys
            // without sending 2048 bytes per section.
            empty_block_mask: bitset(light_sections),
            sky: vec![vec![0xf0u8; 2048]; light_sections],
            block: Vec::new(),
        };

        Some(play::LevelChunkWithLight {
            x: chunk.chunk_x,
            z: chunk.chunk_z,
            chunk: ChunkData {
                heightmaps,
                buffer: ChunkData::from_sections(&sections),
                block_entities: Vec::new(),
            },
            light,
        })
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
                        play::SetChunkCacheCenter {
                            x: g.position[0] as i32 >> 4,
                            z: g.position[2] as i32 >> 4,
                        },
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
            bp::Packet::SetEntityMotion {
                runtime_id,
                velocity,
            } => vec![Event::Packet(Box::new(play::Clientbound::SetEntityMotion(
                play::SetEntityMotion {
                    id: *runtime_id as i32,
                    velocity: [velocity[0] as f64, velocity[1] as f64, velocity[2] as f64],
                },
            )))],
            bp::Packet::Animate { action, runtime_id } => vec![Event::Packet(Box::new(
                play::Clientbound::Animate(play::Animate {
                    id: *runtime_id as i32,
                    action: *action as u8,
                }),
            ))],
            bp::Packet::SetSpawnPosition { position } => vec![Event::Packet(Box::new(
                play::Clientbound::SetDefaultSpawnPosition(play::SetDefaultSpawnPosition {
                    pos: play::GlobalPos {
                        dimension: "minecraft:overworld".into(),
                        pos: *position,
                    },
                    yaw: 0.0,
                    pitch: 0.0,
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
            bp::Packet::LevelChunk(c) => match column(ctx, c) {
                Some(chunk) => vec![Event::Packet(Box::new(
                    play::Clientbound::LevelChunkWithLight(chunk),
                ))],
                None => Vec::new(),
            },
            // Block updates, block entities, inventories and the rest still
            // need their own translation; see the README.
            _ => Vec::new(),
        }
    }

    /// Java serverbound → Bedrock. Only the movement/keepalive core for now.
    pub fn serverbound(bridge: &Bridge, packet: &play::Serverbound) -> Vec<bp::Out> {
        let runtime_id = bridge.runtime_id.load(Ordering::Relaxed) as u64;
        match packet {
            play::Serverbound::MovePlayerPos(p) => {
                let pos = [p.x as f32, p.y as f32, p.z as f32];
                *bridge.position.lock().unwrap() = pos;
                vec![bp::Out::MovePlayer {
                    runtime_id,
                    position: pos,
                    pitch: 0.0,
                    yaw: 0.0,
                    head_yaw: 0.0,
                    mode: 0,
                    on_ground: p.flags & 1 != 0,
                }]
            }
            play::Serverbound::MovePlayerPosRot(p) => {
                let pos = [p.x as f32, p.y as f32, p.z as f32];
                *bridge.position.lock().unwrap() = pos;
                vec![bp::Out::MovePlayer {
                    runtime_id,
                    position: pos,
                    pitch: p.pitch,
                    yaw: p.yaw,
                    head_yaw: p.yaw,
                    mode: 0,
                    on_ground: p.flags & 1 != 0,
                }]
            }
            play::Serverbound::MovePlayerRot(p) => {
                // Rotation only: keep the last position so it is not a teleport.
                let pos = *bridge.position.lock().unwrap();
                vec![bp::Out::MovePlayer {
                    runtime_id,
                    position: pos,
                    pitch: p.pitch,
                    yaw: p.yaw,
                    head_yaw: p.yaw,
                    mode: 0,
                    on_ground: p.flags & 1 != 0,
                }]
            }
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
        pub multiplayer_token: String,
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
        // The multiplayer identity token, signed over the chain's own key.
        use bedrock_core::nethernet::auth_service;
        let xbl3 = core_auth::playfab_xbl3(&msa.access_token).map_err(|e| e.to_string())?;
        let ticket = auth_service::playfab_login(&xbl3).map_err(|e| e.to_string())?;
        let auth_header = auth_service::session_start(&ticket, bedrock_core::MINECRAFT_VERSION)
            .map_err(|e| e.to_string())?;
        let multiplayer_token = auth_service::multiplayer_token(&auth_header, &data.identity.public_der())
            .map_err(|e| e.to_string())?;
        Ok(SignedIn {
            identity: data.identity,
            handshake: data.handshake,
            chain_json: data.chain_json,
            skin_json: data.skin_json,
            multiplayer_token,
        })
    }
}
