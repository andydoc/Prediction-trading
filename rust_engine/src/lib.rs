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
/// - WhatsApp notifications (C3)

pub mod types;
pub mod cached_db;
pub mod book;
pub mod queue;
pub mod state;
pub mod arb;
pub mod eval;
pub mod position;
pub mod ws;
pub mod ws_pool;
pub mod ws_tier_b;
pub mod ws_tier_c;
pub mod ws_tiered;
pub mod dashboard;
pub mod resolution;
pub mod postponement;
pub mod scanner;
pub mod detect;
pub mod notify;
pub mod latency;
pub mod monitor;

use std::collections::HashMap;
use std::sync::Arc;

use book::BookMirror;
use queue::EvalQueue;
use eval::{ConstraintStore, EvalConfig, Constraint};
use types::LoggingCfg;
use ws::WsManager;
use ws_tiered::{TieredWsManager, TieredWsConfig, TieredWsStats};
use ws_tier_c::NewMarketBurst;
use position::PositionManager;
use dashboard::{DashboardState, EngineMetrics};
use latency::LatencyTracker;
use monitor::MonitorState;

/// The core trading engine. Owns all hot-path state:
/// WS connections, order books, eval queue, positions, dashboard.
///
/// Previously exposed to Python via PyO3; now used directly by the
/// Rust supervisor binary.
pub struct TradingEngine {
    pub book: Arc<BookMirror>,
    pub eval_queue: Arc<EvalQueue>,
    pub ws: Arc<WsManager>,
    /// Tiered WS manager (Tier B + C). Created lazily on first tiered start.
    pub ws_tiered: parking_lot::Mutex<Option<TieredWsManager>>,
    pub constraints: Arc<ConstraintStore>,
    pub eval_config: parking_lot::Mutex<EvalConfig>,
    pub runtime: tokio::runtime::Runtime,
    pub positions: Arc<parking_lot::Mutex<PositionManager>>,
    pub engine_metrics: Arc<parking_lot::Mutex<EngineMetrics>>,
    pub recent_opps: Arc<parking_lot::Mutex<Vec<serde_json::Value>>>,
    pub mode: String,
    pub start_time: chrono::DateTime<chrono::Utc>,
    pub delay_table: Arc<parking_lot::Mutex<(HashMap<String, f64>, f64)>>,
    pub http_client: reqwest::blocking::Client,
    pub latency: Arc<LatencyTracker>,
    /// Monitor state for dashboard time-series metrics.
    pub monitor: Arc<parking_lot::Mutex<MonitorState>>,
    /// Separate log ring buffer for dashboard (avoids monitor lock contention).
    pub log_ring: Arc<parking_lot::Mutex<monitor::LogRing>>,
    /// Shared resolved events vec — used by both old WS and tiered WS.
    resolved_events: Arc<parking_lot::Mutex<Vec<ws::ResolvedEvent>>>,
}

