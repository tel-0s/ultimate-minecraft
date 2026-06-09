//! Micro-measurement: what does a world block read actually cost on the
//! physics hot path, and how much of it is the shared `DashMap` lookup?
//!
//! Decides the Phase 6c arena strategy from data instead of assumption:
//! if per-access map overhead dominates, the fix is lookup amortization
//! (hold the chunk ref across clustered reads); if contention dominates,
//! the fix is per-worker storage.
//!
//! Run with: `cargo run --release --example bench_access`

use std::sync::Arc;
use std::time::Instant;

use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::chunk::Chunk;
use ultimate_engine::world::position::{BlockPos, ChunkPos, LocalBlockPos};
use ultimate_engine::world::World;

const READS: usize = 20_000_000;

fn flat_world(radius: i32) -> Arc<World> {
    let world = World::new();
    for cx in -radius..radius {
        for cz in -radius..radius {
            let mut chunk = Chunk::new();
            for x in 0..16u8 {
                for z in 0..16u8 {
                    for y in 0..4i64 {
                        chunk.set_block(LocalBlockPos { x, y, z }, BlockId::new(1));
                    }
                }
            }
            world.insert_chunk(ChunkPos::new(cx, cz), chunk);
        }
    }
    Arc::new(world)
}

/// Deterministic position stream mimicking rule-evaluation locality:
/// clustered reads around a slowly wandering centre.
fn positions(n: usize) -> Vec<BlockPos> {
    let mut out = Vec::with_capacity(n);
    let mut state = 0x5EEDu64;
    let mut next = move || {
        state = state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    };
    let (mut cx, mut cz) = (0i64, 0i64);
    for i in 0..n {
        if i % 16 == 0 {
            // wander the cluster centre
            cx = ((next() % 160) as i64) - 80;
            cz = ((next() % 160) as i64) - 80;
        }
        let dx = (next() % 5) as i64 - 2;
        let dz = (next() % 5) as i64 - 2;
        let y = (next() % 4) as i64;
        out.push(BlockPos::new(cx + dx, y, cz + dz));
    }
    out
}

fn main() {
    let world = flat_world(8);
    let pos = positions(READS);

    // A) Straight world.get_block: hash + DashMap shard lock per read.
    let t0 = Instant::now();
    let mut acc = 0u64;
    for p in &pos {
        acc = acc.wrapping_add(world.get_block(*p).0 as u64);
    }
    let t_map = t0.elapsed();

    // B) Last-chunk memoized: re-acquire only on chunk cross.
    let t0 = Instant::now();
    let mut acc2 = 0u64;
    let mut cached: Option<(ChunkPos, dashmap::mapref::one::Ref<ChunkPos, Chunk>)> = None;
    let mut hits = 0usize;
    for p in &pos {
        let cp = p.chunk();
        let hit = matches!(&cached, Some((c, _)) if *c == cp);
        if hit {
            hits += 1;
        } else {
            cached = world.get_chunk(&cp).map(|r| (cp, r));
        }
        let b = match &cached {
            Some((_, chunk)) => chunk.get_block(p.local()),
            None => BlockId::AIR,
        };
        acc2 = acc2.wrapping_add(b.0 as u64);
    }
    let t_cached = t0.elapsed();
    assert_eq!(acc, acc2);

    // C) Pure section reads (no map at all): the floor.
    let chunk = world.get_chunk(&ChunkPos::new(0, 0)).unwrap();
    let t0 = Instant::now();
    let mut acc3 = 0u64;
    for p in &pos {
        let lp = LocalBlockPos { x: (p.x & 15) as u8, y: p.y, z: (p.z & 15) as u8 };
        acc3 = acc3.wrapping_add(chunk.get_block(lp).0 as u64);
    }
    let t_floor = t0.elapsed();
    std::hint::black_box((acc, acc2, acc3));

    let per = |d: std::time::Duration| d.as_nanos() as f64 / READS as f64;
    println!("=== World access micro-bench ({} clustered reads) ===", READS);
    println!("  A) world.get_block (DashMap per read):   {:>6.2} ns/read", per(t_map));
    println!(
        "  B) last-chunk memoized (hit rate {:>4.1}%):  {:>6.2} ns/read",
        100.0 * hits as f64 / READS as f64,
        per(t_cached)
    );
    println!("  C) direct section read (floor):          {:>6.2} ns/read", per(t_floor));
    println!();
    println!(
        "  DashMap overhead per read: {:.2} ns ({:.0}% of A); memoization recovers {:.0}%",
        per(t_map) - per(t_floor),
        100.0 * (per(t_map) - per(t_floor)) / per(t_map),
        100.0 * (per(t_map) - per(t_cached)) / (per(t_map) - per(t_floor)).max(0.01),
    );
}
