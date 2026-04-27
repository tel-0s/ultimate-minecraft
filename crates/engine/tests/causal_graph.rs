//! Pure causal-graph tests that exercise the DAG mechanics without any
//! game-specific block semantics. All block values are opaque `BlockId`s.

use ultimate_engine::causal::event::{Event, EventPayload};
use ultimate_engine::causal::graph::CausalGraph;
use ultimate_engine::causal::scheduler::Scheduler;
use ultimate_engine::rules::RuleSet;
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::chunk::{Chunk, SECTION_SIZE};
use ultimate_engine::world::position::{BlockPos, ChunkPos, LocalBlockPos};
use ultimate_engine::world::World;

// ---------------------------------------------------------------------------
// CausalGraph unit tests
// ---------------------------------------------------------------------------

#[test]
fn graph_insert_and_retrieve() {
    let mut g = CausalGraph::new();
    let id = g.insert_root(Event {
        payload: EventPayload::BlockNotify {
            pos: BlockPos::new(0, 0, 0),
        },
    });
    assert_eq!(g.len(), 1);
    assert!(g.get(id).is_some());
    assert!(!g.get(id).unwrap().executed);
}

#[test]
fn graph_frontier_roots_only() {
    let mut g = CausalGraph::new();
    let a = g.insert_root(Event {
        payload: EventPayload::BlockNotify { pos: BlockPos::new(0, 0, 0) },
    });
    let b = g.insert_root(Event {
        payload: EventPayload::BlockNotify { pos: BlockPos::new(1, 0, 0) },
    });

    let frontier = g.frontier();
    assert_eq!(frontier.len(), 2);
    assert!(frontier.contains(&a));
    assert!(frontier.contains(&b));
}

#[test]
fn graph_frontier_respects_dependencies() {
    let mut g = CausalGraph::new();
    let a = g.insert_root(Event {
        payload: EventPayload::BlockNotify { pos: BlockPos::new(0, 0, 0) },
    });
    // b depends on a
    let b = g.insert(
        Event {
            payload: EventPayload::BlockNotify { pos: BlockPos::new(1, 0, 0) },
        },
        vec![a],
    );

    // Only a is on the frontier initially.
    let frontier = g.frontier();
    assert_eq!(frontier.len(), 1);
    assert!(frontier.contains(&a));

    // Execute a, now b should appear.
    g.mark_executed(a);
    let frontier = g.frontier();
    assert_eq!(frontier.len(), 1);
    assert!(frontier.contains(&b));
}

#[test]
fn graph_diamond_dependency() {
    // A diamond: root -> {left, right} -> join
    let mut g = CausalGraph::new();
    let root = g.insert_root(Event {
        payload: EventPayload::BlockNotify { pos: BlockPos::new(0, 0, 0) },
    });
    let left = g.insert(
        Event { payload: EventPayload::BlockNotify { pos: BlockPos::new(1, 0, 0) } },
        vec![root],
    );
    let right = g.insert(
        Event { payload: EventPayload::BlockNotify { pos: BlockPos::new(2, 0, 0) } },
        vec![root],
    );
    let join = g.insert(
        Event { payload: EventPayload::BlockNotify { pos: BlockPos::new(3, 0, 0) } },
        vec![left, right],
    );

    // Initially only root.
    assert_eq!(g.frontier(), vec![root]);

    // Execute root -> left and right appear.
    g.mark_executed(root);
    let f = g.frontier();
    assert_eq!(f.len(), 2);
    assert!(f.contains(&left));
    assert!(f.contains(&right));

    // Execute left only -> join still blocked by right.
    g.mark_executed(left);
    let f = g.frontier();
    assert_eq!(f.len(), 1);
    assert!(f.contains(&right));

    // Execute right -> join appears.
    g.mark_executed(right);
    let f = g.frontier();
    assert_eq!(f.len(), 1);
    assert!(f.contains(&join));
}

// ---------------------------------------------------------------------------
// DOT export test
// ---------------------------------------------------------------------------

#[test]
fn dot_export_is_valid() {
    let mut g = CausalGraph::new();
    let a = g.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: BlockPos::new(0, 5, 0),
            old: BlockId::AIR,
            new: BlockId::new(1),
        },
    });
    let _b = g.insert(
        Event {
            payload: EventPayload::BlockNotify { pos: BlockPos::new(0, 4, 0) },
        },
        vec![a],
    );

    let dot = g.to_dot();
    assert!(dot.starts_with("digraph causal {"));
    assert!(dot.ends_with("}\n"));
    assert!(dot.contains("->"));  // At least one edge.
}

// ---------------------------------------------------------------------------
// Quiescence test (empty RuleSet -- no rules means no consequents)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Dedup tests: idempotent events (BlockNotify/LightNotify) coalesce.
// ---------------------------------------------------------------------------

