//! Benchmark: sequential vs parallel scheduler.
//!
//! Drops many sand columns across a grid of chunks and measures time to quiescence.
//! Run with: `cargo run --release -p ultimate-server --example bench_parallel`

use std::time::Instant;
use ultimate_engine::causal::event::{Event, EventPayload};
use ultimate_engine::causal::graph::CausalGraph;
use ultimate_engine::causal::scheduler::Scheduler;
use ultimate_engine::world::chunk::{Chunk, SECTION_SIZE};
use ultimate_engine::world::position::{BlockPos, ChunkPos, LocalBlockPos};
use ultimate_engine::world::World;

use ultimate_server::block;

fn main() {
    // NOTE: drop height is kept at 5 blocks because the current rules generate
    // duplicate events that compound exponentially (~2^N per column for N blocks
    // of fall). To create enough parallel work per chunk, we place multiple sand
    // columns inside each chunk (at different X,Z positions).
    let chunks = 256;
    let sand_per_chunk = 16; // 16 sand columns per chunk (4x4 grid within chunk)
    let drop_height: i64 = 10;
    let side = (chunks as f64).sqrt().ceil() as i32;
    let total_sand = chunks * sand_per_chunk;

    println!("=== Ultimate Minecraft: Parallel Scheduler Benchmark ===\n");
    println!("  {} chunks ({}x{} grid), {} sand columns per chunk", chunks, side, side, sand_per_chunk);
    println!("  {} total sand drops, each from y={} (5-block fall)\n", total_sand, drop_height);

    let rules = ultimate_server::rules::standard();
    let scheduler = Scheduler::new();

    // --- Sequential ---
    let world_seq = build_world(side);
    let mut graph_seq = build_graph(chunks, side, sand_per_chunk, drop_height);

    let t0 = Instant::now();
    let n_seq = scheduler.run_until_quiet(&world_seq, &mut graph_seq, &rules, 10_000);
    let dt_seq = t0.elapsed();

    println!("  Sequential: {:>8} events in {:>8.2?}", n_seq, dt_seq);

    // --- Parallel ---
    let world_par = build_world(side);
    let mut graph_par = build_graph(chunks, side, sand_per_chunk, drop_height);

    let t0 = Instant::now();
    let n_par = scheduler.run_until_quiet_parallel(&world_par, &mut graph_par, &rules, 10_000);
    let dt_par = t0.elapsed();

    println!("  Parallel:   {:>8} events in {:>8.2?}", n_par, dt_par);

    let speedup = dt_seq.as_secs_f64() / dt_par.as_secs_f64();
    println!("\n  Speedup: {:.2}x", speedup);

    // --- Verify identical ---
    let mut mismatches = 0;
    let spc_side = (sand_per_chunk as f64).sqrt().ceil() as i64;
    let mut chunk_idx = 0;
    for cx in 0..side {
        for cz in 0..side {
            if chunk_idx >= chunks { break; }
            for sx in 0..spc_side {
                for sz in 0..spc_side {
                    let x = (cx as i64) * 16 + sx * 4 + 2;
                    let z = (cz as i64) * 16 + sz * 4 + 2;
                    for y in 0..=drop_height {
                        let pos = BlockPos::new(x, y, z);
                        if world_seq.get_block(pos) != world_par.get_block(pos) {
                            mismatches += 1;
                        }
                    }
                }
            }
            chunk_idx += 1;
        }
    }

    if mismatches == 0 {
        println!("  Verification: PASS (worlds identical)");
    } else {
        println!("  Verification: FAIL ({} mismatches!)", mismatches);
    }
}

fn build_world(side: i32) -> World {
    let world = World::new();
    for cx in 0..side {
        for cz in 0..side {
            let mut chunk = Chunk::new();
            for x in 0..SECTION_SIZE as u8 {
                for z in 0..SECTION_SIZE as u8 {
                    chunk.set_block(LocalBlockPos { x, y: 0, z }, block::BEDROCK);
                    for y in 1..=3i64 {
                        chunk.set_block(LocalBlockPos { x, y, z }, block::STONE);
                    }
                    chunk.set_block(LocalBlockPos { x, y: 4, z }, block::DIRT);
                }
            }
            world.insert_chunk(ChunkPos::new(cx, cz), chunk);
        }
    }
    world
}

fn build_graph(chunks: usize, side: i32, sand_per_chunk: usize, drop_height: i64) -> CausalGraph {
    let mut graph = CausalGraph::new();
    let spc_side = (sand_per_chunk as f64).sqrt().ceil() as i64;
    let mut chunk_idx = 0;
    for cx in 0..side {
        for cz in 0..side {
            if chunk_idx >= chunks { break; }
            // Place sand_per_chunk sand blocks in a grid within this chunk.
            for sx in 0..spc_side {
                for sz in 0..spc_side {
                    let x = (cx as i64) * 16 + sx * 4 + 2;
                    let z = (cz as i64) * 16 + sz * 4 + 2;
                    graph.insert_root(Event {
                        payload: EventPayload::BlockSet {
                            pos: BlockPos::new(x, drop_height, z),
                            old: block::AIR,
                            new: block::SAND,
                        },
                    });
                }
            }
            chunk_idx += 1;
        }
    }
    graph
}
