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

Status today: authentication, join, login, spawn, chunk streaming, block changes,
attacks, block placement/breaking, placing/eating item use, hotbar selection,
held-item sync, time/weather, the death screen, automatic respawn, and the
death/removal animation clock work. The bridge now runs against Horizon
`d7077bc27` (the split `horizon-client-lib` layout). `main` does **not** apply
movement corrections and does **not** install a movement hook in Horizon. Two
movement experiments are kept on separate branches only:
`bedrock-movement-parity` (bridge-side shadow) and `bedrock-native-parity`
(Horizon-hook parity). Inventory is currently unreliable: Q-drop, dropping
outside the inventory window, and some stack moves fail. Non-food item use,
effects, fake-entity removal, and `@s` command origins are still open.

---

## 1a. Prompt for the next session — parity after movement

```
PROJECT
        /mnt/project/horizon — Horizon, a Java-edition Minecraft client written in
        Rust. /mnt/project/horizon-bedrock-bridge — a bridge crate whose mixin
        (mixins/bedrock_bridge/session.rs, injected as crate::net::bedrock) lets that
        client join Bedrock servers over NetherNet. Goal: the bridge's behaviour must
        match a real Bedrock client against the same BDS server. Read
        horizon-bedrock-bridge/HANDOVER.md before anything else — it records the
        current state, the failed approaches and their causes, and the API notes.

        AUTHORITATIVE REFERENCES (read them; do not guess packet shapes)
          references/protocol-docs, references/bedrock-protocol-docs — packet and enum
          docs. references/data, references/BedrockProtocol — generated protocol
          definitions. references/Nukkit-master, references/Cloudburst-develop —
          Bedrock server implementations. references/ViaBedrock, references/Geyser-
          master — Bedrock-to-Java protocol translators, the closest thing to what
          this bridge does. references/Pumpkin — a Bedrock server in Rust.
          references/bedrock-wiki, references/BlockStateUpdater-master.
          references/bedsim — the Bedrock movement simulator, for movement only.

        CURRENT STATE
        Join over NetherNet, login, spawn, chunk streaming, block placement and
        breaking, attacks, placing/eating item use, hotbar selection, held-item sync,
        time/weather, the death screen, automatic respawn, and the death/removal
        animation clock work. The last build is native-d7077bc-9fd2bc2c4998 against
        Horizon d7077bc27. The main branch is stable and does not apply server movement
        corrections. Two movement experiments live on separate branches only:
          bedrock-movement-parity  — bridge-side movement shadow
          bedrock-native-parity    — movement hook installed into Horizon
        Do not mix those branches into the work below.

        KNOWN BROKEN / UNVERIFIED
        - Inventory is currently unreliable: Q-drop, dropping outside the inventory
          window, and some stack moves fail. The server owns the inventory; every move
          is an ItemStackRequest (147), and the server's answers are the authority.
        - Non-food item use (bows, shields, fireworks) is only partially wired.
        - Mob effects are not translated.
        - Some entities outlive their removal on the client, and a respawn can leave a
          dead player showing.
        - `/give @s ...` sometimes answers commands.generic.noTargetMatch because the
          command origin's uuid/player id do not always resolve.

        TASK — PARITY AFTER MOVEMENT (in this order, one commit each)
        1. ITEM USE FOR NON-FOOD ITEMS
           Fireworks, bows, shields, and other usable items. Use PlayerAction 37
           (StartUsingItem), a Use action rather than Place, and the existing release
           transaction (ItemReleaseTransaction). Verify that the server actually
           consumes/ends the use. Do not shrink the held stack for non-place items.

        2. EFFECTS
           MobEffect -> Java EntityEffect/RemoveEntityEffect, for the local player and
           other entities. Keep the server's duration/amplifier/ambient/particle flags.

        3. FAKE ENTITIES / REMOVAL
           Trace RemoveEntity ids against the entities the client still holds. Check
           the unique-id -> runtime-id mapping, entities that leave the published area
           without a removal, and players that respawn with a reused runtime id. Keep
           the death animation, then remove.

        4. INVENTORY EDGE CASES
           Dropping out of the inventory window currently sends nothing in some paths;
           fix Q on a slot, Q on the cursor, click-and-drag outside the window, and the
           stuck-click state that needs a rejoin. Check ItemStackRequest field values
           against the 1.26.60 schema before changing bytes.

        5. @s COMMAND ORIGIN
           Make the command origin's uuid and player id resolve reliably so selectors
           such as @s work every time.

        6. REST OF SECTION 12 (parity)
           Player/input: full auth-input flags, block actions, abilities, keepalive.
           Entities: metadata table, attributes, ActorEvent, passengers, item entities,
           skins. World: block entities, per-chunk biomes, dimensions, sounds/particles,
           real light. Inventories/UI: container content/slots, crafting, creative.
           Chat/HUD: titles, scoreboard, boss bar, toasts, gamerules. Registries:
           populate Dynamic. NetherNet: reconnect/backoff, transfer, timeouts.

        HOW TO WORK
        Build: /home/exer/Downloads/horizon-launcher-linux-x86_64/horizon-launcher
               native-build /mnt/project/horizon-bedrock-bridge --instance native-mods
               --trust-native-code
        Launch: same binary, play --instance native-mods --server
                bedrock:127.0.0.1:19132 --arg --control
        Logs: /tmp/hz-play.log (game, including panics) and
              /tmp/horizon-bedrock-nethernet.log (bridge trace).
        The control socket drives the player: look, key, mouse, tp, chat, status. It
        rejects mouse input while an inventory is open, so inventory clicks must be
        made by hand — batch inventory changes per build.

        Rules: one commit per logical change; keep HANDOVER.md out of other commits
        (it gets its own); never name the reference projects in commit messages or
        code comments; build and launch before handing back, and confirm no panic in
        the log.
```

