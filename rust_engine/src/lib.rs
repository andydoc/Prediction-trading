/// Core trading engine library.
///
/// Provides all components for the prediction market trading system:
/// - WebSocket manager with sharded connections
/// - Order book mirror with EFP drift detection
/// - Constraint detection and evaluation
/// - Arbitrage math (mutex + polytope)
/// - Position management (entry, replacement, resolution, liquidation)
/// - Dashboard (axum HTTP + SSE)
/// - State persistence (SQLite in-memory + disk backup)
/// - Market scanner (Gamma API + SQLite cache)
/// - AI resolution validator (Anthropic API)
/// - AI postponement detector (Anthropic API + web search)

pub mod types;
pub mod book;
pub mod queue;
pub mod state;
pub mod arb;
pub mod eval;
pub mod position;
pub mod ws;
pub mod dashboard;
pub mod resolution;
pub mod postponement;
pub mod scanner;
pub mod detect;

use std::collections::HashMap;
use std::sync::Arc;

use book::BookMirror;
use queue::EvalQueue;
use eval::{ConstraintStore, EvalConfig, Constraint, MarketRef};
use types::{EngineConfig, LoggingCfg};
use ws::WsManager;
use position::PositionManager;
use dashboard::{DashboardState, EngineMetrics};

/// The core trading engine. Owns all hot-path state:
/// WS connections, order books, eval queue, positions, dashboard.
///
/// Previously exposed to Python via PyO3; now used directly by the
/// Rust supervisor binary.
pub struct TradingEngine {
    pub book: Arc<BookMirror>,
    pub eval_queue: Arc<EvalQueue>,
    pub ws: Arc<WsManager>,
    pub constraints: Arc<ConstraintStore>,
    pub eval_config: parking_lot::Mutex<EvalConfig>,
    pub runtime: tokio::runtime::Runtime,
    pub positions: Arc<parking_lot::Mutex<PositionManager>>,
    pub engine_metrics: Arc<parking_lot::Mutex<EngineMetrics>>,
    pub recent_opps: Arc<parking_lot::Mutex<Vec<serde_json::Value>>>,
    pub mode: String,
    pub start_time: chrono::DateTime<chrono::Utc>,
    pub delay_table: Arc<parking_lot::Mutex<(HashMap<String, f64>, f64)>>,
}

impl TradingEngine {
    /// Initialize tracing with daily rotating file + stderr output.
    pub fn init_tracing(workspace: &str, log_cfg: &LoggingCfg) {
        use tracing_subscriber::prelude::*;
        use tracing_subscriber::fmt;
        use tracing_subscriber::EnvFilter;

        let log_dir = std::path::PathBuf::from(workspace).join(&log_cfg.log_dir);
        let _ = std::fs::create_dir_all(&log_dir);

        let file_appender = tracing_appender::rolling::daily(&log_dir, &log_cfg.file_prefix);

        let filter_str = format!("rust_engine={},prediction_trader={}", log_cfg.level, log_cfg.level);
        let env_filter = EnvFilter::try_new(&filter_str)
            .unwrap_or_else(|_| EnvFilter::new("rust_engine=debug,prediction_trader=debug"));

        let file_layer = fmt::layer()
            .with_writer(file_appender)
            .with_target(false)
            .with_ansi(false);

        let stderr_layer = fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(false);

        let _ = tracing_subscriber::registry()
            .with(env_filter)
            .with(file_layer)
            .with(stderr_layer)
            .try_init();

        if log_cfg.retention_days > 0 {
            Self::cleanup_old_logs(&log_dir, &log_cfg.file_prefix, log_cfg.retention_days);
        }
    }

