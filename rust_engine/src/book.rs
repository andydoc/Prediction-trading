/// Concurrent order book mirror using DashMap.
/// Thread-safe: WS tasks write, eval thread reads.
use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use crate::types::{OrderBook, BookLevel, OrderedFloat, EngineConfig};

/// Global book mirror — one entry per asset_id.
pub struct BookMirror {
    books: DashMap<String, OrderBook>,
    /// asset_id → last EFP (for drift detection)
    last_efp: DashMap<String, f64>,
    /// asset_id → constraint_ids that contain this asset
    asset_to_constraints: DashMap<String, Vec<String>>,
    /// Total WS messages received
    msg_count: AtomicU64,
    /// Config
    trade_size_usd: f64,
    efp_drift_threshold: f64,
}

impl BookMirror {
    pub fn new(config: &EngineConfig) -> Self {
        Self {
            books: DashMap::new(),
            last_efp: DashMap::new(),
            asset_to_constraints: DashMap::new(),
            msg_count: AtomicU64::new(0),
            trade_size_usd: config.trade_size_usd,
            efp_drift_threshold: config.efp_drift_threshold,
        }
    }

    /// Register asset→constraint index (called during setup).
    pub fn set_asset_index(&self, index: HashMap<String, Vec<String>>) {
        self.asset_to_constraints.clear();
        for (asset_id, cids) in index {
            self.asset_to_constraints.insert(asset_id, cids);
        }
    }

    /// Apply a full book snapshot for an asset.
    /// Returns list of (constraint_id, urgent) if EFP drifted.
    pub fn apply_snapshot(
        &self, asset_id: &str, asks: Vec<BookLevel>, bids: Vec<BookLevel>, ts: f64,
    ) -> Vec<(String, bool)> {
        self.msg_count.fetch_add(1, Ordering::Relaxed);
        let mut book = self.books.entry(asset_id.to_string()).or_default();
        book.apply_snapshot(asks, bids, ts);
        self.check_efp_drift(asset_id, &book)
    }

    /// Apply a delta update.
    /// Returns list of (constraint_id, urgent) if EFP drifted.
    pub fn apply_delta(
        &self, asset_id: &str, is_ask: bool, price: f64, new_size: f64, ts: f64,
    ) -> Vec<(String, bool)> {
        self.msg_count.fetch_add(1, Ordering::Relaxed);
        let mut book = self.books.entry(asset_id.to_string()).or_default();
        book.apply_delta(is_ask, price, new_size, ts);
        // Only check EFP on ask-side changes (that's what affects trade cost)
        if is_ask {
            self.check_efp_drift(asset_id, &book)
        } else {
            vec![]
        }
    }

    /// Apply best_bid_ask update (lightweight — just stores best prices).
    pub fn apply_best_prices(
        &self, asset_id: &str, best_bid: f64, best_ask: f64, ts: f64,
    ) -> Vec<(String, bool)> {
        self.msg_count.fetch_add(1, Ordering::Relaxed);
        // If we don't have a book yet, create one with a single level
        let mut book = self.books.entry(asset_id.to_string()).or_default();
        if book.asks.is_empty() && best_ask > 0.0 {
            book.asks.insert(OrderedFloat(best_ask), 100.0); // synthetic depth
        }
        if book.bids.is_empty() && best_bid > 0.0 {
            book.bids.insert(OrderedFloat(best_bid), 100.0);
        }
        book.last_update = ts;
        self.check_efp_drift(asset_id, &book)
    }

    /// Check if EFP has drifted enough to queue evals.
    fn check_efp_drift(&self, asset_id: &str, book: &OrderBook) -> Vec<(String, bool)> {
        let new_efp = book.effective_fill_price(self.trade_size_usd);
        if new_efp <= 0.0 {
            return vec![];
        }

        let old_efp = self.last_efp.get(asset_id).map(|v| *v).unwrap_or(0.0);
        let drift = (new_efp - old_efp).abs();

        if drift > self.efp_drift_threshold || old_efp == 0.0 {
            self.last_efp.insert(asset_id.to_string(), new_efp);
            // Look up which constraints this asset belongs to
            if let Some(cids) = self.asset_to_constraints.get(asset_id) {
                let urgent = drift > self.efp_drift_threshold;
                return cids.iter().map(|c| (c.clone(), urgent)).collect();
            }
        }
        vec![]
    }

    /// Get EFP for an asset.
    pub fn get_efp(&self, asset_id: &str) -> f64 {
        self.books.get(asset_id)
            .map(|b| b.effective_fill_price(self.trade_size_usd))
            .unwrap_or(0.0)
    }

    /// Get best ask for an asset.
    pub fn get_best_ask(&self, asset_id: &str) -> f64 {
        self.books.get(asset_id)
            .and_then(|b| b.best_ask())
            .unwrap_or(0.0)
    }

    /// Get best bid for an asset.
    pub fn get_best_bid(&self, asset_id: &str) -> f64 {
        self.books.get(asset_id)
            .and_then(|b| b.best_bid())
            .unwrap_or(0.0)
    }

    /// Count assets with book data.
    pub fn live_count(&self) -> usize {
        self.books.len()
    }

    /// Total messages processed.
    pub fn message_count(&self) -> u64 {
        self.msg_count.load(Ordering::Relaxed)
    }

    /// Get all ask prices+sizes for an asset (for Python compatibility).
    pub fn get_asks_vec(&self, asset_id: &str) -> (Vec<f64>, Vec<f64>) {
        match self.books.get(asset_id) {
            Some(book) => {
                let prices: Vec<f64> = book.asks.keys().map(|k| k.0).collect();
                let sizes: Vec<f64> = book.asks.values().copied().collect();
                (prices, sizes)
            }
            None => (vec![], vec![]),
        }
    }

    /// Get stale assets (book older than threshold).
    pub fn get_stale_assets(&self, max_age_secs: f64) -> Vec<String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        self.books.iter()
            .filter(|entry| now - entry.value().last_update > max_age_secs)
            .map(|entry| entry.key().clone())
            .collect()
    }
}
