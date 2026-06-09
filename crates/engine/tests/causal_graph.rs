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

// ---------------------------------------------------------------------------
// Pruning tests: executed nodes are reaped once all children are executed.
// ---------------------------------------------------------------------------

fn notify_at(x: i64) -> Event {
    Event {
        payload: EventPayload::BlockNotify { pos: BlockPos::new(x, 0, 0) },
    }
}

#[test]
fn pruning_reaps_chain_behind_the_wavefront() {
    // Simulates the scheduler's per-event lifecycle on a chain A -> B -> C:
    // drain, execute, insert consequents, finish. Memory must stay bounded
    // to the wavefront — each node is reaped once its child executes.
    let mut g = CausalGraph::with_pruning();

    let a = g.insert_root(notify_at(0));
    assert_eq!(g.drain_ready(10), vec![a]);
    g.mark_executed(a);
    let b = g.insert(notify_at(1), vec![a]);
    g.finish(a);
    assert!(g.get(a).is_some(), "a must survive while child b is pending");

    assert_eq!(g.drain_ready(10), vec![b]);
    g.mark_executed(b);
    assert!(g.get(a).is_none(), "a reaped once its only child executed");
    let c = g.insert(notify_at(2), vec![b]);
    g.finish(b);
    assert!(g.get(b).is_some(), "b must survive while child c is pending");

    assert_eq!(g.drain_ready(10), vec![c]);
    g.mark_executed(c);
    assert!(g.get(b).is_none(), "b reaped once c executed");
    g.finish(c);
    assert!(g.get(c).is_none(), "leaf c reaped at finish (no consequents)");

    assert_eq!(g.len(), 0);
    assert_eq!(g.inserted_total(), 3);
    assert_eq!(g.executed_total(), 3);
    assert_eq!(g.reaped_total(), 3);
}

#[test]
fn pruning_waits_for_all_children() {
    // A parent with two children is reaped only when BOTH have executed.
    let mut g = CausalGraph::with_pruning();
    let root = g.insert_root(notify_at(0));
    g.drain_ready(10);
    g.mark_executed(root);
    let c1 = g.insert(notify_at(1), vec![root]);
    let c2 = g.insert(notify_at(2), vec![root]);
    g.finish(root);

    g.drain_ready(10);
    g.mark_executed(c1);
    g.finish(c1);
    assert!(g.get(root).is_some(), "root survives while c2 is pending");

    g.mark_executed(c2);
    g.finish(c2);
    assert!(g.get(root).is_none());
    assert_eq!(g.len(), 0);
}

#[test]
fn unpruned_graph_retains_everything() {
    // Default graphs keep all nodes for inspection — finish() is a no-op.
    let mut g = CausalGraph::new();
    let a = g.insert_root(notify_at(0));
    g.drain_ready(10);
    g.mark_executed(a);
    g.finish(a);
    assert!(g.get(a).is_some());
    assert_eq!(g.len(), 1);
    assert_eq!(g.reaped_total(), 0);
}

#[test]
fn pruned_scheduler_run_leaves_empty_graph() {
    // A real scheduler run on a pruning graph: at quiescence the graph is
    // empty but the lifetime counters and write log tell the whole story.
    let world = World::new();
    let mut chunk = Chunk::new();
    for x in 0..SECTION_SIZE as u8 {
        for z in 0..SECTION_SIZE as u8 {
            chunk.set_block(LocalBlockPos { x, y: 0, z }, BlockId::new(1));
        }
    }
    world.insert_chunk(ChunkPos::new(0, 0), chunk);

    let mut graph = CausalGraph::with_pruning();
    let rules = RuleSet::new(); // no rules — events execute without consequents
    let scheduler = Scheduler::new();

    graph.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: BlockPos::new(3, 5, 3),
            old: BlockId::AIR,
            new: BlockId::new(7),
        },
    });
    graph.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: BlockPos::new(4, 5, 4),
            old: BlockId::AIR,
            new: BlockId::new(8),
        },
    });

    let total = scheduler.run_until_quiet(&world, &mut graph, &rules, 100);
    assert_eq!(total, 2);
    assert_eq!(graph.len(), 0, "all nodes reaped at quiescence");
    assert_eq!(graph.executed_total(), 2);
    assert_eq!(graph.reaped_total(), 2);
    assert_eq!(graph.write_log().len(), 2, "write log survives pruning");
    assert_eq!(world.get_block(BlockPos::new(3, 5, 3)), BlockId::new(7));
    assert_eq!(world.get_block(BlockPos::new(4, 5, 4)), BlockId::new(8));
}

// ---------------------------------------------------------------------------
// Priority lanes: high-priority events drain before background events
// among ready (spacelike-separated) work; children inherit priority.
// ---------------------------------------------------------------------------

#[test]
fn priority_events_drain_before_background() {
    let mut g = CausalGraph::new();
    // Background roots inserted FIRST...
    let bg1 = g.insert_root(notify_at(0));
    let bg2 = g.insert_root(notify_at(1));
    // ...player action inserted after, at priority 1.
    let player = g.insert_root_with_priority(notify_at(2), 1);

    let batch = g.drain_ready(10);
    assert_eq!(batch[0], player, "priority event must drain first");
    assert_eq!(&batch[1..], &[bg1, bg2], "background events keep FIFO order");
}

