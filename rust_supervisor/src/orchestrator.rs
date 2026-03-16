//! Orchestrator — the heart of the trading system.
//!
//! Replaces Python `trading_engine.py`. Startup sequence + event loop:
//!   1. Load markets (scanner: cache-first, then background API refresh)
//!   2. Detect constraints in Rust
//!   3. Start WS + dashboard
//!   4. Load state from SQLite
//!   5. Check API for missed resolutions
//!   6. Event loop: evaluate → rank → enter/replace → periodic tasks

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use rust_engine::detect::{DetectableMarket, DetectionConfig};
use rust_engine::eval::Opportunity;
use rust_engine::position;
use rust_engine::resolution::ResolutionValidator;
use rust_engine::postponement::PostponementDetector;
use rust_engine::scanner::MarketScanner;
use rust_engine::state::StateDB;
use rust_engine::TradingEngine;

// ---------------------------------------------------------------------------
// Delay model (ported from Python trading_engine.py)
// ---------------------------------------------------------------------------

const FALLBACK_P95: &[(&str, f64)] = &[
    ("football", 14.8), ("us_sports", 33.6), ("esports", 20.0), ("tennis", 20.8),
    ("mma_boxing", 50.3), ("cricket", 21.8), ("rugby", 23.3), ("politics", 350.2),
    ("gov_policy", 44.3), ("crypto", 3.4), ("sports_props", 6.5), ("other", 33.5),
];
const FALLBACK_DEFAULT: f64 = 33.5;

/// Maximum latency samples to retain for percentile calculation.
const MAX_LATENCY_SAMPLES: usize = 200;
/// Proactive exit multiplier — sell if net_proceeds >= this × resolution_payout.
const PROACTIVE_EXIT_MULTIPLIER: f64 = 1.2;

/// Dynamic capital: % of total portfolio value, floor $10, cap $1000.
fn dynamic_capital(total_value: f64, pct: f64) -> f64 {
    total_value.mul_add(pct, 0.0).max(10.0).min(1000.0)
}

/// Score an opportunity: profit_pct / effective_hours.
fn score_opportunity(
    opp: &Opportunity,
    p95_table: &HashMap<String, f64>,
    default_p95: f64,
    min_resolution_secs: f64,
    max_hours: f64,
) -> Option<(f64, f64)> {
    let hours = opp.hours_to_resolve;
    if hours < 0.0 { return None; }
    if hours * 3600.0 < min_resolution_secs { return None; }
    if hours > max_hours { return None; }

    let category = rust_engine::types::classify_category(&opp.market_names);
    let p95_delay = p95_table.get(category).copied().unwrap_or(default_p95);
    let effective_hours = hours + p95_delay;
    let score = opp.expected_profit_pct / effective_hours.max(0.01);
    Some((score, hours))
}

