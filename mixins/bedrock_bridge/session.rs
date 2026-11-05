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
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, Ordering};
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

/// How many column requests the pump sends at once. Each is a round trip and
/// the loop also pumps the WebRTC session, so it is paced, and only into the
/// free part of `REQUEST_WINDOW`.
const REQUESTS_PER_TICK: usize = 12;

/// The shortest gap between two request batches: a little under one Bedrock
/// tick, so the requests go out at a steady rate instead of a burst.
const REQUEST_INTERVAL: std::time::Duration = std::time::Duration::from_millis(40);

/// How many column requests may be outstanding at once. The server does the
/// work for every request it is given, so asking for far more than the loop can
/// translate keeps it busy — and a busy server answers the player's own chat
/// and movement late. A window the size of the translation queue keeps the
/// pipeline full without flooding it.
const REQUEST_WINDOW: usize = 16;

/// A hard stop for the received-packet queue, in case translation stalls for a
/// reason of its own. Far above the request window, so it never shapes the
/// stream in normal play.
const RECEIVE_QUEUE_LIMIT: usize = 1024;

/// How long a column waits for a neighbour before it is sent and lit without
/// it (a light update fixes it once the neighbour is there). Without this a
/// reply the server dropped leaves the column and its neighbours as holes.
const REGION_PATIENCE: std::time::Duration = std::time::Duration::from_millis(1500);

/// How long a section the server never answered may hold a column back before
/// it is sent with what did arrive (and the missing sections are logged).
const COLUMN_PATIENCE: std::time::Duration = std::time::Duration::from_secs(8);

/// How often columns are swept for one that became sendable without a packet
/// arriving for it (a neighbour finished, or a reply has waited long enough).
const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// How far (in chunks) the bridge keeps every section's Bedrock runtime ids for
/// block uses. A runtime id is the exact state the server has, and it cannot
/// always be recovered from the game's state, so the sections around the player
/// keep theirs; the rest of the streamed circle would otherwise hold 16 KB per
/// section — over a gigabyte at a 32-chunk radius — for blocks nobody is near
/// enough to click (reach is five blocks).
const RUNTIME_ID_RADIUS: i32 = 8;

/// How many finished columns are sent to the game per second. The game's chunk
/// packets get a millisecond a frame; past that a backlog builds and, once it
/// is 50 ms old, the game handles the whole backlog in one frame — a stall the
/// player feels as a frozen chat box. This pace is above what the server's
/// replies sustain in normal play, so it only bites when the columns are
/// already in hand (a radius change, a respawn) and must not be dumped at once.
const EMIT_PER_SECOND: f32 = 150.0;

/// The most columns that may be sent at once, so the first view is there
/// quickly (the loading screen is over; the game paces its own inserting).
const EMIT_BURST: f32 = 96.0;

/// The largest chunk radius a vanilla Bedrock server grants (its
/// `view-distance` cap). Used only until `ChunkRadiusUpdated` says otherwise,
/// so the game is never told to hold more columns than one would send.
const BEDROCK_MAX_RADIUS: i32 = 32;

/// How many received packets one loop iteration translates. The loop also
/// pumps the WebRTC session, so it must not spend long in translation; a
/// column can take tens of milliseconds, so the count is also capped by
/// `TRANSLATE_BUDGET`.
const TRANSLATE_PER_TICK: usize = 4;

/// How long one iteration may spend translating before the loop goes back to
/// the outbound direction and the WebRTC pump. This is what keeps the player's
/// own chat and movement prompt while the world streams in: without it a burst
/// of column translations held the loop for up to a second.
const TRANSLATE_BUDGET: std::time::Duration = std::time::Duration::from_millis(20);

/// The radius the bridge streams: what the game asked for, capped by what the
/// server granted. Before the grant arrives the vanilla cap stands in.
fn effective_radius(bridge: &Bridge) -> i32 {
    let requested = bridge.requested_radius.load(Ordering::Relaxed).max(2);
    let granted = bridge.granted_radius.load(Ordering::Relaxed);
    if granted <= 0 {
        requested.min(BEDROCK_MAX_RADIUS)
    } else {
        requested.min(granted).max(2)
    }
}

/// Recompute [`effective_radius`] into the shared value the network thread and
/// the login packets read, and return it.
fn store_effective_radius(bridge: &Bridge) -> i32 {
    let radius = effective_radius(bridge);
    bridge.chunk_radius.store(radius, Ordering::Relaxed);
    radius
}

/// Whether commands are sent in the schema's numeric origin/version form. A
/// live BDS answers a `player`/`latest` string origin and refuses the numeric
/// one (it reads the request as a huge command), so the string form is the
/// default and `HZ_CMD_NUMERIC=1` selects the other.
fn command_numeric_layout() -> bool {
    static NUMERIC: OnceLock<bool> = OnceLock::new();
    *NUMERIC.get_or_init(|| std::env::var_os("HZ_CMD_NUMERIC").is_some())
}

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
    /// The player's unique id as `StartGame` reported it, which a command
    /// origin carries.
    unique_id: AtomicI64,
    /// The chunk radius this session streams: the smaller of the game's
    /// render-distance setting and the server's grant. Login and
    /// `SetChunkCacheRadius` both carry it, and it bounds the columns the
    /// bridge asks for.
    chunk_radius: AtomicI32,
    /// The radius the game's render-distance setting asks for, which is what a
    /// `RequestChunkRadius` carries. The player can change it while playing.
    requested_radius: AtomicI32,
    /// The radius the server granted in `ChunkRadiusUpdated`, or `0` until it
    /// answers.
    granted_radius: AtomicI32,
    /// Set when the effective radius changed, so the network thread tells the
    /// game and rebuilds the columns it streams.
    radius_dirty: AtomicBool,
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
    /// The input flags sent with the previous `PlayerAuthInput`, so a press or
    /// a release can be reported once instead of every tick.
    sent_input: AtomicI64,
    /// Whether the player is sprinting, from the game's `PlayerCommand`. The
    /// input bitfield only carries the sprint key, not the sprint state.
    sprinting: AtomicBool,
    /// Whether the previous input reported sprinting, for the start/stop edge.
    sent_sprint: AtomicBool,
    /// Whether the previous input reported the player on the ground, which is
    /// what a new jump is measured against.
    sent_on_ground: AtomicBool,
    /// Block actions the game asked for since the last input, sent with the
    /// next one so there is still one input per client tick.
    actions: Mutex<Vec<bedrock_core::protocol::BlockAction>>,
    /// The position sent with the previous input, so the input's `delta`
    /// (movement this tick) is real rather than zero.
    last_sent: Mutex<[f32; 3]>,
    /// The feet position reported on each recent input, by tick. A correction
    /// names the tick it answers, so its offset from what we claimed there is
    /// the error to remove.
    history: Mutex<std::collections::VecDeque<(i64, [f32; 3])>>,
    /// The part of a correction still to be applied, in feet space. It is paid
    /// off a little each client tick: one jump of half a block is a teleport,
    /// the same half block spread over five ticks is a step.
    correction: Mutex<[f32; 3]>,
    /// Whether the player's body is in water, sampled from the columns.
    in_water: AtomicBool,
    /// Whether the previous input packet reported the swimming pose.
    sent_swimming: AtomicBool,
    /// The game's event channel, so the tick path can nudge the player without
    /// waiting for a packet to translate.
    events: mpsc::Sender<Event>,
    /// The `PlayerAuthInput` tick id: it advances with every input packet, and
    /// the server pairs corrections with the id they answer. It starts on the
    /// server's own tick from `StartGame`, which is the clock the server
    /// rewinds to.
    tick: AtomicI64,
    input_tick: AtomicI64,
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
    /// The held stack as the server was last told. A move or a use can change
    /// what is in the hand without changing the slot, and then the server (and
    /// every other player) keeps showing the old item.
    sent_held: Mutex<Option<bedrock_core::protocol::ItemDescriptor>>,
    /// The player's inventory slots as reported by the server, so transactions
    /// (place, use, attack) can carry the real held item.
    items: Mutex<Vec<Option<bedrock_core::protocol::ItemDescriptor>>>,
    /// The armour the server reports, in its own order: helmet, chest, legs,
    /// boots.
    armor: Mutex<Vec<Option<bedrock_core::protocol::ItemDescriptor>>>,
    /// The off-hand item.
    offhand: Mutex<Option<bedrock_core::protocol::ItemDescriptor>>,
    /// The item on the mouse cursor, which Bedrock keeps in its own container.
    cursor: Mutex<Option<bedrock_core::protocol::ItemDescriptor>>,
    /// The id of the next inventory move. The server answers each request with
    /// the same id, so the two can be paired up. It has to be a negative odd
    /// number, and each request steps it down by two.
    request_id: AtomicI32,
    /// The container the server opened, if one is open.
    window: Mutex<Option<OpenWindow>>,
    /// Set when the game drops the session, so the thread ends.
    shutdown: AtomicBool,
    /// The tick at which to ask the server for the inventories again, because a
    /// pickup changed them; zero when nothing is pending.
    resync_at: AtomicI64,
    /// The player's health, food and saturation as the server last reported
    /// them. Bedrock carries hunger in its attributes and health in `SetHealth`,
    /// while Java sends all three together, so they are kept here.
    vitals: Mutex<Vitals>,
    /// Set when the player asks to respawn from the death screen, so the
    /// server's `Respawn` is answered only for their own request and not for
    /// the one that ends the join handshake.
    respawn_asked: AtomicBool,
    /// The spawn info `StartGame` gave, so the death screen can be closed even
    /// if the server's own `Respawn` is delayed behind the input tick.
    spawn_info: Mutex<Option<play::SpawnInfo>>,
    /// Set when a teleport (a respawn) has just been applied, so the next input
    /// tells the server it was handled.
    teleport_ack: AtomicBool,
    /// Block uses wait for the network thread, which is where the column data
    /// lives and the clicked block's state can be looked up.
    pending_use: Mutex<Vec<play::UseItemOn>>,
    /// The block of the last use, so a resync can be tied to it.
    last_use: Mutex<Option<[i32; 3]>>,
    /// Entities to take away once their death has been shown, with the tick it
    /// is due on. The game plays the fall-over animation for a second, and
    /// removing the entity in the same tick skipped it.
    removals: Mutex<Vec<(i64, u64)>>,
    /// The slots as they were before each move the bridge has sent but not yet
    /// had answered, keyed by the request id the server echoes. A move is
    /// shown at once (a real client predicts it) and put back if refused.
    pending_moves: Mutex<Vec<(i32, Items)>>,
    /// The slots a move has changed locally but the server has not answered for
    /// yet, each mapped to that request's id. A slot changed by an
    /// unacknowledged request is named by the request id, not by the stack id it
    /// had before; quoting the old id is what made rapid moves and drags fail.
    pending_ids: Mutex<std::collections::HashMap<(u8, u8), i32>>,
    /// The shift-clicks still waiting for an answer. A quick move picks the
    /// slot to move into on both sides, and the two can choose differently, so
    /// its answer is followed by a whole-menu update rather than single slots.
    quick_moves: Mutex<std::collections::HashSet<i32>>,
    /// Each item's Java name by its network id, from the item registry, so a
    /// click can tell armour and a shield from everything else.
    item_names: Mutex<std::collections::HashMap<i16, String>>,
    /// Whether the server has the player's inventory screen open. A real client
    /// asks for it with `Interact`/`open_inventory`; the cursor and the
    /// `hotbar_and_inventory` container only exist while it is.
    inv_open: AtomicBool,
}

/// The local player's health bar as Java conveys it.
#[derive(Clone, Copy)]
struct Vitals {
    health: f32,
    food: i32,
    saturation: f32,
}

impl Default for Vitals {
    fn default() -> Self {
        Self {
            health: 20.0,
            food: 20,
            saturation: 5.0,
        }
    }
}

/// A container the server opened. Bedrock names the container in every packet
/// about it instead of numbering it, so the name is kept from the content the
/// server sent and quoted back when the game moves an item.
struct OpenWindow {
    /// The Bedrock container id, which doubles as the game's window id.
    id: u8,
    /// The Bedrock window type it was opened as, which closing it has to name.
    bedrock_type: u8,
    /// Which menu the game shows for it, and how many slots it holds.
    kind: WindowKind,
    title: String,
    slots: Vec<Option<bedrock_core::protocol::ItemDescriptor>>,
    name: Option<bedrock_core::protocol::FullContainerName>,
}

impl OpenWindow {
    fn store(&mut self, slot: usize, item: Option<bedrock_core::protocol::ItemDescriptor>) {
        if let Some(entry) = self.slots.get_mut(slot) {
            *entry = item;
        }
    }
}

/// A copy of every slot a move can reach: the player's own inventory, the armour
/// and off hand, the cursor and the open container. Taken before a move so a
/// refused one can be undone exactly.
#[derive(Clone)]
struct Items {
    items: Vec<Option<bedrock_core::protocol::ItemDescriptor>>,
    armor: Vec<Option<bedrock_core::protocol::ItemDescriptor>>,
    offhand: Option<bedrock_core::protocol::ItemDescriptor>,
    cursor: Option<bedrock_core::protocol::ItemDescriptor>,
    window: Option<(
        Option<bedrock_core::protocol::FullContainerName>,
        Vec<Option<bedrock_core::protocol::ItemDescriptor>>,
    )>,
}

/// The container menus the bridge can show. Everything else is closed again
/// rather than opened as a screen whose clicks would go nowhere.
#[derive(Clone, Copy, PartialEq)]
enum WindowKind {
    /// A chest, barrel, shulker box or chest minecart: 27 slots.
    Container27,
}

impl WindowKind {
    fn of(bedrock_type: u8) -> Option<WindowKind> {
        match bedrock_type {
            // Container, chest minecart and chest boat all hold 27 slots.
            0 | 10 | 34 => Some(WindowKind::Container27),
            _ => None,
        }
    }

    fn slots(self) -> usize {
        match self {
            WindowKind::Container27 => 27,
        }
    }

    /// The game's menu id for it.
    fn menu(self) -> i32 {
        match self {
            // minecraft:generic_9x3
            WindowKind::Container27 => 2,
        }
    }

    fn title(self) -> &'static str {
        match self {
            WindowKind::Container27 => "Container",
        }
    }

    /// Where the player's own slots sit in the game's menu for it: the main
    /// inventory after the container's slots, then the hotbar.
    fn java_player_slot(self, slot: u8) -> Option<i16> {
        let start = self.slots() as i16;
        let hotbar = start + 27;
        if slot < 9 {
            Some(hotbar + i16::from(slot))
        } else if slot < 36 {
            Some(start + i16::from(slot) - 9)
        } else {
            None
        }
    }

    /// The other direction, for a slot the game names.
    fn bedrock_player_slot(self, java: i16) -> Option<u8> {
        let start = self.slots() as i16;
        let hotbar = start + 27;
        if java >= hotbar && java < hotbar + 9 {
            Some((java - hotbar) as u8)
        } else if java >= start && java < start + 27 {
            Some((java - start + 9) as u8)
        } else {
            None
        }
    }
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
        unique_id: AtomicI64::new(0),
        chunk_radius: AtomicI32::new(
            i32::from(view_distance).max(2).min(BEDROCK_MAX_RADIUS),
        ),
        requested_radius: AtomicI32::new(i32::from(view_distance).max(2)),
        granted_radius: AtomicI32::new(0),
        radius_dirty: AtomicBool::new(false),
        position: Mutex::new([0.0; 3]),
        rotation: Mutex::new([0.0; 2]),
        input: AtomicI64::new(0),
        sent_input: AtomicI64::new(0),
        sprinting: AtomicBool::new(false),
        sent_sprint: AtomicBool::new(false),
        sent_on_ground: AtomicBool::new(true),
        actions: Mutex::new(Vec::new()),
        last_sent: Mutex::new([0.0; 3]),
        history: Mutex::new(std::collections::VecDeque::new()),
        correction: Mutex::new([0.0; 3]),
        in_water: AtomicBool::new(false),
        sent_swimming: AtomicBool::new(false),
        events: send.clone(),
        tick: AtomicI64::new(1),
        input_tick: AtomicI64::new(0),
        on_ground: AtomicBool::new(true),
        collision: AtomicBool::new(false),
        hotbar: AtomicI64::new(0),
        sent_held: Mutex::new(None),
        items: Mutex::new(vec![None; 36]),
        armor: Mutex::new(vec![None; 4]),
        offhand: Mutex::new(None),
        cursor: Mutex::new(None),
        request_id: AtomicI32::new(-1),
        window: Mutex::new(None),
        resync_at: AtomicI64::new(0),
        vitals: Mutex::new(Vitals::default()),
        respawn_asked: AtomicBool::new(false),
        spawn_info: Mutex::new(None),
        teleport_ack: AtomicBool::new(false),
        pending_use: Mutex::new(Vec::new()),
        last_use: Mutex::new(None),
        removals: Mutex::new(Vec::new()),
        pending_moves: Mutex::new(Vec::new()),
        pending_ids: Mutex::new(std::collections::HashMap::new()),
        quick_moves: Mutex::new(std::collections::HashSet::new()),
        item_names: Mutex::new(std::collections::HashMap::new()),
        inv_open: AtomicBool::new(false),
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
            run(&addr, &thread_name, &resources, &send, out_rx, &bridge);
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
        far: Arc::new(crate::lod_net::Inbox::default()),
        recorder: Arc::new(Mutex::new(None)),
    })
}

/// Translate and queue an outgoing Java packet. Returns whether the session is
/// the bridge's (so the hook can cancel the normal Java encode).
pub fn send(session: &Session, packet: &play::Serverbound) -> bool {
    let map = bridges().lock().unwrap();
    let Some(bridge) = map.get(&session.address) else {
        return false;
    };
    // A block use needs the clicked block's Bedrock state, and only the
    // network thread holds the column data.
    if let play::Serverbound::UseItemOn(u) = packet {
        bridge.pending_use.lock().unwrap().push(u.clone());
        return true;
    }
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

    // The block and light tables are read from the prepared resources and take
    // a moment; build them before the connection exists so the WebRTC pump is
    // never left waiting on them.
    let blocks = translate::Blocks::new(resources);
    if blocks.is_none() {
        log::warn!(
            "bedrock: no block registry at {}; chunks will be skipped",
            resources.display()
        );
    }

    // Sign-in comes first: over NetherNet the identity key and the multiplayer
    // token signed over it are what the WebRTC join asserts.
    let login = match auth::sign_in(&host, port, &status) {
        Ok(l) => l,
        Err(e) => {
            let _ = events.send(Event::Disconnected {
                reason: format!("Xbox sign-in failed: {e}"),
                key: None,
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
                            key: None,
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
            key: None,
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
        spawn_info: None,
        spawn_yaw: 0.0,
        spawn_pitch: 0.0,
        published: 0,
        chunks: std::collections::HashMap::new(),
        block_ids: std::collections::HashMap::new(),
        kinds: std::collections::HashMap::new(),
        requests: Vec::new(),
        wanted: std::collections::VecDeque::new(),
        queued: std::collections::HashSet::new(),
        pending_requests: 0,
        columns: std::collections::HashMap::new(),
        wanted_center: None,
        sent_center: None,
        radius: bridge.chunk_radius.load(Ordering::Relaxed).max(2),
        sent_radius: None,
        last_request: None,
        last_sweep: None,
        ready: std::collections::BinaryHeap::new(),
        emit_budget: EMIT_BURST,
        last_emit: None,
        entities: std::collections::HashMap::new(),
        players: std::collections::HashSet::new(),
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

    let mut chunks_seen = 0u32;
    let mut last_water = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(1))
        .unwrap_or_else(std::time::Instant::now);
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
        // A new chunk radius (the player changed the render distance, or the
        // server granted a different one) is passed on before anything is
        // streamed with it, so the game's cache and ours agree.
        if bridge.radius_dirty.swap(false, Ordering::Relaxed) {
            let radius = bridge.chunk_radius.load(Ordering::Relaxed).max(2);
            if ctx.sent_radius != Some(radius) {
                ctx.sent_radius = Some(radius);
                ctx.radius = radius;
                let _ = events.send(Event::Packet(Box::new(
                    play::Clientbound::SetChunkCacheRadius(play::Distance {
                        distance: radius,
                    }),
                )));
                translate::radius_changed(&mut ctx);
            }
        }
        // Ask for more columns, and send whichever ones are now complete.
        // Both run every iteration, so the whole granted circle arrives while
        // the player stands still; only the pump's pace bounds them.
        translate::pump_requests(&mut ctx, queued.len());
        for event in translate::emit_ready(&mut ctx) {
            let _ = events.send(event);
        }
        translate::sweep_columns(&mut ctx);
        if last_water.elapsed() >= std::time::Duration::from_millis(50) {
            translate::refresh_water(&ctx, bridge);
            last_water = std::time::Instant::now();
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
                    key: None,
                    from_server: false,
                });
                return;
            }
        }
        // Translate more packets when a backlog built up, so received entities,
        // chunks and chat are not seconds behind. The budget still bounds each
        // iteration so the WebRTC pump is serviced.
        let budget = if queued.len() > 16 {
            TRANSLATE_PER_TICK * 3
        } else {
            TRANSLATE_PER_TICK
        };
        let translating = std::time::Instant::now();
        for _ in 0..budget {
            // One column can take tens of milliseconds; come back to the
            // outbound direction and the pump rather than piling more on.
            if translating.elapsed() >= TRANSLATE_BUDGET {
                break;
            }
            let Some(packet) = queued.pop_front() else {
                break;
            };
            for event in translate::clientbound(&mut ctx, &**bridge, &packet) {
                let _ = events.send(event);
            }
            // Block uses were queued by the game thread for this one, which
            // holds the column data the transaction needs.
            for u in bridge.pending_use.lock().unwrap().drain(..) {
                let out = translate::use_item_on(&mut ctx, &**bridge, &u);
                if !out.is_empty() {
                    let _ = client.send(&out);
                }
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
                bridge
                    .unique_id
                    .store(g.entity_unique_id, Ordering::Relaxed);
                // The input tick is the server's clock: corrections quote it,
                // and a client-side counter starting at one makes every
                // correction refer to a tick the server rewind cannot place.
                bridge
                    .input_tick
                    .store(g.current_tick.max(1), Ordering::Relaxed);
                // Ask for what the game's setting wants; the grant decides what
                // is actually streamed (and `ChunkRadiusUpdated` reports it).
                let _ = client.request_chunks(
                    bridge.requested_radius.load(Ordering::Relaxed).max(2),
                );
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
        key: None,
        from_server: false,
    });
    let _ = name;
}