impl TradingEngine {
    /// Initialize tracing with daily rotating file + stderr output.
    pub fn init_tracing(workspace: &str, log_cfg: &LoggingCfg, log_ring: Arc<parking_lot::Mutex<monitor::LogRing>>) {
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

        // Monitor layer captures logs for dashboard display
        let monitor_layer = monitor::MonitorLayer {
            log_ring: Arc::clone(&log_ring),
        };

        let _ = tracing_subscriber::registry()
            .with(env_filter)
            .with(file_layer)
            .with(stderr_layer)
            .with(monitor_layer)
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
        let monitor = Arc::new(parking_lot::Mutex::new(MonitorState::new()));
        let log_ring = Arc::new(parking_lot::Mutex::new(monitor::LogRing::new()));
        Self::init_tracing(workspace, &log_cfg, Arc::clone(&log_ring));

        let book = Arc::new(BookMirror::new(&cfg));
        let eval_queue = Arc::new(EvalQueue::new());
        let latency = Arc::new(LatencyTracker::new(false)); // toggled via config
        let positions = Arc::new(parking_lot::Mutex::new(
            PositionManager::new(pos_cfg.initial_capital, pos_cfg.taker_fee)
        ));
        let ws = Arc::new(WsManager::new(
            cfg.clone(), Arc::clone(&book), Arc::clone(&eval_queue),
            Arc::clone(&positions), Arc::clone(&latency),
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
            .worker_threads(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2))
            .enable_all()
            .thread_name("rust-engine")
            .build()
            .map_err(|e| format!("Failed to create tokio runtime: {}", e))?;

        let http_client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
            .build()
            .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

        let resolved_events = Arc::new(parking_lot::Mutex::new(Vec::new()));

        Ok(Self {
            book, eval_queue, ws, constraints,
            ws_tiered: parking_lot::Mutex::new(None),
            eval_config: parking_lot::Mutex::new(eval_config),
            runtime, positions,
            engine_metrics: Arc::new(parking_lot::Mutex::new(EngineMetrics::default())),
            recent_opps: Arc::new(parking_lot::Mutex::new(Vec::new())),
            mode: "shadow".to_string(),
            start_time: chrono::Utc::now(),
            delay_table: Arc::new(parking_lot::Mutex::new((HashMap::new(), 33.5))),
            http_client,
            latency,
            monitor,
            log_ring,
            resolved_events,
        })
    }

    /// Set the asset_id → constraint_id index.
    pub fn set_asset_index(&self, index: HashMap<String, Vec<String>>) {
        self.book.set_asset_index(index);
    }

    /// Start WS connections and dashboard server.
    /// If asset_ids is empty, only starts the dashboard (useful when tiered WS is active).
    /// Start the dashboard HTTP server immediately (before WS or any other init).
    /// This ensures the dashboard is reachable as soon as the process starts.
    pub fn start_dashboard(&self, dashboard_port: u16, dashboard_bind: &str) {
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
                latency: Arc::clone(&self.latency),
                monitor: Arc::clone(&self.monitor),
                log_ring: Arc::clone(&self.log_ring),
            };
            let bind_addr = dashboard_bind.to_string();
            self.runtime.spawn(async move {
                dashboard::start(dash_state, dashboard_port, &bind_addr).await;
            });
        }
    }

    pub fn start(&self, asset_ids: Vec<String>, dashboard_port: u16, _dashboard_bind: &str) {
        if !asset_ids.is_empty() {
            let ws = Arc::clone(&self.ws);
            self.runtime.spawn(async move {
                ws.start(asset_ids).await;
            });
        }

        // Dashboard already started via start_dashboard() — only start if not already running
        // (legacy path: start dashboard here if start_dashboard wasn't called)
        if dashboard_port > 0 {
            // Dashboard is idempotent — axum will fail bind if already listening, which is fine
        }
    }

    /// Stop all WS connections.
    pub fn stop(&self) {
        self.ws.stop();
        if let Some(ref tiered) = *self.ws_tiered.lock() {
            tiered.stop();
        }
    }

    // === Tiered WS management ===

    /// Start the tiered WS system (Tier B + C). Creates the TieredWsManager if needed.
    /// `hot_constraints` maps constraint_id → asset_ids for Tier B.
    /// `position_asset_ids` are assets from open positions for Tier C.
    pub fn start_tiered(
        &self,
        ws_config: TieredWsConfig,
        hot_asset_ids: Vec<String>,
        position_asset_ids: Vec<String>,
    ) {
        let mut tiered = self.ws_tiered.lock();
        if tiered.is_some() {
            tracing::warn!("TieredWS: already running, stopping old instance first");
            if let Some(ref old) = *tiered {
                old.stop();
            }
        }

        let mgr = TieredWsManager::new(
            ws_config,
            Arc::clone(&self.book),
            Arc::clone(&self.eval_queue),
            Arc::clone(&self.resolved_events),
            Arc::clone(&self.positions),
            Arc::clone(&self.latency),
        );

        let rt = self.runtime.handle();
        mgr.start(hot_asset_ids, position_asset_ids, rt);
        *tiered = Some(mgr);
    }

    /// Update Tier B hot constraints after scanner rebuild.
    pub fn update_tier_b(&self, hot_constraints: HashMap<String, Vec<String>>) {
        if let Some(ref tiered) = *self.ws_tiered.lock() {
            tiered.update_tier_b(hot_constraints);
        }
    }

    /// Handle position entry on the tiered WS system.
    pub fn tiered_on_position_entry(&self, constraint_id: &str, asset_ids: Vec<String>) {
        if let Some(ref tiered) = *self.ws_tiered.lock() {
            tiered.on_position_entry(constraint_id, asset_ids);
        }
    }

    /// Handle position exit on the tiered WS system.
    pub fn tiered_on_position_exit(&self, constraint_id: &str, asset_ids: Vec<String>, still_hot: bool) {
        if let Some(ref tiered) = *self.ws_tiered.lock() {
            tiered.on_position_exit(constraint_id, asset_ids, still_hot);
        }
    }

    /// Flush new market bursts from Tier C buffer.
    pub fn tiered_flush_new_markets(&self) -> Vec<NewMarketBurst> {
        if let Some(ref tiered) = *self.ws_tiered.lock() {
            tiered.flush_new_markets()
        } else {
            Vec::new()
        }
    }

    /// Add a new market constraint to Tier B (from Tier C detection).
    pub fn tiered_add_new_market_constraint(&self, constraint_id: String, asset_ids: Vec<String>) {
        if let Some(ref tiered) = *self.ws_tiered.lock() {
            tiered.add_new_market_constraint(constraint_id, asset_ids);
        }
    }

    /// Periodic maintenance for the tiered WS system.
    pub fn tiered_periodic_maintenance(&self) {
        if let Some(ref tiered) = *self.ws_tiered.lock() {
            tiered.periodic_maintenance();
        }
    }

    /// Get tiered WS stats (returns None if tiered WS not active).
    pub fn tiered_stats(&self) -> Option<TieredWsStats> {
        self.ws_tiered.lock().as_ref().map(|t| t.stats())
    }

    /// Check if tiered WS is active.
    pub fn is_tiered_ws_active(&self) -> bool {
        self.ws_tiered.lock().is_some()
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
        depth_haircut: f64,
    ) -> EvalBatchResult {
        let ec = self.eval_config.lock().clone();
        let (opps, n_urg, n_bg, n_eval, n_held) = eval::evaluate_batch(
            &self.eval_queue, &self.book, &self.constraints, &ec,
            max_evals, held_cids, held_mids, top_n, depth_haircut, &self.latency,
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
        let mut events = self.ws.drain_resolved();
        // Also drain from tiered WS resolved events
        let mut tiered_events = std::mem::take(&mut *self.resolved_events.lock());
        if !tiered_events.is_empty() {
            events.append(&mut tiered_events);
        }
        events
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
        chain_info: Option<(&str, u32, &str)>,
    ) -> position::EntryResult {
        let end_date_ts = self.constraints.get(constraint_id)
            .map(|c| c.end_date_ts).unwrap_or(0.0);

        self.positions.lock().enter_position(
            opportunity_id, constraint_id, strategy, method,
            market_ids, market_names, current_prices, current_no_prices,
            optimal_bets, expected_profit, expected_profit_pct, is_sell,
            end_date_ts, chain_info,
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

    /// Combined held IDs — single PM lock acquisition instead of two.
    pub fn get_held_ids(&self) -> (std::collections::HashSet<String>, std::collections::HashSet<String>) {
        self.positions.lock().get_held_ids()
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
        // Collect position IDs and their market IDs under the lock, then release
        let position_data: Vec<(String, Vec<String>)> = {
            let pm = self.positions.lock();
            if pm.open_count() == 0 {
                return Vec::new();
            }
            pm.open_positions().values()
                .map(|p| (p.position_id.clone(), p.markets.keys().cloned().collect()))
                .collect()
        };

        tracing::info!("Checking API resolution for {} open positions...", position_data.len());

        let client = &self.http_client;

        let mut results = Vec::new();

        for (pid, market_ids) in &position_data {
            let mut resolved_outcomes: HashMap<String, String> = HashMap::new();
            let mut all_resolved = true;

            for mid in market_ids {
                let url = format!("https://gamma-api.polymarket.com/markets/{}", mid);
                match client.get(&url).send() {
                    Ok(resp) => {
                        if let Ok(mdata) = resp.json::<serde_json::Value>() {
                            // Only trust umaResolutionStatus == "resolved" — not prices
                            let uma_status = mdata["umaResolutionStatus"].as_str().unwrap_or("");
                            if uma_status != "resolved" {
                                all_resolved = false;
                                continue;
                            }

                            // Market is definitively resolved — read outcome from prices
                            let prices_raw = &mdata["outcomePrices"];
                            let prices: Vec<f64> = if let Some(s) = prices_raw.as_str() {
                                serde_json::from_str::<Vec<serde_json::Value>>(s)
                                    .unwrap_or_default()
                                    .iter()
                                    .filter_map(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
                                    .collect()
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
                                    // Resolved but prices ambiguous — skip
                                    tracing::warn!("Market {} resolved but prices ambiguous: {:?}", mid, prices);
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

            if !all_resolved || resolved_outcomes.len() != market_ids.len() {
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

        // Populate WS stats from tiered WS if active, else from flat WS
        if let Some(ts) = self.ws_tiered.lock().as_ref().map(|t| t.stats()) {
            m.ws_subscribed = ts.total_assets;
            m.ws_total_msgs = ts.total_msgs;
            m.ws_live_books = self.book.live_count() as u64;
            m.ws_connections = ts.total_connections;
        } else {
            let ws = self.ws.stats();
            m.ws_subscribed = ws.subscribed;
            m.ws_total_msgs = ws.total_msgs;
            m.ws_live_books = ws.live_books;
            m.ws_connections = 0;
        }
    }

    pub fn set_recent_opps(&self, opps_json: &[String]) {
        let mut opps = self.recent_opps.lock();
        *opps = opps_json.iter()
            .filter_map(|s| serde_json::from_str(s).ok())
            .collect();
    }
}

/// Result from evaluate_batch.
#[must_use]
pub struct EvalBatchResult {
    pub opportunities: Vec<eval::Opportunity>,
    pub n_urgent: usize,
    pub n_background: usize,
    pub n_evaluated: usize,
    pub n_skipped_held: usize,
}

/// Result from API resolution check.
#[must_use]
pub struct ApiResolution {
    pub position_id: String,
    pub winning_market_id: String,
    pub payout: f64,
    pub profit: f64,
}
