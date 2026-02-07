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
