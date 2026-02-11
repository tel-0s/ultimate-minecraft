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
        let above = BlockPos::new(pos.x, pos.y + 1, pos.z);
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
            // Notify below the landing spot (for continued falling).
            Event {
                payload: EventPayload::BlockNotify { pos: below },
            },
            // Notify above the vacated spot so the rest of the pillar cascades.
            Event {
                payload: EventPayload::BlockNotify { pos: above },
            },
        ]
    } else {
        Vec::new()
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// The four horizontal neighbor positions.
fn horizontal_neighbors(pos: BlockPos) -> [BlockPos; 4] {
    [
        BlockPos::new(pos.x + 1, pos.y, pos.z),
        BlockPos::new(pos.x - 1, pos.y, pos.z),
        BlockPos::new(pos.x, pos.y, pos.z + 1),
        BlockPos::new(pos.x, pos.y, pos.z - 1),
    ]
}

/// A flowing water block at `level` (> 0) is "supported" if it has a path back
/// toward a source block:
///   • Any water directly above (falling water feeds it), OR
///   • A horizontal neighbor with a strictly lower water level.
///
/// Source blocks (level 0) are always supported (player-placed, permanent).
fn has_water_support(world: &World, pos: BlockPos, level: u8) -> bool {
    // Water from above always supports.
    let above = BlockPos::new(pos.x, pos.y + 1, pos.z);
    if block::is_fluid(world.get_block(above)) {
        return true;
    }

    // Horizontal neighbor with a strictly lower level supports.
    for neighbor in horizontal_neighbors(pos) {
        if let Some(n_level) = block::water_level(world.get_block(neighbor)) {
            if n_level < level {
                return true;
            }
        }
    }

    false
}

// ── Fluid rule ───────────────────────────────────────────────────────────

/// Fluid spread **and drainage** rule.
///
/// Spreading (vanilla-like):
///   • Source blocks (level 0) spread to level 1.
///   • Flowing water (level N) spreads to level N+1.
///   • Water at level 7 doesn't spread further.
///   • Water above air falls down as level 1 flowing water.
///
/// Drainage:
///   • On `BlockNotify`, flowing water (level > 0) checks whether it still has
///     a path back to a source. If not, it drains to air and notifies its
///     horizontal neighbors, cascading the drain outward.
///
/// Triggers on:
///   • `BlockSet` where the new block is a fluid (initial placement / cascade).
///   • `BlockNotify` where the notified position already contains a fluid.
pub fn fluid_spread(world: &World, payload: &EventPayload) -> Vec<Event> {
    let is_notify = matches!(payload, EventPayload::BlockNotify { .. });

    let pos = match payload {
        EventPayload::BlockSet { pos, new, .. } if block::is_fluid(*new) => *pos,
        EventPayload::BlockNotify { pos } if block::is_fluid(world.get_block(*pos)) => *pos,
        _ => return Vec::new(),
    };

    let block_id = world.get_block(pos);
    let level = match block::water_level(block_id) {
        Some(l) => l,
        None => return Vec::new(),
    };

    // ── Drainage check (flowing water only, on BlockNotify) ──────────────
    // Source blocks (level 0) never drain.
    if level > 0 && is_notify && !has_water_support(world, pos, level) {
        let mut events = vec![Event {
            payload: EventPayload::BlockSet {
                pos,
                old: block_id,
                new: block::AIR,
            },
        }];
        // Notify horizontal neighbors so they can check their own support.
        for neighbor in horizontal_neighbors(pos) {
            events.push(Event {
                payload: EventPayload::BlockNotify { pos: neighbor },
            });
        }
        return events;
    }

    // ── Spread logic ─────────────────────────────────────────────────────

    let mut events = Vec::new();

    // Water falls down first (gravity-like). Falling water becomes level 1.
    let below = BlockPos::new(pos.x, pos.y - 1, pos.z);
    let below_id = world.get_block(below);
    if below_id == block::AIR {
        events.push(Event {
            payload: EventPayload::BlockSet {
                pos: below,
                old: below_id,
                new: block::water_at_level(1),
            },
        });
        return events;
    }

    // Horizontal spread: level increases by 1 each step, stops at max (7).
    if level >= block::water_max_spread() {
        return Vec::new();
    }
    let next_water = block::water_at_level(level + 1);

    for neighbor in horizontal_neighbors(pos) {
        let nb = world.get_block(neighbor);
        if nb == block::AIR {
            events.push(Event {
                payload: EventPayload::BlockSet {
                    pos: neighbor,
                    old: nb,
                    new: next_water,
                },
            });
        }
    }

    events
}
