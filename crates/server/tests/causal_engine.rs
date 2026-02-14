//! Phase 1 & 2 tests: causal engine correctness, invariance, and parallel execution
//! with Minecraft-specific block types and rules.

use ultimate_engine::causal::event::{Event, EventId, EventPayload};
use ultimate_engine::causal::graph::CausalGraph;
use ultimate_engine::causal::scheduler::Scheduler;
use ultimate_engine::rules::RuleSet;
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::chunk::{Chunk, SECTION_SIZE};
use ultimate_engine::world::position::{BlockPos, ChunkPos, LocalBlockPos};
use ultimate_engine::world::World;

use ultimate_server::block;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a flat world: bedrock y=0, stone y=1..=3, dirt y=4.
fn flat_world(chunk_radius: i32) -> World {
    let world = World::new();
    for cx in -chunk_radius..chunk_radius {
        for cz in -chunk_radius..chunk_radius {
            let mut chunk = Chunk::new();
            for x in 0..SECTION_SIZE as u8 {
                for z in 0..SECTION_SIZE as u8 {
                    chunk.set_block(LocalBlockPos { x, y: 0, z }, block::BEDROCK);
                    for y in 1..=3i64 {
                        chunk.set_block(LocalBlockPos { x, y, z }, block::STONE);
                    }
                    chunk.set_block(LocalBlockPos { x, y: 4, z }, block::DIRT);
                }
            }
            world.insert_chunk(ChunkPos::new(cx, cz), chunk);
        }
    }
    world
}

/// Read a vertical column of block IDs from the world.
fn column(world: &World, x: i64, z: i64, y_range: std::ops::RangeInclusive<i64>) -> Vec<BlockId> {
    y_range.map(|y| world.get_block(BlockPos::new(x, y, z))).collect()
}

/// Execute the causal graph to quiescence with a custom frontier ordering.
/// `order_fn` receives the frontier and returns it reordered.
fn run_with_order<F>(
    world: &World,
    graph: &mut CausalGraph,
    rules: &RuleSet,
    order_fn: F,
    max_events: usize,
) -> usize
where
    F: Fn(Vec<EventId>) -> Vec<EventId>,
{
    let mut total = 0;
    for _ in 0..max_events {
        let frontier = order_fn(graph.frontier());
        if frontier.is_empty() {
            break;
        }
        for id in frontier {
            let event = match graph.get(id) {
                Some(node) => node.event.clone(),
                None => continue,
            };
            // Apply
            match &event.payload {
                EventPayload::BlockSet { pos, new, .. } => {
                    world.set_block(*pos, *new);
                }
                EventPayload::BlockNotify { .. } => {}
            }
            graph.mark_executed(id);
            total += 1;

            let consequents = rules.evaluate(world, &event.payload);
            for new_event in consequents {
                graph.insert(new_event, vec![id]);
            }
        }
    }
    total
}

// ---------------------------------------------------------------------------
// Scheduler + rules integration tests
// ---------------------------------------------------------------------------

#[test]
fn sand_falls_to_surface() {
    let world = flat_world(2);
    let mut graph = CausalGraph::new();
    let rules = ultimate_server::rules::standard();
    let scheduler = Scheduler::new();

    // Place sand at y=10 (5 blocks of air above dirt at y=4).
    graph.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: BlockPos::new(8, 10, 8),
            old: block::AIR,
            new: block::SAND,
        },
    });

    let total = scheduler.run_until_quiet(&world, &mut graph, &rules, 100);

    // Sand should land at y=5 (on top of dirt at y=4).
    assert_eq!(world.get_block(BlockPos::new(8, 5, 8)), block::SAND);
    // Original position should be air.
    assert_eq!(world.get_block(BlockPos::new(8, 10, 8)), block::AIR);
    // Intermediate positions should be air.
    for y in 6..=9 {
        assert_eq!(world.get_block(BlockPos::new(8, y, 8)), block::AIR);
    }
    assert!(total > 0);
}

#[test]
fn sand_stacks_on_sand() {
    let world = flat_world(2);
    let mut graph = CausalGraph::new();
    let rules = ultimate_server::rules::standard();
    let scheduler = Scheduler::new();

    // Place first sand, let it settle.
    graph.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: BlockPos::new(8, 10, 8),
            old: block::AIR,
            new: block::SAND,
        },
    });
    scheduler.run_until_quiet(&world, &mut graph, &rules, 100);
    assert_eq!(world.get_block(BlockPos::new(8, 5, 8)), block::SAND);

    // Place second sand above.
    graph.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: BlockPos::new(8, 10, 8),
            old: block::AIR,
            new: block::SAND,
        },
    });
    scheduler.run_until_quiet(&world, &mut graph, &rules, 100);

    // Second sand should land on top of the first.
    assert_eq!(world.get_block(BlockPos::new(8, 6, 8)), block::SAND);
    assert_eq!(world.get_block(BlockPos::new(8, 5, 8)), block::SAND);
}

