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

use dashmap::DashMap;

use ultimate_engine::world::World;
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::chunk::Chunk;
use ultimate_engine::world::position::{BlockPos, ChunkPos, LocalBlockPos};

use crate::block;
use super::biome::Biome;
use super::climate::BiomeSource;

/// A block write queued for a chunk that hasn't been generated yet. When
/// that chunk is generated, the pipeline drains its pending list into
/// the freshly-generated chunk so cross-chunk features (tree canopies,
/// future structures) survive chunk-border crossings.
#[derive(Debug, Clone, Copy)]
pub struct PendingWrite {
    pub local: LocalBlockPos,
    pub block: BlockId,
}

/// Cross-chunk pending-writes map shared by every chunk's decoration
/// pass. Keyed by *target* chunk: when (cx, cz) is later generated, its
/// list is drained and applied.
pub type PendingWrites = DashMap<ChunkPos, Vec<PendingWrite>>;

/// Per-decoration-pass context handed to every decorator. Carries the
/// mutable chunk plus enough auxiliary state (biome source, surface Y
/// grid, sea level) that decorators can filter on biome / elevation
/// without re-running the density pipeline.
pub struct DecorationContext<'a> {
    pub chunk: &'a mut Chunk,
    pub cx: i32,
    pub cz: i32,
    /// World seed; decorators derive their per-chunk PRNG from
    /// `chunk_decorator_seed(seed, cx, cz, decorator_index)`.
    pub seed: u32,
    pub decorator_index: usize,
    pub biome_source: &'a dyn BiomeSource,
    pub sea_level: i64,
    /// Surface Y per column, indexed `lz * 16 + lx`. Computed from the
    /// density function *before* carving runs, so biome sampling stays
    /// stable regardless of cave hollow-outs at the surface.
    pub surface_y: &'a [i64; 256],
    /// Live world for cross-chunk writes that land in an already-loaded
    /// neighbouring chunk.
    pub world: &'a World,
    /// Shared queue for cross-chunk writes targeting chunks that don't
    /// exist yet. Drained when each target chunk is later generated.
    pub pending: &'a PendingWrites,
}

impl<'a> DecorationContext<'a> {
    /// Biome at the local column `(lx, lz)`. Convenience wrapper around
    /// `biome_source.sample` that resolves world coords and uses the
    /// pre-computed `surface_y` grid.
    pub fn biome_at_local(&mut self, lx: u8, lz: u8) -> Biome {
        let wx = self.cx as i64 * 16 + lx as i64;
        let wz = self.cz as i64 * 16 + lz as i64;
        let sy = self.surface_y[lz as usize * 16 + lx as usize];
        self.biome_source.sample(wx, wz, sy, self.sea_level)
    }

    /// Set a block at any world coordinate. Routes to one of three
    /// destinations based on which chunk the coordinate falls in:
    ///
    /// 1. **In-flight chunk** — direct write to `self.chunk`.
    /// 2. **Already-loaded chunk** — `world.set_block_untracked` mutates the
    ///    live world chunk. Untracked: these writes are procedural terrain,
    ///    not gameplay modifications, so they must not mark the neighbour
    ///    dirty for persistence (a dirty mark would freeze that chunk's
    ///    entire terrain at the current generator version). Note: this
    ///    *doesn't* broadcast a block update to clients, so blocks placed
    ///    mid-game by a neighbour decorator won't appear until that chunk
    ///    is re-sent (next reload).
    /// 3. **Unloaded chunk** — push to `pending[target_chunk]`. When that
    ///    chunk is later generated, its pending list is drained.
    pub fn set_world_block(&mut self, pos: BlockPos, block: BlockId) {
        let chunk_pos = pos.chunk();
        let local = pos.local();
        if chunk_pos.x == self.cx && chunk_pos.z == self.cz {
            self.chunk.set_block(local, block);
        } else if self.world.has_chunk(chunk_pos) {
            self.world.set_block_untracked(pos, block);
        } else {
            self.pending.entry(chunk_pos).or_default().push(PendingWrite { local, block });
        }
    }

