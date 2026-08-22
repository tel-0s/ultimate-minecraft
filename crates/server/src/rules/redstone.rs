//! Redstone: wire, lever, lamp, torch, button, pressure plate, repeater —
//! signal propagation as causality, which is this engine's home turf.
//!
//! - **Wire** re-levels confluently to `max(adjacent source → 15,
//!   adjacent wire → power−1)`, the exact `max−1` mirror of the fluid
//!   rule's `min+1`: the fixpoint is distance-from-source, unique, so
//!   the settled circuit is independent of event execution order. Wire
//!   climbs block steps (up/down diagonal connections, occlusion-checked)
//!   and self-shapes as neighbors appear.
//! - **Torches** invert their support block's power with a 100 ms
//!   `After` delay — a redstone tick as a LOCAL timed event, not a
//!   global tick. A torch clock is a self-chained timer loop; two clocks
//!   on opposite sides of the world share nothing.
//! - **Buttons** press on use and release themselves with a timed event
//!   (stone 1.0 s, wood 1.5 s — vanilla parity).
//! - **Pressure plates** read the ENTITY substrate: pressed while any
//!   entity (item, mob, player mirror) occupies the plate's cell. Press
//!   is edge-triggered by the entity's own `EntitySet`; release is caught
//!   by the departure transition, with a self-chained re-check that runs
//!   only while pressed as a backstop.
//! - **Repeaters** are directional diodes: input behind, output ahead,
//!   flip delayed by `delay` redstone ticks. Right-click cycles the delay.
//! - **Lamps** light when any neighbor pushes power in.
//!
//! Signal speed is one wire cell per component re-solve (microseconds),
//! not one cell per game tick — redstone here propagates at causal speed.

use ultimate_engine::causal::event::{Event, EventPayload};
use ultimate_engine::world::World;
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::position::BlockPos;

use super::helpers::{block_set, horizontal_neighbors, notify_neighbors};

/// One redstone tick (vanilla: 2 game ticks = 100 ms).
const REDSTONE_TICK: u64 = 100_000_000;

// Name/property resolution rides the registry's precomputed tables
// (no per-call allocation).
use crate::registry::{block_name, block_prop as get_prop, with_props};

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
    get_prop(id, "lit") == Some("true")
}

fn is_powered(id: BlockId) -> bool {
    get_prop(id, "powered") == Some("true")
}

fn is_button(id: BlockId) -> bool {
    block_name(id).ends_with("_button")
}

fn is_plate(id: BlockId) -> bool {
    // MVP: the boolean-powered plates (weighted plates carry a 0-15
    // `power` prop instead and come with their own analog semantics).
    let n = block_name(id);
    n.ends_with("_pressure_plate")
        && !n.starts_with("light_weighted")
        && !n.starts_with("heavy_weighted")
}

fn is_repeater(id: BlockId) -> bool {
    block_name(id) == "repeater"
}

/// Horizontal unit vector for a `facing` property value.
fn facing_vec(f: &str) -> Option<(i64, i64)> {
    Some(match f {
        "north" => (0, -1),
        "south" => (0, 1),
        "west" => (-1, 0),
        "east" => (1, 0),
        _ => return None,
    })
}

/// A repeater's input cell: behind the arrow (vanilla `DiodeBlock` reads
/// its input from `pos.relative(FACING)`; placement sets FACING opposite
/// the player's look, so the input faces the placer).
fn repeater_input(pos: BlockPos, id: BlockId) -> Option<BlockPos> {
    let (dx, dz) = facing_vec(get_prop(id, "facing")?)?;
    Some(BlockPos::new(pos.x + dx, pos.y, pos.z + dz))
}

/// A repeater's output cell: the arrow tip, opposite the input.
fn repeater_output(pos: BlockPos, id: BlockId) -> Option<BlockPos> {
    let (dx, dz) = facing_vec(get_prop(id, "facing")?)?;
    Some(BlockPos::new(pos.x - dx, pos.y, pos.z - dz))
}