---

## 2. Repositories, paths, versions

| Thing | Value |
|---|---|
| Mod repo | `/mnt/project/horizon-bedrock-bridge` → `github.com/miyoubi/horizon-bedrock-bridge`, branch `main` |
| Mod version | `0.1.1` |
| Horizon fork | `/mnt/project/horizon` → `github.com/miyoubi/horizon` @ **`d7077bc27`** (current upstream `main` + WebRTC commit) |
| Why the fork | adds `rtc = { version = "0.21", optional = true }` and feature `nethernet` to `crates/horizon-client/Cargo.toml`, putting 16 `rtc*` crates in `Cargo.lock` so a bundle crate may depend on them |
| horizon-mixins | `e8cd0aa38cff…` (0.6.0+) |
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
   - **The container, and the cursor.** The server answers a bad slot with a precise status,
     so a gated probe (one move under each candidate container, see below) settled this
     against the live server: a player slot named under `hotbar_and_inventory` (12) is
     refused (`FailedToValidateSrcSlot`, 49), under `hotbar` (28) it validates as a source and
     the cursor destination is refused instead (`FailedToValidateDstSlot`, 50), and `inventory`
     (29) is like 12. So with no screen open the client names `hotbar`/`inventory` separately
     and has no cursor; the cursor and the combined container only exist while the inventory
     **screen** is open. Since every Java click is built on the cursor, the bridge asks the
     server for that screen with `Interact` (33) action `open_inventory` (6), marks it open,
     and names all player slots under the combined container; a respawn re-asks. BDS does not
     send a `ContainerOpen` for it — the screen is the client's, as with the Bedrock↔Java
     translators. This last part is **not yet confirmed live** (the server went down mid-test;
     see §13 note below).
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
   - **The server went down mid-test.** The listening BDS died while a session was up (`Disconnect`
     reason 39, then a reconnect loop), so the last change could not be confirmed live and a
     plain survival inventory with no items cannot be probed. A second `bedrock_server` is
     still running but not listening on 19132; restart the server before testing. Until then,
     the chunk and entity symptoms are the dead server, not the bridge.
   - **A gated probe** was used and then removed: with `HZ_INV_PROBE` set, on the first player
     `InventoryContent` it sent the same move under containers 12/28/29 and traced the answers.
     Re-add it (or the `NETHER_DUMP` dump) to settle a slot question against a live server.
