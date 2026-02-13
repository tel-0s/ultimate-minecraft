//! Block-update rules: gravity, fluid spread, and fluid drainage.
//!
//! Each public function has the signature `fn(&World, &EventPayload) -> Vec<Event>`
//! so it can be registered directly as a `RuleFn`.

use crate::block::{self, FluidKind};
use super::helpers::{block_set, notify_horizontal, notify_vertical, notify_neighbors, horizontal_neighbors};
use ultimate_engine::causal::event::{Event, EventPayload};
use ultimate_engine::world::position::BlockPos;
use ultimate_engine::world::World;

// ── Gravity ──────────────────────────────────────────────────────────────

/// Gravity rule: if a gravity-affected block (sand, gravel) has a replaceable
/// block below it, swap them and notify above + below.
pub fn gravity(world: &World, payload: &EventPayload) -> Vec<Event> {
    let pos = match payload {
        EventPayload::BlockSet { pos, .. } | EventPayload::BlockNotify { pos } => *pos,
    };

    let block_id = world.get_block(pos);
    if !block::has_gravity(block_id) {
        return Vec::new();
    }

    let below = BlockPos::new(pos.x, pos.y - 1, pos.z);
    let below_id = world.get_block(below);

    if block::is_replaceable(below_id) {
        let mut events = vec![
            block_set(pos, block_id, below_id),
            block_set(below, below_id, block_id),
        ];
        // Notify below (continued falling) and above (pillar cascade).
        events.extend(notify_vertical(pos));
        events
    } else {
        Vec::new()
    }
}

// ── Generic fluid logic ──────────────────────────────────────────────────

/// A flowing fluid block at `level` (> 0) is "supported" if it has a path
/// back toward a source block:
///   - Any fluid of the *same kind* directly above (falling fluid feeds it), OR
///   - A horizontal neighbor of the same kind with a strictly lower level.
///
/// Source blocks (level 0) are always supported (player-placed, permanent).
fn has_fluid_support(world: &World, pos: BlockPos, level: u8, kind: FluidKind) -> bool {
    // Fluid from above always supports.
    let above = BlockPos::new(pos.x, pos.y + 1, pos.z);
    if kind.is_match(world.get_block(above)) {
        return true;
    }

    // Horizontal neighbor with a strictly lower level supports.
    for neighbor in horizontal_neighbors(pos) {
        if let Some(n_level) = kind.level(world.get_block(neighbor)) {
            if n_level < level {
                return true;
            }
        }
    }

    false
}

/// Core fluid rule, parameterized by `FluidKind`.
///
/// Handles **spread**, **drainage**, and **removal notification**:
///   - Removal: when a `BlockSet` replaces this fluid with a non-fluid block,
///     notify all 6 neighbors so drainage can cascade through the rules alone.
///   - Spread: source (level 0) spreads to level 1; flowing (level N) to N+1,
///     up to `kind.max_spread()`. Fluid above air falls down as level 1.
///   - Drain: on `BlockNotify`, flowing fluid (level > 0) without support
///     drains to air and notifies horizontal neighbors.
fn generic_fluid(world: &World, payload: &EventPayload, kind: FluidKind) -> Vec<Event> {
    // ── Removal: fluid replaced by non-fluid → notify neighbors for drainage ─
    if let EventPayload::BlockSet { pos, old, new } = payload {
        if kind.is_match(*old) && !kind.is_match(*new) {
            return notify_neighbors(*pos);
        }
    }

    let is_notify = matches!(payload, EventPayload::BlockNotify { .. });

    let pos = match payload {
        EventPayload::BlockSet { pos, new, .. } if kind.is_match(*new) => *pos,
        EventPayload::BlockNotify { pos } if kind.is_match(world.get_block(*pos)) => *pos,
        _ => return Vec::new(),
    };

    let block_id = world.get_block(pos);
    let level = match kind.level(block_id) {
        Some(l) => l,
        None => return Vec::new(),
    };

    // ── Drainage (flowing only, on BlockNotify) ──────────────────────
    if level > 0 && is_notify && !has_fluid_support(world, pos, level, kind) {
        let mut events = vec![block_set(pos, block_id, block::AIR)];
        events.extend(notify_horizontal(pos));
        return events;
    }

    // ── Spread ───────────────────────────────────────────────────────

    // Falls down first (gravity-like). Falling fluid becomes level 1.
    let below = BlockPos::new(pos.x, pos.y - 1, pos.z);
    let below_id = world.get_block(below);
    if below_id == block::AIR {
        return vec![block_set(below, below_id, kind.at_level(1))];
    }

    // Horizontal spread: level increases by 1 each step, capped at max.
    if level >= kind.max_spread() {
        return Vec::new();
    }
    let next = kind.at_level(level + 1);

    horizontal_neighbors(pos)
        .into_iter()
        .filter(|n| world.get_block(*n) == block::AIR)
        .map(|n| block_set(n, block::AIR, next))
        .collect()
}

// ── Public rule wrappers ─────────────────────────────────────────────────

/// Water spread and drainage rule.
pub fn water_spread(world: &World, payload: &EventPayload) -> Vec<Event> {
    generic_fluid(world, payload, FluidKind::Water)
}

/// Lava spread and drainage rule.
pub fn lava_spread(world: &World, payload: &EventPayload) -> Vec<Event> {
    generic_fluid(world, payload, FluidKind::Lava)
}
