# 7. AnvilDB

*English · [Українська](../uk/docs/07-anvildb.md)*

AnvilDB is a separate database for Minecraft worlds. It stores chunks and global state, grants chunk leases and speaks protobuf. Both Sculk's workers and regular Java servers (through a plugin, chapter 12) can use it.

### Data model

The structure is like a relational database:

- a **database** → holds **worlds**;
- a **world** → holds **dimensions** (overworld, nether, end, custom ones), which work like tables;
- a **dimension** → holds regions and chunks, and the chunks' metadata.

At the world level lives the global state: time of day, weather, gamerules, world border, scoreboard, player data (inventory, position, statistics).

### Chunk leases

AnvilDB is the single source of truth about who owns a chunk. For every chunk it knows the MW, the worker and when the lease expires.

| Operation | What it does |
| --- | --- |
| `acquire` | Atomically leases a set of chunks. If even one is taken, returns its owner |
| `renew` | Extends leases while the worker is alive |
| `release` | Releases leases after the chunks are saved |
| `transfer` | Atomically moves leases from MW to MW on a merge, split or migration |
| `owner` | Returns a chunk's metadata and current owner |

Every lease carries a **generation number** (fencing token). A write with an old number is refused, so a worker that "hung" and lost its lease cannot corrupt the data. *(Proposal.)*

### Storage formats

- **Classic** — compatible with Anvil (`.mca`), so a world can be taken from a regular server and given back.
- **Optimized** — AnvilDB's own format for faster reads and writes and better compression.

A CLI tool imports and exports worlds between the formats. It fits as a subcommand of the same binary: `sculk anvildb import` / `sculk anvildb export`. *(Proposal.)*

### Storage and archive

- Files are kept on the instance's local disk or in an **S3-compatible bucket**.
- Regions nobody has visited for a long time go to the **archive**: they are compressed harder and moved to cold storage. When a player comes there, they come back on request.

---

[← 6. Worker](06-worker.md) · [Contents](../README.md) · [8. Protocols between services →](08-protokoly.md)