    /// Like `set_world_block`, but only writes when the target cell is
    /// currently air. Used by tree canopies so we don't clobber adjacent
    /// terrain. For the "already loaded" path we read via `world.get_block`.
    pub fn set_world_block_if_air(&mut self, pos: BlockPos, block: BlockId) {
        let chunk_pos = pos.chunk();
        let local = pos.local();
        if chunk_pos.x == self.cx && chunk_pos.z == self.cz {
            if self.chunk.get_block(local) == BlockId::AIR {
                self.chunk.set_block(local, block);
            }
        } else if self.world.has_chunk(chunk_pos) {
            if self.world.get_block(pos) == BlockId::AIR {
                self.world.set_block_untracked(pos, block);
            }
        } else {
            // We can't read the future-chunk's contents, so queue
            // unconditionally. The pipeline's drain step does its own
            // "only-write-to-air" guard (we don't want canopies to
            // smash through terrain features that generate later).
            self.pending.entry(chunk_pos).or_default().push(PendingWrite { local, block });
        }
    }
}

/// A post-pass that mutates a chunk to place features. Implementations
/// must be deterministic given the same `ctx.seed`, `ctx.cx`, `ctx.cz`,
/// and `ctx.decorator_index`.
pub trait Decorator: Send + Sync {
    fn decorate(&self, ctx: &mut DecorationContext);
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
    /// If `Some`, only place veins in columns whose biome is in the list.
    /// `None` = place anywhere.
    pub in_biomes: Option<Vec<Biome>>,
}

