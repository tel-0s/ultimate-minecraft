//! Phase 6b-2: partitioned physics scaling benchmark.
//!
//! Submits the 6b-0 workloads through the REAL physics service at
//! increasing partition-worker counts and measures wall time to global
//! quiescence (`pending() == 0`). This is the number 6b-2 exists to move:
//! the 6b-0 baseline showed the old shared-graph scheduler at 1.05-1.15×
//! on cheap-rule workloads regardless of core count, because graph
//! mutation was a single serial gather. Partition workers each own their
//! graph, so mutation itself scales.
//!
//! Run with: `cargo run --release --example bench_partitioned`

use std::sync::Arc;
use std::time::{Duration, Instant};

use ultimate_engine::causal::event::{Event, EventPayload};
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::chunk::Chunk;
use ultimate_engine::world::position::{BlockPos, ChunkPos, LocalBlockPos};
use ultimate_engine::world::World;

use ultimate_server::block;
use ultimate_server::physics;

const R: i32 = 12; // arena half-width in chunks (matches bench_baseline)

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
                    chunk.set_block(LocalBlockPos { x, y: 4, z }, block::DIRT);
                }
            }
            world.insert_chunk(ChunkPos::new(cx, cz), chunk);
        }
    }
    Arc::new(world)
}

fn block_set(pos: BlockPos, old: BlockId, new: BlockId) -> Event {
    Event { payload: EventPayload::BlockSet { pos, old, new } }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn range(&mut self, n: u64) -> u64 { self.next() % n }
}

/// Sand rain (6-block grid, 3-stacks): vertical cascades, 100% chunk-local
/// causality, near-zero rule cost — the workload that exposed the serial
/// gather (1.11× at 32 cores in 6b-0).
fn sand_rain() -> Vec<Event> {
    let mut rng = Rng(0x5EED_5A4D);
    let mut events = Vec::new();
    let start = -(R as i64) * 16 + 4;
    let cells = ((R as i64) * 32 - 8) / 6;
    for i in 0..cells {
        for j in 0..cells {
            let x = start + i * 6 + rng.range(3) as i64;
            let z = start + j * 6 + rng.range(3) as i64;
            for y in 10..13i64 {
                events.push(block_set(BlockPos::new(x, y, z), block::AIR, block::SAND));
            }
        }
    }
    events
}

/// Water flood (18-block grid): fronts cross chunk and region borders.
fn water_flood() -> Vec<Event> {
    let mut rng = Rng(0x5EED_AA77);
    let mut events = Vec::new();
    let start = -(R as i64) * 16 + 9;
    let cells = ((R as i64) * 32 - 18) / 18;
    for i in 0..cells {
        for j in 0..cells {
            let x = start + i * 18 + rng.range(2) as i64;
            let z = start + j * 18 + rng.range(2) as i64;
            events.push(block_set(BlockPos::new(x, 8, z), block::AIR, block::WATER));
        }
    }
    events
}

/// Mixed: sand + water + torches on the 40-block super-grid.
fn mixed() -> Vec<Event> {
    let torch = block::block_id_from_name("minecraft:torch").expect("torch resolves");
    let mut events = Vec::new();
    let start = -(R as i64) * 16 + 10;
    let cells = ((R as i64) * 32 - 20) / 40;
    for i in 0..cells {
        for j in 0..cells {
            let (bx, bz) = (start + i * 40, start + j * 40);
            events.push(block_set(BlockPos::new(bx, 8, bz), block::AIR, block::WATER));
            for (sx, sz) in [(bx + 20, bz), (bx, bz + 20)] {
                for y in 10..13i64 {
                    events.push(block_set(BlockPos::new(sx, y, sz), block::AIR, block::SAND));
                }
            }
            if (i + j) % 2 == 0 {
                events.push(block_set(BlockPos::new(bx + 20, 5, bz + 20), block::AIR, torch));
            }
        }
    }
    events
}

/// Run one (scenario, worker-count) cell: submit everything, wait for
/// global quiescence, return (wall time, executed events, world snapshot).
/// Static assignment (rebalance off) so worker counts are comparable.
fn run_cell(events: Vec<Event>, workers: usize) -> (Duration, u64, Vec<BlockId>) {
    let world = flat_world(R);
    // No bus subscribers: sends fail fast and are ignored by the service.
    let bus_tx = ultimate_server::event_bus::SpatialBus::new();
    let handle = physics::start(
        Arc::clone(&world),
        ultimate_server::rules::standard,
        bus_tx,
        None,
        physics::PhysicsOptions { workers, rebalance: false, ..Default::default() },
    );

    let t0 = Instant::now();
    handle.submit_events(events);
    let deadline = t0 + Duration::from_secs(120);
    loop {
        if handle.pending() == 0 {
            break;
        }
        if Instant::now() > deadline {
            panic!("scenario did not quiesce within 120 s at {} workers", workers);
        }
        std::hint::spin_loop();
    }
    let elapsed = t0.elapsed();
    let executed = handle.executed_total();

    // Block-state snapshot (y=0..16 across the arena) for cross-worker
    // determinism checks. Light is excluded: the light BFS races across
    // partitions by design (documented ownership exception).
    let mut snap = Vec::new();
    for cx in -R..R {
        for cz in -R..R {
            let chunk = world.get_chunk(&ChunkPos::new(cx, cz)).unwrap();
            for x in 0..16u8 {
                for z in 0..16u8 {
                    for y in 0..16i64 {
                        snap.push(chunk.get_block(LocalBlockPos { x, y, z }));
                    }
                }
            }
        }
    }
    (elapsed, executed, snap)
}

