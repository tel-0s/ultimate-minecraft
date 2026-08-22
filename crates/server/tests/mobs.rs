//! Mob-skeleton integration tests: AI think as timed self-chained wakes,
//! in virtual time.
//!
//! The cost contract under test: a mob costs O(thinks) — a handful of
//! events per second at its own cadence — never O(ticks), and wake
//! storms must not multiply its think chain.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ultimate_engine::causal::clock::ManualClock;
use ultimate_engine::causal::event::{Event, EventPayload};
use ultimate_engine::world::World;
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::chunk::Chunk;
use ultimate_engine::world::entity::Vec3;
use ultimate_engine::world::position::{BlockPos, ChunkPos, LocalBlockPos};

use ultimate_server::block;
use ultimate_server::physics::{self, BlockAction, PhysicsHandle};
use ultimate_server::rules::mob::{KIND_MOB, spawn_mob_events};

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

fn advance_and_settle(clock: &ManualClock, handle: &PhysicsHandle, ms: u64) {
    clock.advance(ms * 1_000_000);
    handle.kick();
    quiesce(handle);
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

fn spawn_mob(world: &World, handle: &PhysicsHandle, at: Vec3) {
    handle.submit_events(spawn_mob_events(world, at, 0));
}

#[test]
fn mob_wanders_and_stays_grounded() {
    let (world, clock) = flat_world_manual_clock(2);
    let handle = start(&world, 4);
    spawn_mob(&world, &handle, Vec3::new(8.5, 5.0, 8.5));
    quiesce(&handle);
    assert_eq!(world.entities().len(), 1);

    // 30 virtual seconds of wandering, in steps so thinks/hops interleave.
    for _ in 0..60 {
        advance_and_settle(&clock, &handle, 500);
    }

    let (_, s) = world.entities().snapshot()[0];
    assert_eq!(s.kind, KIND_MOB);
    let moved = (s.pos.x - 8.5).abs() + (s.pos.z - 8.5).abs();
    assert!(moved > 0.5, "mob should have wandered, moved {moved:.3}");
    // At quiescence it rests on the dirt surface (y=5 top face).
    assert!(
        (s.pos.y - 5.0).abs() < 1e-9,
        "mob should rest on the ground, got y={}",
        s.pos.y
    );
}

#[test]
fn mob_cost_is_rate_limited_by_its_think_cadence() {
    let (world, clock) = flat_world_manual_clock(2);
    let handle = start(&world, 4);
    spawn_mob(&world, &handle, Vec3::new(8.5, 5.0, 8.5));
    quiesce(&handle);

    let before = handle.executed_total();
    for _ in 0..120 {
        advance_and_settle(&clock, &handle, 500);
    }
    let events = handle.executed_total() - before;

    // 60 virtual seconds at ~0.9-1.5 s/think = ~40-67 thinks; each think
    // costs a few events (wake + guarded set, plus hop segments). A
    // tick-based sim would burn 1,200 updates in the same span.
    assert!(events > 20, "the mob must actually think, got {events} events");
    assert!(
        events < 800,
        "a lone mob's 60s cost must stay O(thinks), got {events} events"
    );
}

#[test]
fn wake_storms_do_not_multiply_think_chains() {
    let (world, clock) = flat_world_manual_clock(2);
    let handle = start(&world, 4);
    spawn_mob(&world, &handle, Vec3::new(8.5, 5.0, 8.5));
    quiesce(&handle);
    let (id, s) = world.entities().snapshot()[0];

    // Hammer the mob with spurious wakes (the same class a heavy dig
    // cascade would deliver via wake-on-block-change).
    for _ in 0..200 {
        handle.submit_events(vec![Event {
            payload: EventPayload::EntityWake { id, at: s.pos.block_pos() },
        }]);
    }
    quiesce(&handle);

    // The parked chain stays bounded: the pending think (and transiently
    // one just-minted successor) — never 200 chains.
    assert!(
        handle.pending_timed() <= 3,
        "wake storm multiplied think chains: {} parked timers",
        handle.pending_timed()
    );

    // And the ongoing cost matches a quiet mob's.
    let before = handle.executed_total();
    for _ in 0..40 {
        advance_and_settle(&clock, &handle, 500);
    }
    let events = handle.executed_total() - before;
    assert!(
        events < 400,
        "post-storm cost must return to O(thinks), got {events} events"
    );
}

#[test]
fn mob_falls_when_the_floor_breaks() {
    let (world, clock) = flat_world_manual_clock(2);
    let handle = start(&world, 4);
    spawn_mob(&world, &handle, Vec3::new(8.5, 5.0, 8.5));
    quiesce(&handle);

    // Break the dirt underfoot, then the stone below it: wake-on-block-
    // change re-plans the resting mob, which falls to the new floor.
    for y in [4, 3] {
        handle.submit_action(BlockAction {
            pos: BlockPos::new(8, y, 8),
            old: world.get_block(BlockPos::new(8, y, 8)),
            new: block::AIR,
            update_stairs: false,
            drop_item: false,
        });
        quiesce(&handle);
    }
    advance_and_settle(&clock, &handle, 1000);

    let (_, s) = world.entities().snapshot()[0];
    assert!(
        (s.pos.y - 3.0).abs() < 1e-9,
        "mob should fall to the stone at y=2's top face, got y={}",
        s.pos.y
    );
}
