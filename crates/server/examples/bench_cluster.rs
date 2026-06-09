//! Phase 6f: two-PROCESS physics benchmark.
//!
//! Spawns a real peer process (`physics_peer`), splits the world's
//! regions across the two OS processes, runs the sand-rain workload, and
//! compares wall time + outcome against a single-process run. The
//! partition boundary between the processes is the cluster TCP protocol.
//!
//! Run with: `cargo run --release --example bench_cluster`
//! (both processes share one machine here, so this measures protocol
//! overhead and correctness, not added hardware)

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ultimate_engine::causal::event::{Event, EventPayload};
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::chunk::Chunk;
use ultimate_engine::world::position::{BlockPos, ChunkPos, LocalBlockPos};
use ultimate_engine::world::World;

use ultimate_server::block;
use ultimate_server::cluster::ClusterMesh;
use ultimate_server::physics::{self, ClusterCtx, PhysicsOptions};

const R: i32 = 12;

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

fn sand_rain() -> Vec<Event> {
    let mut events = Vec::new();
    let start = -(R as i64) * 16 + 4;
    let cells = ((R as i64) * 32 - 8) / 6;
    for i in 0..cells {
        for j in 0..cells {
            let (x, z) = (start + i * 6, start + j * 6);
            for y in 10..13i64 {
                events.push(Event {
                    payload: EventPayload::BlockSet {
                        pos: BlockPos::new(x, y, z),
                        old: block::AIR,
                        new: block::SAND,
                    },
                });
            }
        }
    }
    events
}

fn spawn_peer(addr: &str, workers: usize) -> Child {
    let me = std::env::current_exe().expect("current_exe");
    let peer_exe = me.with_file_name(format!(
        "physics_peer{}",
        std::env::consts::EXE_SUFFIX
    ));
    Command::new(peer_exe)
        .args([addr, &workers.to_string(), &R.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .spawn()
        .expect("spawn physics_peer (build examples first)")
}

fn run_single(workers: usize, events: Vec<Event>) -> (Duration, u64, Arc<World>) {
    let world = flat_world(R);
    let bus_tx = ultimate_server::event_bus::SpatialBus::new();
    let handle = physics::start(
        Arc::clone(&world),
        ultimate_server::rules::standard,
        bus_tx,
        None,
        PhysicsOptions { workers, rebalance: false, ..Default::default() },
    );
    let t0 = Instant::now();
    handle.submit_events(events);
    while handle.pending() != 0 {
        std::hint::spin_loop();
    }
    (t0.elapsed(), handle.executed_total(), world)
}

fn run_cluster(addr: &str, local_workers: usize, peer_workers: usize, events: Vec<Event>) -> (Duration, u64, Arc<World>, Child) {
    let mut peer = spawn_peer(addr, peer_workers);
    std::thread::sleep(Duration::from_millis(600)); // let it bind

    // We are node 1 of 2: dial the peer (node 0); accept nobody (our
    // listener is a throwaway required by the symmetric form() API).
    let throwaway = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let mesh = ClusterMesh::form(1, 2, &throwaway, &[addr.to_string()]).expect("mesh");
    let world = flat_world(R);
    let bus_tx = ultimate_server::event_bus::SpatialBus::new();
    let handle = physics::start(
        Arc::clone(&world),
        ultimate_server::rules::standard,
        Arc::clone(&bus_tx),
        None,
        PhysicsOptions {
            workers: local_workers,
            rebalance: false,
            cluster: Some(ClusterCtx { mesh: Arc::clone(&mesh) }),
            ..Default::default()
        },
    );
    mesh.attach(Arc::clone(&world), Arc::clone(&bus_tx), handle.clone());

    let t0 = Instant::now();
    handle.submit_events(events);
    assert!(
        mesh.wait_global_quiet(&handle, Duration::from_secs(120)),
        "cluster failed to quiesce",
    );
    let elapsed = t0.elapsed();
    let executed = handle.executed_total(); // node 0 share only
    let _ = peer.stdin.take(); // closing stdin lets the peer exit
    (elapsed, executed, world, peer)
}

fn main() {
    println!("=== Ultimate Minecraft: two-process physics (Phase 6f prototype) ===");
    println!("arena {}x{} chunks | workload: sand-rain | boundary: TCP cluster protocol", 2 * R, 2 * R);
    println!();

    let (t_single, ev_single, world_ref) = run_single(8, sand_rain());
    println!(
        "  single process, 8 workers:        {:>7.1} ms ({} events)",
        t_single.as_secs_f64() * 1e3,
        ev_single,
    );

    let (t_cluster, ev_node0, world_c, mut peer) =
        run_cluster("127.0.0.1:25611", 4, 4, sand_rain());
    println!(
        "  two processes, 4+4 workers (TCP): {:>7.1} ms (node-0 executed {} events)",
        t_cluster.as_secs_f64() * 1e3,
        ev_node0,
    );

    // Outcome equality: node 0's world (own regions + replica) vs the
    // single-process reference. Blocks only.
    let mut mismatches = 0usize;
    for cx in -R..R {
        for cz in -R..R {
            for x in 0..16u8 {
                for z in 0..16u8 {
                    for y in 0..12i64 {
                        let pos = BlockPos::new(cx as i64 * 16 + x as i64, y, cz as i64 * 16 + z as i64);
                        if world_ref.get_block(pos) != world_c.get_block(pos) {
                            mismatches += 1;
                        }
                    }
                }
            }
        }
    }
    println!();
    if mismatches == 0 {
        println!("  determinism: two-process world identical to single-process world ✓");
    } else {
        println!("  DIVERGENCE: {} cells differ!", mismatches);
    }
    println!(
        "  protocol overhead on one machine: {:.0}% wall-time vs single process",
        100.0 * (t_cluster.as_secs_f64() / t_single.as_secs_f64() - 1.0),
    );
    println!(
        "  node-0 executed {:.0}% of events; the rest ran in the peer process",
        100.0 * ev_node0 as f64 / ev_single as f64,
    );

    let _ = peer.wait();
}
