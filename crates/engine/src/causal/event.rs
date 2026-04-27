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
        }
    }

    /// The chunk this event primarily affects (used for parallel grouping).
    pub fn chunk(&self) -> ChunkPos {
        match &self.payload {
            EventPayload::BlockSet { pos, .. }
            | EventPayload::BlockNotify { pos }
            | EventPayload::LightSet { pos, .. }
            | EventPayload::LightNotify { pos } => pos.chunk(),
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
            _ => None,
        }
    }
}
