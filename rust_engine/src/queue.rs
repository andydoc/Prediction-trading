/// Eval queue: Rust WS tasks push constraint IDs here,
/// Python drains them for arb evaluation.
///
/// Two priority levels:
///   - Urgent: EFP drift > threshold (process first)
///   - Background: stale data refresh, periodic re-eval
use std::collections::HashSet;
use parking_lot::Mutex;

/// Thread-safe eval queue with deduplication.
pub struct EvalQueue {
    urgent: Mutex<Vec<QueueEntry>>,
    background: Mutex<Vec<QueueEntry>>,
    /// Track which constraint_ids are already queued (dedup).
    urgent_set: Mutex<HashSet<String>>,
    bg_set: Mutex<HashSet<String>>,
}

#[derive(Debug, Clone)]
pub struct QueueEntry {
    pub constraint_id: String,
    pub trigger_asset: String,
    pub queued_at: f64,
}

/// Result returned to Python from drain.
#[derive(Debug, Clone)]
pub struct DrainResult {
    pub constraint_id: String,
    pub urgent: bool,
}

impl EvalQueue {
    pub fn new() -> Self {
        Self {
            urgent: Mutex::new(Vec::new()),
            background: Mutex::new(Vec::new()),
            urgent_set: Mutex::new(HashSet::new()),
            bg_set: Mutex::new(HashSet::new()),
        }
    }

    /// Push a constraint eval (called from WS handler threads).
    pub fn push(&self, constraint_id: &str, asset_id: &str, urgent: bool) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        let entry = QueueEntry {
            constraint_id: constraint_id.to_string(),
            trigger_asset: asset_id.to_string(),
            queued_at: now,
        };

        if urgent {
            let mut set = self.urgent_set.lock();
            if set.insert(constraint_id.to_string()) {
                self.urgent.lock().push(entry);
            }
            // Also remove from background if it was there
            self.bg_set.lock().remove(constraint_id);
        } else {
            let mut set = self.bg_set.lock();
            // Don't add to background if already in urgent
            if !self.urgent_set.lock().contains(constraint_id) {
                if set.insert(constraint_id.to_string()) {
                    self.background.lock().push(entry);
                }
            }
        }
    }

    /// Drain up to `max` entries, urgent first then background.
    /// Returns entries for Python to evaluate.
    pub fn drain(&self, max: usize) -> Vec<DrainResult> {
        let mut results = Vec::with_capacity(max);

        // Drain urgent first
        {
            let mut q = self.urgent.lock();
            let mut set = self.urgent_set.lock();
            let take = q.len().min(max);
            for entry in q.drain(..take) {
                set.remove(&entry.constraint_id);
                results.push(DrainResult {
                    constraint_id: entry.constraint_id,
                    urgent: true,
                });
            }
        }

        // Fill remainder from background
        let remaining = max.saturating_sub(results.len());
        if remaining > 0 {
            let mut q = self.background.lock();
            let mut set = self.bg_set.lock();
            let take = q.len().min(remaining);
            for entry in q.drain(..take) {
                set.remove(&entry.constraint_id);
                results.push(DrainResult {
                    constraint_id: entry.constraint_id,
                    urgent: false,
                });
            }
        }

        results
    }

    /// Queue depths for monitoring.
    pub fn depths(&self) -> (usize, usize) {
        (self.urgent.lock().len(), self.background.lock().len())
    }

    /// Clear all queues.
    pub fn clear(&self) {
        self.urgent.lock().clear();
        self.background.lock().clear();
        self.urgent_set.lock().clear();
        self.bg_set.lock().clear();
    }
}
