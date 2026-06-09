//! Phase 6c: chunk eviction — memory bounded by ACTIVE area, not
//! explored area.
//!
//! A periodic task drops chunks that are (a) far from every player and
//! the spawn region, and (b) not dirty. Eviction is safe because every
//! non-dirty chunk is exactly `procedural baseline + stored delta`: the
//! server's worldgen is a [`DeltaOverlayGen`](crate::persistence::DeltaOverlayGen),
//! so the next `ensure_generated` (player walks back, neighbour feature
//! spill, etc.) reproduces the chunk bit-for-bit, edits included. Dirty
//! chunks are skipped until an autosave writes their delta — after which
//! they become evictable.
//!
//! Known coarseness (deliberate): an in-flight physics cascade touching a
//! chunk at the moment of eviction sees AIR through the stale-precondition
//! guard and drops its writes. The keep radius exceeds the view distance
//! by a margin, and activity clusters around players, so the window is
//! both rare and self-healing (the cascade's notifies re-evaluate against
//! the regenerated chunk on next contact).

use std::sync::Arc;
use std::time::Duration;

use ultimate_engine::world::position::ChunkPos;
use ultimate_engine::world::World;

use crate::player_registry::PlayerRegistry;

/// One eviction sweep: drop every non-dirty chunk whose Chebyshev
/// distance (in chunks) from every keep-center exceeds `keep_radius`.
/// Returns the number of chunks evicted.
pub fn evict_far_chunks(world: &World, keep_centers: &[ChunkPos], keep_radius: i32) -> usize {
    // Collect first: removing while iterating a DashMap shard deadlocks.
    let candidates: Vec<ChunkPos> = world
        .iter_chunks()
        .map(|entry| *entry.key())
        .filter(|pos| {
            keep_centers
                .iter()
                .all(|c| (pos.x - c.x).abs().max((pos.z - c.z).abs()) > keep_radius)
        })
        .collect();

    let mut evicted = 0;
    for pos in candidates {
        if world.is_dirty(pos) {
            continue; // unsaved edits — wait for autosave
        }
        if world.remove_chunk(pos) {
            evicted += 1;
        }
    }
    evicted
}

/// Start the periodic eviction task. `keep_radius` is in chunks;
/// `spawn_radius` keeps the spawn region resident even with no players.
pub fn start(
    world: Arc<World>,
    registry: Arc<PlayerRegistry>,
    keep_radius: i32,
    spawn_radius: i32,
    interval_secs: u64,
) {
    if interval_secs == 0 {
        tracing::info!("Chunk eviction disabled (world.eviction_interval_secs = 0)");
        return;
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
        interval.tick().await; // skip the immediate first tick
        loop {
            interval.tick().await;

            // Keep-centers: every player's chunk, plus spawn.
            let mut centers: Vec<ChunkPos> = registry
                .snapshot()
                .iter()
                .map(|p| ChunkPos::new((p.x as i32) >> 4, (p.z as i32) >> 4))
                .collect();
            centers.push(ChunkPos::new(0, 0));

            // Spawn keeps its own (possibly larger) radius by expressing
            // it as extra centers on the spawn ring when it exceeds
            // keep_radius; simpler: use max of both radii for the spawn
            // centre by padding the comparison radius per-centre is
            // overkill — pad globally instead.
            let radius = keep_radius.max(spawn_radius);

            let before = world.chunk_count();
            let evicted = evict_far_chunks(&world, &centers, radius);
            if evicted > 0 {
                tracing::info!(
                    "Evicted {} far chunks ({} -> {} resident)",
                    evicted,
                    before,
                    world.chunk_count(),
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ultimate_engine::world::block::BlockId;
    use ultimate_engine::world::position::BlockPos;

    #[test]
    fn evicts_only_far_nondirty_chunks() {
        let world = World::new();
        // Near chunk (0,0), far clean chunk (20,20), far dirty chunk (30,30).
        world.set_block(BlockPos::new(5, 5, 5), BlockId::new(1));
        world.set_block(BlockPos::new(20 * 16 + 1, 5, 20 * 16 + 1), BlockId::new(1));
        world.set_block(BlockPos::new(30 * 16 + 1, 5, 30 * 16 + 1), BlockId::new(1));
        // (0,0) and (20,20) saved; (30,30) stays dirty.
        world.take_dirty_chunks();
        world.set_block(BlockPos::new(30 * 16 + 2, 6, 30 * 16 + 2), BlockId::new(2));

        let evicted = evict_far_chunks(&world, &[ChunkPos::new(0, 0)], 8);
        assert_eq!(evicted, 1, "only the far clean chunk goes");
        assert!(world.has_chunk(ChunkPos::new(0, 0)), "near chunk kept");
        assert!(!world.has_chunk(ChunkPos::new(20, 20)), "far clean chunk evicted");
        assert!(world.has_chunk(ChunkPos::new(30, 30)), "far dirty chunk kept");
    }

    #[test]
    fn multiple_centers_union_their_keep_areas() {
        let world = World::new();
        world.set_block(BlockPos::new(1, 5, 1), BlockId::new(1));
        world.set_block(BlockPos::new(40 * 16, 5, 40 * 16), BlockId::new(1));
        world.take_dirty_chunks();

        let centers = [ChunkPos::new(0, 0), ChunkPos::new(40, 40)];
        let evicted = evict_far_chunks(&world, &centers, 4);
        assert_eq!(evicted, 0, "both chunks sit inside someone's keep area");
    }
}
