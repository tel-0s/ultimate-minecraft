//! Phase 6f: a standalone physics peer node.
//!
//! Owns half the world's regions (node 1 of 2), runs its own partitioned
//! physics service against its own deterministic copy of the world, and
//! speaks the cluster protocol with the primary over TCP. No player
//! networking — pure physics capacity.
//!
//! Usage: physics_peer <listen_addr> <workers> <arena_radius>
//! (spawned by `bench_cluster`; runs until stdin closes)

use std::net::TcpListener;
use std::sync::Arc;


use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::chunk::Chunk;
use ultimate_engine::world::position::{ChunkPos, LocalBlockPos};
use ultimate_engine::world::World;

use ultimate_server::block;
use ultimate_server::cluster::ClusterMesh;
use ultimate_server::physics::{self, ClusterCtx, PhysicsOptions};

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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let addr = args.get(1).cloned().unwrap_or_else(|| "127.0.0.1:25600".into());
    let workers: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(4);
    let radius: i32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(12);
    // How many of the 2 nodes own regions: 2 = split (bench_cluster),
    // 1 = this peer owns EVERYTHING (gateway demo: node 1 is a gateway).
    let physics_nodes: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(2);

    let listener = TcpListener::bind(&addr).expect("peer bind");
    println!("peer: listening on {addr}");

    // Peer is node 0 of 2: it dials nobody (no lower ids) and accepts
    // the primary (node 1).
    let mesh = ClusterMesh::form_with_physics(0, 2, physics_nodes, &listener, &[])
        .expect("peer mesh");
    // World: flat arena by default (benches), or a real preset+seed so a
    // gateway's replica matches (worldgen is deterministic — both nodes
    // generate identical terrain locally).
    let world = match (args.get(5), args.get(6)) {
        (Some(preset), Some(seed)) => {
            let seed: u32 = seed.parse().expect("seed");
            let wg = ultimate_server::worldgen::preset::load(preset, seed).expect("preset");
            let world = Arc::new(World::new());
            wg.pregenerate_radius(&world, radius);
            println!("peer: pregenerated preset {preset:?} seed {seed} radius {radius}");
            world
        }
        _ => flat_world(radius),
    };
    // No tokio runtime here — the broadcast channel only needs senders.
    let bus_tx = ultimate_server::event_bus::SpatialBus::new();

    let handle = physics::start(
        Arc::clone(&world),
        ultimate_server::rules::standard,
        Arc::clone(&bus_tx),
        None,
        PhysicsOptions {
            workers,
            rebalance: false,
            cluster: Some(ClusterCtx { mesh: Arc::clone(&mesh) }),
            ..Default::default()
        },
    );
    mesh.attach(Arc::clone(&world), Arc::clone(&bus_tx), handle.clone());
    println!("peer: physics node 0/2 up ({workers} workers, radius {radius}, physics_nodes {physics_nodes})");

    // Park until the parent closes our stdin (process lifetime control).
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    println!("peer: exiting ({} events executed here)", handle.executed_total());
}