/// Rank opportunities by score. Returns (score, hours, index_into_opps).
fn rank_opportunities(
    opps: &[Opportunity],
    p95_table: &HashMap<String, f64>,
    default_p95: f64,
    min_resolution_secs: f64,
    max_days: u32,
) -> Vec<(f64, f64, usize)> {
    let max_hours = max_days as f64 * 24.0;
    let mut scored: Vec<(f64, f64, usize)> = opps.iter().enumerate()
        .filter_map(|(i, opp)| {
            let (score, hours) = score_opportunity(opp, p95_table, default_p95, min_resolution_secs, max_hours)?;
            Some((score, hours, i))
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Orchestrator config (extracted from config.yaml).
pub struct OrchestratorConfig {
    pub workspace: PathBuf,
    pub mode: String,
    pub shadow_only: bool,
    pub dashboard_port: u16,

    // Arbitrage
    pub max_positions: usize,
    pub max_days_entry: u32,
    pub max_days_replace: u32,
    pub capital_pct: f64,
    pub min_trade_size: f64,
    pub min_resolution_secs: f64,
    pub replacement_cooldown_secs: f64,

    // Engine timing
    pub state_save_interval: f64,
    pub monitor_interval: f64,
    pub constraint_rebuild_interval: f64,
    pub max_evals_per_batch: usize,
    pub stats_log_interval: f64,
    pub stale_sweep_interval: f64,
    pub stale_asset_threshold: f64,

    // AI
    pub resolution_validation_enabled: bool,
    pub anthropic_api_key: String,
    pub postponement_enabled: bool,
    pub postponement_check_interval: f64,
    pub postponement_rescore_days: u32,
    pub state_db_path: PathBuf,
}

impl OrchestratorConfig {
    /// Load from config.yaml + secrets.yaml at workspace path.
    pub fn load(workspace: &Path) -> Self {
        let config_path = workspace.join("config").join("config.yaml");
        let secrets_path = workspace.join("config").join("secrets.yaml");

        let yaml: serde_json::Value = std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|s| serde_yaml_ng::from_str(&s).ok())
            .unwrap_or_default();

        let secrets: serde_json::Value = std::fs::read_to_string(&secrets_path)
            .ok()
            .and_then(|s| serde_yaml_ng::from_str(&s).ok())
            .unwrap_or_default();

        let arb = yaml.get("arbitrage").cloned().unwrap_or_default();
        let eng = yaml.get("engine").cloned().unwrap_or_default();
        let live_cfg = yaml.get("live_trading").cloned().unwrap_or_default();
        let ai = yaml.get("ai").cloned().unwrap_or_default();

        let shadow_only = live_cfg.get("shadow_only").and_then(|v| v.as_bool()).unwrap_or(true);
        let mode_str = yaml.get("mode").and_then(|v| v.as_str()).unwrap_or("dual");

        // API key: secrets > config > env
        let api_key = secrets.pointer("/resolution_validation/anthropic_api_key")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
            .unwrap_or_default();

        let rv_cfg = arb.get("resolution_validation").cloned().unwrap_or_default();
        let pp_cfg = ai.get("postponement").cloned().unwrap_or_default();

        Self {
            workspace: workspace.to_path_buf(),
            mode: mode_str.to_string(),
            shadow_only,
            dashboard_port: yaml.pointer("/dashboard/port")
                .and_then(|v| v.as_u64()).unwrap_or(5556) as u16,

            max_positions: arb.get("max_concurrent_positions")
                .and_then(|v| v.as_u64()).unwrap_or(20) as usize,
            max_days_entry: arb.get("max_days_to_resolution")
                .and_then(|v| v.as_u64()).unwrap_or(60) as u32,
            max_days_replace: arb.get("max_days_to_replacement")
                .and_then(|v| v.as_u64()).unwrap_or(30) as u32,
            capital_pct: arb.get("capital_per_trade_pct")
                .and_then(|v| v.as_f64()).unwrap_or(0.10),
            min_trade_size: arb.get("min_trade_size")
                .and_then(|v| v.as_f64()).unwrap_or(10.0),
            min_resolution_secs: arb.get("min_resolution_time_secs")
                .and_then(|v| v.as_f64()).unwrap_or(300.0),
            replacement_cooldown_secs: arb.get("replacement_cooldown_seconds")
                .and_then(|v| v.as_f64()).unwrap_or(60.0),

            state_save_interval: eng.get("state_save_interval_seconds")
                .and_then(|v| v.as_f64()).unwrap_or(30.0),
            monitor_interval: eng.get("monitor_interval_seconds")
                .and_then(|v| v.as_f64()).unwrap_or(30.0),
            constraint_rebuild_interval: eng.get("constraint_rebuild_interval_seconds")
                .and_then(|v| v.as_f64()).unwrap_or(600.0),
            max_evals_per_batch: eng.get("max_evals_per_batch")
                .and_then(|v| v.as_u64()).unwrap_or(500) as usize,
            stats_log_interval: eng.get("stats_log_interval_seconds")
                .and_then(|v| v.as_f64()).unwrap_or(30.0),
            stale_sweep_interval: eng.get("stale_sweep_interval_seconds")
                .and_then(|v| v.as_f64()).unwrap_or(60.0),
            stale_asset_threshold: eng.get("stale_asset_threshold_seconds")
                .and_then(|v| v.as_f64()).unwrap_or(30.0),

            resolution_validation_enabled: rv_cfg.get("enabled")
                .and_then(|v| v.as_bool()).unwrap_or(true),
            anthropic_api_key: api_key,
            postponement_enabled: pp_cfg.get("enabled")
                .and_then(|v| v.as_bool()).unwrap_or(false),
            postponement_check_interval: pp_cfg.get("check_interval_hours")
                .and_then(|v| v.as_f64()).unwrap_or(24.0) * 3600.0,
            postponement_rescore_days: pp_cfg.get("postponement_rescore_days")
                .and_then(|v| v.as_u64()).unwrap_or(14) as u32,
            state_db_path: yaml.pointer("/state/db_path")
                .and_then(|v| v.as_str())
                .map(|s| workspace.join(s))
                .unwrap_or_else(|| workspace.join("data").join("system_state").join("execution_state.db")),
        }
    }
}

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

pub struct Orchestrator {
    cfg: OrchestratorConfig,
    engine: TradingEngine,
    scanner: Arc<MarketScanner>,
    state_db: Arc<StateDB>,
    resolution_validator: Option<Arc<ResolutionValidator>>,
    postponement_detector: Option<Arc<PostponementDetector>>,

    // Market data cache: market_id → JSON value
    market_lookup: HashMap<String, serde_json::Value>,

    // Parsed config.yaml cached at startup (avoids re-reading from disk)
    cached_yaml: serde_json::Value,

    // Timing state
    last_state_save: f64,
    last_monitor: f64,
    last_replacement: f64,
    last_constraint_rebuild: f64,
    last_stats_log: f64,
    last_stale_sweep: f64,
    last_postponement_check: f64,
    iteration: u64,
    recent_latencies: Vec<f64>,

    // Delay table
    p95_table: HashMap<String, f64>,
    p95_default: f64,

    // Background disk save thread handle
    disk_save_handle: Option<std::thread::JoinHandle<()>>,
}

impl Orchestrator {
    /// Create a new orchestrator from config.
    pub fn new(cfg: OrchestratorConfig) -> Result<Self, String> {
        let ws = cfg.workspace.to_string_lossy().to_string();

        let engine = TradingEngine::new(&ws)?;

        let scanner = MarketScanner::new(
            &cfg.workspace.join("data").join("markets.db").to_string_lossy(),
            Some(&cfg.workspace.join("data").join("latest_markets.json").to_string_lossy()),
        )?;

        let state_db = StateDB::new(
            &cfg.state_db_path.to_string_lossy(),
        )?;

        let resolution_validator = if cfg.resolution_validation_enabled && !cfg.anthropic_api_key.is_empty() {
            match ResolutionValidator::new(&ws, &cfg.anthropic_api_key) {
                Ok(rv) => Some(rv),
                Err(e) => {
                    tracing::warn!("Resolution validator init failed: {}", e);
                    None
                }
            }
        } else {
            None
        };

        let postponement_detector = if cfg.postponement_enabled && !cfg.anthropic_api_key.is_empty() {
            match PostponementDetector::new(&ws, &cfg.anthropic_api_key) {
                Ok(pd) => Some(pd),
                Err(e) => {
                    tracing::warn!("Postponement detector init failed: {}", e);
                    None
                }
            }
        } else {
            None
        };

        // Load delay table from SQLite
        let delay_rows = state_db.get_delay_table();
        let (p95_table, p95_default) = if !delay_rows.is_empty() {
            let table: HashMap<String, f64> = delay_rows.into_iter().collect();
            let default = table.get("other").copied().unwrap_or(FALLBACK_DEFAULT);
            (table, default)
        } else {
            let table: HashMap<String, f64> = FALLBACK_P95.iter()
                .map(|(k, v)| (k.to_string(), *v))
                .collect();
            (table, FALLBACK_DEFAULT)
        };

        // Cache parsed config.yaml at startup
        let config_path = cfg.workspace.join("config").join("config.yaml");
        let cached_yaml: serde_json::Value = std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|s| serde_yaml_ng::from_str(&s).ok())
            .unwrap_or_default();

        Ok(Self {
            cfg, engine,
            scanner: Arc::new(scanner),
            state_db: Arc::new(state_db),
            resolution_validator: resolution_validator.map(Arc::new),
            postponement_detector: postponement_detector.map(Arc::new),
            market_lookup: HashMap::new(),
            cached_yaml,
            last_state_save: 0.0,
            last_monitor: 0.0,
            last_replacement: 0.0,
            last_constraint_rebuild: 0.0,
            last_stats_log: 0.0,
            last_stale_sweep: 0.0,
            last_postponement_check: 0.0,
            iteration: 0,
            recent_latencies: Vec::new(),
            p95_table, p95_default,
            disk_save_handle: None,
        })
    }

    /// Run the full startup + event loop. Blocks until `running` is set to false.
    pub fn run(&mut self, running: Arc<AtomicBool>) {
        tracing::info!("Orchestrator starting...");

        // 1. Load markets: cache-first, then refresh
        self.load_markets_cached();
        tracing::info!("Loaded {} markets from cache", self.market_lookup.len());

        // Background refresh
        match self.scanner.scan() {
            Ok(result) => {
                self.ingest_scan_result(&result);
                tracing::info!("Scanner refresh: {} markets", result.count);
            }
            Err(e) => tracing::warn!("Scanner refresh failed (using cache): {}", e),
        }

        // 2. Detect constraints
        let all_assets = self.detect_constraints();

        // 3. Load delay table into engine
        self.engine.set_delay_table(
            self.p95_table.iter().map(|(k, v)| (k.clone(), *v)).collect()
        );

        // 4. Start WS + dashboard
        if !all_assets.is_empty() {
            self.engine.start(all_assets, self.cfg.dashboard_port);
            tracing::info!("WS engine + dashboard started (port {})", self.cfg.dashboard_port);
        }

        // 5. Load state from SQLite
        self.load_state();

        // 6. Check API for missed resolutions
        if self.cfg.shadow_only {
            let results = self.engine.check_api_resolutions();
            if !results.is_empty() {
                for r in &results {
                    tracing::info!("API resolution: {} → winner={}, profit=${:.4}",
                        r.position_id, r.winning_market_id, r.profit);
                }
            }
        }

        let mode_str = if self.cfg.shadow_only { "SHADOW" } else { "LIVE" };
        tracing::info!("Orchestrator ready [{}] — entering event loop", mode_str);

        // 7. Event loop
        while running.load(Ordering::Relaxed) {
            self.iteration += 1;
            let now = now_secs();

            if let Err(e) = self.tick(now) {
                tracing::error!("[iter {}] Error: {}", self.iteration, e);
            }

            // Wait up to 50ms for urgent work — wakes instantly on EFP drift
            // (matches Python's asyncio.wait_for(_eval_wake.wait(), timeout=0.05))
            self.engine.eval_queue.wait_for_work(std::time::Duration::from_millis(50));
        }

        // Shutdown — wait for any in-flight disk save, then do a final sync save
        tracing::info!("Orchestrator shutting down...");
        self.engine.stop();
        if let Some(h) = self.disk_save_handle.take() {
            let _ = h.join();
        }
        self.save_state();
        // Wait for the final save to complete before exiting
        if let Some(h) = self.disk_save_handle.take() {
            let _ = h.join();
        }
        tracing::info!("Orchestrator stopped.");
    }

    /// Single tick of the event loop.
    fn tick(&mut self, now: f64) -> Result<(), String> {
        // --- Process WS resolution events ---
        let resolved = self.engine.drain_resolved();
        if !resolved.is_empty() {
            let events: Vec<(String, String)> = resolved.iter()
                .map(|r| (r.market_cid.clone(), r.asset_id.clone()))
                .collect();
            let closed = self.engine.resolve_by_ws_events(&events);
            for (pid, winner) in &closed {
                tracing::info!("WS RESOLUTION: {} → winner={}", pid, winner);
            }
        }

        // --- Evaluate batch ---
        let (held_cids, held_mids) = self.engine.get_held_ids();

        let t0 = Instant::now();
        let result = self.engine.evaluate_batch(
            self.cfg.max_evals_per_batch,
            &held_cids, &held_mids, 20,
        );
        let batch_us = t0.elapsed().as_micros() as f64;

        if result.n_evaluated > 0 {
            self.recent_latencies.push(batch_us);
            if self.recent_latencies.len() > MAX_LATENCY_SAMPLES {
                self.recent_latencies = self.recent_latencies[self.recent_latencies.len()-MAX_LATENCY_SAMPLES..].to_vec();
            }
        }

        if !result.opportunities.is_empty() {
            self.try_enter_or_replace(&result.opportunities, &held_cids, &held_mids);
        }

        // --- Monitor (proactive exits) ---
        if (now - self.last_monitor) >= self.cfg.monitor_interval {
            if self.engine.pm_open_count() > 0 {
                self.check_proactive_exits();
            }
            self.last_monitor = now;
        }

        // --- Postponement check ---
        if (now - self.last_postponement_check) >= self.cfg.postponement_check_interval {
            if self.engine.pm_open_count() > 0 && self.postponement_detector.is_some() {
                self.check_postponements();
            }
            self.last_postponement_check = now;
        }

        // --- Constraint rebuild ---
        if (now - self.last_constraint_rebuild) >= self.cfg.constraint_rebuild_interval {
            // Refresh markets in background, then rebuild constraints
            match self.scanner.scan() {
                Ok(result) => {
                    self.ingest_scan_result(&result);
                    let all_assets = self.detect_constraints();
                    if !all_assets.is_empty() {
                        self.engine.start(all_assets, 0); // port=0 skips dashboard restart
                    }
                }
                Err(e) => tracing::warn!("Constraint rebuild scan failed: {}", e),
            }
            self.last_constraint_rebuild = now;
        }

        // --- Save state ---
        if (now - self.last_state_save) >= self.cfg.state_save_interval {
            self.save_state();
            self.last_state_save = now;
        }

        // --- Stale sweep ---
        if (now - self.last_stale_sweep) >= self.cfg.stale_sweep_interval {
            let stale = self.engine.get_stale_assets(self.cfg.stale_asset_threshold);
            if !stale.is_empty() {
                tracing::debug!("Stale assets: {} > {:.0}s old", stale.len(), self.cfg.stale_asset_threshold);
            }
            self.last_stale_sweep = now;
        }

        // --- Stats ---
        if (now - self.last_stats_log) >= self.cfg.stats_log_interval {
            self.log_stats();
            self.last_stats_log = now;
        }

        Ok(())
    }

    // --- Market loading ---

    fn load_markets_cached(&mut self) {
        let cached = self.scanner.load_cached();
        if cached.count > 0 {
            self.ingest_scan_result(&cached);
        }
    }

    fn ingest_scan_result(&mut self, result: &rust_engine::scanner::ScanResult) {
        self.market_lookup.clear();
        for m in &result.markets {
            if let Some(mid) = m.get("market_id").and_then(|v| v.as_str()) {
                self.market_lookup.insert(mid.to_string(), m.clone());
            }
        }
    }

    // --- Constraint detection ---

    fn detect_constraints(&mut self) -> Vec<String> {
        if self.market_lookup.is_empty() {
            tracing::warn!("Cannot detect constraints: no markets loaded");
            return Vec::new();
        }

        let markets_for_detect: Vec<DetectableMarket> = self.market_lookup.values()
            .filter_map(|m| {
                let market_id = m.get("market_id")?.as_str()?.to_string();
                let question = m.get("question").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let yes_asset_id = m.get("yes_asset_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let no_asset_id = m.get("no_asset_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let meta = m.get("metadata").cloned().unwrap_or_default();
                let neg_risk = meta.get("negRisk").and_then(|v| v.as_bool()).unwrap_or(false);
                let neg_risk_market_id = meta.get("negRiskMarketID")
                    .and_then(|v| v.as_str()).unwrap_or("").to_string();

                let yes_price = m.get("outcome_prices")
                    .and_then(|op| {
                        op.get("Yes").or(op.get("yes")).or(op.get("true"))
                            .and_then(|v| v.as_f64())
                    })
                    .unwrap_or(0.5);

                let end_date_ts = parse_end_date_ts(m.get("end_date").and_then(|v| v.as_str()).unwrap_or(""));

                Some(DetectableMarket {
                    market_id, question,
                    yes_asset_id, no_asset_id,
                    neg_risk, neg_risk_market_id,
                    yes_price, end_date_ts,
                })
            })
            .collect();

        let yaml = &self.cached_yaml;
        let cd = yaml.get("constraint_detection").cloned().unwrap_or_default();

        let detect_config = DetectionConfig {
            min_price_sum: cd.get("min_price_sum").and_then(|v| v.as_f64()).unwrap_or(0.85),
            max_price_sum: cd.get("max_price_sum").and_then(|v| v.as_f64()).unwrap_or(1.15),
            min_markets: cd.get("min_markets").and_then(|v| v.as_u64()).unwrap_or(2) as usize,
        };

        let result = self.engine.detect_and_load_constraints(&markets_for_detect, &detect_config);

        // Update eval config with current capital
        let cap = dynamic_capital(self.engine.total_value(), self.cfg.capital_pct);
        let arb = yaml.get("arbitrage").cloned().unwrap_or_default();
        let fees = arb.get("fees").cloned().unwrap_or_default();
        self.engine.set_eval_config(
            cap,
            fees.get("polymarket_taker_fee").and_then(|v| v.as_f64()).unwrap_or(0.0001),
            arb.get("min_profit_threshold").and_then(|v| v.as_f64()).unwrap_or(0.03),
            arb.get("max_profit_threshold").and_then(|v| v.as_f64()).unwrap_or(0.30),
        );

        tracing::info!("Constraints: {} detected, {} assets (capital=${:.2})",
            result.constraints.len(), result.all_asset_ids.len(), cap);

        result.all_asset_ids
    }

    // --- State management ---

    fn load_state(&mut self) {
        // Restore in-memory DB from disk file
        match self.state_db.load_from_disk() {
            Ok(ms) => tracing::info!("SQLite restored from disk in {:.1}ms", ms),
            Err(e) => tracing::warn!("No disk state to restore: {}", e),
        }

        // Load positions from SQLite
        let open_jsons = self.state_db.load_open();
        let closed_jsons = self.state_db.load_closed();

        let cfg_initial = self.cached_yaml.pointer("/live_trading/initial_capital")
            .and_then(|v| v.as_f64())
            .unwrap_or(100.0);
        let capital = self.state_db.get_scalar("current_capital").unwrap_or(cfg_initial);
        let initial = self.state_db.get_scalar("initial_capital").unwrap_or(cfg_initial);

        let fee_rate = self.cached_yaml.pointer("/arbitrage/fees/polymarket_taker_fee")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0001);

        self.engine.init_positions(initial, fee_rate);
        self.engine.import_positions(&open_jsons, &closed_jsons, capital, initial);

        tracing::info!("State loaded: ${:.2} capital, {} open, {} closed",
            self.engine.current_capital(),
            self.engine.pm_open_count(),
            self.engine.pm_closed_count());
    }

    fn save_state(&mut self) {
        let t0 = Instant::now();

        let cap = self.engine.current_capital();
        let init_cap = self.engine.initial_capital();
        let perf = self.engine.get_performance_metrics();

        self.state_db.set_scalars(&[
            ("current_capital".to_string(), cap),
            ("initial_capital".to_string(), init_cap),
        ]);
        for key in &["total_trades", "winning_trades", "losing_trades",
                     "total_actual_profit", "total_expected_profit"] {
            if let Some(&val) = perf.get(*key) {
                self.state_db.set_scalar(key, val);
            }
        }

        // Sync open positions — extract typed data + JSON under one lock
        let (live_ids, open_rows, n_closed_total, closed_rows_data) = {
            let pm = self.engine.positions.lock();
            let live_ids: HashSet<String> = pm.open_positions().keys().cloned().collect();
            let open_rows: Vec<(String, String, String, Option<String>, Option<String>)> =
                pm.open_positions().values()
                .filter_map(|p| {
                    let j = serde_json::to_string(p).ok()?;
                    Some((p.position_id.clone(), "open".to_string(), j, Some(p.entry_timestamp.clone()), None))
                })
                .collect();
            let n_closed_total = pm.closed_count();
            let closed_data: Vec<(String, String, Option<String>, Option<String>)> =
                pm.closed_positions().iter()
                .map(|p| (
                    p.position_id.clone(),
                    p.entry_timestamp.clone(),
                    p.resolved_at.clone(),
                    serde_json::to_string(p).ok(),
                ))
                .collect();
            (live_ids, open_rows, n_closed_total, closed_data)
        };

        let db_open_ids: HashSet<String> = self.state_db.get_open_position_ids().into_iter().collect();
        for stale_id in db_open_ids.difference(&live_ids) {
            self.state_db.delete_position(stale_id);
        }

        if !open_rows.is_empty() {
            self.state_db.save_positions_bulk(&open_rows);
        }

        // Sync closed (incremental)
        let counts = self.state_db.count_by_status();
        let db_closed = counts.iter().find(|(s, _)| s == "closed").map(|(_, c)| *c).unwrap_or(0) as usize;
        debug_assert!(n_closed_total >= db_closed, "closed positions shrunk: {} < {}", n_closed_total, db_closed);
        if closed_rows_data.len() > db_closed {
            let new_rows: Vec<(String, String, String, Option<String>, Option<String>)> =
                closed_rows_data[db_closed..].iter()
                    .filter_map(|(pid, entry_ts, resolved_at, json_opt)| {
                        let j = json_opt.as_ref()?.clone();
                        Some((pid.clone(), "closed".to_string(), j, Some(entry_ts.clone()), resolved_at.clone()))
                    })
                    .collect();
            if !new_rows.is_empty() {
                self.state_db.save_positions_bulk(&new_rows);
            }
        }

        // Disk mirrors run in a background thread to avoid blocking the tick loop.
        // Only spawn if previous save has completed.
        if self.disk_save_handle.as_ref().map_or(true, |h| h.is_finished()) {
            let db = Arc::clone(&self.state_db);
            let rv = self.resolution_validator.as_ref().map(Arc::clone);
            let pd = self.postponement_detector.as_ref().map(Arc::clone);
            let sc = Arc::clone(&self.scanner);
            let n_open = open_rows.len();
            self.disk_save_handle = Some(std::thread::spawn(move || {
                db.mirror_to_disk();
                if let Some(ref rv) = rv { rv.mirror_to_disk(); }
                if let Some(ref pd) = pd { pd.mirror_to_disk(); }
                sc.mirror_to_disk();
                let ms = t0.elapsed().as_millis();
                tracing::info!("State saved: {} open, {} closed [{ms}ms]",
                    n_open, n_closed_total);
            }));
        } else {
            tracing::debug!("Skipping disk mirror — previous save still running");
        }
    }

    // --- Entry / replacement ---

    fn try_enter_or_replace(&mut self, opportunities: &[Opportunity], held_cids: &HashSet<String>, held_mids: &HashSet<String>) {
        let ranked = rank_opportunities(
            opportunities, &self.p95_table, self.p95_default,
            self.cfg.min_resolution_secs, self.cfg.max_days_entry,
        );
        if ranked.is_empty() { return; }

        let mut slots = self.cfg.max_positions.saturating_sub(self.engine.pm_open_count());
        let cap = dynamic_capital(self.engine.total_value(), self.cfg.capital_pct);
        if cap < self.cfg.min_trade_size {
            slots = 0;
        }

        // --- Enter new positions ---
        let mut entered = 0usize;
        for &(score, hours, idx) in &ranked {
            if entered >= slots { break; }
            let opp = &opportunities[idx];

            if !self.validate_opportunity(opp, held_cids, held_mids) { continue; }

            // Scale to dynamic capital
            let old_cap = opp.optimal_bets.values().sum::<f64>();
            let scale = if old_cap > 0.0 { cap / old_cap } else { 1.0 };
            let scaled_bets: HashMap<String, f64> = opp.optimal_bets.iter()
                .map(|(k, v)| (k.clone(), v * scale))
                .collect();
            let scaled_profit = opp.expected_profit * scale;

            let meta = opp.method.clone();
            let is_sell = meta.to_lowercase().contains("sell");

            match self.engine.enter_position(
                &opp.constraint_id, &opp.constraint_id,
                if is_sell { "arb_sell" } else { "arb_buy" }, &opp.method,
                &opp.market_ids, &opp.market_names,
                &opp.current_prices, &opp.current_no_prices,
                &scaled_bets, scaled_profit, opp.expected_profit_pct,
                is_sell,
            ) {
                position::EntryResult::Entered(_pos) => {
                    tracing::info!("ENTER: {}... | ${:.2} | exp ${:.2} | {:.1}h | score={:.6}",
                        &opp.constraint_id[..opp.constraint_id.len().min(30)],
                        cap, scaled_profit, hours, score);
                    entered += 1;
                }
                position::EntryResult::InsufficientCapital { available, required } => {
                    tracing::debug!("Insufficient capital: ${:.2} < ${:.2}", available, required);
                }
            }
        }

        // --- Replacement ---
        let now = now_secs();
        if slots == 0 && (now - self.last_replacement) >= self.cfg.replacement_cooldown_secs {
            let replace_ranked = rank_opportunities(
                opportunities, &self.p95_table, self.p95_default,
                self.cfg.min_resolution_secs, self.cfg.max_days_replace,
            );
            if replace_ranked.is_empty() { return; }

            // Find best untraded opportunity
            let best_new = replace_ranked.iter().find(|&&(_, _, idx)| {
                let opp = &opportunities[idx];
                let cid = &opp.constraint_id;
                !held_cids.contains(cid) && self.validate_opportunity(opp, held_cids, held_mids)
            });

            if let Some(&(best_score, best_hours, best_idx)) = best_new {
                let best_opp = &opportunities[best_idx];

                // Find worst held position using typed access
                let mut worst: Option<(String, f64)> = None;
                let mut worst_bids: HashMap<String, f64> = HashMap::new();

                {
                    let pm = self.engine.positions.lock();
                    for (pid, pos) in pm.open_positions() {
                        let bids = self.get_position_bids_typed(pos);
                        let repl_profit = best_opp.expected_profit;

                        if let Some(eval) = pm.evaluate_replacement(pid, &bids, repl_profit) {
                            let total_cap = pos.total_capital;
                            let remaining_upside = pos.expected_profit - eval.liquidation.profit;
                            // Use a rough hours estimate
                            let hours_rem = 24.0_f64; // simplified
                            let rem_score = (remaining_upside / total_cap.max(0.01)) / hours_rem.max(0.01);

                            if worst.is_none() || rem_score < worst.as_ref().unwrap().1 {
                                if eval.worth_replacing {
                                    worst_bids = bids;
                                    worst = Some((pid.clone(), rem_score));
                                }
                            }
                        }
                    }
                }

                if let Some((worst_pid, worst_score)) = worst {
                    if best_score > worst_score * PROACTIVE_EXIT_MULTIPLIER {
                        // Execute replacement
                        if let Some((net, profit)) = self.engine.liquidate_position(&worst_pid, "replaced", &worst_bids) {
                            tracing::info!("REPLACE: liquidated {} → freed ${:.2}, profit=${:+.2}",
                                &worst_pid[..worst_pid.len().min(30)], net, profit);

                            // Enter replacement
                            let cap = dynamic_capital(self.engine.total_value(), self.cfg.capital_pct);
                            let old_cap = best_opp.optimal_bets.values().sum::<f64>();
                            let scale = if old_cap > 0.0 { cap / old_cap } else { 1.0 };
                            let scaled_bets: HashMap<String, f64> = best_opp.optimal_bets.iter()
                                .map(|(k, v)| (k.clone(), v * scale))
                                .collect();
                            let is_sell = best_opp.method.to_lowercase().contains("sell");

                            let _ = self.engine.enter_position(
                                &best_opp.constraint_id, &best_opp.constraint_id,
                                if is_sell { "arb_sell" } else { "arb_buy" }, &best_opp.method,
                                &best_opp.market_ids, &best_opp.market_names,
                                &best_opp.current_prices, &best_opp.current_no_prices,
                                &scaled_bets, best_opp.expected_profit * scale,
                                best_opp.expected_profit_pct, is_sell,
                            );

                            self.last_replacement = now;
                            tracing::info!("  WITH: {}... | score={:.6} | {:.1}h",
                                &best_opp.constraint_id[..best_opp.constraint_id.len().min(30)],
                                best_score, best_hours);
                        }
                    }
                }
            }
        }
    }

    fn validate_opportunity(&self, opp: &Opportunity, held_cids: &HashSet<String>, held_mids: &HashSet<String>) -> bool {
        if held_cids.contains(&opp.constraint_id) { return false; }
        for mid in &opp.market_ids {
            if held_mids.contains(mid) { return false; }
        }

        // AI resolution validation
        if let Some(ref rv) = self.resolution_validator {
            let mid = opp.market_ids.first()
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);

            if let Some(validation) = rv.validate(&opp.constraint_id, mid) {
                if validation.has_unrepresented_outcome {
                    tracing::info!("SKIP (unrepresented outcome): {}...", &opp.constraint_id[..opp.constraint_id.len().min(30)]);
                    return false;
                }
                if let Ok(vd) = chrono::NaiveDate::parse_from_str(&validation.latest_resolution_date, "%Y-%m-%d") {
                    let today = chrono::Utc::now().date_naive();
                    let days = (vd - today).num_days();
                    if days > self.cfg.max_days_entry as i64 {
                        tracing::debug!("SKIP (AI date): {} resolves in {}d > {}d",
                            &opp.constraint_id[..opp.constraint_id.len().min(30)],
                            days, self.cfg.max_days_entry);
                        return false;
                    }
                }
            }
        }

        true
    }

    /// Extract bid estimates from a typed Position (entry_price as bid proxy).
    fn get_position_bids_typed(&self, pos: &rust_engine::position::Position) -> HashMap<String, f64> {
        pos.markets.iter()
            .map(|(mid, leg)| (mid.clone(), leg.entry_price))
            .collect()
    }

    // --- Proactive exits ---

    fn check_proactive_exits(&self) {
        let all_bids = self.collect_all_position_bids();
        let exits = self.engine.check_proactive_exits(&all_bids, PROACTIVE_EXIT_MULTIPLIER);
        for exit in &exits {
            tracing::info!("PROACTIVE EXIT: {}... ratio={:.3}",
                &exit.position_id[..exit.position_id.len().min(40)], exit.ratio);
            let bids = self.get_position_bids_by_id(&exit.position_id);
            if let Some((net, profit)) = self.engine.liquidate_position(&exit.position_id, "proactive_exit", &bids) {
                tracing::info!("  Sold: freed ${:.2}, profit=${:+.2}", net, profit);
            }
        }
    }

    fn collect_all_position_bids(&self) -> HashMap<String, f64> {
        let pm = self.engine.positions.lock();
        let mut all_bids = HashMap::new();
        for pos in pm.open_positions().values() {
            all_bids.extend(self.get_position_bids_typed(pos));
        }
        all_bids
    }

    fn get_position_bids_by_id(&self, position_id: &str) -> HashMap<String, f64> {
        let pm = self.engine.positions.lock();
        match pm.get_position(position_id) {
            Some(pos) => self.get_position_bids_typed(pos),
            None => HashMap::new(),
        }
    }

    // --- Postponement ---

    fn check_postponements(&mut self) {
        let pd = match self.postponement_detector {
            Some(ref pd) => pd,
            None => return,
        };

        let now = chrono::Utc::now();

        // Collect position data under lock, then release
        let position_data: Vec<(String, Vec<String>, Vec<String>)> = {
            let pm = self.engine.positions.lock();
            pm.open_positions().values().map(|pos| {
                let pid = pos.position_id.clone();
                let market_ids: Vec<String> = pos.markets.keys().cloned().collect();
                let market_names: Vec<String> = pos.markets.values()
                    .map(|leg| leg.name.clone())
                    .collect();
                (pid, market_ids, market_names)
            }).collect()
        };

        let mut checked = 0;

        for (pid, market_ids, market_names) in &position_data {
            // Find end_date
            let mut expected_date = None;
            for mid in market_ids {
                if let Some(m) = self.market_lookup.get(mid) {
                    if let Some(ed) = m.pointer("/metadata/end_date").and_then(|v| v.as_str()) {
                        if ed.len() >= 10 {
                            expected_date = Some(ed[..10].to_string());
                            break;
                        }
                    }
                }
            }

            let expected_date = match expected_date {
                Some(d) => d,
                None => continue,
            };

            // Check if overdue (24h threshold)
            if let Ok(ed) = chrono::NaiveDate::parse_from_str(&expected_date, "%Y-%m-%d") {
                let hours_overdue = (now.date_naive() - ed).num_days() as f64 * 24.0;
                if hours_overdue < 24.0 { continue; }
            } else {
                continue;
            }

            if let Some(result) = pd.check(pid, market_names, &expected_date) {
                if result.effective_resolution_date.is_some() {
                    let display_name = market_names.first()
                        .map(|n| &n[..n.len().min(40)])
                        .unwrap_or("?");
                    tracing::info!("Postponement detected: {}... → {}",
                        display_name,
                        result.effective_resolution_date.as_deref().unwrap_or("?"));
                }
                checked += 1;
            }
        }

        if checked > 0 {
            tracing::info!("Postponement check: scanned {} overdue positions", checked);
        }
    }

    // --- Stats ---

    fn log_stats(&self) {
        let ws = self.engine.stats();
        let (q_urg, q_bg) = self.engine.queue_depths();
        let cap = self.engine.current_capital();
        let npos = self.engine.pm_open_count();

        let (lat_p50, lat_p95, lat_max) = if !self.recent_latencies.is_empty() {
            let mut lats = self.recent_latencies.clone();
            lats.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let p50 = lats[lats.len() / 2];
            let p95 = lats[(lats.len() as f64 * 0.95) as usize];
            let max = *lats.last().unwrap_or(&0.0);
            (p50, p95, max)
        } else {
            (0.0, 0.0, 0.0)
        };

        tracing::info!(
            "[iter {}] Capital=${:.2} positions={} | WS: subs={} msgs={} live={} urgent={} bg={} lat_μs p50={:.0} p95={:.0} max={:.0}",
            self.iteration, cap, npos,
            ws.subscribed, ws.total_msgs, ws.live_books,
            q_urg, q_bg, lat_p50, lat_p95, lat_max,
        );

        // Update dashboard metrics
        self.engine.update_dashboard_metrics(
            self.iteration,
            lat_p50 as u64, lat_p95 as u64, lat_max as u64,
            "running", &chrono::Utc::now().format("%d/%m/%Y %H:%M:%S").to_string(),
            "running", &chrono::Utc::now().format("%d/%m/%Y %H:%M:%S").to_string(),
        );
    }
}

// --- Helpers ---

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn parse_end_date_ts(s: &str) -> f64 {
    if s.is_empty() { return 0.0; }
    chrono::DateTime::parse_from_rfc3339(s)
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(&s.replace("Z", "+00:00")))
        .map(|dt| dt.timestamp() as f64)
        .unwrap_or(0.0)
}
