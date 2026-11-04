# Horizon ↔ Bedrock bridge — handover

A Horizon native mod that joins a **vanilla Bedrock Dedicated Server over NetherNet
(WebRTC)** and translates Bedrock packets into Horizon's clientbound packets, so a
Horizon player can play on a Bedrock server.

This document is the full state of the work: goal, architecture, what is verified
working, every wire format decoded, the traps that cost the most time, the reference
library and how to use it, and everything still outstanding.

---

## 1. Goal and acceptance criteria

Horizon is a custom Java-Edition client written in Rust. The mod loads into it as a
native mod, runs a Bedrock client, and translates in both directions.

The target: **join, see the world and other players with skins, move and be seen,
break and place blocks, fight, use chests and your inventory, chat and run commands,
and see the HUD.**

Status today: authentication, join, login, spawn, world rendering, entity movement,
block changes, player names and the server-list MOTD all work. Player input,
inventories, per-entity detail, biomes and dimensions do not.

---

## 2. Repositories, paths, versions

| Thing | Value |
|---|---|
| Mod repo | `/mnt/project/horizon-bedrock-bridge` → `github.com/miyoubi/horizon-bedrock-bridge`, branch `main` |
| Mod version | `0.1.1` |
| Horizon fork | `/mnt/project/horizon` → `github.com/miyoubi/horizon` @ **`4c3bf9437`** |
| Why the fork | adds `rtc = { version = "0.21", optional = true }` and feature `nethernet` to `crates/horizon-client/Cargo.toml`, putting 16 `rtc*` crates in `Cargo.lock` so a bundle crate may depend on them |
| horizon-mixins | `d9c83d7096b7…` (0.6.0) — first version supporting `[[crate]]` |
| Bundled Bedrock data | 1.26.60 block palette, protocol 2225, 23,960 block states |
| BDS | `/home/exer/bedrock-server-1.26.61.1` |
| BDS settings that matter | `transport=nethernet`, `client-side-chunk-generation-enabled=false`, `block-network-ids-are-hashes=false` |
| BDS ports | **TCP 19132** = NetherNet HTTP signalling · **UDP 7551** = WebRTC · UDP 19132 is *not* RakNet in this mode |
| Launcher | `/home/exer/Downloads/horizon-launcher-linux-x86_64/horizon-launcher` |
| Launcher data | `~/.local/share/horizon-launcher/` |
| Game homes | `~/.local/share/horizon/` and `~/.local/share/horizon/instances/native-mods/` |
| Instance | key `native-mods` ("Native mods") |
| Client log | `~/.local/share/horizon/instances/native-mods/logs/latest.log` |
| Engine trace | `<tmp>/horizon-bedrock-nethernet.log` (i.e. usually `/tmp/…`; respects `TMPDIR`) |
| References | `/mnt/project/references/` — see §11 |

---

## 3. Commands

```bash
# Build and install the mod: applies the mixins to Horizon's source, then compiles it
cd /mnt/project/horizon-bedrock-bridge
/home/exer/Downloads/horizon-launcher-linux-x86_64/horizon-launcher \
    native-build . --trust-native-code

# Engine tests (fast, offline) — 90 of them
CARGO_HOME=/mnt/hz-cargo TMPDIR=/mnt/hz-tmp CARGO_TARGET_DIR=/mnt/hz-target2 \
    cargo test -p bedrock-core --offline

# The injected module has no cargo target, so syntax-check it with rustfmt
rustfmt --edition 2024 --check mixins/bedrock_bridge/session.rs

# Package and install the bundle so the launcher's own builds use the same source
python3 tools/package.py
# then copy target/bedrock_bridge-0.1.1.hnmod each to
#   ~/.local/share/horizon/instances/native-mods/horizon-native-mods/bedrock_bridge.hnmod
#   ~/.local/share/horizon/horizon-native-mods/bedrock_bridge.hnmod
# and rewrite bedrock_bridge.origin.json = {id, version, owner, repo, commit, sha256}
```

**Build-key gotcha:** the launcher's cache key hashes the mod's **`.rs` files** (plus
revision, mixins revision, Rust version, profile and features). Bumping the version in
`horizon-mixin.toml` does **not** force a rebuild — touch a `.rs` file. The same trick
is needed to make the launcher compile a change to Horizon's *cached* source.

---

## 4. Architecture

```
Bedrock server ──RakNet or NetherNet──► bedrock-core (engine)
  ──decoded packets──► session.rs  (injected into horizon-client/src/net.rs
                                    as `crate::net::bedrock`)
  ──Event::Packet(Clientbound)──► net.rs event channel
  ──► GameState::poll_remote ──► handle_packet
```

**Engine — `mixins/bedrock_bridge/engine`, crate `bedrock-core`**

| Module | Role |
|---|---|
| `auth` | device code, XSTS, PlayFab, `auth_info` |
| `crypto` | P-384 identity key, handshake key, base64 |
| `raknet` | RakNet client |
| `client` | `BedrockClient` over a boxed `Transport`; login state machine, compression, crypto |
| `nethernet/{identity,join,webrtc,transport,message,auth_service,discovery,http}` | NetherNet: ES384 identity JWT, HTTP join, sans-I/O WebRTC peer and pump, `Transport` adapter, 1-byte fragment framing |
| `protocol/{mod,packets,ids}` | `Packet`/`Out` enums, codecs, packet ids |
| `chunk`, `subchunk`, `bitarray` | runtime block ids, v8/v9 sub-chunk storages, padded `BitStorage` |
| `blockstates`, `blockmap`, `mapping` | bundled palette, Bedrock→Java block translation, `blocks.nbt`/`items.json`/`biomes.json` |
| `nbt`, `png` | NBT access, zlib PNG writer |

