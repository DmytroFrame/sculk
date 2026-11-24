//! Micro worlds (docs/docs/03-mikrosvity.md): one game loop that owns the leases on its zone.

use crate::play::PlayState;
use sculk_proto::anvildb::ChunkPos;
use std::collections::{HashMap, HashSet};

/// A chunk in a micro world's zone.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Chunk {
    pub dimension: String,
    pub x: i32,
    pub z: i32,
}

impl Chunk {
    pub fn to_pb(&self, world: &str) -> ChunkPos {
        ChunkPos { world: world.to_owned(), dimension: self.dimension.clone(), x: self.x, z: self.z }
    }
}

/// The square of chunks within `view_distance` of a block position.
pub fn zone_around(dimension: &str, x: f64, z: f64, view_distance: u32) -> Vec<Chunk> {
    let (cx, cz) = ((x.floor() as i32) >> 4, (z.floor() as i32) >> 4);
    let r = view_distance as i32;
    let mut zone = Vec::with_capacity(((2 * r + 1) * (2 * r + 1)) as usize);
    for dz in -r..=r {
        for dx in -r..=r {
            zone.push(Chunk { dimension: dimension.to_owned(), x: cx + dx, z: cz + dz });
        }
    }
    zone
}

/// Cluster-wide unique micro world ID: the 16-bit worker ID on top, a per-worker counter below.
pub fn mw_id(worker_id: u32, n: u64) -> u64 {
    (u64::from(worker_id) << 48) | (n & ((1 << 48) - 1))
}

pub struct Player {
    pub name: String,
    pub play: PlayState,
}

pub struct MicroWorld {
    pub id: u64,
    pub players: HashMap<u64, Player>,
    pub zone: HashSet<Chunk>,
    pub ticks: u64,
}

impl MicroWorld {
    pub fn new(id: u64, founder: u64, player: Player, zone: impl IntoIterator<Item = Chunk>) -> Self {
        Self { id, players: HashMap::from([(founder, player)]), zone: zone.into_iter().collect(), ticks: 0 }
    }

    /// The zone the micro world's players need now: the union of the squares around each of them.
    pub fn wanted_zone(&self, dimension: &str, view_distance: u32) -> HashSet<Chunk> {
        self.players
            .values()
            .flat_map(|p| zone_around(dimension, p.play.pos[0], p.play.pos[2], view_distance))
            .collect()
    }

    /// One game tick. The vanilla tick order goes here (docs/docs/06-worker.md, "Тік MW").
    pub fn tick(&mut self) {
        self.ticks += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_is_a_square_around_the_chunk() {
        let zone = zone_around("minecraft:overworld", 0.5, 0.5, 2);
        assert_eq!(zone.len(), 25);
        assert!(zone.iter().all(|c| (-2..=2).contains(&c.x) && (-2..=2).contains(&c.z)));
    }

    #[test]
    fn negative_positions_floor_into_chunks() {
        let zone = zone_around("minecraft:overworld", -0.5, -17.0, 0);
        assert_eq!((zone[0].x, zone[0].z), (-1, -2));
    }

    #[test]
    fn wanted_zone_is_the_union_of_players() {
        let player = |x: f64| Player { name: "p".into(), play: PlayState::new(1, 1, [x, 0.0, 0.5]) };
        let mut mw = MicroWorld::new(1, 1, player(0.5), []);
        mw.players.insert(2, player(16.0 * 2.0 + 0.5));
        let zone = mw.wanted_zone("minecraft:overworld", 1);
        // Two 3x3 squares centred on x = 0 and x = 2 share the column x = 1.
        assert_eq!(zone.len(), 15);
    }

    #[test]
    fn mw_ids_carry_the_worker() {
        assert_eq!(mw_id(2, 1), 0x0002_0000_0000_0001);
        assert_ne!(mw_id(1, 5), mw_id(2, 5));
        assert_eq!(mw_id(3, 7) >> 48, 3);
    }
}
