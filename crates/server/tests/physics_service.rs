//! Phase 6b-1/6b-2 integration tests: the partitioned physics service.
//!
//! The service runs on its own OS threads, so these tests poll the world
//! (with a generous timeout) for the expected post-cascade state and read
//! the broadcast bus synchronously via `try_recv`. Most tests run with 4
//! partition workers so routing is genuinely exercised; the FIFO-dependent
//! stale test keeps its actions within one chunk (one owner).

use std::sync::Arc;
use std::time::{Duration, Instant};


use ultimate_engine::causal::event::{Event, EventPayload};
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::chunk::Chunk;
use ultimate_engine::world::position::{BlockPos, ChunkPos, LocalBlockPos};
use ultimate_engine::world::World;

use ultimate_server::block;
use ultimate_server::event_bus::ChangeSource;
use ultimate_server::physics::{self, BlockAction};

/// Flat world: stone y=0..=3, dirt at y=4, across a few chunks.
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

/// Poll until `cond` holds or 2 s elapse. Returns whether it held.
fn wait_for(cond: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    cond()
}

#[test]
fn block_action_cascades_and_broadcasts() {
    let world = flat_world(2);
    let bus = ultimate_server::event_bus::SpatialBus::new();
    // A spatially-subscribed observer near the action's region.
    let (mut sub, mut rx) = bus.subscribe();
    sub.set_view(0, 0, 4);
    let handle = physics::start(
        Arc::clone(&world),
        ultimate_server::rules::standard,
        Arc::clone(&bus),
        None,
        physics::PhysicsOptions { workers: 4, ..Default::default() },
    );

    // Place sand in the air; gravity must walk it down to the surface.
    handle.submit_action(BlockAction {
        pos: BlockPos::new(8, 10, 8),
        old: block::AIR,
        new: block::SAND,
        update_stairs: false,
    });

    assert!(
        wait_for(|| world.get_block(BlockPos::new(8, 5, 8)) == block::SAND),
        "sand should land at y=5 via the physics service",
    );
    assert_eq!(world.get_block(BlockPos::new(8, 10, 8)), block::AIR);

    // The spatial bus must carry the cascade as Physics-sourced batches
    // including the final landing position.
    let mut landed = false;
    while let Ok(msg) = rx.try_recv() {
        if let ultimate_server::event_bus::SpatialMsg::World(batch) = &*msg {
            assert_eq!(batch.source, ChangeSource::Physics);
            if batch.changes.iter().any(|&(p, b)| p == BlockPos::new(8, 5, 8) && b == block::SAND) {
                landed = true;
            }
        }
    }
    assert!(landed, "spatial delivery should contain the sand landing");
}

#[test]
fn cross_source_actions_share_one_graph() {
    // Two "players" act on the same column through the shared service:
    // A places sand on the surface, then B breaks the dirt underneath.
    // The sand must fall into the gap — causality across sources.
    let world = flat_world(2);
    let bus_tx = ultimate_server::event_bus::SpatialBus::new();
    let handle = physics::start(
        Arc::clone(&world),
        ultimate_server::rules::standard,
        bus_tx,
        None,
        physics::PhysicsOptions { workers: 4, ..Default::default() },
    );

    // Player A: sand placed hovering above the surface; it falls onto the dirt.
    handle.submit_action(BlockAction {
        pos: BlockPos::new(4, 8, 4),
        old: block::AIR,
        new: block::SAND,
        update_stairs: false,
    });
    assert!(
        wait_for(|| world.get_block(BlockPos::new(4, 5, 4)) == block::SAND),
        "sand should land on the dirt surface first",
    );

    // Player B: break the dirt under the sand. The notify fan-out must
    // re-trigger gravity and drop the sand into the hole.
    handle.submit_action(BlockAction {
        pos: BlockPos::new(4, 4, 4),
        old: block::DIRT,
        new: block::AIR,
        update_stairs: false,
    });
    assert!(
        wait_for(|| world.get_block(BlockPos::new(4, 4, 4)) == block::SAND),
        "sand should fall into the hole left by the other player's break",
    );
    assert_eq!(world.get_block(BlockPos::new(4, 5, 4)), block::AIR);
}

#[test]
fn stale_action_is_dropped() {
    // An action whose `old` observation no longer matches the world must
    // be skipped by the stale-precondition guard — no write, no cascade.
    let world = flat_world(1);
    let bus_tx = ultimate_server::event_bus::SpatialBus::new();
    let handle = physics::start(
        Arc::clone(&world),
        ultimate_server::rules::standard,
        bus_tx,
        None,
        physics::PhysicsOptions { workers: 4, ..Default::default() },
    );

    // Claims the surface cell is AIR, but it's DIRT: stale.
    handle.submit_action(BlockAction {
        pos: BlockPos::new(2, 4, 2),
        old: block::AIR,
        new: block::SAND,
        update_stairs: false,
    });

    // Submit a sentinel action afterwards; when IT completes we know the
    // stale one has been processed (single-threaded service, FIFO).
    handle.submit_action(BlockAction {
        pos: BlockPos::new(10, 10, 10),
        old: block::AIR,
        new: BlockId::new(1),
        update_stairs: false,
    });
    assert!(wait_for(|| world.get_block(BlockPos::new(10, 10, 10)) == BlockId::new(1)));

    assert_eq!(
        world.get_block(BlockPos::new(2, 4, 2)),
        block::DIRT,
        "stale action must not overwrite the cell",
    );
}