**Mod glue — `mixins/bedrock_bridge/`**

- `session.rs` — `is_bedrock`, `split`, `base`, `status`, `try_connect`, `run` (the loop),
  `Bridge`, and `mod translate` (`Context`, `Blocks`, `column`, `emit_column`,
  `auth_input`, `clientbound`, `serverbound`).
- `hooks/connect.rs`, `hooks/connect_transferred.rs` (OVERWRITE, take
  `cfg::ClientInformation`), `hooks/send.rs` (HEAD, cancellable), `hooks/disconnect.rs`,
  `hooks/ping.rs` (HEAD, cancellable, synthesises a status for `bedrock:` addresses).
- `horizon-mixin.toml` (module, `[[crate]] engine`, five injections),
  `horizon-native.json` (generated), `compatibility.json` (pins).
- `tools/package.py` (bundle), `tools/embed_blockstates.py` (palette → Rust source).

**`translate::Context`:** `builtin`, `mappings`, `blocks: Option<Blocks>`, `spawn`,
`spawn_yaw`, `spawn_pitch`, `published`, `sections: HashMap<(i32,i32), BTreeMap<i8, Vec<u32>>>`,
`requests: Vec<Out>`, `wanted`, `wanted_center`, `entities: HashMap<u64,([f32;3],yaw,pitch,head)>`,
`runtime_id`, `unique_id`.

**Loop contract:** drain outbound Java→Bedrock; drain `ctx.requests` to the server;
`client.poll()` into a `VecDeque`; translate at most `TRANSLATE_PER_TICK = 4` packets;
sleep 2 ms. **The bound is load-bearing** — stalling the pump lets ICE consent lapse.

**Addressing:** `bedrock:host[:port]` = NetherNet (default), `bedrock+raknet:host[:port]`
= RakNet. The signalling base is `http://host:port` unless the address carries a URL.

---

## 5. Commit history

All pushed to `main` on the mod repo.

| Commit | What it did |
|---|---|
| `74eb8c5` | WebRTC join, live-verified |
| `c808d04` | Reach the Bedrock login over WebRTC |
| `76841bc` | Match a real client's login (captured) |
| `855bae7` | Full login over NetherNet — joins and spawns |
| `aa25fd2` | Resilient batch parsing |
| `13f10bd` | Ship `horizon-native.json` (horizon-mixins 0.6.0) |
| `c197048`, `3152152`, `18f543c` | More decoders |
| `02dd481` | Chunk -> Java block state resolver |
| `2d569af` | `LevelChunk` header |
| `4a952be` | Padded `BitStorage` |
| `8c4ceb4` | Probe captures |
| `759681a` | Remove hardcoded `/mnt` paths |
| `6de2fe9` | Palette ids → real blocks |
| `9535d32` | Chunk translation → `LevelChunkWithLight` |
| `1c5d4df` | Chunk cache centre at the player |
| `fe11889` | Root manifest, palette under `data/`, lock-safe packaging |
| `14dd422` | Pin the WebRTC-enabled Horizon; ship both transports |
| `2a8fa1d` | Move the pin after the force-push |
| `80f4b99` | New `connect` signature, embedded palette |
| `c57a1f1` | Trace the join and handshake |
| `c8a882f` | Stop translation starving the WebRTC pump |
| `4b6e1ad` | Repeat the player position once chunks exist |
| `071fae3` | Stream sub-chunks (blocks appear at all) |
| `783cef2` | Version 0.1.1 |
| `71fcd5d` | Player column first; stop churning the request queue |
| `3a808b3` | **`LEVEL_CHUNKS_LOAD_START`** — closes the loading screen |
| `775336a` | `MoveActorDelta` |
| `1266e0b` | Heightmap sky light |
| `ef674ab` | `PlayerAuthInput` encoder and send |
| `d3d0f90` | `PlayerList` decode and player info |
| `ac9a87a` | PNG writer |
| `8610fb9` | Skin `textures` property |
| `6edea81` | `UpdateBlock` |
| `2f785d2` | `UpdateSubChunkBlocks` |
| `ae97a0e` | **Revert movement to `MovePlayer`** (auth input rejected) |
| `1ab7f77` | Trace column placement and entity heights (**not yet built**) |
| `0f50d2f` | Decode the `StartGame` item palette |
| `225e324`, `76642e6` | Item identifier resolution, and the fix for its red test |
| `b15cc4c` | Item id lookups for the equipment packets |
| `5b8dc7b` | Read all three trailing bytes of `MobEquipment` |
| `d55e6ff` | Read the stack net id unsigned |

---

## 6. Verified working, live

- Full auth chain end to end.
- NetherNet join (signalling + ICE + data channel).
- Bedrock login, resource packs, `StartGame`, spawn — the server logs the player.
- Chunk streaming with correct block ids, heightmaps and heightmap-derived sky light.
- **Terrain and entities at the same heights** — a live column reads bedrock at −64 through
  grass at 68 to air, and the player stands on it (§10.1).
- **The loading screen closes** (requires `GameEvent{event:13}`).
- Entity movement, block changes, player names in the player list, server-list MOTD.
- `ItemRegistry` (162) decoded, and `MobEquipment` translated to `SetEquipment`.
- **Biome tint** — the `minecraft:worldgen/biome` registry is sent so grass and plants are
  green instead of falling back to badlands (§8.15); the world is plains everywhere for now.
- `SetHealth` decodes (it is a `zigzag32`, §8.16).
- A probe held a NetherNet session for 90 s with no ICE failure.

---

