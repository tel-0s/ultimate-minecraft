//! Pistons: blocks that MOVE blocks — the causal model's most interesting
//! case, because a push is a multi-cell rewrite that must be atomic.
//!
//! A push shifts up to 12 blocks one cell along the piston's facing. The
//! whole chain — the piston's own `extended` flip, the head placement,
//! and every shifted block — goes out as ONE engine-level
//! [`AtomicBlockSet`]: every cell guarded on its observed value, applied
//! all-or-nothing under the affected chunks' write locks. A racing
//! cascade (sand landing mid-chain, a fluid re-level) either loses
//! cleanly (the whole push aborts and re-evaluates from a later notify)
//! or happens after the push — a chain can never tear, so matter is
//! conserved by construction. This is the block-lattice sibling of
//! `EntityMaterialize`, and exists for the same §8.6 reason: no
//! composition of separately-guarded events conserves matter under
//! contention.
//!
//! Vanilla semantics carried over: 12-block push limit, immovable blocks
//! (obsidian, bedrock, extended pistons, heads) abort the push,
//! soft blocks (torches, wire, plants...) are destroyed by it, sticky
//! pistons pull one block on retraction, and a piston ignores power
//! arriving through its face. Simplifications (deliberate, for now): no
//! quasi-connectivity, no moving-block animation entity (clients see
//! instant state changes), destroyed soft blocks don't drop items.

use std::sync::Arc;

use ultimate_engine::causal::event::{BlockWrite, Event, EventPayload};
use ultimate_engine::world::World;
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::position::BlockPos;

use super::helpers::notify_neighbors;
use crate::registry::{block_name, block_prop as get_prop, lookup_block_state, with_props};

/// Vanilla's push limit.
const PUSH_LIMIT: usize = 12;

fn is_piston_base(name: &str) -> bool {
    name == "piston" || name == "sticky_piston"
}

/// Facing unit vector, all six directions.
fn facing_vec(f: &str) -> Option<(i64, i64, i64)> {
    Some(match f {
        "north" => (0, 0, -1),
        "south" => (0, 0, 1),
        "west" => (-1, 0, 0),
        "east" => (1, 0, 0),
        "up" => (0, 1, 0),
        "down" => (0, -1, 0),
        _ => return None,
    })
}

fn offset(pos: BlockPos, d: (i64, i64, i64), n: i64) -> BlockPos {
    BlockPos::new(pos.x + d.0 * n, pos.y + d.1 * n, pos.z + d.2 * n)
}

/// Blocks a piston can never move.
fn immovable(id: BlockId) -> bool {
    match block_name(id) {
        "bedrock" | "obsidian" | "crying_obsidian" | "piston_head" | "moving_piston"
        | "reinforced_deepslate" | "respawn_anchor" | "enchanting_table" | "ender_chest"
        | "spawner" | "beacon" | "barrier" | "end_portal_frame" => true,
        // An extended piston (or sticky piston) is anchored by its head.
        n if is_piston_base(n) => get_prop(id, "extended") == Some("true"),
        _ => false,
    }
}

/// Blocks a push destroys instead of moving (vanilla PushReaction::DESTROY,
/// the subset we implement). The chain slides into their cell.
fn push_destroys(id: BlockId) -> bool {
    let n = block_name(id);
    crate::block::is_replaceable(id) // air + fluids end the chain too
        || n == "redstone_wire"
        || n == "lever"
        || n.ends_with("_torch")
        || n.ends_with("_button")
        || n.ends_with("_pressure_plate")
        || n.ends_with("_sapling")
        || n == "short_grass"
        || n == "tall_grass"
        || n == "fern"
        || n == "dead_bush"
        || matches!(
            n,
            "dandelion" | "poppy" | "blue_orchid" | "allium" | "azure_bluet" | "oxeye_daisy"
                | "cornflower" | "lily_of_the_valley" | "wither_rose"
        )
}

/// Is the piston powered? Any neighbor except the cell its face points
/// into (vanilla ignores power arriving through the face).
fn piston_powered(world: &World, pos: BlockPos, front: BlockPos) -> bool {
    pos.neighbors().into_iter().filter(|n| *n != front).any(|n| {
        super::redstone::emitted_power(world, n, pos) > 0
            || super::redstone::wire_power_at(world, n).is_some_and(|p| p > 0)
    })
}

/// The `piston_head` state matching a base piston.
fn head_state(facing: &str, sticky: bool) -> Option<BlockId> {
    let mut props: Vec<(String, String)> = vec![
        ("facing".into(), facing.into()),
        ("short".into(), "false".into()),
        ("type".into(), if sticky { "sticky" } else { "normal" }.into()),
    ];
    props.sort();
    lookup_block_state("piston_head", &props)
}