fn main() {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    println!("=== Ultimate Minecraft: Partitioned Physics Scaling (Phase 6b-2) ===");
    println!("arena: {}x{} chunks | logical cores: {} | region: 4x4 chunks", 2 * R, 2 * R, cores);
    println!();

    let scenarios: Vec<(&str, fn() -> Vec<Event>, bool)> = vec![
        // (name, builder, verify-determinism)
        ("sand-rain", sand_rain, true),
        ("water-flood", water_flood, true),
        ("mixed", mixed, true),
    ];
    let worker_counts = [1usize, 2, 4, 8, 16];

    for (name, builder, verify) in &scenarios {
        println!("{name}:");
        println!("  {:>8} {:>10} {:>10} {:>12} {:>9}", "workers", "ms", "events", "ev/s", "speedup");
        let mut base_time: Option<f64> = None;
        let mut reference_snap: Option<Vec<BlockId>> = None;
        for &w in &worker_counts {
            let (elapsed, executed, snap) = run_cell(builder(), w);
            if *verify {
                match &reference_snap {
                    None => reference_snap = Some(snap),
                    Some(r) => assert_eq!(
                        r, &snap,
                        "{name}: {w}-worker world diverged from 1-worker world",
                    ),
                }
            }
            let secs = elapsed.as_secs_f64();
            let speedup = base_time.map(|b| b / secs).unwrap_or(1.0);
            if base_time.is_none() {
                base_time = Some(secs);
            }
            println!(
                "  {:>8} {:>10.2} {:>10} {:>12.0} {:>8.2}x",
                w, secs * 1e3, executed, executed as f64 / secs, speedup,
            );
        }
        println!("  determinism: block state identical across all worker counts ✓");
        println!();
    }

    hotspot_comparison();
}

/// Phase 6d: sustained single-region hotspot — every event lands in the
/// 4×4-chunk region at the origin, which under static assignment belongs
/// to exactly ONE worker no matter how many exist. The adaptive
/// rebalancer detects the dominating region and splits it into per-chunk
/// ownership, spreading the load. Submitted in waves (sustained load,
/// the case rebalancing exists for).
fn hotspot_comparison() {
    const WAVES: usize = 40;
    const WORKERS: usize = 8;

    let wave = |seed: u64| -> Vec<Event> {
        let mut rng = Rng(seed);
        let mut events = Vec::new();
        // Columns inside chunks 0..4 × 0..4 (one region), 2-block grid.
        for i in 0..32i64 {
            for j in 0..32i64 {
                let x = i * 2 + (rng.range(2) as i64);
                let z = j * 2 + (rng.range(2) as i64);
                for y in 10..13i64 {
                    events.push(block_set(BlockPos::new(x, y, z), block::AIR, block::SAND));
                }
            }
        }
        events
    };

    let run = |rebalance: bool| -> (Duration, u64) {
        let world = flat_world(R);
        let bus_tx = ultimate_server::event_bus::SpatialBus::new();
        let handle = physics::start(
            Arc::clone(&world),
            ultimate_server::rules::standard,
            bus_tx,
            None,
            physics::PhysicsOptions { workers: WORKERS, rebalance, ..Default::default() },
        );
        let t0 = Instant::now();
        for w in 0..WAVES {
            handle.submit_events(wave(w as u64));
            // Sustained load: brief gap between waves, no quiescence wait.
            std::thread::sleep(Duration::from_millis(2));
        }
        let deadline = t0 + Duration::from_secs(120);
        while handle.pending() != 0 {
            if Instant::now() > deadline {
                panic!("hotspot did not quiesce (rebalance={rebalance})");
            }
            std::hint::spin_loop();
        }
        (t0.elapsed(), handle.executed_total())
    };

    println!("hotspot (all load in ONE region, {WORKERS} workers, {WAVES} waves):");
    let (t_static, ev_static) = run(false);
    let (t_adaptive, ev_adaptive) = run(true);
    println!(
        "  static assignment:   {:>8.1} ms  ({} events, {:.1}M ev/s)",
        t_static.as_secs_f64() * 1e3,
        ev_static,
        ev_static as f64 / t_static.as_secs_f64() / 1e6,
    );
    println!(
        "  adaptive (split):    {:>8.1} ms  ({} events, {:.1}M ev/s)",
        t_adaptive.as_secs_f64() * 1e3,
        ev_adaptive,
        ev_adaptive as f64 / t_adaptive.as_secs_f64() / 1e6,
    );
    println!(
        "  adaptive speedup: {:.2}x",
        t_static.as_secs_f64() / t_adaptive.as_secs_f64(),
    );
}