## 7. Wire formats decoded

Reuse these rather than re-deriving them.

- **Framing:** each batch is a 1-byte compressor id (`0` = raw deflate, `255` = none)
  followed by the payload. `Client Network Version` and `Login` are the only
  **big-endian** Bedrock fields.
- **`ResourcePackClientResponse` (1.26.40+):** varint status **then the same status as a
  string**; pack ids only for `send_packs`.
- **`RequestChunkRadius`:** varint radius plus a trailing `u8` max radius.
- **`SubChunkRequest` (175):** `dimension` **zigzag32**, `center = [chunkX, subChunkY, chunkZ]`,
  then offsets `[i8;3]`. **The sub-chunk Y is an absolute section index** (`floor(blockY/16)`),
  so the overworld runs `-4..=19` (min_y −64 → +319). A real client sends base Y **0** and
  carries the absolute index in the offset; the reply's section is `center.y + offset.y`.
  The Java section slot is that index **plus** `|min_y|/16` (4), which is what `column()`
  does. Using a non-zero base Y or subtracting 4 puts every section 64 blocks off (§10.1).
- **`LevelChunk`:** `x zz32, z zz32, dimension zz32, sub_chunk_count varint,
  highest_subchunk_count option zigzag32, cache_enabled bool, blobs array(varint, lu64),
  payload ByteArray, client_biome_update bool`.
- **Sub-chunk (v9):** first byte is the version (`9`); then `count u8`, `y u8`; per storage
  a `header u8` (`bit0` = runtime palette, `bits = header >> 1`), `bits == 0` meaning one
  zigzag id, otherwise **storage words first** (padded, `floor(32/bits)` per word,
  LSB-first) then a zigzag palette size then the entries.
- **`PlayerAuthInput` (144):** `f32 pitch, f32 yaw, vec3 position, f32 moveVecX,
  f32 moveVecZ, f32 headYaw, flags (65 bits: 7 per byte, continuation bit on all but the
  last, so 10 bytes), varint inputMode, varint playMode, varint interactionModel,
  vec2 interactRotation, varint64 tick, vec3 delta, [optional item interaction / stack
  request / block actions / vehicle by flag], vec2 analogMove, vec3 cameraOrientation,
  vec2 rawMove`.
  - Modes: `InputMode::MOUSE_KEYBOARD = 1`, `PlayMode::Normal = 0`,
    `InteractionModel::Crosshair = 1`.
  - Flags: `JUMPING 6, SNEAKING 8, UP 10, DOWN 11, LEFT 12, RIGHT 13, SPRINTING 20,
    START_SPRINTING 25, STOP_SPRINTING 26, START_SNEAKING 27, STOP_SNEAKING 28,
    START_JUMPING 31, PERFORM_BLOCK_ACTIONS 35, PERFORM_ITEM_STACK_REQUEST 36,
    HANDLED_TELEPORT 37, MISSED_SWING 39`; `NUMBER_OF_FLAGS = 65`.
- **`PlayerList` (63):** `type u8` (0 add, 1 remove), `count varint`; an add entry is
  `uuid[16], actorId zigzag64, username, xuid, platformChatId, buildPlatform i32, skin,
  isTeacher, isHost, isSubClient, colour u32`; then one `verified bool` per entry
  **after** all entries.
- **Skin:** `id, playfabId, resourcePatch, image (u32 width, u32 height, varint-length
  RGBA), animationCount i32 + each (image, u32 type, f32 frames, u32 expression),
  cape image, geometry, geometryVersion, animationData, capeId, fullSkinId, armSize,
  skinColour, persona pieces (5 strings + bool each), tints (string + i32 count + strings),
  then five bools: premium, persona, capeOnClassic, isPrimaryUser, override`.
- **`MobEquipment` (31):** `runtimeId varint64`, an item stack descriptor, then **three**
  bytes: slot, selected slot, container id.
- **Item stack descriptor (`readCereal`):** `i16 id`, `u16 stackSize`, varint aux,
  bool + one varint net id, varint blockRuntimeId, string userData. The non-cereal
  network form instead uses a varint id where `0` means empty — that is the form the
  inventory packets use.
- **`ItemRegistry` (162):** the item palette — varint count, then per entry
  `identifier string`, `i16 network id`, `bool component_based`, **version** (`zigzag32`:
  0 legacy, 1 data-driven, 2 none) and the item's **component NBT** (a named compound).
  The version and NBT are **always** present, whatever the flag says; reading them only
  for component-based entries desynchronises the stream.
- **`StartGame` has no item palette in 1.26.** It ends at the block palette
  (`block_properties`: varint count of `identifier + state NBT`) after `enchantment_seed`;
  the items arrive in `ItemRegistry` (162), which is sent once, right after `StartGame`.
  (An earlier build read an item palette out of `StartGame`, ran off the end of the
  packet and dropped the whole `StartGame` — 0f50d2f, fixed here.)
- **`ItemStackRequest` (147):** `requests array(varint)`; each is `request_id zigzag32`,
  `actions array(varint)`, `custom_names array(varint, string)`, `cause li32`. An action is
  `type_id` (**varint**, 0 take … 18 results_deprecated), `legacy_type_id` (**u8**, the same
  value), then take/place `count u8, source, destination`; swap `source, destination`; drop
  `count u8, source, random bool`; destroy/consume `count u8, source`. A slot is
  `FullContainerName{container_id u8 (ContainerSlotType), dynamic_id option u32}`, `slot u8`,
  `stack_id li32` — **not** a varint. `stack_id` is the server's net id for the stack
  (positive), a client request id (negative odd), a legacy request id (negative even), or 0
  for air; quoting the wrong one is refused. The player's own slots are `hotbar_and_inventory`
  (12) 0-35 (0-8 hotbar, 9-35 main), or `hotbar` (28)/`inventory` (29); armour 6,
  offhand 34, cursor 59.
