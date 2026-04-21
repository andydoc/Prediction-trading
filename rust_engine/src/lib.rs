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
/// - Telegram notifications (C3)
/// - Circuit breaker (C1)
/// - POL gas balance monitoring (C1.1)

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
pub mod ws_user;
pub mod dashboard;
pub mod resolution;
pub mod postponement;
pub mod scanner;
pub mod detect;
pub mod notify;
pub mod latency;
pub mod monitor;
pub mod signing;
pub mod instrument;
pub mod rate_limiter;
pub mod executor;
pub mod reconciliation;
pub mod circuit_breaker;
pub mod gas_monitor;
pub mod usdc_monitor;
pub mod http_client;
pub mod gamma_freshness;
pub mod fill_quality;
pub mod fill_confirmation;
pub mod strategy_tracker;
pub mod accounting;
pub mod sports_ws;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use book::BookMirror;
use queue::EvalQueue;
use eval::{ConstraintStore, EvalConfig, Constraint};
use types::LoggingCfg;
use ws::WsManager;
use ws_tiered::{TieredWsManager, TieredWsConfig, TieredWsStats};
use ws_tier_c::NewMarketBurst;
use position::PositionManager;
use instrument::InstrumentStore;
use dashboard::{DashboardState, EngineMetrics};
use latency::LatencyTracker;
use monitor::MonitorState;

