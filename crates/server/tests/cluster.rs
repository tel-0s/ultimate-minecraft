//! Phase 6f integration tests: an N-node physics mesh over REAL TCP.
//!
//! All "nodes" live in this test process — full physics services with
//! their own worlds, graphs, and buses — but every boundary between them
//! is a genuine localhost socket speaking the cluster protocol.

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;


use ultimate_engine::causal::event::{Event, EventPayload};
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::chunk::Chunk;
use ultimate_engine::world::position::{BlockPos, ChunkPos, LocalBlockPos};
use ultimate_engine::world::World;

use ultimate_server::block;
use ultimate_server::cluster::{owner_node, ClusterMesh};
use ultimate_server::physics::{self, BlockAction, ClusterCtx, PhysicsHandle, PhysicsOptions};

const R: i32 = 8; // 16x16-chunk arena on every node

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

struct Node {
    world: Arc<World>,
    physics: PhysicsHandle,
    mesh: Arc<ClusterMesh>,
    _bus: Arc<ultimate_server::event_bus::SpatialBus>,
}

/// Form a full N-node mesh over localhost TCP and start a physics
/// service on every node.
fn form_cluster(total: u32, workers: usize) -> Vec<Node> {
    let listeners: Vec<TcpListener> = (0..total)
        .map(|_| TcpListener::bind("127.0.0.1:0").expect("bind"))
        .collect();
    let addrs: Vec<String> = listeners
        .iter()
        .map(|l| l.local_addr().expect("addr").to_string())
        .collect();

    let meshes: Vec<Arc<ClusterMesh>> = {
        let mut handles = Vec::new();
        for (id, listener) in listeners.into_iter().enumerate() {
            let addrs = addrs.clone();
            handles.push(std::thread::spawn(move || {
                ClusterMesh::form(id as u32, total, &listener, &addrs).expect("mesh form")
            }));
        }
        handles.into_iter().map(|h| h.join().expect("form thread")).collect()
    };

    meshes
        .into_iter()
        .map(|mesh| {
            let world = flat_world(R);
            let bus = ultimate_server::event_bus::SpatialBus::new();
            let physics = physics::start(
                Arc::clone(&world),
                ultimate_server::rules::standard,
                Arc::clone(&bus),
                None,
                PhysicsOptions {
                    workers,
                    rebalance: false,
                    cluster: Some(ClusterCtx { mesh: Arc::clone(&mesh) }),
                    ..Default::default()
                },
            );
            mesh.attach(Arc::clone(&world), Arc::clone(&bus), physics.clone());
            Node { world, physics, mesh, _bus: bus }
        })
        .collect()
}

/// A chunk inside the arena owned (by default hash) by the given node.
fn chunk_owned_by(node: u32, total: u32) -> ChunkPos {
    for cx in -(R - 2)..(R - 2) {
        for cz in -(R - 2)..(R - 2) {
            if owner_node(ChunkPos::new(cx, cz), total) == node {
                return ChunkPos::new(cx, cz);
            }
        }
    }
    panic!("no chunk owned by node {node} in arena");
}

fn wait_quiet(n: &Node) -> bool {
    n.mesh.wait_global_quiet(&n.physics, Duration::from_secs(20))
}

/// Run a sand+border-water workload on a single node (reference) and on
/// an N-node mesh; assert every node's world matches the reference.
fn match_single_node(total: u32) {
    // Water sources near node borders (computed against the N-node
    // hash), sand everywhere else — kept spatially disjoint because
    // sand-vs-water contention is order-dependent in ANY schedule.
    let mut water_spots = Vec::new();
    'scan: for cx in -(R - 2)..(R - 3) {
        for cz in -(R - 2)..(R - 2) {
            let here = owner_node(ChunkPos::new(cx, cz), total);
            let east = owner_node(ChunkPos::new(cx + 1, cz), total);
            if here != east {
                water_spots.push(BlockPos::new(cx as i64 * 16 + 14, 8, cz as i64 * 16 + 8));
                if water_spots.len() >= 4 {
                    break 'scan;
                }
            }
        }
    }
    assert!(!water_spots.is_empty(), "arena must contain a node border");

    let mut events = Vec::new();
    for w in &water_spots {
        events.push(Event {
            payload: EventPayload::BlockSet { pos: *w, old: block::AIR, new: block::WATER },
        });
    }
    for i in 0..24i64 {
        for j in 0..24i64 {
            let (x, z) = (-96 + i * 8, -96 + j * 8);
            if water_spots.iter().any(|w| (w.x - x).abs().max((w.z - z).abs()) <= 12) {
                continue;
            }
            events.push(Event {
                payload: EventPayload::BlockSet {
                    pos: BlockPos::new(x, 10, z),
                    old: block::AIR,
                    new: block::SAND,
                },
            });
        }
    }

    // Single-node reference.
    let ref_world = flat_world(R);
    let ref_bus = ultimate_server::event_bus::SpatialBus::new();
    let ref_physics = physics::start(
        Arc::clone(&ref_world),
        ultimate_server::rules::standard,
        ref_bus,
        None,
        PhysicsOptions { workers: 2, rebalance: false, ..Default::default() },
    );
    ref_physics.submit_events(events.clone());
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while ref_physics.pending() != 0 {
        assert!(std::time::Instant::now() < deadline, "reference run hung");
        std::thread::sleep(Duration::from_millis(2));
    }

    // The mesh, everything submitted through node 0.
    let nodes = form_cluster(total, 2);
    nodes[0].physics.submit_events(events);
    assert!(wait_quiet(&nodes[0]), "{total}-node mesh should reach global quiescence");

    let mut checked = 0usize;
    for cx in -R..R {
        for cz in -R..R {
            for x in (0..16u8).step_by(2) {
                for z in (0..16u8).step_by(2) {
                    for y in 0..12i64 {
                        let pos = BlockPos::new(
                            cx as i64 * 16 + x as i64,
                            y,
                            cz as i64 * 16 + z as i64,
                        );
                        let expect = ref_world.get_block(pos);
                        for (i, node) in nodes.iter().enumerate() {
                            assert_eq!(
                                node.world.get_block(pos), expect,
                                "node {i}/{total} divergence at {pos:?}",
                            );
                        }
                        checked += 1;
                    }
                }
            }
        }
    }
    assert!(checked > 150_000, "sanity: compared {checked} cells per node");
}

