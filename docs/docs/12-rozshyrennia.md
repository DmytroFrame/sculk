# 12. Extensions and AnvilDB Connector

*English · [Українська](../uk/docs/12-rozshyrennia.md)*

Sculk will have its own extension API; Paper's Java plugins will not run inside Sculk. Instead, regular Paper servers will be able to connect to AnvilDB through a separate plugin.

### Sculk's API

Extensions are written against Sculk's own API. Paper plugins are written in Java, and running them inside a Rust server would mean embedding a JVM, which is complex and slow.

**Open question:** native Rust extensions or WASM modules. WASM gives isolation (a plugin cannot "take down" a worker) and lets extensions be written in different languages, but adds overhead to every call.

### AnvilDB Connector

AnvilDB is a full database that does not fit into a plugin. AnvilDB Connector is a light plugin for Paper servers that connects to AnvilDB as a client.

- It intercepts chunk loading and saving and works with chunks through AnvilDB instead of local `.mca` files.
- It gives regular servers S3 storage, the archive and the optimized format.
- How it is built depends on how easily Paper's world saving and loading code can be intercepted. Support for Bukkit/Spigot is to be decided after that.

Since the Connector works with the same leases, a regular Paper server could one day become "one MW" in a world shared with Sculk. That is not a goal of v1, but the architecture does not rule it out.

---

[← 11. Nether Tunnel](11-nether-tunnel.md) · [Contents](../README.md) · [13. Limitations and open questions →](13-obmezhennia.md)
