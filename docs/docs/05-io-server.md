# 5. IO server

*English · [Українська](../uk/docs/05-io-server.md)*

IO is the cluster's only entry point. It holds every Java client connection and manages the workers, but runs no game logic itself. In v1 there is one IO server (its limits and the way forward are in chapters 13 and 15).

### Duties

- **Accepting connections:** handshake, status/ping for the server list, login.
- **Authentication:** checking the player with Mojang's session server and encrypting the connection.
- **Worker registry:** handing out 16-bit Worker IDs, heartbeats, the load of every worker.
- **Placement:** a new player goes to the least loaded worker. If its zone overlaps an existing MW, the player goes to the owning worker.
- **Routing:** a "session → worker" table. On an MW merge or migration IO switches the route without breaking the connection.
- **Handing out entity IDs** to workers in blocks (chapter 9).
- **Global chat and tab list**, since only IO sees all players at once. *(Proposal.)*

### Sharing the packet work

So that IO does not become the bottleneck at 10,000 players, the heavy packet work is done by workers. *(Proposal.)*

- The worker builds game packets and **compresses** them.
- IO only **encrypts** the already compressed stream and sends it to the client.
- Between IO and a worker, a player's packets travel as opaque bytes, without re-encoding into protobuf.

### Keeping connections when a worker fails

1. IO notices that a worker no longer answers heartbeats.
2. Client connections stay open: IO answers keep-alives itself.
3. IO releases the dead worker's leases in AnvilDB.
4. The players are assigned to other workers, which restore their MWs from the last saved state.
5. The client gets the world and its position again, so it sees a short "freeze" instead of a kick.

The progress lost is bounded by the save interval (5 s).

### Planned worker restart (drain)

IO puts the worker into drain mode: it gets no new players, and all its MWs migrate to other workers by the same mechanism as a merge. After that the worker can be restarted safely.

---

[← 4. Cluster architecture](04-arkhitektura.md) · [Contents](../README.md) · [6. Worker →](06-worker.md)
