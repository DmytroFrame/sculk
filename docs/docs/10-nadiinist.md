# 10. Reliability and failures

*English · [Українська](../uk/docs/10-nadiinist.md)*

A crash of any worker costs players at most 5 seconds of progress and a short "freeze". A crash of IO disconnects every player in v1.

### Saving state

A worker keeps its MWs' chunks in memory. The lease guarantees nobody else changes them, so every change need not be written synchronously. Changed chunks and player data are flushed to AnvilDB asynchronously:

- every ~5 seconds;
- before a lease is released;
- on merges, migrations and a regular shutdown.

### Failure scenarios

| What happened | What the system does | What is lost |
| --- | --- | --- |
| A worker crashed | IO notices it by heartbeat, releases the leases, moves the players to other workers, MWs are restored from AnvilDB | Up to 5 s of progress |
| A worker hung but is alive | Leases are not renewed and expire by TTL. Late writes are refused by the generation number | Up to 5 s of progress |
| AnvilDB is unreachable | MWs keep ticking on the chunks in memory, writes pile up in a queue. No new leases are granted, so players cannot leave their zones *(proposal)* | Nothing, if AnvilDB comes back before the queue overflows |
| IO crashed | Every client is disconnected, workers save their state | The players' sessions; progress is saved |
| Planned worker restart | Drain: MWs migrate to other workers | Nothing |

---

[← 9. Global state and identifiers](09-hlobalnyi-stan.md) · [Contents](../README.md) · [11. Nether Tunnel →](11-nether-tunnel.md)
