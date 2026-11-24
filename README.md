<p align="center">
  <img src="https://minecraft.wiki/images/Calibrated_Sculk_Sensor_JE2.gif" width="160" alt="A calibrated sculk sensor, Sculk's logo">
</p>

<h1 align="center">Sculk</h1>

<p align="center"><b>English</b> · <a href="README.uk.md">Українська</a></p>

A distributed Minecraft: Java Edition 26.3 server in Rust. The goal: thousands of players in one seamless world, with
a game no player can tell from a vanilla server.

The world is divided into **micro worlds (MW)**: each has its own tick and its own chunks; micro worlds grow with their
players, merge when players meet and split when they go apart. They run in parallel on many workers, and the world is
kept in a separate database, **AnvilDB**. The concept and architecture are in [docs/](docs/README.md), the state of
the work in [PLAN.md](PLAN.md).

> Status: early, but playable. The vanilla game comes from the [Horizon](https://github.com/horizon-rust/horizon)
> engine (Minecraft Java 26.3 rewritten in Rust); Sculk adds the cluster, AnvilDB, chunk leases, micro worlds and
> moving players between engines without dropping their connection. What is verified and what is not is in
> [PLAN.md](PLAN.md).

## What it does

- Login from the vanilla 26.3 client, online mode (encryption, Mojang's session server, skins)
- The vanilla game on the Horizon engine: world generation, mobs, redstone, the Nether, the End, trading, PvP
- Micro worlds: players spread over engines, merged when they meet, split when apart, moved without disconnecting
- Shared across the cluster: chat, tab list, `/list`, `/tp`, time, weather, operators, whitelist, bans
- AnvilDB: a vanilla world folder (copies in and out as it is), leases with fencing, LZ4, an archive of cold regions
  (on disk or in S3)
- Reliability: a worker dies without disconnecting its players, graceful restarts (drain), keeps running while AnvilDB
  is unreachable
- A web console with a map of the micro worlds, Nether Tunnel (a transport over UDP), a chaos test

## Roles

One binary, `sculk`, several roles:

| Role | What it does |
| --- | --- |
| `sculk io` | The players' entry point: login, online mode, routing and moving players between engines |
| `sculk worker` | Runs the game engine (`sculk-engine`, Horizon with Sculk's hooks) for its micro worlds |
| `sculk anvildb` | The world database: chunks, player data, global state, leases |
| `sculk dev` | Everything in one process, for development and small servers |
| `sculk prepare` | Game data for 26.3 from `server.jar` |
| `sculk tunnel` | Nether Tunnel, the player's side |

## Quick start: Docker Compose with 4 workers

```sh
git clone --recursive <this repository> sculk && cd sculk
tools/horizon.sh setup          # Horizon (the submodule) with Sculk's patches applied
cp .env.example .env
```

In `.env` set `SCULK_EULA=true` (you accept the [Minecraft EULA](https://aka.ms/MinecraftEULA)),
`HORIZON_RESOURCES="<path to Horizon's prepared 26.3 resources>"` and `SCULK_WORKERS=4`. Each engine may take up to
`SCULK_ENGINE_MEMORY` (6g); lower it for a smaller machine. Then:

```sh
tools/docker.sh up              # builds the image, starts AnvilDB, IO and 4 workers
tools/docker.sh ps              # all healthy?
tools/docker.sh logs worker     # the engines' logs
```

Connect to `<host>:25565`; the web console is at `http://127.0.0.1:7180`. `tools/docker.sh up --workers 4` does the
same without editing `.env`; `tools/docker.sh down` stops the cluster (the world stays in the `anvildb-data` volume).

For Kubernetes (autoscaling workers included) see [deploy/kubernetes](deploy/kubernetes/README.md).

## Running it

You need:

- Rust (stable) and `protoc`;
- the Horizon engine beside the code: the `external/horizon` git submodule (`git clone --recursive`, or
  `git submodule update --init`) at upstream's commit in `patches/horizon/BASE`, and `tools/horizon.sh setup` to apply
  Sculk's patches on a `sculk` branch;
- Minecraft 26.3 resources prepared for Horizon (`horizon-prepare --prepare --server-jar ...`, or the Horizon
  launcher's `.local/horizon-client/26.3`). You prepare them from your own copy of the game: Sculk distributes nothing
  of Mojang's;
- your agreement to the [Minecraft EULA](https://aka.ms/MinecraftEULA) (`SCULK_EULA=true`).

### Locally

```sh
cargo build --release -p sculk -p sculk-engine
target/release/sculk prepare                 # once: the 26.3 game data
SCULK_ENGINE=target/release/sculk-engine \
SCULK_RESOURCES=<Horizon resources>/26.3 \
SCULK_EULA=true \
SCULK_PROPERTIES=config/server.properties \
target/release/sculk dev --workers 2
```

Connect to `localhost:25565`. The server console is the same terminal (`op <name>`, `whitelist ...`, `stop`).

### Docker

```sh
cp .env.example .env    # SCULK_EULA=true and HORIZON_RESOURCES="<path to 26.3>"
tools/docker.sh up      # builds the image and starts AnvilDB, IO and the workers
```

The game's settings (`server.properties`, `server-icon.png`, `ops.json`, datapacks, Horizon mods) go in `config/`.
The world lives in the `anvildb-data` volume. The web console: `http://127.0.0.1:7180`.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
tools/chaos.sh --workers 3 --bots 12      # a chaos test under Horizon's bots
```

Changes to Horizon are commits on the `sculk` branch of `external/horizon`, exported to `patches/horizon/`
(`tools/horizon.sh export`).

## Authors

- Dmytro Kostyshyn — author, concept and architecture
- Claude (Anthropic) — co-author of the code (Claude Code)

The full list is in [AUTHORS](AUTHORS). Sculk is built on [Horizon](https://github.com/horizon-rust/horizon) (MIT).
The logo is the calibrated sculk sensor's animation from the [Minecraft Wiki](https://minecraft.wiki/w/Calibrated_Sculk_Sensor);
Minecraft's textures belong to Mojang.

## License

[MIT](LICENSE). Minecraft is a trademark of Mojang AB; Sculk is not affiliated with Mojang or Microsoft.
