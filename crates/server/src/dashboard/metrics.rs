//! Lock-free performance counters.
//!
//! Physics threads update these via atomic operations — no locks, no
//! allocations, no blocking on the hot path. The dashboard server reads
//! them at its own pace.

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

/// Atomic performance counters. ~10 ns to update (a handful of `fetch_add`s).
pub struct Metrics {
    // Monotonic counters
    events_executed: AtomicU64,
    cascades_completed: AtomicU64,
    cascade_events_sum: AtomicU64,
    cascade_ns_sum: AtomicU64,

    // Latency histogram buckets (cascade duration)
    hist_under_1us: AtomicU64,
    hist_1_10us: AtomicU64,
    hist_10_100us: AtomicU64,
    hist_100us_1ms: AtomicU64,
    hist_over_1ms: AtomicU64,

    // Gauges
    players_connected: AtomicU64,

    started_at: Instant,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            events_executed: AtomicU64::new(0),
            cascades_completed: AtomicU64::new(0),
            cascade_events_sum: AtomicU64::new(0),
            cascade_ns_sum: AtomicU64::new(0),
            hist_under_1us: AtomicU64::new(0),
            hist_1_10us: AtomicU64::new(0),
            hist_10_100us: AtomicU64::new(0),
            hist_100us_1ms: AtomicU64::new(0),
            hist_over_1ms: AtomicU64::new(0),
            players_connected: AtomicU64::new(0),
            started_at: Instant::now(),
        }
    }

    /// Called after each `run_until_quiet()` completes. Zero-alloc, ~10 ns.
    pub fn record_cascade(&self, events: u64, duration: Duration) {
        self.events_executed.fetch_add(events, Relaxed);
        self.cascades_completed.fetch_add(1, Relaxed);
        self.cascade_events_sum.fetch_add(events, Relaxed);
        self.cascade_ns_sum
            .fetch_add(duration.as_nanos() as u64, Relaxed);

        let us = duration.as_micros() as u64;
        match us {
            0 => {
                self.hist_under_1us.fetch_add(1, Relaxed);
            }
            1..=9 => {
                self.hist_1_10us.fetch_add(1, Relaxed);
            }
            10..=99 => {
                self.hist_10_100us.fetch_add(1, Relaxed);
            }
            100..=999 => {
                self.hist_100us_1ms.fetch_add(1, Relaxed);
            }
            _ => {
                self.hist_over_1ms.fetch_add(1, Relaxed);
            }
        }
    }

    pub fn player_joined(&self) {
        self.players_connected.fetch_add(1, Relaxed);
    }

    pub fn player_left(&self) {
        self.players_connected.fetch_sub(1, Relaxed);
    }

    /// Read all counters into a serializable snapshot.
    /// Called by the dashboard server (~every 200 ms), never by the hot path.
    pub fn snapshot(&self, chunks_loaded: u64) -> MetricsSnapshot {
        MetricsSnapshot {
            uptime_secs: self.started_at.elapsed().as_secs_f64(),
            events_total: self.events_executed.load(Relaxed),
            cascades_total: self.cascades_completed.load(Relaxed),
            cascade_events_sum: self.cascade_events_sum.load(Relaxed),
            cascade_ns_sum: self.cascade_ns_sum.load(Relaxed),
            chunks_loaded,
            players: self.players_connected.load(Relaxed),
            hist: [
                self.hist_under_1us.load(Relaxed),
                self.hist_1_10us.load(Relaxed),
                self.hist_10_100us.load(Relaxed),
                self.hist_100us_1ms.load(Relaxed),
                self.hist_over_1ms.load(Relaxed),
            ],
        }
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Serializable snapshot of all metrics at a point in time.
/// The client computes rates (events/sec, etc.) by diffing consecutive snapshots.
#[derive(Clone, Serialize)]
pub struct MetricsSnapshot {
    pub uptime_secs: f64,
    pub events_total: u64,
    pub cascades_total: u64,
    pub cascade_events_sum: u64,
    pub cascade_ns_sum: u64,
    pub chunks_loaded: u64,
    pub players: u64,
    /// `[<1μs, 1-10μs, 10-100μs, 100μs-1ms, >1ms]`
    pub hist: [u64; 5],
}