- **`ItemStackResponse` (148):** `responses array(varint)` — **the count is part of the
  packet**; read it or every response shifts. Each response is `status u8` (0 ok, 1 error),
  `request_id zigzag32`, then, when ok, `containers array(varint)` of
  `FullContainerName, slots array(varint)` of `slot u8, hotbar_slot u8, count u8,
  item_stack_id option zigzag32, custom_name string, filtered_custom_name string,
  durability_correction zigzag32`. The container list is an `option` (a presence `bool`
  always precedes it, error or not — the 1.26.60 schema and the Rust server agree; the PHP
  and C++ libraries are the older shape). The response carries counts and stack ids but no
  item, so the client has to have predicted the move.

**Horizon packets to target:** `BlockUpdate {pos, state}` (play.rs:77),
`SectionBlocksUpdate` (:803), `PlayerInfoUpdate {actions: u8, entries}` (:675) with
`PlayerInfo` (:659) — action bits ADD_PLAYER 0, INITIALIZE_CHAT 1, UPDATE_GAME_MODE 2,
UPDATE_LISTED 3, UPDATE_LATENCY 4, UPDATE_DISPLAY_NAME 5, UPDATE_LIST_ORDER 6,
UPDATE_HAT 7 (we send `0b0001_1001`) — `Property {name, value, signature}`
(`packets/login.rs:9`), `SetEntityData {id, values: Vec<(u8,i32,Value)>}` (:818),
`SetEquipment {id, slots: Vec<(u8, Option<Stack>)>}` (:872),
`GameEvent {event: u8, param: f32}` (:358), `TeleportEntity` (:928), `RotateHead` (:800),
and `Stack {item: i32, count: i32, added, removed, wire}` with `Stack::new(item, count)`
(`horizon-protocol/src/item.rs:716`).

---

## 8. Hard-won facts

1. **horizon-mixins' dependency allow-list is Horizon's `Cargo.lock`.** It walks every
   entry of `dependencies`, `dev-dependencies` and `build-dependencies`, **optional
   included** (`native.rs:581-614`), and refuses any registry package that is not already
   locked (`native.rs:327-343`). `path`, `git` and `target` dependencies are rejected
   outright. That is why the fork commit exists.
2. **Bundle layout decides whether pins survive.** `bundle_in` (`native_mods.rs:164`)
   reads the `mixins` pin only when the folder is an unpacked `.hnmod`
   (`horizon-native.json` + `mixins/<id>/`). A folder with `horizon-mixin.toml` at the top
   gets `bundle(dir, None)` — the pin is dropped — and `github_mods::pack` then writes
   `MIXINS_REV` (`3f99a5c60dfa…`, horizon-mixins **0.5.0**), which has no `[[crate]]`,
   producing `E_MANIFEST`. The repo root therefore carries `horizon-native.json`,
   generated by `package.py`.
3. **Bundle contents:** `.rs` and `.toml` anywhere; anything else **only** under
   `mixins/<id>/data/`. A crate is staged to `native-crates/<mod-id>/<crate-id>/`, so it
   **cannot read the mod's `data/`** — hence the block palette is embedded as Rust source
   (`tools/embed_blockstates.py` → `src/blockstates_1_26_60.rs`, 674 KB from 108 KB gzip).
4. **The launcher caches Horizon's source** at `native/source/<rev>/`, reusing it when
   `.horizon-revision` exists (`native_mods.rs:600`). It only downloads from
   `horizon-rust/horizon`, so the fork commit must be seeded there. **Symlinks are rejected
   in staged inputs** — GitHub tarballs drop them, `git archive` keeps them.
5. **The build key hashes only the mod's `.rs` files** — version bumps do not force a
   rebuild.
6. **`Session::connect` and `connect_transferred` take `cfg::ClientInformation`**, not
   `view_distance: i8`.
7. **The loading screen.** `remote_overlay` (`remote.rs:7932`) draws while
   `player.is_none() || level_load.is_active()`. The tracker starts in `WaitingForServer`
   and only advances on **`GameEvent{event:13}`** (`remote.rs:3506`), so `tick_client_load`
   is a no-op before that — **its own timeout cannot fire either**. It then requires
   `streaming.is_lit(player's column)` (`streaming.rs:1038`, set only by `insert_remote`
   at `:797`).
8. **Partial columns.** With client-side chunk generation off, a `LevelChunk` payload can
   be ~34 bytes with no sections; the blocks arrive as `SubChunk` replies to
   `SubChunkRequest`.
9. **Block ids.** With hashes off, a runtime id indexes the version's block-state palette
   (air 18894, oak_log 1899, oak_leaves 3596).
10. **Light is not in the sub-chunk stream** — a section's bytes are fully consumed by its
    storages. Current light is a heightmap approximation; real light may trail the sections
    in a `LevelChunk` payload (unverified).
11. **`PlayerAuthInput` (144) is the primary input packet; `MovePlayer` (19) is deprecated
    and ignored** — and our encoder is currently rejected by the server.
12. **Java skin resolution.** With no `textures` property, Horizon falls back to the Mojang
    services (`remote.rs:4328`) — which is why players show the account skin. With one,
    `skins::unpack(value, signature, TEXTURE_URIS)` decides. Horizon's own LAN server sends
    `signature: Some("c2lnbmVk")` (`lan/hub.rs:3505`); we send `None`, and whether a
    `data:` URL is accepted is unknown.
