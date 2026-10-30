# Third-party notices

`bedrock-bridge` is original work (MIT). Its protocol knowledge and mapping data are derived
from the following projects, which keep their own licences:

| Project | Used for | Licence |
|---|---|---|
| [TunnelMC](https://github.com/THEREALWWEFAN231/TunnelMC) | The Java→Bedrock auth flow and packet-translation strategy. | see upstream |
| [Geyser](https://github.com/GeyserMC/Geyser) / [Floodgate](https://github.com/GeyserMC/Floodgate) | The Bedrock stack, entity/addon handling, and chunk approach. | MIT |
| [GeyserMC mappings](https://github.com/GeyserMC/mappings) (`references/mappings-master`) | Java 26.3 ↔ Bedrock 1.26.60 block/biome/item tables. | MIT |
| [CloudburstMC Network](https://github.com/CloudburstMC/Network) | RakNet/NetherNet transport reference. | Apache-2.0 |
| [CloudburstMC NBT](https://github.com/CloudburstMC/NBT) | Network NBT reference. | Apache-2.0 |
| [Cloudburst BlockStateUpdater](https://github.com/CloudburstMC/BlockStateUpdater) | Bedrock block-state versioning reference. | LGPL-3.0 |
| [Cloudburst](https://github.com/CloudburstMC/Cloudburst) server | Bedrock protocol 2225 reference. | GPL-3.0 |
| [Nukkit](https://github.com/CloudburstMC/Nukkit) | Bedrock packet layouts for protocol 2225 (1.26.60). | GPL-3.0 |
| [horizon-mixins](https://github.com/horizon-rust/horizon-mixins) | The native-mod injection tool the bundle is built with. | MIT |

No Mojang code, assets or data are included. `libminecraftpe.so` and the APK were only read
locally for analysis; nothing from them is redistributed.
