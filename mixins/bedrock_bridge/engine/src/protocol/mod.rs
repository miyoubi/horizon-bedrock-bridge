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

/// A decoded server→client packet. Unmodelled packets keep their raw bytes.
#[derive(Clone, Debug)]
pub enum Packet {
    NetworkSettings(NetworkSettings),
    PlayStatus(i32),
    Disconnect(Disconnect),
    ServerToClientHandshake(String),
    ResourcePacksInfo,
    ResourcePackStack,
    StartGame(StartGame),
    ItemComponent,
    AvailableEntityIdentifiers(crate::nbt::Tag),
    BiomeDefinitionList,
    CreativeContent,
    PlayerList,
    LevelChunk(LevelChunk),
    SubChunk {
        origin: [i32; 3],
        dimension: i32,
        data: Vec<u8>,
    },
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
    SetEntityMotion {
        runtime_id: u64,
        velocity: [f32; 3],
    },
    UpdateBlock {
        position: [i32; 3],
        runtime_id: u32,
        flags: u32,
    },
    UpdateSubChunkBlocks {
        sub_chunk: [i32; 3],
        blocks: usize,
    },
    InventoryContent(InventoryContent),
    InventorySlot(InventorySlot),
    ContainerOpen(ContainerOpen),
    ContainerClose {
        window_id: u8,
    },
    Text(Text),
    SetTime(i32),
    SetPlayerGameType(i32),
    UpdatePlayerGameType {
        gamemode: i32,
        runtime_id: u64,
    },
    SetHealth(f32),
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
    AvailableCommands,
    /// The packet id is known but its body is not modelled here.
    Raw(u32, Vec<u8>),
    /// The packet id is unknown to this build.
    Unknown(u32, Vec<u8>),
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
    RequestChunkRadius {
        radius: i32,
    },
    SetLocalPlayerAsInitialized {
        runtime_id: u64,
    },
    TickSync {
        request: i64,
        response: i64,
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
    PlayerAction {
        runtime_id: u64,
        action: i32,
        position: [i32; 3],
        face: i32,
    },
    MobEquipment {
        runtime_id: u64,
        slot: u8,
        window_id: u8,
    },
    Text {
        kind: u8,
        source: String,
        message: String,
    },
    Animate {
        runtime_id: u64,
        action: i32,
    },
    /// A raw packet id + body, for anything not modelled above.
    Raw(u32, Vec<u8>),
}

/// Decode one packet entry (id + body) into a [`Packet`].
pub fn decode(id: u32, body: &[u8]) -> Result<Packet> {
    let mut r = Reader::new(body);
    Ok(match id {
        ids::NETWORK_SETTINGS => Packet::NetworkSettings(NetworkSettings::read(&mut r)?),
        ids::PLAY_STATUS => Packet::PlayStatus(r.i32()?),
        ids::DISCONNECT => Packet::Disconnect(Disconnect::read(&mut r)?),
        ids::SERVER_TO_CLIENT_HANDSHAKE => Packet::ServerToClientHandshake(r.string()?),
        ids::RESOURCE_PACKS_INFO => Packet::ResourcePacksInfo,
        ids::RESOURCE_PACK_STACK => Packet::ResourcePackStack,
        ids::START_GAME => Packet::StartGame(StartGame::read(&mut r)?),
        ids::ITEM_COMPONENT => Packet::ItemComponent,
        ids::AVAILABLE_ENTITY_IDENTIFIERS => {
            Packet::AvailableEntityIdentifiers(crate::nbt::read_named(&mut r)?.1)
        }
        ids::BIOME_DEFINITION_LIST => Packet::BiomeDefinitionList,
        ids::CREATIVE_CONTENT => Packet::CreativeContent,
        ids::PLAYER_LIST => Packet::PlayerList,
        ids::FULL_CHUNK_DATA => Packet::LevelChunk(LevelChunk::read(&mut r)?),
        ids::SUB_CHUNK => {
            let _cache = r.bool()?;
            let dimension = r.zigzag32()?;
            let origin = [r.zigzag32()?, r.zigzag32()?, r.zigzag32()?];
            let _count = r.varint()?;
            let data = r.byte_slice()?.to_vec();
            Packet::SubChunk {
                origin,
                dimension,
                data,
            }
        }
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
            Packet::UpdateSubChunkBlocks {
                sub_chunk,
                blocks: count,
            }
        }
        ids::INVENTORY_CONTENT => Packet::InventoryContent(InventoryContent::read(&mut r)?),
        ids::INVENTORY_SLOT => Packet::InventorySlot(InventorySlot::read(&mut r)?),
        ids::CONTAINER_OPEN => Packet::ContainerOpen(ContainerOpen::read(&mut r)?),
        ids::CONTAINER_CLOSE => Packet::ContainerClose { window_id: r.u8()? },
        ids::TEXT => Packet::Text(Text::read(&mut r)?),
        ids::SET_TIME => Packet::SetTime(r.zigzag32()?),
        ids::SET_PLAYER_GAME_TYPE => Packet::SetPlayerGameType(r.zigzag32()?),
        ids::SET_HEALTH => Packet::SetHealth(r.f32()?),
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
            let action = r.zigzag32()?;
            let runtime_id = r.varlong()?;
            Packet::Animate { action, runtime_id }
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
        ids::AVAILABLE_COMMANDS => Packet::AvailableCommands,
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
        // A packet we cannot decode yet must not abort the whole stream: keep
        // the raw body so the caller can log it and carry on.
        match decode(id, &body) {
            Ok(packet) => out.push(packet),
            Err(_) => out.push(Packet::Raw(id, body)),
        }
    }
    Ok(out)
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
        Out::TickSync { request, response } => {
            w.varint(ids::TICK_SYNC).i64(*request).i64(*response);
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
            face,
        } => {
            w.varint(ids::PLAYER_ACTION);
            w.varlong(*runtime_id).zigzag32(*action);
            w.zigzag32(position[0])
                .zigzag32(position[1])
                .zigzag32(position[2]);
            w.zigzag32(*face);
            // No "result" block position or runtime id (protocol 313+).
        }
        Out::MobEquipment {
            runtime_id,
            slot,
            window_id,
        } => {
            w.varint(ids::MOB_EQUIPMENT);
            w.varlong(*runtime_id);
            // An empty network item stack descriptor.
            w.i16(0).i16(0).varint(0).bool(false).varint(0).u16(0);
            w.u8(*slot).u8(*window_id);
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
        Out::Animate { runtime_id, action } => {
            w.varint(ids::ANIMATE)
                .zigzag32(*action)
                .varlong(*runtime_id);
        }
        Out::Raw(id, body) => {
            w.varint(*id).bytes(body);
        }
    }
    w.into_inner()
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
