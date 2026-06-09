//! Phase 6b-0: causal-engine baseline measurement harness.
//!
//! Establishes the numbers that 6b-1 (decoupled physics) and 6b-2
//! (partition ownership) must beat, and measures the one ratio the
//! partition design's economics depend on: how much causality crosses
//! chunk boundaries (cross-chunk edges become inter-partition messages).
//!
//! Run with: `cargo run --release --example bench_baseline`
//!
//! Per scenario: total events, sequential vs parallel wall time, speedup,
//! events/sec (and /core), causal-edge chunk locality, peak wavefront
//! width (live nodes under pruning), and effective block writes. Worlds
//! are verified identical between the sequential and parallel runs.
//! A separate section reports single-action quiescence latency and
//! causal propagation velocity.

use std::time::{Duration, Instant};

use ultimate_engine::causal::event::{Event, EventPayload};
use ultimate_engine::causal::graph::CausalGraph;
use ultimate_engine::causal::scheduler::Scheduler;
use ultimate_engine::rules::RuleSet;
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::chunk::Chunk;
use ultimate_engine::world::position::{BlockPos, ChunkPos, LocalBlockPos};
use ultimate_engine::world::World;

use ultimate_server::block;

/// Arena half-width in chunks: world spans [-R, R) on both axes.
const R: i32 = 12; // 24x24 chunks = 384x384 blocks
const MAX_STEPS: usize = 100_000;

