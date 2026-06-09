//! Block-update rules: gravity, fluid spread, and fluid drainage.
//!
//! Each public function has the signature `fn(&World, &EventPayload) -> Vec<Event>`
//! so it can be registered directly as a `RuleFn`.

use crate::block::{self, FluidKind};
use super::helpers::{block_set, notify_vertical, notify_neighbors, horizontal_neighbors};
use ultimate_engine::causal::event::{Event, EventPayload};
use ultimate_engine::world::position::BlockPos;
use ultimate_engine::world::World;

// ── Gravity ──────────────────────────────────────────────────────────────

/// Gravity rule: if a gravity-affected block (sand, gravel) has a replaceable
/// block below it, swap them and notify above + below.
pub fn gravity(world: &World, payload: &EventPayload) -> Vec<Event> {
    let pos = match payload {
        EventPayload::BlockSet { pos, .. } | EventPayload::BlockNotify { pos } => *pos,
        _ => return Vec::new(),
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

/// The level a flowing fluid cell *should* have given its neighbors, or
/// `None` if nothing supports it:
///   - Fluid of the same kind directly above feeds it at level 1 (falling).
///   - Otherwise `min(horizontal neighbor levels) + 1`.
///
/// This is the unique fixed point of fluid flow — every flowing cell's
/// level equals its shortest-path distance from a source. Re-levelling
/// toward it on notify makes fluid **confluent**: the final state is
/// independent of event execution order, which spacelike-parallel and
/// partitioned scheduling require. (Previously a cell kept whichever
/// level arrived first, so two interacting fronts settled differently
/// depending on arrival order.)
fn desired_fluid_level(world: &World, pos: BlockPos, kind: FluidKind) -> Option<u8> {
    // Fluid from above always feeds at level 1.
    let above = BlockPos::new(pos.x, pos.y + 1, pos.z);
    if kind.is_match(world.get_block(above)) {
        return Some(1);
    }

    horizontal_neighbors(pos)
        .into_iter()
        .filter_map(|n| kind.level(world.get_block(n)))
        .min()
        .map(|min_level| min_level.saturating_add(1))
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
        // Re-level: same-kind fluid changed level. Horizontal neighbors'
        // levels may now be wrong (their min-neighbor changed) — notify
        // them so the relaxation propagates. The spread logic below also
        // runs for the new level via the normal BlockSet path.
        if let (Some(old_l), Some(new_l)) = (kind.level(*old), kind.level(*new)) {
            if old_l != new_l {
                let mut events: Vec<Event> = horizontal_neighbors(*pos)
                    .into_iter()
                    .map(|n| Event { payload: EventPayload::BlockNotify { pos: n } })
                    .collect();
                events.extend(spread_events(world, *pos, new_l, kind));
                return events;
            }
        }
        // Appearance: a fluid cell came into existence (old was not this
        // kind). Besides spreading, wake any ADJACENT same-kind fluid so
        // it re-levels against the new cell. This is what makes the
        // relaxation self-stabilizing under concurrent partitioned
        // execution: a neighbour that drained against a stale read of
        // this cell (its rule ran before our write was visible) gets
        // re-evaluated by this notify, which is emitted *after* our write
        // and therefore observes it. Without it, spread only targets AIR
        // and a wrongly-drained fluid cell is never revisited.
        if kind.level(*old).is_none() && kind.is_match(*new) {
            let level = kind.level(*new).expect("is_match implies level");
            let mut events = spread_events(world, *pos, level, kind);
            let below = BlockPos::new(pos.x, pos.y - 1, pos.z);
            for n in horizontal_neighbors(*pos).into_iter().chain([below]) {
                if kind.is_match(world.get_block(n)) {
                    events.push(Event { payload: EventPayload::BlockNotify { pos: n } });
                }
            }
            return events;
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

    // ── Re-level / drain (flowing only, on BlockNotify) ───────────────
    // Relax toward the unique fixed point `level = desired`:
    //   - no feed at all, or desired beyond the spread cap → drain to air
    //     (the removal trigger above then notifies neighbors);
    //   - wrong level → set the correct one (the level-change trigger
    //     above then notifies neighbors, continuing the relaxation);
    //   - correct level → nothing. No re-spread from notify (that caused
    //     feedback loops); spreading cascades via BlockSet events only.
    if level > 0 && is_notify {
        return match desired_fluid_level(world, pos, kind) {
            None => vec![block_set(pos, block_id, block::AIR)],
            Some(d) if d > kind.max_spread() => vec![block_set(pos, block_id, block::AIR)],
            Some(d) if d != level => vec![block_set(pos, block_id, kind.at_level(d))],
            Some(_) => Vec::new(),
        };
    }

    // ── Spread (BlockSet, or source on BlockNotify) ──────────────────
    spread_events(world, pos, level, kind)
}

/// Spread from a fluid cell at `level`: fall into air below as level 1,
/// otherwise flow horizontally into air at `level + 1` (capped).
fn spread_events(world: &World, pos: BlockPos, level: u8, kind: FluidKind) -> Vec<Event> {
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
