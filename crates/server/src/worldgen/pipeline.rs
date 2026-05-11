//! Pipelines compose a density function, biome source, and surface rule
//! into a concrete [`WorldGen`].
//!
//! - [`DensityPipeline`] — walks each column for the surface Y, asks the
//!   [`BiomeSource`] for the column's biome, then walks the surface band
//!   top-down letting the [`SurfaceRule`] tree choose every block.
//! - [`FlatPipeline`] — superflat preset: bedrock floor + a stack of fixed
//!   layers per column, with a single biome. No noise sampling.

use std::sync::Arc;

use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::chunk::Chunk;
use ultimate_engine::world::position::LocalBlockPos;

use crate::block;
use super::WorldGen;
use super::biome::Biome;
use super::carver::Carver;
use super::climate::BiomeSource;
use super::decorator::{Decorator, DecorationContext};
use super::density::DensityFunction;
use super::surface::{SurfaceContext, SurfaceRule};

/// Density-function pipeline with composable biomes + surface rules.
///
/// Generation per chunk:
/// 1. For each column `(x, z)`: find `surface_y` from the density function
///    (heightmap shortcut when available, otherwise a column scan).
/// 2. Ask `biome_source` for the column's biome.
/// 3. Walk `bedrock_y..=surface_y`, placing:
///    - `BEDROCK` at `bedrock_y`,
///    - stone bulk up to `surface_y - skin_depth`,
///    - `surface_rule.try_apply(...)` from the skin band up through the
///      surface block (falls back to stone if no rule fires),
///    - water from `surface_y + 1` up to `sea_level` for submerged columns.
pub struct DensityPipeline {
    pub density: Arc<dyn DensityFunction>,
    /// If the preset's density was structurally `height(x,z) - y_index`
    /// with `height` y-independent, this holds the compiled height field.
    /// `surface_y` then samples it once per column instead of walking up
    /// to ~384 y values.
    pub heightmap_shortcut: Option<Arc<dyn DensityFunction>>,
    pub biome_source: Arc<dyn BiomeSource>,
    pub surface_rule: Arc<dyn SurfaceRule>,
    /// Carvers run in order *after* the heightmap stratification, each
    /// mutating the chunk in place. The most common kind is `NoiseCarver`
    /// (a 3D-noise mask + threshold).
    pub carvers: Vec<Arc<dyn Carver>>,
    /// Decorators run after carvers, scattering features (ores, plants,
    /// trees, structures) deterministically per-chunk.
    pub decorators: Vec<Arc<dyn Decorator>>,
    /// World seed, forwarded to decorators so their per-chunk PRNGs
    /// derive from `(seed, cx, cz, decorator_index)`.
    pub seed: u32,
    pub sea_level: i64,
    pub min_y: i64,
    pub max_y: i64,
    pub bedrock_y: i64,
    /// Depth of the surface band (skin) over which the `surface_rule` runs.
    /// Below this is stone; above the surface is water (or air) regardless
    /// of rule.
    pub skin_depth: i64,
}

impl DensityPipeline {
    /// Find the surface Y for column `(x, z)`. See struct doc for paths.
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

        // Pre-compute the surface-Y grid for all 256 columns. Used by
        // stratification AND handed to decorators so they can sample
        // biomes per column without rerunning the density function.
        let mut surface_grid = [0i64; 256];
        for lz in 0..16u8 {
            for lx in 0..16u8 {
                let wx = base_x + lx as i64;
                let wz = base_z + lz as i64;
                surface_grid[lz as usize * 16 + lx as usize] = self.surface_y(wx, wz);
            }
        }

        for lz in 0..16u8 {
            for lx in 0..16u8 {
                let wx = base_x + lx as i64;
                let wz = base_z + lz as i64;
                let surface = surface_grid[lz as usize * 16 + lx as usize];
                let biome = self.biome_source.sample(wx, wz, surface, self.sea_level);

                // Bedrock floor.
                chunk.set_block(
                    LocalBlockPos { x: lx, y: self.bedrock_y, z: lz },
                    block::BEDROCK,
                );

                // Stone bulk up to where the surface skin starts.
                let skin_bottom = (surface - self.skin_depth).max(self.bedrock_y + 1);
                for y in (self.bedrock_y + 1)..skin_bottom {
                    chunk.set_block(LocalBlockPos { x: lx, y, z: lz }, block::STONE);
                }

                // Surface skin: walk through the rule tree. Anything the
                // rule declines to place falls back to stone (a sensible
                // "nothing more specific" default for sub-surface fill).
                for y in skin_bottom..=surface {
                    let ctx = SurfaceContext {
                        biome, x: wx, y, z: wz, surface_y: surface, sea_level: self.sea_level,
                    };
                    let b = self.surface_rule.try_apply(&ctx).unwrap_or(block::STONE);
                    chunk.set_block(LocalBlockPos { x: lx, y, z: lz }, b);
                }

                // Water from surface+1 up to sea level (submerged columns).
                if surface < self.sea_level {
                    for y in (surface + 1)..=self.sea_level {
                        chunk.set_block(LocalBlockPos { x: lx, y, z: lz }, block::WATER);
                    }
                }
            }
        }