13. **The player entity is created only by a `PlayerPosition`** (`remote_teleport`,
    `remote.rs:4646`, `self.player = Some(…)` at `:4687`).
14. **`remote_chunk_in_range`** = `view_distance.max(2) + 3` (`remote.rs:1712`), so the
    "Ignoring chunk since it's not in the view range" warnings are normal.
15. **Biome ids are network-registry indices, and an empty registry tints the world wrong.**
    `set_registries` (`remote.rs:1572`) builds the chunk biome table from the
    `minecraft:worldgen/biome` entries of the `Event::Joined` registries — each name mapped
    to the client's own id through `res.biomes().id(name)` — and sizes the palette with
    `ceil_log2(len)`. With `Dynamic::default()` (empty) that table is empty, so the biome id
    in every chunk (we sent `1`) fell back to the first biome (badlands) and the grass was
    brown. Send the registry in the client's own id order — the files under
    `data/minecraft/worldgen/biome`, sorted, which is exactly `Biomes::load`'s order and, for
    all-vanilla lists, `registry_order` — and use the plains index (42) as the chunk value.
    The world is plains everywhere until real per-chunk biomes are decoded, but the tint is
    correct and the plants are no longer black.
16. **`SetHealth` (42) is a `zigzag32`, not an `f32`.** Reading an `f32` needs 4 bytes, the
    packet is 1, so it failed to decode and health never reached the HUD.

---

## 9. Failure catalogue

| Symptom | Cause | Fix |
|---|---|---|
| `E_MANIFEST` (unknown field `crate`) | horizon-mixins 0.5.0; the pin was dropped or re-pinned | root manifest + bundle layout (`fe11889`) |
| `E_CRATE_DEPENDENCY` needing `rtc` | Horizon's lock has no WebRTC; optional deps are checked too | fork commit; pin (`14dd422`, `2a8fa1d`) |
| `E_INVALID_PATH` (symlinks) | `git archive` kept `CLAUDE.md` as a link | dereference the seeded source |
| `E_INVALID_BINDING: view_distance` | `connect` signature changed to `ClientInformation` | bindings and hooks (`80f4b99`) |
| Couldn't read `…/data/blockstates…` | a crate cannot read the mod's `data/` | embed as Rust source (`80f4b99`) |
| `PlayStatus` + `Disconnect` right after login | `RequestNetworkSettings`/`Login` protocol field is big-endian | fixed in `855bae7` |
| Server silent after `NetworkSettings` | missing 1-byte compressor id; zlib instead of raw deflate | both fixed |
| `Connection Request invalid.` | authInfo carried a `Certificate` field; skin `x5u` used the wrong key; skin missing fields; **`DeviceOS` 7** | `{AuthenticationType, Token}` only; P-384 key; `DeviceOS = 8` (Win32) |
| `no RakNet reply 0x06` | the server is netherNet; UDP 19132 is unused and signalling is **TCP** 19132 | `bedrock:` = NetherNet (`0e71e33`) |
| `data channel did not open` / `peer connection failed` after ~36 s | translation monopolised the loop and ICE consent lapsed | bounded translation queue (`c8a882f`) |
| Stuck on "Loading terrain" with chunks and a player present | the tracker never left `WaitingForServer` | send `GameEvent{event:13}` (`3a808b3`) |
| The player entity never created | `PlayerPosition` was sent before chunks existed and never repeated | repeat on publisher updates (`4b6e1ad`) |
| The player's column never lit (`is_lit` false) | blocks only arrive as `SubChunk` replies, which we neither requested nor handled | `SubChunkRequest` + merge (`071fae3`, `71fcd5d`) |
| Join dropped, server still lists the player, world reset to the build top | **`PlayerAuthInput` bytes rejected** | reverted to `MovePlayer` (`ae97a0e`) |
| Every packet after a `MobEquipment` one byte early | the decode read two trailing bytes, the wire has three | `5b8dc7b` |

---

## 10. Open questions and bugs

1. **RESOLVED — terrain drew 64 blocks low (four sections).** The cause was the
   `SubChunkRequest` base Y and the reply decode, and the `min_y` theory was wrong:
   `DimensionType::vanilla("minecraft:overworld")` (min_y −64) matches the client's
   dimension. The request sent `center.y = 4` with offsets `-4..=19` — so the server
   answered absolute section indices `0..=23` (y 0..383) — and the decode then subtracted 4
   again, so the section at world y 0 was filed at slot 0 (−64) instead of slot 4. Fixed:
   the request now uses base Y **0** (`center: [cx, 0, cz]`) and the reply's section is
   `position[1] + offset[1]` with no adjustment. Live-verified: a column at (35,89) reads
   bedrock at −64, stone, dirt, grass at 68, air above, and the player stands on it. The
   trace from `1ab7f77` (`column … first_y=Some(-4)`, `add entity … at …`) is what confirmed
   the sections now span the whole column.
2. **`PlayerAuthInput` is rejected** by the server (session dropped, world reset to the
   spawn sentinel). The encoder lives in `protocol/mod.rs` (`Out::PlayerAuthInput`) and is
   used by `auth_input()` in `session.rs`, currently bypassed. Field order matches
   `BedrockProtocol`. The suspects are the three mode constants, the tick's start value,
   and the flag bitset width. **Needs one captured frame.**
3. **Skins are unproven.** The `textures` property is sent (base64 JSON with a
   `data:image/png;base64,…` URL). If players still show the account skin, try
   `signature: Some("c2lnbmVk")` to match the LAN server, then serve the PNG from a local
   URL instead of a `data:` URL.