2. Add chunk-quality trace fields and finish remaining Bedrock definition ports

    - `LevelChunk` stays short; log its subchunk count. The 1.26.60 `LevelChunk` packet has `sub_chunk_count` (`0` here, real payloads of the client's `10`–`13` columns), so nothing arrives inline and every column depends on its `SubChunk` replies (results `1` success, `2` empty, `6` out of range, `8` beyond height).
    - Distinguish requested vs received section counts: the old bridge emitted a column after every individual section arrival, so a column changed shape many times (`1` section, then `2`, …) and the client spent its time re-`unpack`-ing and re-lighting partial columns before they filled. Buffer per column and emit once when it is complete; the outstanding bounded work is logging `sub_chunk_count` alongside it.
    - Keep the SubChunk replies three-part state machine out of the bridge file and keep draining the session pump bounded so ICE consent cannot lapse.
    - Extend the `NetID` log handling to Bedrock and bedrock definitions.
    - Remove the inventory items remains, which are temporary testing fields that can provide information for the continued Bridges API and ultimately for tracking and resolving the issues encountered.

3. Repeat the initial connection with `HZ_INV_TRACE=1 HZ_ENT_TRACE=1` and verify whether the remaining terrain and outside chunks arrive; once they do, wire in predicate lighting following the Bedrock definitions. Two open items hang off this path: the death animation for hostile mobs, regular mobs and players (`EntityEvent(3)` and `HZ_ENT_TRACE=1 DEATH/REMOVE` have never been captured — needs a live kill), and the used-up-block consumption path (`ItemUseOnBlock` not decrementing server-side). Do not start new work unprompted.

### Commands and item use (latest)

**Since that section was written** (five commits):

- `Start the input tick on the server's clock and send the real position delta` — the input
  tick was seeded from `StartGame::current_tick` and `Pos Delta` is now the raw position
  change. A six-second walk and a six-second sprint-jump draw **zero**
  `CorrectPlayerMovePrediction` on the local server; before, the session corrected
  constantly.
- `Apply the server's movement corrections again` — **the correction that followed was
  also needed and is the thing to remember**: a correction names the *input* tick, and the
  rewind-window check compared it with the local counter, so once the input tick became the
  server's clock every correction looked future-dated and was dropped, leaving a player
  stuck against water and block edges.
- `Rebase a movement correction instead of rewinding to it` — applying the correction's
  absolute position teleported the player back to a tick they had already left. The bridge
  now keeps the feet position it reported for each of the last 64 input ticks and applies
  only the *offset* (server position at that tick minus what we claimed there) to the
  player's current position, so movement continues and only the drift goes. Over three
  blocks is treated as a teleport and taken absolutely; a tick outside the history falls
  back to the server's position. ViaBedrock checks the tick against the client's counter
  the same way but has no history to rebase with, so it jumps; Pumpkin's server never
  corrects.
- `Give the game the server's command tree` — `AvailableCommands` is decoded and turned into
  the game's tree (82 commands, names, aliases, literal subcommands, typed arguments), so
  `/` lists commands and the game's own "did you mean" works. The layout came from a
  396 KB capture: permission is an enum *name* (`gamedirectors`), the parameter symbol is a
  u32 bitfield (`0x100000` valid type, `0x200000` enum, `0x4000000` soft, `0x1000000`
  postfix), and the chained-subcommand indexes are fixed u32s, not varints.
- `Tell the server when the held stack itself changed` — `MobEquipment` now follows the
  *contents* of the held slot (a settled move, a whole inventory, one slot), not just the
  selected slot, so moving the held stack away or using the last of it no longer leaves the
  old item in the hand on the server.
- `End an item use when the game releases it` — Java's RELEASE_USE_ITEM (action 6) was
  unhandled; it now sends an `ItemReleaseTransaction`, which is what stops food, a bow, a
  firework or a shield from staying in use.

**Live findings still open**, reported while playing on this build:

1. **Dropping an item out of the inventory window** makes it vanish on the client but it is
   still in the inventory server-side and no item entity appears. `ContainerClick` with a
   slot outside the window is `clicked_slot` → `None` → no request is sent; the drop needs
   an `ItemStackRequest` drop action (or an `ItemRelease`) and no local removal until the
   server answers.
2. **Item moves sometimes stop working until a rejoin** — a stuck request/`pending_ids`/
   `inv_open` state (or an unanswered request) is the suspect; a periodic
   `InventoryMismatch` when a request has been open too long would recover it.
3. **Fireworks, bows and most non-food items do nothing**, the animation sticks, and the
   shield no longer works. These all need the *use* half: a `Use` item-use action (not
   `Place`) plus `PlayerAction` StartUsingItem (37), with the release now in place.
4. **Effects/status** are not implemented: `MobEffect` (28) is not decoded, so no Java
   `EntityEffect`/`RemoveEntityEffect`.
5. **Entities that the server has removed sometimes stay on screen** ("fake entities"). The
   `RemoveEntity` path maps a unique id to a runtime id through `ctx.uniques`; a missing
   mapping, or an entity leaving the published area without a removal, leaves the ghost.
   Worth a trace of `RemoveEntity` ids vs the entities the client still holds.
6. `@s` in a command is unreliable (`commands.generic.noTargetMatch` seen once): the
   command origin's uuid/player id do not always resolve to the player.


**Commands work** (commit `Run Bedrock commands instead of sending them as chat`). A Java
`ChatCommand` is no longer sent as a Bedrock `Text` with a leading slash (the server
ignores those); it goes out as a `CommandRequest`, and the server's `CommandOutput` is
decoded back into chat lines (one per message line; the help body arrives as plain text).

The wire format was **settled against the local server**, and the schema was wrong for it:

| Field | 1.26.60 schema | What BDS 1.26.61 accepts |
|---|---|---|
| origin type | `uint8` | **string** `player` |
| command version | `int32` (CurrentCmdVersion) | **string** `latest` |

Sending the numeric form is refused with `ViolationWarning: "Command exceeds maximum size
of 512 characters"`, which is what a misaligned origin looks like. `CommandOutput` names
its type as a string too (`alloutput`) and ends with one padding zero byte; the decoder
tries the shapes that have shipped (named/numeric origin and header, fixed/varint success
count) and keeps the one that consumes the packet, so both forms decode. The origin
carries the player's unique id from `StartGame` and a random instance uuid.

Known gaps, in order of annoyance:

- the game's own language table has no entries for Bedrock's command keys, so a keyed
  output shows as `key value...` (the help body reads fine);
- `AvailableCommands` (76) is still not decoded, so `/` suggests nothing;
- **`@s` is unreliable**: `/list`, `/help`, `/me`, `/tell @s` answered, but a later
  `/give @s dirt 4` came back as `commands.generic.noTargetMatch` — a selector resolves
  the executor from the command origin, so the uuid/player id we send is not always
  enough. Capture a real client's `CommandRequest` if a selector needs to be exact.

**Item use consumption is still open.** Placing a block works (the block appears) but the
held stack never shrinks server-side, which is what a survival session feels as duplicated
blocks. What was established, all measured against the local server:

- the standalone `InventoryTransaction` (type 2, ItemUse/Place) we send is **byte-for-byte
  what the Bedrock↔Java proxy sends** (verified by dumping our own packet with
  `HZ_DUMP_OUT=30` and decoding it): one action `container inventory / slot 0 / dirt 10 ->
  dirt 9` with the right stack net id, the correct action type (0 = Place), trigger 1,
  block position/face/slot/hand, the clicked block's runtime id, predicted result 1 and
  cooldown 0, bracketed by `PlayerAction` 28/29;
- the server answers with an `InventoryContent` that still reads count 10, so it is not a
  client-side display problem;
- moves to the modern form — carrying the use inside `PlayerAuthInput` (`PERFORM_ITEM_
  INTERACTION`, bit 34, `PackedItemUseLegacyInventoryTransaction`: legacy request id 0, a
  "has legacy slots" bool false, then the actions and the item-use fields) — pass without
  a violation when written that way, but nothing is consumed either; sending *both* forms
  stopped the placement itself from happening, so they are not additive;
- two encoding changes made during this were reverted and must not be "fixed" again
  without a capture: removing the 4-byte user-data trailer from `write_item`, and dropping
  the "source flags present" flag in the inventory action. Both broke the placement that
  otherwise works.

**Next step for this item:** capture a real Bedrock client's placement frames (the Wine
client through the relay, or the JS client) and diff them against `OUT id=30` and the
auth-input dump; the reference proxies agree with what we send, so the missing piece is
probably *another* packet a real client sends around the placement (a `PlayerAuthInput`
block action, an `ItemStackRequest` consume, or a `PlayerAction` variant).

