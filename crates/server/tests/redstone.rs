//! Redstone MVP integration tests: signal propagation as causality.
//!
//! Circuits are built from real MC block states (via the placement
//! lookup tables) and driven through the physics service with a
//! ManualClock — redstone's 100 ms ticks run in virtual time.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ultimate_engine::causal::clock::ManualClock;
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::chunk::Chunk;
use ultimate_engine::world::position::{BlockPos, ChunkPos, LocalBlockPos};
use ultimate_engine::world::World;

use ultimate_server::block;
use ultimate_server::physics::{self, BlockAction, PhysicsHandle};

fn flat_world(radius: i32) -> (Arc<World>, Arc<ManualClock>) {
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

fn tick(clock: &ManualClock, handle: &PhysicsHandle, ms: u64) {
    clock.advance(ms * 1_000_000);
    handle.kick();
    quiesce(handle);
}

fn start(world: &Arc<World>) -> PhysicsHandle {
    physics::start(
        Arc::clone(world),
        ultimate_server::rules::standard,
        ultimate_server::event_bus::SpatialBus::new(),
        None,
        physics::PhysicsOptions { workers: 4, rebalance: false, ..Default::default() },
    )
}

/// Default state id for a named block.
fn state_of(name: &str) -> BlockId {
    use std::str::FromStr;
    let kind = azalea_registry::builtin::BlockKind::from_str(name)
        .unwrap_or_else(|_| panic!("unknown block {name}"));
    BlockId(u32::from(azalea_block::BlockState::from(kind)) as u16)
}

fn place(handle: &PhysicsHandle, world: &World, pos: BlockPos, id: BlockId) {
    handle.submit_action(BlockAction {
        pos,
        old: world.get_block(pos),
        new: id,
        update_stairs: false,
        drop_item: false,
    });
}

fn wire_power(world: &World, pos: BlockPos) -> u8 {
    ultimate_server::rules::redstone::wire_power_at(world, pos).expect("wire expected")
}

fn is_lit(world: &World, pos: BlockPos) -> bool {
    ultimate_server::rules::redstone::is_lit(world.get_block(pos))
}

// Surface is y=5 (on top of dirt at y=4).
const Y: i64 = 5;

#[test]
fn lever_powers_wire_run_and_lamp() {
    let (world, _clock) = flat_world(2);
    let handle = start(&world);

    // lever at x=0, wire x=1..=5, lamp at x=6 — all on the surface.
    place(&handle, &world, BlockPos::new(0, Y, 8), state_of("lever"));
    for x in 1..=5 {
        place(&handle, &world, BlockPos::new(x, Y, 8), state_of("redstone_wire"));
    }
    place(&handle, &world, BlockPos::new(6, Y, 8), state_of("redstone_lamp"));
    quiesce(&handle);

    // Unpowered: wire at 0, lamp dark.
    assert_eq!(wire_power(&world, BlockPos::new(1, Y, 8)), 0);
    assert!(!is_lit(&world, BlockPos::new(6, Y, 8)));

    // Flip the lever ON (as the connection's right-click does).
    let lever = BlockPos::new(0, Y, 8);
    let on = ultimate_server::rules::redstone::toggle_lever(world.get_block(lever)).unwrap();
    place(&handle, &world, lever, on);
    quiesce(&handle);

    // Signal attenuates 15, 14, 13... along the run; the lamp lights.
    for (i, x) in (1..=5).enumerate() {
        assert_eq!(
            wire_power(&world, BlockPos::new(x, Y, 8)),
            15 - i as u8,
            "wire at x={x}"
        );
    }
    assert!(is_lit(&world, BlockPos::new(6, Y, 8)), "lamp lights");

    // Flip OFF: the wire drains to 0 (mutual-feed decrements converge)
    // and the lamp goes dark.
    let off = ultimate_server::rules::redstone::toggle_lever(world.get_block(lever)).unwrap();
    place(&handle, &world, lever, off);
    quiesce(&handle);
    for x in 1..=5 {
        assert_eq!(wire_power(&world, BlockPos::new(x, Y, 8)), 0, "drained at x={x}");
    }
    assert!(!is_lit(&world, BlockPos::new(6, Y, 8)), "lamp dark");
}

#[test]
fn torch_inverts_with_one_redstone_tick_delay() {
    let (world, clock) = flat_world(2);
    let handle = start(&world);

    // Torch stands on a stone pillar; a lever next to the pillar is the
    // input.
    let pillar = BlockPos::new(4, Y, 8);
    let torch = BlockPos::new(4, Y + 1, 8);
    let lever = BlockPos::new(3, Y, 8);
    place(&handle, &world, pillar, BlockId::new(1));
    place(&handle, &world, torch, state_of("redstone_torch"));
    place(&handle, &world, lever, state_of("lever"));
    quiesce(&handle);
    assert!(is_lit(&world, torch), "torch lit while input off");

    // Lever ON → torch turns off, but only after one redstone tick.
    let on = ultimate_server::rules::redstone::toggle_lever(world.get_block(lever)).unwrap();
    place(&handle, &world, lever, on);
    quiesce(&handle);
    assert!(is_lit(&world, torch), "still lit before the tick elapses");

    tick(&clock, &handle, 150);
    assert!(!is_lit(&world, torch), "off one redstone tick later");

    // Lever OFF → back on after another tick.
    let off = ultimate_server::rules::redstone::toggle_lever(world.get_block(lever)).unwrap();
    place(&handle, &world, lever, off);
    quiesce(&handle);
    tick(&clock, &handle, 150);
    assert!(is_lit(&world, torch), "re-lit one redstone tick later");
}

#[test]
fn torch_tracks_oscillating_input_with_one_tick_lag() {
    // A true self-feeding torch clock needs wire climbing (up/down wire
    // connections), which is post-MVP. This verifies the oscillator
    // MECHANISM instead: a torch chasing a toggling input through six
    // delayed inversions, each landing exactly one redstone tick behind
    // its cause, all in virtual time.
    let (world, clock) = flat_world(2);
    let handle = start(&world);

    let pillar = BlockPos::new(8, Y, 8);
    let torch = BlockPos::new(8, Y + 1, 8);
    let lever = BlockPos::new(7, Y, 8);
    place(&handle, &world, pillar, BlockId::new(1));
    place(&handle, &world, torch, state_of("redstone_torch"));
    place(&handle, &world, lever, state_of("lever"));
    quiesce(&handle);

    let mut expected_lit = true;
    assert!(is_lit(&world, torch));
    for _ in 0..6 {
        let toggled =
            ultimate_server::rules::redstone::toggle_lever(world.get_block(lever)).unwrap();
        place(&handle, &world, lever, toggled);
        quiesce(&handle);
        tick(&clock, &handle, 150);
        expected_lit = !expected_lit;
        assert_eq!(is_lit(&world, torch), expected_lit, "torch tracks with one-tick lag");
    }
}
