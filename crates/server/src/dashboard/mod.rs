//! Live web dashboard — near-real-time causal graph & profiling stats.
//!
//! Design contract with the physics hot path:
//!   • Metrics: atomic fetch_add (~10 ns, zero-alloc, never blocks).
//!   • Graph snapshot: published via `tokio::sync::watch` (non-blocking send,
//!     overwrites previous value — if the dashboard is slow it just sees the
//!     latest snapshot, never stalling the engine).
//!   • The web server runs on its own tokio tasks and never touches the
//!     CausalGraph or World directly.

pub mod metrics;
pub mod server;

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::watch;
use ultimate_engine::causal::event::{EventId, EventPayload};
use ultimate_engine::causal::graph::CausalGraph;
use ultimate_engine::world::World;

pub use metrics::Metrics;

// ── Dashboard state (shared between server, connections, and web) ────────

/// Central state shared via `Arc<DashboardState>`.
pub struct DashboardState {
    pub metrics: Metrics,
    pub world: Arc<World>,
    graph_tx: watch::Sender<GraphSnapshot>,
}

impl DashboardState {
    pub fn new(world: Arc<World>) -> Self {
        let (graph_tx, _) = watch::channel(GraphSnapshot::empty());
        Self {
            metrics: Metrics::new(),
            world,
            graph_tx,
        }
    }

    /// Publish a new graph snapshot. Non-blocking (overwrites previous).
    pub fn publish_graph(&self, snapshot: GraphSnapshot) {
        let _ = self.graph_tx.send(snapshot);
    }

    /// Create a new receiver for graph snapshots (one per WebSocket client).
    pub fn subscribe_graph(&self) -> watch::Receiver<GraphSnapshot> {
        self.graph_tx.subscribe()
    }
}

// ── Graph snapshot types ─────────────────────────────────────────────────

#[derive(Clone, Serialize, Default)]
pub struct GraphSnapshot {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<[u32; 2]>, // [parent_index, child_index] into `nodes`
}

impl GraphSnapshot {
    pub fn empty() -> Self {
        Self::default()
    }
}

#[derive(Clone, Serialize)]
pub struct GraphNode {
    pub id: u32,
    pub kind: String,  // "block_set" | "block_notify"
    pub label: String,
    pub pos: [i64; 3],
    pub executed: bool,
    pub depth: u32,
}

// ── Snapshot builder ─────────────────────────────────────────────────────

/// Build a `GraphSnapshot` from the graph's recent events.
/// Called on the connection handler's tokio task after each cascade
/// (~1-10 μs for 200 nodes — negligible vs. the cascade itself).
pub fn snapshot_graph(graph: &CausalGraph) -> GraphSnapshot {
    let recent: Vec<EventId> = graph.recent_node_ids().collect();

    // Map EventId → contiguous index for the snapshot.
    let mut id_map: HashMap<EventId, u32> = HashMap::with_capacity(recent.len());
    for (idx, &eid) in recent.iter().enumerate() {
        id_map.insert(eid, idx as u32);
    }

    let mut depth_cache: HashMap<EventId, u32> = HashMap::new();
    let mut nodes = Vec::with_capacity(recent.len());
    let mut edges = Vec::new();

    for (idx, &eid) in recent.iter().enumerate() {
        let node = match graph.get(eid) {
            Some(n) => n,
            None => continue,
        };

        let depth = compute_depth(graph, eid, &mut depth_cache);

        let (kind, label, pos) = match &node.event.payload {
            EventPayload::BlockSet { pos, old, new } => {
                let old_name = crate::block::name(*old);
                let new_name = crate::block::name(*new);
                (
                    "block_set".to_string(),
                    format!("Set ({},{},{}) {} → {}", pos.x, pos.y, pos.z, old_name, new_name),
                    [pos.x, pos.y, pos.z],
                )
            }
            EventPayload::BlockNotify { pos } => (
                "block_notify".to_string(),
                format!("Notify ({},{},{})", pos.x, pos.y, pos.z),
                [pos.x, pos.y, pos.z],
            ),
        };

        nodes.push(GraphNode {
            id: idx as u32,
            kind,
            label,
            pos,
            executed: node.executed,
            depth,
        });

        for &parent_id in &node.parents {
            if let Some(&parent_idx) = id_map.get(&parent_id) {
                edges.push([parent_idx, idx as u32]);
            }
        }
    }

    GraphSnapshot { nodes, edges }
}

/// Recursively compute the causal depth of a node (memoized).
fn compute_depth(
    graph: &CausalGraph,
    id: EventId,
    cache: &mut HashMap<EventId, u32>,
) -> u32 {
    if let Some(&d) = cache.get(&id) {
        return d;
    }
    let depth = match graph.get(id) {
        Some(node) if !node.parents.is_empty() => node
            .parents
            .iter()
            .map(|&p| compute_depth(graph, p, cache) + 1)
            .max()
            .unwrap_or(0),
        _ => 0,
    };
    cache.insert(id, depth);
    depth
}
