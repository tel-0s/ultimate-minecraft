//! Decorators: post-carver passes that *add* features to a chunk.
//!
//! Carvers remove blocks (caves); decorators add or replace them (ores,
//! plants, trees, structures). Each decorator owns a small deterministic
//! PRNG seeded from `(world_seed, cx, cz, decorator_index)` so the same
//! chunk produces identical features across runs.
//!
//! Stage 4d ships [`OreDecorator`]: a random number of vein attempts per
//! chunk, each attempt growing a short random-walk vein that replaces a
//! substrate block (stone) with an ore. Trees and plants will be additional
//! `Decorator` impls in 4e.
//!
//! ## Schema
//!
//! ```json
//! { "type": "ore",
//!   "block":  "minecraft:coal_ore",
//!   "replaces": ["minecraft:stone"],
//!   "attempts_per_chunk": 20,
//!   "vein_size": 8,
//!   "min_y": 0,
//!   "max_y": 100 }
//! ```

use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::chunk::Chunk;
use ultimate_engine::world::position::LocalBlockPos;

use crate::block;

/// A post-pass that mutates a chunk to place features. Implementations
/// must be deterministic given the same `seed`, `cx`, `cz`, and any
/// `decorator_index` baked into the seed they build.
pub trait Decorator: Send + Sync {
    fn decorate(&self, chunk: &mut Chunk, cx: i32, cz: i32, seed: u32, decorator_index: usize);
}

// ── Deterministic PRNG ──────────────────────────────────────────────────────

/// SplitMix64: small, fast, deterministic state-of-1 PRNG. Enough for
/// scattering ore positions; *not* cryptographic and not statistically
/// rigorous beyond "looks random for worldgen".
pub struct SplitMix64(u64);

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    /// Uniform integer in `[0, max)`. Slightly biased for very large
    /// `max` (modulo bias), negligible for worldgen ranges (<2^32).
    pub fn range_u32(&mut self, max: u32) -> u32 {
        if max == 0 { return 0; }
        (self.next_u64() % max as u64) as u32
    }

    /// Inclusive `[lo, hi]` integer.
    pub fn range_i64(&mut self, lo: i64, hi: i64) -> i64 {
        if hi <= lo { return lo; }
        let span = (hi - lo + 1) as u64;
        lo + (self.next_u64() % span) as i64
    }
}

/// Mix four `u32`s into one `u64` seed for [`SplitMix64`].
pub fn chunk_decorator_seed(world_seed: u32, cx: i32, cz: i32, decorator_index: usize) -> u64 {
    let a = world_seed as u64;
    let b = (cx as i64 as u64).wrapping_mul(0x9E3779B97F4A7C15);
    let c = (cz as i64 as u64).wrapping_mul(0xBF58476D1CE4E5B9);
    let d = (decorator_index as u64).wrapping_mul(0x94D049BB133111EB);
    a ^ b ^ c ^ d
}

// ── OreDecorator ────────────────────────────────────────────────────────────

/// Scatters ore veins through a chunk. For each of `attempts_per_chunk`
/// attempts, picks a random `(x, y, z)` within `[min_y, max_y]` and grows
/// a `vein_size`-block random-walk vein, replacing cells whose current
/// block is in `replaces` (typically just stone).
///
/// Cells outside the chunk's `(x, z)` extent are clipped — veins don't
/// span chunk borders. Cells outside `[min_y, max_y]` are skipped during
/// the walk so the vein "bumps" off the y-band edges.
pub struct OreDecorator {
    pub block: BlockId,
    pub replaces: Vec<BlockId>,
    pub attempts_per_chunk: u32,
    pub vein_size: u32,
    pub min_y: i64,
    pub max_y: i64,
}

