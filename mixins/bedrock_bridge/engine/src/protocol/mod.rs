//! Bedrock packet dispatch: turning a RakNet payload (a *batch* of packets) into
//! typed packets, and the reverse for the packets the bridge sends.
//!
//! A batch payload is a run of `uvarint length` + packet bytes, preceded — after
//! `NetworkSettings` turns compression on — by a zlib stream. The packet's own
//! id is the first unsigned varint inside each entry.

pub mod ids;
pub mod packets;

pub use packets::*;

use crate::error::Result;
use crate::stream::{Reader, Writer};

/// `CurrentCmdVersion::Latest` for 1.26.60: the command syntax revision a
/// `CommandRequest` is parsed with.
const COMMAND_VERSION_LATEST: i32 = 55;

/// A fresh instance UUID for a command, which the server echoes back in the
/// output. Not cryptographic: it only has to be unique per request.
fn command_uuid() -> [u8; 16] {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let mut state = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ^ std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e37_79b9_7f4a_7c15);
    let mut out = [0u8; 16];
    for word in out.chunks_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        word.copy_from_slice(&state.to_le_bytes());
    }
    // Version 4 (random) and the RFC 4122 variant, as a uuid should be.
    out[6] = (out[6] & 0x0f) | 0x40;
    out[8] = (out[8] & 0x3f) | 0x80;
    out
}

/// A decoded server→client packet. Unmodelled packets keep their raw bytes.
#[derive(Clone, Debug)]
pub enum Packet {
    NetworkSettings(NetworkSettings),
    PlayStatus(i32),
    /// The server's anti-cheat telling us one of our packets was malformed
    /// (156). `severity` 2 terminates the connection; `packet_id` is the
    /// offending packet.
    ViolationWarning {
        kind: i32,
        severity: i32,
        packet_id: i32,
        message: String,
    },
    Disconnect(Disconnect),
    ServerToClientHandshake(String),
    ResourcePacksInfo,
    ResourcePackStack,
    StartGame(StartGame),
    /// The item registry (162): `(identifier, network id)` for every item the
    /// inventory and equipment packets can name.
    ItemRegistry(Vec<(String, i16)>),
    AvailableEntityIdentifiers(crate::nbt::Tag),
    BiomeDefinitionList,
    CreativeContent,
    PlayerList(packets::PlayerList),
    LevelChunk(LevelChunk),
    SubChunk(SubChunk),
    NetworkChunkPublisherUpdate {
        position: [i32; 3],
        radius: u32,
    },
    ChunkRadiusUpdated(i32),
    AddPlayer(AddPlayer),
    AddEntity(AddEntity),
    RemoveEntity(u64),
    SetEntityData(SetEntityData),
    MovePlayer(MovePlayer),
    MoveEntityAbsolute(MoveEntityAbsolute),
    MoveActorDelta(MoveActorDelta),
    /// A dropped item lying in the world.
    AddItemEntity {
        runtime_id: u64,
        unique_id: u64,
        item: NetworkItem,
        position: [f32; 3],
        velocity: [f32; 3],
    },
    /// An item being picked up: the item entity and the actor taking it.
    TakeItemEntity {
        item: u64,
        actor: u64,
    },
    /// The server's authoritative correction of our own predicted position.
    CorrectPlayerMovePrediction {
        position: [f32; 3],
        rotation: [f32; 2],
        on_ground: bool,
        /// The `PlayerInputTick` this correction answers. ViaBedrock drops a
        /// correction whose tick has fallen outside the server's rewind window,
        /// because applying it would yank the client to an old position.
        tick: u64,
    },
    LevelEvent(LevelEvent),
    UpdateAttributes(UpdateAttributes),
    SetCommandsEnabled(bool),
    UpdateAbilities(UpdateAbilities),
    SetEntityMotion {
        runtime_id: u64,
        velocity: [f32; 3],
    },
    UpdateBlock {
        position: [i32; 3],
        runtime_id: u32,
        flags: u32,
    },
    /// The batched form of `UpdateBlock`: several blocks in one section.
    UpdateSubChunkBlocks {
        sub_chunk: [i32; 3],
        blocks: Vec<SubChunkBlock>,
    },
    InventoryContent(InventoryContent),
    InventorySlot(InventorySlot),
    /// The server's answers to the stack requests it has seen, as a batch.
    ItemStackResponses(Vec<ItemStackResponse>),
    ContainerOpen(ContainerOpen),
    ContainerClose {
        window_id: u8,
    },
    Text(Text),
    SetTime(i32),
    /// 1.26's level time: a list of world clocks, each with a tick count.
    SyncWorldClocks(Vec<WorldClock>),
    SetPlayerGameType(i32),
    UpdatePlayerGameType {
        gamemode: i32,
        runtime_id: u64,
    },
    SetHealth(i32),
    SetSpawnPosition {
        position: [i32; 3],
    },
    Respawn {
        position: [f32; 3],
        state: u8,
    },
    MobEquipment(MobEquipment),
    Animate {
        action: i32,
        runtime_id: u64,
    },
    BlockEntityData(BlockEntityData),
    SetDifficulty(i32),
    GameRulesChanged,
    PlayerHotbar {
        slot: u32,
        window_id: u8,
    },
    PlaySound,
    LevelSoundEvent,
    BossEvent,
    /// The command tree (76).
    AvailableCommands(AvailableCommands),
    /// The result of a `CommandRequest` (79).
    CommandOutput(CommandOutput),
    /// An actor state change. Death (3) and instant death (61) mean the entity
    /// is gone, whether or not the server removes it afterwards.
    ActorEvent {
        runtime_id: u64,
        event: i32,
        data: i32,
    },
    /// The packet id is known but its body is not modelled here.
    Raw(u32, Vec<u8>),
    /// The packet id is unknown to this build.
    Unknown(u32, Vec<u8>),
}

/// One entry of a batched `UpdateSubChunkBlocks`.
#[derive(Clone, Debug, PartialEq)]
pub struct SubChunkBlock {
    pub position: [i32; 3],
    pub runtime_id: u32,
    pub flags: u32,
}

