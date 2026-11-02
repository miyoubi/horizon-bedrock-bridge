# bedrock-bridge — a Horizon native mod for joining Bedrock servers

A [horizon-mixins](https://github.com/horizon-rust/horizon-mixins) native mod that lets
**Horizon** (Minecraft: Java Edition client code, rewritten in Rust) join **Minecraft: Bedrock
Edition** servers: move and be seen moving, break and place blocks, fight mobs, pick items up and
see them in the inventory, read chat and the player list, and sign in with a real Xbox Live
account.

Horizon speaks the Java protocol to servers. Bedrock servers speak a different protocol over
**RakNet** or **NetherNet**. This mod implements the Bedrock half and translates it into the
Java packets Horizon already understands, so the game itself is unchanged.

```
Bedrock server ──RakNet (UDP)──┐
               └─NetherNet─────┤  bedrock-core (transport, MCPE codecs, Xbox auth, crypto, mappings)
                   (WebRTC)    │
                               ▼
                       bedrock-bridge glue (injected into horizon-client)
                               │  Java clientbound packets
                               ▼
                       Horizon's normal multiplayer client
```

## Transports

Both transports a Bedrock server can serve are implemented, and the address prefix picks one:

| Address | Transport | Notes |
|---|---|---|
| `bedrock:host:19132` | **NetherNet** (WebRTC data channel) | What current Bedrock clients use. BDS serves it on its TCP port, alongside RakNet on UDP. |
| `bedrock+raknet:host:19132` | **RakNet** (UDP) | The classic transport, still served for older clients and `transport=raknet` servers. |

A bare address stays Java, so the mod never hijacks a normal join.

## What is here

| Path | What it is |
|---|---|
| `mixins/bedrock_bridge/engine/` | `bedrock-core`: the whole Bedrock stack, a standalone crate with no Horizon dependency (unit-tested with `cargo test`). |
| `mixins/bedrock_bridge/horizon-mixin.toml` | The bundle: the `[[crate]]`, the injected session module, and the `Session` hooks. |
| `mixins/bedrock_bridge/session.rs` | The injected module: session lifecycle, the client thread, and Bedrock→Java translation. |
| `mixins/bedrock_bridge/hooks/` | Thin `horizon-mixins` helpers for `Session::connect` / `connect_transferred` / `send` / `disconnect`. |
| `compatibility.json` | Pinned Horizon and horizon-mixins revisions. |

### `bedrock-core` (the engine)

Everything below the Horizon glue, in one testable crate:

* `stream` — little-endian primitives, LEB128 varints, zig-zag signed varints (Bedrock's base wire encoding).
* `nbt` — **network NBT** (little-endian, varint/zig-zag int and long, Bedrock flavour).
* `bitarray` / `subchunk` / `chunk` — the paletted sub-chunk format, `(x<<8)|(z<<4)|y` ordering, runtime and persistent (NBT) palettes, and the column assembly the Java chunk packet needs.
* `raknet` — the reliability layer: offline handshake, datagrams, encapsulated packets, reliability/ordering, ACK/NACK, fragment reassembly.
* `nethernet` — the WebRTC transport: signaling, the data-channel message codec with segmentation and reassembly, and the `Transport` adapter.
* `protocol` — packet ids for protocol **2225** (1.26.60), typed codecs for the packets the bridge needs, and batch framing.
* `crypto` — ES384 identity key + JWT signing, X25519 handshake key, AES-256-GCM stream cipher, `ServerToClientHandshake` parsing (`ring`).
* `auth` — Xbox Live device-code sign-in, the user/device/title/XSTS chain with the ES256 proof-of-key signature header, the Minecraft JWT chain, and the client-data (skin) JWT.
* `mapping` — Bedrock↔Java block-range and biome tables (loaded from the GeyserMC mappings), the **addon entity registry** (custom entities from `AvailableEntityIdentifiers`), and the skin helpers.
* `client` — ties it together: connect, zlib batch compression, the login handshake state machine, login/encryption/resource-pack handling, and chunk requests.

### Translation coverage

Clientbound (Bedrock → Java):

* **World** — `StartGame`, time/difficulty/health, level events and sounds, spawn position, chunk publisher, `LevelChunk` + `SubChunk` + block updates.
* **Entities** — `AddEntity` / `AddPlayer` / `AddItemEntity`, `SetEntityData`, equipment, `MovePlayer` / `MoveActorDelta` / `MoveEntityAbsolute` / `SetEntityMotion`, `Animate`, `RemoveEntity`, and item pickup.
* **Player** — the server's position corrections, chat, the player list (tab), and the disconnect reason.

Serverbound (Java → Bedrock):

* **Movement** — one `PlayerAuthInput` per tick carrying the position, rotation, input flags (including the collision and jump/sneak/sprint variants a real client sends) and the velocity delta.
* **Interaction** — block breaking through the input packet's block actions, attacking as an item-use-on-entity inventory transaction, placing and using as an item-use transaction, dropping as an item-release transaction, and the arm swing as `Animate`.
* **Inventory** — the selected hotbar slot as equipment, and item-use transactions carrying the real held stack.

### Xbox authentication

`bedrock-core::auth` runs the full flow, not a pasted token:

1. **Device code** — the mod prints a code and URL and polls until the player signs in; the refresh token is cached under the mod's data directory.
2. **Xbox Live** — user, device and title tokens (each with an ES256 proof-of-key and the `x-xbl-contract-version` signature header) and an XSTS token for `https://multiplayer.minecraft.net/`.
3. **Minecraft chain** — `multiplayer.minecraft.net/authentication` returns the inherited JWTs; the client prepends its own ES384 self-signed JWT whose `identityPublicKey` points at the inherited root.
4. **Skin JWT** — carries the client's X25519 handshake key in `x5u`, signed with the identity key.

### Custom entities

`AvailableEntityIdentifiers` lists every entity identifier, vanilla and addon. `Mappings::parse_entity_identifiers` reads it; `Mappings::java_entity` maps a Bedrock identifier to a Java one and passes an addon identifier (`mymod:robot`) through with its namespace, so the bridge can spawn it and a content mapping can give it a model.

## Install

Native mods are compiled into the game, not dropped beside it. Build the file with
`python3 tools/package.py` (it writes `target/bedrock_bridge-0.1.0.hnmod`), then either:

* **Launcher:** put the `.hnmod` (or the `mixins/bedrock_bridge/` folder) in the instance's
  `horizon-native-mods/`, open **Mods**, turn it on and press **Build**. Accept the native-mods
  warning when asked (Settings ▸ Game ▸ Native mods). The launcher fetches the pinned commit's
  source and its own Rust, then builds a private instance.
* **CLI:** `horizon-launcher native-build target/bedrock_bridge-0.1.0.hnmod --trust-native-code`.

Both compile on the Nightly pin in `compatibility.json`; every mod a build combines must pin the
same revision and the same horizon-mixins revision.

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
python3 tools/horizon.py test  --horizon ../horizon   # embedded engine tests in the game
```

Then launch the built client and join a server with a Bedrock address:

```
Multiplayer ▸ Direct Connection ▸ bedrock:play.example.net:19132
Multiplayer ▸ Direct Connection ▸ bedrock+raknet:play.example.net:19132
```

## Status

Implemented: the wire codecs, both transports, the login chain and Xbox auth, the mappings, the
session lifecycle and packet dispatch, and the translation listed above — movement in both
directions, chunks and blocks, entities, inventory, and the interaction packets.

Known gaps:

1. **Containers beyond the player's own.** Window-type to Java menu mapping (chests, crafting)
   and container clicks that move items between slots.
2. **Damage and death feedback.** The server's actor events (hurt, death) and the knockback it
   applies to the player, plus weather and the day cycle.
3. **The configuration phase.** The bridge emits `Event::Joined` with `Dynamic::default()`, which
   is enough for dimension defaults but not for biomes, damage types or item components; a
   synthetic configuration phase sending the vanilla registries is what full parity needs.
4. **Reconnection.** A quick rejoin after leaving is retried, because the server holds the
   previous session for a while after the bridge drops it.

## References

Standing on the work of others. The Bedrock side was implemented from these sources:

* **[bedrock-protocol-docs](https://github.com/Mojang/bedrock-protocol-docs)** — Mojang's own packet documentation; the authority for 1.26 packet layouts and enum values.
* **[ViaBedrock](https://github.com/RaphiMC/ViaBedrock)** — a Java↔Bedrock proxy. The reference for translating a *Java client* into Bedrock, which is this bridge's direction: its `PlayerAuthInput` builder, its movement-correction handling and its item-use transactions are what the corresponding code here follows.
* **[Pumpkin](https://github.com/Pumpkin-MC/Pumpkin)** — a Bedrock server; the reference for what a server expects from the client (input gating, position basis).
* **[Geyser](https://github.com/GeyserMC/Geyser)** and **[Floodgate](https://github.com/GeyserMC/Floodgate)** — the Bedrock stack and mappings used across the ecosystem.
* **[GeyserMC mappings](https://github.com/GeyserMC/mappings)** — the Java↔Bedrock block, item and biome tables this mod loads.
* **[CloudburstMC Protocol](https://github.com/CloudburstMC/Protocol)**, **[NBT](https://github.com/CloudburstMC/NBT)** and **[BlockStateUpdater](https://github.com/CloudburstMC/BlockStateUpdater)** — Bedrock packet layouts and NBT.
* **[bedrock-protocol](https://github.com/PrismarineJS/bedrock-protocol)** and **[minecraft-data](https://github.com/PrismarineJS/minecraft-data)** — the JavaScript client and its protocol data, used to cross-check field order and enum values.
* **[TunnelMC](https://github.com/THEREALWWEFAN231/TunnelMC)** — a Bedrock↔Java bridge, referenced for the auth and translation structure.
* **[Horizon](https://github.com/horizon-rust/horizon)** and **[horizon-mixins](https://github.com/horizon-rust/horizon-mixins)** — the game this mod is built into, and the injection it uses.

Older packet layouts were also checked against the CloudburstMC server, Nukkit and the Minecraft
Bedrock Wiki. See `THIRD_PARTY_NOTICES.md`.

## Licence

MIT for the project's own work. Minecraft is a trademark of Mojang; this repository contains no
Mojang code, assets or data.
