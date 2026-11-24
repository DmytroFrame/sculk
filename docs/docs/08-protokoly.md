# 8. Protocols between services

*English · [Українська](../uk/docs/08-protokoly.md)*

All internal services talk protobuf. The transport is gRPC with bidirectional streams, as in the IWM prototype. The protocol can be replaced with a faster one if gRPC turns out to be the bottleneck. From outside, the client sees only the standard Java Edition protocol.

| Channel | Transport | What goes over it |
| --- | --- | --- |
| Client → IO | Java Edition protocol (TCP) or Nether Tunnel (UDP) | The player's game traffic |
| IO ↔ Worker | gRPC stream | Players' packets as opaque bytes; control commands: assign a session, switch, drain |
| Worker ↔ Worker | gRPC stream | An MW's serialized state on a merge, split or migration |
| Worker ↔ AnvilDB | gRPC stream with a command queue | Leases, chunks, global state, player data |
| IO ↔ AnvilDB | gRPC | Releasing dead workers' leases, global state |
| AnvilDB Connector ↔ AnvilDB | gRPC | Chunks and world state for regular Paper servers |

One binary holds every role, so the `.proto` schemas and the serialization code are shared by all services and cannot drift apart between versions.

---

[← 7. AnvilDB](07-anvildb.md) · [Contents](../README.md) · [9. Global state and identifiers →](09-hlobalnyi-stan.md)