/// A client→server packet the bridge can build.
#[derive(Clone, Debug)]
pub enum Out {
    RequestNetworkSettings {
        protocol: i32,
    },
    Login {
        auth_info: String,
        skin_json: String,
    },
    ClientToServerHandshake,
    ResourcePackClientResponse {
        status: u8,
        pack_ids: Vec<String>,
    },
    ClientCacheStatus {
        supported: bool,
    },
    /// Ask the server for specific sub-chunks (modern chunk delivery).
    SubChunkRequest {
        dimension: i32,
        center: [i32; 3],
        offsets: Vec<[i8; 3]>,
    },
    RequestChunkRadius {
        radius: i32,
    },
    SetLocalPlayerAsInitialized {
        runtime_id: u64,
    },
    /// Serverbound `respawn`; a client ready to spawn sends it after a death.
    Respawn {
        position: [f32; 3],
        state: u8,
        runtime_id: u64,
    },
    /// Serverbound `loading_screen`; ends the client's loading screen so the
    /// server starts simulating the player.
    LoadingScreen {
        r#type: i32,
    },
    TickSync {
        request: i64,
        response: i64,
    },
    /// Modern client→server movement and input. `MovePlayer` above is the old
    /// form, which a current server no longer reads: without this the server
    /// treats the player as standing still and keeps correcting them.
    PlayerAuthInput {
        position: [f32; 3],
        pitch: f32,
        yaw: f32,
        head_yaw: f32,
        /// Horizontal analog move vector, -1..1.
        move_vec: [f32; 2],
        /// Movement since the previous input (the server compares our claimed
        /// position against it).
        delta: [f32; 3],
        /// `PlayerAuthInputFlags` bits.
        flags: u64,
        /// Block actions for this tick, written when `flags` has
        /// `PERFORM_BLOCK_ACTIONS` (bit 35) set.
        block_actions: Vec<BlockAction>,
        tick: u64,
    },
    MovePlayer {
        runtime_id: u64,
        position: [f32; 3],
        pitch: f32,
        yaw: f32,
        head_yaw: f32,
        mode: u8,
        on_ground: bool,
    },
    /// A `PlayerAction`; the client acknowledges a completed respawn with
    /// action 7.
    PlayerAction {
        runtime_id: u64,
        action: i32,
        position: [i32; 3],
        result: [i32; 3],
        face: i32,
    },
    MobEquipment {
        runtime_id: u64,
        slot: u8,
        window_id: u8,
        item: Option<ItemDescriptor>,
    },
    /// Hitting or interacting with an entity. Bedrock does this as an
    /// `InventoryTransaction` of type item-use-on-entity, not a dedicated
    /// attack packet. `action` is the `ItemUseOnActorActionType` (1 = attack).
    ItemUseOnEntity {
        runtime_id: u64,
        action: i32,
        hotbar_slot: i32,
        item: Option<ItemDescriptor>,
        position: [f32; 3],
        click: [f32; 3],
    },
    /// One inventory move, as the server wants it: a request naming the slots
    /// it touches, answered by an `ItemStackResponse` with the same id.
    ItemStackRequest {
        request_id: i32,
        actions: Vec<StackAction>,
    },
    /// Closing a container window.
    ContainerClose {
        window_id: u8,
        window_type: u8,
    },
    /// Dropping an item: an `ItemStackRequest` with a drop action, which is how
    /// a current client drops (there is no drop flag on an inventory source).
    ItemRelease {
        slot: i32,
        count: u8,
        item: Option<ItemDescriptor>,
        position: [f32; 3],
    },
    ItemUseOnBlock {
        block: [i32; 3],
        face: i32,
        hotbar_slot: i32,
        action: i32,
        item: Option<ItemDescriptor>,
        cursor: [f32; 3],
        position: [f32; 3],
        /// The clicked block's own runtime id; the server checks the use
        /// against it.
        block_id: u32,
    },
    Text {
        kind: u8,
        source: String,
        message: String,
    },
    /// `CommandRequest` (77): a command the player typed, as `/name args`.
    /// `player_id` is the player's unique id (the origin's), and `numeric`
    /// picks the schema-wide numeric origin/version form; the name form is what
    /// a live BDS accepts.
    CommandRequest {
        command: String,
        player_id: i64,
        numeric: bool,
        /// The origin's instance uuid; a fresh one when absent.
        uuid: Option<[u8; 16]>,
    },
    Animate {
        runtime_id: u64,
        action: i32,
    },
    /// Ask the server to resend the inventories, because ours has drifted.
    InventoryMismatch,
    /// Serverbound disconnect: tells the server this client is leaving so it
    /// removes the player at once instead of waiting out the transport timeout.
    Disconnect {
        hide: bool,
        message: String,
    },
    /// An `Interact` (33). The client sends `open_inventory` (6) with its own
    /// runtime id to have the server open the player's inventory screen, which a
    /// stack request's cursor and `hotbar_and_inventory` container need.
    Interact {
        action: u8,
        target: u64,
    },
    /// A raw packet id + body, for anything not modelled above.
    Raw(u32, Vec<u8>),
}

