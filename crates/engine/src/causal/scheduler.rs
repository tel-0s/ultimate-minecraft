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

    /// Sequential step: every consequent stays local. This is
    /// [`step_routed`](Self::step_routed) with the identity router.
    pub fn step(&self, world: &World, graph: &mut CausalGraph, rules: &RuleSet) -> usize {
        self.step_routed(world, graph, rules, &mut |_, _| true)
    }

    /// Sequential step where every consequent passes through `route`
    /// before insertion. `route` returns `true` to keep the event local
    /// (inserted as a child of its cause) or `false` when it has taken
    /// ownership of the event — e.g. shipped it to another partition's
    /// graph — in which case it is NOT inserted here.
    ///
    /// This is the partition-boundary hook (Phase 6b-2): a worker owning a
    /// subset of chunks routes consequents that target foreign chunks to
    /// their owners. Consequents are generated *after* their cause
    /// executed, so the cause's world write is visible before any routed
    /// message is sent — the happens-before edge rides the transport.
    /// The router also receives the priority the consequent inherits
    /// (its cause's priority), so shipped events keep their scheduling
    /// lane on the receiving partition.
    pub fn step_routed(
        &self,
        world: &World,
        graph: &mut CausalGraph,
        rules: &RuleSet,
        route: &mut dyn FnMut(&Event, u8) -> bool,
    ) -> usize {
        let batch = graph.drain_ready(self.max_events_per_step);
        let mut executed = 0;
        let mut synth = Vec::new();

        for id in batch {
            let (event, priority) = match graph.get(id) {
                Some(node) => (node.event.clone(), node.priority),
                None => continue,
            };

            let effective = apply_event(world, &event.payload, &mut synth);
            graph.mark_executed(id);
            executed += 1;

            if should_log(&event.payload, effective) {
                graph.log_write(&event.payload);
            }
            for p in synth.drain(..) {
                graph.log_write(&p);
            }
            if effective {
                let consequents = rules.evaluate(world, &event.payload);
                for new_event in consequents {
                    if route(&new_event, priority) {
                        graph.insert(new_event, vec![id]);
                    }
                }
            }
            graph.finish(id);
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

        let results: Vec<Vec<(EventId, Event, bool, Vec<EventPayload>, Vec<Event>)>> = groups
            .into_par_iter()
            .map(|group| {
                group
                    .into_iter()
                    .map(|(id, event)| {
                        let mut synth = Vec::new();
                        let effective = apply_event(world, &event.payload, &mut synth);
                        let consequents = if effective {
                            rules.evaluate(world, &event.payload)
                        } else {
                            Vec::new()
                        };
                        (id, event, effective, synth, consequents)
                    })
                    .collect()
            })
            .collect();

        let mut executed = 0;
        for group_results in results {
            for (id, event, effective, synth, consequents) in group_results {
                graph.mark_executed(id);
                executed += 1;
                if should_log(&event.payload, effective) {
                    graph.log_write(&event.payload);
                }
                for p in synth {
                    graph.log_write(&p);
                }
                for new_event in consequents {
                    graph.insert(new_event, vec![id]);
                }
                graph.finish(id);
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

/// Should this executed event land in the graph's write log?
///
/// Effective `BlockSet`s, always. `LightSet`s regardless of apply
/// effectiveness: light rules write light storage synchronously (BFS inside
/// the rule) and emit `LightSet` purely as a report of what changed, so by
/// the time the event executes its write is already a no-op.
fn should_log(payload: &EventPayload, effective: bool) -> bool {
    match payload {
        EventPayload::BlockSet { .. } | EventPayload::EntitySet { .. } => effective,
        EventPayload::LightSet { .. } | EventPayload::LightBatch { .. } => true,
        // Compounds: apply synthesizes their concrete outcome entries.
        EventPayload::EntityMaterialize { .. } => false,
        EventPayload::AtomicBlockSet { .. } => false,
        _ => false,
    }
}

/// How far above a blocked landing the materialize scan will climb before
/// voiding the block (matches the build-height ceiling; vanilla also
/// voids above it).
const MATERIALIZE_SCAN_CAP: i64 = 320;

/// Apply the event's write to the world.  Returns `true` when the write was
/// effective (the value actually changed) so that the scheduler can skip rule
/// evaluation for redundant / duplicate writes.
///
/// Compound events (`EntityMaterialize`) perform several concrete writes
/// inside one apply; they push the exact concrete payloads into
/// `synth_log` so replicas and clients replay outcomes, not intents.
fn apply_event(world: &World, payload: &EventPayload, synth_log: &mut Vec<EventPayload>) -> bool {
    match payload {
        EventPayload::BlockSet { pos, old, new } => {
            // Stale-precondition guard: the rule that emitted this event
            // observed `old` at `pos`. If a causally-unrelated event has
            // since changed the cell, this write is based on stale state —
            // skip it (and its consequents) rather than clobber the newer
            // value. Prevents e.g. block duplication when two cascades
            // race to move different blocks into the same cell.
            let current = world.get_block(*pos);
            if current != *old || old == new {
                return false;
            }
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
        // Reporting-only: the light rule's BFS already wrote light storage.
        EventPayload::LightBatch { .. } => true,
        // Guarded entity transition — same first-write-wins semantics as
        // BlockSet: a superseded in-flight trajectory segment fails its
        // guard here and dies without consequents (never forks the entity).
        EventPayload::EntitySet { id, old, new } => {
            world.entities().set_entity(*id, old.as_ref(), new.as_ref()) && old != new
        }
        // Wakes carry no write; the entity rules re-derive from the store.
        EventPayload::EntityWake { .. } => true,
        // The atomic entity→block conversion (see event.rs). Winning the
        // entity guard is the transaction commit point: nothing else can
        // transition this entity afterwards (its state is gone), so the
        // block placement below cannot race a bump/replan — under ANY
        // worker interleaving, including region-handoff dual ownership.
        EventPayload::EntityMaterialize { id, old, block } => {
            if !world.entities().set_entity(*id, Some(old), None) {
                return false; // superseded: another transition won the entity
            }
            synth_log.push(EventPayload::EntitySet { id: *id, old: Some(*old), new: None });
            // First replaceable cell scanning up. The scan+write is
            // COLUMN-ATOMIC (under the chunk's write guard): the entity
            // guard arbitrates which transition wins the entity, and the
            // chunk guard arbitrates which co-landing materialize gets
            // which cell — two concurrent scans serialize, the second
            // climbs past the first's write instead of overwriting it.
            // The scan is vertical, so placement never leaves this
            // event's chunk/owner.
            let base = old.pos.block_pos();
            match world.place_first_replaceable_up(base, MATERIALIZE_SCAN_CAP, *block) {
                Some((cell, replaced)) => {
                    synth_log.push(EventPayload::BlockSet {
                        pos: cell,
                        old: replaced,
                        new: *block,
                    });
                }
                // No free cell below the ceiling (or missing chunk): the
                // block is voided — the despawn stands; matter loss only
                // above build height.
                None => {}
            }
            true
        }
        // ATOMIC multi-cell rewrite (pistons): all-or-nothing under the
        // affected chunks' write locks — see `World::apply_atomic_writes`.
        // On success, synthesize per-cell `BlockSet` log entries so
        // replicas and clients replay the exact outcome; on any stale
        // cell, nothing moved and nothing is logged (the emitting rule
        // re-evaluates from a later notify — self-healing, no tearing).
        EventPayload::AtomicBlockSet { writes } => {
            if !world.apply_atomic_writes(writes) {
                return false;
            }
            for w in writes.iter() {
                synth_log.push(EventPayload::BlockSet { pos: w.pos, old: w.old, new: w.new });
            }
            true
        }
        // `After` never executes directly: the physics router unwraps it
        // into a timer heap. If one reaches a plain scheduler (tests using
        // step()/step_parallel with no timer plane), treat it as
        // ineffective — timed semantics need the physics service.
        EventPayload::After { .. } => false,
    }
}
