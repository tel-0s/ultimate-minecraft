//! World generation.
//!
//! Compositional pipeline modeled on vanilla 1.18+'s noise router. The
//! shape of a world is described by a JSON [`preset`] that compiles to a
//! [`pipeline`] using a tree of [`density`] functions. Each layer is
//! independently replaceable so an operator can swap to a superflat preset
//! by changing a single field in `server.yaml` — no recompile.
//!
//! ## Stages
//! - **4a (current)**: density functions + heightmap stratification + JSON
//!   presets (`noise`, `superflat`).
//! - **4b**: composable surface rules (per-biome top blocks, depth bands).
//! - **4c**: multi-noise climate → biome assignment.
//! - **4d**: 3D-noise carvers (caves, ravines) + decorators (trees, ores,
//!   features).

pub mod biome;
pub mod carver;
pub mod climate;
pub mod decorator;
pub mod density;
pub mod pipeline;
pub mod preset;
pub mod surface;

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

    /// Wire ID of the biome covering chunk `(cx, cz)`. Indexes into the
    /// `worldgen/biome` registry the server sent during configuration —
    /// see `worldgen::biome::Biome::registry_id`. Default: 0
    /// (`minecraft:badlands`); implementations should override.
    fn biome_at(&self, _cx: i32, _cz: i32) -> u32 {
        0
    }

    /// Wire ID of the biome at a specific 4×4×4 cell within a section.
    /// World coordinates `(x, y, z)` are the *block* coordinates of the
    /// cell's anchor (typically the cell's centre block). Default
    /// implementation falls back to per-chunk biome assignment; the noise
    /// pipeline overrides this so biome edges fall on 4-block boundaries
    /// rather than 16-block chunk boundaries.
    fn biome_at_cell(&self, x: i64, _y: i64, z: i64) -> u32 {
        let cx = (x.div_euclid(16)) as i32;
        let cz = (z.div_euclid(16)) as i32;
        self.biome_at(cx, cz)
    }

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