impl Decorator for OreDecorator {
    fn decorate(&self, ctx: &mut DecorationContext) {
        let mut rng = SplitMix64::new(chunk_decorator_seed(ctx.seed, ctx.cx, ctx.cz, ctx.decorator_index));

        for _ in 0..self.attempts_per_chunk {
            let mut x = rng.range_u32(16) as u8;
            let mut z = rng.range_u32(16) as u8;
            let mut y = rng.range_i64(self.min_y, self.max_y);

            // Biome filter at the attempt's starting column. Veins drift
            // a few blocks during the walk; checking the start is enough
            // resolution at our vein sizes and biome cell size.
            if let Some(biomes) = &self.in_biomes {
                if !biomes.contains(&ctx.biome_at_local(x, z)) {
                    continue;
                }
            }

            for _ in 0..self.vein_size {
                let pos = LocalBlockPos { x, y, z };
                let current = ctx.chunk.get_block(pos);
                if self.replaces.contains(&current) {
                    ctx.chunk.set_block(pos, self.block);
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
    /// If `Some`, only grow trees in columns whose biome is in the list.
    /// `None` = anywhere the `surface_block` filter allows.
    pub in_biomes: Option<Vec<Biome>>,
}

impl Decorator for TreeDecorator {
    fn decorate(&self, ctx: &mut DecorationContext) {
        let mut rng = SplitMix64::new(chunk_decorator_seed(ctx.seed, ctx.cx, ctx.cz, ctx.decorator_index));

        for _ in 0..self.attempts_per_chunk {
            let lx = rng.range_u32(16) as u8;
            let lz = rng.range_u32(16) as u8;

            // Biome filter (sampled at the column).
            if let Some(biomes) = &self.in_biomes {
                if !biomes.contains(&ctx.biome_at_local(lx, lz)) {
                    continue;
                }
            }

            // Find the topmost non-air block in this column inside the Y band.
            let surface_y = (self.min_y..=self.max_y).rev().find(|&y| {
                ctx.chunk.get_block(LocalBlockPos { x: lx, y, z: lz }) != BlockId::AIR
            });
            let Some(sy) = surface_y else { continue };

            // Surface-block filter: only grow on the configured ground block.
            if ctx.chunk.get_block(LocalBlockPos { x: lx, y: sy, z: lz }) != self.surface_block {
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
                ctx.chunk.set_block(
                    LocalBlockPos { x: lx, y: sy + dy, z: lz },
                    self.log_block,
                );
            }

            // Canopy. Each cell goes through `ctx.set_world_block_if_air`
            // so canopy that spills into a neighbouring chunk lands in
            // that chunk (via world.set_block if loaded; pending queue if
            // not) instead of being clipped.
            let trunk_top_y = sy + trunk_h as i64;
            place_canopy_layer(ctx, lx, trunk_top_y - 1, lz, 2, self.leaves_block, false);
            place_canopy_layer(ctx, lx, trunk_top_y,     lz, 2, self.leaves_block, true);
            place_canopy_layer(ctx, lx, trunk_top_y + 1, lz, 1, self.leaves_block, false);
        }
    }
}

/// Place a single layer of canopy leaves: a square of side `2*radius+1`
/// centred on the local-x/local-z column at world Y `y`, optionally
/// skipping the four corner cells (for the diamond-ish vanilla canopy
/// shape). Writes route through `ctx.set_world_block_if_air` so canopy
/// that spills across a chunk border lands in the neighbour rather
/// than being clipped.
fn place_canopy_layer(
    ctx: &mut DecorationContext,
    center_lx: u8, y: i64, center_lz: u8,
    radius: i32,
    leaves: BlockId,
    skip_corners: bool,
) {
    let center_wx = ctx.cx as i64 * 16 + center_lx as i64;
    let center_wz = ctx.cz as i64 * 16 + center_lz as i64;
    for dx in -radius..=radius {
        for dz in -radius..=radius {
            if skip_corners && dx.abs() == radius && dz.abs() == radius {
                continue;
            }
            let pos = BlockPos::new(center_wx + dx as i64, y, center_wz + dz as i64);
            ctx.set_world_block_if_air(pos, leaves);
        }
    }
}

// ── PlantDecorator ──────────────────────────────────────────────────────────

/// Scatters single-block surface features (flowers, grass tufts, etc.)
/// on top of a configured `surface_block`. Each attempt picks a random
/// column, finds the topmost non-air block, and — if it matches the
/// surface filter and has air above — places a randomly chosen entry
/// from `blocks` one cell above the surface.
///
/// `blocks` is a weighted draw: duplicates in the list bias the
/// selection (e.g. `[short_grass, short_grass, short_grass, dandelion,
/// poppy]` gives 60 % grass, 20 % dandelion, 20 % poppy).
pub struct PlantDecorator {
    pub blocks: Vec<BlockId>,
    pub surface_block: BlockId,
    pub attempts_per_chunk: u32,
    pub min_y: i64,
    pub max_y: i64,
    pub in_biomes: Option<Vec<Biome>>,
}

impl Decorator for PlantDecorator {
    fn decorate(&self, ctx: &mut DecorationContext) {
        if self.blocks.is_empty() { return; }
        let mut rng = SplitMix64::new(chunk_decorator_seed(ctx.seed, ctx.cx, ctx.cz, ctx.decorator_index));

        for _ in 0..self.attempts_per_chunk {
            let lx = rng.range_u32(16) as u8;
            let lz = rng.range_u32(16) as u8;

            if let Some(biomes) = &self.in_biomes {
                if !biomes.contains(&ctx.biome_at_local(lx, lz)) {
                    continue;
                }
            }

            // Topmost non-air block in the y band.
            let surface_y = (self.min_y..=self.max_y).rev().find(|&y| {
                ctx.chunk.get_block(LocalBlockPos { x: lx, y, z: lz }) != BlockId::AIR
            });
            let Some(sy) = surface_y else { continue };

            // Must be on the right ground block, with empty air one cell above.
            if ctx.chunk.get_block(LocalBlockPos { x: lx, y: sy, z: lz }) != self.surface_block {
                continue;
            }
            let above = LocalBlockPos { x: lx, y: sy + 1, z: lz };
            if ctx.chunk.get_block(above) != BlockId::AIR {
                continue;
            }

            let block = self.blocks[rng.range_u32(self.blocks.len() as u32) as usize];
            ctx.chunk.set_block(above, block);
        }
    }
}

// ── JSON schema ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DecoratorSchema {
    Ore(OreDecoratorSchema),
    Tree(TreeDecoratorSchema),
    Plant(PlantDecoratorSchema),
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
    /// Optional biome whitelist. `None` / omitted = place in any biome.
    #[serde(default)]
    pub in_biomes: Option<Vec<Biome>>,
}

fn default_vein_size() -> u32 { 8 }

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TreeDecoratorSchema {
    pub log_block: String,
    pub leaves_block: String,
    /// Only place trees on top of this block (e.g. `"minecraft:grass_block"`).
    /// Acts as a coarse surface filter; the finer `in_biomes` filter (below)
    /// gates by biome directly.
    pub surface_block: String,
    pub attempts_per_chunk: u32,
    pub trunk_min: u32,
    pub trunk_max: u32,
    pub min_y: i64,
    pub max_y: i64,
    /// Optional biome whitelist. `None` / omitted = anywhere the
    /// `surface_block` filter allows.
    #[serde(default)]
    pub in_biomes: Option<Vec<Biome>>,
}

/// One entry in a plant decorator's `blocks` list: either a bare block
/// name (weight 1) or `{"block": "...", "weight": N}` for vanilla-like
/// ratios without dozens of duplicate strings.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum WeightedBlockSchema {
    Plain(String),
    Weighted { block: String, weight: u32 },
}

impl WeightedBlockSchema {
    fn block(&self) -> &str {
        match self {
            Self::Plain(name) => name,
            Self::Weighted { block, .. } => block,
        }
    }

    fn weight(&self) -> u32 {
        match self {
            Self::Plain(_) => 1,
            Self::Weighted { weight, .. } => *weight,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlantDecoratorSchema {
    /// List of plant entries to choose from per attempt. Each entry is a
    /// bare block name (weight 1) or `{"block": ..., "weight": N}`;
    /// duplicates also stack. E.g. grass at weight 36 + four flowers at
    /// weight 1 each ≈ 10 % flowers.
    pub blocks: Vec<WeightedBlockSchema>,
    /// Only place plants on top of this block (e.g. `"minecraft:grass_block"`).
    pub surface_block: String,
    pub attempts_per_chunk: u32,
    pub min_y: i64,
    pub max_y: i64,
    #[serde(default)]
    pub in_biomes: Option<Vec<Biome>>,
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
                    in_biomes: o.in_biomes.clone(),
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
                    in_biomes: t.in_biomes.clone(),
                }))
            }
            Self::Plant(p) => {
                if p.blocks.is_empty() {
                    return Err(anyhow!("plant decorator: `blocks` must be non-empty"));
                }
                // Expand weights into a flat draw list. Weights are small
                // integers; cap the expansion so a typo'd weight can't
                // balloon memory.
                let total_weight: u64 = p.blocks.iter().map(|e| e.weight() as u64).sum();
                if total_weight == 0 {
                    return Err(anyhow!("plant decorator: total weight must be > 0"));
                }
                if total_weight > 4096 {
                    return Err(anyhow!(
                        "plant decorator: total weight {} exceeds 4096", total_weight
                    ));
                }
                let mut blocks = Vec::with_capacity(total_weight as usize);
                for entry in &p.blocks {
                    let id = block::block_id_from_name(entry.block())
                        .ok_or_else(|| anyhow!("unknown plant block {:?}", entry.block()))?;
                    blocks.extend(std::iter::repeat_n(id, entry.weight() as usize));
                }
                let surface_block = block::block_id_from_name(&p.surface_block)
                    .ok_or_else(|| anyhow!("unknown plant surface block {:?}", p.surface_block))?;
                Ok(Arc::new(PlantDecorator {
                    blocks,
                    surface_block,
                    attempts_per_chunk: p.attempts_per_chunk,
                    min_y: p.min_y,
                    max_y: p.max_y,
                    in_biomes: p.in_biomes.clone(),
                }))
            }
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::climate::FixedBiomeSource;

    /// Helper: run a decorator against a test chunk using a fixed-biome
    /// biome source and a throwaway empty world. Returns the chunk so
    /// tests can inspect it. Cross-chunk writes will go to the pending
    /// queue (which the tests don't inspect) since the test world is empty.
    fn run_decorator(
        dec: &dyn Decorator,
        mut chunk: Chunk,
        cx: i32, cz: i32,
        seed: u32,
        idx: usize,
        biome: Biome,
        surface_y_value: i64,
    ) -> Chunk {
        let source = FixedBiomeSource(biome);
        let surface_y = [surface_y_value; 256];
        let world = World::new();
        let pending = PendingWrites::new();
        let mut ctx = DecorationContext {
            chunk: &mut chunk,
            cx, cz, seed, decorator_index: idx,
            biome_source: &source,
            sea_level: 63,
            surface_y: &surface_y,
            world: &world,
            pending: &pending,
        };
        dec.decorate(&mut ctx);
        chunk
    }

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

    fn unfiltered_ore() -> OreDecorator {
        OreDecorator {
            block: coal_ore(),
            replaces: vec![block::STONE],
            attempts_per_chunk: 20,
            vein_size: 8,
            min_y: 0,
            max_y: 100,
            in_biomes: None,
        }
    }

    #[test]
    fn ore_decorator_places_ore() {
        let dec = unfiltered_ore();
        let chunk = run_decorator(&dec, stone_chunk(), 0, 0, 0xC0FFEE, 0, Biome::Plains, 70);
        let n = count_block(&chunk, coal_ore(), 0..=100);
        // 20 attempts × 8 vein steps with self-overlap and OOB-bumps
        // typically lands ~80-120 ore blocks. Sanity-check a wide band.
        assert!(n > 20 && n < 200, "expected ~80-120 ore, got {}", n);
    }

    #[test]
    fn ore_decorator_respects_y_range() {
        let dec = OreDecorator {
            attempts_per_chunk: 30,
            min_y: 20,
            max_y: 40,
            ..unfiltered_ore()
        };
        let chunk = run_decorator(&dec, stone_chunk(), 0, 0, 42, 0, Biome::Plains, 70);
        // No ore should appear outside [20, 40].
        assert_eq!(count_block(&chunk, coal_ore(), 0..=19), 0);
        assert_eq!(count_block(&chunk, coal_ore(), 41..=100), 0);
        // Plenty inside.
        assert!(count_block(&chunk, coal_ore(), 20..=40) > 0);
    }

    #[test]
    fn ore_decorator_only_replaces_listed_blocks() {
        let dec = OreDecorator {
            replaces: vec![block::DIRT], // only dirt, but chunk is all stone
            attempts_per_chunk: 50,
            ..unfiltered_ore()
        };
        let chunk = run_decorator(&dec, stone_chunk(), 0, 0, 0, 0, Biome::Plains, 70);
        assert_eq!(count_block(&chunk, coal_ore(), 0..=100), 0);
    }

    #[test]
    fn ore_decorator_is_deterministic_per_seed() {
        let dec = unfiltered_ore();
        let c1 = run_decorator(&dec, stone_chunk(), 3, 7, 0xC0FFEE, 0, Biome::Plains, 70);
        let c2 = run_decorator(&dec, stone_chunk(), 3, 7, 0xC0FFEE, 0, Biome::Plains, 70);
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
        let dec = unfiltered_ore();
        let c1 = run_decorator(&dec, stone_chunk(), 0, 0, 0xC0FFEE, 0, Biome::Plains, 70);
        let c2 = run_decorator(&dec, stone_chunk(), 1, 0, 0xC0FFEE, 0, Biome::Plains, 70);
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
    fn ore_decorator_in_biomes_filter_skips_outside() {
        let dec = OreDecorator {
            in_biomes: Some(vec![Biome::Plains]),
            attempts_per_chunk: 50,
            ..unfiltered_ore()
        };
        // Run in Desert — filter should skip everything.
        let chunk = run_decorator(&dec, stone_chunk(), 0, 0, 0, 0, Biome::Desert, 70);
        assert_eq!(count_block(&chunk, coal_ore(), 0..=100), 0,
            "ore decorator with in_biomes=[plains] should place nothing in a desert chunk");
        // Same dec, in Plains: places ore.
        let chunk = run_decorator(&dec, stone_chunk(), 0, 0, 0, 0, Biome::Plains, 70);
        assert!(count_block(&chunk, coal_ore(), 0..=100) > 0,
            "ore decorator should still fire in an allowed biome");
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
            in_biomes: None,
        });
        let json = serde_json::to_string(&schema).unwrap();
        let parsed: DecoratorSchema = serde_json::from_str(&json).unwrap();
        let built = parsed.build().unwrap();
        let chunk = run_decorator(&*built, stone_chunk(), 0, 0, 42, 0, Biome::Plains, 70);
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

    fn unfiltered_tree() -> TreeDecorator {
        TreeDecorator {
            log_block: oak_log(),
            leaves_block: oak_leaves(),
            surface_block: block::GRASS_BLOCK,
            attempts_per_chunk: 4,
            trunk_min: 4,
            trunk_max: 6,
            min_y: 60, max_y: 90,
            in_biomes: None,
        }
    }

    #[test]
    fn tree_decorator_places_log_and_leaves() {
        let dec = unfiltered_tree();
        let chunk = run_decorator(&dec, grassy_chunk(), 0, 0, 0xC0FFEE, 0, Biome::Plains, 70);
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
        assert!(logs >= 8, "expected ≥8 log blocks, got {}", logs);
        assert!(leaves >= 40, "expected ≥40 leaf blocks, got {}", leaves);
    }

    #[test]
    fn tree_decorator_skips_non_surface_blocks() {
        // Surface is sand, not grass.
        let mut chunk = grassy_chunk();
        for lx in 0..16u8 {
            for lz in 0..16u8 {
                chunk.set_block(LocalBlockPos { x: lx, y: 70, z: lz }, block::SAND);
            }
        }
        let dec = TreeDecorator { attempts_per_chunk: 10, ..unfiltered_tree() };
        let chunk = run_decorator(&dec, chunk, 0, 0, 0xC0FFEE, 0, Biome::Plains, 70);
        let logs: usize = (0..16u8).flat_map(|lx| (0..16u8).map(move |lz| (lx, lz)))
            .flat_map(|(lx, lz)| (60..=90i64).map(move |y| (lx, y, lz)))
            .filter(|&(lx, y, lz)| chunk.get_block(LocalBlockPos { x: lx, y, z: lz }) == oak_log())
            .count();
        assert_eq!(logs, 0, "no trees should grow on sand");
    }

    #[test]
    fn tree_decorator_is_deterministic() {
        let dec = TreeDecorator { attempts_per_chunk: 3, ..unfiltered_tree() };
        let a = run_decorator(&dec, grassy_chunk(), 5, 7, 0xC0FFEE, 0, Biome::Plains, 70);
        let b = run_decorator(&dec, grassy_chunk(), 5, 7, 0xC0FFEE, 0, Biome::Plains, 70);
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
        // Saturate the chunk so every column gets hit, including corners.
        // Asserts no out-of-bounds panic from canopy writes near edges.
        let dec = TreeDecorator {
            attempts_per_chunk: 200,
            trunk_min: 5, trunk_max: 5,
            ..unfiltered_tree()
        };
        let chunk = run_decorator(&dec, grassy_chunk(), 0, 0, 0xC0FFEE, 0, Biome::Plains, 70);
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
    fn tree_decorator_in_biomes_filter_skips_outside() {
        let dec = TreeDecorator {
            attempts_per_chunk: 20,
            in_biomes: Some(vec![Biome::Forest]),
            ..unfiltered_tree()
        };
        // Run in Plains — filter is Forest-only, no trees expected.
        let chunk = run_decorator(&dec, grassy_chunk(), 0, 0, 0xC0FFEE, 0, Biome::Plains, 70);
        let logs = (0..16u8).flat_map(|lx| (0..16u8).map(move |lz| (lx, lz)))
            .flat_map(|(lx, lz)| (70..=90i64).map(move |y| (lx, y, lz)))
            .filter(|&(lx, y, lz)| chunk.get_block(LocalBlockPos { x: lx, y, z: lz }) == oak_log())
            .count();
        assert_eq!(logs, 0, "tree decorator filtered to forest should skip plains");

        // Same dec in Forest: should place trees.
        let chunk = run_decorator(&dec, grassy_chunk(), 0, 0, 0xC0FFEE, 0, Biome::Forest, 70);
        let logs = (0..16u8).flat_map(|lx| (0..16u8).map(move |lz| (lx, lz)))
            .flat_map(|(lx, lz)| (70..=90i64).map(move |y| (lx, y, lz)))
            .filter(|&(lx, y, lz)| chunk.get_block(LocalBlockPos { x: lx, y, z: lz }) == oak_log())
            .count();
        assert!(logs > 0, "tree decorator should fire in its allowed biome");
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
            in_biomes: Some(vec![Biome::Plains, Biome::Forest]),
        });
        let json = serde_json::to_string(&schema).unwrap();
        let parsed: DecoratorSchema = serde_json::from_str(&json).unwrap();
        let built = parsed.build().unwrap();
        let _chunk = run_decorator(&*built, grassy_chunk(), 0, 0, 42, 0, Biome::Plains, 70);
    }

