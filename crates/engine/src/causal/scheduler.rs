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
        let batch = graph.drain_ready(self.max_events_per_step);
        let mut executed = 0;

        for id in batch {
            let event = match graph.get(id) {
                Some(node) => node.event.clone(),
                None => continue,
            };

            let effective = apply_event(world, &event.payload);
            graph.mark_executed(id);
            executed += 1;

            if effective {
                let consequents = rules.evaluate(world, &event.payload);
                for new_event in consequents {
                    graph.insert(new_event, vec![id]);
                }
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
        let batch = graph.drain_ready(self.max_events_per_step);
        if batch.is_empty() {
            return 0;
        }

        let events: Vec<(EventId, Event)> = batch
            .iter()
            .filter_map(|&id| graph.get(id).map(|node| (id, node.event.clone())))
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
                        let effective = apply_event(world, &event.payload);
                        let consequents = if effective {
                            rules.evaluate(world, &event.payload)
                        } else {
                            Vec::new()
                        };
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

/// Apply the event's write to the world.  Returns `true` when the write was
/// effective (the value actually changed) so that the scheduler can skip rule
/// evaluation for redundant / duplicate writes.
fn apply_event(world: &World, payload: &EventPayload) -> bool {
    match payload {
        EventPayload::BlockSet { pos, new, .. } => {
            world.set_block(*pos, *new);
            true
        }
        EventPayload::BlockNotify { .. } => true,
        EventPayload::LightSet {
            pos,
            light_type,
            new,
            ..
        } => {
            let current = match light_type {
                super::event::LightType::Sky => world.get_sky_light(*pos),
                super::event::LightType::Block => world.get_block_light(*pos),
            };
            if *new == current {
                return false;
            }
            match light_type {
                super::event::LightType::Sky => world.set_sky_light(*pos, *new),
                super::event::LightType::Block => world.set_block_light(*pos, *new),
            }
            true
        }
        EventPayload::LightNotify { .. } => true,
    }
}
