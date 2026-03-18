/// End-to-end latency instrumentation — measures every link in the chain
/// from Polymarket WS message to position entry/exit.
///
/// Segments:
///   1. ws_network:     Polymarket server timestamp → local receive
///   2. ws_to_queue:    Local receive → queue.push() complete
///   3. queue_wait:     queue.push() → evaluate_batch() drain
///   4. eval_batch:     evaluate_batch() duration (arb math)
///   5. eval_to_entry:  evaluate_batch() return → entry/exit decision
///   6. e2e:            Polymarket server timestamp → entry decision
///
/// Toggle: `engine.latency_instrumentation: true` in config.yaml.
/// Zero overhead when disabled (AtomicBool early-out on every record call).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use parking_lot::Mutex;

const MAX_SAMPLES: usize = 500; // ~83 min at 10s sampling interval

/// Per-segment percentile snapshot.
#[derive(Debug, Clone, Default)]
pub struct SegmentStats {
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
    pub count: usize,
}

/// Full snapshot across all segments.
#[derive(Debug, Clone, Default)]
pub struct LatencySnapshot {
    pub ws_network: SegmentStats,
    pub ws_to_queue: SegmentStats,
    pub queue_wait: SegmentStats,
    pub eval_batch: SegmentStats,
    pub eval_to_entry: SegmentStats,
    pub e2e: SegmentStats,
}

pub struct LatencyTracker {
    enabled: AtomicBool,
    ws_network: Mutex<VecDeque<f64>>,
    ws_to_queue: Mutex<VecDeque<f64>>,
    queue_wait: Mutex<VecDeque<f64>>,
    eval_batch: Mutex<VecDeque<f64>>,
    eval_to_entry: Mutex<VecDeque<f64>>,
    e2e: Mutex<VecDeque<f64>>,
}

impl LatencyTracker {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            ws_network: Mutex::new(VecDeque::with_capacity(MAX_SAMPLES)),
            ws_to_queue: Mutex::new(VecDeque::with_capacity(MAX_SAMPLES)),
            queue_wait: Mutex::new(VecDeque::with_capacity(MAX_SAMPLES)),
            eval_batch: Mutex::new(VecDeque::with_capacity(MAX_SAMPLES)),
            eval_to_entry: Mutex::new(VecDeque::with_capacity(MAX_SAMPLES)),
            e2e: Mutex::new(VecDeque::with_capacity(MAX_SAMPLES)),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
        if on {
            tracing::info!("Latency instrumentation ENABLED");
        } else {
            tracing::info!("Latency instrumentation DISABLED");
        }
    }

    // --- Record methods (all in microseconds) ---

    pub fn record_ws_network(&self, us: f64) {
        if !self.is_enabled() { return; }
        push_sample(&self.ws_network, us);
    }

    pub fn record_ws_to_queue(&self, us: f64) {
        if !self.is_enabled() { return; }
        push_sample(&self.ws_to_queue, us);
    }

    pub fn record_queue_wait(&self, us: f64) {
        if !self.is_enabled() { return; }
        push_sample(&self.queue_wait, us);
    }

    pub fn record_eval_batch(&self, us: f64) {
        if !self.is_enabled() { return; }
        push_sample(&self.eval_batch, us);
    }

    pub fn record_eval_to_entry(&self, us: f64) {
        if !self.is_enabled() { return; }
        push_sample(&self.eval_to_entry, us);
    }

    pub fn record_e2e(&self, us: f64) {
        if !self.is_enabled() { return; }
        push_sample(&self.e2e, us);
    }

    /// Compute percentiles for all segments. Brief lock per buffer.
    pub fn snapshot(&self) -> LatencySnapshot {
        LatencySnapshot {
            ws_network: percentiles(&self.ws_network),
            ws_to_queue: percentiles(&self.ws_to_queue),
            queue_wait: percentiles(&self.queue_wait),
            eval_batch: percentiles(&self.eval_batch),
            eval_to_entry: percentiles(&self.eval_to_entry),
            e2e: percentiles(&self.e2e),
        }
    }

    /// Clear all buffers (e.g. after config change or re-enable).
    pub fn clear(&self) {
        self.ws_network.lock().clear();
        self.ws_to_queue.lock().clear();
        self.queue_wait.lock().clear();
        self.eval_batch.lock().clear();
        self.eval_to_entry.lock().clear();
        self.e2e.lock().clear();
    }
}

fn push_sample(buf: &Mutex<VecDeque<f64>>, value: f64) {
    let mut b = buf.lock();
    if b.len() >= MAX_SAMPLES {
        b.pop_front();
    }
    b.push_back(value);
}

fn percentiles(buf: &Mutex<VecDeque<f64>>) -> SegmentStats {
    let b = buf.lock();
    if b.is_empty() {
        return SegmentStats::default();
    }
    let mut sorted: Vec<f64> = b.iter().copied().collect();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let n = sorted.len();
    SegmentStats {
        p50: sorted[n / 2],
        p95: sorted[((n as f64 * 0.95).ceil() as usize).saturating_sub(1).min(n - 1)],
        max: sorted[n - 1],
        count: n,
    }
}