4. **`MoveActorDelta`'s rotation triple** is assumed to be `(pitch, yaw, head)` in byte
   degrees. Unverified against a capture.

---

## 11. The reference library — what each is, and how to use it

**Rule:** never guess a wire format. Order of authority: `BedrockProtocol` (structs) →
`bedrock-protocol-docs` (semantics) → `sculk-bedrockprotocol` (C++) / `Pumpkin` (Rust) →
`Geyser`/`ViaBedrock` (translation policy) → the `bedrock-protocol` JS client (empirical).
**Two independent references agreeing is the standard of proof used throughout this
project** — it is what found the missing `MobEquipment` byte.

| Reference | What it is | Use it for |
|---|---|---|
| **BedrockProtocol** (`references/BedrockProtocol`) | Cloudburst's PHP protocol library — one class per packet, each with `encodePayload`/`decodePayload` | The authority on layouts. `src/PlayerAuthInputPacket.php`, `src/types/PlayerAuthInputFlags.php` (65 flags), `src/serializer/CommonTypes.php` (`getSkin`, `getSkinImage`, `getNetworkItemStackDescriptor`), `src/serializer/BitSet.php` (7-bit groups with continuation) |
| **sculk-bedrockprotocol** (`references/sculk-bedrockprotocol`) | A **C++** Bedrock protocol implementation | Second, independent check on any layout. `codec/packet/MobEquipmentPacket.cpp`, `codec/inventory/item/NetworkItemStackDescriptor.cpp` (`readCereal` vs `read`), `codec/serializer/BinaryStream.cpp` (`readVariant`, `readOptional`) |
| **bedrock-protocol-docs** (`references/bedrock-protocol-docs`) | Official-style docs (`html/`) plus `additional_docs/` | `PlayerMovementOverview.md` (auth-input ticks, rewind, corrections), `MovementProtocolDeprecation.md` (why 19 is dead), `BlockBreakingOverview.md`, `ConfiguringAntiCheat.md` |
| **mappings-master** (`references/mappings-master`) | Geyser's data: `blocks.nbt` (35,723 Java states → Bedrock), `items.json` (1,658 Java names with `bedrock_identifier`), `biomes.json` | Block mapping (wired), item mapping (wired), biome ids (not yet) |
| **Geyser-master** (`references/Geyser-master`) | The canonical Bedrock↔Java proxy (Java) | Translation **policy**: metadata, items, containers, entity types, what it deliberately drops. Read `session/translator` |
| **ViaBedrock** (`references/ViaBedrock`), **ViaBedrockUtility**, **viafabricplus-bedrock**, **ViaFabricPlus-ver-26.3** | A Bedrock *client* protocol in Java, and its integration layer | Second opinion on metadata, items and containers; structure and phasing |
| **bedrock-protocol** (`references/bedrock-protocol`, JS) | The JS client that was actually got onto BDS | Capture path 1: log what a working client sends (`player_auth_input`, `inventory_transaction`, `item_stack_request`) |
| **nethernet** (`references/nethernet`) | A **Rust workspace** — `nethernet`, `nethernet-tokio`, `bevy-nethernet` — the reference this transport was ported from | Capture path 2. `nethernet-tokio` is the tokio runtime where a **server/accept** path lives; a logger on its message path gives a localhost listener on 7551 |
| **Pumpkin** (`references/Pumpkin`) | A **Rust** Bedrock server | Easiest reader to port. Note `crates/pumpkin-core/src/net/bedrock/play/player_auth_input.rs` is the packet **handler** (sprint/sneak/fly transitions, block actions), not the codec; the codec is in `crates/pumpkin-protocol` |
| **Nukkit-master** | A Java Bedrock server (older, simple) | Container and inventory semantics |
| **BedrockSkinUtility** (`references/BedrockSkinUtility`) | Skin conversion tooling | Exactly our problem: Bedrock RGBA → a Java-renderable skin. Check whether it serves the PNG over a URL — that answers the `data:` question |
| **Cloudburst-develop**, **Network-develop** | Cloudburst framework, `netty-transport-nethernet` | Signalling/ICE detail; a Java capture server |
| **protocol-docs**, **NBT-master**, **BlockStateUpdater-master**, **Floodgate-master**, **TunnelMC-master** | Protocol docs; Java NBT; Bedrock block-state versioning; Bedrock-on-Java auth; transport tunnelling | Cross-checks, network NBT, version targeting, identity mapping, older transports |

Already mined: `BedrockProtocol` (all of §7), `sculk-bedrockprotocol` (`MobEquipment`,
item stacks), `bedrock-protocol-docs` (movement/deprecation), `mappings-master`
(blocks and items), `nethernet` (the transport).

---

## 12. Remaining work

### Critical path
1. ~~Height offset (§10.1)~~ — **done**, live-verified.
2. `PlayerAuthInput` (§10.2) — blocks being a real player. **done**
3. ~~`MobEquipment → SetEquipment`~~ — **wired** (needs visual confirmation on a held item).
4. Entity metadata table.
5. Populate the `Dynamic` registries.

### A. Player and input
Full `PlayerAuthInput` flag set (jump, sneak, sprint and their start/stop pairs,
`HANDLED_TELEPORT`, `MISSED_SWING`, block actions) · analog move vector from Java's
input · decode `CorrectPlayerMovePrediction` (161) and apply it · block breaking
(Java `PlayerAction` → Bedrock 36 plus an `InventoryTransaction`) · placing and using
(→ `InventoryTransaction` 30) · swing/attack (→ `Animate` 44) · gamemode and abilities
(`SetPlayerGameType`, `UpdateAbilities` are decoded) · respawn and death (`Respawn`
decoded) · keepalives and `TickSync` correctness.