### Chunk distance state (latest)

The streamed radius is `min(the game's render-distance setting, the server's grant)`:
`requested_radius` comes from `ClientInformation` (the game's setting), `granted_radius`
from `ChunkRadiusUpdated`, and `chunk_radius` is the effective value. `Login` and
`SetChunkCacheRadius` both carry it; a change emits `SetChunkCacheRadius` exactly once
(tracked by `Context::sent_radius`, so a value set elsewhere cannot hide the change — that
bug left the game with a 12-chunk storage while the bridge streamed 32 and the game
silently dropped most columns).

Each column is tracked individually (`Context::columns`):

- a `LevelChunk` deletes anything held for that column, records the sections the payload
  carries and queues **one** `SubChunkRequest` for the sections up to the highest the
  server named (`highest_subchunk`, a signed section count; a negative value means the
  whole column). The request base Y is 0 and the offsets carry the absolute section
  index, as before;
- `SubChunk` replies fill the sections in and mark those answered (result `1` blocks,
  `6` all air, anything else counted as answered so a refusal cannot stall a column);
- a column is sent to the game **once**, when every section asked for is answered and its
  3x3 neighbourhood is finished, so the game never re-unpacks or re-lights a partial
  column and the light is final. If a neighbour has not arrived after 1.5 s the column is
  sent anyway and remembered (`ColumnState::relight`): when the neighbour is emitted its
  face neighbours get a `LightUpdate`, so a missing reply leaves no hole.
- the player's own column is always emitted first, regardless of its neighbours — it is
  what ends the loading screen.
- emission is **distance ordered** and paced (`EMIT_PER_SECOND`, `EMIT_BURST`): the game
  gives chunk packets 1 ms a frame and, once a backlog is 50 ms old, handles the whole
  backlog in one frame, which the player feels as a frozen chat box.

Ordering and pacing live on `Context::ready` (a min-heap keyed by distance²) and are fed
by `note_finished`. Requests are paced by `REQUEST_WINDOW` (16 outstanding) rather than by
the receive backlog: the server does work for every request, and a busy server answers the
player's own chat late (measured 1.5–7 s of chat round trip when the window was wide).
The translate step is time-bounded (`TRANSLATE_BUDGET`, 20 ms) so the outbound direction
and the WebRTC pump are serviced every iteration.

Measured live (local BDS, grant 32, game setting 80 → stream 32): loading screen closes
~2 s after the join (`server.loaded` true), 54 columns at +2 s, ~270 at +6 s, ~1070 at
+20 s, ~3300 at +60 s, ending at 3400+ columns with the game holding exactly what was
sent, no `Ignoring chunk since it's not in the view range`, no chunk decode errors and no
panic. Chat round trip during streaming is ~180–270 ms (it was 1.5–7.3 s before the
pacing changes). RSS is ~830 MB–1 GB, down from ~1.6 GB. A radius change to 12 and back
to 32 follows correctly (`ChunkRadiusUpdated(12)` → `stream=12`, then `granted=32,
stream=32`, columns refilling). The world spawn moved during testing, and a BDS respawn
re-streams the area the same way (publisher centre follows the player).

**The inflow, not the bridge, is now the limit.** With the window at 16 the server's
LevelChunk replies arrive at roughly 50–85 columns/s (BDS's own per-tick chunk send
budget; there is no client-side knob for it), and the loop keeps up: `wanted` and the
in-flight count are usually 0–10 and the emit budget is never exhausted. Raising the
window to 48 *lowered* throughput to ~30/s and pushed chat latency to ~650 ms, so
widening it is not the answer. Multi-threading the bridge would not raise this either:
the pipeline is waiting on the server's replies, and the client's own insertion/meshing
is what the pacing protects.

Known remaining gaps:

- **Teleport by server command is not verifiable here** (no op, and `/tp` typed as chat
  is ignored by BDS — Bedrock commands go in a `CommandRequest`, still unimplemented).
  What was verified is the mechanism it depends on: `SetChunkCacheCenter` follows every
  publisher update, so the game's chunk storage re-centres before the destination
  columns arrive (walked 37+ chunks from spawn with no out-of-range drop). The same
  path runs on a respawn.
- The streamed area is a circle of radius `granted + 1` (the extra ring is what makes the
  outermost rendered ring meshable; the game's own LAN host streams `view_distance + 1`).
  A gap between that circle and the renderer's square storage is not yet measured; the
  client ends with fewer columns than its `view_distance + 3` storage could hold, which
  is expected for a circle.
- Per-section runtime ids are kept only within 8 chunks of the publisher centre
  (`RUNTIME_ID_RADIUS`); a block use further out falls back to the Java→Bedrock reverse
  mapping, which is what the bridge did before the per-section copy existed.
- `EMIT_PER_SECOND` is above the server's inflow in normal play, so it only shapes the
  case where columns are already in hand (radius change, respawn).
- A respawn that the client handles does not reset `ColumnState::sent`; recovery relies
  on the server re-publishing (`LevelChunk` resets the column). This should be made
  explicit: on `Respawn`/`ChangeDimension`, mark everything unsent and re-emit from the
  cache.
- `NETHER_DUMP=<packet id>` plus `NETHER_DUMP_FILE=…` dumps raw inbound bodies and was
  what settled the signed `ChunkPos`: the wire has zigzag varints there, and reading them
  as plain varints mapped negative chunks onto positive odd numbers (which the old code
  did, and `ctx.chunks` only looked right because the sections were filed by the request's
  coordinates). `HZ_CHUNK_TRACE=1` prints the per-column trace (`chunk recv`/`chunk req`/
  `chunk emit`/`subchunk`/`chunks columns=…` with `inflight`, `ready` and the granted
  radius).

### Movement corrections (latest)

The chain that got here, because each step looked right and was still visible:

1. the input tick started at 1 instead of `StartGame::current_tick`, and `Pos Delta` was the
   friction-scaled velocity a Java→Bedrock translator uses — so the server disagreed with
   the client's movement constantly (fixed in `3a67c6c`; a walk and a sprint-jump then draw
   zero corrections);
2. the correction's tick was checked against the *local* tick counter, so once the input
   tick became the server's clock every correction was dropped and a player could stick
   against water and block edges (`9161cde`);
3. applying the correction's absolute position rewound the player to a tick they had left,
   and applying the rebased offset in one go moved them half a block in a frame — both are
   visible (`4c9eeab`, `612b1c5`).

**What it does now** (`Make a movement resync invisible`): a correction never touches the
client. Its offset (server position at the corrected tick minus what we reported there) is
accumulated into `Bridge::correction` and added to the position each `PlayerAuthInput`
carries; the offset decays 2% a tick, so the server's model converges while the screen shows
the client's own movement. `Pos Delta` stays the client's real movement. Only a divergence
over three blocks moves the client, because that is a teleport (respawn, void fall).
`HZ_CHUNK_TRACE=1` prints `CORR offset=[…] big=… debt=[…]`.

The cost to be aware of: while the offset is non-zero, the server's idea of the player's
position differs from the screen by that amount (bounded at 4 blocks and normally a few
tenths), which is what "resync invisible" means. Reach checks and other players' view use
the server's position, so a long drift in water would be visible *to others* as a small
offset; if that ever matters, the offset should be capped tighter rather than applied to the
camera.

### Movement: measured, and what is actually left

Thirty seconds of logging (client position, the position we reported, the carried correction
and the correction count, sprint-jumping through water) settled this:

- corrections arrive **repeatedly and large**: offsets of 1.0–2.2 blocks, several a second,
  while a plain walk and a sprint-jump on dry ground draw **none**;
- the server keeps its own simulation position: with corrections ignored, `/tp @s ~ ~ ~`
  afterwards moved the client from **-201.27 to -198.96**, i.e. the server was 2.3 blocks
  away from where the client stood. Ignoring a correction does not remove the drift, it
  only postpones the moment it is shown;
- applying it whole is a teleport; applying the rebased offset at 0.1 blocks a tick is a
  visible float; at 0.02 blocks a tick the backlog grows to its 4-block clamp because
  corrections arrive faster than they are paid off.

So the drift is **not** bookkeeping — it is Java's movement physics disagreeing with
Bedrock's, mostly in water and on sprint-jumps. No amount of correction handling can hide
that without either moving the camera or letting the two simulations diverge.

**The fix is the one the player suggested: adapt the movement to Bedrock's model.** Concretely:

1. Port Bedrock's player-movement integrator (the rules are in `bedrock-protocol-docs`
   `PlayerMovementOverview.md`, and in Pumpkin's `living_entity`/`player` movement code):
   ground friction 0.6 and the 0.91 factors, the air factor, the sprint-jump boost, water
   drag and buoyancy, ladder speed, and the `move_vec` → world-space conversion with yaw.
2. Feed it the same keys the game uses (`PlayerInput` flags we already receive) plus the
   block/fluid state under the player. That state is on the network thread in `ctx.chunks`
   (block state and water are already available there), but the input tick runs on the game
   thread through `serverbound`, so a small cached column region around the player has to be
   shared between them.
3. Compare the integrator's next position with the client's, and use the difference as a
   small *velocity* correction each tick (a PlayerPosition by that frame's difference, not
   the accumulated error). Once the integrator is close, the server's corrections stop
   arriving at all — that is the only end state where nothing is ever visible.

Until that exists, the committed behaviour is `Make a movement resync invisible`: the
correction is folded into the position the bridge *reports* and marked down 2% a tick, so
the camera never moves; the server's model converges on its own simulation and the client's
screen is untouched. It is the least-visible of the options, not a cure.

## 14. Debts and risks

- The GitHub token used for pushes was pasted in plaintext and is still valid — **rotate
  it**.
- `HANDOVER.md` always gets its own commit, never mixed with code.
- The launcher's cached Horizon source is currently **pristine** (a temporary debug patch
  in `remote.rs` was reverted).
