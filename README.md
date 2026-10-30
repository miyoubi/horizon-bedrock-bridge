# bedrock-bridge — a Horizon native mod for joining Bedrock servers

A [horizon-mixins](https://github.com/horizon-rust/horizon-mixins) native mod that lets
**Horizon** (Minecraft: Java Edition 26.3, rewritten in Rust) join **Minecraft: Bedrock
Edition** servers, see and interact with entities — including custom entities from addons —
and sign in with a real Xbox Live account.

Horizon speaks Java protocol 777 to servers. Bedrock servers speak a different protocol over
RakNet (moving to NetherNet). This mod adds the Bedrock half and translates it into the
packets Horizon already understands, so the game itself is unchanged.

```
Bedrock server ──RakNet──▶ bedrock-core (RakNet, MCPE codecs, Xbox auth, crypto, mappings)
                                   │  decoded Bedrock packets
                                   ▼
                          bedrock-bridge glue (injected into horizon-client)
                                   │  Java clientbound packets
                                   ▼
                          Horizon's normal multiplayer client (handle_packet)
```

## What is here

| Path | What it is |
|---|---|
| `mixins/bedrock_bridge/engine/` | `bedrock-core`: the whole Bedrock stack, a standalone crate with no Horizon dependency (unit-tested with `cargo test`). |
| `mixins/bedrock_bridge/horizon-mixin.toml` | The bundle: the `[[crate]]`, the injected session module, and the four `Session` hooks. |
| `mixins/bedrock_bridge/session.rs` | The injected module `crate::net::bedrock`: session lifecycle, the client thread, and Bedrock→Java translation. |
| `mixins/bedrock_bridge/hooks/` | Thin `horizon-mixins` helpers for `Session::connect` / `connect_transferred` / `send` / `disconnect`. |
| `compatibility.json` | Pinned Horizon and horizon-mixins revisions. |

### `bedrock-core` (the engine)

Everything below the Horizon glue, in one testable crate:

* `stream` — little-endian primitives, LEB128 varints, zig-zag signed varints (Bedrock's base wire encoding).
* `nbt` — **network NBT** (little-endian, varint/zig-zag int and long, Bedrock flavour).
* `bitarray` / `subchunk` — the paletted sub-chunk format, `(x<<8)|(z<<4)|y` ordering, runtime and persistent (NBT) palettes.
* `raknet` — the reliability layer: offline handshake, datagrams, encapsulated packets, reliability/ordering, ACK/NACK, fragment reassembly.
* `protocol` — packet ids for protocol **2225** (1.26.60), typed codecs for the packets the bridge needs, batch framing.
* `crypto` — ES384 identity key + JWT signing, X25519 handshake key, AES-256-GCM stream cipher, `ServerToClientHandshake` parsing (`ring`).
* `auth` — Xbox Live device-code sign-in, the user/device/title/XSTS chain with the ES256 proof-of-key signature header, the Minecraft JWT chain, and the client-data (skin) JWT.
* `mapping` — Bedrock↔Java block-range and biome tables (loaded from the GeyserMC mappings), the **addon entity registry** (custom entities from `AvailableEntityIdentifiers`), and the skin helpers.
* `client` — ties it together: RakNet connect, zlib batch compression, the login handshake state machine, login/encryption/resource-pack handling.

### Xbox authentication

`bedrock-core::auth` runs the full flow, not a pasted token:

1. **Device code** — the mod prints a code and URL and polls until the player signs in; the refresh token is cached under the mod's data directory.
2. **Xbox Live** — user, device and title tokens (each with an ES256 proof-of-key and the `x-xbl-contract-version` signature header) and an XSTS token for `https://multiplayer.minecraft.net/`.
3. **Minecraft chain** — `multiplayer.minecraft.net/authentication` returns the three inherited JWTs; the client prepends its own ES384 self-signed JWT whose `identityPublicKey` points at the inherited root.
4. **Skin JWT** — carries the client's X25519 handshake key in `x5u`, signed with the identity key.

### Custom entities

`AvailableEntityIdentifiers` lists every entity identifier, vanilla and addon. `Mappings::parse_entity_identifiers` reads it; `Mappings::java_entity` maps a Bedrock identifier to a Java one and passes an addon identifier (`mymod:robot`) through with its namespace, so the bridge can spawn it and a content mapping can give it a model.

## What was learned from the real client (Ghidra)

`references`/`minecraft-26-60` is the Bedrock **1.26.60.4** Android APK, and its
`lib/arm64-v8a/libminecraftpe.so` (342 MB, stripped) was imported into Ghidra 12.2 headless
(`-noanalysis`, since a full pass over ~215 MB of `.text` is not practical here). Ghidra
recovered **80,129 functions** and **95,696 symbols** and mapped the sections; a script then
scanned memory for the transport and protocol names. It confirmed:

* **NetherNet is WebRTC**: `handheld/src-deps/NetherNet/src/...`, `JsonRpc::WebRtcComponent`,
  `HttpSignalingClient`, `Signaling/Http/HttpServer.cpp`, with mbedTLS **DTLS** and WebRTC
  **SCTP data channels** (`webrtc::SctpDataChannel`) and STUN/TURN ICE.
* **RakNet is still linked** (`src-deps/raknet/raknet/RakPeer.cpp`, `ReliabilityLayer.cpp`),
  and `NetworkSystemToggles`/`NetherNetTransportFactory` select between them.
* The **Xbox Live native SDK** (`Xbl*`, `Java_com_microsoft_xbox_idp_*`) and
  `handheld/src-client/common/client/identity/XboxLiveIdentity.cpp`.
* The packet class names (`StartGamePacket`, `PlayerAuthInputPacket`, `SubChunkPacket`,
  `InventoryTransactionPacket`, ...), which match the ids in `protocol/ids.rs`.

That analysis is why the transport is split behind RakNet (implemented) and NetherNet
(WebRTC — out of scope; see below) rather than assuming the old RakNet-only world.

## Install

Native mods are compiled into the game, not dropped beside it. Build the file with
`python3 tools/package.py` (it writes `target/bedrock_bridge-0.1.0.hnmod`), then either:

* **Launcher:** put the `.hnmod` (or the `mixins/bedrock_bridge/` folder) in the instance's
  `horizon-native-mods/`, open **Mods**, turn it on and press **Build**. Accept the native-mods
  warning when asked (Settings ▸ Game ▸ Native mods). The launcher fetches the pinned commit's
  source and its own Rust, then builds a private instance.
* **CLI:** `horizon-launcher native-build target/bedrock_bridge-0.1.0.hnmod --trust-native-code`.

Both compile on **Nightly 2026-10-30** (`ba6344a5`), the pin in `compatibility.json`; every mod a
build combines must pin the same revision and the same horizon-mixins revision.

## Build and test

The engine is a plain crate and needs no Horizon checkout:

```sh
cargo test -p bedrock-core            # the codec/transport/auth/mapping unit tests
```

Building the **modded game** is the horizon-mixins path (it compiles Horizon with the
injection in place):

```sh
python3 tools/horizon.py stage --horizon ../horizon   # export the pin, run plan/apply
python3 tools/horizon.py check --horizon ../horizon
python3 tools/horizon.py test  --horizon ../horizon    # embedded engine tests in the game
```

Then launch the built client with prepared resources and join a server with the
`bedrock:` address prefix:

```
Multiplayer ▸ Direct Connection ▸ bedrock:play.example.net:19132
```

The prefix is deliberate: bare addresses stay Java, so the mod never hijacks a normal join.
Non-prefixed addresses default to port 19132 only when the prefix is present.

## Status and what is left

**Verified against the pinned commit (Nightly 2026-10-30, `ba6344a5`):** `horizon-mixins plan` and
`apply` succeed (4 injections, 1 module, 1 crate all resolve), and the staged workspace
**compiles and links** — `cargo check`/`cargo build -p horizon-client --bin horizon-client
--no-default-features` finish with no warnings and produce the `horizon-client` binary. So the
injection, the engine crate and the whole dependency wiring are real, not just sketched.

Implemented and unit-tested: the wire codecs, RakNet, the login chain and Xbox auth, the
mappings, and the session lifecycle + packet dispatch.

The parts that still need work, in order of importance:

1. **Chunks and blocks.** `Clientbound::LevelChunkWithLight` needs a Bedrock-runtime-id →
   Java-block-state resolver. The GeyserMC `blocks.nbt` mapping is the source; wiring it to
   Horizon's block tables (a resolved Java state id per Bedrock runtime id) is the next step.
   `session.rs` currently drops chunk/block packets.
2. **The configuration phase.** Horizon's client normally receives its `Dynamic` registries
   during the Java configuration phase. The bridge emits `Event::Joined` with
   `Dynamic::default()`, which is enough for dimension defaults but not for biomes, damage
   types or item components; a synthetic configuration phase that sends the vanilla registries
   is needed for full parity.
3. **Inventories and interaction.** `InventoryContent`/`InventorySlot` → Java container
   packets, and `ContainerClick`/`UseItemOn` → `InventoryTransaction`/`ItemStackRequest`.
4. **NetherNet.** Servers that have moved off RakNet need the WebRTC/DTLS/SCTP transport and
   the JSON-RPC signalling client. That is a large, separate piece; RakNet still reaches
   servers on 19132.
5. **The handshake cipher.** The key/IV derivation is the documented modern scheme; the
   AES-GCM nonce layout is isolated in `crypto::Cipher` and should be confirmed against a
   live online-mode server.

## References

Ported/adapted from, and checked against: **TunnelMC** (Java→Bedrock, auth and translation),
**Geyser**/**Floodgate** (Bedrock stack and mappings), the **CloudburstMC** projects (Network,
NBT, BlockStateUpdater, Cloudburst server), **Nukkit** (Bedrock packet layouts, protocol 2225)
and the GeyserMC **mappings** (Java 26.3 ↔ Bedrock 1.26.60). See `THIRD_PARTY_NOTICES.md`.

## Licence

MIT for the project's own work. Minecraft is a trademark of Mojang; this repository contains no
Mojang code, assets or data.