/// Registered in the standard rule sets.
pub fn piston(world: &World, payload: &EventPayload) -> Vec<Event> {
    match payload {
        EventPayload::BlockSet { pos, .. } | EventPayload::BlockNotify { pos } => {
            let id = world.get_block(*pos);
            if is_piston_base(block_name(id)) {
                piston_update(world, *pos, id)
            } else {
                Vec::new()
            }
        }
        // A compound rewrite just applied (this piston's or any other):
        // its synthesized per-cell BlockSets don't evaluate rules, so the
        // world re-derives from notifies emitted here — gravity above
        // moved blocks, fluid re-levels, wire re-solves, other pistons.
        EventPayload::AtomicBlockSet { writes } => {
            let mut events = Vec::new();
            for w in writes.iter() {
                events.push(Event { payload: EventPayload::BlockNotify { pos: w.pos } });
                events.extend(notify_neighbors(w.pos));
            }
            events
        }
        _ => Vec::new(),
    }
}

fn piston_update(world: &World, pos: BlockPos, id: BlockId) -> Vec<Event> {
    let sticky = block_name(id) == "sticky_piston";
    let Some(facing) = get_prop(id, "facing") else {
        return Vec::new();
    };
    let Some(d) = facing_vec(facing) else {
        return Vec::new();
    };
    let front = offset(pos, d, 1);
    let extended = get_prop(id, "extended") == Some("true");
    let powered = piston_powered(world, pos, front);

    if powered && !extended {
        extend(world, pos, id, facing, d, sticky)
    } else if !powered && extended {
        retract(world, pos, id, facing, d, sticky)
    } else {
        Vec::new()
    }
}

/// Compute and emit the extension rewrite: scan the chain in front,
/// abort on immovables or the 12-block limit, destroy soft blocks at
/// the end, shift everything one cell, place the head, flip `extended`.
fn extend(
    world: &World,
    pos: BlockPos,
    id: BlockId,
    facing: &str,
    d: (i64, i64, i64),
    sticky: bool,
) -> Vec<Event> {
    // Chain of movable blocks starting at the front cell.
    let mut chain: Vec<(BlockPos, BlockId)> = Vec::new();
    let end_old;
    let mut i = 1;
    loop {
        let c = offset(pos, d, i);
        let cid = world.get_block(c);
        if push_destroys(cid) {
            end_old = cid; // destroyed (or air/fluid): the chain slides in
            break;
        }
        if immovable(cid) || chain.len() >= PUSH_LIMIT {
            return Vec::new(); // push blocked
        }
        chain.push((c, cid));
        i += 1;
    }

    let Some(extended_id) = with_props(id, &[("extended", "true")]) else {
        return Vec::new();
    };
    let Some(head) = head_state(facing, sticky) else {
        return Vec::new();
    };

    // The piston's own cell is FIRST: it anchors event routing to the
    // piston's chunk owner.
    let mut writes: Vec<BlockWrite> = vec![BlockWrite { pos, old: id, new: extended_id }];
    // Head takes the front cell.
    let front_old = chain.first().map(|(_, b)| *b).unwrap_or(end_old);
    writes.push(BlockWrite { pos: offset(pos, d, 1), old: front_old, new: head });
    // Each chain block shifts one cell forward; the far end lands in the
    // destroyed/air cell.
    for (j, (_, b)) in chain.iter().enumerate() {
        let target = offset(pos, d, j as i64 + 2);
        let target_old = chain.get(j + 1).map(|(_, nb)| *nb).unwrap_or(end_old);
        writes.push(BlockWrite { pos: target, old: target_old, new: *b });
    }

    vec![Event {
        payload: EventPayload::AtomicBlockSet { writes: Arc::from(writes.into_boxed_slice()) },
    }]
}

/// The retraction rewrite: head cell clears (or receives the pulled
/// block, for sticky pistons), `extended` flips off.
fn retract(
    world: &World,
    pos: BlockPos,
    id: BlockId,
    facing: &str,
    d: (i64, i64, i64),
    sticky: bool,
) -> Vec<Event> {
    let Some(retracted_id) = with_props(id, &[("extended", "false")]) else {
        return Vec::new();
    };
    let front = offset(pos, d, 1);
    let front_id = world.get_block(front);
    // Sanity: only retract our own head (a desynced state self-heals by
    // just flipping the base).
    let mut writes: Vec<BlockWrite> = vec![BlockWrite { pos, old: id, new: retracted_id }];
    if block_name(front_id) == "piston_head" {
        let beyond = offset(pos, d, 2);
        let beyond_id = world.get_block(beyond);
        let pullable = sticky
            && !immovable(beyond_id)
            && !push_destroys(beyond_id)
            && beyond_id != crate::block::AIR;
        if pullable {
            writes.push(BlockWrite { pos: front, old: front_id, new: beyond_id });
            writes.push(BlockWrite { pos: beyond, old: beyond_id, new: crate::block::AIR });
        } else {
            writes.push(BlockWrite { pos: front, old: front_id, new: crate::block::AIR });
        }
    }

    vec![Event {
        payload: EventPayload::AtomicBlockSet { writes: Arc::from(writes.into_boxed_slice()) },
    }]
}