- MOTD is synthesised, not read from the server.
- Xbox sign-in surfaces the device code as a **status line**, not a UI.
- A compiler warning, `unused doc comment`, appears in the engine build and has not been
  identified; it predates `d55e6ff`.
- `MovePlayer` is still what moves us, so the server treats the player as stationary.
- The bridge keeps only columns inside the server-granted 32-chunk circular radius; on a
  reconnect, the retained columns are still from the old session until the new radius is
  granted, so re-verify the visible distance after a rejoin.
- The quick-move shift-click order is now "hotbar first, then the rest of the inventory",
  which deliberately differs from the game's own main-inventory-first guess for a hotbar
  item, so a shift-click out of the hotbar can still flash for one round-trip before the
  menu rewrite settles it.

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

## Parity after movement — current state

Seven commits on `main` after the movement work, each built and launched against
the local server with no panic:

| Commit | What it did |
|---|---|
| `a444059` | Hold-style usable items start with `StartUsingItem`, use the `Use` action, and end with an `ItemReleaseTransaction`. The release was missing its empty action list, which made the server drop the session: that is fixed. A block click that passes to the item is dropped and the item use goes out alone. Non-place items never carry an inventory action, so the held stack is not shrunk. |
| `54d2b27` | `MobEffect` is decoded and add/update/remove become the game's effect packets. Bedrock ids are mapped by name to the Java registry; the live server puts `raid_omen` at 36 and `infested` at 35, the reverse of one reference list. |
| `d4ffdae` | The player's unique id is read from the spawn's abilities data and mapped to the runtime id, so a player removal no longer leaves a ghost. A reused runtime id or unique id removes the old body first. Removals only go out for held entities; a once-a-second sweep removes anything left outside the published area. Other players' stored positions follow their move packets. |
| `658e774` | A pickup no longer sends a removal beside it, so the client plays the three-tick flight into the collector. |
| `d7a94a9` | A click or drag outside the inventory window drops the carried stack (all for the left button, one for the right, one for Q), which used to send nothing. A request unanswered for three seconds clears the pending slot names and asks the server for the inventories again instead of needing a rejoin. |
| `1588e9a` | The command origin uses the player's own uuid from the player list, and the player-list entry for the local player refreshes the actor unique id a respawn can change. |
| `b7465f9` | Titles, subtitles, action bars, title timings and clear/reset are decoded and shown. |

