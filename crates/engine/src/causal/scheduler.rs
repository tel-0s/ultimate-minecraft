use super::event::{Event, EventId, EventPayload};
use super::graph::CausalGraph;
use crate::rules::RuleSet;
use crate::world::World;
use rayon::prelude::*;
use std::collections::HashMap;

/// Drains the causal frontier, applying events to the world and generating
/// consequent events via the rule set.
///
/// Provides both sequential (`step`) and parallel (`step_parallel`) execution.
pub struct Scheduler {
    pub max_events_per_step: usize,
}

impl Scheduler {
    pub fn new() -> Self {
        Self {
            max_events_per_step: 10_000,
        }
    }

    // ── Sequential execution ────────────────────────────────────────────

    pub fn step(&self, world: &World, graph: &mut CausalGraph, rules: &RuleSet) -> usize {
        let frontier = graph.frontier();
        let mut executed = 0;

        for id in frontier {
            if executed >= self.max_events_per_step {
                break;
            }

            let event = match graph.get(id) {
                Some(node) => node.event.clone(),
                None => continue,
            };

            apply_event(world, &event.payload);
            graph.mark_executed(id);
            executed += 1;

            let consequents = rules.evaluate(world, &event.payload);
            for new_event in consequents {
                graph.insert(new_event, vec![id]);
            }
        }

        executed
    }

    pub fn run_until_quiet(
        &self,
        world: &World,
        graph: &mut CausalGraph,
        rules: &RuleSet,
        max_steps: usize,
    ) -> usize {
        let mut total = 0;
        for _ in 0..max_steps {
            let n = self.step(world, graph, rules);
            if n == 0 {
                break;
            }
            total += n;
        }
        total
    }

    // ── Parallel execution (snapshot-scatter-gather) ────────────────────

    pub fn step_parallel(&self, world: &World, graph: &mut CausalGraph, rules: &RuleSet) -> usize {
        let frontier = graph.frontier();
        if frontier.is_empty() {
            return 0;
        }

        let events: Vec<(EventId, Event)> = frontier
            .iter()
            .filter_map(|&id| graph.get(id).map(|node| (id, node.event.clone())))
            .take(self.max_events_per_step)
            .collect();

        let mut chunk_groups: HashMap<_, Vec<(EventId, Event)>> = HashMap::new();
        for (id, event) in events {
            chunk_groups
                .entry(event.chunk())
                .or_default()
                .push((id, event));
        }
        let groups: Vec<Vec<(EventId, Event)>> = chunk_groups.into_values().collect();

        let results: Vec<Vec<(EventId, Vec<Event>)>> = groups
            .into_par_iter()
            .map(|group| {
                group
                    .into_iter()
                    .map(|(id, event)| {
                        apply_event(world, &event.payload);
                        let consequents = rules.evaluate(world, &event.payload);
                        (id, consequents)
                    })
                    .collect()
            })
            .collect();

        let mut executed = 0;
        for group_results in results {
            for (id, consequents) in group_results {
                graph.mark_executed(id);
                executed += 1;
                for new_event in consequents {
                    graph.insert(new_event, vec![id]);
                }
            }
        }

        executed
    }

    pub fn run_until_quiet_parallel(
        &self,
        world: &World,
        graph: &mut CausalGraph,
        rules: &RuleSet,
        max_steps: usize,
    ) -> usize {
        let mut total = 0;
        for _ in 0..max_steps {
            let n = self.step_parallel(world, graph, rules);
            if n == 0 {
                break;
            }
            total += n;
        }
        total
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

fn apply_event(world: &World, payload: &EventPayload) {
    match payload {
        EventPayload::BlockSet { pos, new, .. } => {
            world.set_block(*pos, *new);
        }
        EventPayload::BlockNotify { .. } => {}
    }
}
