//! Light propagation rule — BFS flood-fill inside the rule.
//!
//! When a `BlockSet` event fires, this rule runs a synchronous BFS that
//! recomputes block-light and (if the chunk is sky-lit) sky-light in the
//! affected region. It writes directly to world light storage and emits one
//! `LightSet` event per actually-changed cell so `event_bus::collect_light_changes`
//! can locate touched sections for client `LightUpdate` packets.
//!
//! `LightSet` / `LightNotify` events never produce consequents — all
//! propagation is completed synchronously inside this single rule invocation.
//! This replaces the previous event-cascading approach, which generated
//! O(10^5) events per torch placement from the BFS frontier being expressed
//! as graph nodes.

use std::collections::{HashMap, VecDeque};

use crate::block;
use ultimate_engine::causal::event::{Event, EventPayload, LightType};
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::position::BlockPos;
use ultimate_engine::world::World;

const MIN_Y: i64 = -64;
const MAX_Y: i64 = 319;

pub fn light_propagation(world: &World, payload: &EventPayload) -> Vec<Event> {
    match payload {
        EventPayload::BlockSet { pos, old, new } => update_light(world, *pos, *old, *new),
        _ => Vec::new(),
    }
}

fn update_light(world: &World, pos: BlockPos, old: BlockId, new: BlockId) -> Vec<Event> {
    let old_emit = block::light_emission(old);
    let new_emit = block::light_emission(new);
    let old_opacity = block::light_opacity(old);
    let new_opacity = block::light_opacity(new);

    if old_emit == new_emit && old_opacity == new_opacity {
        return Vec::new();
    }

    let mut events = Vec::new();
    update_block_light(world, pos, new_emit, new_opacity, &mut events);

    if old_opacity != new_opacity && world.is_sky_lit(&pos.chunk()) {
        update_sky_light(world, pos, new_opacity, &mut events);
    }

    events
}

// ── Block light ──────────────────────────────────────────────────────────────

/// BFS update for block-light after an emission/opacity change at `pos`.
///
/// Standard two-phase algorithm: first a "darkness" BFS that clears cells
/// whose level was inherited from the changed region, then an "addition" BFS
/// that re-propagates from every independent source encountered during the
/// first phase.
fn update_block_light(
    world: &World,
    pos: BlockPos,
    new_emit: u8,
    new_opacity: u8,
    events: &mut Vec<Event>,
) {
    let old_level = world.get_block_light(pos);
    // Value pos "starts" at after the change, before BFS — just its own emission.
    let pos_seed = new_emit;

    // Net-change tracker: first observed old value, latest new value per cell.
    let mut changed: HashMap<BlockPos, (u8, u8)> = HashMap::new();
    let mut removal: VecDeque<(BlockPos, u8)> = VecDeque::new();
    let mut addition: VecDeque<BlockPos> = VecDeque::new();

    if pos_seed != old_level {
        record(&mut changed, pos, old_level, pos_seed);
        world.set_block_light(pos, pos_seed);
    }
    if old_level > pos_seed {
        removal.push_back((pos, old_level));
    }
    if pos_seed > 0 {
        addition.push_back(pos);
    }
    // Opacity rose without emission change: pos may no longer admit neighbor light
    // into surrounding cells. Enqueue neighbors of pos for re-evaluation via the
    // removal channel (they might shed levels that were propagating *through* pos).
    if new_opacity > 0 && old_level > 0 {
        // The removal seed above already covers this — neighbors at lower levels
        // will be cleared, and any real sources among them get promoted to
        // addition. Nothing extra needed.
    }

    // Removal phase.
    while let Some((p, old_l)) = removal.pop_front() {
        for n in p.neighbors() {
            if n.y < MIN_Y || n.y > MAX_Y { continue; }
            let n_block = world.get_block(n);
            let n_emit = block::light_emission(n_block);
            if n_emit > 0 {
                // Neighbor is an emitter — keep its level, re-propagate from it.
                addition.push_back(n);
                continue;
            }
            let n_l = world.get_block_light(n);
            if n_l == 0 { continue; }
            if n_l < old_l {
                // Possibly lit by us; clear and propagate the removal outward.
                record(&mut changed, n, n_l, 0);
                world.set_block_light_if_loaded(n, 0);
                removal.push_back((n, n_l));
            } else {
                // Independent source; re-propagate from it.
                addition.push_back(n);
            }
        }
    }

    // Addition phase.
    while let Some(p) = addition.pop_front() {
        let p_l = world.get_block_light(p);
        if p_l == 0 { continue; }
        for n in p.neighbors() {
            if n.y < MIN_Y || n.y > MAX_Y { continue; }
            let n_block = world.get_block(n);
            let n_opacity = block::light_opacity(n_block);
            let n_emit = block::light_emission(n_block);
            let n_current = world.get_block_light(n);
            let from_p = p_l.saturating_sub(1.max(n_opacity));
            let target = from_p.max(n_emit);
            if target > n_current {
                record(&mut changed, n, n_current, target);
                if !world.set_block_light_if_loaded(n, target) { continue; }
                addition.push_back(n);
            }
        }
    }

    emit_light_events(changed, LightType::Block, events);
}

