# 13. Limitations and open questions

*English · [Українська](../uk/docs/13-obmezhennia.md)*

The architecture scales because players are spread over the world. It deliberately sacrifices the cases where very many people gather in one place.

### Accepted trade-offs

| Limitation | Consequence | Why it is acceptable |
| --- | --- | --- |
| One MW = one core | More than 200–500 players in one spot slow that MW's tick | The rest of the world does not suffer |
| One IO server | A single point of failure and a traffic bottleneck | Keeps v1 simple; the way forward is in chapter 15 |
| Merge pause | Up to ~200 ms "freeze" for the players who merge | Rare, and only when players come close |
| Asynchronous saving | Up to 5 s of progress lost when a worker crashes | Regular servers lose more when they crash |
| A world without players is frozen | Farms far from players do not work | Vanilla works the same way |

### The biggest amount of work

Reproducing vanilla 1:1 from scratch in Rust is the hardest part of the project, harder than the distributed architecture itself. World generation must match by seed, and redstone must reproduce every nuance technical players rely on. The Rust server Pumpkin is worth studying: some of its protocol or generation code might be reused.

### Open questions

- [ ] Skipping the night: how to count sleeping players among 10,000 people — over the whole world or otherwise?
- [ ] Should spawn chunks be kept loaded all the time?
- [ ] Extension API: native Rust or WASM?
- [ ] The exact lease TTL and entity ID block size.
- [ ] Who absorbs whom on a simultaneous merge (proposal: the MW with the lower ID).

---

[← 12. Extensions and AnvilDB Connector](12-rozshyrennia.md) · [Contents](../README.md) · [14. Roadmap →](14-dorozhnia-karta.md)