// ── Deterministic scatter PRNG (SplitMix64) ─────────────────────────────────

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn range(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

// ── World construction ──────────────────────────────────────────────────────

/// Flat arena: stone y=0..=3, dirt at y=4, air above.
fn flat_world(radius: i32) -> World {
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
    world
}

fn block_set(pos: BlockPos, old: BlockId, new: BlockId) -> Event {
    Event { payload: EventPayload::BlockSet { pos, old, new } }
}

// ── Scenario root-event builders ────────────────────────────────────────────
//
// All scenarios are constructed NON-INTERACTING: feature spacing exceeds
// every interaction radius (water spread 7, torch light 14). This is
// deliberate. The first harness run revealed that *interacting* water
// fronts are not causally invariant under the current rule set — two
// fronts meeting settle at different (both locally stable) levels
// depending on arrival order, because the fluid rule never lowers
// existing water (vanilla's min-neighbor+1 re-level rule would restore
// confluence). Until that semantic gap is closed, seq-vs-par world
// verification is only meaningful for non-interacting workloads; the
// throughput/locality numbers are unaffected.

/// Sand rain: 3-block sand stacks dropped from y=10 on a 6-block grid
/// (unique columns; sand falls straight down, so columns never interact).
/// Gravity cascades are vertical → overwhelmingly intra-chunk causality.
fn roots_sand_rain(g: &mut CausalGraph) {
    let mut rng = Rng(0x5EED_5A4D);
    let start = -(R as i64) * 16 + 4;
    let cells = ((R as i64) * 32 - 8) / 6;
    for i in 0..cells {
        for j in 0..cells {
            // Per-column jitter of ±2 keeps chunk alignment honest while
            // preserving the 6-block pitch (columns stay unique).
            let x = start + i * 6 + rng.range(3) as i64;
            let z = start + j * 6 + rng.range(3) as i64;
            for y in 10..13i64 {
                g.insert_root(block_set(BlockPos::new(x, y, z), block::AIR, block::SAND));
            }
        }
    }
}

/// Water flood: sources at y=8 on an 18-block grid (jitter ±1) fall to the
/// surface and spread to radius 7 — fronts regularly cross chunk borders
/// but never touch each other (min source spacing 16 > 7 + 7).
fn roots_water_flood(g: &mut CausalGraph) {
    let mut rng = Rng(0x5EED_AA77);
    let start = -(R as i64) * 16 + 9;
    let cells = ((R as i64) * 32 - 18) / 18;
    for i in 0..cells {
        for j in 0..cells {
            let x = start + i * 18 + rng.range(2) as i64;
            let z = start + j * 18 + rng.range(2) as i64;
            g.insert_root(block_set(BlockPos::new(x, 8, z), block::AIR, block::WATER));
        }
    }
}

/// Border stress: water sources exactly on chunk-corner intersections, so
/// every spread quadrant lands in a different chunk — worst-case locality.
fn roots_border_flood(g: &mut CausalGraph) {
    let mut placed = 0;
    'outer: for cx in (-R + 1)..R {
        for cz in (-R + 1)..R {
            g.insert_root(block_set(
                BlockPos::new(cx as i64 * 16, 8, cz as i64 * 16),
                block::AIR,
                block::WATER,
            ));
            placed += 1;
            if placed >= 500 { break 'outer; }
        }
    }
}

/// Torch grid: light BFS floods ~14-block spheres of LightSet bookkeeping.
/// Torches spaced 48 apart so light fields never interact (keeps the
/// parallel run order-independent).
fn roots_torch_grid(g: &mut CausalGraph) {
    let torch = block::block_id_from_name("minecraft:torch").expect("torch resolves");
    let start = -(R as i64) * 16 + 24;
    for i in 0..6i64 {
        for j in 0..6i64 {
            let (x, z) = (start + i * 48, start + j * 48);
            g.insert_root(block_set(BlockPos::new(x, 5, z), block::AIR, torch));
        }
    }
}

/// Mixed: sand + water + torches interleaved on a 40-block super-grid —
/// closest to live-server load while keeping every feature outside every
/// other feature's interaction radius (water 7, light 14, sand 0).
fn roots_mixed(g: &mut CausalGraph) {
    let torch = block::block_id_from_name("minecraft:torch").expect("torch resolves");
    let start = -(R as i64) * 16 + 10;
    let cells = ((R as i64) * 32 - 20) / 40;
    for i in 0..cells {
        for j in 0..cells {
            let (bx, bz) = (start + i * 40, start + j * 40);
            // Water at the cell origin.
            g.insert_root(block_set(BlockPos::new(bx, 8, bz), block::AIR, block::WATER));
            // Two sand stacks 20 away on each axis.
            for (sx, sz) in [(bx + 20, bz), (bx, bz + 20)] {
                for y in 10..13i64 {
                    g.insert_root(block_set(BlockPos::new(sx, y, sz), block::AIR, block::SAND));
                }
            }
            // Torch on the cell diagonal (20 from each sand stack, 28 from
            // water), every other cell to keep light volume proportionate.
            if (i + j) % 2 == 0 {
                g.insert_root(block_set(BlockPos::new(bx + 20, 5, bz + 20), block::AIR, torch));
            }
        }
    }
}

// ── Measurement ─────────────────────────────────────────────────────────────

struct ScenarioReport {
    name: &'static str,
    events: usize,
    events_par: usize,
    t_seq: Duration,
    t_par: Duration,
    same_edges: u64,
    cross_edges: u64,
    peak_wavefront: usize,
    block_writes: usize,
}

fn run_scenario(
    name: &'static str,
    rules: &RuleSet,
    build_roots: impl Fn(&mut CausalGraph),
) -> ScenarioReport {
    let scheduler = Scheduler::new();

    // Sequential.
    let world_seq = flat_world(R);
    let mut g_seq = CausalGraph::with_pruning();
    build_roots(&mut g_seq);
    let t0 = Instant::now();
    let n_seq = scheduler.run_until_quiet(&world_seq, &mut g_seq, rules, MAX_STEPS);
    let t_seq = t0.elapsed();

    // Parallel.
    let world_par = flat_world(R);
    let mut g_par = CausalGraph::with_pruning();
    build_roots(&mut g_par);
    let t0 = Instant::now();
    let n_par = scheduler.run_until_quiet_parallel(&world_par, &mut g_par, rules, MAX_STEPS);
    let t_par = t0.elapsed();

    // Event counts may differ slightly between schedules: notify-dedup
    // coalescing depends on what's pending at insert time, and parallel
    // gathers consequents in batches. The invariant is WORLD STATE, which
    // must match exactly.
    verify_worlds_match(name, &world_seq, &world_par);

    let (same_edges, cross_edges) = g_seq.edge_locality();
    let block_writes = g_seq
        .write_log()
        .iter()
        .filter(|p| matches!(p, EventPayload::BlockSet { .. }))
        .count();

    ScenarioReport {
        name,
        events: n_seq,
        events_par: n_par,
        t_seq,
        t_par,
        same_edges,
        cross_edges,
        peak_wavefront: g_seq.peak_len(),
        block_writes,
    }
}

fn verify_worlds_match(name: &str, a: &World, b: &World) {
    for cx in -R..R {
        for cz in -R..R {
            let pos = ChunkPos::new(cx, cz);
            let (ca, cb) = (a.get_chunk(&pos).unwrap(), b.get_chunk(&pos).unwrap());
            for x in 0..16u8 {
                for z in 0..16u8 {
                    for y in 0..16i64 {
                        let lp = LocalBlockPos { x, y, z };
                        assert_eq!(
                            ca.get_block(lp),
                            cb.get_block(lp),
                            "{name}: seq/par divergence in chunk ({cx},{cz}) at local ({x},{y},{z})",
                        );
                    }
                }
            }
        }
    }
}

// ── Single-action latency + propagation velocity ────────────────────────────

/// Median quiescence latency for a single root action on a fresh small
/// world, plus blocks-changed and max horizontal propagation distance.
fn single_action(
    rules: &RuleSet,
    iters: usize,
    make_root: impl Fn() -> Event,
) -> (Duration, usize, f64) {
    let scheduler = Scheduler::new();
    let mut times = Vec::with_capacity(iters);
    let mut block_writes = 0usize;
    let mut max_dist = 0f64;

    for _ in 0..iters {
        let world = flat_world(2);
        let mut g = CausalGraph::with_pruning();
        let root = make_root();
        let origin = root.positions()[0];
        g.insert_root(root);
        let t0 = Instant::now();
        scheduler.run_until_quiet(&world, &mut g, rules, MAX_STEPS);
        times.push(t0.elapsed());

        let dist = |pos: &BlockPos| {
            (((pos.x - origin.x).pow(2)
                + (pos.y - origin.y).pow(2)
                + (pos.z - origin.z).pow(2)) as f64)
                .sqrt()
        };
        block_writes = 0;
        for p in g.write_log() {
            match p {
                EventPayload::BlockSet { pos, .. } | EventPayload::LightSet { pos, .. } => {
                    block_writes += 1;
                    max_dist = max_dist.max(dist(pos));
                }
                EventPayload::LightBatch { changes } => {
                    block_writes += changes.len();
                    for c in changes.iter() {
                        max_dist = max_dist.max(dist(&c.pos));
                    }
                }
                _ => {}
            }
        }
    }
    times.sort();
    (times[times.len() / 2], block_writes, max_dist)
}

// ── Main ────────────────────────────────────────────────────────────────────

fn main() {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let rules = ultimate_server::rules::standard();

    println!("=== Ultimate Minecraft: Causal Engine Baseline (Phase 6b-0) ===");
    println!("arena: {}x{} chunks | logical cores: {} | scheduler: snapshot-scatter-gather, batch {}",
        2 * R, 2 * R, cores, Scheduler::new().max_events_per_step);
    println!();

    let scenarios: Vec<(&'static str, Box<dyn Fn(&mut CausalGraph)>)> = vec![
        ("sand-rain", Box::new(roots_sand_rain)),
        ("water-flood", Box::new(roots_water_flood)),
        ("border-flood", Box::new(roots_border_flood)),
        ("torch-grid", Box::new(roots_torch_grid)),
        ("mixed", Box::new(roots_mixed)),
    ];

    println!("{:<14} {:>9} {:>9} {:>9} {:>8} {:>11} {:>13} {:>9} {:>10} {:>9}",
        "scenario", "events", "seq ms", "par ms", "speedup",
        "ev/s (seq)", "ev/s/core(p)", "locality", "peak-wave", "writes");

    let mut reports = Vec::new();
    for (name, build) in &scenarios {
        let r = run_scenario(name, &rules, build);
        let total_edges = r.same_edges + r.cross_edges;
        let locality = if total_edges > 0 {
            100.0 * r.same_edges as f64 / total_edges as f64
        } else {
            100.0
        };
        println!("{:<14} {:>9} {:>9.2} {:>9.2} {:>7.2}x {:>11.0} {:>13.0} {:>8.2}% {:>10} {:>9}",
            r.name,
            r.events,
            r.t_seq.as_secs_f64() * 1e3,
            r.t_par.as_secs_f64() * 1e3,
            r.t_seq.as_secs_f64() / r.t_par.as_secs_f64(),
            r.events as f64 / r.t_seq.as_secs_f64(),
            r.events_par as f64 / r.t_par.as_secs_f64() / cores as f64,
            locality,
            r.peak_wavefront,
            r.block_writes,
        );
        reports.push(r);
    }

    let total_events: usize = reports.iter().map(|r| r.events).sum();
    let total_seq: f64 = reports.iter().map(|r| r.t_seq.as_secs_f64()).sum();
    let total_par: f64 = reports.iter().map(|r| r.t_par.as_secs_f64()).sum();
    let (same, cross) = reports.iter().fold((0u64, 0u64), |acc, r| {
        (acc.0 + r.same_edges, acc.1 + r.cross_edges)
    });
    println!();
    println!("aggregate: {} events | seq {:.1} ms | par {:.1} ms | speedup {:.2}x | edge locality {:.2}% ({} same / {} cross)",
        total_events, total_seq * 1e3, total_par * 1e3, total_seq / total_par,
        100.0 * same as f64 / (same + cross) as f64, same, cross);

    // ── Single-action latency + propagation velocity ────────────────────
    println!();
    println!("single-action quiescence latency (median of 50, fresh world each):");
    let torch = block::block_id_from_name("minecraft:torch").expect("torch resolves");
    let actions: Vec<(&str, Box<dyn Fn() -> Event>)> = vec![
        ("sand drop (y=10)", Box::new(|| block_set(BlockPos::new(8, 10, 8), block::AIR, block::SAND))),
        ("water source (y=8)", Box::new(|| block_set(BlockPos::new(8, 8, 8), block::AIR, block::WATER))),
        ("torch place (y=5)", Box::new(move || block_set(BlockPos::new(8, 5, 8), block::AIR, torch))),
    ];
    for (name, make) in &actions {
        let (median, writes, max_dist) = single_action(&rules, 50, make);
        let velocity = max_dist / median.as_secs_f64();
        println!("  {:<20} {:>9.1?}   {:>6} writes   front {:>5.1} blocks → {:>12.0} blocks/sec",
            name, median, writes, max_dist, velocity);
    }
}