// -------------------------------------------------------------------------
// Translation: Bedrock packets → Horizon's Java clientbound packets.
// -------------------------------------------------------------------------
mod translate {
    use super::{
        Bridge, COLUMN_PATIENCE, EMIT_BURST, EMIT_PER_SECOND, Event, Items, OpenWindow,
        RECEIVE_QUEUE_LIMIT, REGION_PATIENCE, REQUEST_INTERVAL, REQUESTS_PER_TICK, REQUEST_WINDOW,
        RUNTIME_ID_RADIUS, SWEEP_INTERVAL, WindowKind,
    };
    use bedrock_core::protocol as bp;
    use crate::chunk::{
        BIOME_STRATEGY, Chunk, Section as ClientSection, Light as ClientLight,
    };
    use crate::light::{
        LightChanges, LightTables, apply_lighting, apply_relighting, light_chunk,
    };
    use horizon_core::paletted::PalettedContainer;
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
        /// The dimension and game type as `StartGame` reported them, reused for
        /// the Java respawn that closes the death screen.
        pub spawn_info: Option<play::SpawnInfo>,
        pub spawn_yaw: f32,
        pub spawn_pitch: f32,
        /// How many times the position has been repeated since `StartGame`.
        pub published: u8,
        /// The columns received so far, as the client's own chunks: the same
        /// type the renderer streams and the light engine reads. A partial
        /// column starts empty and fills as the sub-chunk replies arrive.
        pub chunks: std::collections::HashMap<(i32, i32), Chunk>,
        /// The Bedrock runtime id of every block the columns hold, per section.
        /// The game's states cannot be mapped back to them reliably, and a
        /// block use has to name the exact one the server has.
        pub block_ids: std::collections::HashMap<(i32, i32), Vec<Option<Box<[u32; 4096]>>>>,
        /// Each entity's Java identifier, for the sound that belongs to it.
        pub kinds: std::collections::HashMap<u64, String>,
        /// Sub-chunk requests queued while translating; the session loop sends
        /// them.
        pub requests: Vec<bedrock_core::protocol::Out>,
        /// Columns still to ask for (the next section index to request), in the
        /// order the server published them.
        pub wanted: std::collections::VecDeque<((i32, i32), i8)>,
        /// The columns named by an entry of `wanted`, so one is never queued
        /// twice, and how many requests are waiting for a reply.
        pub queued: std::collections::HashSet<(i32, i32)>,
        pub pending_requests: usize,
        /// Each column's section bookkeeping while it streams in. A column is
        /// sent to the game once, after every section it was asked for has been
        /// answered and its neighbourhood is finished, so the game never
        /// re-unpacks or re-lights a column that is still filling.
        pub columns: std::collections::HashMap<(i32, i32), ColumnState>,
        /// The column centre the server last published.
        pub wanted_center: Option<(i32, i32)>,
        /// The centre last sent to the game in `SetChunkCacheCenter`. The game
        /// drops a column outside the storage around it, so it has to follow a
        /// move (and a teleport) before the destination columns arrive.
        pub sent_center: Option<(i32, i32)>,
        /// The radius the bridge streams, in chunks: `min(game setting, grant)`.
        pub radius: i32,
        /// The radius the game was last told (in `Login` or
        /// `SetChunkCacheRadius`), so a change is sent exactly once. Without
        /// this the game keeps an old storage size and drops the columns the
        /// bridge streams past it.
        pub sent_radius: Option<i32>,
        /// When the last batch of sub-chunk requests went out, and when the
        /// columns were last swept for a reply the server dropped.
        pub last_request: Option<std::time::Instant>,
        pub last_sweep: Option<std::time::Instant>,
        /// Columns that may be ready to send, nearest first, and the pacing
        /// budget of how many may go out now.
        pub ready: std::collections::BinaryHeap<std::cmp::Reverse<(i64, i32, i32)>>,
        pub emit_budget: f32,
        pub last_emit: Option<std::time::Instant>,
        /// Where each entity was last put: a Bedrock move carries only the
        /// components that changed, and the client takes absolute positions.
        pub entities: std::collections::HashMap<u64, ([f32; 3], f32, f32, f32)>,
        /// The runtime ids the server spawned as players, so a hurt sound can
        /// pick the player's own hurt sound.
        pub players: std::collections::HashSet<u64>,
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
    /// A cache entry that has not been resolved yet (no block state uses it).
    const UNRESOLVED: u32 = u32::MAX;

    pub struct Blocks {
        registry: horizon_world::registry::BlockRegistry,
        table: bedrock_core::blockstates::BlockStates,
        /// `java()` for every runtime id, resolved on first use. A flat table
        /// rather than a map: a column resolves 4096 blocks a section, so the
        /// hashing was visible in the streaming rate. `UNRESOLVED` means "not
        /// looked up yet".
        cache: Vec<u32>,
        /// The other direction, built when a block use needs the runtime id of
        /// the block that was clicked.
        reverse: Option<std::collections::HashMap<u32, i32>>,
        pub air: u32,
        /// The runtime id of air in the block palette. A sub-chunk's layers are
        /// filled from the bottom up, and a layer of air means the block is in
        /// one of the layers above it.
        pub air_runtime: i32,
        /// The game's own light rules: emission, dampening and occlusion per
        /// block state, built from the same prepared resources the client
        /// loads its copy from. `None` only if the block facts or the
        /// occlusion table could not be read at all.
        pub light: Option<LightTables>,
    }

    impl Blocks {
        pub fn new(resources: &std::path::Path) -> Option<Self> {
            let file = std::fs::File::open(resources.join("facts.json")).ok()?;
            let registry = horizon_world::registry::BlockRegistry::from_facts_json(file).ok()?;
            let air = registry.default_state("minecraft:air")?.0;
            let table = bedrock_core::blockstates::BlockStates::bundled();
            let air_runtime = (0..table.len() as i32)
                .find(|id| {
                    matches!(table.get(*id), Some((name, _)) if name == "minecraft:air")
                })
                .unwrap_or(0);
            // The occlusion table is the expensive part; without it, light is
            // still correct for blocks that do not rely on shape occlusion.
            let light = crate::blocks::BlockFacts::load(resources, &registry)
                .ok()
                .map(|facts| {
                    LightTables::load(resources, &registry, &facts)
                        .unwrap_or_else(|_| LightTables::new(&facts))
                });
            if light.is_none() {
                log::warn!("bedrock: no block facts; chunks will be unlit");
            }
            let mut blocks = Self {
                registry,
                table,
                cache: vec![UNRESOLVED],
                reverse: None,
                air,
                air_runtime,
                light,
            };
            // Resolve the whole palette once, here, before the connection:
            // mapping a runtime id to the game's block state is far more
            // expensive the first time (it builds the Bedrock state tag and
            // walks the registry), and doing it lazily spreads those misses
            // through the chunk stream where they bound the streaming rate.
            let started = std::time::Instant::now();
            let cache: Vec<u32> = (0..blocks.table.len() as i32)
                .map(|id| blocks.resolve(id))
                .collect();
            blocks.cache = cache;
            if chunk_trace() {
                bedrock_core::nethernet::trace(format!(
                    "block palette resolved: {} states in {}ms",
                    blocks.cache.len(),
                    started.elapsed().as_millis()
                ));
            }
            Some(blocks)
        }

        /// The game's state id for a Bedrock runtime id.
        pub fn java(&mut self, runtime_id: i32) -> u32 {
            let Some(slot) = usize::try_from(runtime_id)
                .ok()
                .filter(|slot| *slot < self.cache.len())
            else {
                return self.air;
            };
            let cached = self.cache[slot];
            if cached != UNRESOLVED {
                return cached;
            }
            let id = self.resolve(runtime_id);
            self.cache[slot] = id;
            id
        }

        /// The Bedrock runtime id for one of the game's block states, which is
        /// what a block-use transaction reports as the block it clicked.
        pub fn bedrock(&mut self, java: u32) -> u32 {
            if self.reverse.is_none() {
                let mut map = std::collections::HashMap::new();
                for id in 0..self.table.len() as i32 {
                    let state = self.resolve(id);
                    map.entry(state).or_insert(id);
                }
                self.reverse = Some(map);
            }
            self.reverse
                .as_ref()
                .and_then(|map| map.get(&java).copied())
                .unwrap_or(0) as u32
        }

        /// Whether a Java state is a water block.
        pub fn is_water(&self, state: u32) -> bool {
            self.registry
                .state(horizon_world::registry::BlockStateId(state))
                .is_some_and(|state| state.name == "minecraft:water")
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

    /// The client's own column for (x, z), created on the first section.
    fn new_chunk(x: i32, z: i32, biome: i32) -> Chunk {
        let mut chunk = Chunk::with_layout(x, z, crate::chunk::ColumnLayout::VANILLA);
        for section in &mut chunk.sections {
            Arc::make_mut(section).biomes = PalettedContainer::new(BIOME_STRATEGY, biome as u32);
        }
        chunk
    }

    /// Hold an empty column, so a column whose sections are all air still has
    /// blocks (air) for its neighbours to light against and still reaches the
    /// game.
    fn ensure_chunk(ctx: &mut Context, x: i32, z: i32) {
        let biome = ctx.biome;
        ctx.chunks
            .entry((x, z))
            .or_insert_with(|| new_chunk(x, z, biome));
    }

    /// Store one Bedrock sub-chunk into the client's column, translating each
    /// block to the game's own state id. `ids` is in `java_index` order.
    fn put_section(ctx: &mut Context, x: i32, z: i32, y: i32, ids: &[u32]) -> bool {
        let dimension = DimensionType::vanilla("minecraft:overworld");
        let Some(slot) = usize::try_from((y * 16 - dimension.min_y) / 16)
            .ok()
            .filter(|slot| *slot < dimension.sections())
        else {
            return false;
        };
        let biome = ctx.biome;
        let Some(blocks) = ctx.blocks.as_mut() else {
            return false;
        };
        let mut values = [0u32; 4096];
        for (value, runtime_id) in values.iter_mut().zip(ids) {
            *value = blocks.java(*runtime_id as i32);
        }
        if ids.len() == 4096 && keeps_runtime_ids(ctx, x, z) {
            let mut raw = Box::new([0u32; 4096]);
            raw.copy_from_slice(ids);
            let entry = ctx.block_ids.entry((x, z)).or_default();
            if entry.len() <= slot {
                entry.resize_with(slot + 1, || None);
            }
            entry[slot] = Some(raw);
        }
        // Already in its smallest configuration: `from_blocks` builds the
        // palette from the values, the biomes are one value and the light is
        // uniform, so `compact()` here would only walk 4096 entries to conclude
        // the same thing.
        let section = ClientSection {
            blocks: ClientSection::from_blocks(&values),
            biomes: PalettedContainer::new(BIOME_STRATEGY, biome as u32),
            light: ClientLight::Uniform(0),
        };
        let chunk = ctx
            .chunks
            .entry((x, z))
            .or_insert_with(|| new_chunk(x, z, biome));
        if let Some(target) = chunk.sections.get_mut(slot) {
            *target = Arc::new(section);
        }
        true
    }

    /// A block's own runtime id, straight from what the server sent.
    fn block_id_at(ctx: &Context, pos: [i32; 3]) -> Option<u32> {
        let dimension = DimensionType::vanilla("minecraft:overworld");
        let slot = usize::try_from((pos[1] - dimension.min_y).div_euclid(16)).ok()?;
        let entry = ctx.block_ids.get(&(pos[0] >> 4, pos[2] >> 4))?;
        let section = entry.get(slot)?.as_ref()?;
        Some(section[(((pos[1] & 15) << 8) | ((pos[2] & 15) << 4) | (pos[0] & 15)) as usize])
    }

    /// Record one changed block's runtime id, so a use of it names it right.
    fn store_block_id(ctx: &mut Context, pos: [i32; 3], id: u32) {
        let dimension = DimensionType::vanilla("minecraft:overworld");
        let Ok(slot) = usize::try_from((pos[1] - dimension.min_y).div_euclid(16)) else {
            return;
        };
        let entry = ctx.block_ids.entry((pos[0] >> 4, pos[2] >> 4)).or_default();
        if entry.len() <= slot {
            entry.resize_with(slot + 1, || None);
        }
        let section = entry[slot].get_or_insert_with(|| Box::new([0u32; 4096]));
        section[(((pos[1] & 15) << 8) | ((pos[2] & 15) << 4) | (pos[0] & 15)) as usize] = id;
    }

    /// Ask the server to respawn after a death. A real client does this from
    /// the death screen; doing it here as well keeps a dead player from being
    /// stuck when the button's command is delayed.
    fn request_respawn(ctx: &mut Context, bridge: &Bridge) {
        if ctx.runtime_id == 0 || bridge.respawn_asked.swap(true, Ordering::Relaxed) {
            return;
        }
        ctx.requests.push(bp::Out::Respawn {
            position: [0.0; 3],
            state: 2,
            runtime_id: ctx.runtime_id,
        });
    }

    /// Whether the player's body is submerged, sampled on the network thread.
    /// This only decides the swimming input flag; it never moves the player.
    pub fn refresh_water(ctx: &Context, bridge: &Bridge) {
        let feet = *bridge.position.lock().unwrap();
        let Some(blocks) = ctx.blocks.as_ref() else {
            return;
        };
        let min_x = (feet[0] - 0.3).floor() as i32;
        let max_x = (feet[0] + 0.3).floor() as i32;
        let min_z = (feet[2] - 0.3).floor() as i32;
        let max_z = (feet[2] + 0.3).floor() as i32;
        let min_y = (feet[1] + 0.4).floor() as i32;
        let max_y = (feet[1] + 1.4).floor() as i32;
        let mut water = false;
        'search: for x in min_x..=max_x {
            for z in min_z..=max_z {
                let Some(chunk) = ctx.chunks.get(&(x >> 4, z >> 4)) else {
                    continue;
                };
                for y in min_y..=max_y {
                    if blocks.is_water(chunk.get(x & 15, y, z & 15)) {
                        water = true;
                        break 'search;
                    }
                }
            }
        }
        bridge.in_water.store(water, Ordering::Relaxed);
    }

    /// One column as the Java client wants it: every section, its heightmaps
    /// and the light the game's own engine computes for it. The column is lit
    /// over its 3x3 neighbourhood, so light crosses chunk borders exactly as
    /// the renderer expects.
    fn column(ctx: &mut Context, x: i32, z: i32) -> Option<play::LevelChunkWithLight> {
        if !ctx.chunks.contains_key(&(x, z)) {
            return None;
        }
        let air = ctx.blocks.as_ref()?.air;
        let lights = {
            let tables = ctx.blocks.as_ref()?.light.as_ref()?;
            let region: [[Option<&Chunk>; 3]; 3] = std::array::from_fn(|dz| {
                std::array::from_fn(|dx| {
                    ctx.chunks
                        .get(&(x + dx as i32 - 1, z + dz as i32 - 1))
                })
            });
            light_chunk(&region, tables, true)
        };
        let chunk = ctx.chunks.get_mut(&(x, z))?;
        apply_lighting(chunk, lights);
        chunk.lit = true;
        Some(chunk_packet(chunk, ctx.biome, air))
    }

    /// The packed light of one section as a DataLayer (two nibbles a byte).
    fn light_layer(light: &ClientLight, sky: bool) -> Vec<u8> {
        let at = |packed: u8| if sky { packed >> 4 } else { packed & 15 };
        match light {
            ClientLight::Uniform(packed) => {
                let value = at(*packed);
                vec![value | value << 4; 2048]
            }
            ClientLight::Sky(data, block) => {
                if sky {
                    data.to_vec()
                } else {
                    let value = *block;
                    vec![value | value << 4; 2048]
                }
            }
            ClientLight::Block(data, sky_light) => {
                if sky {
                    let value = *sky_light;
                    vec![value | value << 4; 2048]
                } else {
                    data.to_vec()
                }
            }
            ClientLight::Full(data) => data
                .chunks_exact(2)
                .map(|pair| at(pair[0]) | at(pair[1]) << 4)
                .collect(),
        }
    }

    /// Every light section of a freshly lit column. Light section `b` holds
    /// block section `b - 1`; the two around the column are dark below and
    /// open sky above.
    fn full_light(chunk: &Chunk) -> LightData {
        let light_sections = chunk.sections.len() + 2;
        let mut sky = Vec::with_capacity(light_sections);
        let mut block = Vec::with_capacity(light_sections);
        sky.push(vec![0u8; 2048]);
        block.push(vec![0u8; 2048]);
        for section in &chunk.sections {
            sky.push(light_layer(&section.light, true));
            block.push(light_layer(&section.light, false));
        }
        sky.push(vec![0xF0u8; 2048]);
        block.push(vec![0u8; 2048]);
        LightData {
            // Every section carries light, so both masks are full.
            sky_mask: bitset(light_sections),
            block_mask: bitset(light_sections),
            empty_sky_mask: Vec::new(),
            empty_block_mask: Vec::new(),
            sky,
            block,
        }
    }

    /// Set one bit, as the packet's little-endian `BitSet` codec expects.
    fn set_bit(mask: &mut Vec<u8>, bit: usize) {
        let byte = bit / 8;
        if mask.len() <= byte {
            mask.resize(byte + 1, 0);
        }
        mask[byte] |= 1 << (bit % 8);
    }

    /// A light update naming exactly `sections` (block-section indices).
    fn changed_light(chunk: &Chunk, sections: &[i32]) -> LightData {
        let mut sky = Vec::with_capacity(sections.len());
        let mut block = Vec::with_capacity(sections.len());
        let mut sky_mask = Vec::new();
        let mut block_mask = Vec::new();
        for &section in sections {
            let Ok(index) = usize::try_from(section) else {
                continue;
            };
            let Some(section) = chunk.sections.get(index) else {
                continue;
            };
            // Light section b holds block section b - 1.
            let b = index + 1;
            set_bit(&mut sky_mask, b);
            set_bit(&mut block_mask, b);
            sky.push(light_layer(&section.light, true));
            block.push(light_layer(&section.light, false));
        }
        LightData {
            sky_mask,
            block_mask,
            empty_sky_mask: Vec::new(),
            empty_block_mask: Vec::new(),
            sky,
            block,
        }
    }

