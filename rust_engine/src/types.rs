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
    pub last_efp: f64,                       // last effective fill price (for drift detection)
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
        self.0.total_cmp(&other.0)
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
            // M2: Guard against NaN/Inf from corrupt book entries
            if !price.is_finite() || !size.is_finite() { continue; }
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
    /// `haircut_factor` is a multiplier in 0.0..=1.0 (e.g. 0.5 = keep 50% of depth).
    pub fn ask_depth_usd(&self, haircut_factor: f64) -> f64 {
        debug_assert!((0.0..=1.0).contains(&haircut_factor), "haircut_factor out of range: {}", haircut_factor);
        let raw: f64 = self.asks.iter()
            .map(|(OrderedFloat(p), &s)| p * s)
            .sum();
        raw * haircut_factor
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

/// Engine configuration — loaded from config.yaml at startup.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    // websocket section
    pub ws_url: String,
    pub assets_per_shard: usize,
    pub heartbeat_interval_secs: u64,
    // engine section
    pub efp_drift_threshold: f64,
    pub efp_stale_secs: f64,
    // runtime (set dynamically, not from config)
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

/// Eval-related configuration — loaded from config.yaml at startup.
#[derive(Debug, Clone)]
pub struct EvalCfg {
    pub fee_rate: f64,
    pub min_profit_threshold: f64,
    pub max_profit_threshold: f64,
    /// Profit threshold above which arbs are flagged as suspicious (default 0.20 = 20%).
    /// Genuinely large arbs are extremely rare in liquid markets and may indicate
    /// detection errors (e.g., incomplete mutex groups). Flagged arbs are still
    /// entered but logged at WARN level and marked in the dashboard.
    pub suspicious_profit_threshold: f64,
    pub max_fw_iter: usize,
    pub max_hours: f64,
}

impl Default for EvalCfg {
    fn default() -> Self {
        Self {
            fee_rate: 0.0001,
            min_profit_threshold: 0.03,
            max_profit_threshold: 0.30,
            suspicious_profit_threshold: 0.20,
            max_fw_iter: 200,
            max_hours: 1440.0,
        }
    }
}

/// Position manager configuration — loaded from config.yaml at startup.
#[derive(Debug, Clone)]
pub struct PositionCfg {
    pub initial_capital: f64,
    pub taker_fee: f64,
}

impl Default for PositionCfg {
    fn default() -> Self {
        Self {
            initial_capital: 100.0,
            taker_fee: 0.0001,
        }
    }
}

/// Logging configuration — loaded from config.yaml at startup.
#[derive(Debug, Clone)]
pub struct LoggingCfg {
    /// Minimum tracing level: "trace", "debug", "info", "warn", "error"
    pub level: String,
    /// Directory for log files (relative to workspace)
    pub log_dir: String,
    /// Log file prefix (produces {prefix}.YYYY-MM-DD)
    pub file_prefix: String,
    /// Days to retain old log files (0 = keep forever)
    pub retention_days: u32,
}

impl Default for LoggingCfg {
    fn default() -> Self {
        Self {
            level: "debug".into(),
            log_dir: "logs".into(),
            file_prefix: "rust_engine".into(),
            retention_days: 30,
        }
    }
}

/// Load all engine config from config.yaml.
/// Falls back to defaults if file is missing or values are absent.
pub fn load_engine_config(workspace: &str) -> (EngineConfig, EvalCfg, PositionCfg, LoggingCfg) {
    let path = std::path::PathBuf::from(workspace).join("config").join("config.yaml");

    let val: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(contents) => serde_yaml_ng::from_str(&contents).unwrap_or_default(),
        Err(_) => {
            tracing::warn!("config.yaml not found at {:?}, using defaults", path);
            return (EngineConfig::default(), EvalCfg::default(), PositionCfg::default(), LoggingCfg::default());
        }
    };

    // --- websocket ---
    let ws_url = val.pointer("/websocket/market_channel_url")
        .and_then(|v| v.as_str())
        .unwrap_or("wss://ws-subscriptions-clob.polymarket.com/ws/market")
        .to_string();
    let assets_per_shard = val.pointer("/websocket/assets_per_shard")
        .and_then(|v| v.as_u64())
        .unwrap_or(2000) as usize;
    let heartbeat_interval_secs = val.pointer("/websocket/heartbeat_interval")
        .and_then(|v| v.as_u64())
        .unwrap_or(10);

    // --- engine ---
    let efp_drift_threshold = val.pointer("/engine/efp_drift_threshold")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.005);
    let efp_stale_secs = val.pointer("/engine/efp_staleness_seconds")
        .and_then(|v| v.as_f64())
        .unwrap_or(5.0);

    // --- arbitrage fees ---
    let fee_rate = val.pointer("/arbitrage/fees/trading_fee")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0001);
    let taker_fee = val.pointer("/arbitrage/fees/polymarket_taker_fee")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0001);

    // --- arbitrage thresholds ---
    let min_profit_threshold = val.pointer("/arbitrage/min_profit_threshold")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.03);
    let max_profit_threshold = val.pointer("/arbitrage/max_profit_threshold")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.30);
    let suspicious_profit_threshold = val.pointer("/arbitrage/suspicious_profit_threshold")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.20);
    let max_fw_iter = val.pointer("/arbitrage/optimization/max_iterations")
        .and_then(|v| v.as_u64())
        .unwrap_or(200) as usize;
    let max_days = val.pointer("/arbitrage/max_days_to_resolution")
        .and_then(|v| v.as_f64())
        .unwrap_or(60.0);
    let max_hours = max_days * 24.0;

    // --- live_trading / position ---
    let initial_capital = val.pointer("/live_trading/initial_capital")
        .and_then(|v| v.as_f64())
        .unwrap_or(100.0);

    let engine_cfg = EngineConfig {
        ws_url,
        assets_per_shard,
        heartbeat_interval_secs,
        efp_drift_threshold,
        efp_stale_secs,
        trade_size_usd: 10.0, // set dynamically at runtime via set_trade_size()
    };

    let eval_cfg = EvalCfg {
        fee_rate,
        min_profit_threshold,
        max_profit_threshold,
        suspicious_profit_threshold,
        max_fw_iter,
        max_hours,
    };

    let pos_cfg = PositionCfg {
        initial_capital,
        taker_fee,
    };

    // --- logging ---
    let log_level = val.pointer("/monitoring/logging/level")
        .and_then(|v| v.as_str())
        .unwrap_or("DEBUG")
        .to_lowercase();
    let log_dir = val.pointer("/monitoring/logging/log_dir")
        .and_then(|v| v.as_str())
        .unwrap_or("logs")
        .to_string();
    let file_prefix = val.pointer("/monitoring/logging/rust_file_prefix")
        .and_then(|v| v.as_str())
        .unwrap_or("rust_engine")
        .to_string();
    let retention_days = val.pointer("/monitoring/logging/retention")
        .and_then(|v| v.as_u64())
        .unwrap_or(30) as u32;

    let log_cfg = LoggingCfg {
        level: log_level,
        log_dir,
        file_prefix,
        retention_days,
    };

    tracing::info!(
        "Config loaded: ws_shard={}, efp_drift={}, fee={}, profit=[{:.2}%..{:.2}%] (suspicious>{:.0}%), max_days={}, capital={}",
        assets_per_shard, efp_drift_threshold, fee_rate,
        min_profit_threshold * 100.0, max_profit_threshold * 100.0,
        suspicious_profit_threshold * 100.0, max_days, initial_capital,
    );

    (engine_cfg, eval_cfg, pos_cfg, log_cfg)
}