/// Wait until the service reports global quiescence (pending == 0 with a
/// settle re-check) or 5 s elapse.
fn wait_quiet(handle: &ultimate_server::physics::PhysicsHandle) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if handle.pending() == 0 {
            std::thread::sleep(Duration::from_millis(2));
            if handle.pending() == 0 {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

#[test]
fn cross_partition_cascade_matches_single_worker() {
    // A water source exactly on a region border (chunk (0,0) / (-1,*)
    // boundary at world x=0) spreads radius 7 into chunks owned by
    // different workers. With confluent fluid rules the final field is
    // deterministic, so the 4-worker result must equal the 1-worker one.
    let snapshot = |workers: usize| -> Vec<BlockId> {
        let world = flat_world(2);
        let bus_tx = ultimate_server::event_bus::SpatialBus::new();
        let handle = physics::start(
            Arc::clone(&world),
            ultimate_server::rules::standard,
            bus_tx,
            None,
            // Static assignment for strict 1-vs-4 determinism comparison
            // (rebalancing handoffs are timing-dependent; confluence makes
            // them converge, but the test asserts the cleaner property).
            physics::PhysicsOptions { workers, rebalance: false, ..Default::default() },
        );
        handle.submit_action(BlockAction {
            pos: BlockPos::new(0, 8, 0),
            old: block::AIR,
            new: block::WATER,
            update_stairs: false,
        });
        assert!(wait_quiet(&handle), "{workers}-worker service should quiesce");
        let mut snap = Vec::new();
        for x in -9..=9i64 {
            for z in -9..=9i64 {
                snap.push(world.get_block(BlockPos::new(x, 5, z)));
            }
        }
        // Sanity: water actually landed and spread.
        assert!(snap.iter().any(|b| *b != block::AIR && *b != block::DIRT));
        snap
    };

    assert_eq!(
        snapshot(1),
        snapshot(4),
        "cross-partition execution must converge to the single-owner result",
    );
}

#[test]
fn pending_counter_reaches_zero_after_burst() {
    // A burst of independent actions across many regions: the in-flight
    // counter must return to exactly zero (no lost or double counts).
    let world = flat_world(4);
    let bus_tx = ultimate_server::event_bus::SpatialBus::new();
    let handle = physics::start(
        Arc::clone(&world),
        ultimate_server::rules::standard,
        bus_tx,
        None,
        physics::PhysicsOptions { workers: 4, ..Default::default() },
    );

    for i in 0..40i64 {
        handle.submit_action(BlockAction {
            pos: BlockPos::new(-60 + i * 3, 10, -60 + i * 3),
            old: block::AIR,
            new: block::SAND,
            update_stairs: false,
        });
    }
    assert!(wait_quiet(&handle), "burst should fully drain to pending == 0");
    assert!(handle.executed_total() > 0);
}

#[test]
fn priority_action_publishes_before_background_flood_finishes() {
    // Submit a large background flood, then a player action far away on
    // the SAME worker (workers=1 forces contention). Per-step publishing
    // + the priority lane must deliver the action's block change on the
    // bus before the flood's final change.
    let world = flat_world(4);
    let bus = ultimate_server::event_bus::SpatialBus::new();
    // Subscriber covering BOTH the storm area and the player position.
    let (mut sub, mut rx) = bus.subscribe();
    sub.set_view(0, 0, 8);
    let handle = physics::start(
        Arc::clone(&world),
        ultimate_server::rules::standard,
        Arc::clone(&bus),
        None,
        physics::PhysicsOptions { workers: 1, rebalance: false, ..Default::default() },
    );

    // Background: a big sand storm (raw events → priority 0).
    let mut storm = Vec::new();
    for i in 0..30i64 {
        for j in 0..30i64 {
            for y in 10..13i64 {
                storm.push(Event {
                    payload: EventPayload::BlockSet {
                        pos: BlockPos::new(-60 + i * 2, y, -60 + j * 2),
                        old: block::AIR,
                        new: block::SAND,
                    },
                });
            }
        }
    }
    handle.submit_events(storm);

    // Player action (priority 1) submitted right behind it.
    let player_pos = BlockPos::new(40, 4, 40);
    handle.submit_action(BlockAction {
        pos: player_pos,
        old: block::DIRT,
        new: block::AIR,
        update_stairs: false,
    });

    assert!(wait_quiet(&handle));

    // Find the message index carrying the player change vs the last one
    // carrying a storm change. (Spatial publishes happen sequentially on
    // the single worker, so arrival order reflects publish order.)
    let mut player_batch = None;
    let mut last_storm_batch = None;
    let mut i = 0usize;
    while let Ok(msg) = rx.try_recv() {
        if let ultimate_server::event_bus::SpatialMsg::World(batch) = &*msg {
            for &(pos, _) in batch.changes.iter() {
                if pos == player_pos {
                    player_batch.get_or_insert(i);
                }
                if pos.y == 5 && pos.x < 0 && pos.z < 0 {
                    last_storm_batch = Some(i);
                }
            }
        }
        i += 1;
    }
    let (p, s) = (
        player_batch.expect("player change must be published"),
        last_storm_batch.expect("storm changes must be published"),
    );
    assert!(
        p < s,
        "player action (batch {p}) should publish before the storm finishes (batch {s})",
    );
}

#[test]
fn raw_event_submission_runs_cascades() {
    // Simulation-layer path: raw root events, no notify fan-out.
    let world = flat_world(2);
    let bus_tx = ultimate_server::event_bus::SpatialBus::new();
    let handle = physics::start(
        Arc::clone(&world),
        ultimate_server::rules::standard,
        bus_tx,
        None,
        physics::PhysicsOptions { workers: 4, ..Default::default() },
    );

    handle.submit_events(vec![Event {
        payload: EventPayload::BlockSet {
            pos: BlockPos::new(-8, 12, -8),
            old: block::AIR,
            new: block::SAND,
        },
    }]);

    assert!(
        wait_for(|| world.get_block(BlockPos::new(-8, 5, -8)) == block::SAND),
        "raw-event sand should cascade to the surface",
    );
}
