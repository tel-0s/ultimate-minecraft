//! Redstone (MVP): wire, lever, lamp, torch — signal propagation as
//! causality, which is this engine's home turf.
//!
//! - **Wire** re-levels confluently to `max(adjacent source → 15,
//!   adjacent wire → power−1)`, the exact `max−1` mirror of the fluid
//!   rule's `min+1`: the fixpoint is distance-from-source, unique, so
//!   the settled circuit is independent of event execution order.
//!   Connection-shape properties re-derive on the same evaluation, so
//!   wire "self-shapes" as neighbors appear.
//! - **Torches** invert their support block's power with a 100 ms
//!   `After` delay — a redstone tick as a LOCAL timed event, not a
//!   global tick. A torch clock is a self-chained timer loop; two clocks
//!   on opposite sides of the world share nothing.
//! - **Lamps** light when any neighbor carries power.
//!
//! Signal speed is one wire cell per event (microseconds), not one cell
//! per game tick — redstone here propagates at causal speed.

use ultimate_engine::causal::event::{Event, EventPayload};
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::position::BlockPos;
use ultimate_engine::world::World;

use super::helpers::{block_set, horizontal_neighbors, notify_neighbors};

/// One redstone tick (vanilla: 2 game ticks = 100 ms).
const REDSTONE_TICK: u64 = 100_000_000;

// ── Block property plumbing ──────────────────────────────────────────────

fn block_parts(id: BlockId) -> Option<(String, Vec<(String, String)>)> {
    use azalea_block::{BlockState, BlockTrait};
    let state = BlockState::try_from(id.0 as u32).ok()?;
    let b: Box<dyn BlockTrait> = Box::<dyn BlockTrait>::from(state);
    let props = b
        .property_map()
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Some((b.id().to_string(), props))
}

fn block_name(id: BlockId) -> String {
    block_parts(id).map(|(n, _)| n).unwrap_or_default()
}

fn get_prop(id: BlockId, key: &str) -> Option<String> {
    block_parts(id)?.1.into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// The same block with some properties changed (None if the combination
/// doesn't exist in the state table).
fn with_props(id: BlockId, changes: &[(&str, &str)]) -> Option<BlockId> {
    let (name, mut props) = block_parts(id)?;
    for (key, value) in changes {
        match props.iter_mut().find(|(k, _)| k == key) {
            Some((_, v)) => *v = value.to_string(),
            None => return None,
        }
    }
    props.sort();
    crate::persistence::lookup_block_state(&name, &props).map(BlockId)
}

// ── Power model ──────────────────────────────────────────────────────────

fn is_wire(id: BlockId) -> bool {
    block_name(id) == "redstone_wire"
}

fn wire_power(id: BlockId) -> Option<u8> {
    if !is_wire(id) {
        return None;
    }
    get_prop(id, "power").and_then(|v| v.parse().ok())
}

/// The power level of the wire at `pos`, if there is one (tests/tools).
pub fn wire_power_at(world: &World, pos: BlockPos) -> Option<u8> {
    wire_power(world.get_block(pos))
}

/// Is this block's `lit` property true (lamps, torches)?
pub fn is_lit(id: BlockId) -> bool {
    get_prop(id, "lit").as_deref() == Some("true")
}

/// Full-strength power emitted by this block (levers, lit torches).
fn source_power(id: BlockId) -> u8 {
    let name = block_name(id);
    match name.as_str() {
        "lever" if get_prop(id, "powered").as_deref() == Some("true") => 15,
        "redstone_torch" | "redstone_wall_torch"
            if get_prop(id, "lit").as_deref() == Some("true") =>
        {
            15
        }
        _ => 0,
    }
}

/// Does any neighbor of `pos` push power into it? (Lamp/torch input.)
fn powered_by_neighbors(world: &World, pos: BlockPos) -> bool {
    for n in pos.neighbors() {
        let id = world.get_block(n);
        if source_power(id) > 0 || wire_power(id).is_some_and(|p| p > 0) {
            return true;
        }
    }
    false
}

/// If this block is a lever, its state with `powered` flipped (the
/// right-click interaction). `None` for everything else.
pub fn toggle_lever(id: BlockId) -> Option<BlockId> {
    if block_name(id) != "lever" {
        return None;
    }
    let on = get_prop(id, "powered").as_deref() == Some("true");
    with_props(id, &[("powered", if on { "false" } else { "true" })])
}

// ── The rule ─────────────────────────────────────────────────────────────

/// Registered in the standard rule sets. Dispatches on the block at the
/// event's position: wire network re-solve, lamp lit-toggle, torch
/// delayed inversion. Also relays the event upward when a torch stands
/// on the changed/notified block — a torch's input is its support, and
/// nothing else would tell it (the general form of vanilla's two-step
/// update).
pub fn redstone(world: &World, payload: &EventPayload) -> Vec<Event> {
    let pos = match payload {
        EventPayload::BlockSet { pos, .. } | EventPayload::BlockNotify { pos } => *pos,
        _ => return Vec::new(),
    };
    let id = world.get_block(pos);
    let mut events = match block_name(id).as_str() {
        "redstone_wire" => wire_update(world, pos),
        "redstone_lamp" => lamp_update(world, pos, id),
        "redstone_torch" | "redstone_wall_torch" => torch_update(world, pos, id, payload),
        _ => Vec::new(),
    };
    let above = BlockPos::new(pos.x, pos.y + 1, pos.z);
    if matches!(
        block_name(world.get_block(above)).as_str(),
        "redstone_torch" | "redstone_wall_torch"
    ) {
        events.push(Event { payload: EventPayload::BlockNotify { pos: above } });
    }
    events
}

/// Largest wire network one update will re-solve (safeguard; log-warned).
const WIRE_COMPONENT_CAP: usize = 4096;

/// Re-solve the whole connected wire network in ONE evaluation: collect
/// the component, multi-source BFS from source-fed wires (15, minus 1
/// per step), and emit a `BlockSet` for every wire whose stored state
/// differs. Neighbor-relaxation (`max(neighbor)−1` per event) is NOT
/// used: two adjacent wires feed each other's ghost values and settle on
/// a non-zero fixpoint when the real source dies — the distance-vector
/// count-to-infinity problem. Same medicine as the light engine: a
/// synchronous flood inside the rule, event-reported after the fact.
fn wire_update(world: &World, origin: BlockPos) -> Vec<Event> {
    use std::collections::{HashMap, HashSet, VecDeque};

    // Collect the connected component (horizontal adjacency, MVP).
    let mut component: HashSet<BlockPos> = HashSet::new();
    let mut stack = vec![origin];
    while let Some(c) = stack.pop() {
        if !component.insert(c) {
            continue;
        }
        if component.len() >= WIRE_COMPONENT_CAP {
            tracing::warn!("redstone component cap hit at {origin:?}");
            break;
        }
        for h in horizontal_neighbors(c) {
            if is_wire(world.get_block(h)) && !component.contains(&h) {
                stack.push(h);
            }
        }
    }

    // Multi-source BFS: wires adjacent to an emitting source seed at 15.
    let mut power: HashMap<BlockPos, u8> = HashMap::new();
    let mut queue: VecDeque<BlockPos> = VecDeque::new();
    for &c in &component {
        if c.neighbors().iter().any(|n| source_power(world.get_block(*n)) > 0) {
            power.insert(c, 15);
            queue.push_back(c);
        }
    }
    while let Some(c) = queue.pop_front() {
        let p = power[&c];
        if p <= 1 {
            continue;
        }
        for h in horizontal_neighbors(c) {
            if component.contains(&h) && power.get(&h).copied().unwrap_or(0) < p - 1 {
                power.insert(h, p - 1);
                queue.push_back(h);
            }
        }
    }

    // Emit the diffs (power + connection shape re-derived together).
    let mut events = Vec::new();
    for &c in &component {
        let id = world.get_block(c);
        let desired = power.get(&c).copied().unwrap_or(0);
        let shape = |n: BlockPos| -> &'static str {
            let nid = world.get_block(n);
            if is_wire(nid)
                || matches!(
                    block_name(nid).as_str(),
                    "lever" | "redstone_torch" | "redstone_wall_torch" | "redstone_lamp"
                )
            {
                "side"
            } else {
                "none"
            }
        };
        let power_str = desired.to_string();
        let Some(new_id) = with_props(
            id,
            &[
                ("power", &power_str),
                ("north", shape(BlockPos::new(c.x, c.y, c.z - 1))),
                ("south", shape(BlockPos::new(c.x, c.y, c.z + 1))),
                ("west", shape(BlockPos::new(c.x - 1, c.y, c.z))),
                ("east", shape(BlockPos::new(c.x + 1, c.y, c.z))),
            ],
        ) else {
            continue;
        };
        if new_id != id {
            events.push(block_set(c, id, new_id));
            // Lamps/torches beside this wire learn through these; the
            // dispatch relays notifies upward to torches on supports.
            events.extend(notify_neighbors(c));
        }
    }
    events
}