/// Decode one packet entry (id + body) into a [`Packet`].
pub fn decode(id: u32, body: &[u8]) -> Result<Packet> {
    let mut r = Reader::new(body);
    Ok(match id {
        ids::NETWORK_SETTINGS => Packet::NetworkSettings(NetworkSettings::read(&mut r)?),
        ids::PLAY_STATUS => Packet::PlayStatus(r.i32_be()?),
        ids::VIOLATION_WARNING => Packet::ViolationWarning {
            kind: r.zigzag32()?,
            severity: r.zigzag32()?,
            packet_id: r.zigzag32()?,
            message: r.string()?,
        },
        ids::ACTOR_EVENT => {
            let runtime_id = r.varlong()?;
            let event = r.varint()? as i32;
            let data = r.varint()? as i32;
            Packet::ActorEvent {
                runtime_id,
                event,
                data,
            }
        }
        ids::DISCONNECT => Packet::Disconnect(Disconnect::read(&mut r)?),
        ids::SERVER_TO_CLIENT_HANDSHAKE => Packet::ServerToClientHandshake(r.string()?),
        ids::RESOURCE_PACKS_INFO => Packet::ResourcePacksInfo,
        ids::RESOURCE_PACK_STACK => Packet::ResourcePackStack,
        ids::START_GAME => Packet::StartGame(StartGame::read(&mut r)?),
        ids::ITEM_REGISTRY => Packet::ItemRegistry(read_item_palette(&mut r)?),
        ids::AVAILABLE_ENTITY_IDENTIFIERS => {
            Packet::AvailableEntityIdentifiers(crate::nbt::read_named(&mut r)?.1)
        }
        ids::BIOME_DEFINITION_LIST => Packet::BiomeDefinitionList,
        ids::CREATIVE_CONTENT => Packet::CreativeContent,
        ids::PLAYER_LIST => Packet::PlayerList(packets::PlayerList::read(&mut r)?),
        ids::FULL_CHUNK_DATA => Packet::LevelChunk(LevelChunk::read(&mut r)?),
        ids::SUB_CHUNK => Packet::SubChunk(SubChunk::read(&mut r)?),
        ids::NETWORK_CHUNK_PUBLISHER_UPDATE => {
            let position = [r.zigzag32()?, r.zigzag32()?, r.zigzag32()?];
            let radius = r.varint()?;
            Packet::NetworkChunkPublisherUpdate { position, radius }
        }
        ids::CHUNK_RADIUS_UPDATED => Packet::ChunkRadiusUpdated(r.zigzag32()?),
        ids::ADD_PLAYER => Packet::AddPlayer(AddPlayer::read(&mut r)?),
        ids::ADD_ENTITY => Packet::AddEntity(AddEntity::read(&mut r)?),
        ids::REMOVE_ENTITY => Packet::RemoveEntity(r.varlong()?),
        ids::SET_ENTITY_DATA => Packet::SetEntityData(SetEntityData::read(&mut r)?),
        ids::MOVE_PLAYER => Packet::MovePlayer(MovePlayer::read(&mut r)?),
        ids::MOVE_ENTITY_ABSOLUTE => Packet::MoveEntityAbsolute(MoveEntityAbsolute::read(&mut r)?),
        ids::MOVE_ACTOR_DELTA => Packet::MoveActorDelta(MoveActorDelta::read(&mut r)?),
        ids::CORRECT_PLAYER_MOVE_PREDICTION => {
            let kind = r.u8()?;
            let position = [r.f32()?, r.f32()?, r.f32()?];
            let _delta = [r.f32()?, r.f32()?, r.f32()?];
            let rotation = [r.f32()?, r.f32()?];
            if r.bool()? {
                let _angular_velocity = r.f32()?;
            }
            // `on_ground` and the tick follow only for a Player rewind; the
            // Vehicle form sends vehicle data instead.
            let (on_ground, tick) = if kind == 0 {
                (r.bool()?, r.varlong()? as u64)
            } else {
                (false, 0)
            };
            Packet::CorrectPlayerMovePrediction {
                position,
                rotation,
                on_ground,
                tick,
            }
        }
        ids::ADD_ITEM_ENTITY => {
            let unique_id = r.varlong()?;
            let runtime_id = r.varlong()?;
            let item = NetworkItem::read(&mut r)?;
            let position = [r.f32()?, r.f32()?, r.f32()?];
            let velocity = [r.f32()?, r.f32()?, r.f32()?];
            let _metadata = read_metadata(&mut r)?;
            let _from_fishing = r.bool()?;
            Packet::AddItemEntity {
                runtime_id,
                unique_id,
                item,
                position,
                velocity,
            }
        }
        ids::TAKE_ITEM_ENTITY => {
            let item = r.varlong()?;
            let actor = r.varlong()?;
            Packet::TakeItemEntity { item, actor }
        }
        ids::LEVEL_EVENT => Packet::LevelEvent(LevelEvent::read(&mut r)?),
        ids::UPDATE_ATTRIBUTES => Packet::UpdateAttributes(UpdateAttributes::read(&mut r)?),
        ids::SET_COMMANDS_ENABLED => Packet::SetCommandsEnabled(r.bool()?),
        ids::UPDATE_ABILITIES => Packet::UpdateAbilities(UpdateAbilities::read(&mut r)?),
        ids::SET_ENTITY_MOTION => {
            let runtime_id = r.varlong()?;
            let velocity = [r.f32()?, r.f32()?, r.f32()?];
            Packet::SetEntityMotion {
                runtime_id,
                velocity,
            }
        }
        ids::UPDATE_BLOCK => {
            let position = [r.zigzag32()?, r.zigzag32()?, r.zigzag32()?];
            let runtime_id = r.varint()?;
            let flags = r.varint()?;
            Packet::UpdateBlock {
                position,
                runtime_id,
                flags,
            }
        }
        ids::UPDATE_SUB_CHUNK_BLOCKS => {
            let sub_chunk = [r.zigzag32()?, r.zigzag32()?, r.zigzag32()?];
            let count = r.varint()? as usize;
            let mut blocks = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                let position = [r.zigzag32()?, r.zigzag32()?, r.zigzag32()?];
                let runtime_id = r.varint()?;
                let flags = r.varint()?;
                let _actor = r.zigzag64()?;
                let _message_actor = r.zigzag64()?;
                blocks.push(SubChunkBlock {
                    position,
                    runtime_id,
                    flags,
                });
            }
            Packet::UpdateSubChunkBlocks { sub_chunk, blocks }
        }
            ids::INVENTORY_CONTENT => Packet::InventoryContent(InventoryContent::read(&mut r)?),
            ids::ITEM_STACK_RESPONSE => {
                Packet::ItemStackResponses(read_stack_responses(&mut r)?)
            }
        ids::INVENTORY_SLOT => Packet::InventorySlot(InventorySlot::read(&mut r)?),
        ids::CONTAINER_OPEN => Packet::ContainerOpen(ContainerOpen::read(&mut r)?),
        ids::CONTAINER_CLOSE => Packet::ContainerClose { window_id: r.u8()? },
        ids::TEXT => Packet::Text(Text::read(&mut r)?),
        ids::SET_TIME => Packet::SetTime(r.zigzag32()?),
        ids::SYNC_WORLD_CLOCKS => Packet::SyncWorldClocks(WorldClock::read_all(&mut r)?),
        ids::SET_PLAYER_GAME_TYPE => Packet::SetPlayerGameType(r.zigzag32()?),
        ids::SET_HEALTH => Packet::SetHealth(r.zigzag32()?),
        ids::SET_SPAWN_POSITION => {
            let _spawn_type = r.zigzag32()?;
            let position = [r.zigzag32()?, r.zigzag32()?, r.zigzag32()?];
            Packet::SetSpawnPosition { position }
        }
        ids::RESPAWN => {
            let position = [r.f32()?, r.f32()?, r.f32()?];
            let state = r.u8()?;
            Packet::Respawn { position, state }
        }
        ids::MOB_EQUIPMENT => Packet::MobEquipment(MobEquipment::read(&mut r)?),
        ids::ANIMATE => {
            // 1.26: a byte action, the runtime id, a float, then the swing
            // source and hand bytes.
            let action = r.u8()?;
            let runtime_id = r.varlong()?;
            let _data = r.f32()?;
            let _swing_source = r.u8()?;
            let _hand = r.u8()?;
            Packet::Animate {
                action: i32::from(action),
                runtime_id,
            }
        }
        ids::BLOCK_ENTITY_DATA => Packet::BlockEntityData(BlockEntityData::read(&mut r)?),
        ids::SET_DIFFICULTY => Packet::SetDifficulty(r.zigzag32()?),
        ids::GAME_RULES_CHANGED => Packet::GameRulesChanged,
        ids::PLAYER_HOTBAR => {
            let slot = r.varint()?;
            let window_id = r.u8()?;
            Packet::PlayerHotbar { slot, window_id }
        }
        ids::PLAY_SOUND => Packet::PlaySound,
        ids::LEVEL_SOUND_EVENT => Packet::LevelSoundEvent,
        ids::BOSS_EVENT => Packet::BossEvent,
        ids::AVAILABLE_COMMANDS => {
            Packet::AvailableCommands(AvailableCommands::read(&mut r)?)
        }
        ids::COMMAND_OUTPUT => Packet::CommandOutput(CommandOutput::read(&mut r)?),
        ids::UPDATE_PLAYER_GAME_TYPE => {
            // UpdatePlayerGameType: gamemode varint, then the player's runtime id.
            let gamemode = r.zigzag32()?;
            let runtime_id = r.varlong()?;
            Packet::UpdatePlayerGameType {
                gamemode,
                runtime_id,
            }
        }
        unknown => Packet::Unknown(unknown, body.to_vec()),
    })
}