#[test]
fn dedup_notifies_at_same_position_coalesce() {
    let mut g = CausalGraph::new();
    let a = g.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: BlockPos::new(0, 0, 0),
            old: BlockId::AIR,
            new: BlockId::new(1),
        },
    });
    let b = g.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: BlockPos::new(1, 0, 0),
            old: BlockId::AIR,
            new: BlockId::new(1),
        },
    });

    // Two BlockNotify at the same pos — should coalesce into one node.
    let n1 = g.insert(
        Event { payload: EventPayload::BlockNotify { pos: BlockPos::new(5, 5, 5) } },
        vec![a],
    );
    let n2 = g.insert(
        Event { payload: EventPayload::BlockNotify { pos: BlockPos::new(5, 5, 5) } },
        vec![b],
    );

    assert_eq!(n1, n2, "same-position BlockNotify should coalesce");
    // Total nodes: a, b, n (merged). Three.
    assert_eq!(g.len(), 3);
    // Merged node has both a and b as parents.
    let merged = g.get(n1).unwrap();
    assert_eq!(merged.parents.len(), 2);
    assert!(merged.parents.contains(&a));
    assert!(merged.parents.contains(&b));
}

#[test]
fn dedup_different_positions_do_not_coalesce() {
    let mut g = CausalGraph::new();
    let a = g.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: BlockPos::new(0, 0, 0),
            old: BlockId::AIR,
            new: BlockId::new(1),
        },
    });
    let n1 = g.insert(
        Event { payload: EventPayload::BlockNotify { pos: BlockPos::new(1, 0, 0) } },
        vec![a],
    );
    let n2 = g.insert(
        Event { payload: EventPayload::BlockNotify { pos: BlockPos::new(2, 0, 0) } },
        vec![a],
    );
    assert_ne!(n1, n2);
    assert_eq!(g.len(), 3);
}

#[test]
fn dedup_block_set_never_coalesces() {
    // BlockSet identity depends on its value fields — must NOT dedup.
    let mut g = CausalGraph::new();
    let s1 = g.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: BlockPos::new(0, 0, 0),
            old: BlockId::AIR,
            new: BlockId::new(1),
        },
    });
    let s2 = g.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: BlockPos::new(0, 0, 0),
            old: BlockId::AIR,
            new: BlockId::new(1),
        },
    });
    assert_ne!(s1, s2);
    assert_eq!(g.len(), 2);
}

#[test]
fn dedup_waits_for_merged_parents() {
    // Merging a new parent into an already-ready event should delay firing
    // until *all* parents (including the newly-added one) have executed.
    let mut g = CausalGraph::new();
    let early = g.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: BlockPos::new(0, 0, 0),
            old: BlockId::AIR,
            new: BlockId::new(1),
        },
    });
    // First notify depends on `early` (which is a root, ready).
    let n = g.insert(
        Event { payload: EventPayload::BlockNotify { pos: BlockPos::new(5, 5, 5) } },
        vec![early],
    );
    // Execute `early` so the notify becomes ready.
    g.mark_executed(early);

    // Now insert a new parent that is NOT yet executed, and re-insert the
    // same notify — dedup should merge the new parent in.
    let late = g.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: BlockPos::new(1, 0, 0),
            old: BlockId::AIR,
            new: BlockId::new(1),
        },
    });
    let n2 = g.insert(
        Event { payload: EventPayload::BlockNotify { pos: BlockPos::new(5, 5, 5) } },
        vec![late],
    );
    assert_eq!(n, n2);

    // Drain should NOT yield the notify: `late` isn't executed yet.
    let batch = g.drain_ready(10);
    assert!(!batch.contains(&n), "notify must wait for merged parent `late`");
    assert!(batch.contains(&late), "only `late` should be ready");

    // Execute `late` — now the notify should appear.
    g.mark_executed(late);
    let batch = g.drain_ready(10);
    assert_eq!(batch, vec![n]);
}

#[test]
fn dedup_after_pop_creates_fresh_event() {
    // Once a pending notify is popped for execution, subsequent inserts
    // with the same key must create a NEW event, not merge into the
    // already-in-flight one.
    let mut g = CausalGraph::new();
    let a = g.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: BlockPos::new(0, 0, 0),
            old: BlockId::AIR,
            new: BlockId::new(1),
        },
    });
    g.mark_executed(a);

    let n1 = g.insert(
        Event { payload: EventPayload::BlockNotify { pos: BlockPos::new(5, 5, 5) } },
        vec![a],
    );
    let batch = g.drain_ready(10);
    assert!(batch.contains(&n1));

    // Post-pop: a new notify at the same pos gets a fresh id.
    let n2 = g.insert_root(Event {
        payload: EventPayload::BlockNotify { pos: BlockPos::new(5, 5, 5) },
    });
    assert_ne!(n1, n2);
}

// ---------------------------------------------------------------------------
// Quiescence test (empty RuleSet -- no rules means no consequents)
// ---------------------------------------------------------------------------

#[test]
fn empty_graph_is_quiescent() {
    let world = World::new();
    // Insert a single chunk so the world isn't completely empty.
    let mut chunk = Chunk::new();
    for x in 0..SECTION_SIZE as u8 {
        for z in 0..SECTION_SIZE as u8 {
            chunk.set_block(LocalBlockPos { x, y: 0, z }, BlockId::new(1));
        }
    }
    world.insert_chunk(ChunkPos::new(0, 0), chunk);

    let mut graph = CausalGraph::new();
    let rules = RuleSet::new(); // empty -- no rules
    let scheduler = Scheduler::new();

    let total = scheduler.run_until_quiet(&world, &mut graph, &rules, 100);
    assert_eq!(total, 0);
}
