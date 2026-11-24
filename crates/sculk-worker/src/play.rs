//! A player in play on this worker: what it is sent on joining, its moves, and the chunks around it.
//!
//! The order follows vanilla's PlayerList.placeNewPlayer and ChunkMap, cut to what a flat, static world needs.

use crate::flat::FlatChunk;
use anyhow::anyhow;
use horizon_protocol::packets::configuration::KeepAlive;
use horizon_protocol::packets::play::{
    ChunkBatchFinished, Clientbound, ForgetLevelChunk, GameEvent, GlobalPos, Login, PlayerAbilities, PlayerPosition,
    Serverbound, SetChunkCacheCenter, SetDefaultSpawnPosition, SpawnInfo,
};
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// GameEvent LEVEL_CHUNKS_LOAD_START.
const LEVEL_CHUNKS_LOAD_START: u8 = 13;
/// Vanilla sends a keep-alive every 15 seconds.
const KEEP_ALIVE_EVERY: Duration = Duration::from_secs(15);
const OVERWORLD: &str = "minecraft:overworld";

/// What every player on this worker shares: the world's chunks and registry IDs.
pub struct World {
    pub flat: FlatChunk,
    pub overworld_type: i32,
}

pub struct PlayState {
    pub entity_id: i32,
    pub pos: [f64; 3],
    pub yaw: f32,
    pub pitch: f32,
    /// Chunks sent around the player, in chunks.
    pub view: i32,
    center: (i32, i32),
    sent: HashSet<(i32, i32)>,
    last_keep_alive: Instant,
}

fn chunk_of(pos: [f64; 3]) -> (i32, i32) {
    ((pos[0].floor() as i32) >> 4, (pos[2].floor() as i32) >> 4)
}

/// The square of chunks within `view` of `center`, nearest first, so the ground under the player comes first
/// (IWM's chunk tracer did the same with a spiral).
pub fn chunks_around(center: (i32, i32), view: i32) -> Vec<(i32, i32)> {
    let mut chunks: Vec<(i32, i32)> =
        (-view..=view).flat_map(|dz| (-view..=view).map(move |dx| (center.0 + dx, center.1 + dz))).collect();
    chunks.sort_by_key(|&(x, z)| {
        let (dx, dz) = (x - center.0, z - center.1);
        (dx * dx + dz * dz, dx, dz)
    });
    chunks
}

fn encode(packet: Clientbound) -> Vec<u8> {
    packet.encode()
}

impl PlayState {
    pub fn new(entity_id: i32, view: i32, pos: [f64; 3]) -> Self {
        Self {
            entity_id,
            pos,
            yaw: 0.0,
            pitch: 0.0,
            view,
            center: chunk_of(pos),
            sent: HashSet::new(),
            last_keep_alive: Instant::now(),
        }
    }

    pub fn chunk(&self) -> (i32, i32) {
        chunk_of(self.pos)
    }

    /// Everything a client needs to leave the loading screen: the level, the player and the chunks around it.
    pub fn join(&mut self, world: &World) -> Vec<Vec<u8>> {
        let spawn = SpawnInfo {
            dimension_type: world.overworld_type,
            dimension: OVERWORLD.into(),
            seed: 0,
            game_type: 1, // creative: flying around a world the worker doesn't simulate yet
            previous_game_type: None,
            debug: false,
            flat: true,
            last_death: None,
            portal_cooldown: 0,
            sea_level: 63,
        };
        let mut out = vec![
            encode(Clientbound::Login(Login {
                player_id: self.entity_id,
                hardcore: false,
                levels: vec![OVERWORLD.into(), "minecraft:the_nether".into(), "minecraft:the_end".into()],
                max_players: 0,
                chunk_radius: self.view,
                simulation_distance: self.view,
                reduced_debug_info: false,
                show_death_screen: true,
                limited_crafting: false,
                spawn,
                online_mode: false,
                enforces_secure_chat: false,
            })),
            // Invulnerable, may fly, instant build.
            encode(Clientbound::PlayerAbilities(PlayerAbilities {
                flags: 0x0d,
                flying_speed: 0.05,
                walking_speed: 0.1,
            })),
            encode(Clientbound::SetDefaultSpawnPosition(SetDefaultSpawnPosition {
                pos: GlobalPos {
                    dimension: OVERWORLD.into(),
                    pos: [self.pos[0].floor() as i32, self.pos[1].floor() as i32, self.pos[2].floor() as i32],
                },
                yaw: 0.0,
                pitch: 0.0,
            })),
            encode(Clientbound::PlayerPosition(PlayerPosition {
                teleport: 1,
                position: self.pos,
                delta: [0.0; 3],
                yaw: self.yaw,
                pitch: self.pitch,
                relatives: 0,
            })),
            encode(Clientbound::GameEvent(GameEvent { event: LEVEL_CHUNKS_LOAD_START, param: 0.0 })),
        ];
        self.center = self.chunk();
        out.push(encode(Clientbound::SetChunkCacheCenter(SetChunkCacheCenter { x: self.center.0, z: self.center.1 })));
        self.send_chunks(world, &mut out);
        out
    }