/// Split an uncompressed batch payload into parsed packets.
pub fn parse_batch(data: &[u8]) -> Result<Vec<Packet>> {
    let mut r = Reader::new(data);
    let mut out = Vec::new();
    while !r.is_empty() {
        let len = r.varint()? as usize;
        let entry = r.bytes(len)?;
        let mut e = Reader::new(entry);
        let id = e.varint()?;
        let body = e.rest().to_vec();
        if let Ok(dump) = std::env::var("NETHER_DUMP") {
            let want = dump == "all" || dump.parse::<u32>().ok() == Some(id);
            if want {
                use std::io::Write as _;
                let path = std::env::var_os("NETHER_DUMP_FILE")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::env::temp_dir().join("nether-raw-packets.log"));
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let hex: String = body.iter().map(|b| format!("{b:02x}")).collect();
                    let _ = writeln!(f, "{id} {} {hex}", body.len());
                }
            }
        }
        // A packet we cannot decode yet must not abort the whole stream: keep
        // the raw body so the caller can log it and carry on.
        match decode(id, &body) {
            Ok(packet) => out.push(packet),
            Err(_) => out.push(Packet::Raw(id, body)),
        }
    }
    Ok(out)
}

/// The fields of a network item stack descriptor the bridge can reproduce.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ItemDescriptor {
    pub runtime_id: i16,
    pub count: i16,
    pub damage: u32,
    /// The server's id for this stack. A stack request has to quote it back, so
    /// it is kept alongside the item everywhere the item is stored.
    pub net_id: Option<u32>,
    pub block_runtime_id: u32,
}

/// A slot a stack request acts on: which container, which slot in it, and which
/// stack the client believes is there.
#[derive(Clone, Copy, Debug)]
pub struct SlotRef {
    pub container: FullContainerName,
    pub slot: u8,
    pub stack_id: i32,
}

impl SlotRef {
    pub fn new(container: u8, slot: u8, stack_id: i32) -> Self {
        Self {
            container: FullContainerName {
                name: container,
                dynamic_id: None,
            },
            slot,
            stack_id,
        }
    }
}

/// One move in a stack request.
#[derive(Clone, Copy, Debug)]
pub enum StackAction {
    Take {
        count: u8,
        source: SlotRef,
        destination: SlotRef,
    },
    Place {
        count: u8,
        source: SlotRef,
        destination: SlotRef,
    },
    Swap {
        first: SlotRef,
        second: SlotRef,
    },
    Drop {
        count: u8,
        source: SlotRef,
        randomly: bool,
    },
    Destroy {
        count: u8,
        source: SlotRef,
    },
}

