//! World-change event bus for cross-player and simulation-to-player distribution.
//!
//! Every action that modifies the world (player block break/place, ambient simulation)
//! publishes a [`WorldChangeBatch`] to a shared `tokio::sync::broadcast` channel.
//! Each connection subscribes and forwards changes to its client -- except changes
//! it originated itself.

use std::sync::Arc;

use ultimate_engine::causal::event::EventPayload;
use ultimate_engine::causal::graph::CausalGraph;
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::position::BlockPos;

/// Recommended capacity for the broadcast channel.
/// 256 batches in flight should handle bursty activity without lagging.
pub const BUS_CAPACITY: usize = 256;

/// Identifies where a batch of world changes originated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeSource {
    /// A specific player connection (identified by connection ID).
    Player(u64),
    /// An ambient simulation layer.
    Simulation(&'static str),
}

/// A batch of block changes from a single cascade.
///
/// Uses `Arc<[...]>` so cloning per broadcast subscriber is just a refcount bump.
#[derive(Clone, Debug)]
pub struct WorldChangeBatch {
    pub source: ChangeSource,
    pub changes: Arc<[(BlockPos, BlockId)]>,
}

/// Extract all executed `BlockSet` events from a causal graph into a list of
/// `(position, new_block)` pairs suitable for broadcasting.
pub fn collect_block_changes(graph: &CausalGraph) -> Vec<(BlockPos, BlockId)> {
    let mut changes = Vec::new();
    for id in graph.all_ids() {
        if let Some(node) = graph.get(id) {
            if node.executed {
                if let EventPayload::BlockSet { pos, new, .. } = &node.event.payload {
                    changes.push((*pos, *new));
                }
            }
        }
    }
    changes
}