impl Decorator for OreDecorator {
    fn decorate(&self, chunk: &mut Chunk, cx: i32, cz: i32, seed: u32, decorator_index: usize) {
        let mut rng = SplitMix64::new(chunk_decorator_seed(seed, cx, cz, decorator_index));

        for _ in 0..self.attempts_per_chunk {
            let mut x = rng.range_u32(16) as u8;
            let mut z = rng.range_u32(16) as u8;
            let mut y = rng.range_i64(self.min_y, self.max_y);

            for _ in 0..self.vein_size {
                let pos = LocalBlockPos { x, y, z };
                let current = chunk.get_block(pos);
                if self.replaces.contains(&current) {
                    chunk.set_block(pos, self.block);
                }

                // Random walk one of the 6 cardinal directions.
                match rng.range_u32(6) {
                    0 => x = (x + 1).min(15),
                    1 => x = x.saturating_sub(1),
                    2 => z = (z + 1).min(15),
                    3 => z = z.saturating_sub(1),
                    4 => y = (y + 1).min(self.max_y),
                    _ => y = (y - 1).max(self.min_y),
                }
            }
        }
    }
}

// ── TreeDecorator ───────────────────────────────────────────────────────────

/// Plants trees on top of the configured `surface_block` (typically
/// `grass_block`, which the surface rule places in temperate biomes).
/// Each tree is one column of `log` blocks (height in `[trunk_min, trunk_max]`)
/// with a three-layer canopy of `leaves`: 5×5 bottom, 5×5-minus-corners middle,
/// 3×3 top. Canopy cells only overwrite air, so the trunk pokes through.
///
/// **Chunk clipping:** the canopy and trunk are clipped to the chunk
/// being decorated. Trees near a chunk border lose part of their canopy
/// in the neighbour — a known visual quirk that gets fixed when the
/// decorator framework gains cross-chunk deferred writes (Stage 4e+).
///
/// The `surface_block` filter is a poor-but-effective biome proxy: our
/// surface rules paint grass on plains / forest, sand on desert / beach,
/// snow on snowy_plains. Filtering to `grass_block` keeps trees out of
/// deserts and tundras for free.
pub struct TreeDecorator {
    pub log_block: BlockId,
    pub leaves_block: BlockId,
    pub surface_block: BlockId,
    pub attempts_per_chunk: u32,
    pub trunk_min: u32,
    pub trunk_max: u32,
    pub min_y: i64,
    pub max_y: i64,
}

impl Decorator for TreeDecorator {
    fn decorate(&self, chunk: &mut Chunk, cx: i32, cz: i32, seed: u32, decorator_index: usize) {
        let mut rng = SplitMix64::new(chunk_decorator_seed(seed, cx, cz, decorator_index));

        for _ in 0..self.attempts_per_chunk {
            let lx = rng.range_u32(16) as u8;
            let lz = rng.range_u32(16) as u8;

            // Find the topmost non-air block in this column inside the Y band.
            let surface_y = (self.min_y..=self.max_y).rev().find(|&y| {
                chunk.get_block(LocalBlockPos { x: lx, y, z: lz }) != BlockId::AIR
            });
            let Some(sy) = surface_y else { continue };

            // Biome proxy: only grow on the configured ground block.
            if chunk.get_block(LocalBlockPos { x: lx, y: sy, z: lz }) != self.surface_block {
                continue;
            }

            // Pick trunk height. Bail if the full tree wouldn't fit in band.
            let trunk_h = self.trunk_min
                + rng.range_u32(self.trunk_max - self.trunk_min + 1);
            let canopy_top_y = sy + trunk_h as i64 + 1;
            if canopy_top_y >= self.max_y {
                continue;
            }

            // Trunk: logs from sy+1 up through sy+trunk_h. The top log is
            // covered by the canopy's middle layer, which preserves logs.
            for dy in 1..=(trunk_h as i64) {
                chunk.set_block(
                    LocalBlockPos { x: lx, y: sy + dy, z: lz },
                    self.log_block,
                );
            }

            // Canopy.
            let trunk_top_y = sy + trunk_h as i64;
            place_canopy_layer(chunk, lx, trunk_top_y - 1, lz, 2, self.leaves_block, false);
            place_canopy_layer(chunk, lx, trunk_top_y,     lz, 2, self.leaves_block, true);
            place_canopy_layer(chunk, lx, trunk_top_y + 1, lz, 1, self.leaves_block, false);
        }
    }
}

