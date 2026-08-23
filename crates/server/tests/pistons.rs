//! Piston integration tests: multi-cell atomic rewrites driven through
//! the physics service.
//!
//! The property under test everywhere: a push either happens ENTIRELY or
//! not at all — `AtomicBlockSet` verifies every cell under the chunks'
//! write locks, so chains cannot tear and matter is conserved.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ultimate_engine::causal::clock::ManualClock;
use ultimate_engine::world::World;
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::chunk::Chunk;
use ultimate_engine::world::position::{BlockPos, ChunkPos, LocalBlockPos};

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

fn start(world: &Arc<World>) -> PhysicsHandle {
    physics::start(
        Arc::clone(world),
        ultimate_server::rules::standard,
        ultimate_server::event_bus::SpatialBus::new(),
        None,
        physics::PhysicsOptions { workers: 4, rebalance: false, ..Default::default() },
    )
}

fn state_of(name: &str) -> BlockId {
    use std::str::FromStr;
    let kind = azalea_registry::builtin::BlockKind::from_str(name)
        .unwrap_or_else(|_| panic!("unknown block {name}"));
    BlockId(u32::from(azalea_block::BlockState::from(kind)) as u16)
}

fn with_props(id: BlockId, changes: &[(&str, &str)]) -> BlockId {
    ultimate_server::registry::with_props(id, changes).expect("state combination exists")
}

/// A lever standing on the block below (the default state is a WALL
/// lever, which the attachment rule would rightly pop off thin air).
fn floor_lever() -> BlockId {
    ultimate_server::registry::with_props(state_of("lever"), &[("face", "floor")])
        .expect("floor lever state")
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

fn name_at(world: &World, pos: BlockPos) -> &'static str {
    ultimate_server::registry::block_name(world.get_block(pos))
}

fn prop_at(world: &World, pos: BlockPos, key: &str) -> Option<&'static str> {
    ultimate_server::registry::block_prop(world.get_block(pos), key)
}

const Y: i64 = 5;

/// Piston at (2, Y, 8) facing east, lever behind it at (1, Y, 8).
fn piston_rig(handle: &PhysicsHandle, world: &World, sticky: bool) -> (BlockPos, BlockPos) {
    let base = if sticky { "sticky_piston" } else { "piston" };
    let piston = BlockPos::new(2, Y, 8);
    let lever = BlockPos::new(1, Y, 8);
    place(handle, world, piston, with_props(state_of(base), &[("facing", "east")]));
    place(handle, world, lever, floor_lever());
    (piston, lever)
}

fn flip_lever(handle: &PhysicsHandle, world: &World, lever: BlockPos) {
    let flipped = ultimate_server::rules::redstone::toggle_lever(world.get_block(lever)).unwrap();
    place(handle, world, lever, flipped);
}

#[test]
fn piston_pushes_a_chain_and_extends() {
    let (world, _clock) = flat_world(2);
    let handle = start(&world);

    let (piston, lever) = piston_rig(&handle, &world, false);
    // Three-block chain in front: stone, dirt, sand.
    place(&handle, &world, BlockPos::new(3, Y, 8), BlockId::new(1));
    place(&handle, &world, BlockPos::new(4, Y, 8), block::DIRT);
    place(&handle, &world, BlockPos::new(5, Y, 8), block::SAND);
    quiesce(&handle);

    flip_lever(&handle, &world, lever);
    quiesce(&handle);

    assert_eq!(prop_at(&world, piston, "extended"), Some("true"));
    assert_eq!(name_at(&world, BlockPos::new(3, Y, 8)), "piston_head");
    assert_eq!(world.get_block(BlockPos::new(4, Y, 8)), BlockId::new(1), "stone shifted");
    assert_eq!(world.get_block(BlockPos::new(5, Y, 8)), block::DIRT, "dirt shifted");
    assert_eq!(world.get_block(BlockPos::new(6, Y, 8)), block::SAND, "sand shifted");
}

#[test]
fn immovable_and_push_limit_abort_without_tearing() {
    let (world, _clock) = flat_world(2);
    let handle = start(&world);

    // Obsidian mid-chain: nothing moves at all.
    let (piston, lever) = piston_rig(&handle, &world, false);
    place(&handle, &world, BlockPos::new(3, Y, 8), BlockId::new(1));
    place(&handle, &world, BlockPos::new(4, Y, 8), state_of("obsidian"));
    quiesce(&handle);
    flip_lever(&handle, &world, lever);
    quiesce(&handle);
    assert_eq!(prop_at(&world, piston, "extended"), Some("false"), "push aborted");
    assert_eq!(world.get_block(BlockPos::new(3, Y, 8)), BlockId::new(1), "nothing moved");

    // 13 blocks: over the limit, nothing moves.
    let (piston2, lever2) = {
        let piston = BlockPos::new(2, Y, 10);
        let lever = BlockPos::new(1, Y, 10);
        place(&handle, &world, piston, with_props(state_of("piston"), &[("facing", "east")]));
        place(&handle, &world, lever, floor_lever());
        (piston, lever)
    };
    for x in 3..(3 + 13) {
        place(&handle, &world, BlockPos::new(x, Y, 10), BlockId::new(1));
    }
    quiesce(&handle);
    flip_lever(&handle, &world, lever2);
    quiesce(&handle);
    assert_eq!(prop_at(&world, piston2, "extended"), Some("false"), "13 > limit");
}