/// Power the block at `from` pushes into the adjacent cell `to`.
/// Directional emitters (repeaters) check the direction; everything else
/// radiates. Wire is handled separately (its power attenuates).
fn emitted_power(world: &World, from: BlockPos, to: BlockPos) -> u8 {
    let id = world.get_block(from);
    match block_name(id) {
        "lever" if is_powered(id) => 15,
        "redstone_torch" | "redstone_wall_torch" if is_lit(id) => 15,
        n if n.ends_with("_button") && is_powered(id) => 15,
        "repeater" if is_powered(id) && repeater_output(from, id) == Some(to) => 15,
        _ if is_plate(id) && is_powered(id) => 15,
        _ => 0,
    }
}

/// Does any neighbor of `pos` push power into it? (Lamp input.)
fn powered_by_neighbors(world: &World, pos: BlockPos) -> bool {
    pos.neighbors().iter().any(|n| {
        let id = world.get_block(*n);
        emitted_power(world, *n, pos) > 0 || wire_power(id).is_some_and(|p| p > 0)
    })
}

// ── Player interactions (called from the gameplay layer) ─────────────────

/// If this block is a lever, its state with `powered` flipped (the
/// right-click interaction). `None` for everything else.
pub fn toggle_lever(id: BlockId) -> Option<BlockId> {
    if block_name(id) != "lever" {
        return None;
    }
    let on = is_powered(id);
    with_props(id, &[("powered", if on { "false" } else { "true" })])
}

/// If this block is an unpressed button, its pressed state. (The release
/// schedules itself when the press applies — see `button_update`.)
pub fn press_button(id: BlockId) -> Option<BlockId> {
    if !is_button(id) || is_powered(id) {
        return None;
    }
    with_props(id, &[("powered", "true")])
}

/// If this block is a repeater, its state with the delay cycled
/// 1→2→3→4→1 (the right-click interaction).
pub fn cycle_repeater_delay(id: BlockId) -> Option<BlockId> {
    if !is_repeater(id) {
        return None;
    }
    let delay: u8 = get_prop(id, "delay")?.parse().ok()?;
    let next = if delay >= 4 { 1 } else { delay + 1 };
    with_props(id, &[("delay", &next.to_string())])
}

/// How long a button stays pressed (vanilla: stone-like 1.0 s, wood 1.5 s).
fn button_duration(id: BlockId) -> u64 {
    match block_name(id) {
        "stone_button" | "polished_blackstone_button" => 10 * REDSTONE_TICK,
        _ => 15 * REDSTONE_TICK,
    }
}

// ── The rule ─────────────────────────────────────────────────────────────

/// Registered in the standard rule sets. Dispatches on the block at the
/// event's position: wire network re-solve, lamp lit-toggle, torch
/// delayed inversion, button self-release, plate entity-check, repeater
/// delayed flip. Also relays the event upward when a torch stands on the
/// changed/notified block, and listens to ENTITY transitions to press
/// plates.
pub fn redstone(world: &World, payload: &EventPayload) -> Vec<Event> {
    let pos = match payload {
        EventPayload::BlockSet { pos, .. } | EventPayload::BlockNotify { pos } => *pos,
        // Entities press plates: an entity transition notifies plates at
        // the feet cell of BOTH endpoints (arrive → press; leave →
        // release, without waiting for the backstop poll).
        EventPayload::EntitySet { old, new, .. } => {
            let mut events = Vec::new();
            for state in [old, new].into_iter().flatten() {
                let feet = state.pos.block_pos();
                if is_plate(world.get_block(feet)) {
                    events.push(Event { payload: EventPayload::BlockNotify { pos: feet } });
                }
            }
            return events;
        }
        _ => return Vec::new(),
    };
    let id = world.get_block(pos);
    let mut events = match block_name(id) {
        "redstone_wire" => wire_update(world, pos),
        "redstone_lamp" => lamp_update(world, pos, id),
        "redstone_torch" | "redstone_wall_torch" => torch_update(world, pos, id, payload),
        "repeater" => repeater_update(world, pos, id, payload),
        n if n.ends_with("_button") => button_update(world, pos, id, payload),
        _ if is_plate(id) => plate_update(world, pos, id, payload),
        _ => Vec::new(),
    };
    let above = BlockPos::new(pos.x, pos.y + 1, pos.z);
    if matches!(
        block_name(world.get_block(above)),
        "redstone_torch" | "redstone_wall_torch"
    ) {
        events.push(Event { payload: EventPayload::BlockNotify { pos: above } });
    }
    events
}