#[test]
fn sand_on_bedrock_stays() {
    // Sand directly above bedrock (y=1, since y=0 is bedrock and y=1 is air).
    let world = World::new();
    let mut chunk = Chunk::new();
    for x in 0..SECTION_SIZE as u8 {
        for z in 0..SECTION_SIZE as u8 {
            chunk.set_block(LocalBlockPos { x, y: 0, z }, block::BEDROCK);
        }
    }
    world.insert_chunk(ChunkPos::new(0, 0), chunk);

    let mut graph = CausalGraph::new();
    let rules = ultimate_server::rules::standard();
    let scheduler = Scheduler::new();

    graph.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: BlockPos::new(4, 3, 4),
            old: block::AIR,
            new: block::SAND,
        },
    });

    scheduler.run_until_quiet(&world, &mut graph, &rules, 100);

    // Sand falls to y=1, resting on bedrock at y=0.
    assert_eq!(world.get_block(BlockPos::new(4, 1, 4)), block::SAND);
    assert_eq!(world.get_block(BlockPos::new(4, 3, 4)), block::AIR);
}

#[test]
fn water_spreads_horizontally_on_surface() {
    let world = flat_world(2);
    let mut graph = CausalGraph::new();
    let rules = ultimate_server::rules::standard();
    let scheduler = Scheduler::new();

    // Place water on the surface (y=5, on top of dirt at y=4).
    graph.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: BlockPos::new(8, 5, 8),
            old: block::AIR,
            new: block::WATER,
        },
    });

    // Run a few steps (water spreads outward each step).
    scheduler.run_until_quiet(&world, &mut graph, &rules, 5);

    // The origin should be water.
    assert_eq!(world.get_block(BlockPos::new(8, 5, 8)), block::WATER);

    // At least one horizontal neighbor should also be water (flowing, level 1).
    let neighbors_water = [
        world.get_block(BlockPos::new(9, 5, 8)),
        world.get_block(BlockPos::new(7, 5, 8)),
        world.get_block(BlockPos::new(8, 5, 9)),
        world.get_block(BlockPos::new(8, 5, 7)),
    ];
    assert!(
        neighbors_water.iter().any(|&t| block::is_fluid(t)),
        "water should spread to at least one neighbor"
    );
}

#[test]
fn water_falls_before_spreading() {
    // Water placed above air should fall down, not spread horizontally.
    let world = World::new();
    let mut chunk = Chunk::new();
    // Solid floor at y=0 only.
    for x in 0..SECTION_SIZE as u8 {
        for z in 0..SECTION_SIZE as u8 {
            chunk.set_block(LocalBlockPos { x, y: 0, z }, block::STONE);
        }
    }
    world.insert_chunk(ChunkPos::new(0, 0), chunk);

    let mut graph = CausalGraph::new();
    let rules = ultimate_server::rules::standard();
    let scheduler = Scheduler::new();

    // Place water at y=5 with air below.
    graph.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: BlockPos::new(4, 5, 4),
            old: block::AIR,
            new: block::WATER,
        },
    });

    // Step 1: root event places water at y=5. Fluid rule queues fall to y=4.
    scheduler.step(&world, &mut graph, &rules);
    assert_eq!(world.get_block(BlockPos::new(4, 5, 4)), block::WATER);

    // Step 2: fall event places flowing water (level 1) at y=4.
    scheduler.step(&world, &mut graph, &rules);
    assert!(
        block::is_fluid(world.get_block(BlockPos::new(4, 4, 4))),
        "fallen water should be a fluid"
    );

    // Horizontal neighbors at y=5 should still be air -- the fluid rule
    // returns early when below is air (fall, don't spread).
    assert_eq!(world.get_block(BlockPos::new(5, 5, 4)), block::AIR);
    assert_eq!(world.get_block(BlockPos::new(3, 5, 4)), block::AIR);
}

#[test]
fn no_events_on_inert_block() {
    let world = flat_world(1);
    let mut graph = CausalGraph::new();
    let rules = ultimate_server::rules::standard();
    let scheduler = Scheduler::new();

    // Place a stone block (not gravity-affected, not fluid).
    graph.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: BlockPos::new(4, 10, 4),
            old: block::AIR,
            new: block::STONE,
        },
    });

    let total = scheduler.run_until_quiet(&world, &mut graph, &rules, 100);

    // Only the initial event should execute; no consequents.
    assert_eq!(total, 1);
    assert_eq!(world.get_block(BlockPos::new(4, 10, 4)), block::STONE);
}

