//! Entity kinematics rules (Phase 5): the dropped-item MVP.
//!
//! The design contract (docs/phase5-entities.md):
//! - Entity state is a parametric trajectory (pos, vel, stamp). The rule
//!   plans ONE segment ahead — to the first collision, or a 1 s
//!   extrapolation cap — and emits a single `After`-wrapped `EntitySet`
//!   for the segment's end. Between events nothing runs.
//! - At rest (supported, still) the rule emits NOTHING. A resting entity
//!   is woken exclusively by `EntityWake` (block change, timer, pickup).
//! - Every `EntitySet` is guarded on `old`; a superseded in-flight segment
//!   dies at the guard instead of forking the entity.
//! - Wakes are idempotent: spurious or duplicate wakes re-derive from the
//!   store and re-sleep.

use ultimate_engine::causal::event::{Event, EventPayload};
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::entity::{EntityKind, EntityState, Nanos, Vec3};
use ultimate_engine::world::position::BlockPos;
use ultimate_engine::world::World;

use crate::block;

/// Dropped item. (0 is reserved for players, mirrored in later work.)
pub const KIND_ITEM: EntityKind = EntityKind(1);

/// Gravity for items, blocks/s². (Vanilla's 0.04 blocks/tick² with drag
/// lands near this effective value; we skip air drag for the MVP.)
const GRAVITY: f64 = -20.0;
/// Maximum trajectory-segment length: bounds client extrapolation drift.
const T_CAP: Nanos = 1_000_000_000;
/// Planning resolution for the swept collision check. Planning cost only —
/// paid once per segment, never per frame.
const SUBSTEP: Nanos = 25_000_000;
/// Items despawn 5 minutes after spawn (vanilla parity).
pub const DESPAWN_AFTER: Nanos = 300_000_000_000;
/// |velocity| below this counts as still.
const REST_EPS: f64 = 0.01;

// ── aux packing: high 48 bits = despawn deadline (ms), low 16 = block id ──

fn pack_aux(despawn_at: Nanos, dropped: BlockId) -> u64 {
    ((despawn_at / 1_000_000) << 16) | dropped.0 as u64
}

pub fn aux_despawn_at(aux: u64) -> Nanos {
    (aux >> 16) * 1_000_000
}

pub fn aux_block(aux: u64) -> BlockId {
    BlockId((aux & 0xFFFF) as u16)
}

// ── Spawning ─────────────────────────────────────────────────────────────

/// Events that bring a dropped item into the world: the guarded spawn
/// `EntitySet` plus its despawn timer. Deterministic pop velocity (derived
/// from the id) so replicas/replays agree.
pub fn spawn_item_events(world: &World, at: BlockPos, dropped: BlockId) -> Vec<Event> {
    let id = world.entities().allocate_id();
    let now = world.now();
    let angle = (id.0.wrapping_mul(0x9E3779B97F4A7C15) >> 40) as f64 / (1 << 24) as f64
        * std::f64::consts::TAU;
    let state = EntityState {
        kind: KIND_ITEM,
        pos: Vec3::new(at.x as f64 + 0.5, at.y as f64 + 0.5, at.z as f64 + 0.5),
        vel: Vec3::new(angle.cos() * 0.5, 2.0, angle.sin() * 0.5),
        stamp: now,
        aux: pack_aux(now + DESPAWN_AFTER, dropped),
    };
    vec![
        Event {
            payload: EventPayload::EntitySet { id, old: None, new: Some(state) },
        },
        Event {
            payload: EventPayload::After {
                at: now + DESPAWN_AFTER,
                inner: Box::new(EventPayload::EntityWake { id, at }),
            },
        },
    ]
}

// ── The kinematics rule ──────────────────────────────────────────────────