    fn cleanup_old_logs(log_dir: &std::path::Path, prefix: &str, retention_days: u32) {
        let cutoff = std::time::SystemTime::now()
            - std::time::Duration::from_secs(retention_days as u64 * 86400);

        if let Ok(entries) = std::fs::read_dir(log_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if !name_str.starts_with(prefix) {
                    continue;
                }
                if let Ok(meta) = entry.metadata() {
                    if let Ok(modified) = meta.modified() {
                        if modified < cutoff {
                            let _ = std::fs::remove_file(entry.path());
                        }
                    }
                }
            }
        }
    }

    /// Create a new engine. Reads all config from config.yaml at the given workspace path.
    pub fn new(workspace: &str) -> Result<Self, String> {
        // Install rustls crypto provider
        let _ = rustls::crypto::ring::default_provider().install_default();

        let (cfg, eval_cfg, pos_cfg, log_cfg) = types::load_engine_config(workspace);
        Self::init_tracing(workspace, &log_cfg);

        let book = Arc::new(BookMirror::new(&cfg));
        let eval_queue = Arc::new(EvalQueue::new());
        let positions = Arc::new(parking_lot::Mutex::new(
            PositionManager::new(pos_cfg.initial_capital, pos_cfg.taker_fee)
        ));
        let ws = Arc::new(WsManager::new(
            cfg.clone(), Arc::clone(&book), Arc::clone(&eval_queue),
            Arc::clone(&positions),
        ));
        let constraints = Arc::new(ConstraintStore::new());

        let eval_config = EvalConfig {
            capital: cfg.trade_size_usd,
            fee_rate: eval_cfg.fee_rate,
            min_profit_threshold: eval_cfg.min_profit_threshold,
            max_profit_threshold: eval_cfg.max_profit_threshold,
            max_fw_iter: eval_cfg.max_fw_iter,
            max_hours: eval_cfg.max_hours,
        };

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("rust-engine")
            .build()
            .map_err(|e| format!("Failed to create tokio runtime: {}", e))?;

        Ok(Self {
            book, eval_queue, ws, constraints,
            eval_config: parking_lot::Mutex::new(eval_config),
            runtime, positions,
            engine_metrics: Arc::new(parking_lot::Mutex::new(EngineMetrics::default())),
            recent_opps: Arc::new(parking_lot::Mutex::new(Vec::new())),
            mode: "shadow".to_string(),
            start_time: chrono::Utc::now(),
            delay_table: Arc::new(parking_lot::Mutex::new((HashMap::new(), 33.5))),
        })
    }

    /// Set the asset_id → constraint_id index.
    pub fn set_asset_index(&self, index: HashMap<String, Vec<String>>) {
        self.book.set_asset_index(index);
    }

    /// Start WS connections and dashboard server.
    pub fn start(&self, asset_ids: Vec<String>, dashboard_port: u16) {
        let ws = Arc::clone(&self.ws);
        self.runtime.spawn(async move {
            ws.start(asset_ids).await;
        });

        if dashboard_port > 0 {
            let dash_state = DashboardState {
                positions: Arc::clone(&self.positions),
                book: Arc::clone(&self.book),
                eval_queue: Arc::clone(&self.eval_queue),
                ws: Arc::clone(&self.ws),
                constraints: Arc::clone(&self.constraints),
                engine_metrics: Arc::clone(&self.engine_metrics),
                recent_opps: Arc::clone(&self.recent_opps),
                mode: self.mode.clone(),
                start_time: self.start_time,
                delay_table: Arc::clone(&self.delay_table),
            };
            self.runtime.spawn(async move {
                dashboard::start(dash_state, dashboard_port).await;
            });
        }
    }

    /// Stop all WS connections.
    pub fn stop(&self) {
        self.ws.stop();
    }

    /// Load constraint definitions directly.
    pub fn set_constraints(&self, constraints: Vec<Constraint>) {
        let n = constraints.len();
        self.constraints.set_constraints(constraints);
        tracing::info!("Loaded {} constraints into evaluator", n);
    }

    /// Detect constraints from market data, build indices, load into ConstraintStore.
    pub fn detect_and_load_constraints(
        &self,
        markets: &[detect::DetectableMarket],
        config: &detect::DetectionConfig,
    ) -> detect::DetectionResult {
        let result = detect::detect_constraints(markets, config);

        let n_constraints = result.constraints.len();

        // Load into ConstraintStore
        self.constraints.set_constraints(result.constraints.clone());

        // Set BookMirror asset→constraint index
        self.book.set_asset_index(result.asset_to_constraints.clone());

        // Set PositionManager asset→market index
        self.positions.lock().set_asset_index(result.asset_to_market.clone());

        // Ensure open position assets stay in subscription list
        let open_pos_assets = self.positions.lock().get_open_position_asset_ids();
        let mut all_asset_ids = result.all_asset_ids.clone();
        if !open_pos_assets.is_empty() {
            let asset_set: std::collections::HashSet<String> =
                all_asset_ids.iter().cloned().collect();
            let mut extra = 0usize;
            for aid in &open_pos_assets {
                if !asset_set.contains(aid) {
                    all_asset_ids.push(aid.clone());
                    extra += 1;
                }
            }
            if extra > 0 {
                tracing::info!("Added {} assets from open positions to WS subscription", extra);
            }
        }

        tracing::info!(
            "Detected {} constraints from {} markets ({} assets, {} groups, {} incomplete, {} overpriced)",
            n_constraints, result.n_markets_input, all_asset_ids.len(),
            result.n_groups, result.n_skipped_incomplete, result.n_skipped_overpriced,
        );

        // Return a modified result with the expanded asset list
        detect::DetectionResult {
            all_asset_ids,
            ..result
        }
    }

    /// Load P95 delay table.
    pub fn set_delay_table(&self, delays: Vec<(String, f64)>) {
        let mut table = HashMap::new();
        for (cat, p95) in &delays {
            table.insert(cat.clone(), *p95);
        }
        let default = table.get("other").copied().unwrap_or(33.5);
        *self.delay_table.lock() = (table, default);
    }

    /// Update eval config (called when capital changes).
    pub fn set_eval_config(&self, capital: f64, fee_rate: f64, min_profit: f64, max_profit: f64) {
        let mut ec = self.eval_config.lock();
        ec.capital = capital;
        ec.fee_rate = fee_rate;
        ec.min_profit_threshold = min_profit;
        ec.max_profit_threshold = max_profit;
        self.book.set_trade_size(capital);
    }

    /// Drain queue + read books + arb math → pre-ranked opportunities.
    pub fn evaluate_batch(
        &self,
        max_evals: usize,
        held_cids: &std::collections::HashSet<String>,
        held_mids: &std::collections::HashSet<String>,
        top_n: usize,
    ) -> EvalBatchResult {
        let ec = self.eval_config.lock().clone();
        let (opps, n_urg, n_bg, n_eval, n_held) = eval::evaluate_batch(
            &self.eval_queue, &self.book, &self.constraints, &ec,
            max_evals, held_cids, held_mids, top_n,
        );
        EvalBatchResult {
            opportunities: opps,
            n_urgent: n_urg,
            n_background: n_bg,
            n_evaluated: n_eval,
            n_skipped_held: n_held,
        }
    }

    /// Drain eval queue.
    pub fn drain_evals(&self, max: usize) -> Vec<queue::DrainResult> {
        self.eval_queue.drain(max)
    }

    pub fn get_efp(&self, asset_id: &str) -> f64 { self.book.get_efp(asset_id) }
    pub fn get_best_ask(&self, asset_id: &str) -> f64 { self.book.get_best_ask(asset_id) }
    pub fn get_best_bid(&self, asset_id: &str) -> f64 { self.book.get_best_bid(asset_id) }
    pub fn get_asks(&self, asset_id: &str) -> (Vec<f64>, Vec<f64>) { self.book.get_asks_vec(asset_id) }
    pub fn queue_depths(&self) -> (usize, usize) { self.eval_queue.depths() }
    pub fn get_stale_assets(&self, max_age_secs: f64) -> Vec<String> { self.book.get_stale_assets(max_age_secs) }

    pub fn drain_resolved(&self) -> Vec<ws::ResolvedEvent> {
        self.ws.drain_resolved()
    }

    pub fn stats(&self) -> ws::WsStats {
        self.ws.stats()
    }

    // === Position management ===

    pub fn init_positions(&self, initial_capital: f64, taker_fee: f64) {
        let mut pm = self.positions.lock();
        *pm = PositionManager::new(initial_capital, taker_fee);
    }

    pub fn set_trade_size(&self, trade_size_usd: f64) {
        self.eval_config.lock().capital = trade_size_usd;
        self.book.set_trade_size(trade_size_usd);
    }

    pub fn current_capital(&self) -> f64 { self.positions.lock().current_capital() }
    pub fn total_value(&self) -> f64 { self.positions.lock().total_value() }
    pub fn initial_capital(&self) -> f64 { self.positions.lock().initial_capital() }
    pub fn pm_open_count(&self) -> usize { self.positions.lock().open_count() }
    pub fn pm_closed_count(&self) -> usize { self.positions.lock().closed_count() }

    pub fn enter_position(
        &self,
        opportunity_id: &str, constraint_id: &str,
        strategy: &str, method: &str,
        market_ids: &[String], market_names: &[String],
        current_prices: &HashMap<String, f64>,
        current_no_prices: &HashMap<String, f64>,
        optimal_bets: &HashMap<String, f64>,
        expected_profit: f64, expected_profit_pct: f64,
        is_sell: bool,
    ) -> position::EntryResult {
        let end_date_ts = self.constraints.get(constraint_id)
            .map(|c| c.end_date_ts).unwrap_or(0.0);

        self.positions.lock().enter_position(
            opportunity_id, constraint_id, strategy, method,
            market_ids, market_names, current_prices, current_no_prices,
            optimal_bets, expected_profit, expected_profit_pct, is_sell,
            end_date_ts,
        )
    }

    pub fn close_on_resolution(&self, position_id: &str, winning_market_id: &str) -> Option<position::ResolutionEvent> {
        self.positions.lock().close_on_resolution(position_id, winning_market_id)
    }

    pub fn calculate_liquidation_value(
        &self, position_id: &str, current_bids: &HashMap<String, f64>,
    ) -> Option<position::LiquidationValue> {
        self.positions.lock().calculate_liquidation_value(position_id, current_bids)
    }

    pub fn evaluate_replacement(
        &self, position_id: &str, current_bids: &HashMap<String, f64>, replacement_profit: f64,
    ) -> Option<position::ReplacementEval> {
        self.positions.lock().evaluate_replacement(position_id, current_bids, replacement_profit)
    }

    pub fn check_proactive_exits(
        &self, current_bids: &HashMap<String, f64>, exit_multiplier: f64,
    ) -> Vec<position::ProactiveExit> {
        self.positions.lock().check_proactive_exits(current_bids, exit_multiplier)
    }

    pub fn liquidate_position(
        &self, position_id: &str, reason: &str, current_bids: &HashMap<String, f64>,
    ) -> Option<(f64, f64)> {
        self.positions.lock().liquidate_position(position_id, reason, current_bids)
    }

    pub fn set_resolution_index(&self, index: HashMap<String, (String, bool)>) {
        self.positions.lock().set_asset_index(index);
    }

    pub fn resolve_by_ws_events(&self, events: &[(String, String)]) -> Vec<(String, String)> {
        self.positions.lock().resolve_by_ws_events(events)
    }

    pub fn get_held_constraint_ids(&self) -> std::collections::HashSet<String> {
        self.positions.lock().get_held_constraint_ids()
    }

    pub fn get_held_market_ids(&self) -> std::collections::HashSet<String> {
        self.positions.lock().get_held_market_ids()
    }

    pub fn get_open_position_asset_ids(&self) -> Vec<String> {
        self.positions.lock().get_open_position_asset_ids()
    }

    pub fn get_open_positions_json(&self) -> Vec<String> {
        self.positions.lock().get_open_positions_json()
    }

    pub fn get_closed_positions_json(&self) -> Vec<String> {
        self.positions.lock().get_closed_positions_json()
    }

    pub fn get_open_position_ids(&self) -> Vec<String> {
        self.positions.lock().get_open_position_ids()
    }

    pub fn get_performance_metrics(&self) -> HashMap<String, f64> {
        self.positions.lock().get_performance_metrics()
    }

    pub fn import_positions(
        &self, open_json: &[String], closed_json: &[String],
        capital: f64, initial_capital: f64,
    ) {
        self.positions.lock().import_positions_json(open_json, closed_json, capital, initial_capital);
    }

    /// Check Polymarket API for missed resolutions (catches WS gaps).
    pub fn check_api_resolutions(&self) -> Vec<ApiResolution> {
        let positions_json = self.positions.lock().get_open_positions_json();
        if positions_json.is_empty() {
            return Vec::new();
        }

        tracing::info!("Checking API resolution for {} open positions...", positions_json.len());

        let client = match reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .user_agent("Mozilla/5.0 (prediction-trader)")
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("HTTP client error: {}", e);
                return Vec::new();
            }
        };

        let mut results = Vec::new();

        for pj in &positions_json {
            let p: serde_json::Value = match serde_json::from_str(pj) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let pid = p["position_id"].as_str().unwrap_or("");
            let markets = match p["markets"].as_object() {
                Some(m) => m,
                None => continue,
            };

            let mut resolved_outcomes: HashMap<String, String> = HashMap::new();
            let mut all_resolved = true;

            for mid in markets.keys() {
                let url = format!("https://gamma-api.polymarket.com/markets/{}", mid);
                match client.get(&url).send() {
                    Ok(resp) => {
                        if let Ok(mdata) = resp.json::<serde_json::Value>() {
                            let is_closed = mdata["closed"].as_bool().unwrap_or(false);
                            if !is_closed {
                                all_resolved = false;
                                continue;
                            }

                            let prices_raw = &mdata["outcomePrices"];
                            let prices: Vec<f64> = if let Some(s) = prices_raw.as_str() {
                                serde_json::from_str(s).unwrap_or_default()
                            } else if let Some(arr) = prices_raw.as_array() {
                                arr.iter().filter_map(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).collect()
                            } else {
                                Vec::new()
                            };

                            if prices.len() >= 2 {
                                if prices[0] >= 0.99 && prices[1] <= 0.01 {
                                    resolved_outcomes.insert(mid.clone(), "yes".into());
                                } else if prices[1] >= 0.99 && prices[0] <= 0.01 {
                                    resolved_outcomes.insert(mid.clone(), "no".into());
                                } else {
                                    all_resolved = false;
                                }
                            } else {
                                all_resolved = false;
                            }
                        } else {
                            all_resolved = false;
                        }
                    }
                    Err(e) => {
                        tracing::debug!("API check failed for market {}: {}", mid, e);
                        all_resolved = false;
                    }
                }
            }

            if !all_resolved || resolved_outcomes.len() != markets.len() {
                continue;
            }

            let winning_mid = match resolved_outcomes.iter().find(|(_, v)| v.as_str() == "yes") {
                Some((mid, _)) => mid.clone(),
                None => {
                    tracing::warn!("Position {}: all markets resolved but no YES winner", pid);
                    continue;
                }
            };

            if let Some(event) = self.positions.lock()
                .close_on_resolution(pid, &winning_mid)
            {
                tracing::info!(
                    "API resolution: {} → winner={}, payout={:.2}, profit={:.4}",
                    pid, winning_mid, event.payout, event.profit
                );
                results.push(ApiResolution {
                    position_id: pid.to_string(),
                    winning_market_id: winning_mid,
                    payout: event.payout,
                    profit: event.profit,
                });
            }
        }

        let count = results.len();
        if count > 0 {
            tracing::info!("API resolution check: resolved {} positions", count);
        } else {
            tracing::info!("API resolution check: all positions still open");
        }

        results
    }

    // === Dashboard helpers ===

    pub fn update_dashboard_metrics(
        &self, iteration: u64,
        lat_p50: u64, lat_p95: u64, lat_max: u64,
        scanner_status: &str, scanner_ts: &str,
        engine_status: &str, engine_ts: &str,
    ) {
        let mut m = self.engine_metrics.lock();
        m.iteration = iteration;
        m.lat_p50_us = lat_p50;
        m.lat_p95_us = lat_p95;
        m.lat_max_us = lat_max;
        m.scanner_status = scanner_status.to_string();
        m.scanner_ts = scanner_ts.to_string();
        m.engine_status = engine_status.to_string();
        m.engine_ts = engine_ts.to_string();
    }

    pub fn set_recent_opps(&self, opps_json: &[String]) {
        let mut opps = self.recent_opps.lock();
        *opps = opps_json.iter()
            .filter_map(|s| serde_json::from_str(s).ok())
            .collect();
    }
}

/// Result from evaluate_batch.
pub struct EvalBatchResult {
    pub opportunities: Vec<eval::Opportunity>,
    pub n_urgent: usize,
    pub n_background: usize,
    pub n_evaluated: usize,
    pub n_skipped_held: usize,
}

/// Result from API resolution check.
pub struct ApiResolution {
    pub position_id: String,
    pub winning_market_id: String,
    pub payout: f64,
    pub profit: f64,
}
