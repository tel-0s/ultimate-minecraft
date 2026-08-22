//! Entity kinematics rules (Phase 5): the dropped-item MVP.
//!
//! The design contract (docs/phase5-entities.md):
//! - Entity state is a parametric trajectory (pos, vel, stamp). The rule
//!   plans ONE segment ahead — to the first collision (EXACT parabolic
//!   voxel sweep, see `plan_segment`), or a 1 s extrapolation cap — and
//!   emits a single `After`-wrapped `EntitySet` for the segment's end.
//!   Between events nothing runs.
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

/// A player (Phase 5 unification): position authority lives in the
/// EntityStore — rules see players as ordinary spatial actors, and
/// replicas track them through WriteSync. Identity (name/uuid/tab) and
/// the movement RENDER path stay with `PlayerRegistry`; player entities
/// are externally driven (their connection is the only writer), so no
/// kinematics rule matches them and wakes skip them.
pub const KIND_PLAYER: EntityKind = EntityKind(0);
/// Dropped item.
pub const KIND_ITEM: EntityKind = EntityKind(1);
/// A detached falling block (vanilla sand/gravel parity). `aux` = the
/// block id; on landing it converts back into a block (or a dropped item
/// if something stole the landing cell).
pub const KIND_FALLING_BLOCK: EntityKind = EntityKind(2);

/// Gravity for items, blocks/s². (Vanilla's 0.04 blocks/tick² with drag
/// lands near this effective value; we skip air drag for the MVP.)
const GRAVITY: f64 = -20.0;
/// Maximum trajectory-segment length: bounds client extrapolation drift.
const T_CAP: Nanos = 1_000_000_000;
/// Escape hatch when a segment STARTS inside a solid cell (a block was
/// placed into the entity's cell): the entity pops onto the cell top
/// after this much virtual time, so the zero-length "segment" still
/// advances the timeline.
const BURIED_ESCAPE: Nanos = 25_000_000;
/// Offset from a face a side/ceiling impact rests at, so the resting
/// point floors into the free cell, not the wall.
const FACE_EPS: f64 = 1e-6;
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

// ── Player entities ──────────────────────────────────────────────────────

/// EntityStore id for a player, derived from the registry's i32 entity
/// id in a high-bit namespace so it can never collide with allocated ids
/// (items, falling blocks — counter-based, node-salted in bits 48..63).
pub fn player_entity_id(registry_eid: i32) -> ultimate_engine::world::entity::EntityId {
    ultimate_engine::world::entity::EntityId(0x8000_0000_0000_0000 | registry_eid as u32 as u64)
}

/// Player `aux` packs the view rotations (engine state is game-agnostic;
/// rotations matter only to renderers and future AI rules).
pub fn player_aux(y_rot: f32, x_rot: f32) -> u64 {
    ((y_rot.to_bits() as u64) << 32) | x_rot.to_bits() as u64
}

pub fn player_state(
    pos: Vec3,
    y_rot: f32,
    x_rot: f32,
    stamp: Nanos,
) -> EntityState {
    EntityState {
        kind: KIND_PLAYER,
        pos,
        vel: Vec3::ZERO,
        stamp,
        aux: player_aux(y_rot, x_rot),
    }
}

// ── Spawning ─────────────────────────────────────────────────────────────

/// Entity id for an item popped off a broken support at `pos` — DERIVED
/// from the position (bit-62 namespace; x/z 26 bits, y 9 bits) instead
/// of allocated. Determinism is what makes pop drops exactly-once with
/// no new machinery: two racing pop evaluations spawn the SAME id, and
/// the second dies at the ordinary `EntitySet { old: None }` guard.
/// (While a popped item lies unclaimed on its cell, a second pop at that
/// exact cell is suppressed — accepted, documented coarseness.)
pub fn pop_item_id(pos: BlockPos) -> ultimate_engine::world::entity::EntityId {
    let x = (pos.x as u64) & 0x3FF_FFFF;
    let z = (pos.z as u64) & 0x3FF_FFFF;
    let y = ((pos.y + 64) as u64) & 0x1FF;
    ultimate_engine::world::entity::EntityId((1 << 62) | (x << 35) | (z << 9) | y)
}

