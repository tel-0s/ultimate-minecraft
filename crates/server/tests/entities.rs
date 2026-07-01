//! Phase 5 integration tests: entities as causal actors (dropped items).
//!
//! All tests drive a `ManualClock` — trajectory segments, wakes, and the
//! 5-minute despawn horizon execute in virtual time (`clock.advance` +
//! `handle.kick`), so a full item lifecycle costs milliseconds of wall
//! clock and stays exactly deterministic.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ultimate_engine::causal::clock::ManualClock;
use ultimate_engine::causal::event::{Event, EventPayload};
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::chunk::Chunk;
use ultimate_engine::world::entity::{EntityId, EntityState};
use ultimate_engine::world::position::{BlockPos, ChunkPos, LocalBlockPos};
use ultimate_engine::world::World;

use ultimate_server::event_bus::{EntityChange, SpatialMsg};
use ultimate_server::physics::{self, BlockAction, PhysicsHandle};
use ultimate_server::rules::entity::KIND_ITEM;
use ultimate_server::block;

/// Flat world: stone y=0..=3, dirt at y=4, across a few chunks, with a
/// manual clock installed (must happen BEFORE physics::start — workers
/// capture the clock at startup).
fn flat_world_manual_clock(radius: i32) -> (Arc<World>, Arc<ManualClock>) {
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
    let clock = Arc::new(ManualClock::new());
    world.set_clock(clock.clone());
    (Arc::new(world), clock)
}

fn wait_for(cond: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    cond()
}

fn quiesce(handle: &PhysicsHandle) {
    assert!(wait_for(|| handle.pending() == 0), "physics should quiesce");
}

/// Advance virtual time and let due timers fire and settle.
fn advance_and_settle(clock: &ManualClock, handle: &PhysicsHandle, ms: u64) {
    clock.advance(ms * 1_000_000);
    handle.kick();
    quiesce(handle);
}

fn the_item(world: &World) -> (EntityId, EntityState) {
    let all = world.entities().snapshot();
    assert_eq!(all.len(), 1, "expected exactly one entity, got {}", all.len());
    all[0]
}

fn start(world: &Arc<World>, workers: usize) -> PhysicsHandle {
    physics::start(
        Arc::clone(world),
        ultimate_server::rules::standard,
        ultimate_server::event_bus::SpatialBus::new(),
        None,
        physics::PhysicsOptions { workers, rebalance: false, ..Default::default() },
    )
}

fn break_block(handle: &PhysicsHandle, pos: BlockPos, old: BlockId, drop: bool) {
    handle.submit_action(BlockAction {
        pos,
        old,
        new: block::AIR,
        update_stairs: false,
        drop_item: drop,
    });
}

// ── The full lifecycle in virtual time ───────────────────────────────────

#[test]
fn item_lifecycle_fall_rest_wake_despawn() {
    let (world, clock) = flat_world_manual_clock(2);
    let handle = start(&world, 4);

    // Break the dirt: the block vanishes and an item pops at the cell.
    break_block(&handle, BlockPos::new(8, 4, 8), block::DIRT, true);
    quiesce(&handle);

    let (id, s) = the_item(&world);
    assert_eq!(s.kind, KIND_ITEM);
    assert_eq!(ultimate_server::rules::entity::aux_block(s.aux), block::DIRT);
    assert!((s.pos.y - 4.5).abs() < 1e-9, "spawns at cell center");
    assert!(s.vel.y > 0.0, "pops upward");
    // Two parked timers: the trajectory segment + the despawn wake.
    assert_eq!(handle.pending_timed(), 2);

    // Half a virtual second later the ballistic arc has landed.
    advance_and_settle(&clock, &handle, 500);
    let (_, s) = the_item(&world);
    assert_eq!(s.vel, ultimate_engine::world::entity::Vec3::ZERO, "at rest");
    assert!((s.pos.y - 4.0).abs() < 1e-9, "rests on top of the stone at y=3, got {}", s.pos.y);
    assert_eq!(handle.pending_timed(), 1, "only the despawn timer remains");

    // Idle rest is causally free: LOTS of virtual time, zero events.
    let executed_before = handle.executed_total();
    advance_and_settle(&clock, &handle, 60_000);
    assert_eq!(
        handle.executed_total(),
        executed_before,
        "a resting entity must execute zero events"
    );

    // Break the floor under it: the wake re-plans and it falls again.
    break_block(&handle, BlockPos::new(8, 3, 8), BlockId::new(1), false);
    quiesce(&handle);
    advance_and_settle(&clock, &handle, 600);
    let (id2, s) = the_item(&world);
    assert_eq!(id, id2);
    assert!((s.pos.y - 3.0).abs() < 1e-9, "fell to the new floor, got {}", s.pos.y);
    assert_eq!(s.vel, ultimate_engine::world::entity::Vec3::ZERO);

    // Past the 5-minute horizon the despawn wake fires and the item dies.
    advance_and_settle(&clock, &handle, 301_000);
    assert!(world.entities().is_empty(), "item should despawn at the horizon");
    assert_eq!(handle.pending_timed(), 0);
}

