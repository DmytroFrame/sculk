# 1. Introduction and goal

*English · [Українська](../uk/docs/01-vstup.md)*

Sculk is an open source Minecraft: Java Edition server written in Rust. Its goal is 10,000+ players in one seamless world, behaving so that a player cannot tell it from a vanilla server.

### The problem

A vanilla server has a single game loop: 20 ticks a second, 50 ms a tick. Every chunk, entity and player is processed on one thread. Extra CPU cores barely help, so even a powerful server runs into a wall at a few hundred players.

### The idea: a world as an organism of cells

Instead of one game loop, Sculk runs thousands of independent ones: **micro worlds (MW, Micro World)**. Every player always lives in exactly one MW, and an MW behaves like a small vanilla server with its own mobs, ticks and redstone.

MWs behave like cells:

- they **grow** as their players move;
- they **merge** when players come close to each other;
- they **split** when players stay apart for a long time.

Since players on large servers are usually spread across the map, thousands of MWs tick in parallel on many cores and many machines.

### How Sculk differs from similar projects

- **Folia** (PaperMC) divides the world into regions that merge and split, but works within one process.
- **MultiPaper** distributes chunk ownership among several servers, but each server remains a regular Paper.
- **Sculk** ties regions to players, spreads them over a cluster of workers, keeps the world in a separate database, **AnvilDB**, and is written from scratch in Rust.

### Non-goals

- A thousand players in one spot. One MW is limited to one core, and that is a deliberate trade-off (chapter 13).
- Support for mods and Paper's Java plugins.

The project started in 2023 under the name IWM ([prototype](https://github.com/DmytroFrame/iwm)). Sculk is its new name and a rethought architecture.

---

[Contents](../README.md) · [2. Glossary →](02-hlosarii.md)
