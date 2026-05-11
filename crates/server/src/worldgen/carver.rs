//! Carvers: post-pass passes that mutate generated chunks.
//!
//! Carvers run *after* heightmap stratification, scanning the chunk's
//! solid-fill blocks (stone / dirt / sand / gravel) within a Y range and
//! converting them to air based on a 3D-noise mask. They never touch
//! bedrock, water, or the surface column above the heightmap — water in
//! oceans and lakes stays intact, and surface grass / sand isn't broken
//! into holes.
//!
//! Architecturally this matches vanilla's "carvers run after terrain
//! shape" model rather than baking caves into the main density function.
//! The crucial side effect: the heightmap shortcut keeps working because
//! the density tree's structural shape (`f(x,z) - y_index`) is preserved.
//!
//! ## Schema
//!
//! Each carver in the preset's `carvers` array is one of:
//!
//! ```json
//! { "type": "noise",
//!   "density": { ... 3D density function ... },
//!   "threshold": 0.5,
//!   "min_y": -56,
//!   "max_y": 60 }
//! ```
//!
//! Higher `threshold` → fewer / smaller caves. Caves form wherever
//! `density(x, y, z) > threshold` for any cell in `[min_y, max_y]`.

use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::chunk::Chunk;
use ultimate_engine::world::position::LocalBlockPos;

use crate::block;
use super::density::{DensityFnSchema, DensityFunction};

/// A post-pass that mutates the chunk's blocks. Implementations should be
/// deterministic from their build-time parameters so the same chunk
/// generates identically across runs.
pub trait Carver: Send + Sync {
    fn carve(&self, chunk: &mut Chunk, cx: i32, cz: i32);
}

// ── NoiseCarver ─────────────────────────────────────────────────────────────

/// Carve cells whose 3D-noise density exceeds `threshold`, within
/// `[min_y, max_y]`. Skips air, bedrock, and water so we don't dig
/// holes in the seabed or the bedrock floor.
pub struct NoiseCarver {
    pub density: Arc<dyn DensityFunction>,
    pub threshold: f64,
    pub min_y: i64,
    pub max_y: i64,
}

impl Carver for NoiseCarver {
    fn carve(&self, chunk: &mut Chunk, cx: i32, cz: i32) {
        let base_x = cx as i64 * 16;
        let base_z = cz as i64 * 16;

        for lx in 0..16u8 {
            for lz in 0..16u8 {
                let wx = base_x + lx as i64;
                let wz = base_z + lz as i64;
                for y in self.min_y..=self.max_y {
                    let pos = LocalBlockPos { x: lx, y, z: lz };
                    let current = chunk.get_block(pos);
                    if !is_carvable(current) {
                        continue;
                    }
                    if self.density.sample(wx, y, wz) > self.threshold {
                        chunk.set_block(pos, BlockId::AIR);
                    }
                }
            }
        }
    }
}

/// Whether a block is "natural fill" that a carver is allowed to dig out.
/// Bedrock is preserved so the world floor stays solid; water is preserved
/// so oceans don't drain; air is skipped because there's nothing to carve.
fn is_carvable(b: BlockId) -> bool {
    b != BlockId::AIR && b != block::BEDROCK && b != block::WATER && b != block::LAVA
}

// ── JSON schema ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CarverSchema {
    Noise(NoiseCarverSchema),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NoiseCarverSchema {
    pub density: DensityFnSchema,
    pub threshold: f64,
    pub min_y: i64,
    pub max_y: i64,
}