    /// Sends the chunks in view the client lacks, and forgets those out of it.
    fn send_chunks(&mut self, world: &World, out: &mut Vec<Vec<u8>>) {
        let wanted = chunks_around(self.center, self.view);
        let wanted_set: HashSet<_> = wanted.iter().copied().collect();
        let far: Vec<_> = self.sent.iter().copied().filter(|c| !wanted_set.contains(c)).collect();
        for (x, z) in far {
            self.sent.remove(&(x, z));
            out.push(encode(Clientbound::ForgetLevelChunk(ForgetLevelChunk { pos: [x, z] })));
        }
        let new: Vec<_> = wanted.into_iter().filter(|c| !self.sent.contains(c)).collect();
        if new.is_empty() {
            return;
        }
        out.push(encode(Clientbound::ChunkBatchStart(Default::default())));
        for &(x, z) in &new {
            out.push(encode(Clientbound::LevelChunkWithLight(world.flat.at(x, z))));
            self.sent.insert((x, z));
        }
        out.push(encode(Clientbound::ChunkBatchFinished(ChunkBatchFinished { size: new.len() as i32 })));
    }

    /// One packet from the client. Returns what goes back.
    pub fn handle(&mut self, body: &[u8], world: &World) -> anyhow::Result<Vec<Vec<u8>>> {
        let mut r = horizon_protocol::buf::Reader::new(body);
        let id = r.varint().map_err(|e| anyhow!(e.0))?;
        let packet = Serverbound::decode(id, r.rest()).map_err(|e| anyhow!(e.0))?;
        let mut out = Vec::new();
        match packet {
            Serverbound::MovePlayerPos(p) => self.pos = [p.x, p.y, p.z],
            Serverbound::MovePlayerPosRot(p) => {
                self.pos = [p.x, p.y, p.z];
                (self.yaw, self.pitch) = (p.yaw, p.pitch);
            }
            Serverbound::MovePlayerRot(p) => (self.yaw, self.pitch) = (p.yaw, p.pitch),
            Serverbound::ClientInformation(info) => {
                let view = i32::from(info.view_distance).clamp(2, self.view.max(2));
                if view != self.view {
                    self.view = view;
                    self.send_chunks(world, &mut out);
                }
            }
            // Accepted teleports, chunk batch acks, keep-alive answers and the rest need nothing yet.
            _ => {}
        }
        let center = self.chunk();
        if center != self.center {
            self.center = center;
            out.push(encode(Clientbound::SetChunkCacheCenter(SetChunkCacheCenter { x: center.0, z: center.1 })));
            self.send_chunks(world, &mut out);
        }
        Ok(out)
    }

    /// A keep-alive when one is due.
    pub fn keep_alive(&mut self, now: Instant) -> Option<Vec<u8>> {
        if now.duration_since(self.last_keep_alive) < KEEP_ALIVE_EVERY {
            return None;
        }
        self.last_keep_alive = now;
        let id = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
        Some(encode(Clientbound::KeepAlive(KeepAlive { id: id as i64 })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use horizon_protocol::packets::play::MovePlayerPos;

    fn world() -> World {
        World { flat: FlatChunk::new(&Default::default(), 0, 64), overworld_type: 0 }
    }

    fn decode(body: &[u8]) -> Clientbound {
        let mut r = horizon_protocol::buf::Reader::new(body);
        let id = r.varint().unwrap();
        Clientbound::decode(id, r.rest()).unwrap()
    }

    fn serverbound(p: Serverbound) -> Vec<u8> {
        p.encode()
    }

    #[test]
    fn nearest_chunks_come_first() {
        let chunks = chunks_around((3, -2), 2);
        assert_eq!(chunks.len(), 25);
        assert_eq!(chunks[0], (3, -2));
        assert!(chunks[1..5].iter().all(|&(x, z)| (x - 3).abs() + (z + 2).abs() == 1));
    }

    #[test]
    fn join_sends_login_then_a_batch_of_chunks() {
        let mut p = PlayState::new(42, 2, [0.5, -60.0, 0.5]);
        let out: Vec<Clientbound> = p.join(&world()).iter().map(|b| decode(b)).collect();
        assert!(matches!(&out[0], Clientbound::Login(l) if l.player_id == 42));
        let chunks = out.iter().filter(|p| matches!(p, Clientbound::LevelChunkWithLight(_))).count();
        assert_eq!(chunks, 25);
        assert!(matches!(out.last(), Some(Clientbound::ChunkBatchFinished(b)) if b.size == 25));
    }

    #[test]
    fn crossing_a_chunk_sends_a_row_and_forgets_one() {
        let w = world();
        let mut p = PlayState::new(1, 2, [0.5, -60.0, 0.5]);
        p.join(&w);
        let out = p
            .handle(&serverbound(Serverbound::MovePlayerPos(MovePlayerPos { x: 16.5, y: -60.0, z: 0.5, flags: 1 })), &w)
            .unwrap();
        let out: Vec<Clientbound> = out.iter().map(|b| decode(b)).collect();
        assert!(matches!(&out[0], Clientbound::SetChunkCacheCenter(c) if (c.x, c.z) == (1, 0)));
        let forgot = out.iter().filter(|p| matches!(p, Clientbound::ForgetLevelChunk(f) if f.pos[0] == -2)).count();
        let sent = out.iter().filter(|p| matches!(p, Clientbound::LevelChunkWithLight(c) if c.x == 3)).count();
        assert_eq!((forgot, sent), (5, 5));
        // Moving inside the same chunk sends nothing.
        let out = p
            .handle(&serverbound(Serverbound::MovePlayerPos(MovePlayerPos { x: 20.0, y: -60.0, z: 3.0, flags: 1 })), &w)
            .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn keep_alive_waits_fifteen_seconds() {
        let mut p = PlayState::new(1, 2, [0.0; 3]);
        let now = Instant::now();
        assert!(p.keep_alive(now).is_none());
        assert!(p.keep_alive(now + KEEP_ALIVE_EVERY).is_some());
        assert!(p.keep_alive(now + KEEP_ALIVE_EVERY).is_none());
    }
}