Verified live on the last build (`native-d7077bc-3201f84f216d`, against Horizon
`d7077bc27`):

- a bow fired from the air and from a block, consumed an arrow and was picked
  back up after landing; crossbow charged, showed the using flag and fired;
  shield in the main hand starts and ends its use (Bedrock shields belong in
  the off hand, so it does not block there);
- speed/poison/omen effects add, update and clear, including the newer omen
  ids; the local player's effect list matches the server's;
- `/give @s` resolves repeatedly, before and after a death/respawn;
- Q on a slot, carrying a stack and dropping it outside the window, and
  dragging outside all produce server-accepted stack requests;
- a killed cow plays its death then leaves, and a cow left behind after the
  published centre moved is swept away;
- title, subtitle, action bar and timing commands reach the game;
- block placement still consumes the held stack.

Known gaps after this pass:

- **Item use:** a real client sends a use transaction on simulation ticks while
  a hold-style use continues; the bridge sends only the first one. Fireworks in
  the main hand do not consume on the ground (the server only boosts while
  gliding). A PvP/other-player check of the held use animation is still needed.
- **Pickups:** the pickup particle is now sent alone; a held item that is
  picked up while the cursor is non-empty still depends on the game's own menu
  state.
- **Inventory:** the item stack request bytes are the shape the live server
  accepts (two type bytes and a fixed `li32` stack id), which differs from the
  1.26.60 schema files. Do not "fix" them to the schema without a capture. The
  three-second recovery has not been forced live; only its compile and normal
  operation are verified.
