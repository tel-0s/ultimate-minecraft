use super::event::{Event, EventId, EventPayload};
use slotmap::SlotMap;

/// A node in the causal DAG.
#[derive(Debug)]
pub struct EventNode {
    pub event: Event,
    pub parents: Vec<EventId>,
    pub children: Vec<EventId>,
    pub executed: bool,
}

/// The causal graph: an append-only DAG of events.
///
/// Invariant: if A is a parent of B, then A's world-write must be visible
/// before B executes. Events with no ancestor/descendant relationship are
/// **spacelike-separated** and may execute in any order (or in parallel).
pub struct CausalGraph {
    nodes: SlotMap<EventId, EventNode>,
}

impl CausalGraph {
    pub fn new() -> Self {
        Self {
            nodes: SlotMap::with_key(),
        }
    }

    pub fn insert(&mut self, event: Event, parents: Vec<EventId>) -> EventId {
        let id = self.nodes.insert(EventNode {
            event,
            parents: parents.clone(),
            children: Vec::new(),
            executed: false,
        });

        for &parent_id in &parents {
            if let Some(parent) = self.nodes.get_mut(parent_id) {
                parent.children.push(id);
            }
        }

        id
    }

    pub fn insert_root(&mut self, event: Event) -> EventId {
        self.insert(event, Vec::new())
    }

    /// The "frontier": all events whose parents have all been executed,
    /// but which have not been executed themselves.
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
        if let Some(node) = self.nodes.get_mut(id) {
            node.executed = true;
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
