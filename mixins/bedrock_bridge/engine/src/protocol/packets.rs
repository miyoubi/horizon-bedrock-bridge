//! The Bedrock packets the bridge consumes and produces, decoded into typed
//! structs. Only the fields the translator needs are modelled; everything else
//! is skipped with the exact wire layout so decoding stays aligned.

use crate::error::Result;
use crate::nbt::{self, Tag};
use crate::stream::{Reader, Writer};

/// One entity-metadata value (see `Entity.DATA_TYPE_*`).
#[derive(Clone, Debug, PartialEq)]
pub enum MetaValue {
    Byte(i8),
    Short(i16),
    Int(i32),
    Float(f32),
    String(String),
    Compound(Tag),
    Pos([i32; 3]),
    Long(i64),
    Vec3([f32; 3]),
}

impl MetaValue {
    #[allow(dead_code)]
    fn type_id(&self) -> u32 {
        match self {
            MetaValue::Byte(_) => 0,
            MetaValue::Short(_) => 1,
            MetaValue::Int(_) => 2,
            MetaValue::Float(_) => 3,
            MetaValue::String(_) => 4,
            MetaValue::Compound(_) => 5,
            MetaValue::Pos(_) => 6,
            MetaValue::Long(_) => 7,
            MetaValue::Vec3(_) => 8,
        }
    }
}

/// The metadata dictionary shared by `AddEntity`/`SetEntityData`.
pub type Metadata = Vec<(u32, MetaValue)>;

/// Read a metadata dictionary.
///
/// Since 1.26.40 the writer emits the value's type id twice, and the reader in
/// the same reference only reads it once. This decodes both: after the key it
/// reads the type, then peeks a second varint — if it repeats the type it is the
/// doubled field, otherwise the reader is rewound and the value follows.
pub fn read_metadata(r: &mut Reader) -> Result<Metadata> {
    let count = r.varint()?;
    let mut out = Vec::with_capacity((count as usize).min(256));
    for _ in 0..count {
        let key = r.varint()?;
        let first = r.varint()?;
        let after_first = r.position();
        let doubled = match r.varint() {
            Ok(second) if second == first => true,
            _ => {
                r.seek(after_first);
                false
            }
        };
        let value = read_meta_value(r, first)?;
        let _ = doubled;
        out.push((key, value));
    }
    Ok(out)
}

fn read_meta_value(r: &mut Reader, type_id: u32) -> Result<MetaValue> {
    Ok(match type_id {
        0 => MetaValue::Byte(r.i8()?),
        1 => MetaValue::Short(r.i16()?),
        2 => MetaValue::Int(r.zigzag32()?),
        3 => MetaValue::Float(r.f32()?),
        4 => MetaValue::String(r.string()?),
        5 => MetaValue::Compound(nbt::read_named(r)?.1),
        6 => MetaValue::Pos([r.zigzag32()?, r.zigzag32()?, r.zigzag32()?]),
        7 => MetaValue::Long(r.zigzag64()?),
        8 => MetaValue::Vec3([r.f32()?, r.f32()?, r.f32()?]),
        other => {
            return Err(crate::Error::decode(format!(
                "unknown entity metadata type {other}"
            )))
        }
    })
}

/// A network item stack descriptor (runtime id, count, damage, block id, NBT).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NetworkItem {
    pub runtime_id: i16,
    pub count: i16,
    pub damage: u32,
    pub block_runtime_id: u32,
    pub nbt: Option<Tag>,
}