// ---------------------------------------------------------------------------
// Causal invariance tests
// ---------------------------------------------------------------------------
//
// The core property: if events A and B are on the same frontier (spacelike-
// separated), processing them in order A,B or B,A must yield an identical
// world state. We test this by running the same scenario with different
// frontier orderings and comparing the resulting block columns.

#[test]
fn invariance_two_independent_sand_columns() {
    let rules = ultimate_server::rules::standard();

    // Two sand blocks in different chunks -- completely independent causal chains.
    let setup = |graph: &mut CausalGraph| {
        graph.insert_root(Event {
            payload: EventPayload::BlockSet {
                pos: BlockPos::new(4, 10, 4),  // chunk (0, 0)
                old: block::AIR,
                new: block::SAND,
            },
        });
        graph.insert_root(Event {
            payload: EventPayload::BlockSet {
                pos: BlockPos::new(20, 10, 20), // chunk (1, 1)
                old: block::AIR,
                new: block::SAND,
            },
        });
    };

    // Run with natural frontier order.
    let world_a = flat_world(4);
    let mut graph_a = CausalGraph::new();
    setup(&mut graph_a);
    run_with_order(&world_a, &mut graph_a, &rules, |f| f, 1000);

    // Run with reversed frontier order.
    let world_b = flat_world(4);
    let mut graph_b = CausalGraph::new();
    setup(&mut graph_b);
    run_with_order(
        &world_b,
        &mut graph_b,
        &rules,
        |mut f| { f.reverse(); f },
        1000,
    );

    // Both worlds must be identical at the relevant columns.
    assert_eq!(column(&world_a, 4, 4, 0..=12), column(&world_b, 4, 4, 0..=12));
    assert_eq!(column(&world_a, 20, 20, 0..=12), column(&world_b, 20, 20, 0..=12));

    // And both should have sand at y=5.
    assert_eq!(world_a.get_block(BlockPos::new(4, 5, 4)), block::SAND);
    assert_eq!(world_a.get_block(BlockPos::new(20, 5, 20)), block::SAND);
}

#[test]
fn invariance_sand_and_water_independent() {
    let rules = ultimate_server::rules::standard();

    // Build a world with a walled 3x3 pit in a distant chunk so water
    // reaches quiescence (can't spread past the walls).
    let build_world = || {
        let world = flat_world(4);
        // Build stone walls around (40,5,40) at y=5, leaving the center open.
        // Wall ring at y=5:
        for dx in -2i64..=2 {
            for dz in -2i64..=2 {
                if dx.abs() == 2 || dz.abs() == 2 {
                    world.set_block(
                        BlockPos::new(40 + dx, 5, 40 + dz),
                        block::STONE,
                    );
                }
            }
        }
        world
    };

    let setup = |graph: &mut CausalGraph| {
        // Sand in chunk (0, 0).
        graph.insert_root(Event {
            payload: EventPayload::BlockSet {
                pos: BlockPos::new(4, 10, 4),
                old: block::AIR,
                new: block::SAND,
            },
        });
        // Water in the walled pit (chunk (2, 2)).
        graph.insert_root(Event {
            payload: EventPayload::BlockSet {
                pos: BlockPos::new(40, 5, 40),
                old: block::AIR,
                new: block::WATER,
            },
        });
    };

    let world_a = build_world();
    let mut graph_a = CausalGraph::new();
    setup(&mut graph_a);
    run_with_order(&world_a, &mut graph_a, &rules, |f| f, 1000);

    let world_b = build_world();
    let mut graph_b = CausalGraph::new();
    setup(&mut graph_b);
    run_with_order(
        &world_b,
        &mut graph_b,
        &rules,
        |mut f| { f.reverse(); f },
        1000,
    );

    // Sand column must match.
    assert_eq!(column(&world_a, 4, 4, 0..=12), column(&world_b, 4, 4, 0..=12));
    assert_eq!(world_a.get_block(BlockPos::new(4, 5, 4)), block::SAND);

    // Water region must match.
    for dx in -2i64..=2 {
        for dz in -2i64..=2 {
            let x = 40 + dx;
            let z = 40 + dz;
            assert_eq!(
                column(&world_a, x, z, 4..=6),
                column(&world_b, x, z, 4..=6),
                "mismatch at ({x}, {z})"
            );
        }
    }
}