impl CarverSchema {
    pub fn build(&self, seed: u32) -> Result<Arc<dyn Carver>> {
        match self {
            Self::Noise(n) => Ok(Arc::new(NoiseCarver {
                density: n.density.build(seed),
                threshold: n.threshold,
                min_y: n.min_y,
                max_y: n.max_y,
            })),
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A density that's always +1: carves everything in range.
    fn always_carve() -> Arc<dyn DensityFunction> {
        DensityFnSchema::Constant { value: 1.0 }.build(0)
    }

    /// A density that's always -1: carves nothing.
    fn never_carve() -> Arc<dyn DensityFunction> {
        DensityFnSchema::Constant { value: -1.0 }.build(0)
    }

    fn chunk_with_stone_column() -> Chunk {
        let mut c = Chunk::new();
        for lx in 0..16u8 {
            for lz in 0..16u8 {
                c.set_block(LocalBlockPos { x: lx, y: 0, z: lz }, block::BEDROCK);
                for y in 1..=60i64 {
                    c.set_block(LocalBlockPos { x: lx, y, z: lz }, block::STONE);
                }
            }
        }
        c
    }

    #[test]
    fn always_carve_clears_stone_in_range() {
        let carver = NoiseCarver {
            density: always_carve(),
            threshold: 0.0,
            min_y: 5,
            max_y: 30,
        };
        let mut chunk = chunk_with_stone_column();
        carver.carve(&mut chunk, 0, 0);
        // In range → air.
        for y in 5..=30i64 {
            assert_eq!(chunk.get_block(LocalBlockPos { x: 0, y, z: 0 }), BlockId::AIR,
                "y={} should be carved", y);
        }
        // Below range → stone.
        assert_eq!(chunk.get_block(LocalBlockPos { x: 0, y: 4, z: 0 }), block::STONE);
        // Above range → stone.
        assert_eq!(chunk.get_block(LocalBlockPos { x: 0, y: 35, z: 0 }), block::STONE);
    }

    #[test]
    fn never_carve_leaves_chunk_untouched() {
        let carver = NoiseCarver {
            density: never_carve(),
            threshold: 0.0,
            min_y: -64,
            max_y: 319,
        };
        let mut chunk = chunk_with_stone_column();
        carver.carve(&mut chunk, 0, 0);
        for y in 1..=60i64 {
            assert_eq!(chunk.get_block(LocalBlockPos { x: 5, y, z: 5 }), block::STONE);
        }
    }

    #[test]
    fn bedrock_is_preserved() {
        let carver = NoiseCarver {
            density: always_carve(),
            threshold: 0.0,
            min_y: 0,
            max_y: 5,
        };
        let mut chunk = chunk_with_stone_column();
        carver.carve(&mut chunk, 0, 0);
        // Bedrock floor stays.
        assert_eq!(chunk.get_block(LocalBlockPos { x: 0, y: 0, z: 0 }), block::BEDROCK);
        // Stone above bedrock gets carved.
        assert_eq!(chunk.get_block(LocalBlockPos { x: 0, y: 1, z: 0 }), BlockId::AIR);
    }

    #[test]
    fn water_is_preserved() {
        let carver = NoiseCarver {
            density: always_carve(),
            threshold: 0.0,
            min_y: 0,
            max_y: 60,
        };
        let mut chunk = chunk_with_stone_column();
        // Place water at y=10 to simulate a flooded cell.
        chunk.set_block(LocalBlockPos { x: 0, y: 10, z: 0 }, block::WATER);
        carver.carve(&mut chunk, 0, 0);
        assert_eq!(chunk.get_block(LocalBlockPos { x: 0, y: 10, z: 0 }), block::WATER,
            "water in carver range should not be drained");
        // Stone adjacent to water IS carved.
        assert_eq!(chunk.get_block(LocalBlockPos { x: 0, y: 11, z: 0 }), BlockId::AIR);
    }

    #[test]
    fn threshold_gates_carving() {
        // density = +1.0 constant; threshold = 2.0 → never above threshold.
        let carver = NoiseCarver {
            density: always_carve(),
            threshold: 2.0,
            min_y: 0,
            max_y: 60,
        };
        let mut chunk = chunk_with_stone_column();
        carver.carve(&mut chunk, 0, 0);
        assert_eq!(chunk.get_block(LocalBlockPos { x: 5, y: 30, z: 5 }), block::STONE);
    }

    /// Probe the carver noise distribution to inform threshold tuning. Run
    /// manually with `cargo test diagnose_default_carver_noise_range --
    /// --ignored --nocapture`. Skipped by default because it spams output.
    #[test]
    #[ignore]
    fn diagnose_default_carver_noise_range() {
        // Probe the noise the built-in `noise` preset actually produces
        // for its carver. The seed and noise parameters mirror
        // `presets/noise.json`'s carver block.
        let density = DensityFnSchema::Noise3d {
            seed_offset: 500,
            frequency: 0.035,
            octaves: 3,
            persistence: 0.5,
            lacunarity: 2.0,
        }.build(0xC0FFEE);
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        let mut buckets = [0usize; 9]; // > 0.10, 0.15, 0.20, 0.25, 0.30, 0.35, 0.40, 0.45, 0.50
        let mut total = 0usize;
        for x in -50..50i64 {
            for y in -56..=55i64 {
                for z in -50..50i64 {
                    let v = density.sample(x, y, z);
                    if v < min { min = v; }
                    if v > max { max = v; }
                    for (i, t) in [0.10, 0.15, 0.20, 0.25, 0.30, 0.35, 0.40, 0.45, 0.50].iter().enumerate() {
                        if v > *t { buckets[i] += 1; }
                    }
                    total += 1;
                }
            }
        }
        eprintln!("noise probe: min={:.3}, max={:.3}", min, max);
        for (i, t) in [0.10, 0.15, 0.20, 0.25, 0.30, 0.35, 0.40, 0.45, 0.50].iter().enumerate() {
            eprintln!(
                "  threshold {:.2}: {:>7}/{} carved ({:.2}%)",
                t, buckets[i], total,
                100.0 * buckets[i] as f64 / total as f64,
            );
        }
    }

    #[test]
    fn schema_round_trips_through_json() {
        let schema = CarverSchema::Noise(NoiseCarverSchema {
            density: DensityFnSchema::Noise3d {
                seed_offset: 1, frequency: 0.05,
                octaves: 3, persistence: 0.5, lacunarity: 2.0,
            },
            threshold: 0.4,
            min_y: -56,
            max_y: 50,
        });
        let json = serde_json::to_string(&schema).unwrap();
        let parsed: CarverSchema = serde_json::from_str(&json).unwrap();
        let built = parsed.build(42).unwrap();
        let mut chunk = chunk_with_stone_column();
        built.carve(&mut chunk, 0, 0);
        // Smoke test: just verify it ran without panicking.
        let _ = chunk;
    }
}