#[test]
fn children_inherit_priority_through_cascade() {
    let mut g = CausalGraph::new();
    let bg_root = g.insert_root(notify_at(0));
    let hi_root = g.insert_root_with_priority(notify_at(1), 1);
    g.drain_ready(10);
    g.mark_executed(bg_root);
    g.mark_executed(hi_root);

    // Consequents of each cascade, inserted background-first.
    let bg_child = g.insert(notify_at(10), vec![bg_root]);
    let hi_child = g.insert(notify_at(11), vec![hi_root]);
    assert_eq!(g.get(hi_child).unwrap().priority, 1, "child inherits parent priority");
    assert_eq!(g.get(bg_child).unwrap().priority, 0);

    let batch = g.drain_ready(10);
    assert_eq!(batch[0], hi_child, "inherited priority keeps the cascade in the fast lane");
}

#[test]
fn priority_cannot_overtake_causal_order() {
    // A priority child of an unexecuted background parent must NOT drain
    // before its parent: priority reorders only spacelike events.
    let mut g = CausalGraph::new();
    let parent = g.insert_root(notify_at(0));
    let child = g.insert_with_priority(notify_at(1), vec![parent], 1);

    let batch = g.drain_ready(10);
    assert_eq!(batch, vec![parent], "child stays blocked behind its parent");
    g.mark_executed(parent);
    let batch = g.drain_ready(10);
    assert_eq!(batch, vec![child]);
}

// ---------------------------------------------------------------------------
// Edge-locality + peak-wavefront instrumentation.
// ---------------------------------------------------------------------------

#[test]
fn edge_locality_counts_same_and_cross_chunk() {
    let mut g = CausalGraph::new();
    // Root in chunk (0,0). Chunks are 16x16 in x/z.
    let root = g.insert_root(notify_at(0));
    // Child at x=5 → same chunk (0,0).
    let same = g.insert(notify_at(5), vec![root]);
    // Child at x=20 → chunk (1,0): cross-chunk edge.
    let _cross = g.insert(notify_at(20), vec![same]);
    // Root edges don't count (no parent).
    assert_eq!(g.edge_locality(), (1, 1));

    // Dedup merge adds an edge too: another notify at x=20 (same key,
    // merges) with a parent in chunk (0,0) → one more cross-chunk edge.
    let _merged = g.insert(notify_at(20), vec![root]);
    assert_eq!(g.edge_locality(), (1, 2));
}

#[test]
fn peak_len_tracks_wavefront_under_pruning() {
    let mut g = CausalGraph::with_pruning();
    // Chain of 5 executed step-by-step: wavefront never exceeds 2
    // (current node + its one consequent).
    let mut prev = g.insert_root(notify_at(0));
    g.drain_ready(10);
    for i in 1..5i64 {
        g.mark_executed(prev);
        let next = g.insert(notify_at(i), vec![prev]);
        g.finish(prev);
        g.drain_ready(10);
        prev = next;
    }
    g.mark_executed(prev);
    g.finish(prev);
    assert_eq!(g.len(), 0);
    assert_eq!(g.inserted_total(), 5);
    assert!(g.peak_len() <= 2, "wavefront should stay tiny, peak was {}", g.peak_len());
}

// ---------------------------------------------------------------------------
// Write-log + stale-precondition guard tests.
// ---------------------------------------------------------------------------

#[test]
fn stale_block_set_is_skipped_and_unlogged() {
    // Two spacelike BlockSets target the same cell. The first applies; the
    // second observed `old = AIR` which is no longer true, so it must be
    // skipped (no write, no log entry, no consequents).
    let world = World::new();
    world.insert_chunk(ChunkPos::new(0, 0), Chunk::new());

    let mut graph = CausalGraph::new();
    let rules = RuleSet::new();
    let scheduler = Scheduler::new();

    let pos = BlockPos::new(5, 5, 5);
    graph.insert_root(Event {
        payload: EventPayload::BlockSet { pos, old: BlockId::AIR, new: BlockId::new(1) },
    });
    graph.insert_root(Event {
        payload: EventPayload::BlockSet { pos, old: BlockId::AIR, new: BlockId::new(2) },
    });

    scheduler.run_until_quiet(&world, &mut graph, &rules, 100);

    // First write wins; the stale second write neither applies nor logs.
    assert_eq!(world.get_block(pos), BlockId::new(1));
    assert_eq!(graph.write_log().len(), 1);
    match &graph.write_log()[0] {
        EventPayload::BlockSet { new, .. } => assert_eq!(*new, BlockId::new(1)),
        other => panic!("unexpected log entry {other:?}"),
    }
}

#[test]
fn write_log_preserves_execution_order() {
    let world = World::new();
    world.insert_chunk(ChunkPos::new(0, 0), Chunk::new());

    let mut graph = CausalGraph::new();
    let rules = RuleSet::new();
    let scheduler = Scheduler::new();

    // Same cell written twice, causally ordered: AIR -> 1 -> 2.
    let pos = BlockPos::new(1, 1, 1);
    let first = graph.insert_root(Event {
        payload: EventPayload::BlockSet { pos, old: BlockId::AIR, new: BlockId::new(1) },
    });
    graph.insert(
        Event {
            payload: EventPayload::BlockSet { pos, old: BlockId::new(1), new: BlockId::new(2) },
        },
        vec![first],
    );

    scheduler.run_until_quiet(&world, &mut graph, &rules, 100);

    assert_eq!(world.get_block(pos), BlockId::new(2));
    let news: Vec<BlockId> = graph
        .write_log()
        .iter()
        .map(|p| match p {
            EventPayload::BlockSet { new, .. } => *new,
            other => panic!("unexpected log entry {other:?}"),
        })
        .collect();
    assert_eq!(news, vec![BlockId::new(1), BlockId::new(2)],
        "log order must match execution order, final value last");
}

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