#[test]
fn invariance_many_sand_columns_shuffled() {
    let rules = ultimate_server::rules::standard();

    // 8 sand blocks scattered across different chunks.
    let positions: Vec<BlockPos> = vec![
        BlockPos::new(4, 12, 4),
        BlockPos::new(20, 12, 4),
        BlockPos::new(36, 12, 4),
        BlockPos::new(52, 12, 4),
        BlockPos::new(4, 12, 20),
        BlockPos::new(20, 12, 20),
        BlockPos::new(36, 12, 20),
        BlockPos::new(52, 12, 20),
    ];

    let setup = |graph: &mut CausalGraph| {
        for &pos in &positions {
            graph.insert_root(Event {
                payload: EventPayload::BlockSet {
                    pos,
                    old: block::AIR,
                    new: block::SAND,
                },
            });
        }
    };

    // Natural order.
    let world_a = flat_world(5);
    let mut graph_a = CausalGraph::new();
    setup(&mut graph_a);
    run_with_order(&world_a, &mut graph_a, &rules, |f| f, 5000);

    // Reversed order.
    let world_b = flat_world(5);
    let mut graph_b = CausalGraph::new();
    setup(&mut graph_b);
    run_with_order(
        &world_b,
        &mut graph_b,
        &rules,
        |mut f| { f.reverse(); f },
        5000,
    );

    // Interleaved: even indices first, then odd.
    let world_c = flat_world(5);
    let mut graph_c = CausalGraph::new();
    setup(&mut graph_c);
    run_with_order(
        &world_c,
        &mut graph_c,
        &rules,
        |f| {
            let mut reordered = Vec::with_capacity(f.len());
            for (i, id) in f.iter().enumerate() {
                if i % 2 == 0 { reordered.push(*id); }
            }
            for (i, id) in f.iter().enumerate() {
                if i % 2 == 1 { reordered.push(*id); }
            }
            reordered
        },
        5000,
    );

    // NOTE: Event counts may differ across orderings because BlockNotify
    // events can see stale world state depending on processing order within
    // a frontier wave. This is harmless (duplicate events are idempotent)
    // and will be eliminated in Phase 2 with proper region isolation.
    // What MUST be invariant is the final world state.

    // All positions should have sand at y=5.
    for &pos in &positions {
        let landed = BlockPos::new(pos.x, 5, pos.z);
        assert_eq!(world_a.get_block(landed), block::SAND);
        assert_eq!(world_b.get_block(landed), block::SAND);
        assert_eq!(world_c.get_block(landed), block::SAND);

        // Full column comparison between all orderings.
        assert_eq!(
            column(&world_a, pos.x, pos.z, 0..=14),
            column(&world_b, pos.x, pos.z, 0..=14),
            "column mismatch (natural vs reversed) at ({}, {})", pos.x, pos.z,
        );
        assert_eq!(
            column(&world_a, pos.x, pos.z, 0..=14),
            column(&world_c, pos.x, pos.z, 0..=14),
            "column mismatch (natural vs interleaved) at ({}, {})", pos.x, pos.z,
        );
    }
}

// ---------------------------------------------------------------------------
// Execution count test
// ---------------------------------------------------------------------------

#[test]
fn graph_tracks_execution_count() {
    let world = flat_world(2);
    let mut graph = CausalGraph::new();
    let rules = ultimate_server::rules::standard();
    let scheduler = Scheduler::new();

    graph.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: BlockPos::new(8, 10, 8),
            old: block::AIR,
            new: block::SAND,
        },
    });

    scheduler.run_until_quiet(&world, &mut graph, &rules, 100);

    // Every event in the graph should be executed.
    assert_eq!(graph.executed_count(), graph.len());
    assert!(graph.frontier().is_empty());
}

// ---------------------------------------------------------------------------
// Phase 2: Parallel execution tests
// ---------------------------------------------------------------------------
//
// These verify that the parallel scheduler produces identical world state
// to the sequential scheduler.

#[test]
fn parallel_sand_falls_identically() {
    let world_seq = flat_world(2);
    let world_par = flat_world(2);
    let rules = ultimate_server::rules::standard();
    let scheduler = Scheduler::new();

    let make_graph = || {
        let mut g = CausalGraph::new();
        g.insert_root(Event {
            payload: EventPayload::BlockSet {
                pos: BlockPos::new(8, 10, 8),
                old: block::AIR,
                new: block::SAND,
            },
        });
        g
    };

    let mut graph_seq = make_graph();
    let mut graph_par = make_graph();

    scheduler.run_until_quiet(&world_seq, &mut graph_seq, &rules, 100);
    scheduler.run_until_quiet_parallel(&world_par, &mut graph_par, &rules, 100);

    assert_eq!(
        column(&world_seq, 8, 8, 0..=12),
        column(&world_par, 8, 8, 0..=12),
    );
    assert_eq!(world_par.get_block(BlockPos::new(8, 5, 8)), block::SAND);
}