// ── Sky light ────────────────────────────────────────────────────────────────

/// BFS update for sky-light after an opacity change at `pos`.
///
/// Sky light has a "column" rule: a transparent cell whose direct neighbor
/// above is also transparent and at level 15 inherits level 15 (no attenuation).
/// We honor this by seeding `pos` via `compute_sky_at`, and during the addition
/// phase we let level-15 propagate downward into transparent cells at full
/// strength. For bounded opacity changes this is correct; an unbounded column
/// re-evaluation (block inserted at the top of a tall open shaft) is not yet
/// handled — the BFS radius is 15, which is fine for most placements.
fn update_sky_light(
    world: &World,
    pos: BlockPos,
    new_opacity: u8,
    events: &mut Vec<Event>,
) {
    let old_level = world.get_sky_light(pos);
    let new_desired = compute_sky_at(world, pos, new_opacity);

    let mut changed: HashMap<BlockPos, (u8, u8)> = HashMap::new();
    let mut removal: VecDeque<(BlockPos, u8)> = VecDeque::new();
    let mut addition: VecDeque<BlockPos> = VecDeque::new();

    if new_desired != old_level {
        record(&mut changed, pos, old_level, new_desired);
        world.set_sky_light(pos, new_desired);
    }
    if old_level > new_desired {
        removal.push_back((pos, old_level));
    }
    if new_desired > 0 {
        addition.push_back(pos);
    }

    while let Some((p, old_l)) = removal.pop_front() {
        for n in p.neighbors() {
            if n.y < MIN_Y || n.y > MAX_Y { continue; }
            let n_l = world.get_sky_light(n);
            if n_l == 0 { continue; }
            if n_l < old_l {
                record(&mut changed, n, n_l, 0);
                world.set_sky_light_if_loaded(n, 0);
                removal.push_back((n, n_l));
            } else {
                addition.push_back(n);
            }
        }
    }

    while let Some(p) = addition.pop_front() {
        let p_l = world.get_sky_light(p);
        if p_l == 0 { continue; }
        for n in p.neighbors() {
            if n.y < MIN_Y || n.y > MAX_Y { continue; }
            let n_block = world.get_block(n);
            let n_opacity = block::light_opacity(n_block);
            let n_current = world.get_sky_light(n);
            // Column rule: moving down one cell at level 15 through a transparent
            // target preserves 15.
            let is_down_column = p_l == 15 && n.y == p.y - 1 && n_opacity == 0;
            let target = if is_down_column {
                15
            } else {
                p_l.saturating_sub(1.max(n_opacity))
            };
            if target > n_current {
                record(&mut changed, n, n_current, target);
                if !world.set_sky_light_if_loaded(n, target) { continue; }
                addition.push_back(n);
            }
        }
    }

    emit_light_events(changed, LightType::Sky, events);
}

/// Compute what sky-light should be at `pos` given its opacity, honoring the
/// direct-column rule for transparent cells under an unobstructed sky.
fn compute_sky_at(world: &World, pos: BlockPos, opacity: u8) -> u8 {
    if opacity == 0 {
        let above = BlockPos::new(pos.x, pos.y + 1, pos.z);
        if above.y <= MAX_Y && world.get_sky_light(above) == 15 {
            let above_opacity = block::light_opacity(world.get_block(above));
            if above_opacity == 0 {
                return 15;
            }
        }
    }
    let best_nb = pos
        .neighbors()
        .into_iter()
        .filter(|nb| nb.y >= MIN_Y && nb.y <= MAX_Y)
        .map(|nb| world.get_sky_light(nb))
        .max()
        .unwrap_or(0);
    best_nb.saturating_sub(1.max(opacity))
}

// ── Shared helpers ───────────────────────────────────────────────────────────

fn record(changed: &mut HashMap<BlockPos, (u8, u8)>, pos: BlockPos, old: u8, new: u8) {
    changed
        .entry(pos)
        .and_modify(|e| e.1 = new)
        .or_insert((old, new));
}

fn emit_light_events(
    changed: HashMap<BlockPos, (u8, u8)>,
    light_type: LightType,
    events: &mut Vec<Event>,
) {
    events.reserve(changed.len());
    for (cell, (old_v, new_v)) in changed {
        if old_v != new_v {
            events.push(Event {
                payload: EventPayload::LightSet {
                    pos: cell,
                    light_type,
                    old: old_v,
                    new: new_v,
                },
            });
        }
    }
}
