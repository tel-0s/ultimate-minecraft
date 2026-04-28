//! Stage 1 worldgen: octaved-Perlin heightmap with a fixed sea level.
//!
//! Approximates the *shape* of vanilla overworld terrain (rolling hills,
//! occasional peaks, ocean basins) without yet using vanilla's multi-noise
//! biome system or density-function pipeline. The output is recognisably
//! Minecraft-like: stone bulk, dirt skin, grass surface, sand at the
//! waterline, water filling depressions to sea level, bedrock floor.

use noise::{Fbm, MultiFractal, NoiseFn, Perlin};

use ultimate_engine::world::chunk::Chunk;
use ultimate_engine::world::position::LocalBlockPos;

use crate::block;
use super::WorldGen;

/// Y of the topmost bedrock layer. Below this is void.
const BEDROCK_Y: i64 = 0;
/// Default sea level for Stage 1. Vanilla uses 63; matching that means the
/// flatworld y=64 dirt layer would have been *above* the sea, which keeps
/// continuity with what existed before this generator.
pub const SEA_LEVEL: i64 = 63;
/// Beach band: blocks at or just below sea level get sand, not grass.
const BEACH_BAND: i64 = 2;
/// How deep the dirt skin is below the surface block.
const DIRT_DEPTH: i64 = 4;

/// Octaved-Perlin heightmap terrain. Deterministic from `seed`.
pub struct NoiseTerrainGen {
    /// Coarse continent shape: very low frequency, large amplitude.
    continent: Fbm<Perlin>,
    /// Medium-frequency relief: hills and valleys.
    hills: Fbm<Perlin>,
    /// High-frequency roughness: small bumps, cliffs, surface noise.
    detail: Fbm<Perlin>,
    /// Sea level in blocks.
    sea_level: i64,
}

impl NoiseTerrainGen {
    pub fn new(seed: u32) -> Self {
        // Three independent noise fields with offset seeds so they're
        // uncorrelated. Lacunarity/persistence picks are vanilla-ish:
        // higher persistence (0.5+) gives a noisier, busier world; lower
        // gives smoother rolling terrain.
        let continent = Fbm::<Perlin>::new(seed)
            .set_octaves(4)
            .set_frequency(1.0 / 512.0)
            .set_lacunarity(2.0)
            .set_persistence(0.55);

        let hills = Fbm::<Perlin>::new(seed.wrapping_add(1))
            .set_octaves(4)
            .set_frequency(1.0 / 96.0)
            .set_lacunarity(2.0)
            .set_persistence(0.5);

        let detail = Fbm::<Perlin>::new(seed.wrapping_add(2))
            .set_octaves(3)
            .set_frequency(1.0 / 32.0)
            .set_lacunarity(2.0)
            .set_persistence(0.4);

        Self {
            continent,
            hills,
            detail,
            sea_level: SEA_LEVEL,
        }
    }

    /// Compute the surface height (y of the topmost solid block) at world
    /// column `(x, z)`.
    fn surface_y(&self, x: i64, z: i64) -> i64 {
        let xf = x as f64;
        let zf = z as f64;

        // continent: -1..1 → -16..40 (oceans dip well below sea level,
        //                              continents rise modestly above it).
        let c = self.continent.get([xf, zf]);
        let continent_h = self.sea_level as f64 + c * 28.0 + 12.0;

        // hills: -1..1 → ±18 — local relief on top of the continent shape.
        let h = self.hills.get([xf, zf]);
        let hills_h = h * 18.0;

        // detail: -1..1 → ±3 — small surface roughness so the grass plane
        //                       isn't perfectly smooth.
        let d = self.detail.get([xf, zf]);
        let detail_h = d * 3.0;

        let y = continent_h + hills_h + detail_h;
        // Clamp to a sensible range so we never punch through bedrock or
        // shoot above the world ceiling.
        y.clamp(BEDROCK_Y as f64 + 4.0, 250.0).round() as i64
    }
}