#[test]
fn parallel_many_independent_columns() {
    let rules = ultimate_server::rules::standard();
    let scheduler = Scheduler::new();

    let positions: Vec<BlockPos> = vec![
        BlockPos::new(4, 12, 4),
        BlockPos::new(20, 12, 4),
        BlockPos::new(36, 12, 4),
        BlockPos::new(52, 12, 4),
        BlockPos::new(4, 12, 20),
        BlockPos::new(20, 12, 20),
        BlockPos::new(36, 12, 20),
        BlockPos::new(52, 12, 20),
    ];

    let setup = |graph: &mut CausalGraph| {
        for &pos in &positions {
            graph.insert_root(Event {
                payload: EventPayload::BlockSet {
                    pos,
                    old: block::AIR,
                    new: block::SAND,
                },
            });
        }
    };

    let world_seq = flat_world(5);
    let mut graph_seq = CausalGraph::new();
    setup(&mut graph_seq);
    scheduler.run_until_quiet(&world_seq, &mut graph_seq, &rules, 5000);

    let world_par = flat_world(5);
    let mut graph_par = CausalGraph::new();
    setup(&mut graph_par);
    scheduler.run_until_quiet_parallel(&world_par, &mut graph_par, &rules, 5000);

    for &pos in &positions {
        assert_eq!(
            column(&world_seq, pos.x, pos.z, 0..=14),
            column(&world_par, pos.x, pos.z, 0..=14),
            "seq vs par mismatch at ({}, {})", pos.x, pos.z,
        );
    }
}

#[test]
fn parallel_water_and_sand_independent() {
    let rules = ultimate_server::rules::standard();
    let scheduler = Scheduler::new();

    let build_world = || {
        let world = flat_world(4);
        // Build stone walls around (40,5,40) to contain water.
        for dx in -2i64..=2 {
            for dz in -2i64..=2 {
                if dx.abs() == 2 || dz.abs() == 2 {
                    world.set_block(
                        BlockPos::new(40 + dx, 5, 40 + dz),
                        block::STONE,
                    );
                }
            }
        }
        world
    };

    let setup = |graph: &mut CausalGraph| {
        graph.insert_root(Event {
            payload: EventPayload::BlockSet {
                pos: BlockPos::new(4, 10, 4),
                old: block::AIR,
                new: block::SAND,
            },
        });
        graph.insert_root(Event {
            payload: EventPayload::BlockSet {
                pos: BlockPos::new(40, 5, 40),
                old: block::AIR,
                new: block::WATER,
            },
        });
    };

    let world_seq = build_world();
    let mut graph_seq = CausalGraph::new();
    setup(&mut graph_seq);
    scheduler.run_until_quiet(&world_seq, &mut graph_seq, &rules, 1000);

    let world_par = build_world();
    let mut graph_par = CausalGraph::new();
    setup(&mut graph_par);
    scheduler.run_until_quiet_parallel(&world_par, &mut graph_par, &rules, 1000);

    // Sand column must match.
    assert_eq!(
        column(&world_seq, 4, 4, 0..=12),
        column(&world_par, 4, 4, 0..=12),
    );
    // Water region must match.
    for dx in -2i64..=2 {
        for dz in -2i64..=2 {
            let x = 40 + dx;
            let z = 40 + dz;
            assert_eq!(
                column(&world_seq, x, z, 4..=6),
                column(&world_par, x, z, 4..=6),
                "seq vs par mismatch at ({x}, {z})"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Water drainage tests
// ---------------------------------------------------------------------------

#[test]
fn flowing_water_drains_when_source_removed() {
    // Place a water source, let it spread, remove the source, run to
    // quiescence.  All flowing water should drain to air.
    let world = flat_world(2);
    let mut graph = CausalGraph::new();
    let rules = ultimate_server::rules::standard();
    let scheduler = Scheduler::new();

    let source_pos = BlockPos::new(8, 5, 8);

    // 1. Place water source and let it spread fully.
    graph.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: source_pos,
            old: block::AIR,
            new: block::WATER, // level 0 = source
        },
    });
    scheduler.run_until_quiet(&world, &mut graph, &rules, 500);

    // Sanity: source block should still be water.
    assert_eq!(world.get_block(source_pos), block::WATER);
    // At least some neighbors should be flowing water.
    assert!(
        block::is_fluid(world.get_block(BlockPos::new(9, 5, 8))),
        "water should have spread before we remove the source"
    );

    // 2. Remove the source block (simulate player breaking it).
    let mut graph2 = CausalGraph::new();
    let root = graph2.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: source_pos,
            old: block::WATER,
            new: block::AIR,
        },
    });
    // Notify the 6 neighbours (same as the server does on block break).
    for neighbor in source_pos.neighbors() {
        graph2.insert(
            Event {
                payload: EventPayload::BlockNotify { pos: neighbor },
            },
            vec![root],
        );
    }

    scheduler.run_until_quiet(&world, &mut graph2, &rules, 2000);

    // 3. All blocks in the spread area should be air (except the solid ground).
    //    Check a generous 9×9 area around the former source.
    for dx in -8i64..=8 {
        for dz in -8i64..=8 {
            let check = BlockPos::new(8 + dx, 5, 8 + dz);
            assert_eq!(
                world.get_block(check),
                block::AIR,
                "water should have drained at ({}, 5, {})",
                8 + dx,
                8 + dz,
            );
        }
    }
}

