//! Partition-aware light: floods clip at chunk borders and continue on
//! the neighbor's owner via `LightNotify` — these tests pin that the
//! settled field is CORRECT across borders and independent of worker
//! count (the old flood wrote foreign chunks directly and could race).

use std::sync::Arc;
use std::time::{Duration, Instant};

use ultimate_engine::world::World;
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::chunk::Chunk;
use ultimate_engine::world::position::{BlockPos, ChunkPos, LocalBlockPos};

use ultimate_server::block;
use ultimate_server::physics::{self, BlockAction, PhysicsHandle};

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

fn start(world: &Arc<World>, workers: usize) -> PhysicsHandle {
    physics::start(
        Arc::clone(world),
        ultimate_server::rules::standard,
        ultimate_server::event_bus::SpatialBus::new(),
        None,
        physics::PhysicsOptions { workers, rebalance: false, ..Default::default() },
    )
}

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

const Y: i64 = 6; // torches sit on the surface (dirt top at y=5)

/// Sample the block-light field in a box around `center`.
fn light_field(world: &World, center: BlockPos, r: i64) -> Vec<u8> {
    let mut out = Vec::new();
    for dy in -2..=2 {
        for dz in -r..=r {
            for dx in -r..=r {
                out.push(world.get_block_light(BlockPos::new(
                    center.x + dx,
                    center.y + dy,
                    center.z + dz,
                )));
            }
        }
    }
    out
}

#[test]
fn border_torch_lights_both_chunks_correctly() {
    // Torch at x=16 — the first cell of chunk (1,0); x=15 is chunk (0,0).
    let world = flat_world(2);
    let handle = start(&world, 4);
    let torch = BlockPos::new(16, Y, 8);
    place(&handle, &world, torch, state_of("torch"));
    quiesce(&handle);

    // Torch emission is 14; through air it attenuates 1 per cell — the
    // formula must hold on BOTH sides of the border.
    assert_eq!(world.get_block_light(torch), 14);
    for d in 1..=5i64 {
        assert_eq!(
            world.get_block_light(BlockPos::new(16 + d, Y, 8)),
            (14 - d) as u8,
            "home-side distance {d}"
        );
        assert_eq!(
            world.get_block_light(BlockPos::new(16 - d, Y, 8)),
            (14 - d) as u8,
            "foreign-side distance {d} (crossed the border via notify)"
        );
    }
    // Diagonal reach into the foreign chunk (Manhattan distance).
    assert_eq!(world.get_block_light(BlockPos::new(13, Y, 11)), 14 - 3 - 3);
}

#[test]
fn border_torch_removal_drains_both_chunks() {
    let world = flat_world(2);
    let handle = start(&world, 4);
    let torch = BlockPos::new(16, Y, 8);
    place(&handle, &world, torch, state_of("torch"));
    quiesce(&handle);
    assert!(world.get_block_light(BlockPos::new(12, Y, 8)) > 0);

    place(&handle, &world, torch, block::AIR);
    quiesce(&handle);
    for d in 0..=6i64 {
        assert_eq!(world.get_block_light(BlockPos::new(16 + d, Y, 8)), 0, "home side");
        assert_eq!(
            world.get_block_light(BlockPos::new(16 - d, Y, 8)),
            0,
            "foreign side must drain too (removal crossed the border)"
        );
    }
}

#[test]
fn corner_torch_reaches_all_four_chunks_identically_across_workers() {
    // Torch at (16, Y, 16): the four chunks (0,0), (1,0), (0,1), (1,1)
    // all receive light. The settled field must be identical for 1 and 8
    // workers — with 8, the four quadrant floods run on different owners
    // and stitch through notifies.
    let mut fields = Vec::new();
    for workers in [1usize, 8] {
        let world = flat_world(2);
        let handle = start(&world, workers);
        let torch = BlockPos::new(16, Y, 16);
        place(&handle, &world, torch, state_of("torch"));
        quiesce(&handle);
        // Notify continuations are events; let any tail settle fully.
        quiesce(&handle);
        fields.push(light_field(&world, torch, 15));
    }
    assert_eq!(
        fields[0], fields[1],
        "corner-torch light field must be worker-count independent"
    );
    // And actually correct in each quadrant (spot checks).
    let world = flat_world(2);
    let handle = start(&world, 8);
    place(&handle, &world, BlockPos::new(16, Y, 16), state_of("torch"));
    quiesce(&handle);
    for (x, z) in [(13, 13), (19, 13), (13, 19), (19, 19)] {
        assert_eq!(
            world.get_block_light(BlockPos::new(x, Y, z)),
            14 - 6,
            "quadrant ({x},{z})"
        );
    }
}

#[test]
fn two_border_torches_from_different_owners_converge() {
    // Two torches straddling the same border, three cells apart: their
    // fields OVERLAP across the boundary, so each side's flood must
    // absorb the other's contribution through the notify handoff. The
    // fixpoint (max of both contributions everywhere) must hold at every
    // probe and be worker-count independent.
    let mut fields = Vec::new();
    for workers in [1usize, 8] {
        let world = flat_world(2);
        let handle = start(&world, workers);
        place(&handle, &world, BlockPos::new(15, Y, 8), state_of("torch"));
        place(&handle, &world, BlockPos::new(18, Y, 8), state_of("torch"));
        quiesce(&handle);
        quiesce(&handle);
        fields.push(light_field(&world, BlockPos::new(16, Y, 8), 12));
    }
    assert_eq!(fields[0], fields[1]);

    let world = flat_world(2);
    let handle = start(&world, 8);
    place(&handle, &world, BlockPos::new(15, Y, 8), state_of("torch"));
    place(&handle, &world, BlockPos::new(18, Y, 8), state_of("torch"));
    quiesce(&handle);
    // Between the torches: max of (14 - d_left, 14 - d_right).
    assert_eq!(world.get_block_light(BlockPos::new(16, Y, 8)), 13);
    assert_eq!(world.get_block_light(BlockPos::new(17, Y, 8)), 13);
}

#[test]
fn sky_light_stitches_under_a_border_spanning_platform() {
    let world = flat_world(2);
    // Sky-light initialization: full sky down to the surface, all chunks
    // pre-marked sky-lit (the server does this on first send).
    for cx in -2..2 {
        for cz in -2..2 {
            world.mark_sky_lit(ChunkPos::new(cx, cz));
        }
    }
    for x in -16..32i64 {
        for z in -16..32i64 {
            for y in 5..=30i64 {
                world.set_sky_light(BlockPos::new(x, y, z), 15);
            }
        }
    }
    let handle = start(&world, 8);

    // A 5×3 stone platform at y=10 straddling the x=16 border.
    for x in 14..=18i64 {
        for z in 7..=9i64 {
            place(&handle, &world, BlockPos::new(x, 10, z), BlockId::new(1));
        }
    }
    quiesce(&handle);
    quiesce(&handle);

    // Directly under the platform's center column (16, 9, 8): the direct
    // column is blocked; light arrives from the open sides. Distance to
    // open sky (x=13 or x=19, z=6 or z=10): the nearest is 2 cells away
    // laterally → 15 - 2 = 13.
    assert_eq!(world.get_sky_light(BlockPos::new(16, 9, 8)), 13);
    // Symmetric under-edge cells on either side of the border.
    assert_eq!(
        world.get_sky_light(BlockPos::new(14, 9, 8)),
        world.get_sky_light(BlockPos::new(18, 9, 8)),
        "border must not skew the stitched sky field"
    );
}
