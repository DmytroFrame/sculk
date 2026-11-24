# Sculk implementation plan

*English · [Українська](PLAN.uk.md)*

The working task list for the concept in [docs/](docs/README.md). `[x]` means done and verified, `[~]` in progress or done in part (details in the note), `[ ]` not started. The blocks go in the order they are best done in.

How to check: `cargo test --workspace`, `cargo clippy --workspace --all-targets`, and an end-to-end run of `cargo run -- dev` with a Minecraft 26.3 client or Horizon's bots:

```sh
cargo run -- prepare                        # once: the 26.3 data from server.jar into game-data/
cargo run -- dev --workers 2
(cd external/horizon && cargo build --release -p horizon-bots)
external/horizon/target/release/horizon-bots --server 127.0.0.1:25565 --bots 5 --behaviour mixed --report -
```

The vanilla game (the Horizon engine) instead of the flat world: the same roles, plus the engine and Horizon's prepared resources (`horizon-prepare --prepare --server-jar ...`, or the Horizon launcher's `.local/horizon-client/26.3`):

```sh
cargo build --profile fast -p sculk -p sculk-engine
SCULK_ENGINE=target/fast/sculk-engine SCULK_RESOURCES=<resources>/26.3 SCULK_EULA=true \
  SCULK_PROPERTIES=config/server.properties target/fast/sculk dev --workers 2
# console: op <player>, whitelist ..., say ..., stop (saves the world and the players)
# bots fly: give them gamemode=creative, allow-flight=true in the cluster's server.properties
```

In Docker: `tools/docker.sh up` (in `.env`: `HORIZON_RESOURCES`, `SCULK_EULA=true`; `config/` holds the shared `server.properties` and `server-icon.png`).

## The base: Horizon