#[test]
fn source_block_does_not_drain() {
    // Source blocks (level 0) are permanent — they should not drain on notify.
    let world = flat_world(2);
    let mut graph = CausalGraph::new();
    let rules = ultimate_server::rules::standard();
    let scheduler = Scheduler::new();

    let source_pos = BlockPos::new(8, 5, 8);

    // Place a lone source block (no other water around).
    graph.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: source_pos,
            old: block::AIR,
            new: block::WATER,
        },
    });
    scheduler.run_until_quiet(&world, &mut graph, &rules, 500);

    // Now notify the source as if a neighbor changed.
    let mut graph2 = CausalGraph::new();
    graph2.insert_root(Event {
        payload: EventPayload::BlockNotify { pos: source_pos },
    });
    scheduler.run_until_quiet(&world, &mut graph2, &rules, 100);

    // Source must still be water.
    assert_eq!(world.get_block(source_pos), block::WATER);
}

#[test]
fn water_drains_behind_wall() {
    // Simulates the user's reported scenario: water spreads, then a wall
    // is built cutting it off from the source.  Flowing water behind the
    // wall should drain.
    let world = flat_world(2);
    let mut graph = CausalGraph::new();
    let rules = ultimate_server::rules::standard();
    let scheduler = Scheduler::new();

    let source_pos = BlockPos::new(8, 5, 8);

    // 1. Place water and let it spread.
    graph.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: source_pos,
            old: block::AIR,
            new: block::WATER,
        },
    });
    scheduler.run_until_quiet(&world, &mut graph, &rules, 500);

    // 2. Build a wall of stone around the source, replacing the level-1
    //    flowing water in the 4 horizontal neighbors with stone.
    let wall_positions = [
        BlockPos::new(9, 5, 8),
        BlockPos::new(7, 5, 8),
        BlockPos::new(8, 5, 9),
        BlockPos::new(8, 5, 7),
    ];

    let mut wall_graph = CausalGraph::new();
    for wall_pos in wall_positions {
        let old = world.get_block(wall_pos);
        let root = wall_graph.insert_root(Event {
            payload: EventPayload::BlockSet {
                pos: wall_pos,
                old,
                new: block::STONE,
            },
        });
        // Notify the wall block's own neighbors.
        for neighbor in wall_pos.neighbors() {
            wall_graph.insert(
                Event {
                    payload: EventPayload::BlockNotify { pos: neighbor },
                },
                vec![root],
            );
        }
    }
    scheduler.run_until_quiet(&world, &mut wall_graph, &rules, 2000);

    // 3. Source should still exist.
    assert_eq!(world.get_block(source_pos), block::WATER);

    // 4. Blocks outside the wall should have drained.
    //    Check several positions that were beyond the wall.
    for dx in -8i64..=8 {
        for dz in -8i64..=8 {
            let pos = BlockPos::new(8 + dx, 5, 8 + dz);
            let b = world.get_block(pos);
            if pos == source_pos || wall_positions.contains(&pos) {
                continue; // skip source and wall
            }
            assert!(
                !block::is_fluid(b) || b == block::AIR || b == block::STONE,
                "water should have drained at ({}, 5, {}) but found {:?}",
                8 + dx,
                8 + dz,
                b,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Lava tests
// ---------------------------------------------------------------------------

#[test]
fn lava_spreads_on_surface() {
    let world = flat_world(2);
    let mut graph = CausalGraph::new();
    let rules = ultimate_server::rules::standard();
    let scheduler = Scheduler::new();

    let source_pos = BlockPos::new(8, 5, 8);

    // Place lava source on surface.
    graph.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: source_pos,
            old: block::AIR,
            new: block::LAVA, // level 0 = source
        },
    });
    scheduler.run_until_quiet(&world, &mut graph, &rules, 500);

    // Source should still be lava.
    assert_eq!(world.get_block(source_pos), block::LAVA);

    // At least one horizontal neighbor should have flowing lava.
    let neighbors = [
        world.get_block(BlockPos::new(9, 5, 8)),
        world.get_block(BlockPos::new(7, 5, 8)),
        world.get_block(BlockPos::new(8, 5, 9)),
        world.get_block(BlockPos::new(8, 5, 7)),
    ];
    assert!(
        neighbors.iter().any(|&b| block::lava_level(b).is_some()),
        "lava should spread to at least one neighbor"
    );
}