    /// The Java column packet for a lit client chunk.
    ///
    /// The sections go out as the paletted containers the column already holds
    /// (`to_wire`), not as values run back through `Paletted::from_values`: a
    /// column is 98,304 block states, and re-paletting all of them a second
    /// time was most of the cost of streaming one. The heightmap is taken top
    /// down, so the air above the ground is never walked and the scan stops as
    /// soon as every column has its surface.
    fn chunk_packet(chunk: &Chunk, biome: i32, air: u32) -> play::LevelChunkWithLight {
        let dimension = DimensionType::vanilla("minecraft:overworld");
        // WORLD_SURFACE (1) and MOTION_BLOCKING (4); this decoder does not
        // distinguish them, since it sees no fluids or leaf shapes.
        let mut heights = [0i32; 256];
        let mut unset = 256usize;
        for (slot, section) in chunk.sections.iter().enumerate().rev() {
            if unset == 0 {
                break;
            }
            if let Some(value) = section.blocks.uniform() {
                if value == air {
                    continue;
                }
                let surface = (slot * 16 + 16) as i32;
                for height in &mut heights {
                    if *height == 0 {
                        *height = surface;
                        unset -= 1;
                    }
                }
                continue;
            }
            let mut values = [0u32; 4096];
            section.blocks.unpack(&mut values);
            for y in (0..16).rev() {
                if unset == 0 {
                    break;
                }
                let base = y << 8;
                for column in 0..256 {
                    if heights[column] != 0 || values[base | column] == air {
                        continue;
                    }
                    heights[column] = (slot * 16 + y + 1) as i32;
                    unset -= 1;
                }
            }
        }
        let mut sections = Vec::with_capacity(chunk.sections.len());
        for section in &chunk.sections {
            let non_empty = match section.blocks.uniform() {
                Some(value) if value == air => 0,
                Some(_) => 4096,
                None => {
                    let mut values = [0u32; 4096];
                    section.blocks.unpack(&mut values);
                    values.iter().filter(|value| **value != air).count() as i16
                }
            };
            let (bits, palette, words) = section.blocks.to_wire();
            sections.push(Section {
                non_empty,
                fluids: 0,
                states: Paletted {
                    bits,
                    palette: palette.into_iter().map(|value| value as i32).collect(),
                    data: words.iter().map(|&word| word as i64).collect(),
                },
                biomes: Paletted::from_values(&[biome; 64], Strategy::Biomes, 7),
            });
        }
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
        play::LevelChunkWithLight {
            x: chunk.x,
            z: chunk.z,
            chunk: ChunkData {
                heightmaps,
                buffer: ChunkData::from_sections(&sections),
                block_entities: Vec::new(),
            },
            light: full_light(chunk),
        }
    }

    /// The columns a block change can reach light into.
    fn columns_around(pos: [i32; 3]) -> impl Iterator<Item = (i32, i32)> {
        let (cx, cz) = (pos[0] >> 4, pos[2] >> 4);
        (-1..=1).flat_map(move |dz| (-1..=1).map(move |dx| (cx + dx, cz + dz)))
    }

    /// Put a block into the client's cached column, if that column is loaded.
    fn set_block(ctx: &mut Context, pos: [i32; 3], state: u32) {
        if let Some(chunk) = ctx.chunks.get_mut(&(pos[0] >> 4, pos[2] >> 4)) {
            chunk.set(pos[0] & 15, pos[1], pos[2] & 15, state);
        }
    }

    /// Re-light `columns` from the blocks they hold now and return the light
    /// updates for the sections that actually changed. Only columns the client
    /// has already been sent are touched.
    fn relight_columns(
        ctx: &mut Context,
        columns: &std::collections::BTreeSet<(i32, i32)>,
    ) -> Vec<Event> {
        let Some(tables) = ctx.blocks.as_ref().and_then(|blocks| blocks.light.as_ref()) else {
            return Vec::new();
        };
        let mut changed = LightChanges::default();
        for &(x, z) in columns {
            if !ctx.chunks.get(&(x, z)).is_some_and(|chunk| chunk.lit) {
                continue;
            }
            let lights = {
                let region: [[Option<&Chunk>; 3]; 3] = std::array::from_fn(|dz| {
                    std::array::from_fn(|dx| {
                        ctx.chunks
                            .get(&(x + dx as i32 - 1, z + dz as i32 - 1))
                    })
                });
                light_chunk(&region, tables, true)
            };
            if let Some(chunk) = ctx.chunks.get_mut(&(x, z)) {
                apply_relighting(chunk, lights, &mut changed);
            }
        }
        let mut by_chunk: std::collections::BTreeMap<(i32, i32), Vec<i32>> =
            std::collections::BTreeMap::new();
        for (x, section, z) in changed.sections() {
            by_chunk.entry((x, z)).or_default().push(section);
        }
        let mut events = Vec::new();
        for ((x, z), mut sections) in by_chunk {
            let Some(chunk) = ctx.chunks.get(&(x, z)) else {
                continue;
            };
            sections.sort_unstable();
            sections.dedup();
            events.push(Event::Packet(Box::new(play::Clientbound::LightUpdate(
                play::LightUpdate {
                    x,
                    z,
                    light: changed_light(chunk, &sections),
                },
            ))));
        }
        events
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

    /// The Java command tree for the server's list: a root whose children are
    /// the command names (and their aliases), and under each a chain of
    /// literal subcommands and typed arguments, one chain per overload. Chains
    /// that share a prefix share nodes, and a node is executable where a
    /// command could end — at the end of an overload and before an optional
    /// parameter.
    fn command_tree(commands: &bp::AvailableCommands) -> play::Commands {
        fn literal(name: &str) -> play::CommandNode {
            play::CommandNode {
                kind: 1,
                executable: false,
                restricted: false,
                children: Vec::new(),
                redirect: None,
                name: name.to_owned(),
                parser: None,
                suggestions: None,
            }
        }
        /// The Java argument type and its property bytes for a Bedrock
        /// parameter, by the enum's name or by the built-in type id.
        fn parser(kind: Option<&str>, symbol: u32) -> (i32, Vec<u8>) {
            // Java's `minecraft:command_argument_type` ids, by name.
            let id = |name: &str| {
                play::ARGUMENT_TYPES
                    .iter()
                    .position(|n| *n == name)
                    .map_or(5, |i| i as i32)
            };
            match kind {
                Some("Boolean") => (id("brigadier:bool"), Vec::new()),
                Some("Float") => (id("brigadier:float"), vec![0]),
                Some("GameMode") => (id("minecraft:gamemode"), Vec::new()),
                Some("Item") => (id("minecraft:item_stack"), Vec::new()),
                Some("Dimension") => (id("minecraft:dimension"), Vec::new()),
                Some("BlockPos") | Some("Position") => (id("minecraft:block_pos"), Vec::new()),
                Some("Target") | Some("Entity") | Some("Selector") | Some("EntityTypeSelector") => {
                    (id("minecraft:entity"), vec![0])
                }
                Some("Message") | Some("Json") => (id("minecraft:message"), Vec::new()),
                Some("Time") => (id("minecraft:time"), vec![0; 4]),
                // Everything else, and the numeric built-ins, behave like an
                // integer or a single word.
                _ => match symbol & 0x0f_ffff {
                    1 | 5 => (id("brigadier:integer"), vec![0]),
                    3 => (id("brigadier:float"), vec![0]),
                    0x0f | 0x33 => (id("brigadier:string"), vec![2]), // greedy
                    _ => (id("brigadier:string"), vec![0]),
                },
            }
        }
        let mut nodes: Vec<play::CommandNode> = Vec::new();
        // Index 0 is the root; every command name hangs off it.
        nodes.push(play::CommandNode {
            kind: 0,
            executable: false,
            restricted: false,
            children: Vec::new(),
            redirect: None,
            name: String::new(),
            parser: None,
            suggestions: None,
        });
        let mut root_children: Vec<i32> = Vec::new();

        /// The child of `from` with the same name and parser, or a new one.
        fn child(
            nodes: &mut Vec<play::CommandNode>,
            from: i32,
            node: play::CommandNode,
        ) -> i32 {
            if let Some(existing) = nodes[from as usize]
                .children
                .iter()
                .find(|c| {
                    let n = &nodes[**c as usize];
                    n.kind == node.kind && n.name == node.name && n.parser.as_ref().map(|p| p.0) == node.parser.as_ref().map(|p| p.0)
                })
                .copied()
            {
                return existing;
            }
            let index = nodes.len() as i32;
            nodes.push(node);
            nodes[from as usize].children.push(index);
            index
        }

        for command in &commands.commands {
            // The command's own subtree: one chain per overload, merged where
            // they share a prefix.
            let mut children: Vec<i32> = Vec::new();
            let mut entries: Vec<Vec<play::CommandNode>> = Vec::new();
            let _ = &mut entries;
            // Temporarily attach to a scratch node so the shared `child`
            // helper can build the chains, then move its children onto the
            // command's literal node.
            let scratch = nodes.len() as i32;
            nodes.push(literal(&command.name));
            for overload in &command.overloads {
                let mut from = scratch;
                for (index, param) in overload.iter().enumerate() {
                    if param.optional {
                        nodes[from as usize].executable = true;
                    }
                    let node = match commands.param_literal(param.symbol) {
                        Some(value) => literal(value),
                        None => {
                            let kind = commands.param_type(param.symbol);
                            let (id, props) = parser(kind, param.symbol);
                            play::CommandNode {
                                kind: 2,
                                executable: false,
                                restricted: false,
                                children: Vec::new(),
                                redirect: None,
                                name: param.name.clone(),
                                parser: Some((id, props)),
                                suggestions: None,
                            }
                        }
                    };
                    from = child(&mut nodes, from, node);
                    if index + 1 == overload.len() {
                        nodes[from as usize].executable = true;
                    }
                }
                if overload.is_empty() {
                    nodes[scratch as usize].executable = true;
                }
            }
            children.extend(nodes[scratch as usize].children.clone());
            // The scratch holds the subtree; the real literal reuses it.
            nodes[scratch as usize].children = Vec::new();
            let index = child(&mut nodes, 0, literal(&command.name));
            nodes[index as usize].children = children;
            nodes[index as usize].executable = nodes[scratch as usize].executable;
            root_children.push(index);
            // Aliases share the subtree.
            if let Ok(alias) = usize::try_from(command.alias)
                && let Some(alias_enum) = commands.enums.get(alias)
            {
                for value in &alias_enum.values {
                    let Some(name) = commands.enum_values.get(*value as usize) else {
                        continue;
                    };
                    let index = child(&mut nodes, 0, literal(name));
                    nodes[index as usize].children = nodes[root_children[root_children.len() - 1] as usize]
                        .children
                        .clone();
                    nodes[index as usize].executable =
                        nodes[root_children[root_children.len() - 1] as usize].executable;
                }
            }
        }
        nodes[0].children = root_children;
        play::Commands { nodes, root: 0 }
    }

    /// One `CommandOutput` message as chat lines. BDS prefixes the key with a
    /// colour code and a `%` (`"§2%commands.help.header"`), so the code is kept,
    /// the key is looked up in the game's language table and the values the
    /// message carries are substituted in. A single message can hold several
    /// lines (the help body does).
    fn command_lines(message: &bp::CommandMessage) -> Vec<Event> {
        let (colour, rest) = match message.message.strip_prefix('§') {
            Some(tail) => {
                let split = tail.char_indices().nth(1).map_or(tail.len(), |(i, _)| i);
                let (code, rest) = tail.split_at(split);
                (format!("§{code}"), rest)
            }
            None => (String::new(), message.message.as_str()),
        };
        let key = rest.strip_prefix('%').unwrap_or(rest);
        let mut text = if key.contains('.') {
            crate::lang::get_or(key, key)
        } else {
            key.to_owned()
        };
        for (index, value) in message.parameters.iter().enumerate() {
            let exact = format!("%{}$s", index + 1);
            if text.contains(&exact) {
                text = text.replace(&exact, value);
            } else if text.contains("%s") {
                text = text.replacen("%s", value, 1);
            } else {
                text.push(' ');
                text.push_str(value);
            }
        }
        format!("{colour}{text}")
            .split('\n')
            .map(|line| {
                Event::Packet(Box::new(play::Clientbound::SystemChat(play::SystemChat {
                    content: text_component(line.trim_end()),
                    overlay: false,
                })))
            })
            .collect()
    }

    fn text_component(message: &str) -> JTag {
        JTag::compound([("text", JTag::String(JavaString::from(message)))])
    }

    /// A positional hurt sound at a hurt entity, since the game's sound-event
    /// handler takes a position rather than an entity. Bedrock's own level
    /// sound event for the same hurt is not decoded.
    fn hurt_sound(ctx: &Context, id: u64, position: [f32; 3]) -> Event {
        sound_at(&mob_sound(ctx, id, "hurt"), position)
    }

    /// A positional death sound. The game only plays one by itself for the
    /// local player, so any other dead entity would go quietly.
    fn death_sound(ctx: &Context, id: u64, position: [f32; 3]) -> Event {
        sound_at(&mob_sound(ctx, id, "death"), position)
    }

    /// Bedrock's mobs each have their own hurt and death sounds; the game's
    /// names follow the entity's own Java id.
    fn mob_sound(ctx: &Context, id: u64, verb: &str) -> String {
        if ctx.players.contains(&id) {
            return format!("minecraft:entity.player.{verb}");
        }
        match ctx.kinds.get(&id) {
            Some(kind) => {
                let path = kind.strip_prefix("minecraft:").unwrap_or(kind);
                format!("minecraft:entity.{path}.{verb}")
            }
            None => format!("minecraft:entity.generic.{verb}"),
        }
    }

    fn sound_at(name: &str, position: [f32; 3]) -> Event {
        Event::Packet(Box::new(play::Clientbound::Sound(play::Sound {
            sound: (-1, Some((name.to_owned(), None))),
            source: crate::settings::sound_source(name) as i32,
            x: (f64::from(position[0]) * 8.0) as i32,
            y: (f64::from(position[1]) * 8.0) as i32,
            z: (f64::from(position[2]) * 8.0) as i32,
            volume: 1.0,
            pitch: 1.0,
            seed: 0,
        })))
    }

    /// One column's sections while it streams in. The game is sent the column
    /// once, after every section asked for has been answered, so it never has
    /// to unpack and light a column that is still filling.
    #[derive(Clone)]
    pub struct ColumnState {
        /// The absolute Bedrock section indices asked for.
        pub requested: std::collections::BTreeSet<i32>,
        /// The absolute indices the server answered, with blocks or all air.
        pub answered: std::collections::BTreeSet<i32>,
        /// The next section index to ask for, and the last one the server
        /// named for this column.
        pub next_section: i32,
        pub section_end: i32,
        /// Whether the whole column's request range has been queued.
        pub complete_request: bool,
        /// Whether the column has been sent to the game.
        pub sent: bool,
        /// Whether its light was computed before a neighbour arrived, so it
        /// still needs a light update when that neighbour is emitted.
        pub relight: bool,
        /// Whether a section request for it is waiting for its reply.
        pub pending: bool,
        /// When the column was last asked for, so a reply the server drops
        /// cannot hold the column (and its neighbours) back forever.
        pub asked: Option<std::time::Instant>,
    }

    impl ColumnState {
        fn new(first_section: i32, last_section: i32) -> Self {
            Self {
                requested: std::collections::BTreeSet::new(),
                answered: std::collections::BTreeSet::new(),
                next_section: first_section,
                section_end: last_section,
                complete_request: false,
                sent: false,
                relight: false,
                pending: false,
                asked: None,
            }
        }
    }

    /// Whether per-column tracing is on (`HZ_CHUNK_TRACE=1`). Cached, since it
    /// is read once per column.
    fn chunk_trace() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("HZ_CHUNK_TRACE").is_some())
    }

    /// The overworld's absolute sub-chunk indices: `min_y / 16` through one
    /// less than the section count (`-4..=19`).
    fn section_range() -> (i32, i32) {
        let dimension = DimensionType::vanilla("minecraft:overworld");
        let first = dimension.min_y.div_euclid(16);
        (first, first + dimension.sections() as i32 - 1)
    }

    /// Whether a column is inside the circle the server publishes around the
    /// player. Bedrock's chunk radius is a distance, not a count:
    /// `dx² + dz² <= r²`.
    fn in_circle(ctx: &Context, x: i32, z: i32) -> bool {
        Circle::of(ctx).is_none_or(|circle| circle.contains(x, z))
    }

    /// The area the bridge streams around the player. Its radius is one bigger
    /// than the game's render distance, because a column can only be lit and
    /// meshed once its neighbours are here: the extra ring is what makes the
    /// outermost rendered ring complete, and the game's own LAN host streams
    /// `view_distance + 1` for the same reason.
    #[derive(Clone, Copy)]
    struct Circle {
        cx: i32,
        cz: i32,
        r2: i64,
    }

    impl Circle {
        fn of(ctx: &Context) -> Option<Circle> {
            let (cx, cz) = ctx.wanted_center?;
            let r = i64::from(ctx.radius.max(2) + 1);
            Some(Circle {
                cx,
                cz,
                r2: r * r,
            })
        }

        fn contains(self, x: i32, z: i32) -> bool {
            let dx = i64::from(x - self.cx);
            let dz = i64::from(z - self.cz);
            dx * dx + dz * dz <= self.r2
        }
    }

    /// Whether a column is close enough to the player to keep its section
    /// runtime ids (see `RUNTIME_ID_RADIUS`).
    fn keeps_runtime_ids(ctx: &Context, x: i32, z: i32) -> bool {
        ctx.wanted_center.is_none_or(|(cx, cz)| {
            (x - cx).abs() <= RUNTIME_ID_RADIUS && (z - cz).abs() <= RUNTIME_ID_RADIUS
        })
    }

    /// Whether a column has everything it was asked for (or has waited long
    /// enough that waiting more would only delay the rest of the world).
    fn column_finished(ctx: &Context, x: i32, z: i32) -> bool {
        let Some(state) = ctx.columns.get(&(x, z)) else {
            return false;
        };
        if state.sent {
            return true;
        }
        if !state.complete_request {
            return false;
        }
        state.requested.iter().all(|y| state.answered.contains(y))
            || state
                .asked
                .is_some_and(|at| at.elapsed() >= COLUMN_PATIENCE)
    }

    /// Drop everything held for a column (its blocks, its section state and any
    /// queued request), so a stale column is neither drawn nor asked for.
    pub fn forget_column(ctx: &mut Context, x: i32, z: i32) {
        if ctx.columns.remove(&(x, z)).is_some_and(|state| state.pending) {
            ctx.pending_requests = ctx.pending_requests.saturating_sub(1);
        }
        ctx.queued.remove(&(x, z));
        ctx.wanted.retain(|((cx, cz), _)| (*cx, *cz) != (x, z));
        ctx.chunks.remove(&(x, z));
        ctx.block_ids.remove(&(x, z));
    }

    /// Queue a column's next section batch, if it still needs one. The queue is
    /// filled as the server publishes columns, not with a precomputed ring: a
    /// column the server never sent cannot be requested, and a teleport's
    /// destination is queued the moment its `LevelChunk` arrives.
    pub fn queue_column(ctx: &mut Context, x: i32, z: i32) {
        let Some(state) = ctx.columns.get(&(x, z)) else {
            return;
        };
        if state.sent || state.complete_request || ctx.queued.contains(&(x, z)) {
            return;
        }
        let from = state.next_section;
        if from > state.section_end {
            if let Some(state) = ctx.columns.get_mut(&(x, z)) {
                state.complete_request = true;
            }
            return;
        }
        ctx.wanted.push_back(((x, z), from as i8));
        ctx.queued.insert((x, z));
    }

