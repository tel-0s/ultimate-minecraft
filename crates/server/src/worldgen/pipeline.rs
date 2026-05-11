//! Pipelines compose a density function (and other layers, later) into a
//! concrete [`WorldGen`].
//!
//! ## Stage A scope
//!
//! - [`DensityPipeline`] — walks each column top-down through a density
//!   function to find the surface, then stratifies with a fixed
//!   bedrock / stone / dirt / grass / sand / water stack. Stage B will
//!   replace the fixed stratification with a composable surface-rule tree.
//! - [`FlatPipeline`] — superflat preset: bedrock floor + a stack of fixed
//!   layers per column. No noise sampling, instant generation.

use std::sync::Arc;

use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::chunk::Chunk;
use ultimate_engine::world::position::LocalBlockPos;

use crate::block;
use super::WorldGen;
use super::density::DensityFunction;

/// Stage-A density-function pipeline. Hand-rolled stratification; Stage B
/// will replace the `if`-cascade with a composable `SurfaceRule` tree.
pub struct DensityPipeline {
    pub density: Arc<dyn DensityFunction>,
    /// If the preset's density was structurally `height(x,z) - y_index`
    /// with `height` y-independent, this holds the compiled height field.
    /// `surface_y` then samples it once per column instead of walking up
    /// to ~384 y values — the difference between sub-second pregeneration
    /// and tens-of-minutes hangs.
    pub heightmap_shortcut: Option<Arc<dyn DensityFunction>>,
    pub sea_level: i64,
    pub min_y: i64,
    pub max_y: i64,
    pub bedrock_y: i64,
    pub dirt_depth: i64,
    pub beach_band: i64,
}

impl DensityPipeline {
    /// Find the surface Y for column `(x, z)`.
    ///
    /// Fast path: if the density was structurally `height(x,z) - y_index`,
    /// the surface is exactly `floor(height(x,z))`. One density evaluation.
    ///
    /// Slow path (true 3D density, e.g. with caves): walk the column
    /// top-down looking for the first y where density crosses positive.
    fn surface_y(&self, x: i64, z: i64) -> i64 {
        if let Some(h) = &self.heightmap_shortcut {
            let raw = h.sample(x, 0, z).floor() as i64;
            return raw.clamp(self.bedrock_y, self.max_y);
        }
        for y in (self.bedrock_y + 1..=self.max_y).rev() {
            if self.density.sample(x, y, z) >= 0.0 {
                return y;
            }
        }
        self.bedrock_y
    }
}

impl WorldGen for DensityPipeline {
    fn generate_chunk(&self, cx: i32, cz: i32) -> Chunk {
        let mut chunk = Chunk::new();
        let base_x = cx as i64 * 16;
        let base_z = cz as i64 * 16;

        for lx in 0..16u8 {
            for lz in 0..16u8 {
                let wx = base_x + lx as i64;
                let wz = base_z + lz as i64;
                let surface = self.surface_y(wx, wz);

                // Bedrock floor.
                chunk.set_block(
                    LocalBlockPos { x: lx, y: self.bedrock_y, z: lz },
                    block::BEDROCK,
                );

                // Stone bulk up to where the dirt skin starts.
                let dirt_top = surface;
                let dirt_bottom = (surface - self.dirt_depth).max(self.bedrock_y + 1);
                for y in (self.bedrock_y + 1)..dirt_bottom {
                    chunk.set_block(LocalBlockPos { x: lx, y, z: lz }, block::STONE);
                }

                // Skin band: dirt normally, sand near the waterline.
                let skin = if surface <= self.sea_level + self.beach_band
                    && surface >= self.sea_level - self.beach_band
                {
                    block::SAND
                } else {
                    block::DIRT
                };
                for y in dirt_bottom..dirt_top {
                    chunk.set_block(LocalBlockPos { x: lx, y, z: lz }, skin);
                }

                // Top block.
                let top = if surface < self.sea_level {
                    if self.sea_level - surface <= self.beach_band {
                        block::SAND
                    } else {
                        block::DIRT
                    }
                } else if surface <= self.sea_level + self.beach_band {
                    block::SAND
                } else {
                    block::GRASS_BLOCK
                };
                chunk.set_block(LocalBlockPos { x: lx, y: surface, z: lz }, top);

                // Water from surface+1 up to sea level (only for sub-sea columns).
                if surface < self.sea_level {
                    for y in (surface + 1)..=self.sea_level {
                        chunk.set_block(LocalBlockPos { x: lx, y, z: lz }, block::WATER);
                    }
                }
            }
        }

        chunk
    }

    fn spawn_y(&self, x: i64, z: i64) -> f64 {
        let surface = self.surface_y(x, z);
        (surface.max(self.sea_level) + 1) as f64 + 0.001
    }
}

/// Superflat pipeline: bedrock + a fixed stack of layers per column.
/// Identical across all (x, z), so chunk generation is O(layers).
pub struct FlatPipeline {
    pub min_y: i64,
    /// `(block, count)` pairs, stacked upward from `min_y`.
    pub layers: Vec<(BlockId, i64)>,
}

impl WorldGen for FlatPipeline {
    fn generate_chunk(&self, _cx: i32, _cz: i32) -> Chunk {
        let mut chunk = Chunk::new();
        for lx in 0..16u8 {
            for lz in 0..16u8 {
                let mut y = self.min_y;
                for &(block_id, count) in &self.layers {
                    for _ in 0..count {
                        chunk.set_block(LocalBlockPos { x: lx, y, z: lz }, block_id);
                        y += 1;
                    }
                }
            }
        }
        chunk
    }