    fn short_grass() -> BlockId {
        block::block_id_from_name("minecraft:short_grass").expect("short_grass must resolve")
    }
    fn dandelion() -> BlockId {
        block::block_id_from_name("minecraft:dandelion").expect("dandelion must resolve")
    }
    fn poppy() -> BlockId {
        block::block_id_from_name("minecraft:poppy").expect("poppy must resolve")
    }

    #[test]
    fn plant_decorator_places_above_surface() {
        let dec = PlantDecorator {
            blocks: vec![short_grass(), dandelion(), poppy()],
            surface_block: block::GRASS_BLOCK,
            attempts_per_chunk: 30,
            min_y: 60, max_y: 90,
            in_biomes: None,
        };
        let chunk = run_decorator(&dec, grassy_chunk(), 0, 0, 0xC0FFEE, 0, Biome::Plains, 70);
        // Plants should appear at y=71 (one above the surface at y=70).
        let mut plants_at_71 = 0usize;
        for lx in 0..16u8 {
            for lz in 0..16u8 {
                let b = chunk.get_block(LocalBlockPos { x: lx, y: 71, z: lz });
                if b == short_grass() || b == dandelion() || b == poppy() {
                    plants_at_71 += 1;
                }
            }
        }
        assert!(plants_at_71 >= 10, "expected ≥10 plants at y=71, got {}", plants_at_71);
    }

