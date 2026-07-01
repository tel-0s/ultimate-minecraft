//! Phase 5 bench: falling-block ENTITIES at vanilla-comparison scale.
//!
//! Mirrors the 160k-sand workload measured against the real vanilla
//! server (see ROADMAP 6e): 100×100 columns × 16 layers dropped ~29
//! blocks onto a flat floor. Vanilla ticks every falling entity 20×/s
//! (3.2M integrations/s, CPU-saturated, done in ~12.3 s). Here each
//! entity is a parametric trajectory: ~2 events for the whole fall plus
//! wake-driven re-plans as stacks grow beneath it — and wall time is the
//! PHYSICAL fall time, not a CPU artifact.
//!
//! Usage: cargo run --release --example bench_entities [entities]

use std::sync::Arc;
use std::time::{Duration, Instant};

use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::chunk::Chunk;
use ultimate_engine::world::entity::{EntityState, Vec3};
use ultimate_engine::world::position::{BlockPos, ChunkPos, LocalBlockPos};
use ultimate_engine::world::World;
use ultimate_engine::causal::event::{Event, EventPayload};

use ultimate_server::block;
use ultimate_server::physics::{self, PhysicsOptions};
use ultimate_server::rules::entity::KIND_FALLING_BLOCK;

fn flat_world(chunks: i32) -> Arc<World> {
    let world = World::new();
    for cx in -1..=chunks {
        for cz in -1..=chunks {
            let mut chunk = Chunk::new();
            for x in 0..16u8 {
                for z in 0..16u8 {
                    for y in 0..4i64 {
                        chunk.set_block(LocalBlockPos { x, y, z }, BlockId::new(1));
                    }
                    chunk.set_block(LocalBlockPos { x, y: 4, z }, block::DIRT);
                }
            }
            world.insert_chunk(ChunkPos::new(cx, cz), chunk);
        }
    }
    Arc::new(world)
}

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(160_000);
    let columns = 100i64; // 100×100 columns, layers stacked upward
    let layers = n.div_ceil((columns * columns) as usize);

    let world = flat_world(7);
    let handle = physics::start(
        Arc::clone(&world),
        ultimate_server::rules::standard_with_falling_blocks,
        ultimate_server::event_bus::SpatialBus::new(),
        None,
        // Rebalancing OFF: the 6d rebalancer's transient dual-ownership
        // windows can reorder a landing's despawn against a wake-bump
        // across two workers, duplicating ~0.2% of conversions. Entity
        // conversion atomicity under region handoff is an open item
        // (docs/phase5-entities.md §8); block rules tolerate dual windows
        // by confluence, cross-store conversions don't yet.
        PhysicsOptions { workers: 0, rebalance: false, ..Default::default() },
    );

    // Spawn N falling-block entities in the air, mirroring the vanilla
    // 16-layer sand drop (~29 blocks of fall for the bottom layer).
    let mut spawns = Vec::with_capacity(n);
    let now = world.now();
    'outer: for layer in 0..layers {
        for x in 0..columns {
            for z in 0..columns {
                if spawns.len() >= n {
                    break 'outer;
                }
                let id = world.entities().allocate_id();
                spawns.push(Event {
                    payload: EventPayload::EntitySet {
                        id,
                        old: None,
                        new: Some(EntityState {
                            kind: KIND_FALLING_BLOCK,
                            pos: Vec3::new(
                                x as f64 + 0.5,
                                34.0 + layer as f64,
                                z as f64 + 0.5,
                            ),
                            vel: Vec3::new(0.0, -0.1, 0.0),
                            stamp: now,
                            aux: block::SAND.0 as u64,
                        }),
                    },
                });
            }
        }
    }

    println!(
        "bench_entities: {} falling-block entities, {} layers over {}x{} columns, ~29-block fall",
        spawns.len(), layers, columns, columns,
    );

    let t0 = Instant::now();
    handle.submit_events(spawns);

    // Done when every entity has converted back into a block.
    let mut last_print = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(25));
        if world.entities().is_empty() && handle.pending() == 0 {
            break;
        }
        if last_print.elapsed() > Duration::from_secs(1) {
            last_print = Instant::now();
            println!(
                "  t={:.1}s  entities={}  executed={}  timed={}",
                t0.elapsed().as_secs_f64(),
                world.entities().len(),
                handle.executed_total(),
                handle.pending_timed(),
            );
        }
        if t0.elapsed() > Duration::from_secs(60) {
            eprintln!(
                "TIMEOUT: {} entities remain, pending {}, pending_timed {}",
                world.entities().len(),
                handle.pending(),
                handle.pending_timed(),
            );
            for (id, s) in world.entities().snapshot().into_iter().take(5) {
                eprintln!(
                    "  stuck: id={} kind={} pos=({:.2},{:.2},{:.2}) vel=({:.2},{:.2},{:.2}) stamp={}s",
                    id.0, s.kind.0,
                    s.pos.x, s.pos.y, s.pos.z,
                    s.vel.x, s.vel.y, s.vel.z,
                    s.stamp / 1_000_000_000,
                );
            }
            break;
        }
    }
    let elapsed = t0.elapsed();
    let executed = handle.executed_total();

    // Verify: every entity became a block, stacked on the floor.
    let mut sand = 0usize;
    for x in 0..columns {
        for z in 0..columns {
            for y in 5..(5 + layers as i64 + 2) {
                if world.get_block(BlockPos::new(x, y, z)) == block::SAND {
                    sand += 1;
                }
            }
        }
    }

    println!("  wall time:        {:.2} s  (pure fall time ≈ 1.7 s)", elapsed.as_secs_f64());
    println!("  events executed:  {}  ({:.1} per entity)", executed, executed as f64 / n as f64);
    println!("  blocks re-landed: {} / {}", sand, n);
    println!(
        "  vanilla (measured, same workload): CPU-saturated at 3.2M integrations/s, done <12.3 s"
    );
}
