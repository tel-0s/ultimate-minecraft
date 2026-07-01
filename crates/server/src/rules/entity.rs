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
/// A detached falling block (vanilla sand/gravel parity). `aux` = the
/// block id; on landing it converts back into a block (or a dropped item
/// if something stole the landing cell).
pub const KIND_FALLING_BLOCK: EntityKind = EntityKind(2);

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

/// Which entity does this payload concern (for kinematics rules)?
fn kinematics_subject(payload: &EventPayload) -> Option<(ultimate_engine::world::entity::EntityId, bool)> {
    match payload {
        // A segment endpoint just applied: keep chaining on its timeline.
        EventPayload::EntitySet { id, new: Some(_), .. } => Some((*id, false)),
        // External cause (block change, timer, interaction).
        EventPayload::EntityWake { id, .. } => Some((*id, true)),
        _ => None,
    }
}

fn is_still(s: &EntityState) -> bool {
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
fn plan_next(world: &World, id: ultimate_engine::world::entity::EntityId, cur: EntityState, woken: bool) -> Event {
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
    if is_still(&cur) && supported(world, cur.pos) {
        return Vec::new();
    }

    vec![plan_next(world, id, cur, woken)]
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
    // A gravity block just materialized in the world (usually a landed
    // conversion): despawn exactly ONE resident falling-block entity of
    // that block type — the one whose landing this write IS. This is the
    // sand-conservation keystone: the entity dies only causally AFTER its
    // block write took effect, so every failure path (write lost the cell
    // race, co-resident lost the claim) leaves a live entity that the
    // same BlockSet's wake then bumps upward. MUST be registered before
    // `entity_block_wake` so the despawn beats the bump to the guard.
    // (Known quirk: a player placing sand INTO a cell where a
    // falling-block entity is resting absorbs the entity.)
    if let EventPayload::BlockSet { pos, new, .. } = payload {
        if crate::block::has_gravity(*new) {
            let mut resident: Option<(ultimate_engine::world::entity::EntityId, EntityState)> = None;
            for other in world.entities().in_column(pos.x, pos.z) {
                let Some(o) = world.entities().get(other) else { continue };
                if o.kind == KIND_FALLING_BLOCK
                    && is_still(&o)
                    && o.pos.block_pos() == *pos
                    && aux_block(o.aux) == *new
                    && resident.map_or(true, |(rid, _)| other < rid)
                {
                    resident = Some((other, o));
                }
            }
            if let Some((rid, rstate)) = resident {
                return vec![Event {
                    payload: EventPayload::EntitySet { id: rid, old: Some(rstate), new: None },
                }];
            }
        }
        return Vec::new();
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

    if is_still(&cur) && supported(world, cur.pos) {
        let cell = cur.pos.block_pos();
        // Cell already solid (a neighbor's stack claimed it first): climb
        // one cell and re-check. Each hop is one guarded event;
        // convergence is bounded by stack height.
        if !crate::block::is_replaceable(world.get_block(cell)) {
            return vec![Event {
                payload: EventPayload::EntitySet { id, old: Some(cur), new: Some(bumped_up(world, &cur)) },
            }];
        }
        // Landed: write the block. The entity stays alive until that
        // write APPLIES (the BlockSet arm above then despawns it); if the
        // write dies at the world guard, the winner's BlockSet wakes us
        // and we bump. Sand is conserved on every path.
        let mut events = vec![super::helpers::block_set(cell, world.get_block(cell), aux_block(cur.aux))];
        events.extend(super::helpers::notify_neighbors(cell));
        return events;
    }

    vec![plan_next(world, id, cur, woken)]
}

/// One cell up from a blocked landing — a REST state atop the stolen cell
/// (which is solid, so support is definitional). Rest states re-enter the
/// landing branch on application, so climbs chain through the immediate
/// lane with NO timers: a bump is bookkeeping, not physics, and must not
/// pay the 25 ms segment quantum per hop (at 16-layer density that
/// quantization alone made stacks grow slower than vanilla).
fn bumped_up(world: &World, s: &EntityState) -> EntityState {
    let cell = s.pos.block_pos();
    EntityState {
        pos: Vec3::new(s.pos.x, (cell.y + 1) as f64 + 0.01, s.pos.z),
        vel: Vec3::ZERO,
        stamp: world.now(),
        ..*s
    }
}

// ── Wake-on-block-change ─────────────────────────────────────────────────

/// When a block changes, wake nearby entities so they re-verify support /
/// clearance. Column-granular (±1 block column): waking a still-supported
/// entity is a cheap no-op (it re-sleeps), and `EntityWake`
/// dedup-coalesces in the graph. (This was chunk-granular first — at 160k
/// entities a whole-chunk scan per BlockSet dominated the entire cascade.)
pub fn entity_block_wake(world: &World, payload: &EventPayload) -> Vec<Event> {
    let EventPayload::BlockSet { pos, .. } = payload else {
        return Vec::new();
    };
    let mut events = Vec::new();
    for dx in -1..=1 {
        for dz in -1..=1 {
            for id in world.entities().in_column(pos.x + dx, pos.z + dz) {
                let Some(s) = world.entities().get(id) else { continue };
                // At-or-above the changed cell (support or path affected).
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