/// Registered in `rules::standard()`. Plans the next trajectory segment
/// for items on every `EntitySet` application and `EntityWake`.
pub fn item_kinematics(world: &World, payload: &EventPayload) -> Vec<Event> {
    let (id, restamp) = match payload {
        // A segment endpoint just applied: keep chaining on its timeline.
        EventPayload::EntitySet { id, new: Some(_), .. } => (*id, false),
        // External cause (block change, timer, interaction): the entity's
        // stored stamp may be arbitrarily old (it was asleep) — re-stamp
        // to NOW so it starts moving now, not "since it fell asleep".
        EventPayload::EntityWake { id, .. } => (*id, true),
        _ => return Vec::new(),
    };

    let Some(cur) = world.entities().get(id) else {
        return Vec::new(); // already despawned; stale wake — no-op
    };
    if cur.kind != KIND_ITEM {
        return Vec::new();
    }

    // Despawn horizon.
    if world.now() >= aux_despawn_at(cur.aux) {
        return vec![Event {
            payload: EventPayload::EntitySet { id, old: Some(cur), new: None },
        }];
    }

    // At rest and supported: sleep. (The rest-costs-nothing invariant.)
    let still = cur.vel.x.abs() < REST_EPS
        && cur.vel.y.abs() < REST_EPS
        && cur.vel.z.abs() < REST_EPS;
    if still && supported(world, cur.pos) {
        return Vec::new();
    }

    let start = if restamp { EntityState { stamp: world.now(), ..cur } } else { cur };
    let end = plan_segment(world, &start);
    vec![Event {
        payload: EventPayload::After {
            at: end.stamp,
            // Guard on the STORED state — if anything else transitions the
            // entity before this segment lands, the segment dies unapplied.
            inner: Box::new(EventPayload::EntitySet { id, old: Some(cur), new: Some(end) }),
        },
    }]
}

/// Standing on a solid top face?
fn supported(world: &World, pos: Vec3) -> bool {
    let below = Vec3::new(pos.x, pos.y - 0.06, pos.z).block_pos();
    block::is_solid(world.get_block(below))
}

/// Integrate the trajectory in planning substeps until it enters a solid
/// cell or the extrapolation cap. Returns the segment-end state (velocity
/// zeroed on impact; the follow-up evaluation re-sleeps or re-falls).
fn plan_segment(world: &World, start: &EntityState) -> EntityState {
    let mut pos = start.pos;
    let mut vel = start.vel;
    let dt = SUBSTEP as f64 / 1e9;
    let steps = (T_CAP / SUBSTEP).max(1);

    for i in 1..=steps {
        vel.y += GRAVITY * dt;
        let next = Vec3::new(pos.x + vel.x * dt, pos.y + vel.y * dt, pos.z + vel.z * dt);
        if block::is_solid(world.get_block(next.block_pos())) {
            // Impact: stop at the last free position. If we were falling,
            // snap to the top face of the cell below for a clean rest.
            let mut landed = pos;
            if vel.y < 0.0 {
                landed.y = next.block_pos().y as f64 + 1.0;
            }
            return EntityState {
                pos: landed,
                vel: Vec3::ZERO,
                stamp: start.stamp + i * SUBSTEP,
                ..*start
            };
        }
        pos = next;
    }

    EntityState { pos, vel, stamp: start.stamp + T_CAP, ..*start }
}

// ── Wake-on-block-change ─────────────────────────────────────────────────

/// When a block changes, wake nearby entities in its chunk column so they
/// re-verify support / clearance. Chunk-granular and deliberately
/// over-eager — waking a still-supported entity is a cheap no-op (it
/// re-sleeps), and `EntityWake` dedup-coalesces in the graph.
pub fn entity_block_wake(world: &World, payload: &EventPayload) -> Vec<Event> {
    let EventPayload::BlockSet { pos, .. } = payload else {
        return Vec::new();
    };
    let ids = world.entities().in_chunk(pos.chunk());
    if ids.is_empty() {
        return Vec::new();
    }
    let mut events = Vec::new();
    for id in ids {
        let Some(s) = world.entities().get(id) else { continue };
        // Entities whose footprint or support column the change could
        // affect: nearby horizontally, at-or-above the changed cell.
        if (s.pos.x - (pos.x as f64 + 0.5)).abs() <= 1.5
            && (s.pos.z - (pos.z as f64 + 0.5)).abs() <= 1.5
            && s.pos.y >= pos.y as f64 - 1.0
        {
            events.push(Event {
                payload: EventPayload::EntityWake { id, at: s.pos.block_pos() },
            });
        }
    }
    events
}