    #[test]
    fn plant_decorator_skips_non_surface() {
        let mut chunk = grassy_chunk();
        for lx in 0..16u8 {
            for lz in 0..16u8 {
                chunk.set_block(LocalBlockPos { x: lx, y: 70, z: lz }, block::SAND);
            }
        }
        let dec = PlantDecorator {
            blocks: vec![short_grass()],
            surface_block: block::GRASS_BLOCK,
            attempts_per_chunk: 30,
            min_y: 60, max_y: 90,
            in_biomes: None,
        };
        let chunk = run_decorator(&dec, chunk, 0, 0, 0xC0FFEE, 0, Biome::Plains, 70);
        let plants = (0..16u8).flat_map(|lx| (0..16u8).map(move |lz| (lx, lz)))
            .filter(|&(lx, lz)| chunk.get_block(LocalBlockPos { x: lx, y: 71, z: lz }) == short_grass())
            .count();
        assert_eq!(plants, 0, "plants must not grow on sand");
    }

    #[test]
    fn plant_decorator_respects_in_biomes_filter() {
        let dec = PlantDecorator {
            blocks: vec![short_grass()],
            surface_block: block::GRASS_BLOCK,
            attempts_per_chunk: 30,
            min_y: 60, max_y: 90,
            in_biomes: Some(vec![Biome::Plains]),
        };
        // Desert: filter rejects everything.
        let chunk = run_decorator(&dec, grassy_chunk(), 0, 0, 0xC0FFEE, 0, Biome::Desert, 70);
        let plants = (0..16u8).flat_map(|lx| (0..16u8).map(move |lz| (lx, lz)))
            .filter(|&(lx, lz)| chunk.get_block(LocalBlockPos { x: lx, y: 71, z: lz }) == short_grass())
            .count();
        assert_eq!(plants, 0);
        // Plains: places plants.
        let chunk = run_decorator(&dec, grassy_chunk(), 0, 0, 0xC0FFEE, 0, Biome::Plains, 70);
        let plants = (0..16u8).flat_map(|lx| (0..16u8).map(move |lz| (lx, lz)))
            .filter(|&(lx, lz)| chunk.get_block(LocalBlockPos { x: lx, y: 71, z: lz }) == short_grass())
            .count();
        assert!(plants > 0);
    }

