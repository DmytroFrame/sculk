# 15. Future ideas

*English · [Українська](../uk/docs/15-maibutni-idei.md)*

These ideas are not part of the implementation plan, but the architecture should not rule them out.

### Several IOs: a coordinator and gateways

To remove the single point of failure, IO can be split into two roles:

- a **coordinator** — one, with a hot standby. Only it makes decisions: the worker registry, routes, handing out IDs;
- **gateways** — as many as you like, stateless. They only hold client connections, encrypt and forward packets along the coordinator's routes.

The cluster has no conflicts, since only one side makes decisions. A gateway's crash disconnects only its players, and the standby takes over from a crashed coordinator.

### GPUs on workers

A worker could hand bulk computations to a GPU, such as pathfinding for thousands of mobs at once. An MW's logic must stay deterministic, so the GPU's results must match vanilla's algorithm exactly.

### Dividing dense MWs

If thousands of players in one spot are ever needed, dividing one MW over several threads inside, as Folia does, could be explored. It is deliberately not planned now.

---

[← 14. Roadmap](14-dorozhnia-karta.md) · [Contents](../README.md)