/// Place a single layer of canopy leaves: a square of side `2*radius+1`
/// centred on `(x, y, z)`, optionally skipping the four corner cells (for
/// the diamond-ish vanilla canopy shape). Only air cells are overwritten,
/// so the trunk and surrounding terrain stay put.
fn place_canopy_layer(
    chunk: &mut Chunk,
    x: u8, y: i64, z: u8,
    radius: i32,
    leaves: BlockId,
    skip_corners: bool,
) {
    for dx in -radius..=radius {
        for dz in -radius..=radius {
            if skip_corners && dx.abs() == radius && dz.abs() == radius {
                continue;
            }
            let nx = x as i32 + dx;
            let nz = z as i32 + dz;
            if !(0..16).contains(&nx) || !(0..16).contains(&nz) {
                continue; // clip to chunk
            }
            let pos = LocalBlockPos { x: nx as u8, y, z: nz as u8 };
            if chunk.get_block(pos) == BlockId::AIR {
                chunk.set_block(pos, leaves);
            }
        }
    }
}

// ── JSON schema ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DecoratorSchema {
    Ore(OreDecoratorSchema),
    Tree(TreeDecoratorSchema),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OreDecoratorSchema {
    pub block: String,
    pub replaces: Vec<String>,
    pub attempts_per_chunk: u32,
    #[serde(default = "default_vein_size")]
    pub vein_size: u32,
    pub min_y: i64,
    pub max_y: i64,
}

fn default_vein_size() -> u32 { 8 }

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TreeDecoratorSchema {
    pub log_block: String,
    pub leaves_block: String,
    /// Only place trees on top of this block (e.g. `"minecraft:grass_block"`).
    /// Acts as a biome proxy: surface rules paint grass / sand / snow per
    /// biome, so filtering by surface block keeps trees out of deserts
    /// and tundras automatically.
    pub surface_block: String,
    pub attempts_per_chunk: u32,
    pub trunk_min: u32,
    pub trunk_max: u32,
    pub min_y: i64,
    pub max_y: i64,
}

