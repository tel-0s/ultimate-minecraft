use crate::block;
use ultimate_engine::causal::event::{Event, EventPayload};
use ultimate_engine::world::position::BlockPos;
use ultimate_engine::world::World;

/// Gravity rule: if a gravity-affected block (sand) has air below it, fall.
pub fn gravity(world: &World, payload: &EventPayload) -> Vec<Event> {
    let pos = match payload {
        EventPayload::BlockSet { pos, .. } => *pos,
        EventPayload::BlockNotify { pos } => *pos,
    };

    let block_id = world.get_block(pos);
    if !block::has_gravity(block_id) {
        return Vec::new();
    }

    let below = BlockPos::new(pos.x, pos.y - 1, pos.z);
    let below_id = world.get_block(below);

    if block::is_replaceable(below_id) {
        vec![
            Event {
                payload: EventPayload::BlockSet {
                    pos,
                    old: block_id,
                    new: below_id,
                },
            },
            Event {
                payload: EventPayload::BlockSet {
                    pos: below,
                    old: below_id,
                    new: block_id,
                },
            },
            Event {
                payload: EventPayload::BlockNotify { pos: below },
            },
        ]
    } else {
        Vec::new()
    }
}

/// Fluid spread rule: water spreads to adjacent air blocks.
pub fn fluid_spread(world: &World, payload: &EventPayload) -> Vec<Event> {
    let pos = match payload {
        EventPayload::BlockSet { pos, new, .. } if block::is_fluid(*new) => *pos,
        _ => return Vec::new(),
    };

    let horizontal_neighbors = [
        BlockPos::new(pos.x + 1, pos.y, pos.z),
        BlockPos::new(pos.x - 1, pos.y, pos.z),
        BlockPos::new(pos.x, pos.y, pos.z + 1),
        BlockPos::new(pos.x, pos.y, pos.z - 1),
    ];

    let mut events = Vec::new();

    // Water falls down first (gravity-like).
    let below = BlockPos::new(pos.x, pos.y - 1, pos.z);
    let below_id = world.get_block(below);
    if below_id == block::AIR {
        events.push(Event {
            payload: EventPayload::BlockSet {
                pos: below,
                old: below_id,
                new: block::WATER,
            },
        });
        return events;
    }

    // Otherwise, spread horizontally.
    for neighbor in horizontal_neighbors {
        let nb = world.get_block(neighbor);
        if nb == block::AIR {
            events.push(Event {
                payload: EventPayload::BlockSet {
                    pos: neighbor,
                    old: nb,
                    new: block::WATER,
                },
            });
        }
    }

    events
}
