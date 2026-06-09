use crate::world::block::BlockId;
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
}

impl Event {
    pub fn positions(&self) -> Vec<BlockPos> {
        match &self.payload {
            EventPayload::BlockSet { pos, .. }
            | EventPayload::BlockNotify { pos }
            | EventPayload::LightSet { pos, .. }
            | EventPayload::LightNotify { pos } => vec![*pos],
            EventPayload::LightBatch { changes } => changes.iter().map(|c| c.pos).collect(),
        }
    }

    /// The chunk this event primarily affects (used for parallel grouping).
    pub fn chunk(&self) -> ChunkPos {
        match &self.payload {
            EventPayload::BlockSet { pos, .. }
            | EventPayload::BlockNotify { pos }
            | EventPayload::LightSet { pos, .. }
            | EventPayload::LightNotify { pos } => pos.chunk(),
            // A light flood spans chunks; its origin cell anchors it.
            EventPayload::LightBatch { changes } => changes
                .first()
                .map(|c| c.pos.chunk())
                .unwrap_or(ChunkPos::new(0, 0)),
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
            EventPayload::BlockSet { .. }
            | EventPayload::LightSet { .. }
            | EventPayload::LightBatch { .. } => None,
        }
    }
}
