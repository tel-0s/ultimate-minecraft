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
    /// Scheduling priority (higher drains first among READY events).
    /// Priority only reorders spacelike-separated events — causal order
    /// is still enforced by parent edges — so it cannot change outcomes
    /// for confluent rules, only latency. Player-initiated events run at
    /// priority 1, background physics at 0. Children inherit the max of
    /// their parents' priorities so a player cascade stays prioritized
    /// end-to-end.
    pub priority: u8,
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
///
/// ## Pruning (opt-in)
///
/// With [`CausalGraph::with_pruning`], executed nodes are reaped as soon as
/// all of their children are also executed, bounding memory to the active
/// causal wavefront instead of the total lifetime event count. Reaping is
/// driven from two places: `mark_executed` re-checks each parent of the
/// newly-executed node, and the scheduler calls [`CausalGraph::finish`]
/// after inserting an event's consequents (so leaf events — those that
/// produced no consequents — are reaped too). A missing node is always a
/// reaped node, and reaping requires execution, so readiness checks treat
/// missing parents as executed.
///
/// Pruned graphs can't answer "what happened" by scanning nodes, so the
/// graph also keeps a [`write_log`](CausalGraph::write_log): an
/// execution-ordered list of effective world writes appended by the
/// scheduler via [`log_write`](CausalGraph::log_write).
pub struct CausalGraph {
    nodes: SlotMap<EventId, EventNode>,
    /// Ring buffer of the most recently inserted event IDs (for dashboard snapshots).
    recent_ids: VecDeque<EventId>,
    /// Incrementally-maintained ready queues: events whose parents are all
    /// executed but which have not been executed themselves. Two lanes —
    /// `drain_ready` empties the priority lane before touching the normal
    /// lane, so player-initiated cascades cut ahead of background physics
    /// (Phase 6d priority-aware draining).
    ready_high: VecDeque<EventId>,
    ready_norm: VecDeque<EventId>,
    /// Map from dedup key to the currently pending (not yet popped from
    /// `ready`) event bearing that key. Populated at insert, cleared when
    /// the event is popped by `drain_ready`.
    pending: HashMap<DedupKey, EventId>,
    /// When true, executed nodes whose children have all executed are
    /// removed from `nodes`, keeping memory bounded to the wavefront.
    prune: bool,
    /// Execution-ordered log of *effective* world writes (`BlockSet` /
    /// `LightSet`), appended by the scheduler. Survives pruning.
    write_log: Vec<EventPayload>,
    /// Lifetime counters — unaffected by pruning.
    inserted_total: u64,
    executed_total: u64,
    reaped_total: u64,
    /// Causal edges whose parent and child events affect the *same* chunk.
    /// Together with `cross_chunk_edges` this measures spatial locality of
    /// causality — the number that determines how much cross-partition
    /// message traffic a chunk-ownership scheduler (Phase 6b) would pay.
    same_chunk_edges: u64,
    /// Causal edges that cross a chunk boundary (parent and child chunks
    /// differ). Under partition ownership these become messages.
    cross_chunk_edges: u64,
    /// High-water mark of live node count. With pruning enabled this is
    /// the peak causal wavefront width; without, it ends equal to
    /// `inserted_total` minus dedup merges.
    peak_len: usize,
}

impl CausalGraph {
    pub fn new() -> Self {
        Self {
            nodes: SlotMap::with_key(),
            recent_ids: VecDeque::with_capacity(MAX_RECENT),
            ready_high: VecDeque::new(),
            ready_norm: VecDeque::new(),
            pending: HashMap::new(),
            prune: false,
            write_log: Vec::new(),
            inserted_total: 0,
            executed_total: 0,
            reaped_total: 0,
            same_chunk_edges: 0,
            cross_chunk_edges: 0,
            peak_len: 0,
        }
    }

    /// A graph that reaps executed nodes once all their children have
    /// executed. Memory stays proportional to the causal wavefront, not
    /// the lifetime event count. Use for production cascades; plain
    /// `new()` retains every node for inspection (tests, DOT export).
    pub fn with_pruning() -> Self {
        let mut g = Self::new();
        g.prune = true;
        g
    }

    /// Is `id` executed? Missing nodes count as executed: ids never leave
    /// the graph except by reaping, and only executed nodes are reaped.
    fn is_executed(&self, id: EventId) -> bool {
        self.nodes.get(id).is_none_or(|n| n.executed)
    }