#[test]
fn lava_spread_limited_to_3_blocks() {
    // Lava should spread at most 3 blocks from the source (max level = 3).
    let world = flat_world(2);
    let mut graph = CausalGraph::new();
    let rules = ultimate_server::rules::standard();
    let scheduler = Scheduler::new();

    let source_pos = BlockPos::new(8, 5, 8);

    graph.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: source_pos,
            old: block::AIR,
            new: block::LAVA,
        },
    });
    scheduler.run_until_quiet(&world, &mut graph, &rules, 500);

    // 3 blocks away in +X should be lava (level 3).
    let at_3 = world.get_block(BlockPos::new(11, 5, 8));
    assert!(
        block::lava_level(at_3).is_some(),
        "lava should reach 3 blocks away, got {:?}",
        at_3,
    );

    // 4 blocks away in +X should be air (beyond max spread).
    let at_4 = world.get_block(BlockPos::new(12, 5, 8));
    assert_eq!(
        at_4,
        block::AIR,
        "lava should NOT reach 4 blocks away, got {:?}",
        at_4,
    );
}

#[test]
fn lava_falls_before_spreading() {
    // Lava placed above air should fall, not spread horizontally.
    let world = World::new();
    let mut chunk = Chunk::new();
    for x in 0..SECTION_SIZE as u8 {
        for z in 0..SECTION_SIZE as u8 {
            chunk.set_block(LocalBlockPos { x, y: 0, z }, block::STONE);
        }
    }
    world.insert_chunk(ChunkPos::new(0, 0), chunk);

    let mut graph = CausalGraph::new();
    let rules = ultimate_server::rules::standard();
    let scheduler = Scheduler::new();

    graph.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: BlockPos::new(4, 5, 4),
            old: block::AIR,
            new: block::LAVA,
        },
    });

    // Step 1: root event places lava.
    scheduler.step(&world, &mut graph, &rules);
    assert_eq!(world.get_block(BlockPos::new(4, 5, 4)), block::LAVA);

    // Step 2: lava falls to y=4.
    scheduler.step(&world, &mut graph, &rules);
    assert!(
        block::lava_level(world.get_block(BlockPos::new(4, 4, 4))).is_some(),
        "lava should have fallen"
    );

    // Horizontal neighbors at y=5 should still be air.
    assert_eq!(world.get_block(BlockPos::new(5, 5, 4)), block::AIR);
    assert_eq!(world.get_block(BlockPos::new(3, 5, 4)), block::AIR);
}

#[test]
fn flowing_lava_drains_when_source_removed() {
    let world = flat_world(2);
    let mut graph = CausalGraph::new();
    let rules = ultimate_server::rules::standard();
    let scheduler = Scheduler::new();

    let source_pos = BlockPos::new(8, 5, 8);

    // 1. Place lava source and let it spread fully.
    graph.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: source_pos,
            old: block::AIR,
            new: block::LAVA,
        },
    });
    scheduler.run_until_quiet(&world, &mut graph, &rules, 500);

    // Sanity: source and at least one neighbor should be lava.
    assert_eq!(world.get_block(source_pos), block::LAVA);
    assert!(
        block::lava_level(world.get_block(BlockPos::new(9, 5, 8))).is_some(),
        "lava should have spread before removal"
    );

    // 2. Remove the source.
    let mut graph2 = CausalGraph::new();
    let root = graph2.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: source_pos,
            old: block::LAVA,
            new: block::AIR,
        },
    });
    for neighbor in source_pos.neighbors() {
        graph2.insert(
            Event {
                payload: EventPayload::BlockNotify { pos: neighbor },
            },
            vec![root],
        );
    }
    scheduler.run_until_quiet(&world, &mut graph2, &rules, 2000);

    // 3. All lava in the area should have drained.
    for dx in -4i64..=4 {
        for dz in -4i64..=4 {
            let check = BlockPos::new(8 + dx, 5, 8 + dz);
            assert_eq!(
                world.get_block(check),
                block::AIR,
                "lava should have drained at ({}, 5, {})",
                8 + dx,
                8 + dz,
            );
        }
    }
}