    #[test]
    fn plant_decorator_weighted_selection() {
        // Stack the deck heavily toward dandelion and verify the mix.
        let dec = PlantDecorator {
            blocks: vec![
                dandelion(), dandelion(), dandelion(), dandelion(), dandelion(),
                dandelion(), dandelion(), dandelion(), dandelion(), poppy(),
            ],
            surface_block: block::GRASS_BLOCK,
            attempts_per_chunk: 100,
            min_y: 60, max_y: 90,
            in_biomes: None,
        };
        let chunk = run_decorator(&dec, grassy_chunk(), 0, 0, 0xC0FFEE, 0, Biome::Plains, 70);
        let mut dandelions = 0;
        let mut poppies = 0;
        for lx in 0..16u8 {
            for lz in 0..16u8 {
                let b = chunk.get_block(LocalBlockPos { x: lx, y: 71, z: lz });
                if b == dandelion() { dandelions += 1; }
                if b == poppy() { poppies += 1; }
            }
        }
        // 90% should be dandelions; check the ratio is roughly right.
        assert!(dandelions > poppies * 4,
            "weighted draw: expected dandelions >> poppies, got {} vs {}", dandelions, poppies);
    }

    #[test]
    fn plant_schema_round_trips() {
        let schema = DecoratorSchema::Plant(PlantDecoratorSchema {
            blocks: vec![
                WeightedBlockSchema::Plain("minecraft:short_grass".into()),
                WeightedBlockSchema::Plain("minecraft:dandelion".into()),
            ],
            surface_block: "minecraft:grass_block".into(),
            attempts_per_chunk: 20,
            min_y: 60, max_y: 100,
            in_biomes: Some(vec![Biome::Plains]),
        });
        let json = serde_json::to_string(&schema).unwrap();
        let parsed: DecoratorSchema = serde_json::from_str(&json).unwrap();
        let built = parsed.build().unwrap();
        let _ = run_decorator(&*built, grassy_chunk(), 0, 0, 42, 0, Biome::Plains, 70);
    }