#[test]
fn action_crosses_nodes_and_mirrors_back() {
    let nodes = form_cluster(2, 2);

    let target = chunk_owned_by(1, 2);
    let pos = BlockPos::new(target.x as i64 * 16 + 8, 10, target.z as i64 * 16 + 8);

    nodes[0].physics.submit_action(BlockAction {
        pos,
        old: block::AIR,
        new: block::SAND,
        update_stairs: false,
    });
    assert!(wait_quiet(&nodes[0]));

    let landed = BlockPos::new(pos.x, 5, pos.z);
    assert_eq!(nodes[1].world.get_block(landed), block::SAND, "owner ran the cascade");
    assert_eq!(nodes[0].world.get_block(landed), block::SAND, "replica mirrored it");
    assert_eq!(nodes[0].world.get_block(pos), block::AIR);
}

#[test]
fn two_node_world_matches_single_node_run() {
    match_single_node(2);
}

#[test]
fn three_node_world_matches_single_node_run() {
    match_single_node(3);
}

#[test]
fn region_migration_moves_work_without_state_transfer() {
    let nodes = form_cluster(2, 2);

    // A region node 0 owns by hash; we'll migrate it to node 1 mid-load.
    let chunk = chunk_owned_by(0, 2);
    let region = (chunk.x >> 2, chunk.z >> 2);
    let base = BlockPos::new(
        (region.0 as i64) * 64 + 8, // region = 4 chunks = 64 blocks
        10,
        (region.1 as i64) * 64 + 8,
    );

    let wave = |y: i64| -> Vec<Event> {
        let mut v = Vec::new();
        for i in 0..8i64 {
            for j in 0..8i64 {
                v.push(Event {
                    payload: EventPayload::BlockSet {
                        pos: BlockPos::new(base.x + i * 4, y, base.z + j * 4),
                        old: block::AIR,
                        new: block::SAND,
                    },
                });
            }
        }
        v
    };

    // Wave 1: executes on node 0 (the hash owner).
    nodes[0].physics.submit_events(wave(10));
    assert!(wait_quiet(&nodes[0]));
    let n1_before = nodes[1].physics.executed_total();

    // Migrate — a pure ownership flip; replicas mean no state moves.
    nodes[0].mesh.migrate_region(region, 1);
    assert!(wait_quiet(&nodes[0]), "transfer frame must propagate");
    assert_eq!(nodes[0].mesh.owner(chunk), 1, "initiator sees the flip");
    assert_eq!(nodes[1].mesh.owner(chunk), 1, "peer sees the flip");

    // Wave 2 (stacks on wave 1's sand): must execute on node 1 now.
    nodes[0].physics.submit_events(wave(12));
    assert!(wait_quiet(&nodes[0]));
    let n1_after = nodes[1].physics.executed_total();
    assert!(
        n1_after > n1_before,
        "post-migration work must run on the new owner (node 1 executed {} then {})",
        n1_before, n1_after,
    );

    // Both worlds converged: two sand per column, stacked on the surface.
    for i in 0..8i64 {
        for j in 0..8i64 {
            let (x, z) = (base.x + i * 4, base.z + j * 4);
            for node in &nodes {
                assert_eq!(node.world.get_block(BlockPos::new(x, 5, z)), block::SAND);
                assert_eq!(node.world.get_block(BlockPos::new(x, 6, z)), block::SAND);
                assert_eq!(node.world.get_block(BlockPos::new(x, 7, z)), block::AIR);
            }
        }
    }
}