#[test]
fn lava_source_does_not_drain() {
    let world = flat_world(2);
    let mut graph = CausalGraph::new();
    let rules = ultimate_server::rules::standard();
    let scheduler = Scheduler::new();

    let source_pos = BlockPos::new(8, 5, 8);

    graph.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: source_pos,
            old: block::AIR,
            new: block::LAVA,
        },
    });
    scheduler.run_until_quiet(&world, &mut graph, &rules, 500);

    // Notify the source as if a neighbor changed.
    let mut graph2 = CausalGraph::new();
    graph2.insert_root(Event {
        payload: EventPayload::BlockNotify { pos: source_pos },
    });
    scheduler.run_until_quiet(&world, &mut graph2, &rules, 100);

    // Source must still be lava.
    assert_eq!(world.get_block(source_pos), block::LAVA);
}

// ---------------------------------------------------------------------------
// Elevated water source drainage test
// ---------------------------------------------------------------------------

#[test]
fn elevated_water_source_drains_when_removed() {
    // Scenario: realistic MC world -- dirt at y=4, pillar from y=5 to y=20,
    // water source at y=21. 16-block fall to ground level.
    // Removing the source should drain ALL water.
    let world = flat_world(4);
    let rules = ultimate_server::rules::standard();
    let scheduler = Scheduler::new();

    // Build a tall pillar (like placing blocks in creative mode).
    for y in 5..=20 {
        world.set_block(BlockPos::new(8, y, 8), block::STONE);
    }
    let source_pos = BlockPos::new(8, 21, 8); // on top of the pillar

    // 1. Place water source on top.
    let mut graph = CausalGraph::new();
    graph.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: source_pos,
            old: block::AIR,
            new: block::WATER,
        },
    });
    let spread_events = scheduler.run_until_quiet(&world, &mut graph, &rules, 5000);
    eprintln!("Spread cascade: {} events, {} in graph", spread_events, graph.len());

    // Sanity: source should still be water.
    assert_eq!(world.get_block(source_pos), block::WATER);

    // Water should have spread to horizontal neighbors at source height.
    assert!(
        block::is_fluid(world.get_block(BlockPos::new(9, 21, 8))),
        "water should spread horizontally from source"
    );

    // Water should have fallen to ground level (y=5).
    assert!(
        block::is_fluid(world.get_block(BlockPos::new(9, 5, 8))),
        "water should have fallen to ground level"
    );

    // Count total water blocks before draining.
    let mut water_count = 0;
    for y in 5..=21 {
        for dx in -8i64..=8 {
            for dz in -8i64..=8 {
                if block::is_fluid(world.get_block(BlockPos::new(8 + dx, y, 8 + dz))) {
                    water_count += 1;
                }
            }
        }
    }
    eprintln!("Water blocks before drain: {}", water_count);

    // 2. Remove the source (simulate player placing stone over it).
    let mut graph2 = CausalGraph::new();
    let root = graph2.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: source_pos,
            old: block::WATER,
            new: block::AIR,
        },
    });
    for neighbor in source_pos.neighbors() {
        graph2.insert(
            Event {
                payload: EventPayload::BlockNotify { pos: neighbor },
            },
            vec![root],
        );
    }
    let drain_events = scheduler.run_until_quiet(&world, &mut graph2, &rules, 1000);
    eprintln!("Drain cascade: {} events, {} in graph", drain_events, graph2.len());

    // The drain should complete efficiently -- no spread-drain feedback loop.
    // With ~300-600 water blocks and ~7 events per drain plus redundant neighbor
    // notifications, the total should stay well under 20,000.
    assert!(
        drain_events < 20_000,
        "drain cascade should be efficient (< 20,000 events), got {}",
        drain_events,
    );
    // And the cascade should have fully completed (empty frontier).
    assert!(
        graph2.frontier().is_empty(),
        "drain cascade should reach quiescence within 1000 steps",
    );

    // 3. All water should have drained. Check a generous area.
    let mut remaining_water = Vec::new();
    for y in 5..=21 {
        for dx in -8i64..=8 {
            for dz in -8i64..=8 {
                let pos = BlockPos::new(8 + dx, y, 8 + dz);
                let b = world.get_block(pos);
                if block::is_fluid(b) {
                    remaining_water.push((pos, b));
                }
            }
        }
    }

    assert!(
        remaining_water.is_empty(),
        "water should have fully drained, but {} blocks remain: {:?}",
        remaining_water.len(),
        &remaining_water[..remaining_water.len().min(10)],
    );
}
