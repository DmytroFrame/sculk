//! A flat overworld (stone, two dirt, grass at y = -61), encoded with `horizon_protocol::chunk`. It stands in for
//! world generation until the worker runs Horizon's engine (PLAN.md, block B).

use horizon_protocol::chunk::{BIOMES, BLOCKS, ChunkData, LightData, Paletted, Section, Strategy};
use horizon_protocol::packets::play::LevelChunkWithLight;
use sculk_data::blocks::Blocks;

/// The overworld's dimension type: minY -64, height 384.
pub const MIN_Y: i32 = -64;
pub const SECTIONS: usize = 24;
/// The top block of the ground; players stand one above.
pub const GROUND_Y: i32 = -61;

/// Heightmap.Types network IDs the client receives: WORLD_SURFACE, MOTION_BLOCKING, MOTION_BLOCKING_NO_LEAVES.
const CLIENT_HEIGHTMAPS: [i32; 3] = [1, 4, 5];

/// One flat column, the same for every position: built once and copied per chunk.
#[derive(Clone)]
pub struct FlatChunk {
    chunk: ChunkData,
    light: LightData,
}

/// SimpleBitStorage's longs: `64 / bits` values per long, never spanning two.
fn pack(values: &[u64], bits: u32) -> Vec<i64> {
    let per = (64 / bits) as usize;
    values
        .chunks(per)
        .map(|word| word.iter().enumerate().fold(0u64, |acc, (i, v)| acc | (v << (i as u32 * bits))) as i64)
        .collect()
}

impl FlatChunk {
    /// `biome` is the network ID of the biome every section gets; `biome_count` sizes the global biome palette.
    pub fn new(blocks: &Blocks, biome: i32, biome_count: usize) -> Self {
        let biome_bits = (usize::BITS - biome_count.saturating_sub(1).leading_zeros()).max(1) as u8;
        let biomes = Paletted::from_values(&[biome; BIOMES], Strategy::Biomes, biome_bits);
        let air = Paletted::from_values(&[blocks.air; BLOCKS], Strategy::Blocks, blocks.global_bits);

        let mut ground = [blocks.air; BLOCKS];
        for (y, block) in [blocks.stone, blocks.dirt, blocks.dirt, blocks.grass_block].into_iter().enumerate() {
            ground[y * 256..(y + 1) * 256].fill(block);
        }
        let sections: Vec<Section> = (0..SECTIONS)
            .map(|i| {
                if i == 0 {
                    Section {
                        non_empty: 4 * 256,
                        fluids: 0,
                        states: Paletted::from_values(&ground, Strategy::Blocks, blocks.global_bits),
                        biomes: biomes.clone(),
                    }
                } else {
                    Section { non_empty: 0, fluids: 0, states: air.clone(), biomes: biomes.clone() }
                }
            })
            .collect();

        // Every column's height is the first free block above the ground, counted from MIN_Y.
        let height = (GROUND_Y + 1 - MIN_Y) as u64;
        let bits = u32::BITS - (SECTIONS as u32 * 16).leading_zeros();
        let column = pack(&[height; 256], bits);
        let heightmaps = CLIENT_HEIGHTMAPS.iter().map(|&kind| (kind, column.clone())).collect();

        // Full sky light in the light section under the world and the two lowest block sections; the client takes
        // sections above the last one sent as open sky.
        let lit = 3;
        let light = LightData {
            sky_mask: vec![(1u8 << lit) - 1],
            block_mask: Vec::new(),
            empty_sky_mask: Vec::new(),
            empty_block_mask: Vec::new(),
            sky: vec![vec![0xff; 2048]; lit],
            block: Vec::new(),
        };

        let chunk = ChunkData { heightmaps, buffer: ChunkData::from_sections(&sections), block_entities: Vec::new() };
        Self { chunk, light }
    }

    pub fn at(&self, x: i32, z: i32) -> LevelChunkWithLight {
        LevelChunkWithLight { x, z, chunk: self.chunk.clone(), light: self.light.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use horizon_protocol::packets::play::Clientbound;

    #[test]
    fn chunk_round_trips_through_horizon() {
        let blocks = Blocks::default();
        let flat = FlatChunk::new(&blocks, 3, 65);
        let body = Clientbound::LevelChunkWithLight(flat.at(5, -7)).encode();
        let mut r = horizon_protocol::buf::Reader::new(&body);
        let id = r.varint().unwrap();
        let Clientbound::LevelChunkWithLight(back) = Clientbound::decode(id, r.rest()).unwrap() else { panic!() };
        assert_eq!((back.x, back.z), (5, -7));
        let sections = back.chunk.sections(SECTIONS, blocks.global_bits, 7).unwrap();
        let ground = &sections[0].states;
        assert_eq!(ground.get(0), blocks.stone);
        assert_eq!(ground.get(256), blocks.dirt);
        assert_eq!(ground.get(3 * 256 + 17), blocks.grass_block);
        assert_eq!(ground.get(4 * 256), blocks.air);
        assert_eq!(sections[0].biomes.get(10), 3);
        assert_eq!(sections[5].states.get(100), blocks.air);
    }

    #[test]
    fn heightmap_packs_nine_bits() {
        let longs = pack(&[4; 256], 9);
        assert_eq!(longs.len(), 37);
        assert_eq!(longs[0] & 0x1ff, 4);
        assert_eq!((longs[0] >> 9) & 0x1ff, 4);
        assert_eq!(longs[36] >> 9 & 0x1ff, 4, "the last long holds 256 - 36 * 7 = 4 values");
    }
}