#[test]
fn push_destroys_soft_blocks() {
    let (world, _clock) = flat_world(2);
    let handle = start(&world);

    let (_piston, lever) = piston_rig(&handle, &world, false);
    place(&handle, &world, BlockPos::new(3, Y, 8), BlockId::new(1));
    place(&handle, &world, BlockPos::new(4, Y, 8), state_of("redstone_torch"));
    quiesce(&handle);
    flip_lever(&handle, &world, lever);
    quiesce(&handle);

    assert_eq!(name_at(&world, BlockPos::new(3, Y, 8)), "piston_head");
    assert_eq!(
        world.get_block(BlockPos::new(4, Y, 8)),
        BlockId::new(1),
        "stone slid into the destroyed torch's cell"
    );
    // The crushed torch drops its item — exactly once (position-derived
    // pop id + the compound arm only running on effective apply).
    let items: Vec<_> = world
        .entities()
        .snapshot()
        .into_iter()
        .filter(|(_, s)| s.kind == ultimate_server::rules::entity::KIND_ITEM)
        .collect();
    assert_eq!(items.len(), 1, "exactly one drop from the crushed torch");
    assert_eq!(
        ultimate_server::rules::entity::aux_block(items[0].1.aux),
        state_of("redstone_torch"),
    );
}

#[test]
fn sticky_piston_pulls_on_retract_and_normal_leaves() {
    let (world, _clock) = flat_world(2);
    let handle = start(&world);

    // Sticky: pushes a block out, pulls it back.
    let (piston, lever) = piston_rig(&handle, &world, true);
    place(&handle, &world, BlockPos::new(3, Y, 8), block::DIRT);
    quiesce(&handle);
    flip_lever(&handle, &world, lever);
    quiesce(&handle);
    assert_eq!(name_at(&world, BlockPos::new(3, Y, 8)), "piston_head");
    assert_eq!(world.get_block(BlockPos::new(4, Y, 8)), block::DIRT, "pushed out");

    flip_lever(&handle, &world, lever);
    quiesce(&handle);
    assert_eq!(prop_at(&world, piston, "extended"), Some("false"));
    assert_eq!(world.get_block(BlockPos::new(3, Y, 8)), block::DIRT, "pulled back");
    assert_eq!(world.get_block(BlockPos::new(4, Y, 8)), block::AIR);

    // Normal piston: pushes, then retracts WITHOUT pulling.
    let piston2 = BlockPos::new(2, Y, 12);
    let lever2 = BlockPos::new(1, Y, 12);
    place(&handle, &world, piston2, with_props(state_of("piston"), &[("facing", "east")]));
    place(&handle, &world, lever2, floor_lever());
    place(&handle, &world, BlockPos::new(3, Y, 12), block::DIRT);
    quiesce(&handle);
    flip_lever(&handle, &world, lever2);
    quiesce(&handle);
    assert_eq!(world.get_block(BlockPos::new(4, Y, 12)), block::DIRT);
    flip_lever(&handle, &world, lever2);
    quiesce(&handle);
    assert_eq!(world.get_block(BlockPos::new(3, Y, 12)), block::AIR, "head cell clears");
    assert_eq!(world.get_block(BlockPos::new(4, Y, 12)), block::DIRT, "block stays put");
}

#[test]
fn pushed_sand_cascades_through_gravity() {
    let (world, _clock) = flat_world(2);
    let handle = start(&world);

    // Piston pushes sand over a pit; the post-apply notifies hand it to
    // the gravity rule and it falls — compound rewrites re-enter the
    // ordinary causal cascade.
    let (_piston, lever) = piston_rig(&handle, &world, false);
    place(&handle, &world, BlockPos::new(3, Y, 8), block::SAND);
    // Dig a pit at the landing cell (down to stone at y=2).
    place(&handle, &world, BlockPos::new(4, Y - 1, 8), block::AIR);
    place(&handle, &world, BlockPos::new(4, Y - 2, 8), block::AIR);
    quiesce(&handle);

    flip_lever(&handle, &world, lever);
    quiesce(&handle);

    assert_eq!(name_at(&world, BlockPos::new(3, Y, 8)), "piston_head");
    assert_eq!(world.get_block(BlockPos::new(4, Y, 8)), block::AIR, "sand fell out of the push cell");
    assert_eq!(
        world.get_block(BlockPos::new(4, Y - 2, 8)),
        block::SAND,
        "sand landed at the pit floor"
    );
}
