use crate::world::block::BlockId;
use crate::world::entity::{EntityId, EntityState, Nanos};
use crate::world::position::{BlockPos, ChunkPos};
use slotmap::new_key_type;

new_key_type! {
    /// Unique handle for a node in the causal graph.
    pub struct EventId;
}

/// Sky light (from the sun/moon) vs block light (from torches, glowstone, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LightType {
    Sky,
    Block,
}

/// A single, atomic change to the world -- the fundamental unit of causality.
#[derive(Debug, Clone)]
pub struct Event {
    pub payload: EventPayload,
}

/// One cell of a [`EventPayload::LightBatch`].
#[derive(Debug, Clone, Copy)]
pub struct LightCell {
    pub pos: BlockPos,
    pub light_type: LightType,
    pub old: u8,
    pub new: u8,
}

/// What happened.
#[derive(Debug, Clone)]
pub enum EventPayload {
    /// A block was set (by a player action, gravity, fluid flow, etc.).
    BlockSet {
        pos: BlockPos,
        old: BlockId,
        new: BlockId,
    },

    /// A block's neighbors should be re-evaluated (after a nearby change).
    BlockNotify { pos: BlockPos },

    /// A light value was set at a position.
    LightSet {
        pos: BlockPos,
        light_type: LightType,
        old: u8,
        new: u8,
    },

    /// Every cell changed by ONE synchronous light flood (BFS inside the
    /// light rule). Reporting-only: the rule already wrote light storage;
    /// this event exists so the write log / clients learn what changed.
    /// One graph node instead of thousands of per-cell `LightSet`s — a
    /// torch placement was paying ~1,800 events of pure bookkeeping.
    /// `Arc` keeps `Event` clones cheap.
    LightBatch { changes: std::sync::Arc<[LightCell]> },

    /// A position's light should be recalculated (a neighbor's light changed).
    LightNotify { pos: BlockPos },

    /// An entity state transition (Phase 5) — the entity analog of
    /// `BlockSet`. `old: None` = spawn, `new: None` = despawn. Stale-guarded
    /// (applies only if the store still matches `old`) and write-logged, so
    /// replicas/gateways/clients learn entity changes through the same
    /// machinery as block changes.
    EntitySet {
        id: EntityId,
        old: Option<EntityState>,
        new: Option<EntityState>,
    },

    /// "Re-evaluate this entity now" — the entity analog of `BlockNotify`:
    /// idempotent, dedup-coalesced, safe to deliver spuriously or late.
    /// `at` is a routing hint (the entity's block position at emission).
    EntityWake { id: EntityId, at: BlockPos },

    /// ATOMIC entity→block conversion: despawn the entity (guarded on
    /// `old`) and place `block` at the first replaceable cell scanning UP
    /// from the entity's cell — all inside one `apply_event`. This is the
    /// only cross-store transaction in the engine, and it exists because
    /// no composition of separately-guarded despawn + block-write events
    /// conserves matter under contention or region-handoff dual
    /// ownership: the entity entry lock arbitrates the transition
    /// process-wide, and the block write happens only after winning it.
    /// The upward scan resolves co-landing contention (stacking) inside
    /// the same atom. Apply synthesizes concrete `EntitySet`/`BlockSet`
    /// write-log entries so replicas and clients see exact outcomes.
    /// The scan is vertical, so placement stays in the payload's chunk —
    /// ownership routing is unaffected.
    EntityMaterialize {
        id: EntityId,
        old: EntityState,
        block: BlockId,
    },

    /// A timed event (Phase 5): `inner` must not execute before engine time
    /// `at`. The causal graph itself stays pure-causal — the physics
    /// worker's router unwraps `After` into its timer heap and inserts
    /// `inner` as a root when due (the delay IS the happens-before edge,
    /// riding wall-clock instead of a channel). Wrapping the payload rather
    /// than adding a field to `Event` keeps every existing construction
    /// site and the cluster codec's frame format unchanged.
    After { at: Nanos, inner: Box<EventPayload> },
}

impl Event {
    pub fn positions(&self) -> Vec<BlockPos> {
        self.payload.positions()
    }

    /// The chunk this event primarily affects (used for parallel grouping).
    pub fn chunk(&self) -> ChunkPos {
        self.payload.chunk()
    }
}

impl EventPayload {
    pub fn positions(&self) -> Vec<BlockPos> {
        match self {
            EventPayload::BlockSet { pos, .. }
            | EventPayload::BlockNotify { pos }
            | EventPayload::LightSet { pos, .. }
            | EventPayload::LightNotify { pos } => vec![*pos],
            EventPayload::LightBatch { changes } => changes.iter().map(|c| c.pos).collect(),
            EventPayload::EntitySet { old, new, .. } => new
                .as_ref()
                .or(old.as_ref())
                .map(|s| vec![s.pos.block_pos()])
                .unwrap_or_default(),
            EventPayload::EntityWake { at, .. } => vec![*at],
            EventPayload::EntityMaterialize { old, .. } => vec![old.pos.block_pos()],
            EventPayload::After { inner, .. } => inner.positions(),
        }
    }

    /// The chunk this payload primarily affects (routing / parallel grouping).
    pub fn chunk(&self) -> ChunkPos {
        match self {
            EventPayload::BlockSet { pos, .. }
            | EventPayload::BlockNotify { pos }
            | EventPayload::LightSet { pos, .. }
            | EventPayload::LightNotify { pos } => pos.chunk(),
            // A light flood spans chunks; its origin cell anchors it.
            EventPayload::LightBatch { changes } => changes
                .first()
                .map(|c| c.pos.chunk())
                .unwrap_or(ChunkPos::new(0, 0)),
            // An EntitySet is anchored where the entity ENDS UP (its new
            // owner executes it; the store guard tolerates the transition).
            EventPayload::EntitySet { old, new, .. } => new
                .as_ref()
                .or(old.as_ref())
                .map(|s| s.chunk())
                .unwrap_or(ChunkPos::new(0, 0)),
            EventPayload::EntityWake { at, .. } => at.chunk(),
            EventPayload::EntityMaterialize { old, .. } => old.chunk(),
            EventPayload::After { inner, .. } => inner.chunk(),
        }
    }
}

/// Identity for an *idempotent* event that can be coalesced with other
/// pending events of the same identity. Only returned for events whose
/// semantics are "re-evaluate this position" — never for writes, whose
/// identity depends on their value fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DedupKey {
    BlockNotify(BlockPos),
    LightNotify(BlockPos),
    EntityWake(EntityId),
}

impl EventPayload {
    /// Returns a dedup key if this event can be coalesced with pending events
    /// of the same identity (idempotent re-evaluate-this-position events).
    /// Returns `None` for events whose identity depends on their payload
    /// values (e.g., `BlockSet`, `LightSet`).
    pub fn dedup_key(&self) -> Option<DedupKey> {
        match self {
            EventPayload::BlockNotify { pos } => Some(DedupKey::BlockNotify(*pos)),
            EventPayload::LightNotify { pos } => Some(DedupKey::LightNotify(*pos)),
            EventPayload::EntityWake { id, .. } => Some(DedupKey::EntityWake(*id)),
            EventPayload::BlockSet { .. }
            | EventPayload::LightSet { .. }
            | EventPayload::LightBatch { .. }
            | EventPayload::EntitySet { .. }
            | EventPayload::EntityMaterialize { .. }
            // Timed events never coalesce (their identity includes `at`;
            // two despawn timers for different entities share nothing).
            | EventPayload::After { .. } => None,
        }
    }
}