        // Carve passes (caves, ravines, ...). Each carver mutates the chunk
        // in place, skipping bedrock / water / air so we never break the
        // world floor or drain the oceans.
        for carver in &self.carvers {
            carver.carve(&mut chunk, cx, cz);
        }

        // Decoration passes (ores, plants, trees, structures). Each
        // decorator's PRNG is seeded from `(seed, cx, cz, idx)` and
        // gets access to the biome source + surface grid so it can
        // filter by biome.
        for (idx, decorator) in self.decorators.iter().enumerate() {
            let mut ctx = DecorationContext {
                chunk: &mut chunk,
                cx, cz,
                seed: self.seed,
                decorator_index: idx,
                biome_source: &*self.biome_source,
                sea_level: self.sea_level,
                surface_y: &surface_grid,
            };
            decorator.decorate(&mut ctx);
        }

        chunk
    }

    fn spawn_y(&self, x: i64, z: i64) -> f64 {
        let surface = self.surface_y(x, z);
        (surface.max(self.sea_level) + 1) as f64 + 0.001
    }

    fn biome_at(&self, cx: i32, cz: i32) -> u32 {
        // Convenience: sample at the centre column. The chunk packet
        // doesn't actually use this — it goes through `biome_at_cell`
        // for per-(4×4×4) granularity — but it satisfies the
        // single-biome convenience accessor.
        let wx = cx as i64 * 16 + 8;
        let wz = cz as i64 * 16 + 8;
        let surface = self.surface_y(wx, wz);
        self.biome_source.sample(wx, wz, surface, self.sea_level).registry_id()
    }

    fn biome_at_cell(&self, x: i64, _y: i64, z: i64) -> u32 {
        // Biome assignment depends on (x, z) and the column's surface_y,
        // not directly on y, so the sample column anchors the result.
        let surface = self.surface_y(x, z);
        self.biome_source.sample(x, z, surface, self.sea_level).registry_id()
    }
}

/// Superflat pipeline: bedrock + a fixed stack of layers per column.
/// Identical across all (x, z), so chunk generation is O(layers).
pub struct FlatPipeline {
    pub min_y: i64,
    /// `(block, count)` pairs, stacked upward from `min_y`.
    pub layers: Vec<(BlockId, i64)>,
    pub biome: Biome,
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

    fn biome_at(&self, _cx: i32, _cz: i32) -> u32 {
        self.biome.registry_id()
    }