    /// Send the queued sub-chunk requests, paced so the WebRTC pump and the
    /// translation queue keep up. It runs every loop iteration, not only when
    /// the publisher moves: at a 32-chunk radius most of the circle is fetched
    /// while the player stands still.
    pub fn pump_requests(ctx: &mut Context, backlog: usize) {
        if ctx.wanted.is_empty() || backlog > RECEIVE_QUEUE_LIMIT {
            return;
        }
        if ctx.pending_requests >= REQUEST_WINDOW {
            return;
        }
        let now = std::time::Instant::now();
        if ctx
            .last_request
            .is_some_and(|last| now.duration_since(last) < REQUEST_INTERVAL)
        {
            return;
        }
        ctx.last_request = Some(now);
        // Send only into the free part of the window: the requests already in
        // flight are the work the server is doing for us.
        let room = REQUEST_WINDOW.saturating_sub(ctx.pending_requests);
        for _ in 0..REQUESTS_PER_TICK.min(room) {
            let Some((column, from)) = ctx.wanted.pop_front() else {
                break;
            };
            ctx.queued.remove(&column);
            let (x, z) = column;
            let done = column_finished(ctx, x, z);
            let Some(state) = ctx.columns.get_mut(&(x, z)) else {
                continue;
            };
            if state.sent || done {
                continue;
            }
            // One request per column: the sections from the bottom up to the
            // highest one the server named. Requesting them one at a time made
            // a 32-chunk circle take minutes.
            let from = i32::from(from);
            let to = state.section_end;
            if from > to {
                state.complete_request = true;
                continue;
            }
            for y in from..=to {
                state.requested.insert(y);
            }
            state.next_section = to + 1;
            state.asked = Some(now);
            state.complete_request = true;
            if !state.pending {
                state.pending = true;
                ctx.pending_requests += 1;
            }
            let offsets: Vec<[i8; 3]> = (from..=to).map(|y| [0i8, y as i8, 0]).collect();
            if chunk_trace() {
                bedrock_core::nethernet::trace(format!(
                    "chunk req ({x},{z}) sections {from}..={to}"
                ));
            }
            ctx.requests.push(bp::Out::SubChunkRequest {
                dimension: 0,
                center: [x, 0, z],
                offsets,
            });
        }
    }

    /// Rebuild the streamed set after the radius changed: forget what is now
    /// out of range and ask for the columns already received that now fit.
    pub fn radius_changed(ctx: &mut Context) {
        let Some(circle) = Circle::of(ctx) else {
            return;
        };
        ctx.wanted
            .retain(|((x, z), _)| circle.contains(*x, *z));
        ctx.queued.retain(|&(x, z)| circle.contains(x, z));
        retain_near(ctx, circle.cx, circle.cz);
        queue_published(ctx);
    }

    /// Keep only the columns near `(cx, cz)`. A long walk (or a teleport) must
    /// not grow the cache without bound, and a column dropped this way is
    /// requested again if the server publishes it again.
    fn retain_near(ctx: &mut Context, cx: i32, cz: i32) {
        let keep = ctx.radius.max(2) + 2;
        let near = |x: i32, z: i32| (x - cx).abs() <= keep && (z - cz).abs() <= keep;
        let mut released = 0usize;
        ctx.columns.retain(|&(x, z), state| {
            let keep_it = near(x, z);
            if !keep_it && state.pending {
                released += 1;
            }
            keep_it
        });
        ctx.pending_requests = ctx.pending_requests.saturating_sub(released);
        ctx.chunks.retain(|&(x, z), _| near(x, z));
        // A column that drifts out of reach no longer needs its runtime ids,
        // and a column that comes into reach keeps its own as it is published.
        let centre = ctx.wanted_center;
        ctx.block_ids.retain(|&(x, z), _| {
            centre.is_none_or(|(cx, cz)| {
                (x - cx).abs() <= RUNTIME_ID_RADIUS && (z - cz).abs() <= RUNTIME_ID_RADIUS
            })
        });
    }

    /// Ask for the columns inside the published circle that have arrived but
    /// have not been sent yet. They may have arrived before the publisher
    /// update that named their centre.
    fn queue_published(ctx: &mut Context) {
        let Some(circle) = Circle::of(ctx) else {
            return;
        };
        let waiting: Vec<(i32, i32)> = ctx
            .columns
            .iter()
            .filter(|(position, state)| {
                !state.sent && circle.contains(position.0, position.1)
            })
            .map(|(&(x, z), _)| (x, z))
            .collect();
        for (x, z) in waiting {
            queue_column(ctx, x, z);
        }
    }

    /// Note that a column finished, so it and the neighbours whose own 3x3 it
    /// may have completed are candidates to be sent. They are queued by
    /// distance and sent at a steady pace: the game gives chunk packets a
    /// millisecond a frame and then, once a backlog is 50 ms old, handles the
    /// whole of it in one frame — so a burst of hundreds of columns is a
    /// second-long frame the player feels as a frozen chat box.
    fn note_finished(ctx: &mut Context, x: i32, z: i32) {
        // A column arriving is not a column finishing: only a finished one (and
        // its neighbours) can be sent, so there is nothing to queue until then.
        if !column_finished(ctx, x, z) {
            return;
        }
        let (cx, cz) = ctx.wanted_center.unwrap_or((x, z));
        for dz in -1..=1 {
            for dx in -1..=1 {
                let (nx, nz) = (x + dx, z + dz);
                let dx = i64::from(nx - cx);
                let dz = i64::from(nz - cz);
                ctx.ready
                    .push(std::cmp::Reverse((dx * dx + dz * dz, nx, nz)));
            }
        }
    }

    /// Whether a column can be sent now: every section asked for is in, and
    /// either its 3x3 neighbourhood is finished too (the light is then final)
    /// or it has waited `REGION_PATIENCE` for a neighbour that never came.
    fn region_complete(ctx: &Context, x: i32, z: i32) -> bool {
        (-1..=1).all(|dz| {
            (-1..=1).all(|dx| {
                column_finished(ctx, x + dx, z + dz) && ctx.chunks.contains_key(&(x + dx, z + dz))
            })
        })
    }

    fn column_sendable(ctx: &Context, x: i32, z: i32) -> Option<bool> {
        if ctx.columns.get(&(x, z)).is_none_or(|state| state.sent) || !column_finished(ctx, x, z)
        {
            return None;
        }
        if region_complete(ctx, x, z) {
            return Some(true);
        }
        // The neighbour may simply still be filling. Only give up on it after a
        // while: a column that never arrives (a reply the server dropped, the
        // edge of the published area) must not leave a hole in the world.
        let waited = ctx
            .columns
            .get(&(x, z))
            .and_then(|state| state.asked)
            .is_some_and(|at| at.elapsed() >= REGION_PATIENCE);
        waited.then_some(false)
    }

    /// Light a column and turn it into the packet, marking it sent. `complete`
    /// says whether its whole 3x3 was here, so a column sent early is
    /// remembered for a light update once the missing neighbour shows up.
    fn emit_column(ctx: &mut Context, x: i32, z: i32, complete: bool) -> Option<Event> {
        if ctx.columns.get(&(x, z)).is_none_or(|state| state.sent) || !column_finished(ctx, x, z) {
            return None;
        }
        let missing: Vec<i32> = match ctx.columns.get(&(x, z)) {
            Some(state) => state
                .requested
                .iter()
                .copied()
                .filter(|y| !state.answered.contains(y))
                .collect(),
            None => return None,
        };
        let started = std::time::Instant::now();
        let chunk = column(ctx, x, z)?;
        let elapsed = started.elapsed();
        if let Some(state) = ctx.columns.get_mut(&(x, z)) {
            state.sent = true;
            // A column lit with a neighbour missing takes a light update when
            // that neighbour is emitted.
            state.relight = !complete;
        }
        if chunk_trace() {
            if missing.is_empty() {
                bedrock_core::nethernet::trace(format!(
                    "chunk emit ({x},{z}) total={}ms{}",
                    elapsed.as_millis(),
                    if complete { "" } else { " early" }
                ));
            } else {
                bedrock_core::nethernet::trace(format!(
                    "chunk emit ({x},{z}) incomplete missing={missing:?}"
                ));
            }
        }
        Some(Event::Packet(Box::new(
            play::Clientbound::LevelChunkWithLight(chunk),
        )))
    }

    /// Send a column that was lit with a neighbour missing again as light only,
    /// now that the neighbour is here. Only the sections whose light changed go
    /// out, and only columns sent early are ever checked.
    fn relight_sent_neighbours(ctx: &mut Context, x: i32, z: i32) -> Vec<Event> {
        let mut pending = std::collections::BTreeSet::new();
        for (nx, nz) in [(x - 1, z), (x + 1, z), (x, z - 1), (x, z + 1)] {
            if ctx.columns.get(&(nx, nz)).is_some_and(|state| state.sent && state.relight) {
                pending.insert((nx, nz));
            }
        }
        if pending.is_empty() {
            return Vec::new();
        }
        for &(nx, nz) in &pending {
            if let Some(state) = ctx.columns.get_mut(&(nx, nz)) {
                state.relight = false;
            }
        }
        relight_columns(ctx, &pending)
    }

    /// Send the nearest column that is ready, while the pacing budget allows.
    /// The column under the player goes first whatever the rest are doing: it
    /// is what the loading screen waits for, and it is what makes the world
    /// visible.
    pub fn emit_ready(ctx: &mut Context) -> Vec<Event> {
        let now = std::time::Instant::now();
        let elapsed = ctx
            .last_emit
            .map_or(0.0, |last| now.duration_since(last).as_secs_f32());
        ctx.last_emit = Some(now);
        ctx.emit_budget = (ctx.emit_budget + elapsed * EMIT_PER_SECOND).min(EMIT_BURST);
        let mut events = Vec::new();
        // The player's own column first, whatever its neighbours are doing: it
        // is what the loading screen waits for, and what makes the world
        // visible at all. If one is missing the light update follows later.
        if let Some((x, z)) = ctx.wanted_center
            && let Some(event) = emit_column(ctx, x, z, region_complete(ctx, x, z))
        {
            events.push(event);
        }
        while ctx.emit_budget >= 1.0 {
            let Some(std::cmp::Reverse((_, x, z))) = ctx.ready.pop() else {
                break;
            };
            let Some(complete) = column_sendable(ctx, x, z) else {
                continue;
            };
            ctx.emit_budget -= 1.0;
            if let Some(event) = emit_column(ctx, x, z, complete) {
                events.push(event);
                events.extend(relight_sent_neighbours(ctx, x, z));
            }
        }
        events
    }

