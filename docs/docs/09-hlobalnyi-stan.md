# 9. Global state and identifiers

*English · [Українська](../uk/docs/09-hlobalnyi-stan.md)*

Everything about the world as a whole lives in AnvilDB, and everything local lives in an MW on a worker. Entity identifiers are handed out so that a merge never needs renumbering.

### What lives where

| Data | Where | Who changes it |
| --- | --- | --- |
| Time of day, weather, gamerules, world border, scoreboard | AnvilDB | Only AnvilDB counts time; MWs change the state with a command to AnvilDB (such as `/time set`) |
| Player data | AnvilDB | The MW the player is in now |
| Chunks and blocks | AnvilDB | The MW holding the lease |
| Villagers, raids, the Ender Dragon, other local mechanics | Worker, in the MW | The MW itself |
| Chat, tab list | IO | IO *(proposal)* |

Workers sync their clock with AnvilDB now and then, so all MWs see the same time of day.

### Entity ID

In the Java protocol a UUID travels only in an entity's spawn packet. Every later packet (movement, metadata, a hit, removal) refers to the integer entity ID. Beta 1.7.3 has no UUIDs at all. If two MWs hand out the same IDs, after a merge the client confuses the entities.

**Decision:** entity IDs are globally unique across the cluster.

1. IO hands IDs to workers in blocks, for example 4096 at a time.
2. A worker gives IDs from its block to new entities in its MWs.
3. When the block runs out, the worker asks for the next one.
4. On a merge, entities keep their IDs, since they are already unique.

### UUID

A UUID is the permanent identity of a player or entity in storage. An entity ID exists only while the server runs and is never saved.

---

[← 8. Protocols between services](08-protokoly.md) · [Contents](../README.md) · [10. Reliability and failures →](10-nadiinist.md)