impl NetworkItem {
    pub fn read(r: &mut Reader) -> Result<NetworkItem> {
        let runtime_id = r.i16()?;
        let count = r.i16()?;
        let damage = r.varint()?;
        if r.bool()? {
            let _net_id = r.zigzag32()?;
        }
        let block_runtime_id = r.varint()?;
        let mut nbt = None;
        let user = r.byte_slice()?;
        if !user.is_empty() {
            let mut u = Reader::new(user);
            let head = u.i16()?;
            if head >= 0 {
                // No NBT.
            } else {
                // -1 then a single tag.
                let tag_count = u.u8()?;
                if tag_count == 1 {
                    nbt = Some(nbt::read_named(&mut u)?.1);
                }
            }
        }
        Ok(NetworkItem {
            runtime_id,
            count,
            damage,
            block_runtime_id,
            nbt,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Disconnect {
    pub reason: i32,
    pub hide: bool,
    pub message: String,
}

impl Disconnect {
    pub fn read(r: &mut Reader) -> Result<Disconnect> {
        let reason = r.zigzag32()?;
        let hide = r.bool()?;
        if hide {
            Ok(Disconnect {
                reason,
                hide,
                message: String::new(),
            })
        } else {
            Ok(Disconnect {
                reason,
                hide,
                message: r.string()?,
            })
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NetworkSettings {
    pub compression_threshold: u16,
    pub compression_algorithm: u16,
}

impl NetworkSettings {
    pub fn read(r: &mut Reader) -> Result<NetworkSettings> {
        Ok(NetworkSettings {
            compression_threshold: r.u16()?,
            compression_algorithm: r.u16()?,
        })
    }
}

/// `StartGame`, decoded through the block palette (spawn, mode, seed, level id
/// and the `runtime_id → block` palette). The trailing metadata is not needed
/// to render, so decoding stops after the palette.
#[derive(Clone, Debug, PartialEq)]
pub struct StartGame {
    pub entity_unique_id: i64,
    pub entity_runtime_id: u64,
    pub player_gamemode: i32,
    pub position: [f32; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub seed: i64,
    pub dimension: i32,
    pub generator: i32,
    pub world_gamemode: i32,
    pub difficulty: i32,
    pub spawn: [i32; 3],
    pub level_id: String,
    pub world_name: String,
    /// The server's block palette in runtime-id order: `(identifier, state NBT)`.
    /// Chunk runtime ids index this, then map to Java block states.
    pub palette: Vec<(String, crate::nbt::Tag)>,
}

impl StartGame {
    /// Decode through the block palette (the trailing level metadata is not
    /// needed to render the world, so decoding stops after the palette).
    pub fn read(r: &mut Reader) -> Result<StartGame> {
        let entity_unique_id = r.zigzag64()?;
        let entity_runtime_id = r.varlong()?;
        let player_gamemode = r.zigzag32()?;
        let position = [r.f32()?, r.f32()?, r.f32()?];
        let yaw = r.f32()?;
        let pitch = r.f32()?;
        let seed = r.i64()?;
        let _spawn_biome_type = r.i16()?;
        let _user_biome = r.string()?;
        let dimension = r.zigzag32()?;
        let generator = r.zigzag32()?;
        let world_gamemode = r.zigzag32()?;
        let _hardcore = r.bool()?;
        let difficulty = r.zigzag32()?;
        let spawn = [r.zigzag32()?, r.zigzag32()?, r.zigzag32()?];

        // Level settings.
        let _achievements_disabled = r.bool()?;
        let _editor_world_type = r.zigzag32()?;
        let _created_in_editor = r.bool()?;
        let _exported_from_editor = r.bool()?;
        let _editor_migration_version = r.u8()?;
        let _day_cycle_stop = r.zigzag32()?;
        let _edu_offer = r.varint()?;
        let _edu_features = r.bool()?;
        let _edu_product = r.string()?;
        let _rain = r.f32()?;
        let _lightning = r.f32()?;
        let _platform_locked = r.bool()?;
        let _multiplayer = r.bool()?;
        let _broadcast_lan = r.bool()?;
        let _xbl_intent = r.zigzag32()?;
        let _platform_intent = r.zigzag32()?;
        let _commands = r.bool()?;
        let _texture_packs = r.bool()?;

        // Game rules: uvarint count, each name + canBeChanged + type + value.
        let rules = r.varint()?;
        for _ in 0..rules {
            let _name = r.string()?;
            let _can_change = r.bool()?;
            match r.varint()? {
                1 => {
                    let _ = r.bool()?;
                }
                2 => {
                    let _ = r.i32()?;
                }
                3 => {
                    let _ = r.f32()?;
                }
                _ => {}
            }
        }
        // Experiments: LInt count, each name + enabled, then a bool.
        let experiments = r.i32()?;
        for _ in 0..experiments {
            let _name = r.string()?;
            let _enabled = r.bool()?;
        }
        let _any_experiment = r.bool()?;

        let _bonus_chest = r.bool()?;
        let _start_with_map = r.bool()?;
        let _permission = r.u8()?;
        let _chunk_tick_range = r.i32()?;
        let _locked_behavior = r.bool()?;
        let _locked_resource = r.bool()?;
        let _from_locked_template = r.bool()?;
        let _msa_gamertags = r.bool()?;
        let _from_template = r.bool()?;
        let _template_locked = r.bool()?;
        let _only_v1_villagers = r.bool()?;
        let _disabling_personas = r.bool()?;
        let _disabling_custom_skins = r.bool()?;
        let _emote_chat_muted = r.bool()?;
        let _minecraft_version = r.string()?;
        let _limited_width = r.i32()?;
        let _limited_height = r.i32()?;
        let _nether_type = r.bool()?;
        let _edu_uri_button = r.string()?;
        let _edu_uri_link = r.string()?;
        let _experimental_gameplay = r.bool()?;
        let _chat_restriction = r.u8()?;
        let _disable_interactions = r.bool()?;
        let _editor_connection_policy = r.zigzag32()?;
        let _anon_block_drops = r.bool()?;

        let level_id = r.string()?;
        let world_name = r.string()?;
        let _premium_template = r.string()?;
        let _trial = r.bool()?;
        let _rewind_history = r.zigzag32()?;
        let _server_auth_block_breaking = r.bool()?;
        let _current_tick = r.i64()?;
        let _enchantment_seed = r.zigzag32()?;

        // The block palette: uvarint count of (string identifier, network NBT).
        let count = r.varint()?;
        let mut palette = Vec::with_capacity((count as usize).min(4096));
        for _ in 0..count {
            let name = r.string()?;
            let state = crate::nbt::read_named(r)?.1;
            palette.push((name, state));
        }

        Ok(StartGame {
            entity_unique_id,
            entity_runtime_id,
            player_gamemode,
            position,
            yaw,
            pitch,
            seed,
            dimension,
            generator,
            world_gamemode,
            difficulty,
            spawn,
            level_id,
            world_name,
            palette,
        })
    }

    /// The palette entry for a runtime block id (`identifier`, `state`).
    pub fn block(&self, runtime_id: i32) -> Option<&(String, crate::nbt::Tag)> {
        usize::try_from(runtime_id)
            .ok()
            .and_then(|i| self.palette.get(i))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AddPlayer {
    pub uuid: [u8; 16],
    pub username: String,
    pub runtime_id: u64,
    pub position: [f32; 3],
    pub velocity: [f32; 3],
    pub pitch: f32,
    pub yaw: f32,
    pub held_item: NetworkItem,
    pub gamemode: i32,
    pub metadata: Metadata,
}

impl AddPlayer {
    pub fn read(r: &mut Reader) -> Result<AddPlayer> {
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(r.bytes(16)?);
        let username = r.string()?;
        let runtime_id = r.varlong()?;
        let _platform_chat_id = r.string()?;
        let position = [r.f32()?, r.f32()?, r.f32()?];
        let velocity = [r.f32()?, r.f32()?, r.f32()?];
        let pitch = r.f32()?;
        let yaw = r.f32()?;
        let _head_yaw = r.f32()?;
        let held_item = NetworkItem::read(r)?;
        let gamemode = r.zigzag32()?;
        let metadata = read_metadata(r)?;
        Ok(AddPlayer {
            uuid,
            username,
            runtime_id,
            position,
            velocity,
            pitch,
            yaw,
            held_item,
            gamemode,
            metadata,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AddEntity {
    pub unique_id: i64,
    pub runtime_id: u64,
    /// Bedrock entity identifier, e.g. `minecraft:cow` or `mymod:robot`.
    pub identifier: String,
    pub position: [f32; 3],
    pub velocity: [f32; 3],
    pub pitch: f32,
    pub yaw: f32,
    pub head_yaw: f32,
    pub body_yaw: f32,
    pub metadata: Metadata,
}

impl AddEntity {
    pub fn read(r: &mut Reader) -> Result<AddEntity> {
        let unique_id = r.zigzag64()?;
        let runtime_id = r.varlong()?;
        let identifier = r.string()?;
        let position = [r.f32()?, r.f32()?, r.f32()?];
        let velocity = [r.f32()?, r.f32()?, r.f32()?];
        let pitch = r.f32()?;
        let yaw = r.f32()?;
        let head_yaw = r.f32()?;
        let body_yaw = r.f32()?;
        // attribute list
        let attrs = r.varint()?;
        for _ in 0..attrs {
            let _name = r.string()?;
            let _min = r.f32()?;
            let _value = r.f32()?;
            let _max = r.f32()?;
        }
        let metadata = read_metadata(r)?;
        Ok(AddEntity {
            unique_id,
            runtime_id,
            identifier,
            position,
            velocity,
            pitch,
            yaw,
            head_yaw,
            body_yaw,
            metadata,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SetEntityData {
    pub runtime_id: u64,
    pub metadata: Metadata,
}

impl SetEntityData {
    pub fn read(r: &mut Reader) -> Result<SetEntityData> {
        let runtime_id = r.varlong()?;
        let metadata = read_metadata(r)?;
        Ok(SetEntityData {
            runtime_id,
            metadata,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MovePlayer {
    pub runtime_id: u64,
    pub position: [f32; 3],
    pub pitch: f32,
    pub yaw: f32,
    pub head_yaw: f32,
    pub mode: u8,
    pub on_ground: bool,
}

impl MovePlayer {
    pub fn read(r: &mut Reader) -> Result<MovePlayer> {
        let runtime_id = r.varlong()?;
        let position = [r.f32()?, r.f32()?, r.f32()?];
        let pitch = r.f32()?;
        let yaw = r.f32()?;
        let head_yaw = r.f32()?;
        let mode = r.u8()?;
        let on_ground = r.bool()?;
        Ok(MovePlayer {
            runtime_id,
            position,
            pitch,
            yaw,
            head_yaw,
            mode,
            on_ground,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
/// One actor's position/rotation deltas (packet id 111). Each component is
/// optional and preceded by a presence bool.
pub struct MoveActorDelta {
    pub runtime_id: u64,
    pub position: [Option<f32>; 3],
    pub rotation: [Option<u8>; 3],
    pub on_ground: bool,
    pub force_move: bool,
    pub force_move_local_entity: bool,
    pub force_completion: bool,
    pub ticks: u64,
}

impl MoveActorDelta {
    pub fn read(r: &mut Reader) -> Result<MoveActorDelta> {
        fn opt_f32(r: &mut Reader) -> Result<Option<f32>> {
            if r.bool()? {
                Ok(Some(r.f32()?))
            } else {
                Ok(None)
            }
        }
        fn opt_u8(r: &mut Reader) -> Result<Option<u8>> {
            if r.bool()? {
                Ok(Some(r.u8()?))
            } else {
                Ok(None)
            }
        }
        Ok(MoveActorDelta {
            runtime_id: r.varlong()?,
            position: [opt_f32(r)?, opt_f32(r)?, opt_f32(r)?],
            rotation: [opt_u8(r)?, opt_u8(r)?, opt_u8(r)?],
            on_ground: r.bool()?,
            force_move: r.bool()?,
            force_move_local_entity: r.bool()?,
            force_completion: r.bool()?,
            ticks: r.varlong()?,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AbilityLayer {
    pub kind: u16,
    pub allowed: u32,
    pub enabled: u32,
    pub fly_speed: f32,
    pub vertical_fly_speed: f32,
    pub walk_speed: f32,
}

/// Player abilities: id 187.
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateAbilities {
    pub unique_id: i64,
    pub permission_level: u8,
    pub command_permission: u8,
    pub layers: Vec<AbilityLayer>,
}

impl UpdateAbilities {
    pub fn read(r: &mut Reader) -> Result<UpdateAbilities> {
        let unique_id = r.i64()?;
        let permission_level = r.u8()?;
        let command_permission = r.u8()?;
        let count = r.u8()? as usize;
        let mut layers = Vec::with_capacity(count.min(8));
        for _ in 0..count {
            layers.push(AbilityLayer {
                kind: r.u16()?,
                allowed: r.i32()? as u32,
                enabled: r.i32()? as u32,
                fly_speed: r.f32()?,
                vertical_fly_speed: r.f32()?,
                walk_speed: r.f32()?,
            });
        }
        Ok(UpdateAbilities {
            unique_id,
            permission_level,
            command_permission,
            layers,
        })
    }
}

/// A world/level event (sounds, particles, …): id 25.
#[derive(Clone, Debug, PartialEq)]
pub struct LevelEvent {
    pub event: i32,
    pub position: [f32; 3],
    pub data: i32,
}

impl LevelEvent {
    pub fn read(r: &mut Reader) -> Result<LevelEvent> {
        Ok(LevelEvent {
            event: r.zigzag32()?,
            position: [r.f32()?, r.f32()?, r.f32()?],
            data: r.zigzag32()?,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AttributeModifier {
    pub id: String,
    pub name: String,
    pub amount: f32,
    pub operation: i32,
    pub operand: i32,
    pub serializable: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Attribute {
    pub name: String,
    pub min: f32,
    pub max: f32,
    pub current: f32,
    pub default_min: f32,
    pub default_max: f32,
    pub default: f32,
    pub modifiers: Vec<AttributeModifier>,
}

/// Entity attribute values: id 29.
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateAttributes {
    pub runtime_id: u64,
    pub attributes: Vec<Attribute>,
    pub tick: u64,
}

impl UpdateAttributes {
    pub fn read(r: &mut Reader) -> Result<UpdateAttributes> {
        let runtime_id = r.varlong()?;
        let count = r.varint()? as usize;
        let mut attributes = Vec::with_capacity(count.min(64));
        for _ in 0..count {
            let min = r.f32()?;
            let max = r.f32()?;
            let current = r.f32()?;
            let default_min = r.f32()?;
            let default_max = r.f32()?;
            let default = r.f32()?;
            let name = r.string()?;
            let mcount = r.varint()? as usize;
            let mut modifiers = Vec::with_capacity(mcount.min(64));
            for _ in 0..mcount {
                modifiers.push(AttributeModifier {
                    id: r.string()?,
                    name: r.string()?,
                    amount: r.f32()?,
                    operation: r.i32()?,
                    operand: r.i32()?,
                    serializable: r.bool()?,
                });
            }
            attributes.push(Attribute {
                name,
                min,
                max,
                current,
                default_min,
                default_max,
                default,
                modifiers,
            });
        }
        Ok(UpdateAttributes {
            runtime_id,
            attributes,
            tick: r.varlong()?,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MoveEntityAbsolute {
    pub runtime_id: u64,
    pub on_ground: bool,
    pub teleport: bool,
    pub position: [f32; 3],
    pub pitch: f32,
    pub head_yaw: f32,
    pub yaw: f32,
}

impl MoveEntityAbsolute {
    pub fn read(r: &mut Reader) -> Result<MoveEntityAbsolute> {
        let runtime_id = r.varlong()?;
        let flags = r.u8()?;
        let position = [r.f32()?, r.f32()?, r.f32()?];
        // Rotation bytes are 360/256 degrees.
        let to_deg = |b: u8| f32::from(b) * (360.0 / 256.0);
        let pitch = to_deg(r.u8()?);
        let head_yaw = to_deg(r.u8()?);
        let yaw = to_deg(r.u8()?);
        Ok(MoveEntityAbsolute {
            runtime_id,
            on_ground: flags & 1 != 0,
            teleport: flags & 2 != 0,
            position,
            pitch,
            head_yaw,
            yaw,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct InventoryContent {
    pub window_id: u32,
    pub slots: Vec<NetworkItem>,
}

impl InventoryContent {
    pub fn read(r: &mut Reader) -> Result<InventoryContent> {
        let window_id = r.varint()?;
        let count = r.varint()?;
        let mut slots = Vec::with_capacity((count as usize).min(256));
        for _ in 0..count {
            slots.push(NetworkItem::read(r)?);
        }
        Ok(InventoryContent { window_id, slots })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct InventorySlot {
    pub window_id: u32,
    pub slot: u32,
    pub item: NetworkItem,
}

impl InventorySlot {
    pub fn read(r: &mut Reader) -> Result<InventorySlot> {
        let window_id = r.varint()?;
        let slot = r.varint()?;
        // optional full container name present
        if r.bool()? {
            let _container_id = r.u8()?;
            if r.bool()? {
                let _dynamic_id = r.i32()?;
            }
        }
        // optional storage item present
        if r.bool()? {
            let _storage = NetworkItem::read(r)?;
        }
        let item = NetworkItem::read(r)?;
        Ok(InventorySlot {
            window_id,
            slot,
            item,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContainerOpen {
    pub window_id: u8,
    pub window_type: u8,
    pub position: [i32; 3],
    pub entity_unique_id: i64,
}

impl ContainerOpen {
    pub fn read(r: &mut Reader) -> Result<ContainerOpen> {
        let window_id = r.u8()?;
        let window_type = r.u8()?;
        let position = [r.zigzag32()?, r.zigzag32()?, r.zigzag32()?];
        let entity_unique_id = r.zigzag64()?;
        Ok(ContainerOpen {
            window_id,
            window_type,
            position,
            entity_unique_id,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Text {
    pub kind: u8,
    pub source: String,
    pub message: String,
    pub parameters: Vec<String>,
    pub is_localized: bool,
}

impl Text {
    pub fn read(r: &mut Reader) -> Result<Text> {
        let is_localized = r.bool()?;
        let format = r.u8()?;
        let (kind, source, message, parameters) = match format {
            0 => {
                let kind = r.u8()?;
                (kind, String::new(), r.string()?, Vec::new())
            }
            1 => {
                let kind = r.u8()?;
                (kind, r.string()?, r.string()?, Vec::new())
            }
            2 => {
                let kind = r.u8()?;
                let message = r.string()?;
                let count = r.varint()?;
                let mut parameters = Vec::with_capacity((count as usize).min(64));
                for _ in 0..count {
                    parameters.push(r.string()?);
                }
                (kind, String::new(), message, parameters)
            }
            other => return Err(crate::Error::decode(format!("unknown Text format {other}"))),
        };
        Ok(Text {
            kind,
            source,
            message,
            parameters,
            is_localized,
        })
    }
}

/// `LevelChunk` (the `FULL_CHUNK_DATA` packet): a whole chunk's sub-chunk blob.
#[derive(Clone, Debug, PartialEq)]
pub struct LevelChunk {
    pub chunk_x: i32,
    pub chunk_z: i32,
    pub dimension: i32,
    pub sub_chunk_count: u32,
    pub data: Vec<u8>,
}

impl LevelChunk {
    pub fn read(r: &mut Reader) -> Result<LevelChunk> {
        let chunk_x = r.zigzag32()?;
        let chunk_z = r.zigzag32()?;
        let dimension = r.zigzag32()?;
        let sub_chunk_count = r.varint()?;
        // Client request sub-chunk limit (int32, always present).
        let _limit = r.i32()?;
        let cache_enabled = r.bool()?;
        let blobs = r.varint()?;
        for _ in 0..blobs {
            let _ = r.i64()?;
        }
        let data = r.byte_slice()?.to_vec();
        let _client_biome_update = r.bool()?;
        let _ = cache_enabled;
        Ok(LevelChunk {
            chunk_x,
            chunk_z,
            dimension,
            sub_chunk_count,
            data,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MobEquipment {
    pub runtime_id: u64,
    pub new_item: NetworkItem,
    pub slot: u8,
    pub window_id: u8,
}

impl MobEquipment {
    pub fn read(r: &mut Reader) -> Result<MobEquipment> {
        let runtime_id = r.varlong()?;
        let new_item = NetworkItem::read(r)?;
        let slot = r.u8()?;
        let window_id = r.u8()?;
        Ok(MobEquipment {
            runtime_id,
            new_item,
            slot,
            window_id,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BlockEntityData {
    pub position: [i32; 3],
    pub nbt: Tag,
}

impl BlockEntityData {
    pub fn read(r: &mut Reader) -> Result<BlockEntityData> {
        let position = [r.zigzag32()?, r.zigzag32()?, r.zigzag32()?];
        let nbt = nbt::read_named(r)?.1;
        Ok(BlockEntityData { position, nbt })
    }
}

/// A `PlayerAuthInput` position/rotation sample we send back to the server.
#[allow(clippy::too_many_arguments)]
pub fn write_move_player(
    w: &mut Writer,
    runtime_id: u64,
    position: [f32; 3],
    pitch: f32,
    yaw: f32,
    head_yaw: f32,
    mode: u8,
    on_ground: bool,
) {
    w.varlong(runtime_id);
    for v in position {
        w.f32(v);
    }
    w.f32(pitch).f32(yaw).f32(head_yaw);
    w.u8(mode);
    w.bool(on_ground);
    w.varlong(0); // riding runtime id
    w.bool(false); // no teleport cause (mode != teleport)
    w.varlong(0); // tick
}
