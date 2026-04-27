use super::event::{DedupKey, Event, EventId, EventPayload};
use slotmap::SlotMap;
use std::collections::{HashMap, VecDeque};

/// Maximum number of recent event IDs retained for dashboard snapshots.
const MAX_RECENT: usize = 200;

/// A node in the causal DAG.
#[derive(Debug)]
pub struct EventNode {
    pub event: Event,
    pub parents: Vec<EventId>,
    pub children: Vec<EventId>,
    pub executed: bool,
    dedup_key: Option<DedupKey>,
}

/// The causal graph: an append-only DAG of events.
///
/// Invariant: if A is a parent of B, then A's world-write must be visible
/// before B executes. Events with no ancestor/descendant relationship are
/// **spacelike-separated** and may execute in any order (or in parallel).
///
/// ## Dedup of idempotent events
///
/// `insert` transparently coalesces *idempotent* events (those whose
/// `EventPayload::dedup_key()` returns `Some`) against any pending event
/// sharing the same key. When coalescing, the new parents are merged into
/// the existing event's parent set; no new node is created. This collapses
/// the many-to-one fan-in common to neighbor-notification rules (a single
/// position getting `BlockNotify`'d from each of its six neighbors becomes
/// one notify event with six parents, not six duplicate events).
///
/// Non-idempotent events (`BlockSet`, `LightSet`) whose identity depends
/// on their value fields are never coalesced.
pub struct CausalGraph {
    nodes: SlotMap<EventId, EventNode>,
    /// Ring buffer of the most recently inserted event IDs (for dashboard snapshots).
    recent_ids: VecDeque<EventId>,
    /// Incrementally-maintained ready queue: events whose parents are all
    /// executed but which have not been executed themselves. Avoids full-scan
    /// frontier computation on every scheduler step.
    ready: VecDeque<EventId>,
    /// Map from dedup key to the currently pending (not yet popped from
    /// `ready`) event bearing that key. Populated at insert, cleared when
    /// the event is popped by `drain_ready`.
    pending: HashMap<DedupKey, EventId>,
}

impl CausalGraph {
    pub fn new() -> Self {
        Self {
            nodes: SlotMap::with_key(),
            recent_ids: VecDeque::with_capacity(MAX_RECENT),
            ready: VecDeque::new(),
            pending: HashMap::new(),
        }
    }

    pub fn insert(&mut self, event: Event, parents: Vec<EventId>) -> EventId {
        let dedup_key = event.payload.dedup_key();

        // Dedup path: if a pending event exists with this key, merge the new
        // parents into it instead of creating a new node.
        if let Some(key) = dedup_key {
            if let Some(&existing_id) = self.pending.get(&key) {
                if self.nodes.get(existing_id).is_some_and(|n| !n.executed) {
                    for &parent_id in &parents {
                        if let Some(existing) = self.nodes.get_mut(existing_id) {
                            if !existing.parents.contains(&parent_id) {
                                existing.parents.push(parent_id);
                            }
                        }
                        if let Some(parent) = self.nodes.get_mut(parent_id) {
                            if !parent.children.contains(&existing_id) {
                                parent.children.push(existing_id);
                            }
                        }
                    }
                    return existing_id;
                }
                // Stale pending entry — fall through to normal insert.
                self.pending.remove(&key);
            }
        }

        let all_parents_done = parents.iter().all(|p|
            self.nodes.get(*p).is_some_and(|n| n.executed)
        );

        let id = self.nodes.insert(EventNode {
            event,
            parents: parents.clone(),
            children: Vec::new(),
            executed: false,
            dedup_key,
        });

        for &parent_id in &parents {
            if let Some(parent) = self.nodes.get_mut(parent_id) {
                parent.children.push(id);
            }
        }

        // Track for dashboard snapshots.
        self.recent_ids.push_back(id);
        if self.recent_ids.len() > MAX_RECENT {
            self.recent_ids.pop_front();
        }

        if let Some(key) = dedup_key {
            self.pending.insert(key, id);
        }

        if all_parents_done {
            self.ready.push_back(id);
        }

        id
    }

    pub fn insert_root(&mut self, event: Event) -> EventId {
        self.insert(event, Vec::new())
    }

