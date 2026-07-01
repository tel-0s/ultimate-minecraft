//! Engine time (Phase 5).
//!
//! Causality provides ORDER; the clock provides PACE. Timed events
//! (`EventPayload::After`) carry a not-before deadline in engine nanoseconds;
//! nothing in the engine ever *synchronizes* on the clock — it is a floor,
//! never a barrier.
//!
//! The trait exists so tests and benches can drive virtual time
//! deterministically (`ManualClock`): a 5-minute despawn timer costs zero
//! wall-clock in a test, and causal-invariance tests of entity rules stay
//! exact.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::world::entity::Nanos;

pub trait Clock: Send + Sync {
    /// Engine time now, in nanoseconds since this clock's epoch.
    fn now(&self) -> Nanos;
}

/// Production clock: monotonic wall time since construction.
pub struct MonotonicClock {
    epoch: Instant,
}

impl MonotonicClock {
    pub fn new() -> Self {
        Self { epoch: Instant::now() }
    }
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MonotonicClock {
    fn now(&self) -> Nanos {
        self.epoch.elapsed().as_nanos() as Nanos
    }
}

/// Test clock: time advances only when told to. Pair with
/// `PhysicsHandle::kick()` so parked workers re-check their timer heaps
/// after a jump.
pub struct ManualClock {
    now: AtomicU64,
}

impl ManualClock {
    pub fn new() -> Self {
        Self { now: AtomicU64::new(0) }
    }

    pub fn advance(&self, by: Nanos) {
        self.now.fetch_add(by, Ordering::SeqCst);
    }

    pub fn set(&self, to: Nanos) {
        self.now.store(to, Ordering::SeqCst);
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Nanos {
        self.now.load(Ordering::SeqCst)
    }
}
