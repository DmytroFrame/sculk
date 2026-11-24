# 11. Nether Tunnel

*English · [Українська](../uk/docs/11-nether-tunnel.md)*

Nether Tunnel is an optional client program for those to whom latency matters, such as PvP players. A plain TCP connection stays the main way in, and Nether Tunnel is not needed to play.

### How it works

1. The player starts Nether Tunnel, and it opens a local address, for example `127.0.0.1:25565`.
2. The Minecraft client connects to that address as to a regular server.
3. Nether Tunnel wraps the Java traffic in its own protocol over UDP with stronger compression and sends it to IO.
4. IO unwraps the traffic and handles it as a regular session.

### Where the gain comes from

The Minecraft client still expects an ordered stream, so Nether Tunnel delivers data in order. The gain is on the path between the player and the server:

- faster retransmission of lost packets, without TCP's recovery delays;
- the option of forward error correction (FEC), to recover some losses without retransmission;
- stronger compression with a shared dictionary for typical Minecraft packets.

The biggest effect is expected for players with an unstable connection or far from the server. On a good link the difference will be small, and the players' documentation should say so honestly.

---

[← 10. Reliability and failures](10-nadiinist.md) · [Contents](../README.md) · [12. Extensions and AnvilDB Connector →](12-rozshyrennia.md)