    fn spawn_y(&self, _x: i64, _z: i64) -> f64 {
        let total_height: i64 = self.layers.iter().map(|(_, c)| c).sum();
        (self.min_y + total_height) as f64 + 0.001
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::density::DensityFnSchema;

    fn flat_density(height: i64) -> Arc<dyn DensityFunction> {
        DensityFnSchema::Sub {
            argument1: Box::new(DensityFnSchema::Constant { value: height as f64 }),
            argument2: Box::new(DensityFnSchema::YIndex),
        }.build(0)
    }

    #[test]
    fn density_pipeline_finds_constant_surface() {
        let pipe = DensityPipeline {
            density: flat_density(70),
            heightmap_shortcut: None,  // exercise the column-scan path
            sea_level: 63, min_y: -64, max_y: 319, bedrock_y: 0,
            dirt_depth: 4, beach_band: 2,
        };
        assert_eq!(pipe.surface_y(0, 0), 70);
        assert_eq!(pipe.surface_y(123, -456), 70);
    }

    #[test]
    fn density_pipeline_stratifies_correctly() {
        let pipe = DensityPipeline {
            density: flat_density(70),
            heightmap_shortcut: None,
            sea_level: 63, min_y: -64, max_y: 319, bedrock_y: 0,
            dirt_depth: 4, beach_band: 2,
        };
        let chunk = pipe.generate_chunk(0, 0);
        // y=0: bedrock
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 0, z: 8 }), block::BEDROCK);
        // y=1..65: stone (70 - 4 = 66 is dirt_bottom; stone is 1..65)
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 50, z: 8 }), block::STONE);
        // y=66..69: dirt skin (surface is 70 → grass; surface is above sea+beach, so skin = DIRT)
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 68, z: 8 }), block::DIRT);
        // y=70: grass (surface > sea + beach)
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 70, z: 8 }), block::GRASS_BLOCK);
        // y=71: air (no block written)
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 71, z: 8 }), BlockId::AIR);
    }

    #[test]
    fn density_pipeline_underwater_fills_with_water() {
        // Surface below sea level → water from surface+1 up to sea_level.
        let pipe = DensityPipeline {
            density: flat_density(50),
            heightmap_shortcut: None,
            sea_level: 63, min_y: -64, max_y: 319, bedrock_y: 0,
            dirt_depth: 4, beach_band: 2,
        };
        let chunk = pipe.generate_chunk(0, 0);
        assert_eq!(chunk.get_block(LocalBlockPos { x: 0, y: 51, z: 0 }), block::WATER);
        assert_eq!(chunk.get_block(LocalBlockPos { x: 0, y: 63, z: 0 }), block::WATER);
        // Above sea level: air.
        assert_eq!(chunk.get_block(LocalBlockPos { x: 0, y: 64, z: 0 }), BlockId::AIR);
    }

    #[test]
    fn heightmap_shortcut_matches_column_scan() {
        // Build the same density both ways: with and without the
        // shortcut. The surface_y output must agree.
        let h_schema = DensityFnSchema::Add {
            argument1: Box::new(DensityFnSchema::Constant { value: 75.0 }),
            argument2: Box::new(DensityFnSchema::Mul {
                argument1: Box::new(DensityFnSchema::Noise2d {
                    seed_offset: 0, frequency: 0.005,
                    octaves: 3, persistence: 0.5, lacunarity: 2.0,
                }),
                argument2: Box::new(DensityFnSchema::Constant { value: 20.0 }),
            }),
        };
        let full_schema = DensityFnSchema::Sub {
            argument1: Box::new(h_schema.clone()),
            argument2: Box::new(DensityFnSchema::YIndex),
        };

        let with_shortcut = DensityPipeline {
            density: full_schema.build(7),
            heightmap_shortcut: Some(h_schema.build(7)),
            sea_level: 63, min_y: -64, max_y: 319, bedrock_y: 0,
            dirt_depth: 4, beach_band: 2,
        };
        let without_shortcut = DensityPipeline {
            density: full_schema.build(7),
            heightmap_shortcut: None,
            sea_level: 63, min_y: -64, max_y: 319, bedrock_y: 0,
            dirt_depth: 4, beach_band: 2,
        };
        for x in -20..20i64 {
            for z in -20..20i64 {
                let a = with_shortcut.surface_y(x, z);
                let b = without_shortcut.surface_y(x, z);
                assert_eq!(a, b, "shortcut/scan disagree at ({},{})", x, z);
            }
        }
    }

    #[test]
    fn flat_pipeline_stacks_layers() {
        let pipe = FlatPipeline {
            min_y: 0,
            layers: vec![
                (block::BEDROCK, 1),
                (block::STONE, 5),
                (block::DIRT, 2),
                (block::GRASS_BLOCK, 1),
            ],
        };
        let chunk = pipe.generate_chunk(0, 0);
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 0, z: 8 }), block::BEDROCK);
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 1, z: 8 }), block::STONE);
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 5, z: 8 }), block::STONE);
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 6, z: 8 }), block::DIRT);
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 7, z: 8 }), block::DIRT);
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 8, z: 8 }), block::GRASS_BLOCK);
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 9, z: 8 }), BlockId::AIR);
    }
}
