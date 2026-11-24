# 2. Glossary

*English · [Українська](../uk/docs/02-hlosarii.md)*

Component names stay in English; the other terms are explained here.

| Term | Meaning |
| --- | --- |
| **MW (Micro World)** | An independent game loop that owns a set of chunks and processes all players and entities in them |
| **MW zone** | All chunks within view distance of every player of that MW |
| **Lease** | An MW's exclusive right to read and write a chunk. Granted by AnvilDB, has a time to live (TTL) and is renewed |
| **Merge** | Handing all state of one MW to another when their zones overlap |
| **Split** | Moving a group of players into a new MW when they have been far from the rest for a long time |
| **IO** | The entry server: holds client connections, authenticates, manages workers |
| **Worker** | A processing server: ticks many MWs at once. Has a 16-bit Worker ID from IO |
| **AnvilDB** | The database of Minecraft worlds: chunks, global state, leases |
| **Entity ID** | The integer the Java protocol uses for an entity in packets. Lives only while the server runs |
| **UUID** | The permanent identifier of an entity or player in storage |
| **View distance** | The radius of chunks the server sends to the client |
| **Simulation distance** | The radius of chunks that are ticked |
| **Nether Tunnel** | An optional client program with its own UDP transport |
| **AnvilDB Connector** | A plugin for Paper servers that lets them keep their worlds in AnvilDB |

---

[← 1. Introduction and goal](01-vstup.md) · [Contents](../README.md) · [3. Micro worlds (MW) →](03-mikrosvity.md)
