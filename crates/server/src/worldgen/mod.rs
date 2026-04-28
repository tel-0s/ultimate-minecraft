//! World generation.
//!
//! Phase 4 stage 1: deterministic noise-based heightmap terrain. Chunks
//! generate on demand (and a small initial area is pre-generated at startup
//! so the spawn region is immediate).
//!
//! ## Stages
//! 1. Heightmap noise terrain + sea level + on-demand chunks  ← *current*
//! 2. Multi-noise biomes (continentalness / erosion / peaks-and-valleys)
//! 3. 3D-noise cave carving + ore distribution
//! 4. Surface rules + decorators (trees, plants) + simple structures

pub mod noise_terrain;

use ultimate_engine::world::World;
use ultimate_engine::world::chunk::Chunk;
use ultimate_engine::world::position::ChunkPos;

/// A pluggable world generator. Implementations produce a fully-populated
/// `Chunk` from a `(cx, cz)` coordinate. Generation must be deterministic
/// from the generator's internal seed.
pub trait WorldGen: Send + Sync + 'static {
    /// Generate the chunk at `(cx, cz)` from scratch.
    fn generate_chunk(&self, cx: i32, cz: i32) -> Chunk;

    /// Recommended Y coordinate for the player to spawn at, given an XZ
    /// position. Generators that have a sea level or surface height should
    /// return the surface for that column.
    fn spawn_y(&self, x: i64, z: i64) -> f64;

    /// Pre-generate every chunk inside a radius around the world origin.
    /// Used at server startup so the spawn region is immediate.
    fn pregenerate_radius(&self, world: &World, chunk_radius: i32) {
        for cx in -chunk_radius..chunk_radius {
            for cz in -chunk_radius..chunk_radius {
                if !world.has_chunk(ChunkPos::new(cx, cz)) {
                    let chunk = self.generate_chunk(cx, cz);
                    world.insert_chunk(ChunkPos::new(cx, cz), chunk);
                }
            }
        }
    }

    /// Idempotent on-demand generation: if the chunk doesn't exist, generate
    /// and insert it. Called from chunk-loading code paths so the player can
    /// walk past the pre-generated radius without falling into void.
    fn ensure_generated(&self, world: &World, cx: i32, cz: i32) {
        let pos = ChunkPos::new(cx, cz);
        if !world.has_chunk(pos) {
            let chunk = self.generate_chunk(cx, cz);
            world.insert_chunk(pos, chunk);
        }
    }
}
