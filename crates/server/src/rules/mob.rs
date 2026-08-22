//! Mob skeleton (Phase 5 finisher): AI **think = a timed, self-chained
//! `EntityWake`** — cadence is a per-mob *rate*, not a global tick.
//!
//! A resting mob costs exactly one event per think (~1/s); between
//! thinks nothing runs, and movement rides the same exact-sweep
//! trajectory segments as items. The chain discipline that keeps this
//! bounded: the next-think deadline lives IN the mob's state (`aux`), a
//! new think chain is emitted only by the guarded `EntitySet` that
//! *advances* that deadline, and spurious wakes (block changes, dedup
//! deliveries) see `now < next_think` and emit nothing.

use ultimate_engine::causal::event::{Event, EventPayload};
use ultimate_engine::world::World;
use ultimate_engine::world::entity::{EntityKind, EntityState, Nanos, Vec3};

use super::entity::{is_still, kinematics_subject, plan_next, supported};

/// A mob (wandering AI skeleton).
pub const KIND_MOB: EntityKind = EntityKind(3);

/// Base think interval; per-think jitter is added on top so a crowd of
/// mobs spreads its thinks instead of pulsing in lockstep.
const THINK_BASE: Nanos = 900_000_000;
const THINK_JITTER: Nanos = 600_000_000;

/// Wander hop: horizontal speed and takeoff velocity (a short ballistic
/// hop; landing re-rests via the ordinary kinematics discipline).
const HOP_SPEED: f64 = 2.0;
const HOP_VY: f64 = 3.0;

// ── aux packing: high 48 bits = next-think deadline (ms), low 16 = variant ──

pub fn pack_mob_aux(next_think: Nanos, variant: u16) -> u64 {
    ((next_think / 1_000_000) << 16) | variant as u64
}

pub fn mob_next_think(aux: u64) -> Nanos {
    (aux >> 16) * 1_000_000
}

pub fn mob_variant(aux: u64) -> u16 {
    (aux & 0xFFFF) as u16
}

/// SplitMix64 — the same deterministic PRNG worldgen decorators use.
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// Events that bring a mob into the world: the guarded spawn plus its
/// first think wake.
pub fn spawn_mob_events(world: &World, at: Vec3, variant: u16) -> Vec<Event> {
    let id = world.entities().allocate_id();
    let now = world.now();
    let first_think = now + THINK_BASE + splitmix64(id.0) % THINK_JITTER;
    let state = EntityState {
        kind: KIND_MOB,
        pos: at,
        vel: Vec3::ZERO,
        stamp: now,
        aux: pack_mob_aux(first_think, variant),
    };
    vec![
        Event {
            payload: EventPayload::EntitySet { id, old: None, new: Some(state) },
        },
        Event {
            payload: EventPayload::After {
                at: first_think,
                inner: Box::new(EventPayload::EntityWake { id, at: at.block_pos() }),
            },
        },
    ]
}

/// Registered in both standard rule sets. Chains trajectory segments for
/// mid-flight mobs exactly like items; at rest, thinks at the mob's own
/// cadence and (sometimes) hops in a deterministic pseudo-random
/// direction.
pub fn mob_ai(world: &World, payload: &EventPayload) -> Vec<Event> {
    let Some((id, woken)) = kinematics_subject(payload) else {
        return Vec::new();
    };
    let Some(cur) = world.entities().get(id) else {
        return Vec::new(); // despawned; stale wake — no-op
    };
    if cur.kind != KIND_MOB {
        return Vec::new();
    }

    // Mid-flight (or unsupported): ordinary trajectory chaining.
    if !(is_still(&cur) && supported(world, cur.kind, cur.pos)) {
        return vec![plan_next(world, id, cur, woken)];
    }

    // At rest. Think due?
    let now = world.now();
    if now < mob_next_think(cur.aux) {
        // Spurious wake (block change, dedup delivery): the parked think
        // chain will fire on time; emitting nothing here is what keeps
        // wake storms from multiplying chains.
        return Vec::new();
    }

    // Think: advance the deadline (this guarded transition is the ONE
    // place a new chain link is minted) and roll an action.
    let next_think = now + THINK_BASE + splitmix64(id.0 ^ now) % THINK_JITTER;
    let roll = splitmix64(id.0.rotate_left(17) ^ now);
    let vel = if roll % 100 < 40 {
        // Hop toward a pseudo-random direction; the EntitySet application
        // re-enters this rule, whose mid-flight branch plans the segment.
        let theta = (roll >> 8) as f64 / (1u64 << 32) as f64 * std::f64::consts::TAU;
        Vec3::new(theta.cos() * HOP_SPEED, HOP_VY, theta.sin() * HOP_SPEED)
    } else {
        Vec3::ZERO // idle this think
    };
    let new = EntityState {
        vel,
        stamp: now,
        aux: pack_mob_aux(next_think, mob_variant(cur.aux)),
        ..cur
    };
    vec![
        Event {
            payload: EventPayload::EntitySet { id, old: Some(cur), new: Some(new) },
        },
        Event {
            payload: EventPayload::After {
                at: next_think,
                inner: Box::new(EventPayload::EntityWake { id, at: cur.pos.block_pos() }),
            },
        },
    ]
}
