/// Eval queue: WS tasks push constraint IDs here,
/// orchestrator drains them for arb evaluation.
///
/// Two priority levels:
///   - Urgent: EFP drift > threshold (process first)
///   - Background: stale data refresh, periodic re-eval
///
/// Uses a single Mutex over all state to prevent lock ordering deadlocks.
use std::collections::HashSet;
use parking_lot::Mutex;

/// All queue state under a single lock — prevents ABBA deadlocks.
struct QueueInner {
    urgent: Vec<QueueEntry>,
    background: Vec<QueueEntry>,
    urgent_set: HashSet<String>,
    bg_set: HashSet<String>,
}

pub struct EvalQueue {
    inner: Mutex<QueueInner>,
}

#[derive(Debug, Clone)]
pub struct QueueEntry {
    pub constraint_id: String,
    pub trigger_asset: String,
    pub queued_at: f64,
}

#[derive(Debug, Clone)]
pub struct DrainResult {
    pub constraint_id: String,
    pub urgent: bool,
    pub queued_at: f64,
}

impl EvalQueue {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(QueueInner {
                urgent: Vec::new(),
                background: Vec::new(),
                urgent_set: HashSet::new(),
                bg_set: HashSet::new(),
            }),
        }
    }

    /// Push a constraint eval (called from WS handler threads).
    pub fn push(&self, constraint_id: &str, asset_id: &str, urgent: bool, now: f64) {
        let entry = QueueEntry {
            constraint_id: constraint_id.to_string(),
            trigger_asset: asset_id.to_string(),
            queued_at: now,
        };

        let mut q = self.inner.lock();
        if urgent {
            if q.urgent_set.insert(constraint_id.to_string()) {
                q.urgent.push(entry);
            }
            q.bg_set.remove(constraint_id);
        } else {
            if !q.urgent_set.contains(constraint_id) {
                if q.bg_set.insert(constraint_id.to_string()) {
                    q.background.push(entry);
                }
            }
        }
    }

    /// Drain up to `max` entries, urgent first then background.
    pub fn drain(&self, max: usize) -> Vec<DrainResult> {
        let mut q = self.inner.lock();
        let mut results = Vec::with_capacity(max);

        // Drain urgent first
        let take = q.urgent.len().min(max);
        let urgent_entries: Vec<QueueEntry> = q.urgent.drain(..take).collect();
        for entry in urgent_entries {
            q.urgent_set.remove(&entry.constraint_id);
            results.push(DrainResult {
                constraint_id: entry.constraint_id,
                urgent: true,
                queued_at: entry.queued_at,
            });
        }

        // Fill remainder from background
        let remaining = max.saturating_sub(results.len());
        if remaining > 0 {
            let take = q.background.len().min(remaining);
            let bg_entries: Vec<QueueEntry> = q.background.drain(..take).collect();
            for entry in bg_entries {
                q.bg_set.remove(&entry.constraint_id);
                results.push(DrainResult {
                    constraint_id: entry.constraint_id,
                    urgent: false,
                    queued_at: entry.queued_at,
                });
            }
        }

        results
    }

    /// Queue depths for monitoring.
    pub fn depths(&self) -> (usize, usize) {
        let q = self.inner.lock();
        (q.urgent.len(), q.background.len())
    }

    /// Clear all queues.
    pub fn clear(&self) {
        let mut q = self.inner.lock();
        q.urgent.clear();
        q.background.clear();
        q.urgent_set.clear();
        q.bg_set.clear();
    }
}

impl Default for EvalQueue {
    fn default() -> Self { Self::new() }
}