/// Write a network item stack descriptor: the id, count, damage, an empty
/// stack id, the block runtime id, and the user-data blob (an empty tag marker
/// plus two empty string lists), which the server expects even for no item.
/// The reader does not stop at a zero id, so an empty stack is a whole
/// descriptor too.
fn write_item(w: &mut Writer, item: Option<ItemDescriptor>) {
    let d = item.unwrap_or_default();
    w.i16(d.runtime_id).i16(d.count).varint(d.damage);
    // The stack id the server issued for this stack, when the bridge knows it;
    // a stack request that quotes the wrong id is rejected.
    match d.net_id {
        Some(id) => {
            w.bool(true);
            w.zigzag32(id as i32);
        }
        None => {
            w.bool(false);
        }
    }
    w.varint(d.block_runtime_id);
    w.varint(4); // user-data length: 2 + 1 + 1 bytes
    w.i16(0); // no tag
    w.varint(0); // can place: none
    w.varint(0); // can break: none
}

/// Write a slot a stack request acts on.
fn write_slot(w: &mut Writer, slot: &SlotRef) {
    w.u8(slot.container.name);
    match slot.container.dynamic_id {
        Some(id) => {
            w.bool(true);
            w.i32(id);
        }
        None => {
            w.bool(false);
        }
    }
    w.u8(slot.slot);
    // The stack id here is a plain little-endian int, unlike the varint the
    // item descriptors carry.
    w.i32(slot.stack_id);
}