/// Classify market names into a resolution-delay category.
pub fn classify_category(market_names: &[String]) -> &'static str {
    let combined: String = market_names.iter()
        .map(|n| n.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    let has = |patterns: &[&str]| patterns.iter().any(|p| combined.contains(p));

    if has(&["win on 20", "end in a draw", "halftime", "leading at halftime"]) { return "football"; }
    if has(&["nba ", "nfl ", "nhl ", "mlb ", "wnba ",
            "touchdown", "rushing yards", "passing yards",
            "rebounds", "three-pointer"]) { return "us_sports"; }
    if has(&["spread:", "team total:", "o/u "]) { return "sports_props"; }
    if has(&["counter-strike", "cs2", "dota", "league of legends",
            "valorant", "overwatch", "dreamleague"]) { return "esports"; }
    if has(&["atp ", "wta ", "tennis"]) { return "tennis"; }
    if has(&["ufc ", "mma ", "boxing", "pfl ", "bellator"]) { return "mma_boxing"; }
    if has(&["cricket", "ipl ", "t20 "]) { return "cricket"; }
    if has(&["rugby", "super rugby", "waratahs"]) { return "rugby"; }
    // Short-lived crypto price predictions (5-15 min) — resolve near-instantly on expiry
    if has(&["price at", "price above", "price below", "above or below",
            "up or down at", "btc above", "btc below", "eth above", "eth below",
            "sol above", "sol below", "bitcoin above", "bitcoin below",
            "ethereum above", "ethereum below"]) { return "crypto_price"; }
    if has(&["bitcoin", "ethereum", "solana", "btc ", "eth ", "up or down"]) { return "crypto"; }
    if has(&["governor", "congress", "senate", "primary",
            "democrat", "republican", "election", "president"]) { return "politics"; }
    if has(&["fed ", "federal reserve", "interest rate",
            "tariff", "government shutdown"]) { return "gov_policy"; }
    "other"
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