/// Largest wire network one update will re-solve (safeguard; log-warned).
const WIRE_COMPONENT_CAP: usize = 4096;

/// Does this cell block a diagonal wire connection? (Vanilla checks
/// conductive solidity; our engine-level solidity check stands in.)
fn occludes(world: &World, pos: BlockPos) -> bool {
    crate::block::is_solid(world.get_block(pos))
}

/// The wire cells connected to the wire at `c`: same level, one step up
/// (onto a neighboring block's top, if the cell above `c` is open), one
/// step down (off an edge, if the cell beside `c` is open).
fn wire_neighbors(world: &World, c: BlockPos) -> Vec<BlockPos> {
    let mut out = Vec::with_capacity(4);
    let above_open = !occludes(world, BlockPos::new(c.x, c.y + 1, c.z));
    for h in horizontal_neighbors(c) {
        if is_wire(world.get_block(h)) {
            out.push(h);
            continue;
        }
        // Climb up onto the neighboring block's top.
        let up = BlockPos::new(h.x, h.y + 1, h.z);
        if above_open && is_wire(world.get_block(up)) {
            out.push(up);
            continue;
        }
        // Drop down off our edge.
        let down = BlockPos::new(h.x, h.y - 1, h.z);
        if !occludes(world, h) && is_wire(world.get_block(down)) {
            out.push(down);
        }
    }
    out
}

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

    // Collect the connected component (with climbing).
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
        for n in wire_neighbors(world, c) {
            if !component.contains(&n) {
                stack.push(n);
            }
        }
    }

    // Multi-source BFS: wires adjacent to an emitting source seed at 15.
    let mut power: HashMap<BlockPos, u8> = HashMap::new();
    let mut queue: VecDeque<BlockPos> = VecDeque::new();
    for &c in &component {
        if c.neighbors().iter().any(|n| emitted_power(world, *n, c) > 0) {
            power.insert(c, 15);
            queue.push_back(c);
        }
    }
    while let Some(c) = queue.pop_front() {
        let p = power[&c];
        if p <= 1 {
            continue;
        }
        for n in wire_neighbors(world, c) {
            if component.contains(&n) && power.get(&n).copied().unwrap_or(0) < p - 1 {
                power.insert(n, p - 1);
                queue.push_back(n);
            }
        }
    }

    // Emit the diffs (power + connection shape re-derived together).
    let mut events = Vec::new();
    for &c in &component {
        let id = world.get_block(c);
        let desired = power.get(&c).copied().unwrap_or(0);
        let above_open = !occludes(world, BlockPos::new(c.x, c.y + 1, c.z));
        let shape = |h: BlockPos| -> &'static str {
            // A connection climbing up over this side renders as "up".
            let up = BlockPos::new(h.x, h.y + 1, h.z);
            if above_open && !is_wire(world.get_block(h)) && is_wire(world.get_block(up)) {
                return "up";
            }
            let nid = world.get_block(h);
            let down = BlockPos::new(h.x, h.y - 1, h.z);
            if is_wire(nid)
                || (!occludes(world, h) && is_wire(world.get_block(down)))
                || matches!(
                    block_name(nid),
                    "lever" | "redstone_torch" | "redstone_wall_torch" | "redstone_lamp"
                        | "repeater"
                )
                || is_button(nid)
                || is_plate(nid)
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
            // Lamps/torches/repeaters beside this wire learn through
            // these; the dispatch relays notifies upward to torches.
            events.extend(notify_neighbors(c));
        }
    }
    events
}

fn lamp_update(world: &World, pos: BlockPos, id: BlockId) -> Vec<Event> {
    let lit = is_lit(id);
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
            emitted_power(world, n, support) > 0 || wire_power(nid).is_some_and(|p| p > 0)
        });
    let lit = is_lit(id);
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