### B. Entities
`AddPlayer` → `Clientbound::AddPlayer` rather than collapsing into `AddEntity` ·
`SetEntityData` → metadata (name tags, health, pose, flags, owner) via a Bedrock
metadata-key → Java index/type table · `MobEquipment` → `SetEquipment` ·
`UpdateAttributes` → Java attributes (needs the attribute table) · `ActorEvent` →
`EntityEvent` and `HurtAnimation` · passengers / `SetPassengers` · `AddItemEntity` and
`TakeItemEntity` → item entities, so drops and pickups appear · addon entity models
(the identifiers parse today, the geometry does not) · verify skins and handle
`PlayerSkin` (93).

### C. World, blocks, chunks
`BlockEntityData` → Java `BlockEntityData` (chests, signs; `translate::nbt` exists and is
marked dead) · real per-chunk biomes (`BiomeDefinitionList` decodes as a unit today and the
`minecraft:worldgen/biome` registry is sent, but every chunk still says plains — decode the
sub-chunk biome palettes and map Bedrock biome ids) · `LevelEvent` → particles and sounds ·
`LevelSoundEvent` payload → sounds · real light (probably trailing a `LevelChunk`
payload) · a proper chunk streamer (radius from `SetChunkCacheRadius`, priority by
distance, cancellation on movement, per-column state) · `LevelChunk` blob cache
(`blobs`/`cache_enabled`) · dimensions (`ChangeDimension` 61 → Java `Respawn`) · weather.

### D. Inventories and UI (all downstream of the item mapping, which is now done)
`InventoryContent` → `ContainerSetContent` · `InventorySlot` → `ContainerSetSlot` ·
`ContainerOpen`/`Close` → `OpenScreen`/`ContainerClose` · `ContainerSetData` ·
`PlayerHotbar` → `SetHeldSlot` · `InventoryTransaction` and `ItemStackRequest` (147) →
clicks and item use · crafting (`CraftingData` 52, `CraftingEvent` 53,
`CreativeContent` 145) · item components from `ItemRegistry` (162).

### E. Chat, commands, HUD
`SetTitle` 88 · scoreboard (`SetScore`, `RemoveObjective`, `SetDisplayObjective`) ·
`BossEvent` · `ToastRequest` · `AvailableCommands` 76 into Java's command tree, with
`CommandRequest`/`CommandOutput` · `GameRulesChanged` · decide explicitly what to do with
forms and emotes.

### F. Registries
Populate `Dynamic` — we still send `Dynamic::default()` — with biomes, dimension types,
damage types, chat types and item components; decode `BiomeDefinitionList` into it.

### G. NetherNet robustness
Verify the server's `a=identity` (TOFU pinning) · reconnect with backoff on ICE failure ·
transfer support (`connect_transferred` refuses Bedrock addresses today) · an unreliable
channel for movement deltas · timeouts and errors surfaced into the trace · compression
edge cases.

### H. Packaging, CI, maintenance
CI running `cargo test -p bedrock-core` plus a headless mixin apply · named hook points so
the mod is not tied to `4c3bf9437` · upstream the `package.py` recursion fix · **rotate the
GitHub token** · refresh the README · golden-byte tests for the encoder and decoder tests
for `PlayerList` and the skin.

### Dependency notes
Items gate all of D and `MobEquipment` in B. `Dynamic` gates biomes and item components,
and becomes the next wall the moment items land. Entity metadata gates name tags and
health. Dimensions gate the nether/end and correct `Respawn`.

### Honest limits
This will not be feature-identical to a Bedrock client: persona and emote UI, marketplace
content and some addon behaviour will be approximations.

---

## 13. Immediate next steps

1. **Full inventory support — the active task.** Inventory does not work: picking up,
   moving or dropping an item, chests and anything else that opens an inventory (including
   bundles), wearing armor, and using shields and totems. Every interaction snaps back — the
   item returns to its slot and nothing moves. Acceptance is the full set: stack move,
   single-item move, shift-click transfer, number-key swap, Q on a slot, Q on the cursor,
   armor equip/unequip, chest open → item in → shift-click back → close, bundles, shields,
   totems.
   - **State:** `ItemStackRequest` (147) is encoded the way 1.26.60 lays it out (each action
     carries its type as a varint **and** a legacy `u8`; a slot's stack id is an `li32`; the
     container is a `ContainerSlotType`), and the server's answers are now read as the batch
     they are. The "refusals with no container list" were the decoder skipping the packet's
     response **count**, so the count byte was read as the first response's status and every
     verdict shifted — every move read as refused and the client never applied the result.
     The move is also now predicted locally (a real client does) because the response names
     the count and stack id of each slot but not the item, and a copy of the touched slots is
     put back if the server refuses. **Pending a hand test** of the full set below.
   - **Evidence:** `/tmp/horizon-bedrock-nethernet.log` carries the click
     (`INV click container=… slot=… button=… input=…`), the request bytes
     (`STACK REQ id=… <hex>`) and the server's verdict (`inventory move … refused`);
     `NETHER_DUMP=<packet id>` with `NETHER_DUMP_FILE=…` dumps raw inbound packets.
   - **Authority:** verify every field against the version-matched 1.26.60 schema rather than
     assuming it, and prefer a capture of a real client's request over inference — the relay
     at `/tmp/hz-capture-relay.js` (listens on 19133, forwards to `127.0.0.1:19132`, logs
     packet 147 to `/tmp/hz-capture.log`) exists for that, but its upstream connection does
     not complete yet, so no capture has been taken.
   - **Environment:** BDS speaks NetherNet — TCP 19132 is its HTTP signaling endpoint and UDP
     7551 is LAN discovery, which the Wine Bedrock client on this machine holds while it is
     open, so only one of client, relay and BDS can hold it at a time. The control socket
     rejects mouse input while an inventory is open, so inventory clicks have to be made by
     hand — batch changes per build.