- **`@s`:** the uuid comes from the player-list entry with no name. If the
  server ever sends that entry only as a removal, the uuid is still the
  account's, but a new-session check is worthwhile.
- **Section 12:** most of it remains. The largest visible pieces still missing
  are gamerules, boss bars, scoreboards, block entities, per-chunk biomes,
  dimensions, attributes, passengers, crafting/creative, `Dynamic` registries,
  and the NetherNet reconnect/transfer work.

### Boss bars, explosions and actor ids (latest)

`b0b452c` adds the rest of the wither/boss path that a live check exposed:

- `BossEvent` is decoded (signed actor id, event, title, filtered title,
  health, colour, overlay). The first `Add` creates the game's bar, later
  `Add`s are health updates (the server repeats `Add` while a boss spawns),
  `Update_Name`, `Update_Percent` and both style events map directly, and
  `Remove` takes the bar away. A `RemoveEntity` for the boss actor also
  removes its bar, because the server did not send event 2 in this test.
- The title is a translation key (`entity.wither.name`). It is sent as the
  game's own key (`entity.minecraft.wither`), so the bar reads `Wither`
  rather than the key. Literal title text still goes as literal text.
- `LevelEvent` 2025 becomes the game's explosion packet with an explosion
  or explosion-emitter particle, an explosion sound and a radius from the
  event's data (wither 7, creeper-like 1). This is the wither death burst.
- Actor unique ids are signed on the wire. `AddEntity`, `AddItemEntity`,
  `AddPlayer`, `RemoveEntity` and `BossEvent` now all use the same signed
  id and the same key, which is what stopped wither skulls (mobs that leave
  without a death event) from remaining as ghosts.

Verified on `native-d7077bc-940c4b41631a`: `/give @s` still resolves, a
wither shows a bar, sends the 2025 explosion at death, its skulls disappear,
and no panic appears in either log. The bar's disappearance itself needs one
more visual pass: the server's `RemoveEntity` for the boss id is present and the
bridge emits the bar removal for it.

Remaining from section 12 is unchanged: gamerules, scoreboards, block entities,
per-chunk biomes, dimensions, attributes, passengers, crafting/creative,
`Dynamic` registries, and the NetherNet reconnect/transfer work.
