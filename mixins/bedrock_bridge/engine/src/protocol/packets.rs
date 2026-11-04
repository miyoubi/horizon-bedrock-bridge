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
            )));
        }
    })
}

/// A network item stack descriptor (runtime id, count, damage, block id, NBT).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NetworkItem {
    pub runtime_id: i16,
    pub count: i16,
    pub damage: u32,
    /// The server's id for this stack, which a stack request must quote back so
    /// the server knows which stack it is acting on.
    pub net_id: Option<u32>,
    pub block_runtime_id: u32,
    pub nbt: Option<Tag>,
}

impl NetworkItem {
    pub fn read(r: &mut Reader) -> Result<NetworkItem> {
        let runtime_id = r.i16()?;
        let count = r.i16()?;
        let damage = r.varint()?;
        let net_id = if r.bool()? { Some(r.zigzag32()? as u32) } else { None };
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
            net_id,
            block_runtime_id,
            nbt,
        })
    }
}

/// Which container a stack request slot refers to: a name plus, for containers
/// that exist per item (a bundle), the dynamic id that tells them apart.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FullContainerName {
    pub name: u8,
    pub dynamic_id: Option<i32>,
}

impl FullContainerName {
    pub fn read(r: &mut Reader) -> Result<FullContainerName> {
        let name = r.u8()?;
        let dynamic_id = if r.bool()? { Some(r.i32()?) } else { None };
        Ok(FullContainerName { name, dynamic_id })
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
    /// The server's current tick at spawn. `PlayerAuthInput`'s tick must start
    /// here, not at zero — the server pairs inputs and corrections by it.
    pub current_tick: i64,
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
        // Rotation is (pitch, yaw), like every other rotation packet here.
        // Read as (yaw, pitch) the two swap and the client spawns pitched past
        // vertical, its camera pointed at the sky.
        let pitch = r.f32()?;
        let yaw = r.f32()?;
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
        let current_tick = r.i64()?;
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
            current_tick,
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

/// The `ItemRegistry` (162) payload: `(identifier, network id)` in arrival
/// order. The inventory and equipment packets index this by network id.
///
/// Each entry is the identifier, a little-endian `i16` network id, the
/// component-based flag, a version (0 legacy, 1 data-driven, 2 none) and the
/// item's component NBT. The version and the NBT are always on the wire,
/// whatever the flag says — reading them only for component-based entries runs
/// the next name off the end of the stream and the whole packet fails.
///
/// Note that `StartGame` does **not** carry this palette in 1.26: it ends at the
/// block palette (`block_properties`), and the items arrive in this packet.
pub fn read_item_palette(r: &mut Reader) -> Result<Vec<(String, i16)>> {
    let count = r.varint()?;
    let mut items = Vec::with_capacity((count as usize).min(4096));
    for _ in 0..count {
        let name = r.string()?;
        let network_id = r.i16()?;
        let _component_based = r.bool()?;
        let _version = r.zigzag32()?;
        let _components = crate::nbt::read_named(r)?.1;
        items.push((name, network_id));
    }
    Ok(items)
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
/// One world clock's state, as `SyncWorldClocks` (id 344) carries it.
pub struct WorldClock {
    pub id: u64,
    /// Only the registry variant names a clock.
    pub name: Option<String>,
    pub time: i32,
    pub paused: bool,
}

impl WorldClock {
    /// The packet is a variant: its state, its registry, or a marker change.
    /// Only the first two carry a time, and the markers are read past.
    pub fn read_all(r: &mut Reader) -> Result<Vec<WorldClock>> {
        let mut clocks = Vec::new();
        match r.varint()? {
            0 => {
                for _ in 0..r.varint()? {
                    clocks.push(WorldClock {
                        id: r.varlong()?,
                        name: None,
                        time: r.zigzag32()?,
                        paused: r.bool()?,
                    });
                }
            }
            1 => {
                for _ in 0..r.varint()? {
                    let id = r.varlong()?;
                    let name = Some(r.string()?);
                    let time = r.zigzag32()?;
                    let paused = r.bool()?;
                    for _ in 0..r.varint()? {
                        let _marker = r.varlong()?;
                        let _name = r.string()?;
                        let _time = r.zigzag32()?;
                        if r.bool()? {
                            let _period = r.zigzag32()?;
                        }
                    }
                    clocks.push(WorldClock {
                        id,
                        name,
                        time,
                        paused,
                    });
                }
            }
            2 => {
                let _clock = r.varlong()?;
                for _ in 0..r.varint()? {
                    let _marker = r.varlong()?;
                    let _name = r.string()?;
                    let _time = r.zigzag32()?;
                    if r.bool()? {
                        let _period = r.zigzag32()?;
                    }
                }
            }
            3 => {
                let _clock = r.varlong()?;
                for _ in 0..r.varint()? {
                    let _marker = r.varlong()?;
                }
            }
            _ => {}
        }
        Ok(clocks)
    }
}

#[derive(Clone, Debug, PartialEq)]
/// One actor's position/rotation deltas (packet id 111). Each component is
/// optional and preceded by a presence bool.
pub struct MoveActorDelta {
    pub runtime_id: u64,
    pub position: [Option<f32>; 3],
    /// Rotation bytes, signed: a byte is 360/256 degrees, and an upward look
    /// is a negative angle, so reading them as unsigned turns it into a large
    /// positive one.
    pub rotation: [Option<i8>; 3],
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
        fn opt_i8(r: &mut Reader) -> Result<Option<i8>> {
            if r.bool()? {
                Ok(Some(r.i8()?))
            } else {
                Ok(None)
            }
        }
        Ok(MoveActorDelta {
            runtime_id: r.varlong()?,
            position: [opt_f32(r)?, opt_f32(r)?, opt_f32(r)?],
            rotation: [opt_i8(r)?, opt_i8(r)?, opt_i8(r)?],
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

/// One entry of a `SubChunk` (174) reply.
#[derive(Clone, Debug, PartialEq)]
pub struct SubChunkEntry {
    pub offset: [i8; 3],
    pub result: u8,
    /// The serialized sub-chunk, when `result` is success.
    pub payload: Vec<u8>,
    pub blob_hash: Option<u64>,
}

/// A batch of sub-chunks relative to a center sub-chunk position: id 174.
#[derive(Clone, Debug, PartialEq)]
pub struct SubChunk {
    pub cache_enabled: bool,
    pub dimension: i32,
    pub position: [i32; 3],
    pub entries: Vec<SubChunkEntry>,
}

impl SubChunk {
    pub fn read(r: &mut Reader) -> Result<SubChunk> {
        fn skip_heightmap(r: &mut Reader) -> Result<()> {
            if r.bool()? {
                for _ in 0..16 {
                    let n = r.varint()? as usize;
                    let _ = r.bytes(n)?;
                }
            }
            Ok(())
        }
        let cache_enabled = r.bool()?;
        let dimension = r.zigzag32()?;
        let position = [r.i32()?, r.i32()?, r.i32()?];
        let count = r.varint()? as usize;
        let mut entries = Vec::with_capacity(count.min(8192));
        for _ in 0..count {
            let offset = [r.u8()? as i8, r.u8()? as i8, r.u8()? as i8];
            let result = r.u8()?;
            let payload = if r.bool()? {
                let n = r.varint()? as usize;
                r.bytes(n)?.to_vec()
            } else {
                Vec::new()
            };
            let _height_type = r.u8()?;
            skip_heightmap(r)?;
            let _render_type = r.u8()?;
            skip_heightmap(r)?;
            let blob_hash = if r.bool()? { Some(r.u64()?) } else { None };
            entries.push(SubChunkEntry {
                offset,
                result,
                payload,
                blob_hash,
            });
        }
        Ok(SubChunk {
            cache_enabled,
            dimension,
            position,
            entries,
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
        // Rotation bytes are signed 360/256 degrees, in (pitch, yaw, head).
        let to_deg = |b: i8| f32::from(b) * (360.0 / 256.0);
        let pitch = to_deg(r.i8()?);
        let yaw = to_deg(r.i8()?);
        let head_yaw = to_deg(r.i8()?);
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
    /// Which container the slots belong to. Only meaningful for containers that
    /// are not identified by the window id alone, such as a bundle.
    pub container: Option<FullContainerName>,
    /// The item that holds this container, for a bundle.
    pub storage: Option<NetworkItem>,
}

impl InventoryContent {
    pub fn read(r: &mut Reader) -> Result<InventoryContent> {
        let window_id = r.varint()?;
        let count = r.varint()?;
        let mut slots = Vec::with_capacity((count as usize).min(256));
        for _ in 0..count {
            slots.push(NetworkItem::read(r)?);
        }
        if r.is_empty() {
            return Ok(InventoryContent {
                window_id,
                slots,
                container: None,
                storage: None,
            });
        }
        let container = FullContainerName::read(r)?;
        let storage = NetworkItem::read(r)?;
        Ok(InventoryContent {
            window_id,
            slots,
            container: Some(container),
            storage: Some(storage),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct InventorySlot {
    pub window_id: u32,
    pub slot: u32,
    pub container: Option<FullContainerName>,
    pub item: NetworkItem,
}

impl InventorySlot {
    pub fn read(r: &mut Reader) -> Result<InventorySlot> {
        let window_id = r.varint()?;
        let slot = r.varint()?;
        let container = if r.bool()? {
            Some(FullContainerName::read(r)?)
        } else {
            None
        };
        // optional storage item present
        if r.bool()? {
            let _storage = NetworkItem::read(r)?;
        }
        let item = NetworkItem::read(r)?;
        Ok(InventorySlot {
            window_id,
            slot,
            container,
            item,
        })
    }
}

/// The server's answer to one stack request: whether it was applied, and for
/// each container the slots it decided on.
#[derive(Clone, Debug, PartialEq)]
pub struct ItemStackResponse {
    pub result: u8,
    pub request_id: i32,
    pub containers: Vec<ResponseContainer>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResponseContainer {
    pub container: FullContainerName,
    pub slots: Vec<ResponseSlot>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResponseSlot {
    pub slot: u8,
    pub hotbar_slot: u8,
    pub count: u8,
    pub stack_id: i32,
    pub durability_correction: i32,
}

impl ItemStackResponse {
    pub fn read(r: &mut Reader) -> Result<ItemStackResponse> {
        let result = r.u8()?;
        let request_id = r.zigzag32()?;
        let mut containers = Vec::new();
        // The container list is an optional that a refused request leaves out.
        if r.bool()? {
            let count = r.varint()?;
            for _ in 0..count {
                let container = FullContainerName::read(r)?;
                let slot_count = r.varint()?;
                let mut slots = Vec::with_capacity((slot_count as usize).min(256));
                for _ in 0..slot_count {
                    let slot = r.u8()?;
                    let hotbar_slot = r.u8()?;
                    let count = r.u8()?;
                    // The stack id is optional, and only the server's own id
                    // for the stack is carried.
                    let stack_id = if r.bool()? { r.zigzag32()? } else { 0 };
                    let _custom_name = read_string(r)?;
                    let _filtered = read_string(r)?;
                    let durability_correction = r.zigzag32()?;
                    slots.push(ResponseSlot {
                        slot,
                        hotbar_slot,
                        count,
                        stack_id,
                        durability_correction,
                    });
                }
                containers.push(ResponseContainer { container, slots });
            }
        }
        Ok(ItemStackResponse {
            result,
            request_id,
            containers,
        })
    }
}

/// The body of an `ItemStackResponse` packet: a varint count followed by that
/// many responses. The count is part of the packet, so skipping it shifts every
/// response and every verdict reads as a refusal.
pub fn read_stack_responses(r: &mut Reader) -> Result<Vec<ItemStackResponse>> {
    let count = r.varint()?;
    let mut responses = Vec::with_capacity((count as usize).min(64));
    for _ in 0..count {
        responses.push(ItemStackResponse::read(r)?);
    }
    Ok(responses)
}

/// A length-prefixed string, as the protocol writes names and tags.
fn read_string(r: &mut Reader) -> Result<String> {
    let len = r.varint()?;
    let bytes = r.bytes(len as usize)?;
    Ok(String::from_utf8_lossy(bytes).into_owned())
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
    /// The last sub-chunk the client is asked to request, as a count of
    /// sections from the bottom of the dimension: the packet carries the
    /// highest populated section (one based), or a negative value when there
    /// is no limit at all. Only present in request mode.
    pub highest_subchunk: Option<i32>,
    pub data: Vec<u8>,
}

impl LevelChunk {
    pub fn read(r: &mut Reader) -> Result<LevelChunk> {
        // `ChunkPos` is a pair of signed, compressed int32s: zigzag varints.
        // Read as plain varints, a negative chunk maps to a positive odd
        // number and every positive one is doubled, so the whole column set
        // sits in the wrong place.
        let chunk_x = r.zigzag32()?;
        let chunk_z = r.zigzag32()?;
        let dimension = r.varint()? as i32;
        let sub_chunk_count = r.varint()?;
        // Whether the server named the highest sub-chunk to ask for. It is a
        // signed field: a negative value means "request the whole column".
        let highest_subchunk = if r.bool()? {
            Some(r.zigzag32()?)
        } else {
            None
        };
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
            highest_subchunk,
            data,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MobEquipment {
    pub runtime_id: u64,
    pub new_item: NetworkItem,
    pub slot: u8,
    pub selected_slot: u8,
    pub window_id: u8,
}

impl MobEquipment {
    pub fn read(r: &mut Reader) -> Result<MobEquipment> {
        let runtime_id = r.varlong()?;
        let new_item = NetworkItem::read(r)?;
        let slot = r.u8()?;
        let selected_slot = r.u8()?;
        let window_id = r.u8()?;
        Ok(MobEquipment {
            runtime_id,
            new_item,
            slot,
            selected_slot,
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

/// One block action carried in a `PlayerAuthInput` (the `PERFORM_BLOCK_ACTIONS`
/// flag). `action` is a Bedrock `PlayerAction`: `START_BREAK` 0, `ABORT_BREAK` 1,
/// `STOP_BREAK` 2, `CRACK_BREAK` 18, `PREDICT_DESTROY_BLOCK` 26,
/// `CONTINUE_DESTROY_BLOCK` 27. Every action carries the block position and face
/// except `STOP_BREAK`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlockAction {
    pub action: i32,
    pub position: [i32; 3],
    pub face: i32,
}

impl BlockAction {
    /// Whether the action is followed by the block position and face
    /// (`PlayerBlockActionWithBlockInfo::isValidActionType`).
    fn has_block_info(self) -> bool {
        matches!(self.action, 0 | 1 | 18 | 26 | 27)
    }
}

/// The block actions of a `PlayerAuthInput`, written after the block-actions
/// presence bool. The count is an unsigned varint; the action, position and face
/// are signed.
pub fn write_block_actions(w: &mut Writer, actions: &[BlockAction]) {
    w.varint(actions.len() as u32);
    for action in actions {
        w.zigzag32(action.action);
        if action.has_block_info() {
            w.zigzag32(action.position[0])
                .zigzag32(action.position[1])
                .zigzag32(action.position[2])
                .zigzag32(action.face);
        }
    }
}

/// The `PlayerAuthInput` input flags, as a real 1.26 client sends them: a varint
/// count of the set flags, then each flag index as a signed varint, ascending.
/// (The server-side schema is `array<InputData>`, and `InputData` is zigzag32.)
pub fn write_input_flags(w: &mut Writer, flags: u64) {
    w.varint(flags.count_ones());
    for bit in 0..64u32 {
        if flags & (1u64 << bit) != 0 {
            w.zigzag32(bit as i32);
        }
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

/// One entry of a `PlayerList` (63) update.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerListEntry {
    pub uuid: [u8; 16],
    /// Zero for a removal, which carries only the UUID.
    pub unique_id: i64,
    pub username: String,
    pub xuid: String,
    pub skin: Option<Skin>,
}

/// A `PlayerList` entry's skin: the pixels and the geometry, which is what the
/// Java client needs before it can draw the player.
#[derive(Clone, Debug, PartialEq)]
pub struct Skin {
    pub id: String,
    pub resource_patch: String,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub cape_width: u32,
    pub cape_height: u32,
    pub cape: Vec<u8>,
    pub geometry: String,
    pub animation: String,
}

impl Skin {
    fn read(r: &mut Reader) -> Result<Skin> {
        let id = r.string()?;
        let _playfab_id = r.string()?;
        let resource_patch = r.string()?;
        let (width, height, data) = image(r)?;
        // Animations: an image, then its type, frame count and expression.
        let animations = r.i32()?;
        for _ in 0..animations.max(0) {
            image(r)?;
            r.i32()?;
            r.f32()?;
            r.i32()?;
        }
        let (cape_width, cape_height, cape) = image(r)?;
        let geometry = r.string()?;
        let _geometry_version = r.string()?;
        let animation = r.string()?;
        let _cape_id = r.string()?;
        let _full_skin_id = r.string()?;
        let _arm_size = r.string()?;
        let _skin_colour = r.string()?;
        // Persona pieces: five strings and a flag each.
        let pieces = r.i32()?;
        for _ in 0..pieces.max(0) {
            for _ in 0..5 {
                r.string()?;
            }
            r.bool()?;
        }
        // Persona tint colours: a piece name and its colours.
        let tints = r.i32()?;
        for _ in 0..tints.max(0) {
            r.string()?;
            let colours = r.i32()?;
            for _ in 0..colours.max(0) {
                r.string()?;
            }
        }
        // premium, persona, cape-on-classic, primary user, override.
        for _ in 0..5 {
            r.bool()?;
        }
        Ok(Skin {
            id,
            resource_patch,
            width,
            height,
            data,
            cape_width,
            cape_height,
            cape,
            geometry,
            animation,
        })
    }
}

/// A skin or cape image: its size, then length-prefixed RGBA bytes.
fn image(r: &mut Reader) -> Result<(u32, u32, Vec<u8>)> {
    let width = r.i32()? as u32;
    let height = r.i32()? as u32;
    let len = r.varint()? as usize;
    Ok((width, height, r.bytes(len)?.to_vec()))
}

/// A `PlayerList` (63) update: the server's players, their names and skins.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerList {
    pub entries: Vec<PlayerListEntry>,
}

impl PlayerList {
    pub fn read(r: &mut Reader) -> Result<PlayerList> {
        let kind = r.u8()?;
        let count = r.varint()? as usize;
        let mut entries = Vec::with_capacity(count.min(256));
        for _ in 0..count {
            let mut uuid = [0u8; 16];
            uuid.copy_from_slice(r.bytes(16)?);
            if kind != 0 {
                entries.push(PlayerListEntry {
                    uuid,
                    unique_id: 0,
                    username: String::new(),
                    xuid: String::new(),
                    skin: None,
                });
                continue;
            }
            let unique_id = r.zigzag64()?;
            let username = r.string()?;
            let xuid = r.string()?;
            let _platform_chat_id = r.string()?;
            let _build_platform = r.i32()?;
            let skin = Skin::read(r)?;
            // is teacher, is host, is sub-client.
            for _ in 0..3 {
                r.bool()?;
            }
            let _colour = r.i32()?;
            entries.push(PlayerListEntry {
                uuid,
                unique_id,
                username,
                xuid,
                skin: Some(skin),
            });
        }
        // One verified flag per entry, after them all.
        if kind == 0 {
            for _ in 0..count {
                r.bool()?;
            }
        }
        Ok(PlayerList { entries })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nbt::{self, Tag};
    use crate::stream::{Reader, Writer};

    /// One 1.26.60 item-palette entry: identifier, `i16` id, component-based,
    /// version, then the component NBT — the last two always present.
    fn entry(w: &mut Writer, name: &str, id: i16, component_based: bool, version: i32) {
        w.string(name)
            .i16(id)
            .bool(component_based)
            .zigzag32(version);
        nbt::write_named(
            w,
            "",
            &Tag::Compound(vec![("minecraft:max_stack_size".into(), Tag::Int(64))]),
        );
    }

    #[test]
    fn item_palette_reads_version_and_nbt_for_every_entry() {
        let mut w = Writer::new();
        w.varint(2);
        entry(&mut w, "minecraft:air", 0, false, 0);
        entry(&mut w, "minecraft:diamond_sword", 257, true, 1);
        let bytes = w.into_inner();

        let mut r = Reader::new(&bytes);
        let items = read_item_palette(&mut r).expect("palette decodes");
        assert_eq!(
            items,
            vec![
                ("minecraft:air".to_owned(), 0),
                ("minecraft:diamond_sword".to_owned(), 257),
            ]
        );
        // The bug this guards: skipping the version/NBT on a legacy
        // (non-component-based) entry leaves the reader mid-stream.
        assert!(r.is_empty(), "the palette must consume every entry");
    }

    #[test]
    fn input_flags_are_a_count_then_signed_indices() {
        let mut w = Writer::new();
        write_input_flags(&mut w, 0);
        assert_eq!(w.into_inner(), vec![0x00]);

        let mut w = Writer::new();
        write_input_flags(&mut w, 1 << 6);
        assert_eq!(w.into_inner(), vec![0x01, 0x0c]);

        let mut w = Writer::new();
        write_input_flags(&mut w, (1 << 6) | (1 << 25));
        assert_eq!(w.into_inner(), vec![0x02, 0x0c, 0x32]);
    }

    #[test]
    fn block_actions_carry_position_except_stop_break() {
        let actions = [
            BlockAction {
                action: 0,
                position: [1, 64, -2],
                face: 1,
            },
            // STOP_BREAK has no position or face on the wire.
            BlockAction {
                action: 2,
                position: [9, 9, 9],
                face: 5,
            },
        ];
        let mut w = Writer::new();
        write_block_actions(&mut w, &actions);
        let bytes = w.into_inner();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.varint().unwrap(), 2); // count (unsigned)
        assert_eq!(r.zigzag32().unwrap(), 0); // START_BREAK
        assert_eq!(r.zigzag32().unwrap(), 1);
        assert_eq!(r.zigzag32().unwrap(), 64);
        assert_eq!(r.zigzag32().unwrap(), -2);
        assert_eq!(r.zigzag32().unwrap(), 1); // face
        assert_eq!(r.zigzag32().unwrap(), 2); // STOP_BREAK, and nothing more
        assert!(r.is_empty(), "STOP_BREAK carries no position or face");
    }
}
