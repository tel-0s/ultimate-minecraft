use crate::world::block::BlockId;
use crate::world::position::{BlockPos, ChunkPos};
use slotmap::new_key_type;

new_key_type! {
    /// Unique handle for a node in the causal graph.
    pub struct EventId;
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
}

impl Event {
    pub fn positions(&self) -> Vec<BlockPos> {
        match &self.payload {
            EventPayload::BlockSet { pos, .. } => vec![*pos],
            EventPayload::BlockNotify { pos } => vec![*pos],
        }
    }

    /// The chunk this event primarily affects (used for parallel grouping).
    pub fn chunk(&self) -> ChunkPos {
        match &self.payload {
            EventPayload::BlockSet { pos, .. } => pos.chunk(),
            EventPayload::BlockNotify { pos } => pos.chunk(),
        }
    }
}