// ── Exactly-once semantics ───────────────────────────────────────────────

#[test]
fn contested_break_drops_exactly_one_item() {
    let (world, _clock) = flat_world_manual_clock(2);
    let handle = start(&world, 1); // one owner: both actions serialize

    // Two players race to break the same block: one write wins the stale
    // guard, so exactly one item drops.
    break_block(&handle, BlockPos::new(8, 4, 8), block::DIRT, true);
    break_block(&handle, BlockPos::new(8, 4, 8), block::DIRT, true);
    quiesce(&handle);

    assert_eq!(world.entities().len(), 1, "exactly one item for a contested break");
}

#[test]
fn contested_pickup_despawns_exactly_once() {
    let (world, clock) = flat_world_manual_clock(2);
    let bus = ultimate_server::event_bus::SpatialBus::new();
    let (mut sub, mut rx) = bus.subscribe();
    sub.set_view(0, 0, 4);
    let handle = physics::start(
        Arc::clone(&world),
        ultimate_server::rules::standard,
        Arc::clone(&bus),
        None,
        physics::PhysicsOptions { workers: 4, rebalance: false, ..Default::default() },
    );

    break_block(&handle, BlockPos::new(8, 4, 8), block::DIRT, true);
    quiesce(&handle);
    advance_and_settle(&clock, &handle, 500);
    let (id, s) = the_item(&world);

    // Two players lunge for the item with the same observed state: the
    // first guarded despawn wins, the second dies at the guard.
    for _ in 0..2 {
        handle.submit_events(vec![Event {
            payload: EventPayload::EntitySet { id, old: Some(s), new: None },
        }]);
    }
    assert!(wait_for(|| world.entities().is_empty()), "item picked up");
    quiesce(&handle);

    let mut despawns = 0;
    while let Ok(msg) = rx.try_recv() {
        if let SpatialMsg::Entities(changes) = &*msg {
            despawns += changes
                .iter()
                .filter(|c| matches!(c, EntityChange::Despawn { .. }))
                .count();
        }
    }
    assert_eq!(despawns, 1, "exactly one authoritative despawn on the bus");
}

// ── FallingBlock: vanilla sand parity via entities ───────────────────────

fn start_falling(world: &Arc<World>, workers: usize) -> PhysicsHandle {
    physics::start(
        Arc::clone(world),
        ultimate_server::rules::standard_with_falling_blocks,
        ultimate_server::event_bus::SpatialBus::new(),
        None,
        physics::PhysicsOptions { workers, rebalance: false, ..Default::default() },
    )
}

