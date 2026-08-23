//! Natural mob spawning + per-kind collision fidelity.

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
use ultimate_server::rules::entity::{player_entity_id, player_state};
use ultimate_server::rules::mob::KIND_MOB;
use ultimate_server::simulation::SimulationLayer;
use ultimate_server::spawning::MobSpawner;

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

fn place(handle: &PhysicsHandle, world: &World, pos: BlockPos, id: BlockId) {
    handle.submit_action(BlockAction {
        pos,
        old: world.get_block(pos),
        new: id,
        update_stairs: false,
        drop_item: false,
    });
}

fn add_player(world: &World, handle: &PhysicsHandle, eid: i32, at: Vec3) {
    let pid = player_entity_id(eid);
    handle.submit_events(vec![Event {
        payload: EventPayload::EntitySet {
            id: pid,
            old: None,
            new: Some(player_state(at, 0.0, 0.0, world.now())),
        },
    }]);
    assert!(wait_for(|| world.entities().get(pid).is_some()));
}

fn mob_count(world: &World) -> usize {
    world
        .entities()
        .snapshot()
        .iter()
        .filter(|(_, s)| s.kind == KIND_MOB)
        .count()
}

/// The fidelity fix per-kind collision buys: a dropped item falls INTO a
/// pressure plate's cell (plates have no collision box) and presses it.
#[test]
fn item_falls_into_a_plate_cell_and_presses_it() {
    let (world, clock) = flat_world(2);
    let handle = start(&world);
    let plate = BlockPos::new(8, 5, 8);
    place(&handle, &world, plate, state_of("stone_pressure_plate"));
    place(&handle, &world, BlockPos::new(9, 5, 8), state_of("redstone_wire"));
    place(&handle, &world, BlockPos::new(10, 5, 8), state_of("redstone_lamp"));
    quiesce(&handle);

    // Drop an item three cells above the plate.
    let id = world.entities().allocate_id();
    handle.submit_events(vec![Event {
        payload: EventPayload::EntitySet {
            id,
            old: None,
            new: Some(ultimate_engine::world::entity::EntityState {
                kind: ultimate_server::rules::entity::KIND_ITEM,
                pos: Vec3::new(8.5, 8.5, 8.5),
                vel: Vec3::new(0.0, -0.5, 0.0),
                aux: (600_000u64 << 16) | block::DIRT.0 as u64,
                stamp: world.now(),
            }),
        },
    }]);
    quiesce(&handle);
    clock.advance(1_500_000_000);
    handle.kick();
    quiesce(&handle);

    let (_, s) = world.entities().snapshot()[0];
    assert_eq!(
        s.pos.block_pos(),
        plate,
        "item must rest IN the plate's cell, not on top of it (got {:?})",
        s.pos
    );
    assert!(
        ultimate_server::rules::redstone::wire_power_at(&world, BlockPos::new(9, 5, 8))
            .unwrap_or(0)
            > 0,
        "the landed item presses the plate"
    );
}

#[test]
fn spawner_populates_around_a_player_and_respects_the_cap() {
    let (world, _clock) = flat_world(3);
    let handle = start(&world);
    add_player(&world, &handle, 7, Vec3::new(8.5, 5.0, 8.5));

    let spawner = MobSpawner::new(4, 64);
    for _ in 0..40 {
        let events = spawner.generate_events(&world);
        handle.submit_events(events);
        quiesce(&handle);
    }

    let n = mob_count(&world);
    assert!(n > 0, "mobs should spawn near the player");
    assert!(n <= 4 + 1, "per-player cap must hold, got {n}");

    // Everything spawned inside the ring, on standable ground.
    for (_, s) in world.entities().snapshot() {
        if s.kind != KIND_MOB {
            continue;
        }
        let dx = (s.pos.x - 8.5).abs();
        let dz = (s.pos.z - 8.5).abs();
        assert!(
            dx <= 65.0 && dz <= 65.0 && (dx.max(dz)) >= 15.0,
            "spawn ring violated: ({}, {})",
            s.pos.x,
            s.pos.z
        );
    }
}

#[test]
fn far_mobs_despawn_when_no_player_is_near() {
    let (world, _clock) = flat_world(2);
    let handle = start(&world);
    // A mob with NO player nearby at all.
    handle.submit_events(ultimate_server::rules::mob::spawn_mob_events(
        &world,
        Vec3::new(8.5, 5.0, 8.5),
        0,
    ));
    assert!(wait_for(|| mob_count(&world) == 1));
    add_player(&world, &handle, 9, Vec3::new(400.5, 5.0, 400.5)); // far away

    let spawner = MobSpawner::new(4, 64);
    handle.submit_events(spawner.generate_events(&world));
    quiesce(&handle);
    assert!(
        wait_for(|| mob_count(&world) == 0),
        "mob beyond 128 blocks of every player must despawn"
    );
}

#[test]
fn no_players_means_no_spawns_and_no_despawns() {
    let (world, _clock) = flat_world(2);
    let handle = start(&world);
    handle.submit_events(ultimate_server::rules::mob::spawn_mob_events(
        &world,
        Vec3::new(8.5, 5.0, 8.5),
        0,
    ));
    assert!(wait_for(|| mob_count(&world) == 1));

    let spawner = MobSpawner::new(4, 64);
    let events = spawner.generate_events(&world);
    assert!(
        events.is_empty(),
        "an empty server neither spawns nor reaps (mobs keep while chunks stay loaded)"
    );
}
