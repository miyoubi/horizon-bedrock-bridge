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
use std::sync::{Arc, Mutex, OnceLock, mpsc};

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
    /// Last (yaw, pitch) the game sent, so a block action can carry the view
    /// it was aimed with.
    rotation: Mutex<[f32; 2]>,
    /// The game's `PlayerInput` flags (Java bitfield): forward 1, back 2, left
    /// 4, right 8, jump 16, sneak 32, sprint 64. The server simulates movement
    /// from this and the move vector, not from the position we claim.
    input: AtomicI64,
    /// The position sent with the previous input, so the input's `delta`
    /// (movement this tick) is real rather than zero.
    last_sent: Mutex<[f32; 3]>,
    /// The `PlayerAuthInput` tick id: it advances with every input packet, and
    /// the server pairs corrections with the id they answer.
    tick: AtomicI64,
    /// Whether the game last reported the player standing on ground, and
    /// whether it reported a horizontal collision. Bedrock carries these as the
    /// `VerticalCollision`/`HorizontalCollision` input flags, and the server
    /// picks ground friction from them; without them it treats the player as
    /// airborne and glides after the keys are released.
    on_ground: AtomicBool,
    collision: AtomicBool,
    /// The hotbar slot the game last selected, carried by item-use
    /// transactions.
    hotbar: AtomicI64,
    /// The player's inventory slots as reported by the server, so transactions
    /// (place, use, attack) can carry the real held item.
    items: Mutex<Vec<Option<bedrock_core::protocol::ItemDescriptor>>>,
    /// Set when the game drops the session, so the thread ends.
    shutdown: AtomicBool,
    /// The tick at which to ask the server for the inventories again, because a
    /// pickup changed them; zero when nothing is pending.
    resync_at: AtomicI64,
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
        rotation: Mutex::new([0.0; 2]),
        input: AtomicI64::new(0),
        last_sent: Mutex::new([0.0; 3]),
        tick: AtomicI64::new(1),
        on_ground: AtomicBool::new(true),
        collision: AtomicBool::new(false),
        hotbar: AtomicI64::new(0),
        items: Mutex::new(vec![None; 36]),
        resync_at: AtomicI64::new(0),
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
            run(
                &addr,
                &thread_name,
                view_distance,
                &resources,
                &send,
                out_rx,
                &bridge,
            );
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
    bedrock_core::client::BedrockClient::connect_nethernet(
        &base(address, host, port),
        "1",
        &identity,
    )
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

    // The server keeps the previous session for a while after a bridge leaves,
    // so a quick reconnect is often refused. Retry it rather than giving up.
    let mut client = {
        let mut attempt = 0;
        loop {
            match connect(address, &host, port, &login) {
                Ok(c) => break c,
                Err(e) => {
                    attempt += 1;
                    if attempt >= 8 {
                        let _ = events.send(Event::Disconnected {
                            reason: format!("Bedrock connection failed: {e}"),
                            from_server: false,
                        });
                        return;
                    }
                    status(format!("Bedrock connection failed ({e}); retrying"));
                    std::thread::sleep(std::time::Duration::from_secs(3));
                }
            }
        }
    };
    let auth_info = bedrock_core::auth::auth_info(&login.multiplayer_token, 0);
    if let Err(e) = client.begin_login(login.identity, login.handshake, auth_info, login.skin_json)
    {
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
        log::warn!(
            "bedrock: no block registry at {}; chunks will be skipped",
            resources.display()
        );
    }

    // The client's biome ids: its own file order, with plains as the default
    // tint for every chunk until real per-chunk biomes are decoded.
    let biomes = translate::biome_names(&resources);
    let biome = biomes
        .iter()
        .position(|name| name == "minecraft:plains")
        .unwrap_or(0) as i32;

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
        drops: std::collections::HashMap::new(),
        drop_owner: std::collections::HashMap::new(),
        uniques: std::collections::HashMap::new(),
        drop_children: std::collections::HashMap::new(),
        name: name.to_string(),
        items: Vec::new(),
        biomes,
        biome,
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

    let mut last_heartbeat = std::time::Instant::now();
    // How many world packets have arrived since StartGame; the initialized
    // signal is repeated while the world streams in.
    let mut chunks_seen = 0u32;
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
            bedrock_core::nethernet::trace(format!("session ending: alive={alive} shutdown={}", bridge.shutdown.load(Ordering::Relaxed)));
            // Tell the server the player is leaving and close the transport
            // gracefully; dropping it without this leaves them in the world
            // until the server times the connection out.
            client.disconnect();
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
        // Keep the input stream going, as a real client does, so the server
        // never sees the player go silent (other clients would see it frozen).
        // Only once we are in-world: a `PlayerAuthInput` during login is an
        // unexpected packet and the server kicks with reason 49.
        if ctx.runtime_id != 0 && last_heartbeat.elapsed() >= std::time::Duration::from_millis(50) {
            let batch = translate::heartbeat(&bridge);
            if let Err(e) = client.send(&batch) {
                log::warn!("bedrock: heartbeat failed: {e}");
            }
            last_heartbeat = std::time::Instant::now();
        }
        // Translate more packets when a backlog built up, so received entities,
        // chunks and chat are not seconds behind. The budget still bounds each
        // iteration so the WebRTC pump is serviced.
        let budget = if queued.len() > 16 {
            TRANSLATE_PER_TICK * 3
        } else {
            TRANSLATE_PER_TICK
        };
        for _ in 0..budget {
            let Some(packet) = queued.pop_front() else {
                break;
            };
            for event in translate::clientbound(&mut ctx, &**bridge, &packet) {
                let _ = events.send(event);
            }
            // React to the resource-pack phase automatically.
            if let bedrock_core::protocol::Packet::PlayStatus(s) = &packet {
                if *s == 0 {
                    let _ = client.accept_resource_packs();
                }
                // 3 = PlayerSpawn. A real client ends its loading screen and
                // reports itself initialized here; without that the server keeps
                // the player's movement skipped.
                if *s == 3 {
                    let _ = client.end_loading_screen();
                }
            }
            if let bedrock_core::protocol::Packet::StartGame(g) = &packet {
                ctx.runtime_id = g.entity_runtime_id;
                ctx.unique_id = g.entity_unique_id;
                bridge
                    .runtime_id
                    .store(g.entity_runtime_id as i64, Ordering::Relaxed);
                let _ = client.request_chunks(i32::from(view_distance));
            }
            // The server ignores the player's input until the client reports
            // itself initialized, and a real client only reports it once the
            // world is arriving. Repeat it as chunks come in so it lands after
            // the load, not only in the login burst.
            if matches!(
                &packet,
                bedrock_core::protocol::Packet::LevelChunk(_)
                    | bedrock_core::protocol::Packet::SubChunk(_)
            ) && chunks_seen < 24
            {
                chunks_seen += 1;
                let _ = client.set_local_player_as_initialized();
            }
        }
        // Only sleep when there is nothing queued; with a backlog, go straight
        // round again to catch up.
        if queued.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(2));
        } else {
            std::thread::yield_now();
        }
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
    use horizon_protocol::item::Stack;
    use horizon_protocol::packets::configuration::{RegistryData, RegistryEntry};
    use horizon_protocol::packets::play;
    use horizon_protocol::registry::{Builtin, DimensionType, Dynamic};
    use horizon_world::nbt::{JavaString, Tag as JTag};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

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
        /// The stack each dropped item entity carries, so a pickup can put it
        /// into the inventory: the server sends no inventory packet for one.
        pub drops: std::collections::HashMap<u64, bp::NetworkItem>,
        /// Which drawn item entity each item actor belongs to. A Bedrock client
        /// renders dropped items of one kind that lie together as a single
        /// stack while the server keeps them as separate actors, so several
        /// actors share one entity here and it goes when the last of them does.
        pub drop_owner: std::collections::HashMap<u64, u64>,
        /// The runtime id an entity's unique id names. The server removes an
        /// entity by its unique id, so the runtime id the game was told about
        /// has to be found from it before the removal can be passed on.
        pub uniques: std::collections::HashMap<u64, u64>,
        /// The dropped stacks the server folded into another one, by the stack
        /// they joined. They stay drawn where they fell and go in with it.
        pub drop_children: std::collections::HashMap<u64, Vec<u64>>,
        /// The player's own name, for its tab entry: the server's `PlayerList`
        /// entry for the local player carries no name.
        pub name: String,
        /// The server's item palette from `StartGame`: `(identifier, network
        /// id)`. The inventory and equipment packets carry the network id, so
        /// this is what resolves them to a Java item.
        pub items: Vec<(String, i16)>,
        /// The client's biome names in its own id order (the files under
        /// `data/minecraft/worldgen/biome`, sorted by name, which is how the
        /// client builds its table). The bridged `minecraft:worldgen/biome`
        /// registry is sent in this order so a network id maps straight back to
        /// the same biome.
        pub biomes: Vec<String>,
        /// The network biome id written into every chunk: the client's id for
        /// plains. Real per-chunk biomes need the sub-chunk biome palettes and
        /// are still TODO, but the tint is correct rather than falling back to
        /// whatever biome id 1 hit in an empty registry.
        pub biome: i32,
        pub runtime_id: u64,
        pub unique_id: i64,
    }

    /// The client's biome names in its own id order.
    ///
    /// The client loads `data/minecraft/worldgen/biome/*.json` and sorts the
    /// file names; its ids are that order. Sending the `minecraft:worldgen/biome`
    /// registry in the same order makes a network id equal to the client's own
    /// id, so a chunk's biome value needs no remapping.
    pub fn biome_names(resources: &std::path::Path) -> Vec<String> {
        let dir = resources.join("data/minecraft/worldgen/biome");
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let path = e.path();
                if path.extension().is_some_and(|ext| ext == "json") {
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .map(|s| format!("minecraft:{s}"))
                } else {
                    None
                }
            })
            .collect();
        names.sort();
        names
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
        let biome = ctx.biome;
        let blocks = ctx.blocks.as_mut()?;
        let dimension = DimensionType::vanilla("minecraft:overworld");
        // Where the column's sections were put, so a vertical offset against
        // the entities can be read off rather than guessed.
        bedrock_core::nethernet::trace(format!(
            "column ({x},{z}) min_y={} sections={} first_y={:?}",
            dimension.min_y,
            dimension.sections(),
            known.keys().next(),
        ));
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
                biomes: Paletted::from_values(&[biome; 64], Strategy::Biomes, 7),
            });
        }
        let air = blocks.air;
        let air_section = move || Section {
            non_empty: 0,
            fluids: 0,
            states: Paletted::from_values(&[air as i32; 4096], Strategy::Blocks, 16),
            biomes: Paletted::from_values(&[biome; 64], Strategy::Biomes, 7),
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
                    chunk.iter().enumerate().fold(0u64, |word, (k, v)| {
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
                            // The top solid block is at `top - 1` and is exposed
                            // to the sky, so it is fully lit too — lighting only
                            // `>= top` left the surface block itself dark.
                            if base + (y as i32) < top - 1 {
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

    /// A standing player's eye height above its feet. Bedrock player positions
    /// (`PlayerAuthInput` mPos and `MovePlayer`) are eye-based, while Java's
    /// entity positions are feet-based, so the two directions need this offset.
    const PLAYER_EYE: f32 = 1.62;

    /// The Java entity-type id for a name, or `None` when the client has no
    /// such entity. A miss must not fall back to 0: id 0 is `acacia_boat`, which
    /// is what turned every unrecognised entity into a boat.
    fn entity_type_id(ctx: &Context, java_name: &str) -> Option<i32> {
        ctx.builtin
            .get("minecraft:entity_type")
            .and_then(|ids| ids.id(java_name))
    }

    /// A Bedrock item identifier → Horizon's numeric item id (0 when unknown).
    fn item_id(builtin: &Builtin, java_name: &str) -> i32 {
        builtin
            .get("minecraft:item")
            .and_then(|ids| ids.id(java_name))
            .unwrap_or(0)
    }

    /// A network id from the server's item palette → Horizon's numeric item id.
    fn item_by_network_id(
        mappings: &bedrock_core::mapping::Mappings,
        builtin: &Builtin,
        palette: &[(String, i16)],
        network_id: i16,
    ) -> i32 {
        mappings
            .java_item_by_network_id(palette, network_id)
            .map(|name| item_id(builtin, &name))
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

    /// The stack a drawn item entity shows: every actor folded into it, summed.
    fn drop_stack(ctx: &Context, owner: u64) -> Option<Stack> {
        let mut net: Option<&bp::NetworkItem> = None;
        let mut count: i32 = 0;
        for (actor, item) in ctx.drops.iter() {
            if ctx.drop_owner.get(actor) == Some(&owner) {
                net.get_or_insert(item);
                count += i32::from(item.count);
            }
        }
        let net = net?;
        let id = item_by_network_id(ctx.mappings, ctx.builtin, &ctx.items, net.runtime_id);
        if id == 0 {
            return None;
        }
        Some(Stack::new(id, count.max(1)))
    }

    /// An item entity's stack as a data update, which is how a merged stack
    /// shows its new count without being respawned.
    fn drop_stack_data(owner: u64, stack: Stack) -> Event {
        Event::Packet(Box::new(play::Clientbound::SetEntityData(
            play::SetEntityData {
                id: owner as i32,
                values: vec![(
                    8,
                    horizon_protocol::entity_data::serializer::ITEM_STACK,
                    horizon_protocol::entity_data::Value::Item(Some(stack)),
                )],
            },
        )))
    }

    pub fn clientbound(ctx: &mut Context, bridge: &Bridge, packet: &bp::Packet) -> Vec<Event> {
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
                // StartGame's position is the eye; the client's feet are 1.62
                // lower. Everything downstream (the repeated spawn teleport and
                // the auth input) is feet-based, so convert here or the player
                // spawns — and stays — floating a block and a half up.
                ctx.spawn = [
                    g.position[0] as f64,
                    g.position[1] as f64 - PLAYER_EYE as f64,
                    g.position[2] as f64,
                ];
                ctx.spawn_yaw = g.yaw;
                ctx.spawn_pitch = g.pitch;
                // The biome registry in the client's own id order. Without it the
                // client's biome table is empty and every chunk biome id falls
                // back to the first biome (badlands — brown grass); with it, the
                // id we write into each chunk maps straight to that biome.
                let mut registries = Dynamic::default();
                registries.add(RegistryData {
                    registry: "minecraft:worldgen/biome".to_owned(),
                    entries: ctx
                        .biomes
                        .iter()
                        .map(|name| RegistryEntry {
                            id: name.clone(),
                            data: None,
                        })
                        .collect(),
                });
                vec![
                    Event::Status("Bedrock login accepted".into()),
                    Event::Joined {
                        registries: Arc::new(registries),
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
                            position: [
                                g.position[0] as f64,
                                g.position[1] as f64 - PLAYER_EYE as f64,
                                g.position[2] as f64,
                            ],
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
                // A player's chat carries the sender's name in `source`;
                // without it the line reads as if nobody said it.
                let content = if t.source.is_empty() {
                    text_component(message.trim())
                } else {
                    text_component(&format!("<{}> {}", t.source, message.trim()))
                };
                vec![Event::Packet(Box::new(play::Clientbound::SystemChat(
                    play::SystemChat {
                        content,
                        overlay: false,
                    },
                )))]
            }
            bp::Packet::SetTime(time) => vec![Event::Packet(Box::new(play::Clientbound::SetTime(
                play::SetTime {
                    game_time: i64::from(*time),
                    clocks: Vec::new(),
                },
            )))],
            bp::Packet::SetDifficulty(difficulty) => vec![Event::Packet(Box::new(
                play::Clientbound::ChangeDifficulty(play::ChangeDifficulty {
                    difficulty: *difficulty,
                    locked: true,
                }),
            ))],
            // Bedrock sends health as a signed varint.
            bp::Packet::SetHealth(health) => vec![Event::Packet(Box::new(
                play::Clientbound::SetHealth(play::SetHealth {
                    health: *health as f32,
                    food: 20,
                    saturation: 5.0,
                }),
            ))],
            bp::Packet::AddEntity(entity) => {
                let java_name = ctx.mappings.java_entity(&entity.identifier);
                // An entity the client has no type for is skipped rather than
                // spawned as id 0 (acacia boat).
                let Some(kind) = entity_type_id(ctx, &java_name) else {
                    return Vec::new();
                };
                bedrock_core::nethernet::trace(format!(
                    "add entity {} {} at {:?}",
                    entity.runtime_id, entity.identifier, entity.position
                ));
                ctx.entities.insert(
                    entity.runtime_id,
                    (entity.position, entity.yaw, entity.pitch, entity.head_yaw),
                );
                ctx.uniques
                    .insert(entity.unique_id as u64, entity.runtime_id);
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
            // A dropped item: spawn the Java item entity and put the stack in
            // its metadata (index 8 is the item entity's stack).
            bp::Packet::AddItemEntity {
                runtime_id,
                unique_id,
                item,
                position,
                velocity,
            } => {
                let java_name = ctx.mappings.java_entity("minecraft:item");
                let Some(kind) = entity_type_id(ctx, &java_name) else {
                    return Vec::new();
                };
                // Every actor is drawn as its own entity, as the reference does;
                // the removals are what keep the ground clear.
                let owner: Option<u64> = None;
                ctx.drops.insert(*runtime_id, item.clone());
                ctx.uniques.insert(*unique_id as u64, *runtime_id);
                let merged = match owner {
                    Some(owner) => {
                        ctx.drop_owner.insert(*runtime_id, owner);
                        owner
                    }
                    None => {
                        ctx.drop_owner.insert(*runtime_id, *runtime_id);
                        ctx.entities
                            .insert(*runtime_id, (*position, 0.0, 0.0, 0.0));
                        *runtime_id
                    }
                };
                let shown = drop_stack(ctx, merged);
                if merged != *runtime_id {
                    // Already drawn: only the stack's count changes.
                    return shown.map_or_else(Vec::new, |s| vec![drop_stack_data(merged, s)]);
                }
                let Some(stack) = shown.or_else(|| map_item(ctx, item)) else {
                    return Vec::new();
                };
                vec![
                    Event::Packet(Box::new(play::Clientbound::AddEntity(play::AddEntity {
                        id: *runtime_id as i32,
                        uuid: *unique_id as u128,
                        kind,
                        position: [
                            position[0] as f64,
                            position[1] as f64,
                            position[2] as f64,
                        ],
                        velocity: [
                            velocity[0] as f64,
                            velocity[1] as f64,
                            velocity[2] as f64,
                        ],
                        pitch: 0,
                        yaw: 0,
                        head_yaw: 0,
                        data: 0,
                    }))),
                    drop_stack_data(*runtime_id, stack),
                ]
            }
            // Picking an item up: it flies to the actor and despawns. The
            // server sends no inventory packet for the pickup, so put the stack
            // into the player's first free slot ourselves.
            bp::Packet::TakeItemEntity { item, actor } => {
                if *actor != ctx.runtime_id as u64 && ctx.drops.contains_key(item) {
                    // The server folded this stack into another dropped one. Keep
                    // it on the ground to be seen on its own, and remember the
                    // stack it joined so the two are collected together.
                    ctx.drop_children.entry(*actor).or_default().push(*item);
                    return Vec::new();
                }
                // This player took it: the stack and everything folded into it
                // fly in together.
                let mut flying = vec![*item];
                if let Some(children) = ctx.drop_children.remove(item) {
                    flying.extend(children);
                }
                let mut events = Vec::new();
                for id in flying {
                    ctx.drops.remove(&id);
                    ctx.entities.remove(&id);
                    events.push(Event::Packet(Box::new(
                        play::Clientbound::TakeItemEntity(play::TakeItemEntity {
                            item: id as i32,
                            player: *actor as i32,
                            amount: 1,
                        }),
                    )));
                    events.push(Event::Packet(Box::new(
                        play::Clientbound::RemoveEntities(play::RemoveEntities {
                            ids: vec![id as i32],
                        }),
                    )));
                }
                if *actor == ctx.runtime_id as u64 {
                    let now = bridge.tick.load(Ordering::Relaxed);
                    bridge.resync_at.store(now + 10, Ordering::Relaxed);
                }
                events
            }
            bp::Packet::AddPlayer(player) => {
                let uuid = u128::from_be_bytes(player.uuid);
                let Some(kind) = entity_type_id(ctx, "minecraft:player") else {
                    return Vec::new();
                };
                ctx.entities.insert(
                    player.runtime_id,
                    (player.position, player.yaw, player.pitch, player.yaw),
                );
                // The server's player-list entries carry no name, so the tab
                // entry for a player is built from the spawn, which names it.
                vec![
                    Event::Packet(Box::new(play::Clientbound::PlayerInfoUpdate(
                        play::PlayerInfoUpdate {
                            actions: 0b0001_1001, // add, latency, listed
                            entries: vec![play::PlayerInfo {
                                uuid,
                                name: Some(player.username.clone()),
                                listed: Some(true),
                                latency: Some(0),
                                ..Default::default()
                            }],
                        },
                    ))),
                    Event::Packet(Box::new(play::Clientbound::AddEntity(play::AddEntity {
                        id: player.runtime_id as i32,
                        uuid,
                        kind,
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
                    }))),
                ]
            }
            // Entity metadata. Bedrock's actor flags (index 0, a long) become
            // Java's shared-flags byte: on-fire, sneaking, sprinting and
            // invisible occupy the same bit positions in both, so the rest of
            // the long is dropped. Bedrock's name tag (index 4) and its
            // always-show flag (index 81) become Java's custom name (index 2)
            // and its visible flag (index 3).
            bp::Packet::SetEntityData(d) => {
                let mut values = Vec::new();
                for (k, v) in &d.metadata {
                    match (*k, v) {
                        (0, bp::MetaValue::Long(f)) => values.push((
                            0,
                            horizon_protocol::entity_data::serializer::BYTE,
                            horizon_protocol::entity_data::Value::Byte((*f as u64 & 0x2B) as i8),
                        )),
                        (4, bp::MetaValue::String(name)) => values.push((
                            2,
                            horizon_protocol::entity_data::serializer::OPTIONAL_COMPONENT,
                            horizon_protocol::entity_data::Value::OptText(Some(
                                text_component(name),
                            )),
                        )),
                        (81, bp::MetaValue::Byte(show)) => values.push((
                            3,
                            horizon_protocol::entity_data::serializer::BOOLEAN,
                            horizon_protocol::entity_data::Value::Bool(*show != 0),
                        )),
                        _ => {}
                    }
                }
                if values.is_empty() {
                    return Vec::new();
                }
                vec![Event::Packet(Box::new(play::Clientbound::SetEntityData(
                    play::SetEntityData {
                        id: d.runtime_id as i32,
                        values,
                    },
                )))]
            }
            // The item registry: the network ids every inventory and equipment
            // packet names an item by. It arrives once, after `StartGame`.
            bp::Packet::ItemRegistry(items) => {
                ctx.items = items.clone();
                Vec::new()
            }
            // The item an entity is holding. `SetEquipment` carries the two
            // hands Java models; other containers are inventory traffic. An
            // empty or unknown stack clears the slot.
            bp::Packet::MobEquipment(e) => {
                let java_slot = match e.window_id {
                    // Inventory / hotbar → main hand.
                    0 | 28 | 29 => 0u8,
                    // Off hand.
                    34 | 119 => 1u8,
                    _ => return Vec::new(),
                };
                let id = item_by_network_id(
                    ctx.mappings,
                    ctx.builtin,
                    &ctx.items,
                    e.new_item.runtime_id,
                );
                let stack = (id != 0 && e.new_item.count > 0)
                    .then(|| Stack::new(id, i32::from(e.new_item.count)));
                vec![Event::Packet(Box::new(play::Clientbound::SetEquipment(
                    play::SetEquipment {
                        id: e.runtime_id as i32,
                        slots: vec![(java_slot, stack)],
                    },
                )))]
            }
            // The server's authoritative position for our own player. Without
            // applying it the client drifts from the server (collisions, holes,
            // knockback) and then ignores the chunks sent for the server's
            // position, because they fall outside its view range.
            bp::Packet::CorrectPlayerMovePrediction { position, tick, .. } => {
                // Correct a real divergence only. Applying every tick's
                // prediction snapped the player back and forth, which reads as
                // teleporting; a small gap is just the server trailing the
                // client by a tick, and the client's own movement is closer to
                // correct there. Knockback and void falls are far enough apart
                // to still land.
                let held = *bridge.position.lock().unwrap();
                let feet = position[1] - PLAYER_EYE;
                let (dx, dy, dz) = (
                    position[0] - held[0],
                    feet - held[1],
                    position[2] - held[2],
                );
                {
                    use std::sync::atomic::{AtomicU64, Ordering};
                    static N: AtomicU64 = AtomicU64::new(0);
                    if N.fetch_add(1, Ordering::Relaxed) % 3 == 0 {
                        bedrock_core::nethernet::trace(format!(
                            "CORR tick={} now={} input={:#04x} feet=[{:.2},{:.2},{:.2}] held=[{:.2},{:.2},{:.2}] d=[{:.2},{:.2},{:.2}]",
                            tick,
                            bridge.tick.load(Ordering::Relaxed),
                            bridge.input.load(Ordering::Relaxed),
                            position[0], feet, position[2],
                            held[0], held[1], held[2], dx, dy, dz
                        ));
                    }
                }
                // Follow ViaBedrock: drop a correction whose tick has fallen
                // outside the server's rewind window, because applying it would
                // pull the client back to a position it has already left.
                const REWIND_WINDOW: u64 = 40;
                let now_tick = bridge.tick.load(Ordering::Relaxed) as u64;
                if *tick == 0 || *tick + REWIND_WINDOW < now_tick || *tick > now_tick {
                    return Vec::new();
                }
                // Apply only a genuine teleport-sized move. Tracking the
                // server's simulation at its own threshold pulled the client
                // back to the server's trailing position on every tick while
                // running, which reads as teleporting; the server's view of the
                // player is driven by the input, not by this.
                if dx * dx + dz * dz < 4.0 * 4.0 && dy.abs() < 3.0 {
                    return Vec::new();
                }
                vec![Event::Packet(Box::new(play::Clientbound::PlayerPosition(
                    play::PlayerPosition {
                        teleport: 0,
                        position: [position[0] as f64, feet as f64, position[2] as f64],
                        delta: [0.0; 3],
                        // Keep the client's own heading: the correction is a
                        // position, not a rotation to apply.
                        yaw: 0.0,
                        pitch: 0.0,
                        relatives: 0x18, // yaw + pitch relative
                    },
                )))]
            }
            bp::Packet::MovePlayer(m) => {
                // Bedrock reports a player's position at the eye; Java entity
                // positions are feet. Without the offset the player floats ~1.6.
                let position = [
                    m.position[0] as f64,
                    (m.position[1] - PLAYER_EYE) as f64,
                    m.position[2] as f64,
                ];
                if m.runtime_id as i64 == ctx.runtime_id as i64 {
                    vec![Event::Packet(Box::new(play::Clientbound::PlayerPosition(
                        play::PlayerPosition {
                            teleport: 0,
                            position,
                            delta: [0.0; 3],
                            yaw: m.yaw,
                            pitch: m.pitch,
                            relatives: 0,
                        },
                    )))]
                } else {
                    // The head turns on its own (head_yaw) while the body yaw
                    // lags behind, so without the head rotation a player only
                    // appears to pitch.
                    vec![
                        Event::Packet(Box::new(play::Clientbound::TeleportEntity(
                            play::TeleportEntity {
                                id: m.runtime_id as i32,
                                position,
                                delta: [0.0; 3],
                                yaw: m.yaw,
                                pitch: m.pitch,
                                relatives: 0,
                                on_ground: m.on_ground,
                            },
                        ))),
                        Event::Packet(Box::new(play::Clientbound::RotateHead(play::RotateHead {
                            id: m.runtime_id as i32,
                            head_yaw: (m.head_yaw / (360.0 / 256.0)) as i8,
                        }))),
                    ]
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
                let degrees =
                    |v: Option<u8>, last: f32| v.map_or(last, |v| f32::from(v) * (360.0 / 256.0));
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
                            position: [position[0] as f64, position[1] as f64, position[2] as f64],
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
                vec![Event::Packet(Box::new(play::Clientbound::TeleportEntity(
                    play::TeleportEntity {
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
                    },
                )))]
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
            bp::Packet::RemoveEntity(unique) => {
                let id = ctx.uniques.remove(unique).unwrap_or(*unique);
                // The server folds a pile of dropped stacks by taking the
                // absorbed actors away. Each of them still stands for a part of
                // that pile, so keep it on the ground and take it away with the
                // stack it joined: removing them here is what collapsed twelve
                // thrown blocks into a single one.
                if let Some(item) = ctx.drops.get(&id).cloned() {
                    let joined = ctx
                        .drops
                        .iter()
                        .find(|(other, d)| {
                            **other != id
                                && d.runtime_id == item.runtime_id
                                && d.damage == item.damage
                        })
                        .map(|(other, _)| *other);
                    if let Some(parent) = joined {
                        ctx.drop_children.entry(parent).or_default().push(id);
                        return Vec::new();
                    }
                }
                let children = ctx.drop_children.remove(&id).unwrap_or_default();
                let mut gone = vec![id];
                gone.extend(children);
                let mut events = Vec::new();
                for entity in gone {
                    ctx.entities.remove(&entity);
                    ctx.drops.remove(&entity);
                    events.push(Event::Packet(Box::new(play::Clientbound::RemoveEntities(
                        play::RemoveEntities {
                            ids: vec![entity as i32],
                        },
                    ))));
                }
                events
            }
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
                // Ask for the columns around the player every time they cross a
                // chunk boundary, and keep what is already queued. Refilling only
                // after several chunks, and emptying the queue to do it, left the
                // client without the columns it was standing in: it draws those as
                // holes, and the player falls through them.
                if ctx.wanted_center != Some(center) {
                    if ctx.wanted_center.is_none() {
                        ctx.wanted.push_back((
                            (ctx.spawn[0].floor() as i32) >> 4,
                            (ctx.spawn[2].floor() as i32) >> 4,
                        ));
                    }
                    ctx.wanted_center = Some(center);
                    // Only what the client will accept: anything further out is
                    // ignored on arrival ("not in the view range") and wasted.
                    let r = i32::try_from((*radius).min(8)).unwrap_or(4);
                    for dx in -r..=r {
                        for dz in -r..=r {
                            let column = (center.0 + dx, center.1 + dz);
                            if !ctx.wanted.contains(&column) {
                                ctx.wanted.push_back(column);
                            }
                        }
                    }
                }
                for _ in 0..super::REQUESTS_PER_TICK {
                    let Some((cx, cz)) = ctx.wanted.pop_front() else {
                        break;
                    };
                    // Every section of the column: the client meshes what it has.
                    // The base Y is 0 and the offsets carry the absolute section
                    // index, which for the overworld (min_y -64) runs -4..=19 —
                    // the convention a real Bedrock client uses.
                    let offsets: Vec<[i8; 3]> = (-4..=19).map(|sy| [0i8, sy as i8, 0]).collect();
                    ctx.requests
                        .push(bedrock_core::protocol::Out::SubChunkRequest {
                            dimension: 0,
                            center: [cx, 0, cz],
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
                    .map(|entry| {
                        // A Bedrock entry need not carry a skin; the tab entry
                        // still belongs there, so add the textures only when
                        // there is one rather than dropping the player.
                        let properties = entry
                            .skin
                            .as_ref()
                            .map(|skin| {
                                vec![horizon_protocol::packets::login::Property {
                                    name: "textures".into(),
                                    value: bedrock_core::png::textures_value(skin),
                                    signature: None,
                                }]
                            })
                            .unwrap_or_default();
                        play::PlayerInfo {
                            uuid: u128::from_be_bytes(entry.uuid),
                            // The entry for the local player carries no name.
                            name: Some(if entry.username.is_empty() {
                                ctx.name.clone()
                            } else {
                                entry.username.clone()
                            }),
                            properties,
                            listed: Some(true),
                            latency: Some(0),
                            ..Default::default()
                        }
                    })
                    .collect();
                if entries.is_empty() {
                    return Vec::new();
                }
                vec![Event::Packet(Box::new(
                    play::Clientbound::PlayerInfoUpdate(play::PlayerInfoUpdate {
                        // ADD_PLAYER, UPDATE_LISTED, UPDATE_LATENCY.
                        actions: 0b0001_1001,
                        entries,
                    }),
                ))]
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
            // The batched form of the change above.
            bp::Packet::UpdateSubChunkBlocks { blocks, .. } => {
                let Some(registry) = ctx.blocks.as_mut() else {
                    return Vec::new();
                };
                blocks
                    .iter()
                    .map(|block| {
                        let state = registry.java(block.runtime_id as i32);
                        Event::Packet(Box::new(play::Clientbound::BlockUpdate(
                            play::BlockUpdate {
                                pos: block.position,
                                state: state as i32,
                            },
                        )))
                    })
                    .collect()
            }
            // The player's own inventory. Bedrock's window 0 lists its slots in
            // Bedrock order, which is not Java's inventory-menu order, so place
            // each item at its Java slot. Other windows still need mapping.
            bp::Packet::InventoryContent(c) if c.window_id == 0 => {
                let mut items: Vec<Option<Stack>> = (0..46).map(|_| None).collect();
                for (i, item) in c.slots.iter().enumerate() {
                    let Some(slot) = java_inventory_slot(i as u32) else {
                        continue;
                    };
                    if let Ok(index) = usize::try_from(slot) {
                        if index < items.len() {
                            items[index] = map_item(ctx, item);
                        }
                    }
                }
                {
                    let mut held = bridge.items.lock().unwrap();
                    *held = c.slots.iter().map(descriptor).collect();
                }
                vec![Event::Packet(Box::new(
                    play::Clientbound::ContainerSetContent(play::ContainerSetContent {
                        container: 0,
                        state: 0,
                        items,
                        carried: None,
                    }),
                ))]
            }
            bp::Packet::InventorySlot(s) if s.window_id == 0 => {
                if let Ok(index) = usize::try_from(s.slot) {
                    let mut held = bridge.items.lock().unwrap();
                    if let Some(slot) = held.get_mut(index) {
                        *slot = descriptor(&s.item);
                    }
                }
                let Some(slot) = java_inventory_slot(s.slot) else {
                    return Vec::new();
                };
                let item = map_item(ctx, &s.item);
                vec![Event::Packet(Box::new(
                    play::Clientbound::ContainerSetSlot(play::ContainerSetSlot {
                        container: 0,
                        state: 0,
                        slot,
                        item,
                    }),
                ))]
            }
            // Java conveys the selected hotbar slot as inventory data id 0.
            bp::Packet::PlayerHotbar { slot, window_id } if *window_id == 0 => {
                vec![Event::Packet(Box::new(
                    play::Clientbound::ContainerSetData(play::ContainerSetData {
                        container: 0,
                        id: 0,
                        value: *slot as i16,
                    }),
                ))]
            }
            bp::Packet::ContainerClose { window_id } => {
                vec![Event::Packet(Box::new(play::Clientbound::ContainerClose(
                    play::ContainerClose {
                        container: i32::from(*window_id),
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
                    // The entry's section is the reply's base Y plus the offset,
                    // both absolute section indices (the request asked with base
                    // Y 0). `column` then places it against the dimension's
                    // `min_y`, which is the shift that was missing before.
                    let y = (sub.position[1] + i32::from(entry.offset[1])) as i8;
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
        auth_input_actions(bridge, position, yaw, pitch, Vec::new())
    }

    /// The same packet with block actions (start/stop/abort a break). The
    /// `PERFORM_BLOCK_ACTIONS` flag (bit 35) is set, which is the flag the
    /// encoder keys the action list off.
    fn auth_input_actions(
        bridge: &Bridge,
        position: [f32; 3],
        yaw: f32,
        pitch: f32,
        actions: Vec<bp::BlockAction>,
    ) -> Vec<bp::Out> {
        let tick = bridge.tick.fetch_add(1, Ordering::Relaxed) as u64;
        let raw_input = bridge.input.load(Ordering::Relaxed) as u8;
        let (mut flags, mut move_vec) = input_bits(bridge);
        if !actions.is_empty() {
            flags |= 1 << 35;
        }
        // The delta is a velocity in Bedrock's movement maths, which ViaBedrock
        // builds from the raw position change: horizontal * 0.98 * friction *
        // 0.91, and vertical minus gravity then * 0.98. Sending the raw position
        // change instead makes the server's reconciliation disagree every tick.
        let on_ground = bridge.on_ground.load(Ordering::Relaxed);
        let delta = {
            let mut last = bridge.last_sent.lock().unwrap();
            let d = [
                position[0] - last[0],
                position[1] - last[1],
                position[2] - last[2],
            ];
            *last = position;
            let friction = if on_ground { 0.6 } else { 1.0 };
            [
                d[0] * 0.98 * friction * 0.91,
                (d[1] - 0.08) * 0.98,
                d[2] * 0.98 * friction * 0.91,
            ]
        };
        // The move vector comes from the game's input bitfield, which holds the
        // pressed keys until they change. Deriving it from the position delta as
        // well read the client's own glide after a key release as still-held
        // input, and normalized it to full speed, so the server kept walking the
        // player. The bitfield is the real key state, so trust it alone.
        // Snap a near-zero component to zero: a leftover cross-component is
        // enough to set a direction flag, which the server honours.
        if move_vec[0].abs() < 0.05 {
            move_vec[0] = 0.0;
        }
        if move_vec[1].abs() < 0.05 {
            move_vec[1] = 0.0;
        }
        if move_vec != [0.0, 0.0] {
            flags &= !((1 << 10) | (1 << 11) | (1 << 12) | (1 << 13));
            if move_vec[1] > 0.0 {
                flags |= 1 << 10; // up / forward
            } else if move_vec[1] < 0.0 {
                flags |= 1 << 11; // down / backward
            }
            if move_vec[0] > 0.0 {
                flags |= 1 << 12; // left
            } else if move_vec[0] < 0.0 {
                flags |= 1 << 13; // right
            }
        }
        // Bedrock's mPos is the eye position; the game tracks feet.
        let position = [position[0], position[1] + PLAYER_EYE, position[2]];
        if tick % 20 == 0 || move_vec != [0.0, 0.0] {
            bedrock_core::nethernet::trace(format!(
                "AUTHIN-MOVE pos={:?} move={:?} flags={:#x} input={:#04x} delta={:?} tick={}",
                position, move_vec, flags, raw_input, delta, tick
            ));
        }
        vec![bp::Out::PlayerAuthInput {
            position,
            pitch,
            yaw,
            head_yaw: yaw,
            move_vec,
            delta,
            flags,
            block_actions: actions,
            tick,
        }]
    }

    /// Record the game's on-ground and horizontal-collision state from a Java
    /// movement packet's flags: bit 0 is on-ground, bit 1 is horizontal
    /// collision.
    fn set_movement_flags(bridge: &Bridge, flags: u8) {
        bridge.on_ground.store(flags & 1 != 0, Ordering::Relaxed);
        bridge.collision.store(flags & 2 != 0, Ordering::Relaxed);
    }

    /// A Bedrock network item as a Java stack, or `None` for air/unknown.
    fn map_item(ctx: &Context, item: &bp::NetworkItem) -> Option<Stack> {
        let id = item_by_network_id(ctx.mappings, ctx.builtin, &ctx.items, item.runtime_id);
        (id != 0 && item.count > 0).then(|| Stack::new(id, i32::from(item.count)))
    }

    /// The stack in the selected hotbar slot, as a descriptor for transactions.
    fn held_item(bridge: &Bridge) -> Option<bedrock_core::protocol::ItemDescriptor> {
        let slot = bridge.hotbar.load(Ordering::Relaxed).max(0) as usize;
        bridge.items.lock().unwrap().get(slot).copied().flatten()
    }

    /// A Bedrock network item as the descriptor a transaction carries.
    fn descriptor(item: &bp::NetworkItem) -> Option<bedrock_core::protocol::ItemDescriptor> {
        (item.runtime_id != 0 && item.count > 0).then(
            || bedrock_core::protocol::ItemDescriptor {
                runtime_id: item.runtime_id,
                count: item.count,
                damage: item.damage,
                block_runtime_id: item.block_runtime_id,
            },
        )
    }

    /// The whole player inventory as a Java content update, from what the
    /// server reported plus any pickups the bridge predicted. Sending the whole
    /// container keeps every count consistent, where a single slot update can
    /// be discarded when its menu state does not match.
    fn inventory_content(ctx: &Context, bridge: &Bridge) -> Event {
        let items = bridge.items.lock().unwrap();
        let mut slots: Vec<Option<Stack>> = (0..46).map(|_| None).collect();
        for (i, entry) in items.iter().enumerate() {
            let Some(d) = entry else { continue };
            let Some(slot) = java_inventory_slot(i as u32) else {
                continue;
            };
            let Ok(index) = usize::try_from(slot) else {
                continue;
            };
            if index >= slots.len() {
                continue;
            }
            let id = item_by_network_id(ctx.mappings, ctx.builtin, &ctx.items, d.runtime_id);
            if id != 0 {
                slots[index] = Some(Stack::new(id, i32::from(d.count)));
            }
        }
        Event::Packet(Box::new(play::Clientbound::ContainerSetContent(
            play::ContainerSetContent {
                container: 0,
                state: 0,
                items: slots,
                carried: None,
            },
        )))
    }

    /// Bedrock's player-inventory slot order to Java's inventory-menu order:
    /// hotbar 0..8, main 9..35, armour 36..39, off hand 40.
    fn java_inventory_slot(slot: u32) -> Option<i16> {
        Some(match slot {
            0..=8 => 36 + slot as i16,
            9..=35 => slot as i16,
            36..=39 => 5 + (slot - 36) as i16,
            40 => 45,
            _ => return None,
        })
    }

    /// The Bedrock input flags and move vector for the game's current
    /// `PlayerInput` bitfield. The server simulates movement from these, so with
    /// an empty vector it holds the player still however the position claims
    /// otherwise.
    fn input_bits(bridge: &Bridge) -> (u64, [f32; 2]) {
        let f = bridge.input.load(Ordering::Relaxed) as u8;
        // The game sends its bitfield only when it changes, so a held jump would
        // re-fire every tick. Consume it so one press is one jump.
        if f & 0x10 != 0 {
            bridge.input.fetch_and(!0x10, Ordering::Relaxed);
        }
        let (forward, backward) = (f & 0x01 != 0, f & 0x02 != 0);
        let (left, right) = (f & 0x04 != 0, f & 0x08 != 0);
        let (jump, sneak, sprint) = (f & 0x10 != 0, f & 0x20 != 0, f & 0x40 != 0);

        // block_breaking_delay_enabled is on every real client input.
        let mut bits = 1u64 << 48;
        // ViaBedrock sends these two from the game's own on-ground/collision
        // state, and the server picks ground friction from VerticalCollision:
        // without it the player is simulated as airborne and glides after the
        // keys are released.
        let on_ground = bridge.on_ground.load(Ordering::Relaxed);
        if on_ground {
            bits |= 1 << 50; // vertical_collision
        }
        if bridge.collision.load(Ordering::Relaxed) {
            bits |= 1 << 49; // horizontal_collision
        }
        if forward {
            bits |= 1 << 10; // up
        }
        if backward {
            bits |= 1 << 11; // down
        }
        if left {
            bits |= 1 << 12; // left
        }
        if right {
            bits |= 1 << 13; // right
        }
        if jump {
            // jumping, jump_down, want_up, jump_current_raw
            bits |= (1 << 6) | (1 << 3) | (1 << 16) | (1 << 61);
            if on_ground {
                bits |= 1 << 31; // start_jumping
            }
        }
        if sneak {
            // sneaking, sneak_down, want_down (sneak_current_raw is bit 64,
            // past the 64-bit flag set the encoder carries)
            bits |= (1 << 8) | (1 << 9) | (1 << 17);
        }
        if sprint {
            bits |= (1 << 20) | (1 << 4); // sprinting, sprint_down
        }
        // Bedrock's local move vector: +x is LEFT, +y is FORWARD. A diagonal is
        // scaled by 1/√2 and sneaking by 0.3, as the reference translator does.
        let mut mv = [
            if left {
                1.0f32
            } else if right {
                -1.0
            } else {
                0.0
            },
            if forward {
                1.0f32
            } else if backward {
                -1.0
            } else {
                0.0
            },
        ];
        if mv[0] != 0.0 && mv[1] != 0.0 {
            mv[0] *= std::f32::consts::FRAC_1_SQRT_2;
            mv[1] *= std::f32::consts::FRAC_1_SQRT_2;
        }
        if sneak {
            mv[0] *= 0.3;
            mv[1] *= 0.3;
        }
        (bits, mv)
    }

    /// One `PlayerAuthInput` for the current tick even when the game sent no
    /// movement. A real client transmits one every tick, and the server expects
    /// the stream to keep going (an idle client that stops sending looks frozen
    /// to the other players).
    pub fn heartbeat(bridge: &Bridge) -> Vec<bp::Out> {
        let pos = *bridge.position.lock().unwrap();
        let [yaw, pitch] = *bridge.rotation.lock().unwrap();
        let mut out = auth_input(bridge, pos, yaw, pitch);
        // A pickup is granted on the server a moment after it happens, so ask
        // for the inventories once it has had time to apply it. Asking straight
        // away returned the state from before the pickup and undid it.
        let due = bridge.resync_at.load(Ordering::Relaxed);
        if due != 0 && bridge.tick.load(Ordering::Relaxed) >= due {
            bridge.resync_at.store(0, Ordering::Relaxed);
            out.push(bedrock_core::protocol::Out::InventoryMismatch);
        }
        out
    }

    /// Java serverbound → Bedrock.
    pub fn serverbound(bridge: &Bridge, packet: &play::Serverbound) -> Vec<bp::Out> {
        match packet {
            // Movement and input only update the bridge state; the loop sends
            // one `PlayerAuthInput` per tick (50 ms) with all of it. Sending one
            // per movement packet ran the input tick far faster than the
            // server's, and it stopped applying our movement.
            play::Serverbound::MovePlayerPos(p) => {
                *bridge.position.lock().unwrap() = [p.x as f32, p.y as f32, p.z as f32];
                set_movement_flags(bridge, p.flags);
                Vec::new()
            }
            play::Serverbound::MovePlayerPosRot(p) => {
                *bridge.position.lock().unwrap() = [p.x as f32, p.y as f32, p.z as f32];
                *bridge.rotation.lock().unwrap() = [p.yaw, p.pitch];
                set_movement_flags(bridge, p.flags);
                Vec::new()
            }
            play::Serverbound::MovePlayerRot(p) => {
                *bridge.rotation.lock().unwrap() = [p.yaw, p.pitch];
                set_movement_flags(bridge, p.flags);
                Vec::new()
            }
            // A status-only packet carries just the movement flags; the game
            // sends it when only the on-ground/collision state changed.
            play::Serverbound::MovePlayerStatus(p) => {
                set_movement_flags(bridge, p.flags);
                Vec::new()
            }
            play::Serverbound::PlayerInput(p) => {
                bridge.input.store(i64::from(p.flags), Ordering::Relaxed);
                bedrock_core::nethernet::trace(format!("recv PlayerInput flags={:#04x}", p.flags));
                Vec::new()
            }
            // Swinging the arm. Bedrock has no local swing packet: it is an
            // `Animate` of the swing-arm action on the player's own runtime id.
            play::Serverbound::Punch(_) => vec![bp::Out::Animate {
                runtime_id: bridge.runtime_id.load(Ordering::Relaxed) as u64,
                action: 1, // swing_arm
            }],
            // Hitting an entity. Bedrock performs the hit as an item-use-on-
            // entity inventory transaction, and needs the swing alongside it.
            play::Serverbound::Attack(a) => {
                let pos = *bridge.position.lock().unwrap();
                vec![
                    bp::Out::Animate {
                        runtime_id: bridge.runtime_id.load(Ordering::Relaxed) as u64,
                        action: 1, // swing_arm
                    },
                    bp::Out::ItemUseOnEntity {
                        runtime_id: a.id as u64,
                        action: 1, // attack
                        hotbar_slot: bridge.hotbar.load(Ordering::Relaxed) as i32,
                        item: held_item(bridge),
                        position: [pos[0], pos[1] + PLAYER_EYE, pos[2]],
                    },
                ]
            }
            play::Serverbound::SetCarriedItem(i) => {
                bridge.hotbar.store(i64::from(i.slot), Ordering::Relaxed);
                vec![bp::Out::MobEquipment {
                    runtime_id: bridge.runtime_id.load(Ordering::Relaxed) as u64,
                    slot: i.slot as u8,
                    window_id: 0,
                }]
            }
            // Placing or using an item on a block: Bedrock's item-use
            // inventory transaction with a place action.
            play::Serverbound::UseItemOn(u) => {
                let pos = *bridge.position.lock().unwrap();
                vec![bp::Out::ItemUseOnBlock {
                    block: u.pos,
                    face: u.face,
                    hotbar_slot: bridge.hotbar.load(Ordering::Relaxed) as i32,
                    action: 0, // place
                    item: held_item(bridge),
                    cursor: u.cursor,
                    position: [pos[0], pos[1] + PLAYER_EYE, pos[2]],
                }]
            }
            // Using the held item in the air (eating, throwing): the same
            // transaction with a use action and no block.
            play::Serverbound::UseItem(_) => {
                let pos = *bridge.position.lock().unwrap();
                vec![bp::Out::ItemUseOnBlock {
                    block: [0, 0, 0],
                    face: 255,
                    hotbar_slot: bridge.hotbar.load(Ordering::Relaxed) as i32,
                    action: 1, // use
                    item: held_item(bridge),
                    cursor: [0.0; 3],
                    position: [pos[0], pos[1] + PLAYER_EYE, pos[2]],
                }]
            }
            // Dropping an item: the game clicks "outside" the window (slot
            // -999). Bedrock performs it as an item-release transaction.
            play::Serverbound::ContainerClick(c) if c.slot == -999 => {
                let pos = *bridge.position.lock().unwrap();
                let slot = bridge.hotbar.load(Ordering::Relaxed) as i32;
                let item = held_item(bridge);
                bedrock_core::nethernet::trace(format!(
                    "DROP click slot={} button={} input={} hotbar={slot} item={item:?}",
                    c.slot, c.button, c.input
                ));
                {
                    let mut items = bridge.items.lock().unwrap();
                    if let Some(entry) = items.get_mut(slot.max(0) as usize) {
                        *entry = None;
                    }
                }
                vec![bp::Out::ItemRelease {
                    slot,
                    count: item.map_or(1, |i| i.count.max(1) as u8),
                    item,
                    position: [pos[0], pos[1] + PLAYER_EYE, pos[2]],
                }]
            }
            // A creative-screen drop: the game sets slot -1, which drops the
            // stack from the hand. Bedrock does it as an item-release.
            play::Serverbound::SetCreativeModeSlot(s) if s.slot == -1 => {
                let pos = *bridge.position.lock().unwrap();
                let slot = bridge.hotbar.load(Ordering::Relaxed) as i32;
                let item = held_item(bridge);
                {
                    let mut items = bridge.items.lock().unwrap();
                    if let Some(entry) = items.get_mut(slot.max(0) as usize) {
                        *entry = None;
                    }
                }
                vec![bp::Out::ItemRelease {
                    slot,
                    count: item.map_or(1, |i| i.count.max(1) as u8),
                    item,
                    position: [pos[0], pos[1] + PLAYER_EYE, pos[2]],
                }]
            }
            // Block breaking, with the action ids a real 1.26 client sends:
            // `start_break` 0, `abort_break` 1, `predict_break` 26. Horizon's
            // Java numbering (its `lan_actions.rs` handler) is 0 start, 1/2
            // abort, 3 finished.
            play::Serverbound::PlayerAction(p) => {
                let action = match p.action {
                    0 => 0,     // start_break
                    1 | 2 => 1, // abort_break
                    3 => 26,    // predict_break (the client predicted the break)
                    // The game's own numbering drops through a player action:
                    // 4 drops the whole stack, 5 drops one. Bedrock drops with
                    // an item-release transaction instead.
                    4 | 5 => {
                        let pos = *bridge.position.lock().unwrap();
                        let slot = bridge.hotbar.load(Ordering::Relaxed) as i32;
                        let item = held_item(bridge);
                        bedrock_core::nethernet::trace(format!(
                            "DROP action={} slot={slot} item={item:?}",
                            p.action
                        ));
                        {
                            let mut items = bridge.items.lock().unwrap();
                            let index = slot.max(0) as usize;
                            if let Some(entry) = items.get_mut(index) {
                                if p.action == 5 && entry.as_ref().is_some_and(|e| e.count > 1) {
                                    if let Some(e) = entry.as_mut() {
                                        e.count -= 1;
                                    }
                                } else {
                                    *entry = None;
                                }
                            }
                        }
                        return vec![bp::Out::ItemRelease {
                            slot,
                            count: if p.action == 5 {
                                1
                            } else {
                                item.map_or(1, |i| i.count.max(1) as u8)
                            },
                            item,
                            position: [pos[0], pos[1] + PLAYER_EYE, pos[2]],
                        }];
                    }
                    _ => return Vec::new(),
                };
                let pos = *bridge.position.lock().unwrap();
                let [yaw, pitch] = *bridge.rotation.lock().unwrap();
                auth_input_actions(
                    bridge,
                    pos,
                    yaw,
                    pitch,
                    vec![bp::BlockAction {
                        action,
                        position: p.pos,
                        face: i32::from(p.direction),
                    }],
                )
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
            // ServerboundClientCommandPacket: 0 is respawn. A dead player's
            // respawn sends it, and the server revives the player only once it
            // hears back, so report the client ready to spawn.
            play::Serverbound::ClientCommand(c) if c.action == 0 => {
                bedrock_core::nethernet::trace("recv ClientCommand respawn".to_string());
                vec![bp::Out::Respawn {
                    position: [0.0; 3],
                    state: 2, // ClientReadyToSpawn
                    runtime_id: bridge.runtime_id.load(Ordering::Relaxed) as u64,
                }]
            }
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
            let _ = std::fs::write(
                token_path(),
                serde_json::to_string(refresh).unwrap_or_default(),
            );
        }
        let data = core_auth::login(&msa.access_token, &address, bedrock_core::MINECRAFT_VERSION)
            .map_err(|e| e.to_string())?;
        // The multiplayer identity token, signed over the chain's own key.
        use bedrock_core::nethernet::auth_service;
        let xbl3 = core_auth::playfab_xbl3(&msa.access_token).map_err(|e| e.to_string())?;
        let ticket = auth_service::playfab_login(&xbl3).map_err(|e| e.to_string())?;
        let auth_header = auth_service::session_start(&ticket, bedrock_core::MINECRAFT_VERSION)
            .map_err(|e| e.to_string())?;
        let multiplayer_token =
            auth_service::multiplayer_token(&auth_header, &data.identity.public_der())
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