    /// Queue the columns that became sendable without a packet naming them
    /// (a reply the server dropped has now waited long enough), and report the
    /// streaming state once a second under `HZ_CHUNK_TRACE`.
    pub fn sweep_columns(ctx: &mut Context) {
        let now = std::time::Instant::now();
        if ctx
            .last_sweep
            .is_some_and(|last| now.duration_since(last) < SWEEP_INTERVAL)
        {
            return;
        }
        ctx.last_sweep = Some(now);
        if chunk_trace() {
            let total = ctx.columns.len();
            let sent = ctx.columns.values().filter(|s| s.sent).count();
            let waiting = ctx.columns.values().filter(|s| !s.complete_request).count();
            bedrock_core::nethernet::trace(format!(
                "chunks columns={total} sent={sent} waiting={waiting} queued={} inflight={} ready={} radius={} center={:?}",
                ctx.wanted.len(),
                ctx.pending_requests,
                ctx.ready.len(),
                ctx.radius,
                ctx.wanted_center
            ));
        }
        // Only a reply the server never sent needs re-queueing: a column that
        // is merely waiting for a neighbour is queued again when that
        // neighbour completes, which is what `note_finished` is for. Pushing
        // every finished column here would grow the queue without bound.
        let overdue: Vec<(i32, i32)> = ctx
            .columns
            .iter()
            .filter(|(_, state)| {
                !state.sent
                    && state.complete_request
                    && state.asked.is_some_and(|at| at.elapsed() >= COLUMN_PATIENCE)
            })
            .map(|(&(x, z), _)| (x, z))
            .collect();
        for (x, z) in overdue {
            note_finished(ctx, x, z);
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
        let mut events = clientbound_inner(ctx, bridge, packet);
        // Any entity whose death animation has run its course leaves now, on
        // whichever packet comes next.
        let now = bridge.tick.load(Ordering::Relaxed);
        let mut removals = bridge.removals.lock().unwrap();
        if removals.iter().any(|(due, _)| *due <= now) {
            let ids: Vec<i32> = removals
                .iter()
                .filter(|(due, _)| *due <= now)
                .map(|(_, id)| *id as i32)
                .collect();
            removals.retain(|(due, _)| *due > now);
            if std::env::var_os("HZ_ENT_TRACE").is_some() {
                bedrock_core::nethernet::trace(format!("REMOVE ids={ids:?} now={now}"));
            }
            events.push(Event::Packet(Box::new(play::Clientbound::RemoveEntities(
                play::RemoveEntities { ids },
            ))));
        }
        events
    }

    fn clientbound_inner(ctx: &mut Context, bridge: &Bridge, packet: &bp::Packet) -> Vec<Event> {
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
                ctx.spawn_info = Some(spawn.clone());
                *bridge.spawn_info.lock().unwrap() = Some(spawn.clone());
                // The game's chunk storage is centred on the spawn column from
                // here on; the first publisher update only repeats it if the
                // player has already moved away.
                ctx.sent_center = Some((g.position[0] as i32 >> 4, g.position[2] as i32 >> 4));
                // `Login` carries the radius, so it is already known to the game.
                ctx.sent_radius = Some(bridge.chunk_radius.load(Ordering::Relaxed).max(2));
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
                        // The streamed radius: the game's render-distance
                        // setting capped by the server's grant. The game drops
                        // anything past it and sizes its cache from it, so it
                        // is never the publisher's block count (512).
                        chunk_radius: bridge.chunk_radius.load(Ordering::Relaxed),
                        simulation_distance: bridge.chunk_radius.load(Ordering::Relaxed),
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
            // What a command printed, or why it was refused. Bedrock names a
            // translation key and passes the values separately; the game's own
            // table is Java's, so the key is looked up and the values are
            // appended, the same way chat parameters are.
            bp::Packet::CommandOutput(output) => {
                if std::env::var_os("HZ_CMD_TRACE").is_some() {
                    bedrock_core::nethernet::trace(format!(
                        "command output success={} messages={:?}",
                        output.success_count,
                        output
                            .messages
                            .iter()
                            .map(|m| (m.message.as_str(), m.parameters.len()))
                            .collect::<Vec<_>>()
                    ));
                }
                output
                    .messages
                    .iter()
                    .flat_map(command_lines)
                    .collect()
            }
            // The server's command list: the game's command tree, so `/` lists
            // the commands, tab-completion works and an unknown command gets
            // the game's own "did you mean" suggestion.
            bp::Packet::AvailableCommands(commands) => {
                if chunk_trace() {
                    bedrock_core::nethernet::trace(format!(
                        "commands: {} offered, {} enums, {} soft",
                        commands.commands.len(),
                        commands.enums.len(),
                        commands.soft_enums.len()
                    ));
                }
                vec![Event::Packet(Box::new(play::Clientbound::Commands(
                    command_tree(commands),
                )))]
            }
            bp::Packet::SetDifficulty(difficulty) => vec![Event::Packet(Box::new(
                play::Clientbound::ChangeDifficulty(play::ChangeDifficulty {
                    difficulty: *difficulty,
                    locked: true,
                }),
            ))],
            // Bedrock sends health as a signed varint, the hunger bar through
            // the player's attributes (below); Java sends all three at once.
            bp::Packet::SetHealth(health) => {
                let mut vitals = bridge.vitals.lock().unwrap();
                vitals.health = *health as f32;
                let set = play::SetHealth {
                    health: vitals.health,
                    food: vitals.food,
                    saturation: vitals.saturation,
                };
                drop(vitals);
                if *health <= 0 {
                    request_respawn(ctx, bridge);
                }
                vec![Event::Packet(Box::new(play::Clientbound::SetHealth(set)))]
            }
            // The player's hunger and saturation are Bedrock attributes, so a
            // change there is what moves the hunger bar. Health rides along in
            // case the server reports it here instead of in `SetHealth`.
            bp::Packet::UpdateAttributes(attributes) if attributes.runtime_id == ctx.runtime_id => {
                let mut vitals = bridge.vitals.lock().unwrap();
                let mut changed = false;
                for attribute in &attributes.attributes {
                    match attribute.name.as_str() {
                        "minecraft:health" => {
                            if vitals.health != attribute.current {
                                vitals.health = attribute.current;
                                changed = true;
                            }
                        }
                        "minecraft:player.hunger" => {
                            let food = attribute.current.round() as i32;
                            if vitals.food != food {
                                vitals.food = food;
                                changed = true;
                            }
                        }
                        "minecraft:player.saturation" => {
                            if vitals.saturation != attribute.current {
                                vitals.saturation = attribute.current;
                                changed = true;
                            }
                        }
                        _ => {}
                    }
                }
                if !changed {
                    return Vec::new();
                }
                let set = play::SetHealth {
                    health: vitals.health,
                    food: vitals.food,
                    saturation: vitals.saturation,
                };
                drop(vitals);
                if set.health <= 0.0 {
                    request_respawn(ctx, bridge);
                }
                vec![Event::Packet(Box::new(play::Clientbound::SetHealth(set)))]
            }
            // Time of day. Bedrock sends only the tick count, and the game takes
            // it as the overworld's own clock; without it the sky sits still.
            bp::Packet::SetTime(time) => {
                let clock = ctx
                    .builtin
                    .get("minecraft:world_clock")
                    .and_then(|ids| ids.id("minecraft:overworld"))
                    .unwrap_or(0);
                let ticks = i64::from(*time);
                vec![Event::Packet(Box::new(play::Clientbound::SetTime(
                    play::SetTime {
                        game_time: ticks,
                        clocks: vec![play::ClockState {
                            clock,
                            total_ticks: ticks,
                            partial_tick: 0.0,
                            rate: 1.0,
                        }],
                    },
                )))]
            }
            // 1.26 sends the level's time as world clocks rather than
            // `SetTime`. The overworld clock is the one the sky follows.
            bp::Packet::SyncWorldClocks(clocks) => {
                let Some(clock) = clocks
                    .iter()
                    .find(|c| c.name.as_deref() == Some("minecraft:overworld"))
                    .or_else(|| clocks.first())
                else {
                    return Vec::new();
                };
                let java = ctx
                    .builtin
                    .get("minecraft:world_clock")
                    .and_then(|ids| ids.id(clock.name.as_deref().unwrap_or("minecraft:overworld")))
                    .unwrap_or(0);
                let ticks = i64::from(clock.time);
                vec![Event::Packet(Box::new(play::Clientbound::SetTime(
                    play::SetTime {
                        game_time: ticks,
                        clocks: vec![play::ClockState {
                            clock: java,
                            total_ticks: ticks,
                            partial_tick: 0.0,
                            rate: if clock.paused { 0.0 } else { 1.0 },
                        }],
                    },
                )))]
            }
            // Weather. Bedrock announces rain and thunder as level events, and
            // the game wants both the state and the level it rains at -- the
            // state alone does not make it rain.
            bp::Packet::LevelEvent(e) if (3001..=3004).contains(&e.event) => {
                let (event, param) = match e.event {
                    3001 => (1u8, 0.0f32),
                    3003 => (2, 0.0),
                    3002 => (8, 1.0),
                    3004 => (8, 0.0),
                    _ => return Vec::new(),
                };
                let mut events = vec![Event::Packet(Box::new(play::Clientbound::GameEvent(
                    play::GameEvent { event, param },
                )))];
                if e.event == 3001 || e.event == 3003 {
                    events.push(Event::Packet(Box::new(play::Clientbound::GameEvent(
                        play::GameEvent {
                            event: 7,
                            param: if e.event == 3001 { 1.0 } else { 0.0 },
                        },
                    ))));
                }
                events
            }
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
                bridge
                    .removals
                    .lock()
                    .unwrap()
                    .retain(|(_, id)| *id != entity.runtime_id);
                ctx.entities.insert(
                    entity.runtime_id,
                    (entity.position, entity.yaw, entity.pitch, entity.head_yaw),
                );
                ctx.kinds.insert(entity.runtime_id, java_name.clone());
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
                // An item actor taking another is the server folding dropped
                // stacks: keep it on the ground and collect it with the stack it
                // joined. Anything else (our own player or another one, a mob) is
                // a real pickup, and the stack has to fly there and go.
                if ctx.drops.contains_key(actor) {
                    ctx.drop_children.entry(*actor).or_default().push(*item);
                    return Vec::new();
                }
                // The stack and everything folded into it fly in together.
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
                ctx.players.insert(player.runtime_id);
                ctx.kinds
                    .insert(player.runtime_id, "minecraft:player".to_owned());
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
                        // Bedrock sends an empty tag for almost every
                        // entity. Translating it as a component puts an empty
                        // name plate over them, so an empty tag clears the
                        // name instead.
                        (4, bp::MetaValue::String(name)) => values.push((
                            2,
                            horizon_protocol::entity_data::serializer::OPTIONAL_COMPONENT,
                            horizon_protocol::entity_data::Value::OptText(
                                (!name.trim().is_empty()).then(|| text_component(name)),
                            ),
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
                let names: std::collections::HashMap<i16, String> = items
                    .iter()
                    .map(|(identifier, network_id)| {
                        (*network_id, ctx.mappings.java_item(identifier))
                    })
                    .collect();
                *bridge.item_names.lock().unwrap() = names;
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
                // The correction names the tick it answers, which is the
                // input tick — the same counter the inputs are numbered with.
                // Comparing it with the local tick counter (as this did once the
                // input tick became the server's clock) made every correction
                // look like it belonged to the future, so all of them were
                // dropped and a player could be left stuck against a block.
                const REWIND_WINDOW: u64 = 40;
                let now_tick = bridge.input_tick.load(Ordering::Relaxed).max(0) as u64;
                if *tick == 0 || *tick + REWIND_WINDOW < now_tick {
                    return Vec::new();
                }
                // The server is authoritative, but its position is where it
                // thinks the player was *at the corrected tick*, and the player
                // has moved since. What has to be removed is only the error:
                // the difference between the server's position and the one we
                // reported for that same tick. It is added to a debt that the
                // tick path pays off a little at a time, because applying it
                // whole is the teleport the player sees. (ViaBedrock validates
                // the tick the same way but has no history to rebase with, so
                // it jumps; a Bedrock client replays its predictions, which is
                // gradual by construction.)
                let offset = {
                    let history = bridge.history.lock().unwrap();
                    match history
                        .iter()
                        .rev()
                        .find(|(reported, _)| *reported == *tick as i64)
                        .map(|(_, at)| *at)
                    {
                        Some(at_tick) => [
                            position[0] - at_tick[0],
                            feet - at_tick[1],
                            position[2] - at_tick[2],
                        ],
                        // No record for that tick (it predates the history):
                        // take the server's position as the truth.
                        None => [dx, dy, dz],
                    }
                };
                if offset[0].abs() < 0.001 && offset[1].abs() < 0.001 && offset[2].abs() < 0.001 {
                    return Vec::new();
                }
                bedrock_core::nethernet::trace(format!(
                    "CORR offset=[{:.3},{:.3},{:.3}] held=[{:.2},{:.2},{:.2}]",
                    offset[0], offset[1], offset[2], held[0], held[1], held[2]
                ));
                // Never move the client from a movement correction. The
                // server's prediction is diagnostic; the local simulation is
                // the player's movement. Applying this packet is what caused
                // the visible teleports and the correction feedback loop.
                Vec::new()
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
                    |v: Option<i8>, last: f32| v.map_or(last, |v| f32::from(v) * (360.0 / 256.0));
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
            // Bedrock's `Animate`: 1 swing, 3 wake up, 4 critical hit, 5 magic
            // critical hit. Horizon has no swing action on `Animate` any more
            // (its actions are 0 wake up, 1 crit, 2 magic crit), so a swing
            // goes out as its own packet and only real crits as particles.
            bp::Packet::Animate { action, runtime_id } => {
                let id = *runtime_id as i32;
                match *action {
                    1 => vec![Event::Packet(Box::new(play::Clientbound::SwingAnimation(
                        play::SwingAnimation {
                            id,
                            hand: 0,
                            kind: 1,
                            duration: 6,
                        },
                    )))],
                    3 => vec![Event::Packet(Box::new(play::Clientbound::Animate(
                        play::Animate { id, action: 0 },
                    )))],
                    4 => vec![Event::Packet(Box::new(play::Clientbound::Animate(
                        play::Animate { id, action: 1 },
                    )))],
                    5 => vec![Event::Packet(Box::new(play::Clientbound::Animate(
                        play::Animate { id, action: 2 },
                    )))],
                    _ => Vec::new(),
                }
            }
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
                bridge
                    .removals
                    .lock()
                    .unwrap()
                    .retain(|(_, pending)| !gone.contains(pending));
                let mut events = Vec::new();
                for entity in gone {
                    ctx.entities.remove(&entity);
                    ctx.kinds.remove(&entity);
                    ctx.players.remove(&entity);
                    ctx.drops.remove(&entity);
                    events.push(Event::Packet(Box::new(play::Clientbound::RemoveEntities(
                        play::RemoveEntities {
                            ids: vec![entity as i32],
                        },
                    ))));
                }
                events
            }
            // An entity was hurt. The game's damage event flashes it red, and
            // for our own player plays the hurt sound; another entity gets a
            // positional hurt sound, since the game has no sound-on-entity
            // packet here. Bedrock also announces its own hurt sound as a
            // level sound event, which this bridge does not decode.
            bp::Packet::ActorEvent {
                runtime_id,
                event,
                data: _,
            } if *event == 2 => {
                let id = *runtime_id;
                let mut events = vec![Event::Packet(Box::new(play::Clientbound::DamageEvent(
                    play::DamageEvent {
                        id: id as i32,
                        source_type: 0,
                        cause: None,
                        direct: None,
                        position: None,
                    },
                )))];
                if id != ctx.runtime_id as u64
                    && let Some((position, ..)) = ctx.entities.get(&id)
                {
                    events.push(hurt_sound(&ctx, id, *position));
                }
                events
            }
            // An entity died. The game is shown the death and the entity is
            // taken away, rather than left standing as a corpse until the
            // server gets round to removing it.
            bp::Packet::ActorEvent {
                runtime_id,
                event,
                data: _,
            } if *event == 3 || *event == 61 => {
                let id = *runtime_id;
                if id == ctx.runtime_id as u64 {
                    // Our own death. The game shows its death screen from this
                    // packet and nothing else announces it, so the player died
                    // with no screen and stayed on their feet. The screen stays
                    // up until the player asks for a respawn.
                    return vec![Event::Packet(Box::new(
                        play::Clientbound::PlayerCombatKill(play::PlayerCombatKill {
                            id: id as i32,
                            message: text_component("You died"),
                        }),
                    ))];
                }
                ctx.uniques.retain(|_, runtime| *runtime != id);
                let children = ctx.drop_children.remove(&id).unwrap_or_default();
                let mut gone = vec![id];
                gone.extend(children);
                let mut events = Vec::new();
                if let Some((position, ..)) = ctx.entities.get(&id) {
                    events.push(death_sound(&ctx, id, *position));
                }
                let due = bridge.tick.load(Ordering::Relaxed) + 20;
                for entity in gone {
                    ctx.entities.remove(&entity);
                    ctx.kinds.remove(&entity);
                    ctx.players.remove(&entity);
                    ctx.drops.remove(&entity);
                    if std::env::var_os("HZ_ENT_TRACE").is_some() {
                        bedrock_core::nethernet::trace(format!(
                            "DEATH id={entity} own={} due={due}",
                            entity == ctx.runtime_id as u64
                        ));
                    }
                    events.push(Event::Packet(Box::new(play::Clientbound::EntityEvent(
                        play::EntityEvent {
                            id: entity as i32,
                            event: 3,
                        },
                    ))));
                    // The game falls the body over for a second; taking it away
                    // straight after the death event cut the animation short.
                    bridge.removals.lock().unwrap().push((due, entity));
                }
                events
            }
            bp::Packet::ActorEvent { .. } => Vec::new(),
            // The server has placed the player again (after a death, or a
            // dimension change). Java closes its death screen and rebuilds the
            // player from a `Respawn`, then the position; the Bedrock packet
            // alone is not one Java understands, which is why the screen used
            // to sit there until the client rejoined.
            bp::Packet::Respawn { position, state } if *state == 1 => {
                // The same handshake ends the join handshake too, and the server
                // may send it on its own; only the player's own respawn request
                // closes the death screen.
                if !bridge.respawn_asked.swap(false, Ordering::Relaxed) {
                    return Vec::new();
                }
                let mut events = Vec::new();
                if let Some(spawn) = ctx.spawn_info.clone() {
                    events.push(Event::Packet(Box::new(play::Clientbound::Respawn(
                        play::Respawn { spawn, keep: 0 },
                    ))));
                }
                // Bedrock reports the position at the eye; Java's is the feet.
                let feet = [
                    position[0] as f64,
                    position[1] as f64 - PLAYER_EYE as f64,
                    position[2] as f64,
                ];
                ctx.spawn = feet;
                // The server simulates the player from our input, so the input
                // must already hold the new position or it drags the player
                // straight back to where they died.
                let at = [feet[0] as f32, feet[1] as f32, feet[2] as f32];
                *bridge.position.lock().unwrap() = at;
                *bridge.last_sent.lock().unwrap() = at;
                bridge.teleport_ack.store(true, Ordering::Relaxed);
                events.push(Event::Packet(Box::new(play::Clientbound::PlayerPosition(
                    play::PlayerPosition {
                        teleport: 0,
                        position: feet,
                        delta: [0.0; 3],
                        yaw: ctx.spawn_yaw,
                        pitch: ctx.spawn_pitch,
                        relatives: 0,
                    },
                ))));
                // The game's respawn starts its level tracker again, and it
                // leaves `WaitingForServer` only on this event.
                events.push(Event::Packet(Box::new(play::Clientbound::GameEvent(
                    play::GameEvent {
                        event: 13,
                        param: 0.0,
                    },
                ))));
                // The server's spawn is not finished until the client says it
                // has respawned: a `PlayerAction` of action 7 clears the dead
                // state there. Without it the player stays a corpse to everyone
                // else however the client looks.
                // A respawn closes the inventory screen on the server, so ask
                // for it again; the game's clicks need it.
                bridge.inv_open.store(false, Ordering::Relaxed);
                ctx.requests.push(bp::Out::Interact {
                    action: OPEN_INVENTORY,
                    target: ctx.runtime_id,
                });
                ctx.requests.push(bp::Out::PlayerAction {
                    runtime_id: ctx.runtime_id,
                    action: 7, // respawn
                    position: [0; 3],
                    result: [0; 3],
                    face: -1,
                });
                events
            }
            bp::Packet::Disconnect(d) => vec![Event::Disconnected {
                reason: if d.message.is_empty() {
                    format!("Bedrock disconnect reason {}", d.reason)
                } else {
                    d.message.clone()
                },
                key: None,
                from_server: true,
            }],
            // The client creates its own player from a position packet, and that
            // is what ends the loading screen. At StartGame its chunk area is not
            // there yet, so the position is repeated while the server publishes
            // chunks — a few times, then never again. The same update says which
            // columns to ask for.
            bp::Packet::NetworkChunkPublisherUpdate {
                position,
                radius: _,
            } => {
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
                ctx.radius = bridge.chunk_radius.load(Ordering::Relaxed).max(2);
                // The game ignores a column outside the storage around the
                // centre it was last told, so the centre must follow the player
                // (and a teleport) before the columns there arrive — not only
                // at login, or the destination chunks are refused.
                if ctx.sent_center != Some(center) {
                    ctx.sent_center = Some(center);
                    out.push(Event::Packet(Box::new(
                        play::Clientbound::SetChunkCacheCenter(play::SetChunkCacheCenter {
                            x: center.0,
                            z: center.1,
                        }),
                    )));
                }
                // Ask for the columns around the player whenever they cross a
                // chunk boundary, and keep what is already queued. Each streamed
                // column is queued as the server publishes it, not from a ring
                // built here: a column the server never sent cannot be asked
                // for, and a teleport's destination is queued the moment its
                // `LevelChunk` arrives.
                if ctx.wanted_center != Some(center) {
                    ctx.wanted_center = Some(center);
                    // The granted Bedrock radius is a distance in chunks (32
                    // here), not a count: retain only columns inside the circle,
                    // and never interpret the publisher's block amount (512).
                    let circle = Circle::of(ctx).expect("the centre was just set");
                    ctx.wanted
                        .retain(|((x, z), _)| circle.contains(*x, *z));
                    ctx.queued.retain(|&(x, z)| circle.contains(x, z));
                    retain_near(ctx, center.0, center.1);
                    queue_published(ctx);
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
            // The server's answer to the chunk-radius request. It is the cap
            // on what may be granted, so the smaller of the game's setting and
            // the grant is what the game's cache and the stream use.
            bp::Packet::ChunkRadiusUpdated(radius) => {
                let granted = (*radius).max(2);
                bridge.granted_radius.store(granted, Ordering::Relaxed);
                let effective = super::store_effective_radius(bridge);
                // The loop sends the new radius and rebuilds the streamed set;
                // setting `ctx.radius` here too would hide the change from it.
                bridge.radius_dirty.store(true, Ordering::Relaxed);
                if chunk_trace() {
                    bedrock_core::nethernet::trace(format!(
                        "chunk radius granted={granted} setting={} stream={effective}",
                        bridge.requested_radius.load(Ordering::Relaxed)
                    ));
                }
                Vec::new()
            }
            bp::Packet::LevelChunk(c) => {
                // A column sent again replaces the old one, exactly as the
                // game's cache replaces a column it is handed twice.
                forget_column(ctx, c.chunk_x, c.chunk_z);
                let (first, last) = section_range();
                // The server says how far up the column has blocks: the field
                // is a section count from the bottom of the dimension (one
                // based), and a negative value means "request the whole
                // column". Sections above it are air, so they are never asked
                // for.
                let section_end = match c.highest_subchunk {
                    Some(highest) if highest >= 0 => (first + highest).min(last),
                    _ => last,
                };
                let mut state = ColumnState::new(first, section_end);
                // Whatever blocks the payload carries; the rest arrive as
                // sub-chunk replies. A payload is only decoded up to the
                // world's own section count, so a request-mode marker (which
                // reads as a huge count) cannot make the decoder run away.
                let air = ctx.blocks.as_ref().map(|b| b.air_runtime).unwrap_or(0);
                let inline = c.sub_chunk_count.min(64) as i32;
                if inline > 0 {
                    if let Ok(decoded) = bedrock_core::chunk::decode_sections(c, air) {
                        for section in decoded {
                            let y = i32::from(section.y);
                            put_section(ctx, c.chunk_x, c.chunk_z, y, &section.blocks);
                            state.requested.insert(y);
                            state.answered.insert(y);
                            state.next_section = state.next_section.max(y + 1);
                        }
                    }
                }
                if state.next_section > state.section_end {
                    state.complete_request = true;
                }
                if chunk_trace() {
                    bedrock_core::nethernet::trace(format!(
                        "chunk recv ({},{}) sub_chunks={} inline={} highest={:?} end={} payload={} next={}",
                        c.chunk_x,
                        c.chunk_z,
                        c.sub_chunk_count,
                        inline,
                        c.highest_subchunk,
                        state.section_end,
                        c.data.len(),
                        state.next_section
                    ));
                }
                ctx.columns.insert((c.chunk_x, c.chunk_z), state);
                // The server publishes the columns around the player; each is
                // asked for as it arrives, inside the streamed circle.
                if in_circle(ctx, c.chunk_x, c.chunk_z) {
                    queue_column(ctx, c.chunk_x, c.chunk_z);
                }
                // A payload carrying every section completes the column here.
                note_finished(ctx, c.chunk_x, c.chunk_z);
                Vec::new()
            }
            // A changed block. Without it, nothing built or broken shows up
            // until the column is sent again. The block also changes the light
            // around it, which goes out as a light update.
            bp::Packet::UpdateBlock {
                position,
                runtime_id,
                flags: _,
            } => {
                let Some(state) = ctx.blocks.as_mut().map(|blocks| blocks.java(*runtime_id as i32))
                else {
                    return Vec::new();
                };
                store_block_id(ctx, *position, *runtime_id);
                set_block(ctx, *position, state);
                let mut events = vec![Event::Packet(Box::new(play::Clientbound::BlockUpdate(
                    play::BlockUpdate {
                        pos: *position,
                        state: state as i32,
                    },
                )))];
                let columns = columns_around(*position).collect();
                events.extend(relight_columns(ctx, &columns));
                events
            }
            // The batched form of the change above.
            bp::Packet::UpdateSubChunkBlocks { blocks, .. } => {
                let mut events = Vec::new();
                let mut columns = std::collections::BTreeSet::new();
                for block in blocks {
                    let Some(state) = ctx
                        .blocks
                        .as_mut()
                        .map(|registry| registry.java(block.runtime_id as i32))
                    else {
                        return Vec::new();
                    };
                    set_block(ctx, block.position, state);
                    store_block_id(ctx, block.position, block.runtime_id);
                    columns.extend(columns_around(block.position));
                    events.push(Event::Packet(Box::new(play::Clientbound::BlockUpdate(
                        play::BlockUpdate {
                            pos: block.position,
                            state: state as i32,
                        },
                    ))));
                }
                events.extend(relight_columns(ctx, &columns));
                events
            }
            // The player's own inventory. Bedrock's window 0 lists its slots in
            // Bedrock order, which is not Java's inventory-menu order, so place
            // each item at its Java slot. Other windows still need mapping.
            bp::Packet::InventoryContent(c) if c.window_id == 0 => {
                if let Some(pos) = bridge.last_use.lock().unwrap().take() {
                    bedrock_core::nethernet::trace(format!(
                        "inventory resynced after a use at {pos:?}"
                    ));
                }
                {
                    let mut held = bridge.items.lock().unwrap();
                    *held = c.slots.iter().take(36).map(descriptor).collect();
                }
                // The server has sent the whole inventory again; its ids are the
                // truth now, so nothing is waiting to be answered.
                bridge.pending_ids.lock().unwrap().clear();
                // The window also reports the armour and the off hand, past the
                // inventory's own slots.
                {
                    let mut armor = bridge.armor.lock().unwrap();
                    for (i, entry) in c.slots.iter().enumerate().skip(36).take(4) {
                        if let Some(slot) = armor.get_mut(i - 36) {
                            *slot = descriptor(entry);
                        }
                    }
                }
                if let Some(item) = c.slots.get(40) {
                    *bridge.offhand.lock().unwrap() = descriptor(item);
                }
                // Ask for the server to open the inventory screen if it has not;
                // its cursor and combined container are what the game's clicks
                // are made of, and neither exists without it. The screen is the
                // client's: it is open as soon as the request is out.
                if !bridge.inv_open.load(Ordering::Relaxed) {
                    ctx.requests.push(bp::Out::Interact {
                        action: OPEN_INVENTORY,
                        target: ctx.runtime_id,
                    });
                    bridge.inv_open.store(true, Ordering::Relaxed);
                }
                // The whole menu comes from the bridge's model, so the armour and
                // the off hand keep their contents: a content packet replaces
                // every slot, and one built only from window 0 would wipe them.
                let mut events = vec![inventory_content(ctx, bridge)];
                // The same slots are shown inside an open window's menu.
                let guard = bridge.window.lock().unwrap();
                if let Some(window) = guard.as_ref() {
                    events.push(window_content(ctx, bridge, window));
                }
                events
            }
            // The server's answers to inventory moves: each settles the counts
            // and says which stacks it acted on.
            bp::Packet::ItemStackResponses(responses) => {
                let mut events = Vec::new();
                for response in responses {
                    events.extend(stack_response(ctx, bridge, response));
                }
                events
            }
            // The item on the cursor lives in its own container, and the game
            // shows it as the carried stack.
            bp::Packet::InventorySlot(s) if s.window_id == 124 => {
                *bridge.cursor.lock().unwrap() = descriptor(&s.item);
                vec![Event::Packet(Box::new(play::Clientbound::SetCursorItem(
                    play::SetCursorItem {
                        item: map_item(ctx, &s.item),
                    },
                )))]
            }
            // Armour (120) and the off hand (119) have containers of their own;
            // a slot update there is part of the player's inventory menu.
            bp::Packet::InventorySlot(s) if s.window_id == 120 => {
                if let Ok(index) = usize::try_from(s.slot) {
                    if let Some(entry) = bridge.armor.lock().unwrap().get_mut(index) {
                        *entry = descriptor(&s.item);
                    }
                }
                vec![Event::Packet(Box::new(
                    play::Clientbound::ContainerSetSlot(play::ContainerSetSlot {
                        container: 0,
                        state: 0,
                        slot: 5 + i16::try_from(s.slot).unwrap_or(0),
                        item: map_item(ctx, &s.item),
                    }),
                ))]
            }
            bp::Packet::InventorySlot(s) if s.window_id == 119 => {
                *bridge.offhand.lock().unwrap() = descriptor(&s.item);
                vec![Event::Packet(Box::new(
                    play::Clientbound::ContainerSetSlot(play::ContainerSetSlot {
                        container: 0,
                        state: 0,
                        slot: 45,
                        item: map_item(ctx, &s.item),
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
                let mut events = vec![Event::Packet(Box::new(
                    play::Clientbound::ContainerSetSlot(play::ContainerSetSlot {
                        container: 0,
                        state: 0,
                        slot,
                        item,
                    }),
                ))];
                // The player's slots are shown again inside an open window's
                // menu, so they have to be updated there too.
                let guard = bridge.window.lock().unwrap();
                if let Some(window) = guard.as_ref() {
                    if let Some(event) = window_player_slot(ctx, bridge, window, s.slot as u8) {
                        events.push(event);
                    }
                }
                events
            }
            // A slot of a container the server opened.
            bp::Packet::InventorySlot(s) => {
                let mut guard = bridge.window.lock().unwrap();
                let Some(window) = guard.as_mut() else {
                    return Vec::new();
                };
                if s.window_id != u32::from(window.id) {
                    return Vec::new();
                }
                if s.container.is_some() {
                    window.name = s.container;
                }
                let Ok(index) = usize::try_from(s.slot) else {
                    return Vec::new();
                };
                window.store(index, descriptor(&s.item));
                if index >= window.kind.slots() {
                    return Vec::new();
                }
                vec![Event::Packet(Box::new(
                    play::Clientbound::ContainerSetSlot(play::ContainerSetSlot {
                        container: i32::from(window.id),
                        state: 0,
                        slot: index as i16,
                        item: map_item(ctx, &s.item),
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
            // A container the server opened. Bedrock sends no title with it, so
            // the game is given the name of the kind of menu it is. A kind the
            // bridge cannot translate is closed again rather than shown, since
            // its clicks would go nowhere.
            bp::Packet::ContainerOpen(c) if c.window_type == INVENTORY_WINDOW => {
                // The player's own inventory screen. It has no Java counterpart
                // (the game draws it itself), but a stack request's cursor and
                // combined container only exist while it is open on the server.
                bridge.inv_open.store(true, Ordering::Relaxed);
                bedrock_core::nethernet::trace("inventory screen opened".to_string());
                Vec::new()
            }
            bp::Packet::ContainerOpen(c) => {
                let Some(kind) = WindowKind::of(c.window_type) else {
                    bedrock_core::nethernet::trace(format!(
                        "container type {} is not supported; refusing it",
                        c.window_type
                    ));
                    ctx.requests.push(bp::Out::ContainerClose {
                        window_id: c.window_id,
                        window_type: c.window_type,
                    });
                    return Vec::new();
                };
                let window = OpenWindow {
                    id: c.window_id,
                    kind,
                    title: kind.title().to_string(),
                    slots: vec![None; kind.slots()],
                    name: None,
                    bedrock_type: c.window_type,
                };
                let title = window.title.clone();
                let event = Event::Packet(Box::new(play::Clientbound::OpenScreen(
                    play::OpenScreen {
                        container: i32::from(c.window_id),
                        menu: kind.menu(),
                        title: text_component(&title),
                    },
                )));
                *bridge.window.lock().unwrap() = Some(window);
                vec![event]
            }
            // Armour, the off hand and the cursor arrive in their own
            // containers (120, 119, 124), not in the inventory's content.
            bp::Packet::InventoryContent(c) if c.window_id == 120 => {
                let mut armor = bridge.armor.lock().unwrap();
                let mut events = Vec::new();
                for (i, item) in c.slots.iter().enumerate().take(4) {
                    if let Some(slot) = armor.get_mut(i) {
                        *slot = descriptor(item);
                    }
                    events.push(Event::Packet(Box::new(
                        play::Clientbound::ContainerSetSlot(play::ContainerSetSlot {
                            container: 0,
                            state: 0,
                            slot: 5 + i as i16,
                            item: map_item(ctx, item),
                        }),
                    )));
                }
                events
            }
            bp::Packet::InventoryContent(c) if c.window_id == 119 => {
                let item = c.slots.first();
                if std::env::var_os("HZ_INV_TRACE").is_some() {
                    bedrock_core::nethernet::trace(format!(
                        "off hand container: {:?} -> slot 45 {:?}",
                        item.map(|i| (i.runtime_id, i.count, i.net_id)),
                        item.and_then(|i| map_item(ctx, i))
                            .map(|s| (s.item, s.count)),
                    ));
                }
                *bridge.offhand.lock().unwrap() = item.and_then(|i| descriptor(i));
                vec![Event::Packet(Box::new(
                    play::Clientbound::ContainerSetSlot(play::ContainerSetSlot {
                        container: 0,
                        state: 0,
                        slot: 45,
                        item: item.and_then(|i| map_item(ctx, i)),
                    }),
                ))]
            }
            bp::Packet::InventoryContent(c) if c.window_id == 124 => {
                let item = c.slots.first();
                *bridge.cursor.lock().unwrap() = item.and_then(|i| descriptor(i));
                vec![Event::Packet(Box::new(play::Clientbound::SetCursorItem(
                    play::SetCursorItem {
                        item: item.and_then(|i| map_item(ctx, i)),
                    },
                )))]
            }
            // The contents of an open container: the game shows the container's
            // slots along with the player's, as one menu.
            bp::Packet::InventoryContent(c) if c.window_id != 0 => {
                let mut guard = bridge.window.lock().unwrap();
                let Some(window) = guard.as_mut() else {
                    return Vec::new();
                };
                if c.window_id != u32::from(window.id) {
                    return Vec::new();
                }
                if c.container.is_some() {
                    window.name = c.container;
                }
                for (i, item) in c.slots.iter().enumerate() {
                    window.store(i, descriptor(item));
                }
                vec![window_content(ctx, bridge, window)]
            }
            bp::Packet::ContainerClose { window_id } => {
                *bridge.window.lock().unwrap() = None;
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
                let started = std::time::Instant::now();
                let (x, z) = (sub.position[0], sub.position[2]);
                // Every offset refused as "the column is gone": the player
                // left it, so what the bridge holds is stale.
                if !sub.entries.is_empty() && sub.entries.iter().all(|e| e.result == 2) {
                    forget_column(ctx, x, z);
                    return Vec::new();
                }
                if let Some(state) = ctx.columns.get_mut(&(x, z)) {
                    if state.pending {
                        state.pending = false;
                        ctx.pending_requests = ctx.pending_requests.saturating_sub(1);
                    }
                }
                let air = ctx.blocks.as_ref().map(|b| b.air_runtime).unwrap_or(0);
                let mut delivered = 0usize;
                let mut empty = 0usize;
                let mut refused = 0usize;
                for entry in &sub.entries {
                    // The entry's section is the reply's base Y plus the offset,
                    // both absolute section indices (the request asked with base
                    // Y 0). `put_section` then places it against the dimension's
                    // `min_y`.
                    let y = sub.position[1] + i32::from(entry.offset[1]);
                    match entry.result {
                        // Success: a serialized section.
                        1 if !entry.payload.is_empty() => {
                            let mut r = bedrock_core::stream::Reader::new(&entry.payload[1..]);
                            let Ok(sc) =
                                bedrock_core::subchunk::SubChunk::read(&mut r, entry.payload[0])
                            else {
                                continue;
                            };
                            let mut ids = vec![0u32; 4096];
                            for sy in 0..16usize {
                                for sz in 0..16usize {
                                    for sx in 0..16usize {
                                        let id = sc.runtime_id(sx, sy, sz, air).unwrap_or(0);
                                        ids[(sy * 16 + sz) * 16 + sx] =
                                            u32::try_from(id).unwrap_or(0);
                                    }
                                }
                            }
                            put_section(ctx, x, z, y, &ids);
                            delivered += 1;
                        }
                        // Success, all air: the section exists with no blocks.
                        // A column re-sent since is built from scratch, so its
                        // sections already read as air; the column itself must
                        // still exist for the light and the mesh.
                        6 => {
                            ensure_chunk(ctx, x, z);
                            empty += 1;
                        }
                        _ => refused += 1,
                    }
                    // The section counts as answered whatever the result: a
                    // refusal must not hold the column back forever.
                    if let Some(state) = ctx.columns.get_mut(&(x, z)) {
                        state.answered.insert(y);
                    }
                }
                if chunk_trace() {
                    bedrock_core::nethernet::trace(format!(
                        "subchunk ({x},{z}) entries={} delivered={delivered} air={empty} refused={refused} took={}ms",
                        sub.entries.len(),
                        started.elapsed().as_millis()
                    ));
                }
                note_finished(ctx, x, z);
                Vec::new()
            }
            // Block updates, block entities, inventories and the rest still
            // need their own translation; see the README.
            _ => Vec::new(),
        }
    }

    /// One `PlayerAuthInput`, built from the state the game has reported. It
    /// goes out once per client tick, and carries the press and release edges
    /// of jump, sneak and sprint exactly once.
    fn auth_input(bridge: &Bridge, actions: Vec<bp::BlockAction>) -> Vec<bp::Out> {
        let tick = {
            let seeded = bridge.input_tick.load(Ordering::Relaxed);
            bridge.input_tick.fetch_add(1, Ordering::Relaxed);
            u64::try_from(seeded.max(1)).unwrap_or(1)
        };
        let position = *bridge.position.lock().unwrap();
        let [yaw, pitch] = *bridge.rotation.lock().unwrap();
        let input = bridge.input.load(Ordering::Relaxed) as u8;
        // The game sends its bitfield only when it changes, so the previous
        // one is what a press or a release is measured against.
        let previous = bridge.sent_input.swap(i64::from(input), Ordering::Relaxed) as u8;
        let sprinting = bridge.sprinting.load(Ordering::Relaxed);
        let was_sprinting = bridge.sent_sprint.swap(sprinting, Ordering::Relaxed);
        let on_ground = bridge.on_ground.load(Ordering::Relaxed);
        let was_on_ground = bridge.sent_on_ground.swap(on_ground, Ordering::Relaxed);
        let collision = bridge.collision.load(Ordering::Relaxed);
        let (mut flags, move_vec) = input_bits(
            input,
            previous,
            sprinting,
            was_sprinting,
            on_ground,
            was_on_ground,
            collision,
        );
        // Bedrock only uses the fast sprint-water drag while the swimming
        // pose is reported. Horizon enters that pose when sprinting and moving
        // forward in water, so report the same start/stop edge.
        let swimming = bridge.in_water.load(Ordering::Relaxed)
            && sprinting
            && move_vec[1] > 0.0;
        let was_swimming = bridge.sent_swimming.swap(swimming, Ordering::Relaxed);
        if swimming && !was_swimming {
            flags |= 1 << 29; // start_swimming
        } else if !swimming && was_swimming {
            flags |= 1 << 30; // stop_swimming
        }
        if !actions.is_empty() {
            flags |= 1 << 35; // perform_block_actions
        }
        // A respawn's teleport is not settled on the server until an input
        // reports it handled.
        if bridge.teleport_ack.swap(false, Ordering::Relaxed) {
            flags |= 1 << 37; // handled_teleport
        }
        // `Pos Delta` is the position change since the previous input, not a
        // velocity: the server compares its own simulation against it, so a
        // friction-scaled value made it disagree on every sprint jump and
        // climb. It is the client's own movement, without the correction below.
        let delta = {
            let mut last = bridge.last_sent.lock().unwrap();
            let d = [
                position[0] - last[0],
                position[1] - last[1],
                position[2] - last[2],
            ];
            *last = position;
            d
        };
        // A server reads the vertical-collision bit together with a falling
        // delta to decide that the player is on ground. Sending it while
        // swimming or jumping up makes water travel pick ground physics, so
        // report it only for a descending tick.
        if delta[1] >= 0.0 {
            flags &= !(1 << 50);
        } else if on_ground {
            flags |= 1 << 50;
        }
        {
            // Keep the feet position by tick for the correction trace.
            let mut history = bridge.history.lock().unwrap();
            history.push_back((i64::try_from(tick).unwrap_or(0), position));
            while history.len() > 64 {
                history.pop_front();
            }
        }
        // Bedrock's mPos is the eye position; the game tracks feet.
        let position = [position[0], position[1] + PLAYER_EYE, position[2]];
        if tick % 20 == 0 || move_vec != [0.0, 0.0] {
            bedrock_core::nethernet::trace(format!(
                "AUTHIN-MOVE pos={:?} move={:?} flags={:#x} input={:#04x} delta={:?} tick={}",
                position, move_vec, flags, input, delta, tick
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

    /// The same, for an item the bridge knows by its own id and count.
    fn map_item_by_id(ctx: &Context, runtime_id: i16, count: i32) -> Option<Stack> {
        let id = item_by_network_id(ctx.mappings, ctx.builtin, &ctx.items, runtime_id);
        (id != 0 && count > 0).then(|| Stack::new(id, count))
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
                net_id: item.net_id,
                block_runtime_id: item.block_runtime_id,
            },
        )
    }

    /// The whole player inventory as a Java content update, from what the
    /// server reported plus any pickups the bridge predicted. Sending the whole
    /// container keeps every count consistent, where a single slot update can
    /// be discarded when its menu state does not match.
    fn inventory_content(ctx: &Context, bridge: &Bridge) -> Event {
        let mut slots: Vec<Option<Stack>> = (0..46).map(|_| None).collect();
        let map = |d: &bedrock_core::protocol::ItemDescriptor| {
            let id = item_by_network_id(ctx.mappings, ctx.builtin, &ctx.items, d.runtime_id);
            (id != 0).then(|| Stack::new(id, i32::from(d.count)))
        };
        for (i, entry) in bridge.items.lock().unwrap().iter().enumerate() {
            let Some(d) = entry else { continue };
            let Some(index) = java_inventory_slot(i as u32).and_then(|s| usize::try_from(s).ok()) else {
                continue;
            };
            if index < slots.len() {
                slots[index] = map(d);
            }
        }
        // The armour and the off hand are part of the same menu, so a resync has
        // to carry them too or they read as emptied.
        for (i, entry) in bridge.armor.lock().unwrap().iter().enumerate() {
            if let Some(d) = entry {
                if 5 + i < slots.len() {
                    slots[5 + i] = map(d);
                }
            }
        }
        if let Some(d) = *bridge.offhand.lock().unwrap() {
            slots[45] = map(&d);
        }
        // A content packet carries the stack on the cursor too; leaving it out
        // would clear whatever the player is holding.
        let carried = bridge.cursor.lock().unwrap().as_ref().and_then(|d| map(d));
        Event::Packet(Box::new(play::Clientbound::ContainerSetContent(
            play::ContainerSetContent {
                container: 0,
                state: 0,
                items: slots,
                carried,
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

    /// The container names a stack request uses. A container is named in the
    /// request rather than identified by a number, and the player's own window
    /// is made of several of them.
    const CONTAINER_COMBINED: u8 = 12;
    const CONTAINER_HOTBAR: u8 = 28;
    const CONTAINER_INVENTORY: u8 = 29;
    const CONTAINER_ARMOR: u8 = 6;
    const CONTAINER_CRAFTING_INPUT: u8 = 13;
    const CONTAINER_OFFHAND: u8 = 34;
    const CONTAINER_CURSOR: u8 = 59;
    /// `Interact` action: ask the server to open the player's inventory screen.
    const OPEN_INVENTORY: u8 = 6;
    /// The window type the server answers that with (`WindowType::inventory`, -1).
    const INVENTORY_WINDOW: u8 = 0xFF;

    /// Where a slot in the game's own inventory window lives on the server,
    /// as its container name and the slot within it.
    fn bedrock_slot_of(java: i16) -> Option<(u8, u8)> {
        Some(match java {
            1..=4 => (CONTAINER_CRAFTING_INPUT, (java - 1) as u8),
            5..=8 => (CONTAINER_ARMOR, (java - 5) as u8),
            9..=35 => (CONTAINER_COMBINED, java as u8),
            36..=44 => (CONTAINER_COMBINED, (java - 36) as u8),
            45 => (CONTAINER_OFFHAND, 1),
            _ => return None,
        })
    }

    /// The other direction, for something the server reports.
    fn java_slot_of(container: u8, slot: u8) -> Option<i16> {
        Some(match container {
            CONTAINER_COMBINED | CONTAINER_HOTBAR | CONTAINER_INVENTORY => {
                if slot < 9 {
                    36 + i16::from(slot)
                } else {
                    i16::from(slot)
                }
            }
            CONTAINER_ARMOR => 5 + i16::from(slot),
            CONTAINER_OFFHAND => 45,
            CONTAINER_CRAFTING_INPUT => 1 + i16::from(slot),
            _ => return None,
        })
    }

    /// What the bridge believes is in a slot the server names. A container the
    /// server opened is named by the server rather than numbered, so it is
    /// matched against the open window first.
    fn item_at(
        bridge: &Bridge,
        container: u8,
        slot: u8,
    ) -> Option<bedrock_core::protocol::ItemDescriptor> {
        if let Some(window) = bridge.window.lock().unwrap().as_ref() {
            if window.name.map(|name| name.name) == Some(container) {
                return window.slots.get(usize::from(slot)).copied().flatten();
            }
        }
        match container {
            CONTAINER_COMBINED | CONTAINER_HOTBAR | CONTAINER_INVENTORY => bridge
                .items
                .lock()
                .unwrap()
                .get(usize::from(slot))
                .copied()
                .flatten(),
            CONTAINER_ARMOR => bridge
                .armor
                .lock()
                .unwrap()
                .get(usize::from(slot))
                .copied()
                .flatten(),
            CONTAINER_OFFHAND => *bridge.offhand.lock().unwrap(),
            CONTAINER_CURSOR => *bridge.cursor.lock().unwrap(),
            _ => None,
        }
    }

    /// Record what the server settled on for a slot.
    fn set_item_at(
        bridge: &Bridge,
        container: u8,
        slot: u8,
        item: Option<bedrock_core::protocol::ItemDescriptor>,
    ) {
        if let Some(window) = bridge.window.lock().unwrap().as_mut() {
            if window.name.map(|name| name.name) == Some(container) {
                window.store(usize::from(slot), item);
                return;
            }
        }
        match container {
            CONTAINER_COMBINED | CONTAINER_HOTBAR | CONTAINER_INVENTORY => {
                if let Some(entry) = bridge
                    .items
                    .lock()
                    .unwrap()
                    .get_mut(usize::from(slot))
                {
                    *entry = item;
                }
            }
            CONTAINER_ARMOR => {
                if let Some(entry) = bridge
                    .armor
                    .lock()
                    .unwrap()
                    .get_mut(usize::from(slot))
                {
                    *entry = item;
                }
            }
            CONTAINER_OFFHAND => *bridge.offhand.lock().unwrap() = item,
            CONTAINER_CURSOR => *bridge.cursor.lock().unwrap() = item,
            _ => {}
        }
    }

    /// Copy every slot a move can reach, so a refused move can be undone.
    fn snapshot(bridge: &Bridge) -> Items {
        Items {
            items: bridge.items.lock().unwrap().clone(),
            armor: bridge.armor.lock().unwrap().clone(),
            offhand: *bridge.offhand.lock().unwrap(),
            cursor: *bridge.cursor.lock().unwrap(),
            window: bridge
                .window
                .lock()
                .unwrap()
                .as_ref()
                .map(|w| (w.name, w.slots.clone())),
        }
    }

    /// Put a snapshot back, undoing a move the server refused.
    fn restore(bridge: &Bridge, snapshot: Items) {
        *bridge.items.lock().unwrap() = snapshot.items;
        *bridge.armor.lock().unwrap() = snapshot.armor;
        *bridge.offhand.lock().unwrap() = snapshot.offhand;
        *bridge.cursor.lock().unwrap() = snapshot.cursor;
        if let Some(window) = bridge.window.lock().unwrap().as_mut() {
            if let Some((name, slots)) = snapshot.window {
                window.name = name;
                window.slots = slots;
            }
        }
    }

    /// Put one slot back to what a snapshot held, returning what it now holds.
    fn restore_slot(
        bridge: &Bridge,
        before: &Items,
        container: u8,
        slot: u8,
    ) -> Option<bedrock_core::protocol::ItemDescriptor> {
        let item = match container {
            CONTAINER_COMBINED | CONTAINER_HOTBAR | CONTAINER_INVENTORY => {
                before.items.get(usize::from(slot)).copied().flatten()
            }
            CONTAINER_ARMOR => before.armor.get(usize::from(slot)).copied().flatten(),
            CONTAINER_OFFHAND => before.offhand,
            CONTAINER_CURSOR => before.cursor,
            _ => before
                .window
                .as_ref()
                .and_then(|(_, slots)| slots.get(usize::from(slot)).copied().flatten()),
        };
        set_item_at(bridge, container, slot, item);
        item
    }

    /// Move `count` of the item at `from` into `to`, merging a matching stack,
    /// the way the server applies a take or a place.
    fn move_item(bridge: &Bridge, from: bp::SlotRef, to: bp::SlotRef, count: u8) {
        let count = i16::from(count);
        if count <= 0 {
            return;
        }
        let Some(mut item) = item_at(bridge, from.container.name, from.slot) else {
            return;
        };
        if item.count <= 0 {
            return;
        }
        let moved = count.min(item.count);
        match item_at(bridge, to.container.name, to.slot) {
            Some(mut dest) if dest.runtime_id == item.runtime_id => {
                dest.count = dest.count.saturating_add(moved);
                set_item_at(bridge, to.container.name, to.slot, Some(dest));
            }
            Some(_) => return,
            None => {
                let mut dest = item;
                dest.count = moved;
                set_item_at(bridge, to.container.name, to.slot, Some(dest));
            }
        }
        item.count -= moved;
        set_item_at(
            bridge,
            from.container.name,
            from.slot,
            (item.count > 0).then_some(item),
        );
    }

    fn swap_items(bridge: &Bridge, a: bp::SlotRef, b: bp::SlotRef) {
        let first = item_at(bridge, a.container.name, a.slot);
        let second = item_at(bridge, b.container.name, b.slot);
        set_item_at(bridge, a.container.name, a.slot, second);
        set_item_at(bridge, b.container.name, b.slot, first);
    }

    fn remove_items(bridge: &Bridge, slot: bp::SlotRef, count: u8) {
        let Some(mut item) = item_at(bridge, slot.container.name, slot.slot) else {
            return;
        };
        item.count -= i16::from(count).min(item.count);
        set_item_at(
            bridge,
            slot.container.name,
            slot.slot,
            (item.count > 0).then_some(item),
        );
    }

    /// Apply one request action to the local slots, so the game shows the move
    /// before the server answers.
    fn apply_action(bridge: &Bridge, action: &bp::StackAction) {
        match *action {
            bp::StackAction::Take {
                count,
                source,
                destination,
            }
            | bp::StackAction::Place {
                count,
                source,
                destination,
            } => move_item(bridge, source, destination, count),
            bp::StackAction::Swap { first, second } => swap_items(bridge, first, second),
            bp::StackAction::Drop { count, source, .. }
            | bp::StackAction::Destroy { count, source } => remove_items(bridge, source, count),
        }
    }

    /// The container name one of the player's own slots is named under. While
    /// the inventory screen is open that is the screen's combined view; with no
    /// screen open a real client names the hotbar and the main inventory
    /// separately (the combined view does not exist then).
    fn player_container(slot: u8, screen_open: bool) -> u8 {
        if screen_open {
            CONTAINER_COMBINED
        } else if slot < 9 {
            CONTAINER_HOTBAR
        } else {
            CONTAINER_INVENTORY
        }
    }

    fn player_slot_ref(bridge: &Bridge, slot: u8) -> bp::SlotRef {
        let screen_open = bridge.inv_open.load(Ordering::Relaxed);
        slot_ref(bridge, player_container(slot, screen_open), slot)
    }

    /// A slot the bridge resolved to the combined player inventory, named the way
    /// the server wants it.
    fn inventory_slot_ref(bridge: &Bridge, container: u8, slot: u8) -> bp::SlotRef {
        if container == CONTAINER_COMBINED {
            player_slot_ref(bridge, slot)
        } else {
            slot_ref(bridge, container, slot)
        }
    }

    fn slot_ref(bridge: &Bridge, container: u8, slot: u8) -> bp::SlotRef {
        // A stack request has to quote the stack id the server issued for the
        // slot it names; zero means "empty", so a move that leaves it out is
        // read as acting on nothing and refused. A slot a move has already
        // changed here but the server has not answered for yet is named by that
        // request's id instead, which is how a real client refers to its own
        // not-yet-confirmed changes.
        let pending = bridge
            .pending_ids
            .lock()
            .unwrap()
            .get(&(container, slot))
            .copied();
        let stack_id = match pending {
            Some(id) => id,
            None => item_at(bridge, container, slot)
                .and_then(|d| d.net_id)
                .map_or(0, |id| id as i32),
        };
        bp::SlotRef::new(container, slot, stack_id)
    }

    /// Remember the slots a request has just changed locally, so later requests
    /// name them by its id until the server answers.
    fn note_pending(bridge: &Bridge, request_id: i32, actions: &[bp::StackAction]) {
        let mut pending = bridge.pending_ids.lock().unwrap();
        // A name whose answer never came would otherwise outlive its request and
        // wedge every later move on that slot; drop the lot rather than keep
        // quoting stale request ids.
        if pending.len() > 128 {
            pending.clear();
        }
        for action in actions {
            match *action {
                bp::StackAction::Take {
                    source,
                    destination,
                    ..
                }
                | bp::StackAction::Place {
                    source,
                    destination,
                    ..
                } => {
                    pending.insert((source.container.name, source.slot), request_id);
                    pending.insert((destination.container.name, destination.slot), request_id);
                }
                bp::StackAction::Swap { first, second } => {
                    pending.insert((first.container.name, first.slot), request_id);
                    pending.insert((second.container.name, second.slot), request_id);
                }
                bp::StackAction::Drop { source, .. }
                | bp::StackAction::Destroy { source, .. } => {
                    pending.insert((source.container.name, source.slot), request_id);
                }
            }
        }
    }

    /// Which server slot a click on the game's menu refers to, whether that is
    /// the player's own window or a container the server opened.
    fn click_slot(bridge: &Bridge, container: i32, slot: i16) -> Option<bp::SlotRef> {
        if let Some(window) = bridge.window.lock().unwrap().as_ref() {
            if container != i32::from(window.id) {
                return None;
            }
            let count = window.kind.slots() as i16;
            if (0..count).contains(&slot) {
                let item = window.slots.get(slot as usize).copied().flatten();
                return Some(bp::SlotRef {
                    container: window.name.unwrap_or_default(),
                    slot: slot as u8,
                    stack_id: item.and_then(|d| d.net_id).map_or(0, |id| id as i32),
                });
            }
            let bedrock = window.kind.bedrock_player_slot(slot)?;
            return Some(player_slot_ref(bridge, bedrock));
        }
        if container != 0 {
            return None;
        }
        let (name, at) = bedrock_slot_of(slot)?;
        Some(inventory_slot_ref(bridge, name, at))
    }

    /// The whole menu of an open window as the game wants it: the container's
    /// slots and then the player's.
    fn window_content(ctx: &Context, bridge: &Bridge, window: &OpenWindow) -> Event {
        let mut slots: Vec<Option<Stack>> =
            (0..window.kind.slots() + 36).map(|_| None).collect();
        for (i, entry) in window.slots.iter().enumerate() {
            if let Some(d) = entry {
                slots[i] = map_item_by_id(ctx, d.runtime_id, i32::from(d.count));
            }
        }
        for (i, entry) in bridge.items.lock().unwrap().iter().enumerate() {
            let Some(d) = entry else { continue };
            let Some(java) = window.kind.java_player_slot(i as u8) else {
                continue;
            };
            if let Ok(index) = usize::try_from(java) {
                if index < slots.len() {
                    slots[index] = map_item_by_id(ctx, d.runtime_id, i32::from(d.count));
                }
            }
        }
        Event::Packet(Box::new(play::Clientbound::ContainerSetContent(
            play::ContainerSetContent {
                container: i32::from(window.id),
                state: 0,
                items: slots,
                carried: None,
            },
        )))
    }

    /// One of the player's own slots, shown inside the open window's menu.
    fn window_player_slot(
        ctx: &Context,
        bridge: &Bridge,
        window: &OpenWindow,
        bedrock_slot: u8,
    ) -> Option<Event> {
        let java = window.kind.java_player_slot(bedrock_slot)?;
        let item = bridge
            .items
            .lock()
            .unwrap()
            .get(usize::from(bedrock_slot))
            .copied()
            .flatten();
        Some(Event::Packet(Box::new(
            play::Clientbound::ContainerSetSlot(play::ContainerSetSlot {
                container: i32::from(window.id),
                state: 0,
                slot: java,
                item: item.and_then(|d| map_item_by_id(ctx, d.runtime_id, i32::from(d.count))),
            }),
        )))
    }

    /// Where a shift-click sends a stack: a stack of the same item with room
    /// first, then the first empty slot, and how many items fit. The hotbar is
    /// filled before the main inventory, and anything worn goes back the same
    /// way. With a container open the two sides are the container and the
    /// player.
    /// The Java slot an item is worn in, by its name: armour by suffix, an
    /// elytra in the chest slot, a shield in the off hand.
    fn worn_slot(bridge: &Bridge, runtime_id: i16) -> Option<i16> {
        let names = bridge.item_names.lock().unwrap();
        let name = names.get(&runtime_id)?;
        let short = name.strip_prefix("minecraft:").unwrap_or(name);
        Some(match short {
            "shield" => 45,
            "elytra" => 6,
            "turtle_helmet" | "carved_pumpkin" => 5,
            _ if short.ends_with("_helmet") => 5,
            _ if short.ends_with("_chestplate") => 6,
            _ if short.ends_with("_leggings") => 7,
            _ if short.ends_with("_boots") => 8,
            _ => return None,
        })
    }

    fn quick_move_target(
        bridge: &Bridge,
        from: bp::SlotRef,
        item: bedrock_core::protocol::ItemDescriptor,
    ) -> Option<(bp::SlotRef, u8)> {
        let window = bridge.window.lock().unwrap();
        let (container, region): (u8, Vec<u8>) = match window.as_ref() {
            // Out of the open container and into the player's inventory.
            Some(w) if w.name.map(|n| n.name) == Some(from.container.name) => {
                (CONTAINER_COMBINED, (0..36).collect())
            }
            Some(w) => {
                let name = w.name.unwrap_or_default().name;
                (name, (0..w.kind.slots() as u8).collect())
            }
            None => match from.container.name {
                // The hotbar first, then the rest of the inventory, so a
                // shift-click always fills the quick-access row before it
                // spills into the main slots.
                CONTAINER_COMBINED => (
                    CONTAINER_COMBINED,
                    (0..9).chain(9..36).filter(|&s| s != from.slot).collect(),
                ),
                // Armour and the off hand go the same way.
                _ => (CONTAINER_COMBINED, (0..9).chain(9..36).collect()),
            },
        };
        drop(window);
        // A stack with room first, then the first empty slot, like the game.
        for &slot in &region {
            let Some(existing) = item_at(bridge, container, slot) else {
                continue;
            };
            if existing.runtime_id != item.runtime_id {
                continue;
            }
            let room = 64u8.saturating_sub(existing.count.max(0) as u8);
            let count = (item.count.max(0) as u8).min(room);
            if count > 0 {
                return Some((inventory_slot_ref(bridge, container, slot), count));
            }
        }
        for slot in region {
            if item_at(bridge, container, slot).is_none() {
                return Some((
                    inventory_slot_ref(bridge, container, slot),
                    item.count.max(1) as u8,
                ));
            }
        }
        None
    }

    /// One click in the game's inventory, turned into the move the server wants.
    /// The server owns the inventory, so nothing can be changed without asking
    /// it with the exact slots involved.
    fn container_click(bridge: &Bridge, c: &play::ContainerClick) -> Vec<bp::Out> {
        let request_id = bridge.request_id.fetch_sub(2, Ordering::Relaxed);
        let cursor = slot_ref(bridge, CONTAINER_CURSOR, 0);
        let clicked = clicked_slot(bridge, c);
        let actions = match c.input {
            // Pick up: the stack, or one item of it with the right button.
            0 => {
                let Some(clicked) = clicked else {
                    return Vec::new();
                };
                let held = *bridge.cursor.lock().unwrap();
                let here = item_at(bridge, clicked.container.name, clicked.slot);
                let count = if c.button == 1 {
                    1
                } else {
                    here.map_or(0, |d| d.count.max(0) as u8)
                };
                match (held, here) {
                    (None, Some(_)) => vec![bp::StackAction::Take {
                        count,
                        source: clicked,
                        destination: cursor,
                    }],
                    (Some(holding), Some(existing))
                        if holding.runtime_id == existing.runtime_id =>
                    {
                        vec![bp::StackAction::Place {
                            count: if c.button == 1 {
                                1
                            } else {
                                holding.count.max(0) as u8
                            },
                            source: cursor,
                            destination: clicked,
                        }]
                    }
                    (Some(_), Some(_)) => vec![bp::StackAction::Swap {
                        first: clicked,
                        second: cursor,
                    }],
                    (Some(holding), None) => vec![bp::StackAction::Place {
                        count: holding.count.max(0) as u8,
                        source: cursor,
                        destination: clicked,
                    }],
                    (None, None) => Vec::new(),
                }
            }
            // Shift-click: move the stack towards the other side of the window.
            // A worn item goes to its armour or off-hand slot instead: onto an
            // empty one, or swapping with whatever is already worn there.
            1 => {
                let Some(clicked) = clicked else {
                    return Vec::new();
                };
                let Some(item) = item_at(bridge, clicked.container.name, clicked.slot) else {
                    return Vec::new();
                };
                let worn = worn_slot(bridge, item.runtime_id).and_then(|java| {
                    let (container, slot) = if java == 45 {
                        (CONTAINER_OFFHAND, 1u8)
                    } else {
                        (CONTAINER_ARMOR, (java - 5) as u8)
                    };
                    // Clicking the worn slot itself hands the item back.
                    (clicked.container.name != container || clicked.slot != slot)
                        .then_some((container, slot))
                });
                if let Some((container, slot)) = worn {
                    let to = slot_ref(bridge, container, slot);
                    if item_at(bridge, container, slot).is_none() {
                        vec![bp::StackAction::Take {
                            count: item.count.max(1) as u8,
                            source: clicked,
                            destination: to,
                        }]
                    } else {
                        vec![bp::StackAction::Swap {
                            first: clicked,
                            second: to,
                        }]
                    }
                } else if let Some((to, count)) = quick_move_target(bridge, clicked, item) {
                    vec![bp::StackAction::Take {
                        count,
                        source: clicked,
                        destination: to,
                    }]
                } else {
                    Vec::new()
                }
            }
            // A number key: move the slot to that hotbar slot. The server
            // refuses a swap whose destination holds nothing, so a move onto an
            // empty slot is a take; only two occupied slots swap.
            2 => {
                let Some(clicked) = clicked else {
                    return Vec::new();
                };
                let hotbar = c.button.clamp(0, 8) as u8;
                let to = player_slot_ref(bridge, hotbar);
                let here = item_at(bridge, clicked.container.name, clicked.slot);
                let there = item_at(bridge, to.container.name, to.slot);
                match (here, there) {
                    (Some(item), None) => vec![bp::StackAction::Take {
                        count: item.count.max(0) as u8,
                        source: clicked,
                        destination: to,
                    }],
                    (None, Some(item)) => vec![bp::StackAction::Take {
                        count: item.count.max(0) as u8,
                        source: to,
                        destination: clicked,
                    }],
                    (Some(_), Some(_)) => vec![bp::StackAction::Swap {
                        first: clicked,
                        second: to,
                    }],
                    (None, None) => Vec::new(),
                }
            }
            // Throwing an item out of a slot, or out of the cursor when the
            // click is outside the window.
            4 => {
                if c.slot == -999 {
                    let Some(holding) = *bridge.cursor.lock().unwrap() else {
                        return Vec::new();
                    };
                    // Outside the window the button is the mouse's: a left click
                    // drops the whole carried stack, a right click one.
                    vec![bp::StackAction::Drop {
                        count: if c.button == 1 {
                            1
                        } else {
                            holding.count.max(0) as u8
                        },
                        source: cursor,
                        randomly: false,
                    }]
                } else {
                    let Some(clicked) = clicked else {
                        return Vec::new();
                    };
                    let Some(item) = item_at(bridge, clicked.container.name, clicked.slot) else {
                        return Vec::new();
                    };
                    // On a slot the button says whether a modifier is held: plain
                    // Q (0) drops one, Ctrl+Q (1) the whole stack.
                    vec![bp::StackAction::Drop {
                        count: if c.button == 1 {
                            item.count.max(0) as u8
                        } else {
                            1
                        },
                        source: clicked,
                        randomly: false,
                    }]
                }
            }
            // Dragging: a left drag takes one item from each slot it crosses to
            // the cursor, a right drag leaves one from the cursor in each slot.
            // The start and end clicks (buttons 0/4 and 2/6) move nothing.
            5 => {
                let Some(clicked) = clicked else {
                    return Vec::new();
                };
                match c.button {
                    // Left drag, a slot crossed: one item to the cursor.
                    1 => {
                        if item_at(bridge, clicked.container.name, clicked.slot).is_none() {
                            return Vec::new();
                        }
                        vec![bp::StackAction::Take {
                            count: 1,
                            source: clicked,
                            destination: cursor,
                        }]
                    }
                    // Right drag, a slot crossed: one item from the cursor. An
                    // empty slot is a valid target here.
                    5 => {
                        let held = *bridge.cursor.lock().unwrap();
                        let Some(holding) = held else {
                            return Vec::new();
                        };
                        let here = item_at(bridge, clicked.container.name, clicked.slot);
                        match here {
                            Some(existing) if existing.runtime_id != holding.runtime_id => {
                                Vec::new()
                            }
                            _ => vec![bp::StackAction::Place {
                                count: 1,
                                source: cursor,
                                destination: clicked,
                            }],
                        }
                    }
                    _ => Vec::new(),
                }
            }
            // Double-click: collect the stacks that match what is on the
            // cursor, one slot at a time.
            6 => {
                let Some(clicked) = clicked else {
                    return Vec::new();
                };
                // Pick-all, the way a real client gathers: every stack of the
                // item the cursor holds (or, with an empty cursor, the clicked
                // slot's) goes to the cursor until it is a full stack. The
                // clicked slot is often empty already: the item is on the
                // cursor.
                let here = item_at(bridge, clicked.container.name, clicked.slot);
                let held = *bridge.cursor.lock().unwrap();
                let Some(target) = held.or(here) else {
                    return Vec::new();
                };
                let mut room = 64i16 - held.map_or(0, |h| h.count.max(0));
                if room <= 0 {
                    return Vec::new();
                }
                // The clicked slot first, then the player's own slots, then the
                // open container's.
                let mut slots: Vec<bp::SlotRef> = vec![clicked];
                for slot in 0..36u8 {
                    let at = player_slot_ref(bridge, slot);
                    if at.container.name != clicked.container.name || at.slot != clicked.slot {
                        slots.push(at);
                    }
                }
                if let Some(window) = bridge.window.lock().unwrap().as_ref() {
                    let name = window.name.unwrap_or_default();
                    for slot in 0..window.kind.slots() as u8 {
                        slots.push(bp::SlotRef {
                            container: name,
                            slot,
                            stack_id: 0,
                        });
                    }
                }
                let mut actions = Vec::new();
                // Once the first take has landed, the cursor is a stack this
                // request created, so later takes name it by the request id.
                let mut cursor_id = cursor.stack_id;
                for at in slots {
                    if room <= 0 {
                        break;
                    }
                    let Some(item) = item_at(bridge, at.container.name, at.slot) else {
                        continue;
                    };
                    if item.runtime_id != target.runtime_id || item.count <= 0 {
                        continue;
                    }
                    let take = item.count.min(room);
                    room -= take;
                    actions.push(bp::StackAction::Take {
                        count: take as u8,
                        source: slot_ref(bridge, at.container.name, at.slot),
                        destination: bp::SlotRef {
                            container: cursor.container,
                            slot: cursor.slot,
                            stack_id: cursor_id,
                        },
                    });
                    cursor_id = request_id;
                }
                actions
            }
            _ => Vec::new(),
        };
        if actions.is_empty() {
            return Vec::new();
        }
        // Show the move at once and keep the state before it: the server's
        // answer only corrects the counts and the stack ids, and a refusal puts
        // the snapshot back.
        let before = snapshot(bridge);
        for action in &actions {
            apply_action(bridge, action);
        }
        {
            let mut pending = bridge.pending_moves.lock().unwrap();
            pending.push((request_id, before));
            if pending.len() > 64 {
                pending.remove(0);
            }
        }
        note_pending(bridge, request_id, &actions);
        if c.input == 1 {
            // A quick move chooses its destination on both sides; the answer
            // reconciles the whole menu so the other guess cannot survive as a
            // second copy.
            let mut quick = bridge.quick_moves.lock().unwrap();
            if quick.len() > 64 {
                quick.clear();
            }
            quick.insert(request_id);
        }
        vec![bp::Out::ItemStackRequest {
            request_id,
            actions,
        }]
    }

    /// The server slot a click refers to, if the bridge knows it. A click
    /// outside the window has no slot.
    fn clicked_slot(bridge: &Bridge, c: &play::ContainerClick) -> Option<bp::SlotRef> {
        if c.slot < 0 {
            return None;
        }
        click_slot(bridge, c.container, c.slot)
    }

    /// The server's answer to a move. It names the slots it settled on, which is
    /// where the counts come from; a refusal means the game has to be shown the
    /// truth again.
    fn stack_response(
        ctx: &Context,
        bridge: &Bridge,
        response: &bp::ItemStackResponse,
    ) -> Vec<Event> {
        // The move was shown when it was sent, so its copy is no longer needed;
        // a refusal puts it back before the truth is shown again.
        let before = {
            let mut pending = bridge.pending_moves.lock().unwrap();
            pending
                .iter()
                .position(|(id, _)| *id == response.request_id)
                .map(|index| pending.remove(index).1)
        };
        if response.result != 0 {
            bedrock_core::nethernet::trace(format!(
                "inventory move {} refused: result {}",
                response.request_id, response.result
            ));
            if let Some(before) = before {
                restore(bridge, before);
            }
            // The refused move changed nothing. The model is back to the
            // snapshot, whose ids the server knows, so every outstanding name
            // goes: one that is never answered would wedge its slot for good.
            bridge.pending_ids.lock().unwrap().clear();
            return vec![inventory_content(ctx, bridge)];
        }
        let mut events = Vec::new();
        for container in &response.containers {
            let name = container.container.name;
            for slot in &container.slots {
                let count = i16::from(slot.count);
                let existing = item_at(bridge, name, slot.slot);
                let settled = if count <= 0 {
                    None
                } else {
                    existing.map(|mut d| {
                        d.count = count;
                        d.net_id = Some(slot.stack_id as u32);
                        d
                    })
                };
                set_item_at(bridge, name, slot.slot, settled);
                {
                    // This slot's unacknowledged change is answered now, so it
                    // goes back to naming the server's stack id.
                    let mut pending = bridge.pending_ids.lock().unwrap();
                    let key = (name, slot.slot);
                    if pending.get(&key) == Some(&response.request_id) {
                        pending.remove(&key);
                    }
                }
                if std::env::var_os("HZ_INV_TRACE").is_some() {
                    bedrock_core::nethernet::trace(format!(
                        "INV OUT id={} container={name} slot={} count={count} settled={}",
                        response.request_id,
                        slot.slot,
                        settled.map_or_else(
                            || "none".to_string(),
                            |d| format!("runtime={} count={} id={:?}", d.runtime_id, d.count, d.net_id)
                        ),
                    ));
                }
                if name == CONTAINER_CURSOR {
                    events.push(Event::Packet(Box::new(play::Clientbound::SetCursorItem(
                        play::SetCursorItem {
                            item: settled.and_then(|d| {
                                map_item_by_id(ctx, d.runtime_id, i32::from(d.count))
                            }),
                        },
                    ))));
                    continue;
                }
                let Some(java) = java_slot_of(name, slot.slot) else {
                    continue;
                };
                events.push(Event::Packet(Box::new(
                    play::Clientbound::ContainerSetSlot(play::ContainerSetSlot {
                        container: 0,
                        state: 0,
                        slot: java,
                        item: settled
                            .and_then(|d| map_item_by_id(ctx, d.runtime_id, i32::from(d.count))),
                    }),
                )));
            }
        }
        // A successful answer names every slot it changed, so any slot still
        // counted against this request was not settled and its predicted copy
        // has to be removed.
        if let Some(before) = &before {
            let touched: Vec<(u8, u8)> = {
                let pending = bridge.pending_ids.lock().unwrap();
                pending
                    .iter()
                    .filter(|(_, id)| **id == response.request_id)
                    .map(|(key, _)| *key)
                    .collect()
            };
            let mut named = std::collections::HashSet::new();
            for container in &response.containers {
                for slot in &container.slots {
                    named.insert((container.container.name, slot.slot));
                }
            }
            let mut ghost = false;
            for (container, slot) in touched {
                if named.contains(&(container, slot)) {
                    continue;
                }
                // The move settled somewhere other than where it was drawn
                // here, so the game is showing a copy the server does not have.
                ghost = true;
                restore_slot(bridge, before, container, slot);
                bridge.pending_ids.lock().unwrap().remove(&(container, slot));
            }
            // Unanswered, stale names must not linger into the next move.
            let mut pending = bridge.pending_ids.lock().unwrap();
            pending.retain(|_, id| *id != response.request_id);
            drop(pending);
            if ghost {
                // The game's own prediction can have put the stack in a slot the
                // bridge never named, so patching single slots leaves a second
                // copy behind. One whole-menu update from the bridge's model
                // rewrites every slot and takes any stray copy with it.
                events.push(inventory_content(ctx, bridge));
            }
        }
        // A quick move is the one click whose destination both sides guess, so
        // its answer always rewrites the whole menu.
        if bridge
            .quick_moves
            .lock()
            .unwrap()
            .remove(&response.request_id)
        {
            events.push(inventory_content(ctx, bridge));
        }
        events
    }

    /// The Bedrock input flags and move vector for one tick. Held keys come
    /// from the game's `PlayerInput` bitfield; the ones that are events rather
    /// than states (jump, sneak, sprint) are reported only on the tick they
    /// change, which is what the server's movement simulation expects.
    #[allow(clippy::too_many_arguments)]
    fn input_bits(
        input: u8,
        previous: u8,
        sprinting: bool,
        was_sprinting: bool,
        on_ground: bool,
        was_on_ground: bool,
        collision: bool,
    ) -> (u64, [f32; 2]) {
        let (forward, backward) = (input & 0x01 != 0, input & 0x02 != 0);
        let (left, right) = (input & 0x04 != 0, input & 0x08 != 0);
        let (jump, sneak) = (input & 0x10 != 0, input & 0x20 != 0);
        let (was_jump, was_sneak) = (previous & 0x10 != 0, previous & 0x20 != 0);

        // block_breaking_delay_enabled is on every real client input.
        let mut bits = 1u64 << 48;
        // The server picks ground friction from these two; without them the
        // player is simulated as airborne and glides after the keys are let go.
        if on_ground {
            bits |= 1 << 50; // vertical_collision
        }
        if collision {
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
            if was_on_ground {
                bits |= 1 << 31; // start_jumping
            }
            if !was_jump {
                bits |= 1 << 60; // jump_pressed_raw
            }
        } else if was_jump {
            bits |= 1 << 59; // jump_released_raw
        }
        if sneak {
            // sneaking, sneak_down, want_down (sneak_current_raw is bit 64,
            // past the 64-bit flag set the encoder carries)
            bits |= (1 << 8) | (1 << 9) | (1 << 17);
            if !was_sneak {
                bits |= (1 << 63) | (1 << 27); // sneak_pressed_raw, start_sneaking
            }
        } else if was_sneak {
            bits |= (1 << 62) | (1 << 28); // sneak_released_raw, stop_sneaking
        }
        if sprinting {
            bits |= (1 << 4) | (1 << 20); // sprint_down, sprinting
            if !was_sprinting {
                bits |= 1 << 25; // start_sprinting
            }
        } else if was_sprinting {
            bits |= 1 << 26; // stop_sprinting
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

    /// The input for the client tick that just ended: one `PlayerAuthInput`,
    /// and the inventory resync a pickup asked for once its tick is due.
    pub fn tick_input(bridge: &Bridge) -> Vec<bp::Out> {
        // The bridge's own clock. Death animations and delayed removals are
        // scheduled against it, so it must advance once per client tick.
        bridge.tick.fetch_add(1, Ordering::Relaxed);
        let actions = std::mem::take(&mut *bridge.actions.lock().unwrap());
        let mut out = auth_input(bridge, actions);
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

    /// A block use, translated where the columns are: the state of the clicked
    /// block goes into the transaction as the server's check.
    pub fn use_item_on(ctx: &mut Context<'_>, bridge: &Bridge, u: &play::UseItemOn) -> Vec<bp::Out> {
        let block_id = match block_id_at(ctx, u.pos) {
            Some(id) => id,
            None => {
                let state = ctx
                    .chunks
                    .get(&(u.pos[0] >> 4, u.pos[2] >> 4))
                    .map_or(0, |chunk| chunk.get(u.pos[0] & 15, u.pos[1], u.pos[2]));
                ctx.blocks.as_mut().map_or(0, |blocks| blocks.bedrock(state))
            }
        };
        // A placement puts a new block there, and the server does not always
        // echo it back, so the client's next use of it must know its id.
        if let Some(item) = held_item(bridge) {
            if item.block_runtime_id > 0 {
                let normal = match u.face {
                    0 => [0, -1, 0],
                    1 => [0, 1, 0],
                    2 => [0, 0, -1],
                    3 => [0, 0, 1],
                    4 => [-1, 0, 0],
                    5 => [1, 0, 0],
                    _ => [0, 0, 0],
                };
                let target = if u.inside {
                    u.pos
                } else {
                    [
                        u.pos[0] + normal[0],
                        u.pos[1] + normal[1],
                        u.pos[2] + normal[2],
                    ]
                };
                store_block_id(ctx, target, item.block_runtime_id as u32);
            }
        }
        *bridge.last_use.lock().unwrap() = Some(u.pos);
        serverbound_with(
            bridge,
            &play::Serverbound::UseItemOn(u.clone()),
            block_id,
        )
    }

    /// Java serverbound → Bedrock.
    pub fn serverbound(bridge: &Bridge, packet: &play::Serverbound) -> Vec<bp::Out> {
        serverbound_with(bridge, packet, 0)
    }

    /// A block use also reports the clicked block's Bedrock state, which only
    /// the network thread can look up. Everything else passes zero.
    pub fn serverbound_with(
        bridge: &Bridge,
        packet: &play::Serverbound,
        block_id: u32,
    ) -> Vec<bp::Out> {
        match packet {
            // The game's render distance is the chunk radius asked of the
            // server, and the player can change it while playing, so a new
            // client-information packet re-requests it. Before the world has
            // started the radius is only remembered: the request goes out with
            // the rest of the start-up burst.
            play::Serverbound::ClientInformation(info) => {
                let radius = i32::from(info.view_distance).max(2);
                bridge.requested_radius.store(radius, Ordering::Relaxed);
                // The streamed radius is the smaller of this setting and the
                // grant; a setting past the grant is not streamed, since the
                // game drops columns past its own render distance anyway.
                let _ = super::store_effective_radius(bridge);
                bridge.radius_dirty.store(true, Ordering::Relaxed);
                if bridge.runtime_id.load(Ordering::Relaxed) != 0 {
                    vec![bp::Out::RequestChunkRadius { radius }]
                } else {
                    Vec::new()
                }
            }
            // Movement and input only update the bridge state; one
            // `PlayerAuthInput` goes out at the end of the client tick with all
            // of it, so the server's input tick and ours stay the same tick.
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
            // One input per client tick, sent when the tick ends. A synthetic
            // tick counter advances with them, instead of a wall clock that
            // drifts against the server's simulation.
            play::Serverbound::ClientTickEnd(_) => {
                if bridge.runtime_id.load(Ordering::Relaxed) == 0 {
                    return Vec::new();
                }
                tick_input(bridge)
            }
            // The game changes sprint state with a player command; the input
            // bitfield only carries the sprint key.
            play::Serverbound::PlayerCommand(c) => {
                match c.action {
                    1 => bridge.sprinting.store(true, Ordering::Relaxed),
                    2 => bridge.sprinting.store(false, Ordering::Relaxed),
                    _ => {}
                }
                Vec::new()
            }
            // The game asks to respawn when the death screen is dismissed.
            // Bedrock respawns through its own handshake, sent as a client that
            // is ready to spawn, and the server then places the player.
            play::Serverbound::ClientCommand(c) if c.action == 0 => {
                bedrock_core::nethernet::trace("respawn: client command 0".to_string());
                bridge.respawn_asked.store(true, Ordering::Relaxed);
                let mut out = Vec::new();
                if let Some(spawn) = bridge.spawn_info.lock().unwrap().clone() {
                    let _ = bridge.events.send(Event::Packet(Box::new(
                        play::Clientbound::Respawn(play::Respawn { spawn, keep: 0 }),
                    )));
                }
                let runtime_id = bridge.runtime_id.load(Ordering::Relaxed).max(0) as u64;
                // The client-ready respawn carries no position; the server
                // places the player where its own spawn is.
                out.push(bp::Out::Respawn {
                    position: [0.0; 3],
                    state: 2,
                    runtime_id,
                });
                out
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
                        position: pos,
                        click: [0.0; 3],
                    },
                ]
            }
            // Right-clicking an entity: the same transaction with the
            // interact action, which is what feeds, milks and saddles.
            play::Serverbound::Interact(i) => {
                let pos = *bridge.position.lock().unwrap();
                vec![bp::Out::ItemUseOnEntity {
                    runtime_id: i.id as u64,
                    action: 0, // interact
                    hotbar_slot: bridge.hotbar.load(Ordering::Relaxed) as i32,
                    item: held_item(bridge),
                    position: pos,
                    click: [0.0; 3],
                }]
            }
            play::Serverbound::SetCarriedItem(i) => {
                bridge.hotbar.store(i64::from(i.slot), Ordering::Relaxed);
                vec![bp::Out::MobEquipment {
                    runtime_id: bridge.runtime_id.load(Ordering::Relaxed) as u64,
                    slot: i.slot as u8,
                    window_id: 0,
                    item: held_item(bridge),
                }]
            }
            // Placing or using an item on a block: Bedrock's item-use
            // inventory transaction with a place action.
            play::Serverbound::UseItemOn(u) => {
                let pos = *bridge.position.lock().unwrap();
                let runtime_id = bridge.runtime_id.load(Ordering::Relaxed) as u64;
                // The packet must describe the slot as the server still has
                // it; the tally moves only after that.
                let held = held_item(bridge);
                bedrock_core::nethernet::trace(format!(
                    "use item on {:?} face={} inside={} hotbar={} item={:?} block={block_id}",
                    u.pos,
                    u.face,
                    u.inside,
                    bridge.hotbar.load(Ordering::Relaxed),
                    held
                ));
                // A block is consumed when it is placed. An item such as a
                // bow, shield or firework is only started, and the count must
                // not change.
                let placing = held.as_ref().is_some_and(|item| item.block_runtime_id > 0);
                if placing && let Some(from) = held {
                    let slot = bridge.hotbar.load(Ordering::Relaxed).max(0) as usize;
                    let mut items = bridge.items.lock().unwrap();
                    if let Some(entry) = items.get_mut(slot) {
                        *entry = if from.count > 1 {
                            Some(bedrock_core::protocol::ItemDescriptor {
                                count: from.count - 1,
                                ..from
                            })
                        } else {
                            None
                        };
                    }
                }
                let normal = match u.face {
                    0 => [0, -1, 0],
                    1 => [0, 1, 0],
                    2 => [0, 0, -1],
                    3 => [0, 0, 1],
                    4 => [-1, 0, 0],
                    5 => [1, 0, 0],
                    _ => [0, 0, 0],
                };
                // The server wants the block use bracketed by the start and
                // stop actions; the transaction alone leaves it half-open.
                let result = if u.inside {
                    u.pos
                } else {
                    [
                        u.pos[0] + normal[0],
                        u.pos[1] + normal[1],
                        u.pos[2] + normal[2],
                    ]
                };
                let mut out = vec![
                    // Placing or using swings the arm, as a hit does.
                    bp::Out::Animate {
                        runtime_id,
                        action: 1, // swing_arm
                    },
                    bp::Out::PlayerAction {
                        runtime_id,
                        action: 28, // start item use on
                        position: u.pos,
                        result,
                        face: u.face,
                    },
                ];
                if !placing {
                    // A bow, shield, firework or other usable item starts its
                    // use animation. A real client reports this as action 37.
                    out.push(bp::Out::PlayerAction {
                        runtime_id,
                        action: 37, // start using item
                        position: u.pos,
                        result: [0; 3],
                        face: 255,
                    });
                }
                out.push(bp::Out::ItemUseOnBlock {
                    block: u.pos,
                    face: u.face,
                    hotbar_slot: bridge.hotbar.load(Ordering::Relaxed) as i32,
                    action: if placing { 0 } else { 1 }, // place / use
                    item: held,
                    cursor: u.cursor,
                    // The transaction carries the feet position, not the eye:
                    // the server measures reach from here.
                    position: pos,
                    block_id,
                });
                out.push(bp::Out::PlayerAction {
                    runtime_id,
                    action: 29, // stop item use on
                    position: u.pos,
                    result: [0; 3],
                    face: 0,
                });
                out.push(
                    // The stack shrank; the held item the server shows must say
                    // so, or everyone else keeps seeing the old count.
                    bp::Out::MobEquipment {
                        runtime_id,
                        slot: bridge.hotbar.load(Ordering::Relaxed).max(0) as u8,
                        window_id: 0,
                        item: held_item(bridge),
                    },
                );
                out
            }
            // Using the held item in the air (eating, throwing, drawing,
            // raising): the same transaction with a use action and no block.
            play::Serverbound::UseItem(_) => {
                let pos = *bridge.position.lock().unwrap();
                let runtime_id = bridge.runtime_id.load(Ordering::Relaxed) as u64;
                vec![
                    bp::Out::PlayerAction {
                        runtime_id,
                        action: 37, // start using item
                        position: [0; 3],
                        result: [0; 3],
                        face: 255,
                    },
                    bp::Out::ItemUseOnBlock {
                        block: [0, 0, 0],
                        face: 255,
                        hotbar_slot: bridge.hotbar.load(Ordering::Relaxed) as i32,
                        action: 1, // use
                        item: held_item(bridge),
                        cursor: [0.0; 3],
                        position: pos,
                        block_id: 0,
                    },
                ]
            }
            // The game closed a container: tell the server, which is the only
            // side that can actually close it.
            play::Serverbound::ContainerClose(c) => {
                let window = bridge.window.lock().unwrap().take();
                match window {
                    Some(w) if i32::from(w.id) == c.container => {
                        vec![bp::Out::ContainerClose {
                            window_id: w.id,
                            window_type: w.bedrock_type,
                        }]
                    }
                    _ => Vec::new(),
                }
            }
            // A click in the game's inventory: it becomes the move the server
            // wants, since the server owns the inventory.
            play::Serverbound::ContainerClick(c) => {
                bedrock_core::nethernet::trace(format!(
                    "INV click container={} slot={} button={} input={}",
                    c.container, c.slot, c.button, c.input
                ));
                container_click(bridge, c)
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
                    position: pos,
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
                            position: pos,
                        }];
                    }
                    // RELEASE_USE_ITEM: the use has ended (food, a bow, a
                    // shield). Bedrock ends it with an item-release
                    // transaction, and without one the server keeps the item
                    // in use: the arm stays out and the shield stays up.
                    6 => {
                        let pos = *bridge.position.lock().unwrap();
                        let item = held_item(bridge);
                        bedrock_core::nethernet::trace(format!(
                            "RELEASE use slot={} item={item:?}",
                            bridge.hotbar.load(Ordering::Relaxed)
                        ));
                        return vec![bp::Out::ItemUseRelease {
                            // `ItemReleaseActionType::Release`; the server decides
                            // from the ticks whether a consumable was finished.
                            action: 0,
                            hotbar_slot: bridge.hotbar.load(Ordering::Relaxed) as i32,
                            item,
                            position: pos,
                        }];
                    }
                    _ => return Vec::new(),
                };
                bridge.actions.lock().unwrap().push(bp::BlockAction {
                    action,
                    position: p.pos,
                    face: i32::from(p.direction),
                });
                Vec::new()
            }
            play::Serverbound::Chat(c) => vec![bp::Out::Text {
                kind: 1,
                source: String::new(),
                message: c.message.clone(),
            }],
            // A typed command is a `CommandRequest`, not chat: the server does
            // not parse a leading slash out of a `Text` packet.
            play::Serverbound::ChatCommand(c) => {
                if std::env::var_os("HZ_CMD_TRACE").is_some() {
                    bedrock_core::nethernet::trace(format!(
                        "SEND command request: /{} player={}",
                        c.command,
                        bridge.unique_id.load(Ordering::Relaxed)
                    ));
                }
                vec![bp::Out::CommandRequest {
                    command: format!("/{}", c.command),
                    player_id: bridge.unique_id.load(Ordering::Relaxed),
                    numeric: super::command_numeric_layout(),
                    uuid: None,
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
