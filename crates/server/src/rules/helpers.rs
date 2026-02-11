//! Event construction helpers to reduce boilerplate in rule implementations.

use ultimate_engine::causal::event::{Event, EventPayload};
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::position::BlockPos;

// ── Position helpers ─────────────────────────────────────────────────────

/// The four horizontal neighbor positions (±X, ±Z).
pub fn horizontal_neighbors(pos: BlockPos) -> [BlockPos; 4] {
    [
        BlockPos::new(pos.x + 1, pos.y, pos.z),
        BlockPos::new(pos.x - 1, pos.y, pos.z),
        BlockPos::new(pos.x, pos.y, pos.z + 1),
        BlockPos::new(pos.x, pos.y, pos.z - 1),
    ]
}

// ── Event constructors ───────────────────────────────────────────────────

/// Create a `BlockSet` event.
pub fn block_set(pos: BlockPos, old: BlockId, new: BlockId) -> Event {
    Event {
        payload: EventPayload::BlockSet { pos, old, new },
    }
}

/// Create a `BlockNotify` event.
pub fn notify(pos: BlockPos) -> Event {
    Event {
        payload: EventPayload::BlockNotify { pos },
    }
}

// ── Batch notify helpers ─────────────────────────────────────────────────

/// Notify all 6 cardinal neighbors.
pub fn notify_neighbors(pos: BlockPos) -> Vec<Event> {
    pos.neighbors().into_iter().map(notify).collect()
}

/// Notify the 4 horizontal neighbors (±X, ±Z).
pub fn notify_horizontal(pos: BlockPos) -> Vec<Event> {
    horizontal_neighbors(pos).into_iter().map(notify).collect()
}

/// Notify the 2 vertical neighbors (above and below).
pub fn notify_vertical(pos: BlockPos) -> Vec<Event> {
    vec![
        notify(BlockPos::new(pos.x, pos.y + 1, pos.z)),
        notify(BlockPos::new(pos.x, pos.y - 1, pos.z)),
    ]
}