fn lamp_update(world: &World, pos: BlockPos, id: BlockId) -> Vec<Event> {
    let lit = get_prop(id, "lit").as_deref() == Some("true");
    let want = powered_by_neighbors(world, pos);
    if lit == want {
        return Vec::new();
    }
    let Some(new_id) = with_props(id, &[("lit", if want { "true" } else { "false" })]) else {
        return Vec::new();
    };
    vec![block_set(pos, id, new_id)]
}

fn torch_update(world: &World, pos: BlockPos, id: BlockId, payload: &EventPayload) -> Vec<Event> {
    let mut events = Vec::new();

    // Our own state just changed (the delayed flip applied): tell the
    // circuit. Wires and lamps react via these notifies.
    if matches!(payload, EventPayload::BlockSet { .. }) {
        events.extend(notify_neighbors(pos));
    }

    // Inversion with a one-redstone-tick delay: lit ⇔ support unpowered.
    // The input is the block we stand on (floor torch MVP): powered when
    // any of ITS neighbors (other than us) feeds it.
    let support = BlockPos::new(pos.x, pos.y - 1, pos.z);
    let input_powered = support
        .neighbors()
        .into_iter()
        .filter(|n| *n != pos)
        .any(|n| {
            let nid = world.get_block(n);
            source_power(nid) > 0 || wire_power(nid).is_some_and(|p| p > 0)
        });
    let lit = get_prop(id, "lit").as_deref() == Some("true");
    let want = !input_powered;
    if lit != want {
        if let Some(new_id) = with_props(id, &[("lit", if want { "true" } else { "false" })]) {
            // Guarded on the torch's CURRENT state: a manual replacement
            // kills the queued flip. If the input flips back inside the
            // delay, the flip still applies and immediately schedules the
            // counter-flip — a short pulse, like vanilla.
            events.push(Event {
                payload: EventPayload::After {
                    at: world.now() + REDSTONE_TICK,
                    inner: Box::new(EventPayload::BlockSet { pos, old: id, new: new_id }),
                },
            });
        }
    }
    events
}