2. **DONE — height offset (§10.1).** The `SubChunkRequest` base Y and reply decode were
   fixed; terrain and entities now share the same Y, live-verified on a BDS join. Also fixed
   in the same build: `StartGame` stopped decoding because `0f50d2f` read an item palette
   that 1.26 does not put there (§7) — the palette moved to `ItemRegistry` (162).
3. **`MobEquipment → SetEquipment`.** Wired: `ItemRegistry` fills `ctx.items`, and a
   `MobEquipment` on the inventory/hotbar container (0/28/29) maps to main hand, off hand
   (34/119) to off hand, with `Stack::new(java_item, count)`; an unknown id clears the
   slot. Compiles and runs; visual confirmation on another player's held item is still
   open.
4. **Capture one `PlayerAuthInput`** (§10.2) — still the gate on being a real player. Use
   `references/nethernet`'s `nethernet-tokio` accept path for a localhost listener on 7551,
   or log the JS `bedrock-protocol` client's outgoing packets.

---

## 14. Debts and risks

- The GitHub token used for pushes was pasted in plaintext and is still valid — **rotate
  it**.
- `horizon-native.json` at the repo root is generated by `package.py`; never hand-edit it.
- `compatibility.json` pins `horizon` and `mixins` and carries a `source_sha256` that must
  be refreshed whenever the pin moves.
- The launcher's cached Horizon source is currently **pristine** (a temporary debug patch
  in `remote.rs` was reverted).
- MOTD is synthesised, not read from the server.
- Xbox sign-in surfaces the device code as a **status line**, not a UI.
- A compiler warning, `unused doc comment`, appears in the engine build and has not been
  identified; it predates `d55e6ff`.
- `MovePlayer` is still what moves us, so the server treats the player as stationary.

---

## 15. Working style that fits this project

- Verify each change cheaply: `cargo test -p bedrock-core` for the engine,
  `rustfmt --check` for the injected module. A launcher build is 3–10 minutes and is the
  only thing that typechecks `session.rs`, so batch related changes per build.
- Gate commits on the suite actually reporting `90 passed`; `grep`-and-continue has
  already let one red commit through (`225e324`, fixed by `76642e6`).
- Never guess a wire format — check two independent references and say which when they
  disagree (this is how `MobEquipment` and the stack net id were both settled).
- Keep the WebRTC pump sacred: anything heavy on the translate path must be bounded.
- No hardcoded machine paths in code; use env and temp dirs.
- Scope structural edits precisely. A global search for `window_id` in `packets.rs` hits
  `ContainerOpen`, not `MobEquipment`; that mistake cost a compile and a revert.

## Lighting (current state)

The Bedrock stream carries **no light** at all — chunks are blocks, block
actors and a heightmap, and a Bedrock client lights itself. A Java client
trusts the light it is given, so the bridge has to produce it.

Two attempts are in the tree and neither is the answer:

- A flood fill (sky + block light over the column). Correct in shape, but the
  renderer and the fill disagree about what a level means, so slopes band.
- The current heightmap engine: sky light 15 at and above the surface, zero
  below; block light as stamped spheres per emitter. Surface and caves read
  well; walls still show bands.

**The next step is to stop hand-rolling it.** The game computes light itself
for LAN hosts: `crates/horizon-client/src/lan/chunks.rs` — `light_sections`,
`light_masks`, `sent_layers`, `light_data_of`, plus the `LightEngine` in
`crates/horizon-world/src/level/tick.rs`. `light_sections` takes
`&[Arc<Section>]`, which are the same sections the bridge already builds in
`column()` for the chunk packet, so there is no conversion layer — hand them
over and let `light_data_of` fill the chunk. `light_update(chunk, changed)`
then gives incremental relighting on block changes, which is also still
missing (a placed torch does not relight until the chunk reloads).

**Resolved: use the render crate's engine instead.** `lan/chunks.rs` is private
to the client crate, but `horizon-client-render/src/light.rs` is public and is
the same vanilla-rules engine the renderer itself uses — which is why using it
removes the banding rather than shifting it:

    pub fn light_chunk(region: &[[Option<&Chunk>; 3]; 3],
                       tables: &LightTables,
                       sky_light: bool) -> Vec<Light>;
    pub fn apply_lighting(chunk: &mut Chunk, lights: Vec<Light>);
    pub fn apply_relighting(chunk: &mut Chunk, lights: Vec<Light>,
                            changed: &mut LightChanges);

`LightTables::new(&BlockFacts)` gives `emission` and `dampening` per state.
`LightChanges::sections()` yields exactly the sections a block update touched,
which is what the `LightUpdate` packet wants. The 3x3 region also gives
cross-border light for free.

So: build the 3x3 `Chunk` region around the column the bridge is sending, call
`light_chunk`, and let `apply_lighting` fill the sections; on block updates use
`apply_relighting` plus `LightChanges::sections()`. Then delete the bridge's
own light module.

## Pitfalls hit in this area

- Bedrock chunks carry no light; do not look for it.
- The light arrays are 98,304 bytes; an index with a negative coordinate wraps
  to a huge usize and panics the session thread. That panic looks like "stuck
  on loading terrain, then kicked into an empty world" — read the panic line
  before optimising anything.
- Read crashes from the play log first: `crates/horizon-client/src/net.rs:<n>`
  maps into the injected `session.rs`.