    fn biome_at_cell(&self, _x: i64, _y: i64, _z: i64) -> u32 {
        self.biome.registry_id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::climate::FixedBiomeSource;
    use super::super::density::DensityFnSchema;
    use super::super::surface::SurfaceRuleSchema;

    fn flat_density(height: i64) -> Arc<dyn DensityFunction> {
        DensityFnSchema::Sub {
            argument1: Box::new(DensityFnSchema::Constant { value: height as f64 }),
            argument2: Box::new(DensityFnSchema::YIndex),
        }.build(0)
    }

    /// Surface rule: grass on top, dirt in skin, fall through (stone).
    fn vanilla_ish_rule() -> Arc<dyn SurfaceRule> {
        SurfaceRuleSchema::Sequence {
            rules: vec![
                SurfaceRuleSchema::Condition {
                    condition: super::super::surface::ConditionSchema::AtSurface,
                    rule: Box::new(SurfaceRuleSchema::Block { block: "minecraft:grass_block".into() }),
                },
                SurfaceRuleSchema::Condition {
                    condition: super::super::surface::ConditionSchema::DepthAtMost { depth: 4 },
                    rule: Box::new(SurfaceRuleSchema::Block { block: "minecraft:dirt".into() }),
                },
            ],
        }.build().unwrap()
    }

    fn pipe_with(density: Arc<dyn DensityFunction>) -> DensityPipeline {
        DensityPipeline {
            density,
            heightmap_shortcut: None,
            biome_source: Arc::new(FixedBiomeSource(Biome::Plains)),
            surface_rule: vanilla_ish_rule(),
            carvers: Vec::new(),
            decorators: Vec::new(),
            seed: 0,
            sea_level: 63, min_y: -64, max_y: 319, bedrock_y: 0,
            skin_depth: 4,
        }
    }

    #[test]
    fn density_pipeline_finds_constant_surface() {
        let pipe = pipe_with(flat_density(70));
        assert_eq!(pipe.surface_y(0, 0), 70);
        assert_eq!(pipe.surface_y(123, -456), 70);
    }

    #[test]
    fn density_pipeline_stratifies_via_surface_rule() {
        let pipe = pipe_with(flat_density(70));
        let chunk = pipe.generate_chunk(0, 0);
        // y=0: bedrock floor
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 0, z: 8 }), block::BEDROCK);
        // y=50: deep stone
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 50, z: 8 }), block::STONE);
        // y=68: skin band — the DepthAtMost(4) rule fires → dirt
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 68, z: 8 }), block::DIRT);
        // y=70: AtSurface rule fires → grass
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 70, z: 8 }), block::GRASS_BLOCK);
        // y=71: above surface, no block placed
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 71, z: 8 }), BlockId::AIR);
    }

    #[test]
    fn density_pipeline_underwater_fills_with_water() {
        let pipe = pipe_with(flat_density(50));
        let chunk = pipe.generate_chunk(0, 0);
        assert_eq!(chunk.get_block(LocalBlockPos { x: 0, y: 51, z: 0 }), block::WATER);
        assert_eq!(chunk.get_block(LocalBlockPos { x: 0, y: 63, z: 0 }), block::WATER);
        // Above sea level: air.
        assert_eq!(chunk.get_block(LocalBlockPos { x: 0, y: 64, z: 0 }), BlockId::AIR);
    }

    #[test]
    fn heightmap_shortcut_matches_column_scan() {
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

        let mut with_shortcut = pipe_with(full_schema.build(7));
        with_shortcut.heightmap_shortcut = Some(h_schema.build(7));
        let without_shortcut = pipe_with(full_schema.build(7));

        for x in -20..20i64 {
            for z in -20..20i64 {
                let a = with_shortcut.surface_y(x, z);
                let b = without_shortcut.surface_y(x, z);
                assert_eq!(a, b, "shortcut/scan disagree at ({},{})", x, z);
            }
        }
    }

    #[test]
    fn biome_at_uses_biome_source() {
        let mut pipe = pipe_with(flat_density(70));
        pipe.biome_source = Arc::new(FixedBiomeSource(Biome::Desert));
        assert_eq!(pipe.biome_at(0, 0), Biome::Desert.registry_id());
    }

    #[test]
    fn density_pipeline_runs_carvers_after_stratification() {
        use super::super::carver::NoiseCarver;

        let mut pipe = pipe_with(flat_density(70));
        // Carver that carves everything between y=10 and y=30.
        let always = DensityFnSchema::Constant { value: 1.0 }.build(0);
        pipe.carvers = vec![Arc::new(NoiseCarver {
            density: always,
            threshold: 0.0,
            min_y: 10,
            max_y: 30,
        })];

        let chunk = pipe.generate_chunk(0, 0);
        // y=0: bedrock survives carving (carvers skip bedrock).
        assert_eq!(chunk.get_block(LocalBlockPos { x: 0, y: 0, z: 0 }), block::BEDROCK);
        // y=5: below carver range, stone stays.
        assert_eq!(chunk.get_block(LocalBlockPos { x: 0, y: 5, z: 0 }), block::STONE);
        // y=15: within carver range AND stratified as stone → carved to air.
        assert_eq!(chunk.get_block(LocalBlockPos { x: 0, y: 15, z: 0 }), BlockId::AIR);
        // y=50: above carver range, stone stays.
        assert_eq!(chunk.get_block(LocalBlockPos { x: 0, y: 50, z: 0 }), block::STONE);
        // y=70: still grass at the surface (carver range doesn't reach here).
        assert_eq!(chunk.get_block(LocalBlockPos { x: 0, y: 70, z: 0 }), block::GRASS_BLOCK);
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
            biome: Biome::Plains,
        };
        let chunk = pipe.generate_chunk(0, 0);
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 0, z: 8 }), block::BEDROCK);
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 1, z: 8 }), block::STONE);
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 5, z: 8 }), block::STONE);
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 6, z: 8 }), block::DIRT);
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 7, z: 8 }), block::DIRT);
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 8, z: 8 }), block::GRASS_BLOCK);
        assert_eq!(chunk.get_block(LocalBlockPos { x: 8, y: 9, z: 8 }), BlockId::AIR);
        assert_eq!(pipe.biome_at(0, 0), Biome::Plains.registry_id());
    }
}