    /// Insert with priority inherited from the parents (max). Roots get 0.
    pub fn insert(&mut self, event: Event, parents: Vec<EventId>) -> EventId {
        let inherited = parents
            .iter()
            .filter_map(|p| self.nodes.get(*p).map(|n| n.priority))
            .max()
            .unwrap_or(0);
        self.insert_with_priority(event, parents, inherited)
    }

    pub fn insert_with_priority(
        &mut self,
        event: Event,
        parents: Vec<EventId>,
        priority: u8,
    ) -> EventId {
        let dedup_key = event.payload.dedup_key();

        // Dedup path: if a pending event exists with this key, merge the new
        // parents into it instead of creating a new node.
        if let Some(key) = dedup_key {
            if let Some(&existing_id) = self.pending.get(&key) {
                if self.nodes.get(existing_id).is_some_and(|n| !n.executed) {
                    let child_chunk = self.nodes.get(existing_id)
                        .map(|n| n.event.chunk());
                    for &parent_id in &parents {
                        let mut added = false;
                        if let Some(existing) = self.nodes.get_mut(existing_id) {
                            if !existing.parents.contains(&parent_id) {
                                existing.parents.push(parent_id);
                                added = true;
                            }
                            // Escalate: a priority cascade merging into a
                            // pending background notify lifts it. (If it
                            // already sits in the normal lane it drains
                            // from there — a one-time latency miss, not a
                            // correctness issue.)
                            if priority > existing.priority {
                                existing.priority = priority;
                            }
                        }
                        if let Some(parent) = self.nodes.get_mut(parent_id) {
                            if !parent.children.contains(&existing_id) {
                                parent.children.push(existing_id);
                            }
                        }
                        if added {
                            self.count_edge_locality(parent_id, child_chunk);
                        }
                    }
                    return existing_id;
                }
                // Stale pending entry — fall through to normal insert.
                self.pending.remove(&key);
            }
        }

        let all_parents_done = parents.iter().all(|p| self.is_executed(*p));

        self.inserted_total += 1;
        let child_chunk = Some(event.chunk());
        let id = self.nodes.insert(EventNode {
            event,
            parents: parents.clone(),
            children: Vec::new(),
            executed: false,
            priority,
            dedup_key,
        });
        self.peak_len = self.peak_len.max(self.nodes.len());

        for &parent_id in &parents {
            if let Some(parent) = self.nodes.get_mut(parent_id) {
                parent.children.push(id);
            }
            self.count_edge_locality(parent_id, child_chunk);
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
            self.push_ready(id, priority);
        }

        id
    }

    pub fn insert_root(&mut self, event: Event) -> EventId {
        self.insert(event, Vec::new())
    }

    pub fn insert_root_with_priority(&mut self, event: Event, priority: u8) -> EventId {
        self.insert_with_priority(event, Vec::new(), priority)
    }