/// Snapshot of position stats captured under a single lock.
#[derive(Debug, Clone, Copy)]
pub struct DashboardSnapshot {
    pub current_capital: f64,
    pub total_value: f64,
    pub initial_capital: f64,
    pub open_count: usize,
    pub closed_count: usize,
}

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
    /// Instrument store — token_id → Instrument (tick_size, rounding, neg_risk, etc.)
    pub instruments: Arc<InstrumentStore>,
    /// C2: Kill switch flag — shared with dashboard, read by orchestrator.
    pub kill_switch: Arc<AtomicBool>,
    /// Strategy tracker summary JSON — updated by orchestrator, read by dashboard SSE.
    pub strategy_summary: Arc<parking_lot::Mutex<serde_json::Value>>,
    /// Double-entry accounting ledger — tracks all cash movements independently.
    pub accounting: Arc<parking_lot::Mutex<accounting::AccountingLedger>>,
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

        let http_client = crate::http_client::secure_client_with_timeout(10)?;

        let resolved_events = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let instruments = Arc::new(InstrumentStore::new());
        let kill_switch = Arc::new(AtomicBool::new(false));

        Ok(Self {
            book, eval_queue, ws, constraints, instruments,
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
            kill_switch,
            strategy_summary: Arc::new(parking_lot::Mutex::new(serde_json::Value::Null)),
            accounting: Arc::new(parking_lot::Mutex::new(accounting::AccountingLedger::new(0.0, 0.0))),
        })
    }

    /// Set the asset_id → constraint_id index.
    pub fn set_asset_index(&self, index: HashMap<String, Vec<String>>) {
        self.book.set_asset_index(index);
    }

    /// Load instruments from scanner market data.
    ///
    /// Called by the orchestrator after scanner completes. Each market produces
    /// two instruments (YES and NO tokens) with tick_size, rounding config, etc.
    pub fn load_instruments(&self, markets: &HashMap<String, serde_json::Value>) {
        self.instruments.load_from_markets(markets);
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
                kill_switch: Arc::clone(&self.kill_switch),
                strategy_summary: Arc::clone(&self.strategy_summary),
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
            Some(Arc::clone(&self.instruments)),
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
        // P10: HashSet rebuild on each constraint detection is acceptable — runs once
        // every few minutes at most, and the set is typically < 1000 elements.
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
        // Initialize accounting ledger with same capital and fee rate
        let mut acct = self.accounting.lock();
        *acct = accounting::AccountingLedger::new(initial_capital, taker_fee);
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

    /// Pre-seed the monitor ring buffer with the historical equity curve derived from
    /// strategy tracker closed positions. Called once at startup after strategy state
    /// is loaded so the portfolio graph shows history immediately (not just since last restart).
    ///
    /// Reconstructs a step-function equity curve: starting from the sum of all strategy
    /// initial capitals, applies each closed trade's profit in chronological order.
    pub fn seed_monitor_from_strategy_history(&self, tracker: &crate::strategy_tracker::StrategyTracker) {
        // Collect all (close_ts, profit) events across all portfolios
        let mut events: Vec<(f64, f64)> = tracker.portfolios.iter()
            .flat_map(|p| p.closed_positions.iter().map(|c| (c.close_ts, c.actual_profit)))
            .collect();

        if events.is_empty() { return; }

        // Sort chronologically
        events.sort_by(|a, b| a.0.total_cmp(&b.0));

        // Running equity starts from aggregate initial capital across all portfolios
        let agg_initial: f64 = tracker.portfolios.iter()
            .map(|p| p.config.initial_capital)
            .sum();

        let mut running_value = agg_initial;
        let mut running_realized = 0.0f64;

        let mut mon = self.monitor.lock();
        for (ts, profit) in events {
            running_value += profit;
            running_realized += profit;
            mon.total_value.push(ts, running_value);
            mon.realized_pnl.push(ts, running_realized);
        }
        tracing::info!(
            "Monitor seeded from strategy history: {} data points, equity ${:.2}→${:.2}",
            mon.total_value.len(), agg_initial, running_value
        );
    }

    /// Snapshot of dashboard-relevant position stats under a single lock acquisition.
    pub fn dashboard_snapshot(&self) -> DashboardSnapshot {
        let pm = self.positions.lock();
        DashboardSnapshot {
            current_capital: pm.current_capital(),
            total_value: pm.total_value(),
            initial_capital: pm.initial_capital(),
            open_count: pm.open_count(),
            closed_count: pm.closed_count(),
        }
    }

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

        let result = self.positions.lock().enter_position(
            opportunity_id, constraint_id, strategy, method,
            market_ids, market_names, current_prices, current_no_prices,
            optimal_bets, expected_profit, expected_profit_pct, is_sell,
            end_date_ts, chain_info,
        );

        // Record on accounting ledger if position was entered (with dedup)
        if let position::EntryResult::Entered(ref pos) = result {
            let desc = format!("BUY {} markets, ${:.2} capital", pos.markets.len(), pos.total_capital);
            let mut acct = self.accounting.lock();
            for (mid, leg) in &pos.markets {
                let asset_id = if !leg.token_id.is_empty() {
                    leg.token_id.clone()
                } else {
                    mid.clone()
                };
                // Dedup key: position_id + market_id (unique per leg entry)
                let trade_key = format!("entry_{}_{}", pos.position_id, mid);
                acct.record_buy_dedup(
                    &trade_key, &pos.position_id,
                    leg.bet_amount, pos.fees_paid / pos.markets.len() as f64,
                    &asset_id, mid, leg.shares, leg.entry_price, &desc,
                );
            }
        }

        result
    }

    pub fn close_on_resolution(&self, position_id: &str, winning_market_id: &str) -> Option<position::ResolutionEvent> {
        // Get position data before resolution for accounting
        let pos_data = {
            let pm = self.positions.lock();
            pm.open_positions().get(position_id).map(|p| {
                let legs: Vec<(String, String, f64, f64)> = p.markets.iter().map(|(mid, leg)| {
                    let asset_id = if !leg.token_id.is_empty() { leg.token_id.clone() } else { mid.clone() };
                    (mid.clone(), asset_id, leg.shares, leg.bet_amount)
                }).collect();
                (p.total_capital, legs)
            })
        };

        let result = self.positions.lock().close_on_resolution(position_id, winning_market_id);

        // Record on accounting ledger (with dedup)
        if let (Some(ref event), Some((total_capital, legs))) = (&result, pos_data) {
            let desc = format!("Resolution {} (winner={})", position_id, winning_market_id);
            let payout = event.payout;
            let mut acct = self.accounting.lock();
            for (mid, asset_id, shares, cost_basis) in &legs {
                // R4: Skip empty legs (unfilled or zero-capital) to avoid spurious journal entries
                if *shares <= 0.0 && *cost_basis <= 0.0 { continue; }
                // ACC-1: Allocate full payout to winning leg, zero to losers.
                // Only the winning market pays out; losing legs receive nothing.
                let leg_payout = if mid == winning_market_id { payout } else { 0.0 };
                let trade_key = format!("resolve_{}_{}", position_id, mid);
                acct.record_sell_dedup(
                    &trade_key, position_id, leg_payout, *cost_basis, 0.0,
                    asset_id, *shares, 0.0, &desc,
                );
            }
        }

        result
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
        // Get position data before liquidation for accounting
        let pos_data = {
            let pm = self.positions.lock();
            pm.open_positions().get(position_id).map(|p| {
                let legs: Vec<(String, String, f64, f64)> = p.markets.iter().map(|(mid, leg)| {
                    let asset_id = if !leg.token_id.is_empty() { leg.token_id.clone() } else { mid.clone() };
                    (mid.clone(), asset_id, leg.shares, leg.bet_amount)
                }).collect();
                (p.total_capital, p.fees_paid, legs)
            })
        };

        let result = self.positions.lock().liquidate_position(position_id, reason, current_bids);

        // Record sell on accounting ledger (with dedup)
        if let (Some((_net_proceeds, _profit)), Some((_total_capital, _entry_fees, legs))) = (result, pos_data) {
            let desc = format!("SELL {} ({})", position_id, reason);
            let taker_fee = self.positions.lock().taker_fee();
            let mut acct = self.accounting.lock();
            for (mid, asset_id, shares, cost_basis) in &legs {
                let bid = current_bids.get(mid).copied().unwrap_or(0.0);
                let leg_proceeds = shares * bid;
                let leg_fees = leg_proceeds * taker_fee;
                let trade_key = format!("liquidate_{}_{}", position_id, mid);
                acct.record_sell_dedup(
                    &trade_key, position_id, leg_proceeds, *cost_basis, leg_fees,
                    asset_id, *shares, bid, &desc,
                );
            }
        }

        result
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
    /// Query Gamma API for a single market's resolution status.
    /// Returns Some(("yes"|"no", uma_status)) if definitively resolved, None otherwise.
    fn check_market_resolution_gamma(&self, mid: &str) -> (Option<String>, String) {
        let url = format!("https://gamma-api.polymarket.com/markets/{}", mid);
        match self.http_client.get(&url).send() {
            Ok(resp) => {
                if let Ok(mdata) = resp.json::<serde_json::Value>() {
                    let uma_status = mdata["umaResolutionStatus"].as_str().unwrap_or("").to_string();
                    if uma_status != "resolved" {
                        return (None, uma_status);
                    }
                    // Market is definitively resolved — read outcome from prices
                    let prices_raw = &mdata["outcomePrices"];
                    let prices: Vec<f64> = if let Some(s) = prices_raw.as_str() {
                        match serde_json::from_str::<Vec<serde_json::Value>>(s) {
                            Ok(vals) => vals.iter()
                                .filter_map(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
                                .collect(),
                            Err(e) => {
                                tracing::warn!("outcomePrices parse fail for market {}: {e}", mid);
                                return (None, uma_status);
                            }
                        }
                    } else if let Some(arr) = prices_raw.as_array() {
                        arr.iter().filter_map(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).collect()
                    } else {
                        Vec::new()
                    };
                    if prices.len() >= 2 {
                        if prices[0] >= 0.99 && prices[1] <= 0.01 {
                            return (Some("yes".into()), uma_status);
                        } else if prices[1] >= 0.99 && prices[0] <= 0.01 {
                            return (Some("no".into()), uma_status);
                        } else {
                            tracing::warn!("Market {} resolved but prices ambiguous: {:?}", mid, prices);
                        }
                    }
                    (None, uma_status)
                } else {
                    (None, String::new())
                }
            }
            Err(e) => {
                tracing::debug!("API check failed for market {}: {}", mid, e);
                (None, String::new())
            }
        }
    }

    /// Check all constraint groups for resolution via Gamma API.
    /// Returns Vec<(id, winning_market_id)> and Vec<DisputeInfo>.
    fn check_constraints_resolution(&self, items: &[(String, Vec<String>)], label: &str)
        -> (Vec<(String, String)>, Vec<DisputeInfo>)
    {
        let mut resolved = Vec::new();
        let mut disputes = Vec::new();

        for (id, market_ids) in items {
            let mut outcomes: HashMap<String, String> = HashMap::new();
            let mut all_ok = true;

            for mid in market_ids {
                let (outcome, uma_status) = self.check_market_resolution_gamma(mid);
                if let Some(o) = outcome {
                    outcomes.insert(mid.clone(), o);
                } else {
                    if !uma_status.is_empty() && uma_status != "too_early" && uma_status != "resolved" {
                        disputes.push(DisputeInfo {
                            position_id: id.clone(),
                            market_id: mid.clone(),
                            uma_status,
                        });
                    }
                    all_ok = false;
                }
            }

            if !all_ok || outcomes.len() != market_ids.len() { continue; }

            if let Some((winning_mid, _)) = outcomes.iter().find(|(_, v)| v.as_str() == "yes") {
                resolved.push((id.clone(), winning_mid.clone()));
            } else {
                // All markets resolved NO — non-exhaustive arb or cancelled market.
                // Push NONE so the position is closed (buy_all: full loss; sell_all: all NO
                // tokens win = max payout). Without this, the position stays open forever.
                tracing::warn!("{} {}: all markets resolved NO — closing with NONE winner", label, id);
                resolved.push((id.clone(), "NONE".to_string()));
            }
        }

        (resolved, disputes)
    }

    pub fn check_api_resolutions(&self) -> (Vec<ApiResolution>, Vec<DisputeInfo>) {
        // Collect position IDs and their market IDs under the lock, then release
        let position_data: Vec<(String, Vec<String>)> = {
            let pm = self.positions.lock();
            if pm.open_count() == 0 {
                return (Vec::new(), Vec::new());
            }
            pm.open_positions().values()
                .map(|p| (p.position_id.clone(), p.markets.keys().cloned().collect()))
                .collect()
        };

        tracing::info!("Checking API resolution for {} open positions...", position_data.len());

        let (results, disputes) = self.check_constraints_resolution(&position_data, "Position");

        // Apply all resolutions in a single lock acquisition
        let mut final_results = Vec::new();
        if !results.is_empty() {
            let mut pm = self.positions.lock();
            for (pid, winning_mid) in &results {
                if let Some(event) = pm.close_on_resolution(pid, winning_mid) {
                    tracing::info!(
                        "API resolution: {} → winner={}, payout={:.2}, profit={:.4}",
                        pid, winning_mid, event.payout, event.profit
                    );
                    final_results.push(ApiResolution {
                        position_id: pid.to_string(),
                        winning_market_id: winning_mid.clone(),
                        payout: event.payout,
                        profit: event.profit,
                    });
                }
            }
        }

        let count = final_results.len();
        if count > 0 {
            tracing::info!("API resolution check: resolved {} positions", count);
        } else {
            tracing::info!("API resolution check: all positions still open");
        }

        if !disputes.is_empty() {
            tracing::warn!("UMA dispute detected: {} market(s) in dispute", disputes.len());
        }

        (final_results, disputes)
    }

    /// Check resolution for strategy-only constraints (not in main PM).
    /// Returns Vec<(constraint_id, winning_market_id)>.
    pub fn check_strategy_resolutions(&self, constraints: Vec<(String, Vec<String>)>)
        -> Vec<(String, String)>
    {
        if constraints.is_empty() { return Vec::new(); }
        tracing::info!("Checking strategy resolution for {} constraints...", constraints.len());
        let (resolved, disputes) = self.check_constraints_resolution(&constraints, "Strategy constraint");
        if !resolved.is_empty() {
            tracing::info!("Strategy resolution check: resolved {} constraints", resolved.len());
        }
        if !disputes.is_empty() {
            tracing::warn!("Strategy UMA dispute: {} market(s) in dispute", disputes.len());
        }
        resolved
    }

    // === Reconciliation (B4.0/B4.1/B4.2) ===

    /// Extract open position data for reconciliation comparison.
    /// Returns Vec<(position_id, market_id, shares)> — one entry per leg.
    fn extract_position_data(&self) -> Vec<(String, String, f64)> {
        let pm = self.positions.lock();
        let mut data = Vec::new();
        for p in pm.open_positions().values() {
            for (market_id, leg) in &p.markets {
                data.push((p.position_id.clone(), market_id.clone(), leg.shares));
            }
        }
        data
    }

    /// Run startup reconciliation (B4.1).
    /// Called once after state is loaded from SQLite.
    /// Pass `clob_auth` for live CLOB position query, or None for shadow mode.
    pub fn reconcile_startup_with_auth(
        &self, clob_host: &str, auth: Option<&signing::ClobAuth>, escalation_threshold: f64,
    ) -> (reconciliation::ReconciliationReport, Vec<reconciliation::VenuePosition>) {
        let positions = self.extract_position_data();
        reconciliation::reconcile_startup(
            &positions, &self.http_client, clob_host, auth, escalation_threshold,
        )
    }

    /// Startup reconciliation without auth (shadow mode).
    /// Automatically applies reconciliation adjustments when discrepancies found.
    pub fn reconcile_startup(&self, escalation_threshold: f64) -> reconciliation::ReconciliationReport {
        let (report, venue) = self.reconcile_startup_with_auth(
            "https://clob.polymarket.com", None, escalation_threshold,
        );
        if !report.passed && !venue.is_empty() {
            let adjustments = self.apply_reconciliation(&report, &venue);
            for adj in &adjustments {
                tracing::info!("B4.1 startup auto-adjustment: {}", adj);
            }
        }
        report
    }

    /// Run periodic reconciliation (B4.0).
    /// Called on interval from the orchestrator tick loop.
    pub fn reconcile_periodic_with_auth(
        &self, clob_host: &str, auth: Option<&signing::ClobAuth>, escalation_threshold: f64,
    ) -> (reconciliation::ReconciliationReport, Vec<reconciliation::VenuePosition>) {
        let positions = self.extract_position_data();
        reconciliation::reconcile_periodic(
            &positions, &self.http_client, clob_host, auth, escalation_threshold,
        )
    }

    /// Periodic reconciliation without auth (shadow mode).
    /// Automatically applies reconciliation adjustments when discrepancies found.
    pub fn reconcile_periodic(&self, escalation_threshold: f64) -> reconciliation::ReconciliationReport {
        let (report, venue) = self.reconcile_periodic_with_auth(
            "https://clob.polymarket.com", None, escalation_threshold,
        );
        if !report.passed && !venue.is_empty() {
            let adjustments = self.apply_reconciliation(&report, &venue);
            for adj in &adjustments {
                tracing::info!("B4.0 periodic auto-adjustment: {}", adj);
            }
        }
        report
    }

    /// Apply reconciliation results: update internal state to match venue (source of truth).
    ///
    /// For each discrepancy:
    /// - QuantityMismatch: adjust MarketLeg.shares + accounting
    /// - PositionMissingOnVenue: mark position stale
    /// - PositionMissingInternal: log orphan (venue has it, we don't)
    ///
    /// Returns descriptions of adjustments made.
    pub fn apply_reconciliation(
        &self,
        report: &reconciliation::ReconciliationReport,
        venue_positions: &[reconciliation::VenuePosition],
    ) -> Vec<String> {
        let mut adjustments = Vec::new();

        // Build venue lookup: market_id → (total_size, avg_price, asset_id)
        let mut venue_by_market: std::collections::HashMap<String, (f64, f64, String)> =
            std::collections::HashMap::new();
        for vp in venue_positions {
            let entry = venue_by_market.entry(vp.market_id.clone())
                .or_insert((0.0, vp.avg_price, vp.asset_id.clone()));
            entry.0 += vp.size;
        }

        for d in &report.discrepancies {
            match d.kind {
                reconciliation::DiscrepancyKind::QuantityMismatch => {
                    let market_id = match &d.market_id {
                        Some(m) => m.clone(),
                        None => continue,
                    };
                    let internal = d.internal_value.unwrap_or(0.0);
                    let venue = d.venue_value.unwrap_or(0.0);
                    let delta = venue - internal;

                    // Get venue price and asset_id for accounting
                    let (_, price, asset_id) = venue_by_market.get(&market_id)
                        .cloned()
                        .unwrap_or((0.0, 0.0, String::new()));

                    // Update position shares in PositionManager
                    let position_id = d.position_id.clone().unwrap_or_default();
                    {
                        let mut pm = self.positions.lock();
                        if let Some(pos) = pm.open_positions_mut().get_mut(&position_id) {
                            if let Some(leg) = pos.markets.get_mut(&market_id) {
                                let old = leg.shares;
                                leg.shares = venue;
                                adjustments.push(format!(
                                    "QuantityMismatch {}: shares {:.2} → {:.2} (delta={:+.2})",
                                    &market_id[..market_id.len().min(20)], old, venue, delta
                                ));
                            }
                        }
                    }

                    // Record accounting adjustment
                    if delta.abs() > 0.001 {
                        let mut acct = self.accounting.lock();
                        acct.record_reconciliation_adjustment(
                            &position_id, &asset_id, &market_id,
                            delta, price,
                            &format!("Recon: {}{:.2} shares @ {:.4} ({})",
                                if delta > 0.0 { "+" } else { "" }, delta, price,
                                &market_id[..market_id.len().min(20)]),
                        );
                    }
                }
                reconciliation::DiscrepancyKind::PositionMissingOnVenue => {
                    let market_id = d.market_id.clone().unwrap_or_default();
                    adjustments.push(format!(
                        "MissingOnVenue {}: position exists internally but not on exchange (stale)",
                        &market_id[..market_id.len().min(20)]
                    ));
                    // Don't auto-close — flag for manual review
                }
                reconciliation::DiscrepancyKind::PositionMissingInternal => {
                    let market_id = d.market_id.clone().unwrap_or_default();
                    let venue_shares = d.venue_value.unwrap_or(0.0);
                    adjustments.push(format!(
                        "MissingInternal {}: venue has {:.2} shares, engine doesn't track it (orphan)",
                        &market_id[..market_id.len().min(20)], venue_shares
                    ));
                    // Don't auto-adopt — flag for manual review
                }
                _ => {}
            }
        }

        if adjustments.is_empty() {
            tracing::info!("[RECON] No adjustments needed — internal state matches venue");
        } else {
            tracing::info!("[RECON] Applied {} adjustments to match venue state", adjustments.len());
        }

        adjustments
    }

    // === Dashboard helpers ===

    pub fn update_dashboard_metrics(
        &self, iteration: u64,
        lat_p50: u64, lat_p95: u64, lat_max: u64,
        scanner_status: &str, scanner_ts: &str,
        engine_status: &str, engine_ts: &str,
        pol_balance: Option<f64>,
        usdc_balance: Option<f64>,
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
            m.ws_reconnects = ts.reconnects;
            m.ws_pong_timeouts = ts.pong_timeouts;
            m.ws_heartbeat_failures = ts.heartbeat_failures;
        } else {
            let ws = self.ws.stats();
            m.ws_subscribed = ws.subscribed;
            m.ws_total_msgs = ws.total_msgs;
            m.ws_live_books = ws.live_books;
            m.ws_connections = 0;
        }
        m.pol_balance = pol_balance;
        m.usdc_balance = usdc_balance;
    }

    /// E2.5: Update eval/opp/stale counters for stress test metrics.
    pub fn update_stress_counters(
        &self, evals_total: u64, opps_found: u64,
        stale_sweeps: u64, stale_assets_swept: u64,
    ) {
        let mut m = self.engine_metrics.lock();
        m.evals_total = evals_total;
        m.opps_found = opps_found;
        m.stale_sweeps = stale_sweeps;
        m.stale_assets_swept = stale_assets_swept;
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

/// R14: UMA dispute detection — market is in active dispute process.
pub struct DisputeInfo {
    pub position_id: String,
    pub market_id: String,
    pub uma_status: String,
}
