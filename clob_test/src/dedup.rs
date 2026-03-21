/// Position deduplication — prevents opening positions on markets already in use.

use std::collections::{HashMap, HashSet};

pub struct PositionDedup {
    /// market_id → test_id that first opened a position there
    occupied: HashMap<String, String>,
}

impl PositionDedup {
    pub fn new() -> Self {
        Self { occupied: HashMap::new() }
    }

    /// Check if ALL market_ids are available (none already occupied).
    pub fn can_open(&self, market_ids: &[String]) -> bool {
        market_ids.iter().all(|id| !self.occupied.contains_key(id))
    }

    /// Record that a test opened positions on these markets.
    pub fn record_open(&mut self, market_ids: &[String], test_id: &str) {
        for id in market_ids {
            self.occupied.insert(id.clone(), test_id.to_string());
        }
    }

    /// Record that positions on these markets were closed.
    pub fn record_close(&mut self, market_ids: &[String]) {
        for id in market_ids {
            self.occupied.remove(id);
        }
    }

    /// Get all occupied market IDs.
    pub fn occupied_market_ids(&self) -> HashSet<String> {
        self.occupied.keys().cloned().collect()
    }

    /// Get owner test_id for a market.
    pub fn owner(&self, market_id: &str) -> Option<&str> {
        self.occupied.get(market_id).map(|s| s.as_str())
    }
}
