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

/// Addresses the bridge handles: `bedrock:host[:port]`, which is NetherNet —
/// the transport the Bedrock client itself uses, and what a Bedrock server
/// serves by default — or `bedrock+raknet:host[:port]` for a server started
/// with `transport=raknet`. Both default to port 19132.
const PREFIX: &str = "bedrock:";
const RAKNET_PREFIX: &str = "bedrock+raknet:";
const DEFAULT_PORT: u16 = 19132;

/// How many columns one publisher update asks for. Each is a Bedrock round trip
/// and the loop also pumps the WebRTC session, so it is spread out.
const REQUESTS_PER_TICK: usize = 6;

/// How many received packets one loop iteration translates. The loop also
/// pumps the WebRTC session, so it must not spend long in translation.
const TRANSLATE_PER_TICK: usize = 4;

/// Whether `address` names a Bedrock server rather than a Java one.
pub fn is_bedrock(address: &str) -> bool {
    address.starts_with(PREFIX) || address.starts_with(RAKNET_PREFIX)
}

/// A server-list status for a Bedrock address, so the entry renders instead of
/// reporting the failed Java ping. Cosmetic: NetherNet has no status exchange,
/// and a RakNet server's own MOTD would take its unconnected ping. The protocol
/// is this game's, which is what makes the entry read as reachable.
pub fn status(address: &str) -> Option<serde_json::Value> {
    if !is_bedrock(address) {
        return None;
    }
    let (host, port) = split(address);
    let transport = if over_raknet(address) {
        "RakNet"
    } else {
        "NetherNet"
    };
    Some(serde_json::json!({
        "version": {
            "name": format!("Bedrock via {transport}"),
            "protocol": horizon_protocol::PROTOCOL_VERSION,
        },
        "players": { "max": 100, "online": 0, "sample": [] },
        "description": { "text": format!("Bedrock server {host}:{port} ({transport})") },
    }))
}

/// Whether `address` asks for RakNet; NetherNet is the default.
fn over_raknet(address: &str) -> bool {
    address.starts_with(RAKNET_PREFIX)
}

/// The address without its scheme.
fn bare(address: &str) -> &str {
    address
        .strip_prefix(RAKNET_PREFIX)
        .or_else(|| address.strip_prefix(PREFIX))
        .unwrap_or(address)
}

/// NetherNet's signalling base: a URL when the address carries one, else the
/// server's own HTTP service, which is what a Bedrock server answers on.
fn base(address: &str, host: &str, port: u16) -> String {
    let rest = bare(address);
    if rest.starts_with("http://") || rest.starts_with("https://") {
        rest.trim_end_matches('/').to_owned()
    } else {
        format!("http://{host}:{port}")
    }
}