Sculk is built on [Horizon](https://github.com/horizon-rust/horizon)'s code (MIT) rather than rewriting vanilla itself: that way it moves to new game versions together with Horizon.

- `external/horizon` is upstream at the commit in `patches/horizon/BASE` plus our patches from `patches/horizon/*.patch`, on the `sculk` branch. To reproduce: `tools/horizon.sh setup`.
- What Horizon lacks is added by a patch to Horizon itself (a commit on the `sculk` branch, then `tools/horizon.sh export`), not by copying code into Sculk.
- Updating Horizon: rebase the `sculk` branch on the new upstream, a new `BASE`, `tools/horizon.sh export`.

What already comes from Horizon: the whole game engine (`sculk-engine`), `horizon-protocol` (26.3 packets, framing, compression, Velocity forwarding), `horizon-world` (NBT, region files), `horizon-bots` (the test client). Patches: `patches/horizon/0001`–`0007` (block B). Note: `external/rustfmt.toml` keeps Sculk's `rustfmt.toml` from reformatting Horizon.

## 0. Skeleton

- [x] Cargo workspace: `sculk-proto`, `sculk-data`, `sculk-io`, `sculk-worker`, `sculk-anvildb` and the `sculk` binary
- [x] Roles `sculk io | worker | anvildb` and `sculk dev` (everything in one process)
- [x] gRPC schemas: leases (`anvildb.proto`), the IO↔Worker channel (`cluster.proto`)
- [x] Server list ping (status + pong)
- [x] Login start in offline mode, the offline UUID as in Java
- [x] The own `sculk-mc` codec replaced by `horizon-protocol`
- [x] Worker registry: 16-bit IDs, heartbeat, watchdog, the least loaded worker
- [x] Entity IDs in blocks of 4096
- [x] Lease table: acquire / renew / release / transfer / owner / release_worker, fencing token
- [x] MW: born from the zone's lease, joining an MW on the same worker, lease renewal, 20 TPS tick
- [x] A worker's death: IO releases its leases, the worker reconnects by itself
- [x] A new worker with a reissued ID does not inherit old leases

## Deployment

- [x] Production configuration (24.11.2026): `compose.yaml` runs the vanilla game by default — workers with the Horizon engine (`SCULK_WORKERS`, 2), the flat test world only as the `flat` profile (`tools/docker.sh up --flat`). Online mode on, only the game port published (TCP, and UDP for Nether Tunnel) and the web console on 127.0.0.1; IO's control port, AnvilDB and the engines stay on the cluster's network. Memory limits, log rotation, 120 s for a worker to stop (players move, the world is saved). `.env.example` in sections (required: `SCULK_EULA`, `HORIZON_RESOURCES`), `tools/docker.sh up` checks both before building. Verified in Docker: 6 bots, 5429 chunks, 0 errors, micro world merges between containers. Found and fixed: an empty `SCULK_ARCHIVE_S3` from compose crashed AnvilDB, a MOTD with spaces broke `.env`
- [x] `Dockerfile` (multi-stage, cargo cache through BuildKit): one image for every role + `sculk-engine`; builds for Linux (233 MB)
- [x] `compose.yaml`: `prepare` (one-shot, `--if-missing`), `anvildb`, `io`, `worker` (scales), healthchecks, volumes for the world and the game data; settings in `.env` (`.env.example`)
- [x] `tools/docker.sh up|build|down|logs|ps`: builds the image tagged `<commit>-<time>` and `latest`, starts the cluster on exactly the tag it just built
- [x] The engine in a container: `SCULK_ENGINE_BIND=0.0.0.0`, IO uses the worker's IP; the EULA is `SCULK_EULA=true` from the owner
- [x] Verified in Docker (OrbStack): `prepare` downloaded server.jar from Mojang (SHA-1 matched), 6 bots through port 25565, 3232 chunks, 0 errors, join p50 32 ms
- [x] Engines in Docker: 2 engine workers, 4 bots, 2352 chunks, 0 errors; resources prepared on macOS work in a Linux container. `docker stop` lets the engine save the world
- [x] `config/`: the cluster's shared `server.properties` (SCULK_PROPERTIES; every engine takes its keys) and `server-icon.png`

## A. A player in the world

- [x] `sculk prepare`: 26.3 registries and tags from `server.jar` (the jar is read as a zip, nothing in it runs); the download from Mojang with SHA-1 check verified in Docker; `--if-missing` for containers
- [x] The data generator's report: `sculk prepare --java <java 25>` runs Mojang's data generator (`--reports`) in a temporary folder and takes the real block IDs and full tags from it (or `--reports DIR` with a ready report). Verified: the output matches a manual run byte for byte; the flat world with it — 5 bots, 0 errors, no warning about the report
- [x] Game data loaded at start: synchronized registries (entry names in order), their tags
- [x] Login Success → the configuration state
- [x] Configuration: brand, feature flags, Select Known Packs (`minecraft:core` 26.3), Registry Data without NBT, Update Tags, Finish Configuration. A client without the `minecraft:core` pack is refused with an explanation
- [x] Tags of the built-in registries (block, item, entity_type, fluid, game_event, potion, point_of_interest_type): without them the real client crashed on `dimension_type`, `enchantment`, `sulfur_cube_archetype`, which refer to `#minecraft:infiniburn_overworld`, `#minecraft:enchantable/*`. Without the generator's report the tags go without members; with it (`sculk prepare --reports`), complete. Horizon's bots do not check registries, so only the real client found this
- [x] The player is placed on a worker while still in configuration, so a refusal shows in the client
- [x] IO sends the player to the worker whose MW owns the spawn zone (an `owner` query to AnvilDB); if the owner changed during the login, the worker names it and IO redirects the player once
- [x] The player's packet channel IO↔Worker in the `Register` stream, multiplexed by `session_id`
- [x] Play: Login (play), abilities (creative), spawn point, position, the "waiting for chunks" Game Event, chunk centre
- [x] Chunks: sections, palettes, heightmaps and light through `horizon_protocol::chunk`; a flat world (stone, 2 dirt, grass at y=-61)
- [x] Chunks are sent nearest first (IWM's `get_chunk_tracer` idea), in ChunkBatches; far ones are forgotten
- [x] Keep-alive every 15 s, player movement, new chunks on crossing a chunk border
- [x] Compression: IO turns it on at login, the worker compresses play packets, IO only adds the length (docs/05). 9.3 KB → 135 B per flat chunk
- [x] Verified with Horizon's bots: 5 bots (walk/explore/idle), 3000 chunks, 0 errors, 0 position rollbacks
- [x] Players see each other, chat, tab list, commands, mobs, redstone, survival: all from the Horizon engine; across engines see block G
- [x] A new entity ID block from IO when the worker's block runs out (`AllocateEntityIds`; for the flat world — the engine hands out entity IDs itself)
- [x] Online mode (`SCULK_ONLINE_MODE=true`, `crates/sculk-io/src/auth.rs`): the RSA request, AES/CFB8 through `horizon_protocol::crypto::Cfb8` on the whole connection (and in the raw proxy to the engine), Mojang session server's `hasJoined`; the real UUID and skin (the `textures` property) go to the engine through Velocity forwarding. Test: real RSA, encryption both ways and a fake session server. Not yet verified with a real client and a licensed account
- [x] As in vanilla: logging in again closes the old connection; the player list (up to 12) on hover in the server list; `server-icon.png` (64×64); the default MOTD "A Minecraft Server"

## B. Vanilla logic in MWs (Horizon)

What was found out (10.11.2026):
- Horizon's server is `Game<'_>` from `horizon-client` (`game/dedicated.rs`, `lan/hub.rs`): the same object as the client with its renderer, plus a few global `static`s (`dedicated::SETUP`, `lan/handoff`). Thousands of them per process are not an option.
- Horizon itself plans `tick = "regions"` (docs/server-next.md §4.2): groups of chunks more than 2 chunks apart, each with its own random, entity ID range and scheduled tick queue, merging and splitting as players move. That is Sculk's MW within one process. The code does not have it yet.
- Moving the server half of `Game` into a library is also planned by Horizon (docs/layers.md, step P), with no rush.

The chosen path (10.11.2026): **a worker = a Horizon engine** (`sculk-engine`: Horizon's dedicated server with Sculk's hooks added, as `horizon-official` adds its own). All vanilla gameplay comes from Horizon; Sculk provides the world in AnvilDB under leases, player routing and the cluster. Horizon's regions (`tick = "regions"`) would later become MWs inside the engine.

- [x] Horizon patch 0001: the `horizon_hooks::storage::ChunkStore` hook — chunks (region/entities/poi) are read and written through the store of a crate above the engine instead of region files. Horizon's 86 saving tests pass
- [x] Horizon patch 0002: the Velocity forwarding codec moved into `horizon-protocol` together with the proxy's side (`sign`); the signature of a recorded Velocity 4.3.1 answer matches byte for byte
- [x] `sculk-engine`: Horizon's engine executable with Sculk's hooks; the chunk store is AnvilDB, a lease on every chunk read, lease renewal
- [x] IO proxies the player to the worker's engine: logs in as Velocity, signed forwarding, then frames both ways (a test with a fake engine checking the signature with Horizon's code)
- [x] Engine worker: prepares the engine's folder (forwarding.secret, server.properties: online-mode=false, port, shared seed), requires `eula=true` from the owner, starts and restarts the engine
- [x] First end-to-end run with the engine (24.11.2026, resources from the Horizon launcher): IO → engine, 6 bots (walk/explore/idle), 5810 chunks, 0 errors, chunks in AnvilDB. Found and fixed: `--resources` must come before `--dedicated` (otherwise the engine does not start)
- [x] Game mechanics through the Sculk cluster (24.11.2026, survival, console commands near a bot): redstone (a redstone block lights a lamp, a piston pushes a block, dust carries power 9 blocks), falling sand, wheat growing on random ticks, mob AI (a cow wanders), a zombie attacks and kills the player at night, water falls and spreads as in vanilla (on an open floor to all four sides, near an edge only toward the drop within 4 blocks), a nether portal lights in an obsidian frame, damage takes health, travel to the Nether (terrain generated) and the End (the dragon is there) and back — all three dimensions are in AnvilDB, villager trades, enchanting. PvP: 4 bots fighting through Sculk — 402 hits, health drops (after patch 0007)
- [x] Vanilla defaults where Horizon differs: `white-list=false` for a new engine; Horizon's chat filter and anticheat off (`server.toml`, the owner can turn them on); IO counts `max-players`
- [x] Console: lines from the worker's stdin go to the engine's console (`op`, `whitelist`, `ban`, `say`...); `stop` stops the worker, and in `sculk dev` the whole cluster, saving first
- [x] Graceful stop: Ctrl-C / SIGTERM / `stop` → `stop` to the engine's console (players and world saved), then the last files to AnvilDB; IO sees the worker alive the whole time (heartbeat), otherwise it would release its leases and the engine could not save its chunks (found by a test)
- [x] The engine stops by itself when its worker died (SIGKILL): IO has already released the leases, and an orphan must not take chunks in a dead worker's name
- [x] The engine starts lazily, with its first player or a console command: standby workers hold no leases (two engines started together fought over the chunks near spawn)
- [x] Horizon patch 0003: the `ChunkStore::unloaded` hook; the engine releases an unloaded chunk's lease after a grace period (now 10 s, or 2 s after its last write), reading it again cancels the release
- [x] A lease conflict on a chunk read (another engine's zone): the engine tells IO, and the micro worlds merge (block D) — or, if the owner is letting the chunk go, the engine waits for it; the engine never saves another MW's copy of a chunk
- [x] Global world state and player data in AnvilDB (without a Horizon patch): the `Files` service (level.dat, `players/{data,stats,advancements}`, `data/**`, dimension data, and `ops.json`/`whitelist.json`/bans under `server/`). The worker puts them in the engine's folder before start, sends changed ones back every second, and before a player arrives (IO's `PreparePlayer`) pulls its files. A player who just left another worker waits up to 3 s for that worker to save it
- [ ] Horizon patch: `tick = "regions"` — regions with their own random, entity IDs and tick queue (= MWs inside the engine)
- [x] Moving a player between engines (24.11.2026): Horizon's `[transfer]` (`--transfers`, the engine's control socket, through which the worker relays IO's commands). IO does the hand-over as in Horizon's docs/server.md: `player-expect` on the new engine → `player-export freeze` on the old → IO logs in to the new engine itself, sends the client `StartConfiguration` and switches the connection once acknowledged (`crates/sculk-io/src/proxy.rs`, as Velocity does on a server switch; the new engine gets the client's last `ClientInformation`) → `player-import` → `player-release moved`. A failed step is undone and the player plays on where it was. Tested with two fake engines and compression; live with Horizon's bots
- [x] Horizon patch 0004: Horizon's bots go through configuration again mid-game, as the vanilla client does (they took it for the end of the connection)
- [x] Horizon patch 0005: the store hook also hears of dropped proto chunks and, now and then, of every chunk the engine holds; without it the leases of chunks read by generation and abandoned hung forever (found by measuring: 1400 of 2025 leases were never released)
- [x] Horizon patch 0006 (found by the mechanics check): `/enchant` on a dedicated server refused every player ("not a valid entity"), and `/data get` on a villager nobody had traded with had no `Offers` (vanilla makes them on saving) — fixed, verified live through Sculk
- [x] Horizon patch 0007: a player's health in the entity data the other players get (vanilla sends 20, since the field's defined value is 1.0; Horizon left out exactly 20). Because of it bots (and anything reading other players' health) did not see players as alive
- [x] The engine does not save chunks it holds no lease on (a copy generated after a refused lease is not the world's), and does not crash on it

## C. AnvilDB as a database

- [x] Chunk storage: the `Chunks` service (read / write / saved_in_region), vanilla `.mca` through `horizon_world::region` in the 26.3 layout (`<world>/dimensions/<ns>/<path>/region`), so a world copies in and out as a folder
- [x] Writes allowed only to the worker holding a live lease on the chunk (fencing); world/dimension/folder names are checked, nothing can leave the data folder
- [x] Player data (position, inventory, game mode, statistics, advancements): vanilla `players/` files in AnvilDB's world folder
- [x] Global state: time of day, weather, gamerules, world border, scoreboard, raids, maps — vanilla `data/` files (the last write wins)
- [x] Leases survive an AnvilDB restart: the table is written to disk (`<data>/leases.json`, once a second when it changed), every lease keeps its remaining TTL, fencing generations are not reset; and an engine renewed fewer leases than it holds takes them again (verified: 2251 leases under load, 0 lost writes). An IO restart: workers reconnect, the engine saves and starts again
- [x] The worker's command queue (docs/06): one permanent connection from the engine to AnvilDB; the leases of simultaneous reads are gathered for 2 ms into one `Acquire` (a conflict in the batch: each chunk on its own); Horizon's writes go in batches per folder
- [x] Import/export: `anvildb-data/<world>/` is a whole vanilla 26.3 world folder (level.dat, players, data, dimensions/*/region|entities|poi): it copies in and out as it is. Older versions are upgraded by the engine when read (Horizon's datafix)
- [x] Archive of cold regions: region files with no writes for `SCULK_ARCHIVE_AFTER_DAYS` days (30) are gzipped by AnvilDB into `<data>/archive/` and quietly put back on the first read; `sculk unarchive` puts them all back (before copying the world out as a vanilla one). Test: archiving, reading from the archive, full restore
- [x] Optimized format: chunk record compression — `SCULK_REGION_COMPRESSION` (`lz4` by default, several times faster than zlib; `deflate`, `none`), all read by vanilla 26.3, and a region may mix them (test)
- [x] S3 storage for the archive: `SCULK_ARCHIVE_S3=<endpoint>/<bucket>` (+ `SCULK_S3_ACCESS_KEY`/`SCULK_S3_SECRET_KEY`, `SCULK_S3_REGION`) — cold regions go to the bucket (AWS, MinIO, R2...), a local index knows what is there, they come back on read. The SigV4 signature verified against AWS's reference example (and HMAC against RFC 4231), archiving and restoring through a fake bucket; no real AWS here

## D. MW lifecycle

- [x] The zone grows with its players: leasing new chunks and releasing unneeded ones (the zone = the union of squares around the MW's players)

In engine mode an MW = a worker's engine: its own ticks, its own leases. IO spreads players over the engines (`SCULK_PLACEMENT=spread`, the default; `single` — all on one) and brings them together when their zones meet.

- [x] Birth by the zone where the player appears: IO reads the player's saved position (`players/data/<uuid>.dat` from AnvilDB) and sends it to the engine owning that chunk; with no owner, to the engine with the fewest players (counting those joining)
- [x] Merging across workers (24.11.2026): an engine refused a lease tells IO (`ReportConflict`); if the owning engine has players near the chunk, one engine's players nearby move to the other without disconnecting (see block B). Verified live: 6 bots on 3 engines near spawn → one engine; A at 3000/3000 on engine 2, B near spawn on engine 1 → `tp B0` to A → A moves to engine 1; 0 errors, 0 crashes
- [x] If the owner has no players near the chunk (they left, the engine is about to release it), the engine waits up to 20 s for the chunk instead of generating its own copy
- [x] Moving a single player between MWs on teleport/portal/respawn: a teleport into another engine's zone is the same lease conflict → merge (verified with `/tp`)
- [x] Split: every 5 s IO groups an engine's players by distance; when the groups' zones stay ≥ 8 chunks apart for 5 minutes (`SCULK_SPLIT_GAP`, `SCULK_SPLIT_AFTER_SECS`), the group without the founder (the oldest session) moves to a free engine. Verified live (with a 10 s threshold): A0 went to 3000 → moved to engine 2, which waited for the real chunks
- [x] Simultaneous merge: the engine with more players absorbs, the lower worker ID among equals; one merge of a pair of engines at a time; a moved player stays put for 15 s (no ping-pong)
- [x] Merging on one worker: the same hand-over (through localhost) if a worker runs several engines
- [x] Migration for balance: an engine that can't keep up (`SCULK_SPLIT_OVERLOAD_MSPT`, 45 ms a tick) spreads its micro worlds over free engines at once, without waiting 5 minutes; a stopping worker hands all its players to other engines (block F)

## E. Worker scheduler

- [~] Thread pool: every MW is a separate engine process with its own tick and Horizon's thread pool sized to the cores; MWs on different engines tick in parallel, the OS shares the cores between them. Several MWs inside one process (and a scheduler over threads) do not exist — that is Horizon's `tick = "regions"` (block B)
- [x] MSPT per MW: the worker reads its engine's `server-status` every second (in its own task, never delaying the heartbeat — the chaos test found that regression), MSPT and players go to IO in the heartbeat and show in the web console

## F. Reliability

- [x] Recovery after a worker's death: IO releases the leases, players rejoining go to a standby worker, which starts an engine with the world and players from AnvilDB (verified: SIGKILL of the main worker, 2207 leases released, 3 bots joined the standby, 0 errors). Progress since the last autosave is lost
- [x] A player moves without disconnecting: until the client acknowledged going back to configuration its keep-alives go to the old engine; then the new engine handles them (the switch takes ~0.3 s, so IO need not answer them itself)
- [x] A worker's death without disconnecting its players: when the connection to the engine breaks and IO no longer has the worker (not a kick), IO moves the player to another engine, which loads it from AnvilDB. `tools/chaos.sh`: 3 kills of the busiest worker under 12 bots — 36 rescues, 0 disconnected, 0 engine crashes, 0 refused writes
- [x] Draining a worker before a restart: SIGTERM / `docker stop` → the worker tells IO (`Draining`), IO moves its players to other engines (and sends no new ones there), then the engine saves the world and stops. Verified: 4 bots moved, 0 errors. With no other engine, players are saved as on vanilla's stop
- [x] "AnvilDB unreachable" mode: chunk writes queue up (a newer record of a chunk replaces the older, reads see the queue), chunks with unsent records keep their leases; reads and new leases wait up to 30 s; IO lets no new players in. Verified: AnvilDB off for 15 s under bots — 0 errors, 0 crashes, the queue was written

## G. Global mechanics

- [x] Players on different engines (24.11.2026, `crates/sculk-io/src/cluster.rs`, `proxy.rs`): chat (`<Name> text`, as vanilla's `chat.type.text`), "joined/left the game", the tab list (IO adds other engines' players and removes those who left; a player who came to the client's engine is left to it), `/list` and `/list uuids` over the whole cluster. Verified by recording a bot's packets
- [x] Global clock: the engine with the most players keeps the time, the others are set to it when more than 5 s apart (verified: `time set 13000` on one engine → the other at 13,500 within 10 s)
- [x] `/tp <player>` to a player on another engine: IO first moves the player to the target's engine, then the command itself runs there (with the permissions it has there)
- [x] Sleeping and skipping the night (docs/13's open question): the night passes for everyone when the players of one micro world sleep (vanilla's percentage among them) — a forward jump of time on any engine spreads to all
- [x] The same weather on every engine: the reference engine sets rain and thunder (vanilla's `weather_check` predicate), the others follow when they differ. Verified live together with the clock (26.3 counts time with `time query time`; 26.3 has no `daytime` — found by checking)
- [x] `/op`, whitelist, bans shared by the cluster: the lists are in AnvilDB, every engine takes them at start

## H. Ecosystem

- [x] Debugging web console (`SCULK_CONSOLE_LISTEN`, 127.0.0.1:7180): a map of leased chunks coloured by worker, the players on it, a table of workers (players, MSPT, chunks); `/api/state` — the same as JSON. AnvilDB has `Leases.List` for it
- [ ] AnvilDB Connector for Paper
- [x] Nether Tunnel (`crates/sculk-tunnel`, docs/11): `sculk tunnel --server <IO>:<port>` listens on a local address for the Minecraft client and carries its TCP stream to IO over UDP (`SCULK_TUNNEL_LISTEN`), and IO handles the session as a regular connection. Its own reliable ordered transport: selective acknowledgement of the next 32 segments in every datagram, fast retransmission after three misses (not on a timeout), an RTO from the measured RTT with a 20 ms floor; every segment deflated when that makes it smaller. Tests: 300 KB + 120 KB both ways over a link with 20 % loss and reordering — whole and in order; live, 4 bots through the tunnel, 0 errors. FEC and compression with a shared dictionary (docs: "could be added") — no
- [x] Extension API (docs/13's open question "native Rust or WASM"): neither on its own — the extensions are the game's, the same on every engine. Vanilla datapacks (`config/datapacks/` → every engine's `world/datapacks/`) and Horizon's mods (content `.hmod`s in `config/horizon-mods/` with the `horizon-mods.txt` switches, native ones in `config/horizon-native-mods/`, Horizon's docs/mods.md) are handed to the engine by its worker before start. Verified live: a datapack's `#minecraft:load` function ran on the engine. Sameness matters, since players move between engines

## I. Scale

- [~] Load with bots: 40 bots on one engine — MSPT 12–14 ms (budget 50), 45 thousand chunks, 0 errors. The control plane (IO, AnvilDB, worker channels) under 300 bots on flat workers found and closed three faults: HTTP/2's flood protection dropped the worker's connection because every player's packet went as its own message (now `PlayerBatch` both ways, docs/06), a heartbeat late behind packets "killed" a live worker (now any message shows a worker alive), and an overloaded worker's unread incoming frames (now a task of their own reads them). The limit is one micro world on one core: all bots appear at one spot, so ~130 players in one MW of the flat world (50–100 in an engine, as in Horizon); this is docs/13's deliberate trade-off. 10,000 players spread over the world need a cluster of machines; one laptop can't verify it
- [x] Chaos tests: `tools/chaos.sh` (separate processes, bots, SIGKILL of the busiest worker every N s and a restart, then a fresh crowd of bots must get in without errors)