    #[inline]
    fn push_ready(&mut self, id: EventId, priority: u8) {
        if priority > 0 {
            self.ready_high.push_back(id);
        } else {
            self.ready_norm.push_back(id);
        }
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
            // Priority lane first: player cascades cut ahead of background
            // physics among spacelike-separated (causally unordered) events.
            let id = match self.ready_high.pop_front().or_else(|| self.ready_norm.pop_front()) {
                Some(id) => id,
                None => break,
            };
            let ready = match self.nodes.get(id) {
                Some(node) => {
                    !node.executed
                        && node.parents.iter().all(|p|
                            self.nodes.get(*p).is_none_or(|n| n.executed)
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
                        .all(|p| self.nodes.get(*p).is_none_or(|n| n.executed))
            })
            .map(|(id, _)| id)
            .collect()
    }

    pub fn mark_executed(&mut self, id: EventId) {
        let (children, parents) = match self.nodes.get_mut(id) {
            Some(node) => {
                if !node.executed {
                    node.executed = true;
                    self.executed_total += 1;
                }
                (node.children.clone(), node.parents.clone())
            }
            None => return,
        };

        for child_id in children {
            if let Some(child) = self.nodes.get(child_id) {
                if !child.executed
                    && child.parents.iter().all(|p|
                        self.nodes.get(*p).is_none_or(|n| n.executed)
                    )
                {
                    let prio = child.priority;
                    self.push_ready(child_id, prio);
                }
            }
        }

        // This node's execution may have completed a parent's reap
        // condition (all children executed). The node itself is *not*
        // reaped here — its consequents haven't been inserted yet; the
        // scheduler calls `finish` once they have.
        if self.prune {
            for parent_id in parents {
                self.try_reap(parent_id);
            }
        }
    }

    /// Declare that no further consequents will be inserted under `id`.
    /// Called by the scheduler after rule evaluation; with pruning enabled
    /// this reaps `id` immediately when it produced no (live) consequents.
    pub fn finish(&mut self, id: EventId) {
        if self.prune {
            self.try_reap(id);
        }
    }

    /// Reap `id` if it is executed and all of its children are executed.
    /// Children of a reaped node hold a dangling parent id, which readiness
    /// checks treat as executed — valid precisely because the reap
    /// condition guarantees no unexecuted child exists.
    fn try_reap(&mut self, id: EventId) {
        let reapable = match self.nodes.get(id) {
            Some(node) => {
                node.executed && node.children.iter().all(|c| self.is_executed(*c))
            }
            None => false,
        };
        if !reapable {
            return;
        }
        let node = self.nodes.remove(id).expect("checked above");
        if let Some(key) = node.dedup_key {
            if self.pending.get(&key) == Some(&id) {
                self.pending.remove(&key);
            }
        }
        self.reaped_total += 1;
    }

    /// Append an *effective* world write to the execution-ordered log.
    /// Only write payloads (`BlockSet`, `LightSet`) are retained; notify
    /// events are ignored.
    pub fn log_write(&mut self, payload: &EventPayload) {
        match payload {
            EventPayload::BlockSet { .. }
            | EventPayload::LightSet { .. }
            | EventPayload::LightBatch { .. } => {
                self.write_log.push(payload.clone());
            }
            EventPayload::BlockNotify { .. } | EventPayload::LightNotify { .. } => {}
        }
    }

    /// Execution-ordered log of effective world writes. Unlike node scans,
    /// this survives pruning and preserves causal execution order.
    pub fn write_log(&self) -> &[EventPayload] {
        &self.write_log
    }

    /// Drain the write log, returning everything logged since the last
    /// drain. A long-lived graph (the shared physics graph) must consume
    /// its log per processing batch or it grows without bound.
    pub fn take_write_log(&mut self) -> Vec<EventPayload> {
        std::mem::take(&mut self.write_log)
    }

    /// Lifetime number of inserted events (dedup merges don't count;
    /// reaping doesn't subtract).
    pub fn inserted_total(&self) -> u64 {
        self.inserted_total
    }

    /// Lifetime number of executed events (reaping doesn't subtract).
    pub fn executed_total(&self) -> u64 {
        self.executed_total
    }

    /// Lifetime number of reaped (pruned) nodes.
    pub fn reaped_total(&self) -> u64 {
        self.reaped_total
    }

    /// Attribute one causal edge to the same-chunk or cross-chunk counter.
    /// Edges to already-reaped parents are not counted (their chunk is
    /// unknowable) — under the scheduler this can't happen, because a
    /// parent survives until `finish` runs after its consequents insert.
    fn count_edge_locality(&mut self, parent_id: EventId, child_chunk: Option<crate::world::position::ChunkPos>) {
        let (Some(parent), Some(child_chunk)) = (self.nodes.get(parent_id), child_chunk) else {
            return;
        };
        if parent.event.chunk() == child_chunk {
            self.same_chunk_edges += 1;
        } else {
            self.cross_chunk_edges += 1;
        }
    }

    /// `(same_chunk, cross_chunk)` causal-edge counts. The cross-chunk
    /// fraction is the share of causality that would become inter-partition
    /// messages under chunk-ownership scheduling (Phase 6b).
    pub fn edge_locality(&self) -> (u64, u64) {
        (self.same_chunk_edges, self.cross_chunk_edges)
    }

    /// High-water mark of live node count. With pruning this is the peak
    /// causal wavefront width — the working-set size of a cascade.
    pub fn peak_len(&self) -> usize {
        self.peak_len
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
                EventPayload::LightBatch { changes } => (
                    format!("LightBatch ({} cells)", changes.len()),
                    "#cce5ff",
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
