//! Ambient simulation framework.
//!
//! Each [`SimulationLayer`] runs on its own tokio task, periodically generating
//! root causal events. Those events are run through a fresh [`CausalGraph`] +
//! scheduler, and the resulting block changes are published to the event bus.
//!
//! # Adding a new layer
//!
//! 1. Implement [`SimulationLayer`] for your struct.
//! 2. Push a `Box::new(YourLayer)` into the `layers` vec in `main.rs`.
//!
//! The runner handles scheduling, cascade execution, and bus publishing.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use ultimate_engine::causal::event::Event;
use ultimate_engine::causal::graph::CausalGraph;
use ultimate_engine::causal::scheduler::Scheduler;
use ultimate_engine::world::World;

use crate::event_bus::{self, ChangeSource, WorldChangeBatch};

/// A pluggable simulation layer that generates root causal events on a timer.
///
/// Layers are expected to be cheap per tick -- heavy work should be amortized
/// across ticks or done lazily.
pub trait SimulationLayer: Send + Sync + 'static {
    /// Human-readable name (used for logging and [`ChangeSource::Simulation`]).
    fn name(&self) -> &'static str;

    /// How often this layer ticks.
    fn interval(&self) -> Duration;

    /// Inspect the world and return root events to inject (if any).
    ///
    /// Returning an empty vec is fine -- it just means "nothing to do this tick."
    fn generate_events(&self, world: &World) -> Vec<Event>;
}

/// Spawn one tokio task per simulation layer.
///
/// Each task loops on `layer.interval()`, runs a fresh causal cascade for the
/// generated events, and publishes the resulting block changes to `bus`.
pub fn start(
    world: Arc<World>,
    layers: Vec<Box<dyn SimulationLayer>>,
    bus: broadcast::Sender<WorldChangeBatch>,
) {
    for layer in layers {
        let world = Arc::clone(&world);
        let bus = bus.clone();
        tokio::spawn(async move {
            let name = layer.name();
            let mut interval = tokio::time::interval(layer.interval());
            // The first tick fires immediately; skip it so the world has time to initialize.
            interval.tick().await;

            tracing::info!("Simulation layer '{}' started (interval {:?})", name, layer.interval());

            loop {
                interval.tick().await;

                let events = layer.generate_events(&world);
                if events.is_empty() {
                    continue;
                }

                // Fresh graph + scheduler per tick (same pattern as player actions).
                let mut graph = CausalGraph::new();
                for event in events {
                    graph.insert_root(event);
                }

                let rules = crate::rules::standard();
                let scheduler = Scheduler::new();
                let executed = scheduler.run_until_quiet(&world, &mut graph, &rules, 1000);

                let changes = event_bus::collect_block_changes(&graph);
                if !changes.is_empty() {
                    let num_changes = changes.len();
                    let batch = WorldChangeBatch {
                        source: ChangeSource::Simulation(name),
                        changes: changes.into(),
                    };
                    // Ignore send errors (no subscribers = no problem).
                    let _ = bus.send(batch);

                    tracing::debug!(
                        "Simulation '{}': {} events executed, {} block changes published",
                        name, executed, num_changes
                    );
                }
            }
        });
    }
}