impl DecoratorSchema {
    pub fn build(&self) -> Result<Arc<dyn Decorator>> {
        match self {
            Self::Ore(o) => {
                let block = block::block_id_from_name(&o.block)
                    .ok_or_else(|| anyhow!("unknown ore block {:?}", o.block))?;
                let replaces = o.replaces.iter()
                    .map(|name| block::block_id_from_name(name)
                        .ok_or_else(|| anyhow!("unknown replace target {:?}", name)))
                    .collect::<Result<Vec<_>>>()?;
                Ok(Arc::new(OreDecorator {
                    block,
                    replaces,
                    attempts_per_chunk: o.attempts_per_chunk,
                    vein_size: o.vein_size,
                    min_y: o.min_y,
                    max_y: o.max_y,
                }))
            }
            Self::Tree(t) => {
                let log_block = block::block_id_from_name(&t.log_block)
                    .ok_or_else(|| anyhow!("unknown tree log block {:?}", t.log_block))?;
                let leaves_block = block::block_id_from_name(&t.leaves_block)
                    .ok_or_else(|| anyhow!("unknown tree leaves block {:?}", t.leaves_block))?;
                let surface_block = block::block_id_from_name(&t.surface_block)
                    .ok_or_else(|| anyhow!("unknown tree surface block {:?}", t.surface_block))?;
                if t.trunk_max < t.trunk_min {
                    return Err(anyhow!("tree decorator: trunk_max < trunk_min"));
                }
                Ok(Arc::new(TreeDecorator {
                    log_block,
                    leaves_block,
                    surface_block,
                    attempts_per_chunk: t.attempts_per_chunk,
                    trunk_min: t.trunk_min,
                    trunk_max: t.trunk_max,
                    min_y: t.min_y,
                    max_y: t.max_y,
                }))
            }
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn stone_chunk() -> Chunk {
        let mut c = Chunk::new();
        for lx in 0..16u8 {
            for lz in 0..16u8 {
                for y in 0..=100i64 {
                    c.set_block(LocalBlockPos { x: lx, y, z: lz }, block::STONE);
                }
            }
        }
        c
    }

    fn count_block(chunk: &Chunk, block: BlockId, y_range: std::ops::RangeInclusive<i64>) -> usize {
        let mut count = 0;
        for lx in 0..16u8 {
            for lz in 0..16u8 {
                for y in y_range.clone() {
                    if chunk.get_block(LocalBlockPos { x: lx, y, z: lz }) == block {
                        count += 1;
                    }
                }
            }
        }
        count
    }

    fn coal_ore() -> BlockId {
        block::block_id_from_name("minecraft:coal_ore").expect("coal_ore must resolve")
    }

    #[test]
    fn ore_decorator_places_ore() {
        let dec = OreDecorator {
            block: coal_ore(),
            replaces: vec![block::STONE],
            attempts_per_chunk: 20,
            vein_size: 8,
            min_y: 0,
            max_y: 100,
        };
        let mut chunk = stone_chunk();
        dec.decorate(&mut chunk, 0, 0, 0xC0FFEE, 0);
        let n = count_block(&chunk, coal_ore(), 0..=100);
        // 20 attempts × 8 vein steps with self-overlap and OOB-bumps
        // typically lands ~80-120 ore blocks. Sanity-check a wide band.
        assert!(n > 20 && n < 200, "expected ~80-120 ore, got {}", n);
    }

    #[test]
    fn ore_decorator_respects_y_range() {
        let dec = OreDecorator {
            block: coal_ore(),
            replaces: vec![block::STONE],
            attempts_per_chunk: 30,
            vein_size: 8,
            min_y: 20,
            max_y: 40,
        };
        let mut chunk = stone_chunk();
        dec.decorate(&mut chunk, 0, 0, 42, 0);
        // No ore should appear outside [20, 40].
        assert_eq!(count_block(&chunk, coal_ore(), 0..=19), 0);
        assert_eq!(count_block(&chunk, coal_ore(), 41..=100), 0);
        // Plenty inside.
        assert!(count_block(&chunk, coal_ore(), 20..=40) > 0);
    }

    #[test]
    fn ore_decorator_only_replaces_listed_blocks() {
        let dec = OreDecorator {
            block: coal_ore(),
            replaces: vec![block::DIRT], // only dirt, but chunk is all stone
            attempts_per_chunk: 50,
            vein_size: 8,
            min_y: 0,
            max_y: 100,
        };
        let mut chunk = stone_chunk();
        dec.decorate(&mut chunk, 0, 0, 0, 0);
        assert_eq!(count_block(&chunk, coal_ore(), 0..=100), 0);
    }

    #[test]
    fn ore_decorator_is_deterministic_per_seed() {
        let dec = OreDecorator {
            block: coal_ore(),
            replaces: vec![block::STONE],
            attempts_per_chunk: 20,
            vein_size: 8,
            min_y: 0,
            max_y: 100,
        };
        let mut c1 = stone_chunk();
        let mut c2 = stone_chunk();
        dec.decorate(&mut c1, 3, 7, 0xC0FFEE, 0);
        dec.decorate(&mut c2, 3, 7, 0xC0FFEE, 0);
        for lx in 0..16u8 {
            for lz in 0..16u8 {
                for y in 0..=100i64 {
                    let pos = LocalBlockPos { x: lx, y, z: lz };
                    assert_eq!(c1.get_block(pos), c2.get_block(pos));
                }
            }
        }
    }

    #[test]
    fn different_chunks_get_different_veins() {
        let dec = OreDecorator {
            block: coal_ore(),
            replaces: vec![block::STONE],
            attempts_per_chunk: 20,
            vein_size: 8,
            min_y: 0,
            max_y: 100,
        };
        let mut c1 = stone_chunk();
        let mut c2 = stone_chunk();
        dec.decorate(&mut c1, 0, 0, 0xC0FFEE, 0);
        dec.decorate(&mut c2, 1, 0, 0xC0FFEE, 0);
        let mut differences = 0;
        for lx in 0..16u8 {
            for lz in 0..16u8 {
                for y in 0..=100i64 {
                    let pos = LocalBlockPos { x: lx, y, z: lz };
                    if c1.get_block(pos) != c2.get_block(pos) {
                        differences += 1;
                    }
                }
            }
        }
        assert!(differences > 0, "adjacent chunks should differ");
    }

    #[test]
    fn schema_round_trips_through_json() {
        let schema = DecoratorSchema::Ore(OreDecoratorSchema {
            block: "minecraft:coal_ore".into(),
            replaces: vec!["minecraft:stone".into()],
            attempts_per_chunk: 20,
            vein_size: 8,
            min_y: 0,
            max_y: 100,
        });
        let json = serde_json::to_string(&schema).unwrap();
        let parsed: DecoratorSchema = serde_json::from_str(&json).unwrap();
        let built = parsed.build().unwrap();
        let mut chunk = stone_chunk();
        built.decorate(&mut chunk, 0, 0, 42, 0);
        assert!(count_block(&chunk, coal_ore(), 0..=100) > 0);
    }

    fn oak_log() -> BlockId {
        block::block_id_from_name("minecraft:oak_log").expect("oak_log must resolve")
    }
    fn oak_leaves() -> BlockId {
        block::block_id_from_name("minecraft:oak_leaves").expect("oak_leaves must resolve")
    }

    /// Build a chunk with a flat grass surface at y=70 (dirt below, stone deeper).
    fn grassy_chunk() -> Chunk {
        let mut c = Chunk::new();
        for lx in 0..16u8 {
            for lz in 0..16u8 {
                for y in 0..=66i64 {
                    c.set_block(LocalBlockPos { x: lx, y, z: lz }, block::STONE);
                }
                for y in 67..=69i64 {
                    c.set_block(LocalBlockPos { x: lx, y, z: lz }, block::DIRT);
                }
                c.set_block(LocalBlockPos { x: lx, y: 70, z: lz }, block::GRASS_BLOCK);
            }
        }
        c
    }

    #[test]
    fn tree_decorator_places_log_and_leaves() {
        let dec = TreeDecorator {
            log_block: oak_log(),
            leaves_block: oak_leaves(),
            surface_block: block::GRASS_BLOCK,
            attempts_per_chunk: 4,
            trunk_min: 4,
            trunk_max: 6,
            min_y: 60, max_y: 90,
        };
        let mut chunk = grassy_chunk();
        dec.decorate(&mut chunk, 0, 0, 0xC0FFEE, 0);

        let mut logs = 0;
        let mut leaves = 0;
        for lx in 0..16u8 {
            for lz in 0..16u8 {
                for y in 70..=90i64 {
                    let b = chunk.get_block(LocalBlockPos { x: lx, y, z: lz });
                    if b == oak_log() { logs += 1; }
                    if b == oak_leaves() { leaves += 1; }
                }
            }
        }
        // 4 trees × 4-6 trunk logs ≈ 16-24 logs.
        assert!(logs >= 8, "expected ≥8 log blocks, got {}", logs);
        // 4 trees × ~30 leaves each (5x5 + 5x5-corners + 3x3 = 25+21+9 = 55, minus
        // the trunk-top slot in the middle layer ≈ ~50). Clipping near
        // chunk edges trims some, hence the loose lower bound.
        assert!(leaves >= 40, "expected ≥40 leaf blocks, got {}", leaves);
    }

    #[test]
    fn tree_decorator_skips_non_surface_blocks() {
        // Replace the surface with sand (still flat, just wrong substrate).
        let mut chunk = grassy_chunk();
        for lx in 0..16u8 {
            for lz in 0..16u8 {
                chunk.set_block(LocalBlockPos { x: lx, y: 70, z: lz }, block::SAND);
            }
        }
        let dec = TreeDecorator {
            log_block: oak_log(),
            leaves_block: oak_leaves(),
            surface_block: block::GRASS_BLOCK,
            attempts_per_chunk: 10,
            trunk_min: 4, trunk_max: 6,
            min_y: 60, max_y: 90,
        };
        dec.decorate(&mut chunk, 0, 0, 0xC0FFEE, 0);
        let logs: usize = (0..16u8).flat_map(|lx| (0..16u8).map(move |lz| (lx, lz)))
            .flat_map(|(lx, lz)| (60..=90i64).map(move |y| (lx, y, lz)))
            .filter(|&(lx, y, lz)| chunk.get_block(LocalBlockPos { x: lx, y, z: lz }) == oak_log())
            .count();
        assert_eq!(logs, 0, "no trees should grow on sand");
    }

    #[test]
    fn tree_decorator_is_deterministic() {
        let dec = TreeDecorator {
            log_block: oak_log(),
            leaves_block: oak_leaves(),
            surface_block: block::GRASS_BLOCK,
            attempts_per_chunk: 3,
            trunk_min: 4, trunk_max: 6,
            min_y: 60, max_y: 90,
        };
        let mut a = grassy_chunk();
        let mut b = grassy_chunk();
        dec.decorate(&mut a, 5, 7, 0xC0FFEE, 0);
        dec.decorate(&mut b, 5, 7, 0xC0FFEE, 0);
        for lx in 0..16u8 {
            for lz in 0..16u8 {
                for y in 0..=100i64 {
                    let pos = LocalBlockPos { x: lx, y, z: lz };
                    assert_eq!(a.get_block(pos), b.get_block(pos));
                }
            }
        }
    }

    #[test]
    fn tree_decorator_clips_at_chunk_edge() {
        // Saturate the chunk with placement attempts so every column gets
        // hit, including the four extreme corners where canopy writes
        // would go out of bounds without the chunk clip. The test passes
        // when no panic occurs; the count assertion just confirms work
        // happened.
        let mut chunk = grassy_chunk();
        let dec = TreeDecorator {
            log_block: oak_log(),
            leaves_block: oak_leaves(),
            surface_block: block::GRASS_BLOCK,
            attempts_per_chunk: 200,
            trunk_min: 5, trunk_max: 5,
            min_y: 60, max_y: 90,
        };
        dec.decorate(&mut chunk, 0, 0, 0xC0FFEE, 0);

        // The actual test is "no panic from out-of-bounds canopy writes
        // when trees land near chunk corners". Saturation + a loose log
        // count confirms work happened — most attempts get rejected
        // because previous trees' canopies have already overwritten the
        // grass surface, so we end up with ~20-30 trees out of 200
        // attempts, not 200.
        let mut hit = 0usize;
        for lx in 0..16u8 {
            for lz in 0..16u8 {
                let has_log = (66..=85i64).any(|y| {
                    chunk.get_block(LocalBlockPos { x: lx, y, z: lz }) == oak_log()
                });
                if has_log { hit += 1; }
            }
        }
        assert!(hit >= 10, "expected ≥10 columns with logs, got {}", hit);
    }

    #[test]
    fn tree_schema_round_trips() {
        let schema = DecoratorSchema::Tree(TreeDecoratorSchema {
            log_block: "minecraft:oak_log".into(),
            leaves_block: "minecraft:oak_leaves".into(),
            surface_block: "minecraft:grass_block".into(),
            attempts_per_chunk: 2,
            trunk_min: 4,
            trunk_max: 6,
            min_y: 60,
            max_y: 100,
        });
        let json = serde_json::to_string(&schema).unwrap();
        let parsed: DecoratorSchema = serde_json::from_str(&json).unwrap();
        let built = parsed.build().unwrap();
        let mut chunk = grassy_chunk();
        built.decorate(&mut chunk, 0, 0, 42, 0);
        // Smoke test: just verify it ran.
        let _ = chunk;
    }

    #[test]
    fn unknown_block_in_schema_errors() {
        let bad = DecoratorSchema::Ore(OreDecoratorSchema {
            block: "minecraft:not_a_real_block".into(),
            replaces: vec!["minecraft:stone".into()],
            attempts_per_chunk: 1,
            vein_size: 1,
            min_y: 0,
            max_y: 10,
        });
        assert!(bad.build().is_err());
    }
}