/// Button: the press (a `BlockSet` to powered, submitted by the gameplay
/// layer) notifies the circuit and schedules its own release — a timed,
/// guarded un-press. Re-pressing a pressed button is a no-op upstream
/// (`press_button` returns `None`).
fn button_update(world: &World, pos: BlockPos, id: BlockId, payload: &EventPayload) -> Vec<Event> {
    let mut events = Vec::new();
    if !matches!(payload, EventPayload::BlockSet { .. }) {
        return events;
    }
    events.extend(notify_neighbors(pos));
    if is_powered(id) {
        if let Some(released) = with_props(id, &[("powered", "false")]) {
            events.push(Event {
                payload: EventPayload::After {
                    at: world.now() + button_duration(id),
                    // Guarded on the pressed state: breaking or replacing
                    // the button inside the window kills the queued
                    // release at the stale guard.
                    inner: Box::new(EventPayload::BlockSet { pos, old: id, new: released }),
                },
            });
        }
    }
    events
}

/// How often a pressed plate re-checks for release. Only pressed plates
/// poll, and only until they release — an idle plate costs zero.
const PLATE_RECHECK: u64 = 5 * REDSTONE_TICK;

/// Pressure plate: pressed ⇔ some entity's feet occupy the plate's cell.
/// Press is edge-triggered by the entity's `EntitySet` (see the
/// dispatch); release is caught by the departure transition, with a
/// self-chained re-check that runs only while pressed as a backstop.
/// (Spurious notifies on a pressed plate can park extra re-check timers;
/// they're cheap, their notifies dedup-coalesce on delivery, and they
/// stop the moment the plate releases.)
fn plate_update(world: &World, pos: BlockPos, id: BlockId, payload: &EventPayload) -> Vec<Event> {
    let mut events = Vec::new();
    if matches!(payload, EventPayload::BlockSet { .. }) {
        events.extend(notify_neighbors(pos));
    }

    let occupied = world
        .entities()
        .in_column(pos.x, pos.z)
        .into_iter()
        .filter_map(|eid| world.entities().get(eid))
        .any(|s| s.pos.block_pos().y == pos.y);
    let pressed = is_powered(id);

    if occupied != pressed {
        if let Some(new_id) =
            with_props(id, &[("powered", if occupied { "true" } else { "false" })])
        {
            events.push(block_set(pos, id, new_id));
        }
    }
    if occupied {
        // Backstop release poll, alive only while something stands here.
        events.push(Event {
            payload: EventPayload::After {
                at: world.now() + PLATE_RECHECK,
                inner: Box::new(EventPayload::BlockNotify { pos }),
            },
        });
    }
    events
}

/// Repeater: a directional diode with a configurable delay. Input is
/// read behind the arrow only; output pushes ahead only
/// (`emitted_power`), so signal isolation holds by construction.
fn repeater_update(world: &World, pos: BlockPos, id: BlockId, payload: &EventPayload) -> Vec<Event> {
    let mut events = Vec::new();

    // Our delayed flip just applied: tell the circuit (the output wire
    // re-solves; it seeds from us via `emitted_power`).
    if matches!(payload, EventPayload::BlockSet { .. }) {
        events.extend(notify_neighbors(pos));
    }

    let Some(input) = repeater_input(pos, id) else {
        return events;
    };
    let input_powered = {
        let iid = world.get_block(input);
        wire_power(iid).is_some_and(|p| p > 0) || emitted_power(world, input, pos) > 0
    };
    let powered = is_powered(id);
    if input_powered != powered {
        let delay: u64 = get_prop(id, "delay")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        if let Some(new_id) =
            with_props(id, &[("powered", if input_powered { "true" } else { "false" })])
        {
            // Guarded on the CURRENT state, like the torch: superseded
            // flips die at the stale guard.
            events.push(Event {
                payload: EventPayload::After {
                    at: world.now() + delay * REDSTONE_TICK,
                    inner: Box::new(EventPayload::BlockSet { pos, old: id, new: new_id }),
                },
            });
        }
    }
    events
}