    /// Drain up to `limit` ready events from the incremental queue.
    ///
    /// Re-checks `parents.all(executed)` at pop time because dedup merges
    /// can add unfinished parents to an already-ready event, regressing
    /// readiness. Skipped events are dropped from the queue; when their
    /// remaining parents eventually execute, `mark_executed` will re-enqueue
    /// them.
    pub fn drain_ready(&mut self, limit: usize) -> Vec<EventId> {
        let mut batch = Vec::new();
        while batch.len() < limit {
            let id = match self.ready.pop_front() {
                Some(id) => id,
                None => break,
            };
            let ready = match self.nodes.get(id) {
                Some(node) => {
                    !node.executed
                        && node.parents.iter().all(|p|
                            self.nodes.get(*p).is_some_and(|n| n.executed)
                        )
                }
                None => false,
            };
            if !ready {
                continue;
            }
            // Clear from pending: once an event is about to execute, new
            // inserts with the same key must create a fresh event (not merge
            // into this one, which is mid-flight).
            if let Some(node) = self.nodes.get(id) {
                if let Some(key) = node.dedup_key {
                    if self.pending.get(&key) == Some(&id) {
                        self.pending.remove(&key);
                    }
                }
            }
            batch.push(id);
        }
        batch
    }

    /// The "frontier": all events whose parents have all been executed,
    /// but which have not been executed themselves.  Full scan — kept for
    /// tests and debugging; the scheduler uses `drain_ready` instead.
    pub fn frontier(&self) -> Vec<EventId> {
        self.nodes
            .iter()
            .filter(|(_, node)| {
                !node.executed
                    && node
                        .parents
                        .iter()
                        .all(|p| self.nodes.get(*p).is_some_and(|n| n.executed))
            })
            .map(|(id, _)| id)
            .collect()
    }

    pub fn mark_executed(&mut self, id: EventId) {
        let children = self.nodes.get(id)
            .map(|n| n.children.clone())
            .unwrap_or_default();

        if let Some(node) = self.nodes.get_mut(id) {
            node.executed = true;
        }

        for child_id in children {
            if let Some(child) = self.nodes.get(child_id) {
                if !child.executed
                    && child.parents.iter().all(|p|
                        self.nodes.get(*p).is_some_and(|n| n.executed)
                    )
                {
                    self.ready.push_back(child_id);
                }
            }
        }
    }

    pub fn get(&self, id: EventId) -> Option<&EventNode> {
        self.nodes.get(id)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn executed_count(&self) -> usize {
        self.nodes.values().filter(|n| n.executed).count()
    }

    pub fn all_ids(&self) -> Vec<EventId> {
        self.nodes.keys().collect()
    }

    /// Iterator over the most recently inserted event IDs (for dashboard snapshots).
    pub fn recent_node_ids(&self) -> impl Iterator<Item = EventId> + '_ {
        self.recent_ids.iter().copied()
    }

    /// Export the graph in Graphviz DOT format.
    pub fn to_dot(&self) -> String {
        let mut out = String::from(
            "digraph causal {\n  rankdir=BT;\n  node [shape=box, fontname=\"monospace\", fontsize=10];\n",
        );
        let entries: Vec<_> = self.nodes.iter().collect();
        for (id, node) in &entries {
            let (label, color) = match &node.event.payload {
                EventPayload::BlockSet { pos, new, .. } => (
                    format!("Set ({},{},{})\\n-> {:?}", pos.x, pos.y, pos.z, new),
                    "#d4edda",
                ),
                EventPayload::BlockNotify { pos } => (
                    format!("Notify ({},{},{})", pos.x, pos.y, pos.z),
                    "#fff3cd",
                ),
                EventPayload::LightSet { pos, light_type, new, .. } => (
                    format!("Light{:?} ({},{},{})\\n-> {}", light_type, pos.x, pos.y, pos.z, new),
                    "#cce5ff",
                ),
                EventPayload::LightNotify { pos } => (
                    format!("LightNotify ({},{},{})", pos.x, pos.y, pos.z),
                    "#e2e3e5",
                ),
            };
            let fill = if node.executed { color } else { "#f8f9fa" };
            out.push_str(&format!(
                "  \"{id:?}\" [label=\"{label}\", style=filled, fillcolor=\"{fill}\"];\n"
            ));
            for parent in &node.parents {
                out.push_str(&format!("  \"{parent:?}\" -> \"{id:?}\";\n"));
            }
        }
        out.push_str("}\n");
        out
    }
}

impl Default for CausalGraph {
    fn default() -> Self {
        Self::new()
    }
}
