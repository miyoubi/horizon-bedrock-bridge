# Prompt for the next session — parity after movement

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