#[test]
fn sand_detaches_falls_and_relands_as_block() {
    let (world, clock) = flat_world_manual_clock(2);
    let handle = start_falling(&world, 4);

    // Place sand high in the air: it detaches into an entity immediately.
    handle.submit_action(BlockAction {
        pos: BlockPos::new(8, 10, 8),
        old: block::AIR,
        new: block::SAND,
        update_stairs: false,
        drop_item: false,
    });
    quiesce(&handle);
    assert_eq!(world.get_block(BlockPos::new(8, 10, 8)), block::AIR, "block detached");
    assert_eq!(world.entities().len(), 1, "falling entity exists");

    // ~5 blocks of fall at g=20 ≈ 0.7 s; give it a virtual second, then
    // the landing conversion.
    advance_and_settle(&clock, &handle, 1500);
    assert!(world.entities().is_empty(), "entity converted back to a block");
    assert_eq!(
        world.get_block(BlockPos::new(8, 5, 8)),
        block::SAND,
        "sand re-landed exactly where instant gravity would put it"
    );
}

#[test]
fn stacked_sands_reland_as_a_stack() {
    let (world, clock) = flat_world_manual_clock(2);
    let handle = start_falling(&world, 4);

    // A floating 3-sand pillar: all three detach; the mid-flight entities
    // above must re-plan when the ones below re-land (wake-on-block-change
    // while MOVING — the re-plan keeps the original timeline and its
    // earlier-deadline segment wins at the guard).
    for y in [10, 11, 12] {
        handle.submit_action(BlockAction {
            pos: BlockPos::new(8, y, 8),
            old: block::AIR,
            new: block::SAND,
            update_stairs: false,
            drop_item: false,
        });
    }
    quiesce(&handle);

    // Settle in small steps so wakes and re-plans interleave realistically.
    for _ in 0..30 {
        advance_and_settle(&clock, &handle, 100);
    }

    assert!(world.entities().is_empty(), "all sand re-landed");
    for y in [5, 6, 7] {
        assert_eq!(
            world.get_block(BlockPos::new(8, y, 8)),
            block::SAND,
            "stack layer at y={y}"
        );
    }
    assert_eq!(world.get_block(BlockPos::new(8, 8, 8)), block::AIR);
}

#[test]
fn falling_matches_instant_gravity_final_state() {
    // The same scattered sand drop under both rule sets must produce the
    // same final block state — entity gravity changes pacing, not physics.
    let run = |falling: bool| {
        let (world, clock) = flat_world_manual_clock(2);
        let handle = if falling { start_falling(&world, 4) } else { start(&world, 4) };
        for (x, z, y) in [(3i64, 3i64, 9i64), (8, 8, 12), (12, 5, 7), (20, 20, 15)] {
            handle.submit_action(BlockAction {
                pos: BlockPos::new(x, y, z),
                old: block::AIR,
                new: block::SAND,
                update_stairs: false,
                drop_item: false,
            });
        }
        quiesce(&handle);
        for _ in 0..30 {
            advance_and_settle(&clock, &handle, 100);
        }
        assert!(world.entities().is_empty());
        let mut landed = Vec::new();
        for (x, z, _) in [(3i64, 3i64, 0i64), (8, 8, 0), (12, 5, 0), (20, 20, 0)] {
            for y in 0..20 {
                if world.get_block(BlockPos::new(x, y, z)) == block::SAND {
                    landed.push((x, y, z));
                }
            }
        }
        landed
    };
    assert_eq!(run(true), run(false), "entity and instant gravity must agree");
}

// ── Determinism across worker counts ─────────────────────────────────────

#[test]
fn item_trajectory_is_identical_across_worker_counts() {
    let mut outcomes = Vec::new();
    for workers in [1usize, 4] {
        let (world, clock) = flat_world_manual_clock(2);
        let handle = start(&world, workers);
        break_block(&handle, BlockPos::new(8, 4, 8), block::DIRT, true);
        quiesce(&handle);
        advance_and_settle(&clock, &handle, 1000);
        let (_, s) = the_item(&world);
        outcomes.push(s);
    }
    // Ids differ (allocation order), but the physical outcome must not.
    assert_eq!(outcomes[0].pos, outcomes[1].pos);
    assert_eq!(outcomes[0].vel, outcomes[1].vel);
    assert_eq!(outcomes[0].stamp, outcomes[1].stamp);
}

// ── Players in the EntityStore (Phase 5 unification) ─────────────────────

