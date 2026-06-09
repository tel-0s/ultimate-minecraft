//! Ambient simulation framework.
//!
//! Each [`SimulationLayer`] runs on its own tokio task, periodically
//! generating root causal events. Since Phase 6b-1 the layers are pure
//! event *sources*: generated events are submitted to the shared physics
//! service, which runs the cascade on the server-wide causal graph and
//! broadcasts the resulting changes on the event bus.
//!
//! # Adding a new layer
//!
//! 1. Implement [`SimulationLayer`] for your struct.
//! 2. Push a `Box::new(YourLayer)` into the `layers` vec in `main.rs`.

use std::sync::Arc;
use std::time::Duration;

use ultimate_engine::causal::event::Event;
use ultimate_engine::world::World;

use crate::physics::PhysicsHandle;

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
/// Each task loops on `layer.interval()` and submits generated events to
/// the shared physics service; the service runs the cascade and publishes
/// the resulting changes to the event bus.
pub fn start(
    world: Arc<World>,
    layers: Vec<Box<dyn SimulationLayer>>,
    physics: PhysicsHandle,
) {
    for layer in layers {
        let world = Arc::clone(&world);
        let physics = physics.clone();
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

                tracing::debug!("Simulation '{}': submitting {} root events", name, events.len());
                physics.submit_events(events);
            }
        });
    }
}
