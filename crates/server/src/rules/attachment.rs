//! Attachment support: blocks that live ON other blocks (torches, wall
//! torches, levers, buttons, plates, wire, plants) pop off — and drop as
//! items — when their support disappears.
//!
//! This is also the ONE path that drops items in creative: direct breaks
//! drop nothing (vanilla), but a popped attachment does.
//!
//! Exactly-once discipline, with zero new machinery: the pop's `BlockSet`
//! is guarded on the attachment's observed state, and the dropped item's
//! entity id is DERIVED from the position (`entity::pop_item_id`) — two
//! racing pop evaluations spawn the same id, so the loser's spawn dies at
//! the ordinary `EntitySet { old: None }` guard exactly like a contested
//! pickup does in reverse.

use ultimate_engine::causal::event::{Event, EventPayload};
use ultimate_engine::world::World;
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::position::BlockPos;

use super::helpers::{block_set, notify_neighbors};
use crate::registry::{block_id_from_name, block_name, block_prop};

/// The support cell this block must stand on/against, or `None` when the
/// block isn't an attachment.
fn support_of(pos: BlockPos, id: BlockId) -> Option<BlockPos> {
    let n = block_name(id);
    // Wall torches hang off the block behind their facing.
    if matches!(n, "wall_torch" | "soul_wall_torch" | "redstone_wall_torch") {
        let (dx, dz) = match block_prop(id, "facing")? {
            "north" => (0, 1),
            "south" => (0, -1),
            "west" => (1, 0),
            "east" => (-1, 0),
            _ => return None,
        };
        return Some(BlockPos::new(pos.x + dx, pos.y, pos.z + dz));
    }
    // Face-attached (levers, buttons): floor/ceiling/wall via `face`.
    if n == "lever" || n.ends_with("_button") {
        return Some(match block_prop(id, "face")? {
            "floor" => BlockPos::new(pos.x, pos.y - 1, pos.z),
            "ceiling" => BlockPos::new(pos.x, pos.y + 1, pos.z),
            _ => {
                let (dx, dz) = match block_prop(id, "facing")? {
                    "north" => (0, 1),
                    "south" => (0, -1),
                    "west" => (1, 0),
                    "east" => (-1, 0),
                    _ => return None,
                };
                BlockPos::new(pos.x + dx, pos.y, pos.z + dz)
            }
        });
    }
    // Everything that simply stands on the block below.
    let floor_standing = matches!(n, "torch" | "soul_torch" | "redstone_torch" | "redstone_wire")
        || n.ends_with("_pressure_plate")
        || n.ends_with("_sapling")
        || matches!(
            n,
            "short_grass" | "tall_grass" | "fern" | "dead_bush" | "dandelion" | "poppy"
                | "blue_orchid" | "allium" | "azure_bluet" | "oxeye_daisy" | "cornflower"
                | "lily_of_the_valley" | "wither_rose"
        );
    if floor_standing {
        return Some(BlockPos::new(pos.x, pos.y - 1, pos.z));
    }
    None
}

/// The item form a popped attachment drops: wall variants drop their
/// standing block; grass-like plants drop NOTHING (vanilla — otherwise
/// every dig through grassy terrain sprays item entities).
fn drop_form(id: BlockId) -> Option<BlockId> {
    let standing = match block_name(id) {
        "wall_torch" => "torch",
        "soul_wall_torch" => "soul_torch",
        "redstone_wall_torch" => "redstone_torch",
        "short_grass" | "tall_grass" | "fern" | "dead_bush" => return None,
        other => other,
    };
    Some(block_id_from_name(standing).unwrap_or(id))
}

/// Registered in the standard rule sets: when an attachment's support is
/// gone, pop it (guarded) and drop the item (guarded via the derived id).
pub fn attachment_support(world: &World, payload: &EventPayload) -> Vec<Event> {
    let pos = match payload {
        EventPayload::BlockSet { pos, .. } | EventPayload::BlockNotify { pos } => *pos,
        _ => return Vec::new(),
    };
    let id = world.get_block(pos);
    let Some(support) = support_of(pos, id) else {
        return Vec::new();
    };
    if crate::block::is_solid(world.get_block(support)) {
        return Vec::new();
    }

    // Pop: clear the cell (stale-guarded — a concurrent break makes this
    // a no-op) and spawn the drop at a position-derived id (the second of
    // two racing pops loses the spawn guard, never duplicating).
    let mut events = vec![block_set(pos, id, crate::block::AIR)];
    if let Some(dropped) = drop_form(id) {
        events.extend(crate::rules::entity::spawn_item_events_with_id(
            world,
            crate::rules::entity::pop_item_id(pos),
            pos,
            dropped,
        ));
    }
    // The vacated cell re-enters the ordinary cascade (wire re-solves,
    // fluids re-level, stacked attachments above pop in turn).
    events.extend(notify_neighbors(pos));
    events
}
