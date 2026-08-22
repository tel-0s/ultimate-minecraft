//! Attachment support-pop + creative drop semantics.
//!
//! Creative breaks drop nothing; the one legitimate drop path is an
//! attachment popping off a broken support — exactly once, via the
//! position-derived pop entity id.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ultimate_engine::causal::clock::ManualClock;
use ultimate_engine::world::World;
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::chunk::Chunk;
use ultimate_engine::world::position::{BlockPos, ChunkPos, LocalBlockPos};

use ultimate_server::block;
use ultimate_server::physics::{self, PhysicsHandle};
use ultimate_server::rules::entity::{KIND_ITEM, aux_block};

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

/// Creative break, exactly as the connection submits it.
fn creative_break(handle: &PhysicsHandle, world: &World, pos: BlockPos) {
    handle.submit_action(ultimate_server::gameplay::break_action(world, pos));
}

fn place(handle: &PhysicsHandle, world: &World, pos: BlockPos, id: BlockId) {
    handle.submit_action(physics::BlockAction {
        pos,
        old: world.get_block(pos),
        new: id,
        update_stairs: false,
        drop_item: false,
    });
}

const SURFACE: i64 = 5; // first air cell above the dirt at y=4

#[test]
fn creative_breaks_drop_nothing() {
    let (world, _clock) = flat_world(2);
    let handle = start(&world);
    creative_break(&handle, &world, BlockPos::new(8, 4, 8)); // plain dirt
    quiesce(&handle);
    assert_eq!(world.get_block(BlockPos::new(8, 4, 8)), block::AIR);
    assert!(world.entities().is_empty(), "creative breaks must not drop items");
}

#[test]
fn torch_pops_and_drops_when_its_support_breaks() {
    let (world, _clock) = flat_world(2);
    let handle = start(&world);
    let torch = BlockPos::new(8, SURFACE, 8);
    place(&handle, &world, torch, state_of("torch"));
    quiesce(&handle);

    creative_break(&handle, &world, BlockPos::new(8, 4, 8)); // its dirt
    quiesce(&handle);

    assert_eq!(world.get_block(torch), block::AIR, "torch popped");
    let all = world.entities().snapshot();
    assert_eq!(all.len(), 1, "exactly one drop");
    let (_, s) = all[0];
    assert_eq!(s.kind, KIND_ITEM);
    assert_eq!(aux_block(s.aux), state_of("torch"), "drops the torch item");
}

#[test]
fn wall_torch_pops_when_its_wall_breaks_and_drops_a_standing_torch() {
    let (world, _clock) = flat_world(2);
    let handle = start(&world);
    let wall = BlockPos::new(8, SURFACE, 8);
    let torch = BlockPos::new(9, SURFACE, 8); // on the wall's east side
    place(&handle, &world, wall, BlockId::new(1));
    place(&handle, &world, torch, with_props(state_of("wall_torch"), &[("facing", "east")]));
    quiesce(&handle);

    creative_break(&handle, &world, wall);
    quiesce(&handle);

    assert_eq!(world.get_block(torch), block::AIR, "wall torch popped");
    let all = world.entities().snapshot();
    assert_eq!(all.len(), 1);
    assert_eq!(
        aux_block(all[0].1.aux),
        state_of("torch"),
        "wall torch drops the STANDING torch item"
    );
}

#[test]
fn wall_lever_pops_with_its_wall() {
    let (world, _clock) = flat_world(2);
    let handle = start(&world);
    let wall = BlockPos::new(8, SURFACE, 8);
    let lever = BlockPos::new(9, SURFACE, 8);
    place(&handle, &world, wall, BlockId::new(1));
    place(
        &handle,
        &world,
        lever,
        with_props(state_of("lever"), &[("face", "wall"), ("facing", "east")]),
    );
    quiesce(&handle);

    creative_break(&handle, &world, wall);
    quiesce(&handle);

    assert_eq!(world.get_block(lever), block::AIR, "lever popped");
    let all = world.entities().snapshot();
    assert_eq!(all.len(), 1);
    assert_eq!(aux_block(all[0].1.aux), state_of("lever"));
}

#[test]
fn grass_pops_without_dropping() {
    let (world, _clock) = flat_world(2);
    let handle = start(&world);
    let grass = BlockPos::new(8, SURFACE, 8);
    place(&handle, &world, grass, state_of("short_grass"));
    quiesce(&handle);

    creative_break(&handle, &world, BlockPos::new(8, 4, 8));
    quiesce(&handle);

    assert_eq!(world.get_block(grass), block::AIR, "grass popped");
    assert!(
        world.entities().is_empty(),
        "grass pops silently — no item spray from digging terrain"
    );
}

#[test]
fn racing_pops_drop_exactly_once() {
    // The pop item id derives from the position, so even two overlapping
    // spawn attempts (e.g. a duplicated evaluation before the pop's
    // BlockSet applied) collapse at the EntitySet spawn guard.
    let (world, _clock) = flat_world(2);
    let handle = start(&world);
    let pos = BlockPos::new(8, SURFACE, 8);
    for _ in 0..2 {
        handle.submit_events(ultimate_server::rules::entity::spawn_item_events_with_id(
            &world,
            ultimate_server::rules::entity::pop_item_id(pos),
            pos,
            state_of("torch"),
        ));
    }
    quiesce(&handle);
    assert_eq!(world.entities().len(), 1, "same-id spawns collapse at the guard");
}

#[test]
fn torch_placement_swaps_to_wall_variant() {
    use azalea_core::direction::Direction;
    let torch_state = azalea_block::BlockState::from(azalea_registry::builtin::BlockKind::Torch);

    let wall = ultimate_server::placement::attachable_wall_variant(torch_state, Direction::East)
        .expect("side placement becomes a wall torch");
    let id = BlockId(u32::from(wall) as u16);
    assert_eq!(ultimate_server::registry::block_name(id), "wall_torch");
    assert_eq!(ultimate_server::registry::block_prop(id, "facing"), Some("east"));

    assert!(
        ultimate_server::placement::attachable_wall_variant(torch_state, Direction::Up).is_none(),
        "top placement keeps the standing torch"
    );
    let rs = azalea_block::BlockState::from(azalea_registry::builtin::BlockKind::RedstoneTorch);
    let wall = ultimate_server::placement::attachable_wall_variant(rs, Direction::North).unwrap();
    let id = BlockId(u32::from(wall) as u16);
    assert_eq!(ultimate_server::registry::block_name(id), "redstone_wall_torch");
}