fn split(address: &str) -> (String, u16) {
    let rest = bare(address);
    match rest.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => (
            host.trim_start_matches("http://")
                .trim_start_matches("https://")
                .to_owned(),
            port.parse().unwrap(),
        ),
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
    /// The `PlayerAuthInput` tick id: it advances with every input packet, and
    /// the server pairs corrections with the id they answer.
    tick: AtomicI64,
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
        tick: AtomicI64::new(1),
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

/// The Bedrock client for an address: NetherNet by default, which is what the
/// Bedrock client itself speaks and what a server serves unless it was started
/// with `transport=raknet`, which `bedrock+raknet:` asks for.
fn connect(
    address: &str,
    host: &str,
    port: u16,
    login: &auth::SignedIn,
) -> Result<bedrock_core::client::BedrockClient, String> {
    if over_raknet(address) {
        return bedrock_core::client::BedrockClient::connect(host, port, 1400)
            .map_err(|e| e.to_string());
    }
    // The identity key the chain was built on, with the multiplayer token the
    // authorization service signed over it: what the join asserts.
    let identity = bedrock_core::nethernet::identity::ClientIdentity::external(
        login.identity.try_clone().map_err(|e| e.to_string())?,
        login.multiplayer_token.clone(),
    );
    // The join path carries the client's own NetworkID, per the onboarding guide.
    bedrock_core::client::BedrockClient::connect_nethernet(&base(address, host, port), "1", &identity)
        .map_err(|e| e.to_string())
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
    let raknet = over_raknet(address);
    status(format!(
        "Connecting to Bedrock server {host}:{port} over {}",
        if raknet { "RakNet" } else { "NetherNet" }
    ));

    // Sign-in comes first: over NetherNet the identity key and the multiplayer
    // token signed over it are what the WebRTC join asserts.
    let login = match auth::sign_in(&host, port, &status) {
        Ok(l) => l,
        Err(e) => {
            let _ = events.send(Event::Disconnected {
                reason: format!("Xbox sign-in failed: {e}"),
                from_server: false,
            });
            return;
        }
    };

    let mut client = match connect(address, &host, port, &login) {
        Ok(c) => c,
        Err(e) => {
            let _ = events.send(Event::Disconnected {
                reason: format!("Bedrock connection failed: {e}"),
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
        spawn: [0.0; 3],
        spawn_yaw: 0.0,
        spawn_pitch: 0.0,
        published: 0,
        sections: std::collections::HashMap::new(),
        requests: Vec::new(),
        wanted: std::collections::VecDeque::new(),
        wanted_center: None,
        entities: std::collections::HashMap::new(),
        runtime_id: 0,
        unique_id: 0,
    };

    // Received packets waiting to be translated. Translation is heavy — a
    // column is 24 sections through the block registry — and it runs on the
    // same loop that pumps the WebRTC session, so only a few are translated per
    // iteration and the rest wait. A long stall lets ICE consent lapse, which
    // drops the peer connection mid-load.
    let mut queued: std::collections::VecDeque<bedrock_core::protocol::Packet> =
        std::collections::VecDeque::new();

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
        // Sub-chunk requests queued while translating.
        if !ctx.requests.is_empty() {
            let batch: Vec<bedrock_core::protocol::Out> = ctx.requests.drain(..).collect();
            if let Err(e) = client.send(&batch) {
                log::warn!("bedrock: sub-chunk request failed: {e}");
            }
        }
        // Bedrock → Java.
        match client.poll() {
            Ok(packets) => queued.extend(packets),
            Err(e) => {
                let _ = events.send(Event::Disconnected {
                    reason: format!("Bedrock read failed: {e}"),
                    from_server: false,
                });
                return;
            }
        }
        for _ in 0..TRANSLATE_PER_TICK {
            let Some(packet) = queued.pop_front() else {
                break;
            };
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
        /// The position the client should hold, from `StartGame`.
        pub spawn: [f64; 3],
        pub spawn_yaw: f32,
        pub spawn_pitch: f32,
        /// How many times the position has been repeated since `StartGame`.
        pub published: u8,
        /// The runtime ids of every section received for a column, by Java
        /// section index. A partial chunk carries no blocks, so the column is
        /// built from these as the sub-chunk replies arrive.
        pub sections:
            std::collections::HashMap<(i32, i32), std::collections::BTreeMap<i8, Vec<u32>>>,
        /// Sub-chunk requests queued while translating; the session loop sends
        /// them.
        pub requests: Vec<bedrock_core::protocol::Out>,
        /// Columns still to ask for, and the centre they were queued for.
        pub wanted: std::collections::VecDeque<(i32, i32)>,
        pub wanted_center: Option<(i32, i32)>,
        /// Where each entity was last put: a Bedrock move carries only the
        /// components that changed, and the client takes absolute positions.
        pub entities: std::collections::HashMap<u64, ([f32; 3], f32, f32, f32)>,
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
    fn column(ctx: &mut Context, x: i32, z: i32) -> Option<play::LevelChunkWithLight> {
        // Taken out of the map so the block registry can be borrowed while
        // building from it, and put back before returning.
        let known = ctx.sections.remove(&(x, z))?;
        if known.is_empty() {
            ctx.sections.insert((x, z), known);
            return None;
        }
        let blocks = ctx.blocks.as_mut()?;
        let dimension = DimensionType::vanilla("minecraft:overworld");
        let count = dimension.sections();
        let mut filled: Vec<Option<Section>> = (0..count).map(|_| None).collect();
        let mut heights = [0i32; 256];
        for (&y, ids) in &known {
            let base = i32::from(y) * 16;
            let Some(slot) = usize::try_from((base - dimension.min_y) / 16)
                .ok()
                .filter(|i| *i < count)
            else {
                continue;
            };
            let mut values = [0i32; 4096];
            let mut non_empty = 0i16;
            for (i, runtime_id) in ids.iter().enumerate() {
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
        // Sky light from the heightmap: the surface of each column and
        // everything above it is fully lit, below it is dark. This is not the
        // server's light — that is not in the sub-chunk stream — but it shades
        // terrain instead of lighting every face equally, which is what made
        // the shadows look wrong.
        let sky: Vec<Vec<u8>> = (0..light_sections)
            .map(|section| {
                let mut layer = vec![0u8; 2048];
                // Light section i covers block section i - 1.
                let base = dimension.min_y + (section as i32 - 1) * 16;
                for z in 0..16usize {
                    for x in 0..16usize {
                        let top = heights[z * 16 + x] + dimension.min_y;
                        for y in 0..16usize {
                            if base + (y as i32) < top {
                                continue;
                            }
                            let index = y * 256 + z * 16 + x;
                            let byte = index >> 1;
                            if index & 1 == 0 {
                                layer[byte] |= 15;
                            } else {
                                layer[byte] |= 15 << 4;
                            }
                        }
                    }
                }
                layer
            })
            .collect();
        let light = LightData {
            sky_mask: bitset(light_sections),
            block_mask: Vec::new(),
            empty_sky_mask: Vec::new(),
            // Block light everywhere is zero, which the empty mask conveys
            // without sending 2048 bytes per section.
            empty_block_mask: bitset(light_sections),
            sky,
            block: Vec::new(),
        };

        ctx.sections.insert((x, z), known);
        Some(play::LevelChunkWithLight {
            x,
            z,
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

    /// The column as an event, once it has any blocks.
    fn emit_column(ctx: &mut Context, x: i32, z: i32) -> Vec<Event> {
        match column(ctx, x, z) {
            Some(chunk) => vec![Event::Packet(Box::new(
                play::Clientbound::LevelChunkWithLight(chunk),
            ))],
            None => Vec::new(),
        }
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
                // Remembered so the position can be sent again once the client
                // has chunks: it is the packet that creates its own player, and
                // that is what ends the loading screen.
                ctx.spawn = [
                    g.position[0] as f64,
                    g.position[1] as f64,
                    g.position[2] as f64,
                ];
                ctx.spawn_yaw = g.yaw;
                ctx.spawn_pitch = g.pitch;
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
                    // LEVEL_CHUNKS_LOAD_START: the level tracker sits in
                    // `WaitingForServer` until this arrives, and its tick does
                    // nothing before that — so without it the loading screen
                    // never closes, however much of the world has arrived.
                    Event::Packet(Box::new(play::Clientbound::GameEvent(play::GameEvent {
                        event: 13,
                        param: 0.0,
                    }))),
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
                ctx.entities.insert(
                    entity.runtime_id,
                    (
                        entity.position,
                        entity.yaw,
                        entity.pitch,
                        entity.head_yaw,
                    ),
                );
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
                ctx.entities.insert(
                    player.runtime_id,
                    (player.position, player.yaw, player.pitch, player.yaw),
                );
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
            // Entities move with this one packet; without it everything stands
            // still. A delta carries only the components that changed, and the
            // client takes absolute positions, so the rest comes from where we
            // last put the entity.
            bp::Packet::MoveActorDelta(m) => {
                let Some(&(last, last_yaw, last_pitch, last_head)) =
                    ctx.entities.get(&m.runtime_id)
                else {
                    return Vec::new();
                };
                let degrees = |v: Option<u8>, last: f32| {
                    v.map_or(last, |v| f32::from(v) * (360.0 / 256.0))
                };
                // Bedrock sends (pitch, yaw, head yaw).
                let pitch = degrees(m.rotation[0], last_pitch);
                let yaw = degrees(m.rotation[1], last_yaw);
                let head = degrees(m.rotation[2], last_head);
                let position = [
                    m.position[0].unwrap_or(last[0]),
                    m.position[1].unwrap_or(last[1]),
                    m.position[2].unwrap_or(last[2]),
                ];
                let moved = m.position.iter().any(Option::is_some);
                let turned = m.rotation.iter().any(Option::is_some);
                ctx.entities
                    .insert(m.runtime_id, (position, yaw, pitch, head));
                if moved {
                    vec![Event::Packet(Box::new(play::Clientbound::TeleportEntity(
                        play::TeleportEntity {
                            id: m.runtime_id as i32,
                            position: [
                                position[0] as f64,
                                position[1] as f64,
                                position[2] as f64,
                            ],
                            delta: [0.0; 3],
                            yaw,
                            pitch,
                            relatives: 0,
                            on_ground: m.on_ground,
                        },
                    )))]
                } else if turned {
                    // Turning on the spot: a teleport would also move it.
                    vec![Event::Packet(Box::new(play::Clientbound::RotateHead(
                        play::RotateHead {
                            id: m.runtime_id as i32,
                            head_yaw: (head / (360.0 / 256.0)) as i8,
                        },
                    )))]
                } else {
                    Vec::new()
                }
            }
            bp::Packet::MoveEntityAbsolute(m) => {
                ctx.entities
                    .insert(m.runtime_id, (m.position, m.yaw, m.pitch, m.yaw));
                vec![Event::Packet(Box::new(
                    play::Clientbound::TeleportEntity(play::TeleportEntity {
                        id: m.runtime_id as i32,
                        position: [
                            m.position[0] as f64,
                            m.position[1] as f64,
                            m.position[2] as f64,
                        ],
                        delta: [0.0; 3],
                        yaw: m.yaw,
                        pitch: m.pitch,
                        relatives: 0,
                        on_ground: m.on_ground,
                    }),
                ))]
            }
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
            // The client creates its own player from a position packet, and that
            // is what ends the loading screen. At StartGame its chunk area is not
            // there yet, so the position is repeated while the server publishes
            // chunks — a few times, then never again. The same update says which
            // columns to ask for.
            bp::Packet::NetworkChunkPublisherUpdate { position, radius } => {
                let mut out = Vec::new();
                if ctx.published < 4 {
                    ctx.published += 1;
                    out.push(Event::Packet(Box::new(play::Clientbound::PlayerPosition(
                        play::PlayerPosition {
                            teleport: 0,
                            position: ctx.spawn,
                            delta: [0.0; 3],
                            yaw: ctx.spawn_yaw,
                            pitch: ctx.spawn_pitch,
                            relatives: 0,
                        },
                    ))));
                }
                let center = (position[0] >> 4, position[2] >> 4);
                // Only refill once the player has really moved on, or the queue
                // is emptied again before it is ever drained. The player's own
                // column goes first: the client lights a column only when it
                // receives it, and that is the one the loading screen wants.
                let moved = ctx.wanted_center.is_none_or(|last| {
                    (last.0 - center.0).abs() > 2 || (last.1 - center.1).abs() > 2
                });
                if moved {
                    ctx.wanted_center = Some(center);
                    ctx.wanted.clear();
                    ctx.wanted.push_back((
                        (ctx.spawn[0].floor() as i32) >> 4,
                        (ctx.spawn[2].floor() as i32) >> 4,
                    ));
                    let r = i32::try_from((*radius).min(12)).unwrap_or(4);
                    for dx in -r..=r {
                        for dz in -r..=r {
                            ctx.wanted.push_back((center.0 + dx, center.1 + dz));
                        }
                    }
                }
                for _ in 0..super::REQUESTS_PER_TICK {
                    let Some((cx, cz)) = ctx.wanted.pop_front() else {
                        break;
                    };
                    // Every section of the column: the client meshes what it has.
                    let offsets: Vec<[i8; 3]> = (-4..=19).map(|sy| [0i8, sy as i8, 0]).collect();
                    ctx.requests
                        .push(bedrock_core::protocol::Out::SubChunkRequest {
                            dimension: 0,
                            center: [cx, 4, cz],
                            offsets,
                        });
                }
                out
            }
            // Other players' names. A player is only listed — and its avatar
            // only drawn — once the client has an info entry for its UUID, so
            // this goes out before the entity that uses it. The skin pixels are
            // carried in the entry but not sent yet: Java wants a PNG behind a
            // textures property, and Bedrock hands over raw RGBA.
            bp::Packet::PlayerList(list) => {
                let entries: Vec<play::PlayerInfo> = list
                    .entries
                    .iter()
                    .filter_map(|entry| {
                        let skin = entry.skin.as_ref()?;
                        Some(play::PlayerInfo {
                            uuid: u128::from_be_bytes(entry.uuid),
                            name: Some(entry.username.clone()),
                            properties: vec![horizon_protocol::packets::login::Property {
                                name: "textures".into(),
                                value: bedrock_core::png::textures_value(skin),
                                signature: None,
                            }],
                            listed: Some(true),
                            latency: Some(0),
                            ..Default::default()
                        })
                    })
                    .collect();
                if entries.is_empty() {
                    return Vec::new();
                }
                vec![Event::Packet(Box::new(play::Clientbound::PlayerInfoUpdate(
                    play::PlayerInfoUpdate {
                        // ADD_PLAYER, UPDATE_LISTED, UPDATE_LATENCY.
                        actions: 0b0001_1001,
                        entries,
                    },
                )))]
            }
            bp::Packet::LevelChunk(c) => {
                // Whatever blocks the payload carries; the rest arrive as
                // sub-chunk replies.
                if let Ok(decoded) = bedrock_core::chunk::decode_sections(c) {
                    let entry = ctx.sections.entry((c.chunk_x, c.chunk_z)).or_default();
                    for section in decoded {
                        entry.insert(section.y, section.blocks);
                    }
                }
                emit_column(ctx, c.chunk_x, c.chunk_z)
            }
            // A changed block. Without it, nothing built or broken shows up
            // until the column is sent again.
            bp::Packet::UpdateBlock {
                position,
                runtime_id,
                flags: _,
            } => {
                let Some(blocks) = ctx.blocks.as_mut() else {
                    return Vec::new();
                };
                let state = blocks.java(*runtime_id as i32);
                vec![Event::Packet(Box::new(play::Clientbound::BlockUpdate(
                    play::BlockUpdate {
                        pos: *position,
                        state: state as i32,
                    },
                )))]
            }
            // A column's blocks. A server running with client-side chunk
            // generation sends partial chunks and answers these requests, so
            // without this every column stays empty and never lights.
            bp::Packet::SubChunk(sub) => {
                let mut any = false;
                for entry in &sub.entries {
                    if entry.result != 1 || entry.payload.is_empty() {
                        continue;
                    }
                    let mut r = bedrock_core::stream::Reader::new(&entry.payload[1..]);
                    let Ok(sc) = bedrock_core::subchunk::SubChunk::read(&mut r, entry.payload[0])
                    else {
                        continue;
                    };
                    let y = (sub.position[1] + i32::from(entry.offset[1]) - 4) as i8;
                    let mut ids = vec![0u32; 4096];
                    for sy in 0..16usize {
                        for sz in 0..16usize {
                            for sx in 0..16usize {
                                let id = sc.top_runtime_id(sx, sy, sz).unwrap_or(0);
                                ids[(sy * 16 + sz) * 16 + sx] = u32::try_from(id).unwrap_or(0);
                            }
                        }
                    }
                    ctx.sections
                        .entry((sub.position[0], sub.position[2]))
                        .or_default()
                        .insert(y, ids);
                    any = true;
                }
                if any {
                    emit_column(ctx, sub.position[0], sub.position[2])
                } else {
                    Vec::new()
                }
            }
            // Block updates, block entities, inventories and the rest still
            // need their own translation; see the README.
            _ => Vec::new(),
        }
    }

    /// A `PlayerAuthInput` from a Java movement packet — the packet the server
    /// actually simulates from (id 144; `MovePlayer`, 19, is deprecated and
    /// ignored, which is why the player looked like a statue to it). The flag
    /// set is empty for now: no sneaking, sprinting or block actions yet.
    fn auth_input(bridge: &Bridge, position: [f32; 3], yaw: f32, pitch: f32) -> Vec<bp::Out> {
        let tick = bridge.tick.fetch_add(1, Ordering::Relaxed) as u64;
        vec![bp::Out::PlayerAuthInput {
            position,
            pitch,
            yaw,
            head_yaw: yaw,
            move_vec: [0.0; 2],
            flags: 0,
            tick,
        }]
    }

    /// Java serverbound → Bedrock.
    pub fn serverbound(bridge: &Bridge, packet: &play::Serverbound) -> Vec<bp::Out> {
        let runtime_id = bridge.runtime_id.load(Ordering::Relaxed) as u64;
        match packet {
            play::Serverbound::MovePlayerPos(p) => {
                let pos = [p.x as f32, p.y as f32, p.z as f32];
                *bridge.position.lock().unwrap() = pos;
                auth_input(bridge, pos, 0.0, 0.0)
            }
            play::Serverbound::MovePlayerPosRot(p) => {
                let pos = [p.x as f32, p.y as f32, p.z as f32];
                *bridge.position.lock().unwrap() = pos;
                auth_input(bridge, pos, p.yaw, p.pitch)
            }
            play::Serverbound::MovePlayerRot(p) => {
                // Rotation only: keep the last position so it is not a teleport.
                let pos = *bridge.position.lock().unwrap();
                auth_input(bridge, pos, p.yaw, p.pitch)
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

    pub fn sign_in(host: &str, port: u16, status: &dyn Fn(String)) -> Result<SignedIn, String> {
        let address = format!("{host}:{port}");
        // Reuse a refresh token when we have one.
        let cached = std::fs::read_to_string(token_path())
            .ok()
            .and_then(|t| serde_json::from_str::<String>(&t).ok());
        let msa = if let Some(refresh) = cached {
            core_auth::refresh(&refresh).map_err(|e| e.to_string())?
        } else {
            let code = core_auth::begin_device_code().map_err(|e| e.to_string())?;
            // The status line too: the game shows it, the console may not be read.
            status(format!(
                "Sign in at {} with code {}",
                code.verification_uri, code.user_code
            ));
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