/// Events that bring a dropped item into the world: the guarded spawn
/// `EntitySet` plus its despawn timer. Deterministic pop velocity (derived
/// from the id) so replicas/replays agree.
pub fn spawn_item_events(world: &World, at: BlockPos, dropped: BlockId) -> Vec<Event> {
    let id = world.entities().allocate_id();
    spawn_item_events_with_id(world, id, at, dropped)
}

/// `spawn_item_events` with a caller-chosen id (position-derived pop
/// drops use this for guard-based exactly-once).
pub fn spawn_item_events_with_id(
    world: &World,
    id: ultimate_engine::world::entity::EntityId,
    at: BlockPos,
    dropped: BlockId,
) -> Vec<Event> {
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

/// Which entity does this payload concern (for kinematics rules)?
pub(crate) fn kinematics_subject(payload: &EventPayload) -> Option<(ultimate_engine::world::entity::EntityId, bool)> {
    match payload {
        // A segment endpoint just applied: keep chaining on its timeline.
        EventPayload::EntitySet { id, new: Some(_), .. } => Some((*id, false)),
        // External cause (block change, timer, interaction).
        EventPayload::EntityWake { id, .. } => Some((*id, true)),
        _ => None,
    }
}

pub(crate) fn is_still(s: &EntityState) -> bool {
    s.vel.x.abs() < REST_EPS && s.vel.y.abs() < REST_EPS && s.vel.z.abs() < REST_EPS
}

/// Plan the next segment from `cur` and wrap it as a timed, guarded
/// transition.
///
/// Re-stamping subtlety: a wake on a STILL entity re-stamps to now (it was
/// asleep — it starts moving now, not "since it fell asleep"). A wake on a
/// MOVING entity must NOT re-stamp: its stored state is its segment START,
/// so re-stamping would rewind it. Instead we re-plan on the same timeline
/// — the world changed, so the recomputed segment (e.g. an earlier landing
/// on a newly placed block) carries an earlier deadline and beats the
/// superseded in-flight segment to the guard. A block *removed* from the
/// path makes the old (now floating) endpoint win instead — and its
/// removal wake then re-derives, so it self-heals downward.
pub(crate) fn plan_next(world: &World, id: ultimate_engine::world::entity::EntityId, cur: EntityState, woken: bool) -> Event {
    let start = if woken && is_still(&cur) {
        EntityState { stamp: world.now(), ..cur }
    } else {
        cur
    };
    let end = plan_segment(world, &start);
    Event {
        payload: EventPayload::After {
            at: end.stamp,
            // Guard on the STORED state — if anything else transitions the
            // entity before this segment lands, the segment dies unapplied.
            inner: Box::new(EventPayload::EntitySet { id, old: Some(cur), new: Some(end) }),
        },
    }
}

/// Registered in `rules::standard()`. Plans the next trajectory segment
/// for items on every `EntitySet` application and `EntityWake`.
pub fn item_kinematics(world: &World, payload: &EventPayload) -> Vec<Event> {
    let Some((id, woken)) = kinematics_subject(payload) else {
        return Vec::new();
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
    if is_still(&cur) && supported(world, cur.kind, cur.pos) {
        return Vec::new();
    }

    vec![plan_next(world, id, cur, woken)]
}

/// Does the block at this cell stop entities of `kind`? Falling blocks
/// keep the strict rule (they must land exactly where instant gravity
/// would put the block); items and mobs pass through attachments and
/// plants — which is what lets a pig stand IN a pressure plate's cell
/// and press it.
pub(crate) fn collides(kind: EntityKind, id: BlockId) -> bool {
    if kind == KIND_FALLING_BLOCK {
        block::is_solid(id)
    } else {
        block::blocks_entity_movement(id)
    }
}

/// Standing on a solid top face? Center-point, consistent with the
/// point trajectory sweep — footprint-corner probing without a matching
/// Minkowski-dilated sweep lets a box "stand" on a wall it is pressed
/// against (found by the fast-item wall test). True per-kind AABB
/// sweeps (dilating the DDA by the half-width) are the follow-up.
pub(crate) fn supported(world: &World, kind: EntityKind, pos: Vec3) -> bool {
    let below = Vec3::new(pos.x, pos.y - 0.06, pos.z).block_pos();
    collides(kind, world.get_block(below))
}

/// EXACT swept collision: walk the parabolic trajectory
/// (`x,z` linear, `y` quadratic under gravity) through the voxel grid by
/// solving successive cell-boundary crossing times, until it enters a
/// solid cell or reaches the extrapolation cap. Replaces the fixed-25ms
/// substep sampler, which could tunnel through single blocks above
/// ~40 blocks/s and quantized landing times to the substep. Cost is
/// O(cells traversed); the math is pure f64, so trajectories stay
/// bit-identical across worker counts.
///
/// Returns the segment-end state — velocity zeroed on impact; the
/// follow-up evaluation re-sleeps, converts (falling blocks), or
/// re-falls (side/ceiling stops re-plan straight down).
fn plan_segment(world: &World, start: &EntityState) -> EntityState {
    let (p0, v0) = (start.pos, start.vel);
    let t_max = T_CAP as f64 / 1e9;
    let mut cell = p0.block_pos();
    // A rest pose sits EXACTLY on a cell boundary (y = k + 1.0). A
    // non-rising entity there occupies the cell BELOW — without this the
    // τ=0 double root of the floor plane is filtered as "already
    // crossed" and the sweep never sees the entity enter it (planning a
    // clean 1-second drop through the floor).
    if p0.y == cell.y as f64 && v0.y <= 0.0 {
        cell.y -= 1;
    }

    // Segment starts inside a solid cell (a block was placed into the
    // entity's cell): pop onto the cell top, like the sampler did.
    if collides(start.kind, world.get_block(cell)) {
        return EntityState {
            pos: Vec3::new(p0.x, cell.y as f64 + 1.0, p0.z),
            vel: Vec3::ZERO,
            stamp: start.stamp + BURIED_ESCAPE,
            ..*start
        };
    }

    // Position on the exact parabola at time t since segment start.
    let at = |t: f64| {
        Vec3::new(
            p0.x + v0.x * t,
            p0.y + v0.y * t + 0.5 * GRAVITY * t * t,
            p0.z + v0.z * t,
        )
    };

    // Linear DDA state per horizontal axis: time of next boundary
    // crossing and the fixed per-cell time step.
    let axis_init = |x0: f64, vx: f64, cx: i64| -> (f64, f64) {
        if vx > 0.0 {
            (((cx + 1) as f64 - x0) / vx, 1.0 / vx)
        } else if vx < 0.0 {
            ((cx as f64 - x0) / vx, -1.0 / vx)
        } else {
            (f64::INFINITY, f64::INFINITY)
        }
    };
    let (mut t_x, dt_x) = axis_init(p0.x, v0.x, cell.x);
    let (mut t_z, dt_z) = axis_init(p0.z, v0.z, cell.z);

    // Next y-boundary crossing strictly after `t`, leaving the current
    // cell. The parabola is concave (GRAVITY < 0), so it crosses any
    // horizontal line at most twice: once rising (smaller root), once
    // falling (larger root). Returns (t_cross, dy).
    let next_y = |t: f64, cy: i64| -> (f64, i64) {
        // Roots of ½g·τ² + v0y·τ + (p0.y - yb) = 0.
        let roots = |yb: f64| -> (f64, f64) {
            let (a, b, c) = (0.5 * GRAVITY, v0.y, p0.y - yb);
            let disc = b * b - 4.0 * a * c;
            if disc < 0.0 {
                return (f64::INFINITY, f64::INFINITY);
            }
            let sq = disc.sqrt();
            // a < 0: (−b+√)/2a is the SMALLER (rising) root.
            let r1 = (-b + sq) / (2.0 * a);
            let r2 = (-b - sq) / (2.0 * a);
            (r1.min(r2), r1.max(r2))
        };
        const T_EPS: f64 = 1e-12;
        // Rising exit through the ceiling plane…
        let (up, _) = roots((cy + 1) as f64);
        // …or falling exit through the floor plane.
        let (_, down) = roots(cy as f64);
        let up = if up > t + T_EPS { up } else { f64::INFINITY };
        let down = if down > t + T_EPS { down } else { f64::INFINITY };
        if up < down { (up, 1) } else { (down, -1) }
    };

    let mut t = 0.0;
    loop {
        let (t_y, dy) = next_y(t, cell.y);
        // Earliest crossing wins; ties break y-first (deterministic).
        let (t_cross, axis) = if t_y <= t_x && t_y <= t_z {
            (t_y, 1)
        } else if t_x <= t_z {
            (t_x, 0)
        } else {
            (t_z, 2)
        };

        if t_cross > t_max {
            // No solid within the cap: chain another segment from the
            // exact parabola state at t_max.
            return EntityState {
                pos: at(t_max),
                vel: Vec3::new(v0.x, v0.y + GRAVITY * t_max, v0.z),
                stamp: start.stamp + T_CAP,
                ..*start
            };
        }

        let entered = match axis {
            1 => BlockPos::new(cell.x, cell.y + dy, cell.z),
            0 => BlockPos::new(cell.x + v0.x.signum() as i64, cell.y, cell.z),
            _ => BlockPos::new(cell.x, cell.y, cell.z + v0.z.signum() as i64),
        };

        if collides(start.kind, world.get_block(entered)) {
            let hit = at(t_cross);
            let pos = match (axis, dy) {
                // Landing on a top face: rest exactly on it.
                (1, -1) => Vec3::new(hit.x, entered.y as f64 + 1.0, hit.z),
                // Ceiling: stop just below the face.
                (1, _) => Vec3::new(hit.x, entered.y as f64 - FACE_EPS, hit.z),
                // Wall: stop just in front of the face; the follow-up
                // evaluation finds no support and re-plans straight down.
                // (The sampler used to VAULT side-grazes onto the wall
                // top whenever the entity was falling — an artifact of
                // its 0.5-block resolution, not vanilla behavior.)
                (0, _) => {
                    let face = if v0.x > 0.0 {
                        entered.x as f64 - FACE_EPS
                    } else {
                        (entered.x + 1) as f64 + FACE_EPS
                    };
                    Vec3::new(face, hit.y, hit.z)
                }
                _ => {
                    let face = if v0.z > 0.0 {
                        entered.z as f64 - FACE_EPS
                    } else {
                        (entered.z + 1) as f64 + FACE_EPS
                    };
                    Vec3::new(hit.x, hit.y, face)
                }
            };
            return EntityState {
                pos,
                vel: Vec3::ZERO,
                stamp: start.stamp + (t_cross * 1e9) as Nanos,
                ..*start
            };
        }

        cell = entered;
        match axis {
            0 => t_x += dt_x,
            2 => t_z += dt_z,
            _ => {}
        }
        t = t_cross;
    }
}

// ── FallingBlock (vanilla sand/gravel parity) ────────────────────────────

/// Replaces the instant `block_updates::gravity` rule in
/// `rules::standard_with_falling_blocks()`: an unsupported gravity block
/// DETACHES into a FallingBlock entity instead of teleporting down cell
/// by cell. Final block state is identical; the difference is that the
/// fall is now a visible, causally-paced trajectory.
pub fn falling_block_gravity(world: &World, payload: &EventPayload) -> Vec<Event> {
    let pos = match payload {
        EventPayload::BlockSet { pos, .. } | EventPayload::BlockNotify { pos } => *pos,
        _ => return Vec::new(),
    };
    let block_id = world.get_block(pos);
    if !crate::block::has_gravity(block_id) {
        return Vec::new();
    }
    let below = BlockPos::new(pos.x, pos.y - 1, pos.z);
    if !crate::block::is_replaceable(world.get_block(below)) {
        return Vec::new();
    }

    // Detach: entity spawns just above the cell floor, block vanishes.
    // Both consequents are guarded; a cross-partition write racing the
    // removal is the same (tolerated, self-healing) class as the instant
    // rule's two-cell swap.
    let id = world.entities().allocate_id();
    let state = EntityState {
        kind: KIND_FALLING_BLOCK,
        pos: Vec3::new(pos.x as f64 + 0.5, pos.y as f64 + 0.01, pos.z as f64 + 0.5),
        // A REAL initial fall velocity, above REST_EPS. This is what makes
        // `is_still` a reliable rest-vs-mid-flight discriminator: a stored
        // mid-flight state must never look like rest, or a wake would
        // re-stamp it (rewinding the fall) instead of re-planning on the
        // original timeline — and the stale segment would win the guard
        // race and convert inside an occupied cell.
        vel: Vec3::new(0.0, -0.1, 0.0),
        stamp: world.now(),
        aux: block_id.0 as u64, // no despawn deadline; converts on landing
    };
    let mut events = vec![
        Event { payload: EventPayload::EntitySet { id, old: None, new: Some(state) } },
        super::helpers::block_set(pos, block_id, crate::block::AIR),
    ];
    // The vacated cell lets the block above fall (pillar collapse) and
    // fluids re-level into it.
    events.extend(super::helpers::notify_vertical(pos));
    events
}

/// Kinematics + landing conversion for falling blocks. Unlike items,
/// "at rest" is not sleep — it's the moment the entity turns back into a
/// block (or into a dropped item when the landing cell was stolen).
///
/// CONVERSION IS TWO CAUSAL STEPS, not one event pair. The landing emits
/// ONLY the guarded despawn; the block placement is a consequent of the
/// despawn's APPLICATION (the `new: None` arm below). Emitting
/// despawn+block together is a duplication bug at scale: if a wake-replan
/// endpoint beats the despawn to the entity store, the despawn dies at
/// its guard but the independently-guarded block write still lands —
/// duplicated sand plus a zombie entity replanning forever (found by
/// bench_entities at 160k, invisible at 3 sands).
pub fn falling_block_kinematics(world: &World, payload: &EventPayload) -> Vec<Event> {
    // A materialize just APPLIED (atomically: guarded despawn + block
    // placement at the first replaceable cell scanning up — see
    // scheduler::apply_event). Emit the follow-up notifies for the
    // placement RUN: the synthesized BlockSet in the write log reaches
    // replicas/clients but does not evaluate rules, so fluids/gravity/
    // entity wakes around the placed cell fire from here. We can't know
    // which cell apply chose, so we over-notify the whole contended run
    // (old cell up through the post-apply solid stretch) — idempotent
    // and short (its length IS the contention depth).
    if let EventPayload::EntityMaterialize { old, .. } = payload {
        let base = old.pos.block_pos();
        let mut events = Vec::new();
        let mut y = base.y;
        loop {
            let cell = BlockPos::new(base.x, y, base.z);
            events.extend(super::helpers::notify_neighbors(cell));
            events.extend(wakes_near(world, cell));
            if crate::block::is_replaceable(world.get_block(cell)) || y > base.y + 64 {
                break;
            }
            y += 1;
        }
        return events;
    }

    let Some((id, woken)) = kinematics_subject(payload) else {
        return Vec::new();
    };
    let Some(cur) = world.entities().get(id) else {
        return Vec::new();
    };
    if cur.kind != KIND_FALLING_BLOCK {
        return Vec::new();
    }

    if is_still(&cur) && supported(world, cur.kind, cur.pos) {
        // Landed: ONE atomic conversion event. Contention (stolen cells,
        // co-landing stacks, region-handoff dual ownership) is resolved
        // inside its apply — the entity guard is the commit point and
        // the upward scan is the climb. No bumps, no resident scans.
        return vec![Event {
            payload: EventPayload::EntityMaterialize {
                id,
                old: cur,
                block: aux_block(cur.aux),
            },
        }];
    }

    vec![plan_next(world, id, cur, woken)]
}

// ── Wake-on-block-change ─────────────────────────────────────────────────

/// Wake events for entities whose support or path the change at `pos`
/// could affect: ±1 block column, at-or-above the cell. Waking a
/// still-supported entity is a cheap no-op (it re-sleeps), and
/// `EntityWake` dedup-coalesces in the graph.
fn wakes_near(world: &World, pos: BlockPos) -> Vec<Event> {
    let mut events = Vec::new();
    for dx in -1..=1 {
        for dz in -1..=1 {
            for id in world.entities().in_column(pos.x + dx, pos.z + dz) {
                let Some(s) = world.entities().get(id) else { continue };
                // Players are externally driven — never woken.
                if s.kind == KIND_PLAYER {
                    continue;
                }
                if s.pos.y >= pos.y as f64 - 1.0 {
                    events.push(Event {
                        payload: EventPayload::EntityWake { id, at: s.pos.block_pos() },
                    });
                }
            }
        }
    }
    events
}

/// When a block changes, wake nearby entities so they re-verify support /
/// clearance. Column-granular (±1 block column). (This was chunk-granular
/// first — at 160k entities a whole-chunk scan per BlockSet dominated the
/// entire cascade.)
pub fn entity_block_wake(world: &World, payload: &EventPayload) -> Vec<Event> {
    let EventPayload::BlockSet { pos, .. } = payload else {
        return Vec::new();
    };
    wakes_near(world, *pos)
}