#[test]
fn player_mirror_lives_in_store_but_stays_off_the_bus() {
    let (world, _clock) = flat_world_manual_clock(2);
    let bus = ultimate_server::event_bus::SpatialBus::new();
    let (mut sub, mut rx) = bus.subscribe();
    sub.set_view(0, 0, 4);
    let handle = physics::start(
        Arc::clone(&world),
        ultimate_server::rules::standard,
        Arc::clone(&bus),
        None,
        physics::PhysicsOptions { workers: 4, rebalance: false, ..Default::default() },
    );

    let pid = ultimate_server::rules::entity::player_entity_id(7);
    let s0 = ultimate_server::rules::entity::player_state(
        ultimate_engine::world::entity::Vec3::new(8.5, 5.0, 8.5),
        90.0,
        10.0,
        world.now(),
    );
    handle.submit_events(vec![Event {
        payload: EventPayload::EntitySet { id: pid, old: None, new: Some(s0) },
    }]);
    assert!(wait_for(|| world.entities().get(pid).is_some()), "player mirrored");

    // Move: guarded on the store-current state, like the connection does.
    let cur = world.entities().get(pid).unwrap();
    let s1 = ultimate_server::rules::entity::player_state(
        ultimate_engine::world::entity::Vec3::new(20.5, 5.0, 20.5),
        180.0,
        0.0,
        world.now(),
    );
    handle.submit_events(vec![Event {
        payload: EventPayload::EntitySet { id: pid, old: Some(cur), new: Some(s1) },
    }]);
    assert!(
        wait_for(|| world.entities().get(pid).map(|s| s.pos.x) == Some(20.5)),
        "player position updates in the store"
    );
    // Rules can find the player spatially.
    assert!(world.entities().in_column(20, 20).contains(&pid));
    assert!(!world.entities().in_column(8, 8).contains(&pid));

    // Player transitions must NOT reach the spatial entity plane — their
    // render path is PlayerEvent::Moved via the registry.
    quiesce(&handle);
    while let Ok(msg) = rx.try_recv() {
        if let SpatialMsg::Entities(changes) = &*msg {
            assert!(changes.is_empty(), "player leaked onto the entity bus: {changes:?}");
        }
    }

    // Disconnect: guarded despawn.
    let cur = world.entities().get(pid).unwrap();
    handle.submit_events(vec![Event {
        payload: EventPayload::EntitySet { id: pid, old: Some(cur), new: None },
    }]);
    assert!(wait_for(|| world.entities().is_empty()), "player despawned on disconnect");
}

// ── Spatial projection ───────────────────────────────────────────────────

#[test]
fn spawn_and_landing_reach_spatial_subscribers() {
    let (world, clock) = flat_world_manual_clock(2);
    let bus = ultimate_server::event_bus::SpatialBus::new();
    let (mut sub, mut rx) = bus.subscribe();
    sub.set_view(0, 0, 4);
    let handle = physics::start(
        Arc::clone(&world),
        ultimate_server::rules::standard,
        Arc::clone(&bus),
        None,
        physics::PhysicsOptions { workers: 4, rebalance: false, ..Default::default() },
    );

    break_block(&handle, BlockPos::new(8, 4, 8), block::DIRT, true);
    quiesce(&handle);
    advance_and_settle(&clock, &handle, 500);

    let (mut saw_spawn, mut saw_move) = (false, false);
    while let Ok(msg) = rx.try_recv() {
        if let SpatialMsg::Entities(changes) = &*msg {
            for c in changes {
                match c {
                    EntityChange::Spawn { state, .. } => {
                        assert_eq!(state.kind, KIND_ITEM);
                        saw_spawn = true;
                    }
                    EntityChange::Move { state, .. } => {
                        // The landing segment endpoint.
                        assert!((state.pos.y - 4.0).abs() < 1e-9);
                        saw_move = true;
                    }
                    EntityChange::Despawn { .. } => {}
                }
            }
        }
    }
    assert!(saw_spawn, "clients must learn the item spawned");
    assert!(saw_move, "clients must learn the landing");
}