/// Encode one outbound packet (id + body).
pub fn encode(packet: &Out) -> Vec<u8> {
    let mut w = Writer::new();
    match packet {
        Out::RequestNetworkSettings { protocol } => {
            // Bedrock sends the protocol as a big-endian i32 (unlike its other
            // integers, which are little-endian).
            w.varint(ids::REQUEST_NETWORK_SETTINGS);
            w.bytes(&protocol.to_be_bytes());
        }
        Out::Disconnect { hide, message } => {
            w.varint(ids::DISCONNECT);
            w.zigzag32(0);
            w.bool(*hide);
            if !hide {
                w.varint(message.len() as u32).bytes(message.as_bytes());
            }
        }
        Out::Login {
            auth_info,
            skin_json,
        } => {
            // protocol (big-endian), then the varint-framed, length-prefixed
            // (little-endian) authInfo and skin strings.
            w.varint(ids::LOGIN);
            w.bytes(&crate::PROTOCOL_VERSION.to_be_bytes());
            let mut body = Writer::new();
            let auth = auth_info.as_bytes();
            let skin = skin_json.as_bytes();
            body.i32(auth.len() as i32).bytes(auth);
            body.i32(skin.len() as i32).bytes(skin);
            w.byte_slice(&body.data);
        }
        Out::ClientToServerHandshake => {
            w.varint(ids::CLIENT_TO_SERVER_HANDSHAKE);
        }
        Out::ResourcePackClientResponse { status, pack_ids } => {
            // 1.26.40+: varint status (0 refused, 1 send_packs, 2 have_all_packs,
            // 3 completed), the same status again as a string, then pack ids
            // only for `send_packs`.
            // 0-based, with the names the server expects: 2 = downloadingfinished,
            // 3 = resourcepackstackfinished.
            let names = [
                "cancel",
                "downloading",
                "downloadingfinished",
                "resourcepackstackfinished",
            ];
            w.varint(ids::RESOURCE_PACK_CLIENT_RESPONSE);
            w.varint(*status as u32);
            w.string(names.get(*status as usize).copied().unwrap_or("cancel"));
            if *status == 1 {
                w.u16(pack_ids.len() as u16);
                for id in pack_ids {
                    w.string(id);
                }
            }
        }
        Out::ClientCacheStatus { supported } => {
            w.varint(ids::CLIENT_CACHE_STATUS).bool(*supported);
        }
        Out::SubChunkRequest {
            dimension,
            center,
            offsets,
        } => {
            w.varint(ids::SUB_CHUNK_REQUEST).zigzag32(*dimension);
            w.varint(offsets.len() as u32);
            for o in offsets {
                w.u8(o[0] as u8).u8(o[1] as u8).u8(o[2] as u8);
            }
            w.i32(center[0]).i32(center[1]).i32(center[2]);
        }
        Out::RequestChunkRadius { radius } => {
            // zigzag32 chunk radius, then the u8 max radius the client supports.
            w.varint(ids::REQUEST_CHUNK_RADIUS)
                .zigzag32(*radius)
                .u8((*radius).clamp(0, 255) as u8);
        }
        Out::SetLocalPlayerAsInitialized { runtime_id } => {
            w.varint(ids::SET_LOCAL_PLAYER_AS_INITIALIZED)
                .varlong(*runtime_id);
        }
        Out::Respawn {
            position,
            state,
            runtime_id,
        } => {
            w.varint(ids::RESPAWN)
                .f32(position[0])
                .f32(position[1])
                .f32(position[2])
                .u8(*state)
                .varlong(*runtime_id);
        }
        Out::LoadingScreen { r#type } => {
            w.varint(ids::LOADING_SCREEN);
            w.zigzag32(*r#type);
            w.bool(false); // no loading screen id
        }
        Out::TickSync { request, response } => {
            w.varint(ids::TICK_SYNC).i64(*request).i64(*response);
        }
        Out::PlayerAuthInput {
            position,
            pitch,
            yaw,
            head_yaw,
            move_vec,
            delta,
            flags,
            block_actions,
            tick,
        } => {
            w.varint(ids::PLAYER_AUTH_INPUT);
            let start = w.data.len();
            w.f32(*pitch);
            w.f32(*yaw);
            w.f32(position[0]);
            w.f32(position[1]);
            w.f32(position[2]);
            w.f32(move_vec[0]);
            w.f32(move_vec[1]);
            w.f32(*head_yaw);
            packets::write_input_flags(&mut w, *flags);
            w.varint(1); // InputMode: mouse and keyboard
            w.varint(0); // PlayMode: normal
            w.zigzag32(0); // InteractionModel: touch (what a real 1.26 client sends)
            w.f32(*pitch);
            w.f32(*yaw); // interact rotation: the view interactions are aimed with
            w.varlong(*tick);
            w.f32(delta[0]);
            w.f32(delta[1]);
            w.f32(delta[2]); // delta
            // The optional sections are each preceded by a presence bool. The
            // server wants these (movement fails without them); the block-action
            // payload itself still needs a captured frame to finish.
            let has_actions = *flags & (1 << 35) != 0;
            w.bool(false); // item interaction
            w.bool(false); // item stack request
            w.bool(has_actions);
            if has_actions {
                packets::write_block_actions(&mut w, block_actions);
            }
            w.bool(false); // vehicle rotation
            w.bool(false); // vehicle id
            w.f32(0.0);
            w.f32(0.0); // analog move vector: empty with mouse-and-keyboard input
            // Camera orientation: the world-space unit vector the view faces.
            let yaw_rad = f32::to_radians(*yaw);
            let pitch_rad = f32::to_radians(*pitch);
            let cos_pitch = pitch_rad.cos();
            w.f32(-yaw_rad.sin() * cos_pitch);
            w.f32(-pitch_rad.sin());
            w.f32(yaw_rad.cos() * cos_pitch); // camera orientation
            w.f32(move_vec[0]);
            w.f32(move_vec[1]); // raw move vector
            if !block_actions.is_empty() {
                crate::nethernet::trace(format!(
                    "AUTHIN flags={flags:#x} tick={tick} pos={position:?} actions={block_actions:?} bytes={:02x?}",
                    &w.data[start..]
                ));
            }
        }
        Out::MovePlayer {
            runtime_id,
            position,
            pitch,
            yaw,
            head_yaw,
            mode,
            on_ground,
        } => {
            w.varint(ids::MOVE_PLAYER);
            packets::write_move_player(
                &mut w,
                *runtime_id,
                *position,
                *pitch,
                *yaw,
                *head_yaw,
                *mode,
                *on_ground,
            );
        }
        Out::PlayerAction {
            runtime_id,
            action,
            position,
            result,
            face,
        } => {
            w.varint(ids::PLAYER_ACTION);
            w.varlong(*runtime_id).zigzag32(*action);
            w.zigzag32(position[0])
                .zigzag32(position[1])
                .zigzag32(position[2]);
            w.zigzag32(result[0])
                .zigzag32(result[1])
                .zigzag32(result[2]);
            w.zigzag32(*face);
        }
        Out::MobEquipment {
            runtime_id,
            slot,
            window_id,
            item,
        } => {
            w.varint(ids::MOB_EQUIPMENT);
            w.varlong(*runtime_id);
            write_item(&mut w, *item);
            // Slot, selected slot and container id: three bytes, not two. The
            // missing one truncated the packet, which is what made merely
            // selecting an item drop the connection.
            w.u8(*slot).u8(*slot).u8(*window_id);
        }
        Out::ItemUseOnEntity {
            runtime_id,
            action,
            hotbar_slot,
            item,
            position,
            click,
        } => {
            w.varint(ids::INVENTORY_TRANSACTION);
            w.varint(0); // legacy request id
            w.bool(false); // no legacy data
            w.varint(3); // ItemUseOnEntityTransaction
            w.varint(0); // actions count
            w.varlong(*runtime_id);
            // The action type is a compressed int32, so it is zigzag encoded.
            // Everything else we send uses action 0, where the two encodings
            // agree, so this is the first value that can show the difference.
            w.zigzag32(*action);
            w.zigzag32(*hotbar_slot); // slot
            w.u8(0); // hand slot: main hand
            // The held item, as a network item stack descriptor.
            write_item(&mut w, *item);
            w.f32(position[0]);
            w.f32(position[1]);
            w.f32(position[2]); // the player's position
            w.f32(click[0]);
            w.f32(click[1]);
            w.f32(click[2]); // the position clicked on the entity
        }
        Out::ItemUseOnBlock {
            block,
            face,
            hotbar_slot,
            action,
            item,
            cursor,
            position,
            block_id,
        } => {
            w.varint(ids::INVENTORY_TRANSACTION);
            w.varint(0); // legacy request id
            w.bool(false); // no legacy data
            w.varint(2); // ItemUseTransaction
            // A placement says which slot changed: the held stack becomes one
            // smaller. The server checks the transaction against it, and with
            // no action it treats the placement as not matching the inventory.
            let placed = if *action == 0 { *item } else { None };
            match placed.filter(|from| from.count > 0) {
                Some(from) => {
                    w.varint(1); // actions count
                    w.varint(0); // source: container inventory
                    w.bool(true); // a container id follows
                    w.u8(0); // the player's inventory
                    w.bool(true); // source flags follow
                    w.bool(false); // no source flags
                    w.varint(*hotbar_slot as u32);
                    write_item(&mut w, Some(from));
                    let remaining = if from.count > 1 {
                        Some(ItemDescriptor {
                            count: from.count - 1,
                            ..from
                        })
                    } else {
                        None
                    };
                    write_item(&mut w, remaining);
                }
                None => {
                    w.varint(0); // actions count
                }
            }
            // The action type is a compressed int32, so it is zigzag encoded.
            // Placing sends action zero, where a plain varint reads the same,
            // which is why placing worked while using an item in the air did
            // not.
            w.zigzag32(*action);
            w.u8(1); // ItemUseTriggerType: player input
            w.zigzag32(block[0])
                .zigzag32(block[1])
                .zigzag32(block[2]);
            w.u8(*face as u8);
            // The slot is a signed varint here, and the action's slot below is
            // unsigned: writing this one plain made every slot but zero read
            // back as a negative number, which the server refused.
            w.zigzag32(*hotbar_slot);
            w.u8(0); // hand: main
            write_item(&mut w, *item);
            w.f32(position[0]);
            w.f32(position[1]);
            w.f32(position[2]); // the player's position
            w.f32(cursor[0]);
            w.f32(cursor[1]);
            w.f32(cursor[2]); // the position clicked on the block
            w.varint(*block_id); // the clicked block's runtime id
            w.u8(1); // ItemUsePredictedResult: success
            w.u8(0); // cooldown state: off
        }
        Out::ItemStackRequest {
            request_id,
            actions,
        } => {
            w.varint(ids::ITEM_STACK_REQUEST);
            w.varint(1); // one request per packet
            w.zigzag32(*request_id);
            w.varint(actions.len() as u32);
            for action in actions {
                match action {
                    StackAction::Take {
                        count,
                        source,
                        destination,
                    } => {
                        w.u8(0); // the action the list selects
                        w.u8(0); // its own const action type
                        w.u8(*count);
                        write_slot(&mut w, source);
                        write_slot(&mut w, destination);
                    }
                    StackAction::Place {
                        count,
                        source,
                        destination,
                    } => {
                        w.u8(1); // the action the list selects
                        w.u8(1); // its own const action type
                        w.u8(*count);
                        write_slot(&mut w, source);
                        write_slot(&mut w, destination);
                    }
                    StackAction::Swap { first, second } => {
                        w.u8(2); // the action the list selects
                        w.u8(2); // its own const action type
                        write_slot(&mut w, first);
                        write_slot(&mut w, second);
                    }
                    StackAction::Drop {
                        count,
                        source,
                        randomly,
                    } => {
                        w.u8(3); // the action the list selects
                        w.u8(3); // its own const action type
                        w.u8(*count);
                        write_slot(&mut w, source);
                        w.bool(*randomly);
                    }
                    StackAction::Destroy { count, source } => {
                        w.u8(4); // the action the list selects
                        w.u8(4); // its own const action type
                        w.u8(*count);
                        write_slot(&mut w, source);
                    }
                }
            }
            w.varint(0); // no filter strings
            w.i32(0); // filter string cause
        }
        Out::ContainerClose {
            window_id,
            window_type,
        } => {
            w.varint(ids::CONTAINER_CLOSE);
            w.u8(*window_id);
            w.u8(*window_type);
            w.bool(false); // not server initiated
        }
        Out::ItemRelease {
            slot,
            count,
            item,
            position: _,
        } => {
            // A drop is a normal transaction with two actions: the stack
            // appears in the world from a world-interaction source, and leaves
            // the inventory container. The world source carries no flag.
            let held = item.unwrap_or_default();
            let amount = *count as i16;
            let dropped = ItemDescriptor {
                count: amount,
                ..held
            };
            let remaining = if held.count > amount {
                ItemDescriptor {
                    count: held.count - amount,
                    ..held
                }
            } else {
                ItemDescriptor::default()
            };

            w.varint(ids::INVENTORY_TRANSACTION);
            w.varint(0); // legacy request id
            w.bool(false); // no legacy data
            w.varint(0); // NormalTransaction
            w.varint(2); // actions count
            // 1. the stack enters the world.
            w.varint(2); // source: world interaction
            w.bool(false); // no container id
            w.bool(true); // flags present
            w.varint(0); // no flag
            w.varint(0); // slot
            write_item(&mut w, None);
            write_item(&mut w, Some(dropped));
            // 2. the stack leaves the player's inventory.
            w.varint(0); // source: container inventory
            w.bool(true); // container id present
            w.u8(0); // container id: the player's inventory
            w.bool(false); // no flags
            w.varint(*slot as u32);
            write_item(&mut w, Some(held));
            write_item(&mut w, Some(remaining));
        }
        Out::InventoryMismatch => {
            // The server answers an inventory mismatch by resending everything
            // it holds, which is how a client gets back in step.
            w.varint(ids::INVENTORY_TRANSACTION);
            w.varint(0); // legacy request id
            w.bool(false); // no legacy data
            w.varint(1); // InventoryMismatch
            w.varint(0); // actions count
        }
        Out::Text {
            kind,
            source,
            message,
        } => {
            w.varint(ids::TEXT).bool(false);
            match *kind {
                1 => {
                    w.u8(1).u8(*kind).string(source).string(message);
                }
                _ => {
                    w.u8(0).u8(*kind).string(message);
                }
            }
            w.string("").string("");
            w.bool(false);
        }
        Out::CommandRequest {
            command,
            player_id,
            numeric,
            uuid,
        } => {
            w.varint(ids::COMMAND_REQUEST);
            w.string(command);
            let uuid = uuid.unwrap_or_else(command_uuid);
            if *numeric {
                // The schema's form: an origin type byte and the command
                // version as a `CurrentCmdVersion` number.
                w.u8(0); // `CommandOriginType::Player`
                w.bytes(&uuid);
                w.string(""); // request id
                w.i64(*player_id);
                w.bool(false); // not internal
                w.i32(COMMAND_VERSION_LATEST);
            } else {
                // What a live BDS accepts: the origin type by name (`player`)
                // and the command version as the string the client sends
                // (`latest`). Both are echoed back the same way.
                w.string("player");
                w.bytes(&uuid);
                w.string("");
                w.i64(*player_id);
                w.bool(false);
                w.string("latest");
            }
        }
        Out::Animate { runtime_id, action } => {
            // 1.26's Animate: a byte action, the runtime id, a float, then the
            // swing source and hand bytes. A shorter form makes the server reject
            // the batch and drop the connection.
            w.varint(ids::ANIMATE);
            w.u8(*action as u8);
            w.varlong(*runtime_id);
            w.f32(0.0); // data
            w.u8(0); // swing source
            w.u8(0); // hand: main
        }
        Out::Interact { action, target } => {
            w.varint(ids::INTERACT);
            w.u8(*action);
            w.varlong(*target);
            w.bool(false); // no position
        }
        Out::Raw(id, body) => {
            w.varint(*id).bytes(body);
        }
    }
    let out = w.into_inner();
    if let Out::ItemStackRequest { request_id, .. } = packet {
        let hex: String = out.iter().map(|b| format!("{b:02x}")).collect();
        crate::nethernet::trace(format!("STACK REQ id={request_id} {hex}"));
    }
    if let Out::Interact { action, target } = packet {
        crate::nethernet::trace(format!("INTERACT action={action} target={target}"));
    }
    out
}

/// Frame packets into a batch payload (uncompressed).
pub fn encode_batch(packets: &[Out]) -> Vec<u8> {
    let mut w = Writer::new();
    for p in packets {
        let body = encode(p);
        w.varint(body.len() as u32).bytes(&body);
    }
    w.into_inner()
}

/// True for the outbound packet kinds the bridge can build.
pub fn is_known(id: u32) -> bool {
    matches!(
        id,
        ids::NETWORK_SETTINGS
            | ids::PLAY_STATUS
            | ids::DISCONNECT
            | ids::START_GAME
            | ids::SUB_CHUNK
            | ids::FULL_CHUNK_DATA
            | ids::ADD_PLAYER
            | ids::ADD_ENTITY
            | ids::SET_ENTITY_DATA
            | ids::MOVE_PLAYER
            | ids::TEXT
    )
}

#[cfg(test)]
mod inventory_tests {
    use super::*;
    use crate::stream::{Reader, Writer};

    /// A stack request is an id, one request, and the actions it names.
    #[test]
    fn stack_request_writes_take_slots_in_order() {
        let out = encode(&Out::ItemStackRequest {
            request_id: 1,
            actions: vec![StackAction::Take {
                count: 1,
                source: SlotRef::new(29, 9, 0),
                destination: SlotRef::new(59, 0, 0),
            }],
        });
        assert_eq!(
            out,
            vec![
                0x93, 0x01, // ITEM_STACK_REQUEST
                0x01, // one request
                0x02, // request id 1, zigzag
                0x01, // one action
                0x00, 0x00, // Take, then its const action type
                0x01, // one item
                0x1d, 0x00, 0x09, 0x00, 0x00, 0x00, 0x00, // source: inventory 29, slot 9, stack id 0
                0x3b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // destination: cursor 59, slot 0, stack id 0
                0x00, // no filter strings
                0x00, 0x00, 0x00, 0x00, // filter string cause
            ]
        );
    }

    /// A drop names a count, a slot and whether the items scatter.
    #[test]
    fn stack_request_writes_drop_with_random_flag() {
        let out = encode(&Out::ItemStackRequest {
            request_id: 4,
            actions: vec![StackAction::Drop {
                count: 3,
                source: SlotRef::new(28, 2, 0),
                randomly: false,
            }],
        });
        assert_eq!(
            out,
            vec![
                0x93, 0x01, // ITEM_STACK_REQUEST
                0x01, // one request
                0x08, // request id 4, zigzag
                0x01, // one action
                0x03, 0x03, 0x03, // Drop: the action, its const type, three items
                0x1c, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, // source: hotbar 28, slot 2, stack id 0
                0x00, // not scattered
                0x00, 0x00, 0x00, 0x00, 0x00,
            ]
        );
    }

    /// The response carries the slots the server decided on, and nothing when it
    /// refused the request.
    #[test]
    fn stack_response_is_read_with_its_slots() {
        let mut w = Writer::new();
        w.varint(1) // one response in the packet
            .u8(0) // applied
            .zigzag32(5) // request id
            .bool(true) // a container list follows
            .varint(1) // one container
            .u8(29) // inventory
            .bool(false) // no dynamic id
            .varint(1) // one slot
            .u8(9) // slot
            .u8(9) // hotbar slot
            .u8(3) // count
            .bool(true) // a stack id follows
            .zigzag32(7) // the server's stack id
            .varint(0) // custom name
            .varint(0) // filtered name
            .zigzag32(0); // durability correction
        let packet = decode(ids::ITEM_STACK_RESPONSE, &w.into_inner()).unwrap();
        let Packet::ItemStackResponses(responses) = packet else {
            panic!("expected a stack response");
        };
        assert_eq!(responses.len(), 1);
        let response = &responses[0];
        assert_eq!(response.result, 0);
        assert_eq!(response.request_id, 5);
        assert_eq!(response.containers.len(), 1);
        let container = &response.containers[0];
        assert_eq!(container.container.name, 29);
        assert_eq!(container.container.dynamic_id, None);
        let slot = &container.slots[0];
        assert_eq!((slot.slot, slot.count, slot.stack_id), (9, 3, 7));

        let mut w = Writer::new();
        w.varint(1).u8(1).zigzag32(5).bool(false); // refused: no container list follows
        let packet = decode(ids::ITEM_STACK_RESPONSE, &w.into_inner()).unwrap();
        let Packet::ItemStackResponses(responses) = packet else {
            panic!("expected a stack response");
        };
        assert_eq!(responses.len(), 1);
        let response = &responses[0];
        assert_eq!(response.result, 1);
        assert_eq!(response.request_id, 5);
        assert!(response.containers.is_empty());
    }

    /// A packet can carry more than one verdict, and the count is what tells
    /// them apart; reading it as the first response shifted everything.
    #[test]
    fn stack_response_reads_every_response_in_the_batch() {
        let mut w = Writer::new();
        w.varint(2)
            .u8(0)
            .zigzag32(1)
            .bool(false)
            .u8(1)
            .zigzag32(3)
            .bool(false);
        let packet = decode(ids::ITEM_STACK_RESPONSE, &w.into_inner()).unwrap();
        let Packet::ItemStackResponses(responses) = packet else {
            panic!("expected stack responses");
        };
        assert_eq!(responses.len(), 2);
        assert_eq!((responses[0].result, responses[0].request_id), (0, 1));
        assert_eq!((responses[1].result, responses[1].request_id), (1, 3));
    }

    /// The stack id has to survive the descriptor round trip, because a request
    /// that quotes the wrong one is refused.
    #[test]
    fn descriptor_round_trips_the_stack_id() {
        for net_id in [None, Some(0), Some(12345)] {
            let item = ItemDescriptor {
                runtime_id: 42,
                count: 7,
                damage: 3,
                net_id,
                block_runtime_id: 99,
            };
            let mut w = Writer::new();
            write_item(&mut w, Some(item));
            let bytes = w.into_inner();
            let mut r = Reader::new(&bytes);
            let read = NetworkItem::read(&mut r).unwrap();
            assert_eq!(read.runtime_id, 42);
            assert_eq!(read.count, 7);
            assert_eq!(read.damage, 3);
            assert_eq!(read.block_runtime_id, 99);
            assert_eq!(read.net_id, net_id);
        }
    }
}