impl WorldGen for NoiseTerrainGen {
    fn generate_chunk(&self, cx: i32, cz: i32) -> Chunk {
        let mut chunk = Chunk::new();
        let base_x = cx as i64 * 16;
        let base_z = cz as i64 * 16;

        for lx in 0..16u8 {
            for lz in 0..16u8 {
                let wx = base_x + lx as i64;
                let wz = base_z + lz as i64;
                let surface = self.surface_y(wx, wz);

                // Bedrock floor (single layer).
                chunk.set_block(
                    LocalBlockPos { x: lx, y: BEDROCK_Y, z: lz },
                    block::BEDROCK,
                );

                // Solid bulk: stone from BEDROCK_Y+1 up to dirt skin.
                let dirt_top = surface;
                let dirt_bottom = (surface - DIRT_DEPTH).max(BEDROCK_Y + 1);
                for y in (BEDROCK_Y + 1)..dirt_bottom {
                    chunk.set_block(LocalBlockPos { x: lx, y, z: lz }, block::STONE);
                }

                // Dirt skin (or sand at the waterline).
                let surface_block = if surface <= self.sea_level + BEACH_BAND
                    && surface >= self.sea_level - BEACH_BAND
                {
                    // Beach: top + dirt depth → sand.
                    block::SAND
                } else {
                    block::DIRT
                };
                for y in dirt_bottom..dirt_top {
                    chunk.set_block(LocalBlockPos { x: lx, y, z: lz }, surface_block);
                }

                // Surface block.
                let top_block = if surface < self.sea_level {
                    // Underwater seafloor: sand or dirt depending on depth.
                    if self.sea_level - surface <= BEACH_BAND {
                        block::SAND
                    } else {
                        block::DIRT
                    }
                } else if surface <= self.sea_level + BEACH_BAND {
                    block::SAND
                } else {
                    block::GRASS_BLOCK
                };
                chunk.set_block(
                    LocalBlockPos { x: lx, y: surface, z: lz },
                    top_block,
                );

                // Fill water from surface+1 up to sea level if the surface
                // is below sea level.
                if surface < self.sea_level {
                    for y in (surface + 1)..=self.sea_level {
                        chunk.set_block(
                            LocalBlockPos { x: lx, y, z: lz },
                            block::WATER,
                        );
                    }
                }
            }
        }

        chunk
    }

    fn spawn_y(&self, x: i64, z: i64) -> f64 {
        let surface = self.surface_y(x, z);
        // Stand on top of the surface, with a one-block air gap. If the
        // column is underwater, spawn at sea level + 1 (player on water).
        (surface.max(self.sea_level) + 1) as f64 + 0.001
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_from_seed() {
        let g1 = NoiseTerrainGen::new(42);
        let g2 = NoiseTerrainGen::new(42);
        for x in -50..50i64 {
            for z in -50..50i64 {
                assert_eq!(g1.surface_y(x, z), g2.surface_y(x, z));
            }
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let g1 = NoiseTerrainGen::new(42);
        let g2 = NoiseTerrainGen::new(43);
        let mut differences = 0;
        for x in -50..50i64 {
            for z in -50..50i64 {
                if g1.surface_y(x, z) != g2.surface_y(x, z) {
                    differences += 1;
                }
            }
        }
        assert!(differences > 1000, "different seeds should diverge widely");
    }

    #[test]
    fn surface_height_in_reasonable_range() {
        let g = NoiseTerrainGen::new(0xdeadbeef);
        for x in -200..200i64 {
            for z in -200..200i64 {
                let y = g.surface_y(x, z);
                assert!(y > BEDROCK_Y, "surface above bedrock at ({},{}): {}", x, z, y);
                assert!(y < 250, "surface below ceiling at ({},{}): {}", x, z, y);
            }
        }
    }

    #[test]
    fn chunk_has_bedrock_floor_and_solid_column() {
        let g = NoiseTerrainGen::new(7);
        let chunk = g.generate_chunk(0, 0);
        // Center column.
        assert_eq!(
            chunk.get_block(LocalBlockPos { x: 8, y: BEDROCK_Y, z: 8 }),
            block::BEDROCK,
        );
        // y=1 should be stone (above bedrock).
        assert_eq!(
            chunk.get_block(LocalBlockPos { x: 8, y: BEDROCK_Y + 1, z: 8 }),
            block::STONE,
        );
    }

    #[test]
    fn ocean_basins_exist_somewhere() {
        // Continent noise dips below sea level in some regions of the world.
        // Sample a wide patch — the continent noise is low-frequency, so
        // small grids may sit entirely on a continent's plateau.
        let g = NoiseTerrainGen::new(0xc0ffee);
        let mut underwater_columns = 0;
        for cx in -16..16i32 {
            for cz in -16..16i32 {
                let chunk = g.generate_chunk(cx, cz);
                for lx in 0..16u8 {
                    for lz in 0..16u8 {
                        let at_sea = chunk.get_block(LocalBlockPos {
                            x: lx, y: SEA_LEVEL, z: lz,
                        });
                        if at_sea == block::WATER {
                            underwater_columns += 1;
                        }
                    }
                }
            }
        }
        assert!(
            underwater_columns > 0,
            "expected some underwater columns across a 32x32 chunk patch"
        );
    }
}
