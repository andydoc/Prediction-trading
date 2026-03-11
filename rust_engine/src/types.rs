/// Shared types for the Rust engine.
use std::collections::BTreeMap;

/// A single price level in the order book.
#[derive(Debug, Clone)]
pub struct BookLevel {
    pub price: f64,
    pub size: f64,
}

/// Local order book for a single asset (one side of a market).
/// Asks sorted ascending (best ask first), bids sorted descending (best bid first).
#[derive(Debug, Clone, Default)]
pub struct OrderBook {
    pub asks: BTreeMap<OrderedFloat, f64>,  // price → size
    pub bids: BTreeMap<OrderedFloat, f64>,  // price → size (reverse iter for best)
    pub last_update: f64,                    // unix timestamp
}

/// Wrapper for f64 that implements Ord (for BTreeMap keys).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OrderedFloat(pub f64);

impl Eq for OrderedFloat {}

impl PartialOrd for OrderedFloat {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderedFloat {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.partial_cmp(&other.0).unwrap_or(std::cmp::Ordering::Equal)
    }
}

impl std::hash::Hash for OrderedFloat {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.to_bits().hash(state);
    }
}

impl OrderBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Best ask price (lowest ask).
    pub fn best_ask(&self) -> Option<f64> {
        self.asks.keys().next().map(|k| k.0)
    }

    /// Best bid price (highest bid).
    pub fn best_bid(&self) -> Option<f64> {
        self.bids.keys().next_back().map(|k| k.0)
    }

    /// Compute Effective Fill Price (VWAP) walking the ask side at a given trade size.
    /// Returns 0.0 if insufficient depth.
    pub fn effective_fill_price(&self, trade_size_usd: f64) -> f64 {
        if self.asks.is_empty() || trade_size_usd <= 0.0 {
            return 0.0;
        }
        let mut remaining = trade_size_usd;
        let mut total_shares = 0.0;
        let mut total_cost = 0.0;

        for (&OrderedFloat(price), &size) in &self.asks {
            let level_usd = price * size;
            if level_usd <= 0.0 {
                continue;
            }
            if level_usd >= remaining {
                total_shares += remaining / price;
                total_cost += remaining;
                remaining = 0.0;
                break;
            } else {
                total_shares += size;
                total_cost += level_usd;
                remaining -= level_usd;
            }
        }
        if total_shares <= 0.0 || remaining > 0.0 {
            return 0.0;
        }
        total_cost / total_shares
    }

    /// Total ask depth in USD (with optional haircut for phantom orders).
    pub fn ask_depth_usd(&self, haircut: f64) -> f64 {
        let raw: f64 = self.asks.iter()
            .map(|(OrderedFloat(p), &s)| p * s)
            .sum();
        raw * haircut
    }

    /// Apply a full book snapshot (replaces all asks/bids).
    pub fn apply_snapshot(&mut self, asks: Vec<BookLevel>, bids: Vec<BookLevel>, ts: f64) {
        self.asks.clear();
        for l in asks {
            if l.size > 0.0 {
                self.asks.insert(OrderedFloat(l.price), l.size);
            }
        }
        self.bids.clear();
        for l in bids {
            if l.size > 0.0 {
                self.bids.insert(OrderedFloat(l.price), l.size);
            }
        }
        self.last_update = ts;
    }

    /// Apply a delta update to one side.
    pub fn apply_delta(&mut self, is_ask: bool, price: f64, new_size: f64, ts: f64) {
        let book = if is_ask { &mut self.asks } else { &mut self.bids };
        let key = OrderedFloat(price);
        if new_size <= 0.0 {
            book.remove(&key);
        } else {
            book.insert(key, new_size);
        }
        self.last_update = ts;
    }
}

/// Configuration passed from Python.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub ws_url: String,
    pub assets_per_shard: usize,
    pub heartbeat_interval_secs: u64,
    pub efp_drift_threshold: f64,
    pub efp_stale_secs: f64,
    pub trade_size_usd: f64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            ws_url: "wss://ws-subscriptions-clob.polymarket.com/ws/market".into(),
            assets_per_shard: 2000,
            heartbeat_interval_secs: 10,
            efp_drift_threshold: 0.005,
            efp_stale_secs: 5.0,
            trade_size_usd: 10.0,
        }
    }
}

/// Reason an eval was queued.
#[derive(Debug, Clone)]
pub enum EvalReason {
    EfpDrift { asset_id: String, old_efp: f64, new_efp: f64 },
    Stale { asset_id: String, stale_secs: f64 },
    Resolved { market_cid: String, asset_id: String },
}

/// Entry in the eval queue.
#[derive(Debug, Clone)]
pub struct EvalEntry {
    pub constraint_id: String,
    pub reason: EvalReason,
    pub queued_at: f64,
    pub urgent: bool,
}
