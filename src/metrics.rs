//! Lightweight in-process job counters + a periodic heartbeat log line
//! (PLAN Phase 5 ops). All atomics, cheap to share via `Arc`; the heartbeat is
//! opt-out via `HEARTBEAT_SECS=0`. Kept deliberately tiny — no histograms or
//! external exporters on the 1 GB box.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct Metrics {
    received: AtomicU64,
    succeeded: AtomicU64,
    failed: AtomicU64,
    timed_out: AtomicU64,
    start: Instant,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            received: AtomicU64::new(0),
            succeeded: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            timed_out: AtomicU64::new(0),
            start: Instant::now(),
        }
    }

    /// Count a received job; returns its 1-based sequence number (used as the
    /// job span's `seq`, so log lines for one post share an id).
    pub fn record_received(&self) -> u64 {
        self.received.fetch_add(1, Ordering::Relaxed) + 1
    }
    pub fn record_succeeded(&self) {
        self.succeeded.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_failed(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_timed_out(&self) {
        self.timed_out.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            received: self.received.load(Ordering::Relaxed),
            succeeded: self.succeeded.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            timed_out: self.timed_out.load(Ordering::Relaxed),
            uptime_secs: self.start.elapsed().as_secs(),
        }
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Snapshot {
    pub received: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub timed_out: u64,
    pub uptime_secs: u64,
}

/// Spawn a task that logs a `heartbeat` line every `interval`. The immediate
/// first tick is consumed so it doesn't fire at t=0 alongside the startup logs.
pub fn spawn_heartbeat(metrics: Arc<Metrics>, interval: Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.tick().await; // consume the immediate first tick
        loop {
            tick.tick().await;
            let s = metrics.snapshot();
            tracing::info!(
                received = s.received,
                succeeded = s.succeeded,
                failed = s.failed,
                timed_out = s.timed_out,
                uptime_secs = s.uptime_secs,
                "heartbeat"
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_increment_and_snapshot() {
        let m = Metrics::new();
        assert_eq!(m.record_received(), 1);
        assert_eq!(m.record_received(), 2);
        m.record_succeeded();
        m.record_failed();
        m.record_failed();
        m.record_timed_out();
        let s = m.snapshot();
        assert_eq!(s.received, 2);
        assert_eq!(s.succeeded, 1);
        assert_eq!(s.failed, 2);
        assert_eq!(s.timed_out, 1);
    }
}