    #[test]
    fn plant_schema_rejects_empty_blocks() {
        let bad = DecoratorSchema::Plant(PlantDecoratorSchema {
            blocks: vec![],
            surface_block: "minecraft:grass_block".into(),
            attempts_per_chunk: 1,
            min_y: 60, max_y: 100,
            in_biomes: None,
        });
        assert!(bad.build().is_err());
    }

    #[test]
    fn plant_schema_weighted_entries_parse_and_expand() {
        // Mixed plain + weighted JSON forms; the weighted entry dominates
        // the expanded draw list.
        let json = r#"{
            "type": "plant",
            "blocks": [
                { "block": "minecraft:short_grass", "weight": 9 },
                "minecraft:dandelion"
            ],
            "surface_block": "minecraft:grass_block",
            "attempts_per_chunk": 100,
            "min_y": 60,
            "max_y": 90
        }"#;
        let parsed: DecoratorSchema = serde_json::from_str(json).unwrap();
        let built = parsed.build().unwrap();
        let chunk = run_decorator(&*built, grassy_chunk(), 0, 0, 0xC0FFEE, 0, Biome::Plains, 70);

        let mut grass = 0;
        let mut flowers = 0;
        for lx in 0..16u8 {
            for lz in 0..16u8 {
                let b = chunk.get_block(LocalBlockPos { x: lx, y: 71, z: lz });
                if b == short_grass() { grass += 1; }
                if b == dandelion() { flowers += 1; }
            }
        }
        assert!(grass > flowers * 3,
            "weight 9:1 should heavily favor grass, got {} grass vs {} flowers", grass, flowers);
    }

    #[test]
    fn plant_schema_rejects_zero_total_weight() {
        let json = r#"{
            "type": "plant",
            "blocks": [{ "block": "minecraft:short_grass", "weight": 0 }],
            "surface_block": "minecraft:grass_block",
            "attempts_per_chunk": 1,
            "min_y": 60,
            "max_y": 90
        }"#;
        let parsed: DecoratorSchema = serde_json::from_str(json).unwrap();
        assert!(parsed.build().is_err());
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
            in_biomes: None,
        });
        assert!(bad.build().is_err());
    }
}
