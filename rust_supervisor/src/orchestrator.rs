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
use rust_engine::notify::{Notifier, NotifyConfig, NotifyEvent};
use rust_engine::position;
use rust_engine::resolution::ResolutionValidator;
use rust_engine::postponement::PostponementDetector;
use rust_engine::scanner::MarketScanner;
use rust_engine::state::StateDB;
use rust_engine::ws_tiered::TieredWsConfig;
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
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
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
    pub dashboard_bind: String,

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
    pub api_resolution_interval: f64,

    // Pre-trade validation (B1.3)
    pub min_depth_per_leg: f64,
    pub depth_haircut: f64,
    pub max_book_staleness_secs: f64,
    pub min_profit_ratio: f64,

    // Record retention (B1.2)
    pub closed_retention_days: u32,

    // AI
    pub resolution_validation_enabled: bool,
    pub anthropic_api_key: String,
    pub postponement_enabled: bool,
    pub postponement_check_interval: f64,
    pub postponement_rescore_days: u32,
    pub state_db_path: PathBuf,

    // Latency instrumentation
    pub latency_instrumentation: bool,

    // Tiered WS
    pub use_tiered_ws: bool,
    pub tiered_ws_url: String,
    pub tier_b_max_connections: usize,
    pub tier_b_hysteresis_scans: u32,
    pub tier_b_consolidation_threshold: usize,
    pub tier_c_new_market_buffer_secs: f64,
    pub ws_stagger_ms: u64,
    pub ws_max_assets_per_connection: usize,
    pub ws_heartbeat_interval_secs: u64,
    pub tier_b_top_n_constraints: usize,  // 0 = no limit
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
        let ws_cfg = yaml.get("websocket").cloned().unwrap_or_default();

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
            dashboard_bind: yaml.pointer("/dashboard/bind_addr")
                .and_then(|v| v.as_str()).unwrap_or("127.0.0.1").to_string(),

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
            api_resolution_interval: eng.get("api_resolution_interval_seconds")
                .and_then(|v| v.as_f64()).unwrap_or(300.0),

            min_depth_per_leg: live_cfg.get("min_depth_per_leg")
                .and_then(|v| v.as_f64()).unwrap_or(0.0),
            depth_haircut: live_cfg.get("depth_haircut")
                .and_then(|v| v.as_f64()).unwrap_or(0.80),
            max_book_staleness_secs: eng.get("max_book_staleness_secs")
                .and_then(|v| v.as_f64()).unwrap_or(30.0),
            min_profit_ratio: live_cfg.get("min_profit_ratio")
                .and_then(|v| v.as_f64()).unwrap_or(0.70),
            closed_retention_days: eng.get("closed_position_retention_days")
                .and_then(|v| v.as_u64()).unwrap_or(90) as u32,

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
            latency_instrumentation: eng.get("latency_instrumentation")
                .and_then(|v| v.as_bool()).unwrap_or(false),

            // Tiered WS config
            use_tiered_ws: ws_cfg.get("use_tiered_ws")
                .and_then(|v| v.as_bool()).unwrap_or(false),
            tiered_ws_url: ws_cfg.get("market_channel_url")
                .and_then(|v| v.as_str())
                .unwrap_or("wss://ws-subscriptions-clob.polymarket.com/ws/market")
                .to_string(),
            tier_b_max_connections: ws_cfg.get("tier_b_max_connections")
                .and_then(|v| v.as_u64()).unwrap_or(10) as usize,
            tier_b_hysteresis_scans: ws_cfg.get("tier_b_hysteresis_scans")
                .and_then(|v| v.as_u64()).unwrap_or(3) as u32,
            tier_b_consolidation_threshold: ws_cfg.get("tier_b_consolidation_threshold")
                .and_then(|v| v.as_u64()).unwrap_or(300) as usize,
            tier_c_new_market_buffer_secs: ws_cfg.get("tier_c_new_market_buffer_secs")
                .and_then(|v| v.as_f64()).unwrap_or(2.5),
            ws_stagger_ms: ws_cfg.get("stagger_ms")
                .and_then(|v| v.as_u64()).unwrap_or(150),
            ws_max_assets_per_connection: ws_cfg.get("max_assets_per_connection")
                .and_then(|v| v.as_u64()).unwrap_or(450) as usize,
            ws_heartbeat_interval_secs: ws_cfg.get("heartbeat_interval")
                .and_then(|v| v.as_u64()).unwrap_or(10),
            tier_b_top_n_constraints: ws_cfg.get("tier_b_top_n_constraints")
                .and_then(|v| v.as_u64()).unwrap_or(0) as usize,
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
    last_api_resolution_check: f64,
    last_retention_prune: f64,
    iteration: u64,
    recent_latencies: std::collections::VecDeque<f64>,

    // Delay table
    p95_table: HashMap<String, f64>,
    p95_default: f64,

    // Background disk save thread handle
    disk_save_handle: Option<std::thread::JoinHandle<()>>,

    // WhatsApp notifier (C3)
    notifier: Arc<Notifier>,

    // P1: Cached held IDs (invalidated on position entry/exit/resolution)
    held_ids_cache: Option<(HashSet<String>, HashSet<String>)>,

    /// Tiered WS: last constraint→assets map (for Tier B hot constraint diffing).
    last_constraint_to_assets: HashMap<String, Vec<String>>,
}

impl Orchestrator {
    /// Create a new orchestrator from config.
    pub fn new(cfg: OrchestratorConfig, log_ring: std::sync::Arc<parking_lot::Mutex<rust_engine::monitor::LogRing>>) -> Result<Self, String> {
        let ws = cfg.workspace.to_string_lossy().to_string();

        let mut engine = TradingEngine::new(&ws)?;
        engine.log_ring = log_ring;

        // Enable latency instrumentation if configured
        if cfg.latency_instrumentation {
            engine.latency.set_enabled(true);
        }

        let scanner = MarketScanner::new(
            &cfg.workspace.join("data").join("markets.db").to_string_lossy(),
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

        // Load notification config (C3)
        let notify_cfg = {
            let n = cached_yaml.get("notifications").cloned().unwrap_or_default();
            NotifyConfig {
                enabled: n.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false),
                webhook_url: n.get("webhook_url").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                api_key: n.get("api_key").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                phone_number: n.get("phone_number").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                on_entry: n.get("on_entry").and_then(|v| v.as_bool()).unwrap_or(true),
                on_resolution: n.get("on_resolution").and_then(|v| v.as_bool()).unwrap_or(true),
                on_error: n.get("on_error").and_then(|v| v.as_bool()).unwrap_or(true),
                on_circuit_breaker: n.get("on_circuit_breaker").and_then(|v| v.as_bool()).unwrap_or(true),
                on_daily_summary: n.get("on_daily_summary").and_then(|v| v.as_bool()).unwrap_or(true),
                rate_limit_seconds: n.get("rate_limit_seconds").and_then(|v| v.as_f64()).unwrap_or(10.0),
            }
        };
        let notifier = Arc::new(Notifier::new(notify_cfg));

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
            last_api_resolution_check: 0.0,
            last_retention_prune: 0.0,
            iteration: 0,
            recent_latencies: std::collections::VecDeque::with_capacity(MAX_LATENCY_SAMPLES),
            p95_table, p95_default,
            disk_save_handle: None,
            notifier,
            held_ids_cache: None,
            last_constraint_to_assets: HashMap::new(),
        })
    }

    /// Run the full startup + event loop. Blocks until `running` is set to false.
    pub fn run(&mut self, running: Arc<AtomicBool>) {
        tracing::info!("Orchestrator starting...");

        // 0. Start dashboard server IMMEDIATELY so it's reachable during init
        self.engine.start_dashboard(self.cfg.dashboard_port, &self.cfg.dashboard_bind);
        tracing::info!("Dashboard server started on port {} (splash screen ready)", self.cfg.dashboard_port);

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
        let (all_assets, constraint_to_assets) = self.detect_constraints();

        // 3. Load delay table into engine
        self.engine.set_delay_table(
            self.p95_table.iter().map(|(k, v)| (k.clone(), *v)).collect()
        );

        // 4. Start WS (dashboard already started in step 0)
        if !all_assets.is_empty() {
            if self.cfg.use_tiered_ws {
                // Tiered WS: Tier C gets position assets, Tier B gets all hot constraint assets
                let position_asset_ids = self.engine.get_open_position_asset_ids();
                let hot_asset_ids: Vec<String> = constraint_to_assets.values()
                    .flat_map(|v| v.iter().cloned())
                    .collect();

                let ws_config = TieredWsConfig {
                    ws_url: self.cfg.tiered_ws_url.clone(),
                    heartbeat_interval_secs: self.cfg.ws_heartbeat_interval_secs,
                    max_assets_per_connection: self.cfg.ws_max_assets_per_connection,
                    stagger_ms: self.cfg.ws_stagger_ms,
                    tier_b_max_connections: self.cfg.tier_b_max_connections,
                    tier_b_hysteresis_scans: self.cfg.tier_b_hysteresis_scans,
                    tier_b_consolidation_threshold: self.cfg.tier_b_consolidation_threshold,
                    tier_c_new_market_buffer_secs: self.cfg.tier_c_new_market_buffer_secs,
                };

                self.engine.start_tiered(ws_config, hot_asset_ids, position_asset_ids);
                self.last_constraint_to_assets = constraint_to_assets;
                tracing::info!("Tiered WS started (Tier B + C)");
            } else {
                self.engine.start(all_assets, 0, "");  // port=0: skip dashboard, already running
            }
            tracing::info!("WS engine started");
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

            // Before resolving, capture constraint info for tiered WS exit hooks
            let pre_resolve_info: Vec<(String, String, Vec<String>)> = if self.cfg.use_tiered_ws {
                let pm = self.engine.positions.lock();
                pm.open_positions().values()
                    .map(|p| {
                        let cid = p.metadata.get("constraint_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let assets: Vec<String> = p.markets.keys().cloned().collect();
                        (p.position_id.clone(), cid, assets)
                    })
                    .collect()
            } else {
                Vec::new()
            };

            let closed = self.engine.resolve_by_ws_events(&events);
            if !closed.is_empty() { self.held_ids_cache = None; }
            for (pid, winner) in &closed {
                tracing::info!("WS RESOLUTION: {} → winner={}", pid, winner);

                // Tiered WS: migrate resolved position assets from Tier C → possibly Tier B
                if self.cfg.use_tiered_ws {
                    if let Some((_, cid, assets)) = pre_resolve_info.iter().find(|(p, _, _)| p == pid) {
                        let still_hot = self.last_constraint_to_assets.contains_key(cid);
                        self.engine.tiered_on_position_exit(cid, assets.clone(), still_hot);
                    }
                }

                let _ = self.notifier.send(&NotifyEvent::PositionResolved {
                    position_id: pid.clone(),
                    profit: 0.0, // actual P&L computed at resolution
                    method: format!("ws_resolution({})", winner),
                });
            }
        }

        // --- Evaluate batch ---
        // P1: Use cached held IDs (rebuilt only when positions change)
        if self.held_ids_cache.is_none() {
            self.held_ids_cache = Some(self.engine.get_held_ids());
        }
        let (held_cids, held_mids) = self.held_ids_cache.clone().unwrap();

        let t0 = Instant::now();
        let result = self.engine.evaluate_batch(
            self.cfg.max_evals_per_batch,
            &held_cids, &held_mids, 20,
            self.cfg.depth_haircut,
        );
        let batch_us = t0.elapsed().as_micros() as f64;

        if result.n_evaluated > 0 {
            // Segment 4: eval batch duration
            self.recent_latencies.push_back(batch_us);
            while self.recent_latencies.len() > MAX_LATENCY_SAMPLES {
                self.recent_latencies.pop_front();
            }
            self.engine.latency.record_eval_batch(batch_us);
        }

        if !result.opportunities.is_empty() {
            let entry_t0 = Instant::now();
            self.try_enter_or_replace(&result.opportunities, &held_cids, &held_mids);
            // Segment 5: eval → entry decision
            let entry_us = entry_t0.elapsed().as_micros() as f64;
            self.engine.latency.record_eval_to_entry(entry_us);

            // Segment 6 (e2e): origin_ts → now for each opportunity that had an entry attempt
            if self.engine.latency.is_enabled() {
                let now = now_secs();
                for opp in &result.opportunities {
                    if opp.origin_ts > 0.0 {
                        let e2e_us = (now - opp.origin_ts) * 1_000_000.0;
                        if e2e_us > 0.0 && e2e_us < 60_000_000.0 {
                            self.engine.latency.record_e2e(e2e_us);
                        }
                    }
                }
            }
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
                    let (all_assets, constraint_to_assets) = self.detect_constraints();
                    if !all_assets.is_empty() {
                        if self.cfg.use_tiered_ws {
                            // Tiered WS: incremental update — no connection churn!
                            self.engine.update_tier_b(constraint_to_assets.clone());
                            self.last_constraint_to_assets = constraint_to_assets;
                        } else {
                            self.engine.start(all_assets, 0, &self.cfg.dashboard_bind); // port=0 skips dashboard restart
                        }
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

        // --- Tiered WS: flush new markets + periodic maintenance ---
        if self.cfg.use_tiered_ws {
            // Flush new market bursts from Tier C buffer
            let bursts = self.engine.tiered_flush_new_markets();
            for burst in &bursts {
                tracing::info!("New market burst: event='{}' ({} markets)",
                    burst.event_title, burst.markets.len());
                // Collect all asset IDs from the burst for Tier B subscription
                let all_burst_assets: Vec<String> = burst.markets.iter()
                    .flat_map(|m| m.asset_ids.iter().cloned())
                    .collect();
                if !all_burst_assets.is_empty() {
                    // Use event_id as a synthetic constraint_id for new market tracking
                    let synthetic_cid = format!("newmkt_{}", &burst.event_id[..burst.event_id.len().min(32)]);
                    self.engine.tiered_add_new_market_constraint(synthetic_cid, all_burst_assets);
                }
            }

            // Periodic maintenance (hourly consolidation)
            self.engine.tiered_periodic_maintenance();
        }

        // --- API resolution poll (safety net for missed WS events) ---
        if (now - self.last_api_resolution_check) >= self.cfg.api_resolution_interval {
            if self.engine.pm_open_count() > 0 {
                let results = self.engine.check_api_resolutions();
                if !results.is_empty() {
                    self.held_ids_cache = None;
                    for r in &results {
                        tracing::info!("API RESOLUTION: {} → winner={}, profit=${:.4}",
                            r.position_id, r.winning_market_id, r.profit);
                        let _ = self.notifier.send(&NotifyEvent::PositionResolved {
                            position_id: r.position_id.clone(),
                            profit: r.profit,
                            method: format!("api_resolution({})", r.winning_market_id),
                        });
                    }
                }
            }
            self.last_api_resolution_check = now;
        }

        // --- Record retention pruning (B1.2) — daily ---
        if self.cfg.closed_retention_days > 0 && (now - self.last_retention_prune) >= 86400.0 {
            let cutoff = now - (self.cfg.closed_retention_days as f64 * 86400.0);
            let pruned = self.engine.positions.lock().prune_closed_before(cutoff);
            if pruned > 0 {
                tracing::info!("Pruned {} closed positions (> {}d old)", pruned, self.cfg.closed_retention_days);
            }
            self.last_retention_prune = now;
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

    /// Detect constraints and return (all_asset_ids, constraint_to_assets).
    /// When `tier_b_top_n_constraints > 0`, constraint_to_assets is filtered to the
    /// top N constraints by tightest spread (|price_sum - 1.0|, ascending).
    fn detect_constraints(&mut self) -> (Vec<String>, HashMap<String, Vec<String>>) {
        if self.market_lookup.is_empty() {
            tracing::warn!("Cannot detect constraints: no markets loaded");
            return (Vec::new(), HashMap::new());
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

        let mut constraint_to_assets = result.constraint_to_assets.clone();

        // Filter to top N constraints by composite score for Tier B.
        // Score = spread / time_factor  (lower = better).
        // spread = |price_sum - 1.0|  (tighter = better)
        // time_factor = max(hours_to_resolution, 1.0)  (sooner = better, divides score down)
        // So constraints with tight spreads resolving soon rank highest.
        let top_n = self.cfg.tier_b_top_n_constraints;
        if top_n > 0 && constraint_to_assets.len() > top_n {
            let spread = &result.constraint_spread;
            let end_ts = &result.constraint_end_ts;
            let now_ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);

            let mut ranked: Vec<_> = constraint_to_assets.keys().cloned().collect();
            ranked.sort_by(|a, b| {
                let score = |cid: &str| -> f64 {
                    let s = spread.get(cid).copied().unwrap_or(1.0);
                    let ets = end_ts.get(cid).copied().unwrap_or(0.0);
                    let hours = if ets > now_ts { (ets - now_ts) / 3600.0 } else { 8760.0 }; // unknown → 1yr
                    let time_factor = hours.max(1.0);
                    s / time_factor  // lower = better (tight spread + soon resolution)
                };
                score(a).total_cmp(&score(b))
            });
            let keep: std::collections::HashSet<String> = ranked.into_iter().take(top_n).collect();
            let removed = constraint_to_assets.len() - keep.len();
            constraint_to_assets.retain(|k, _| keep.contains(k));
            let hot_assets: usize = constraint_to_assets.values().map(|v| v.len()).sum();
            tracing::info!("Tier B filter: top {} constraints ({} assets), {} demoted to Tier A (scored by spread/time_to_resolve)",
                top_n, hot_assets, removed);
        }

        (result.all_asset_ids, constraint_to_assets)
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
        // B2 fix: runtime guard instead of debug_assert (protects release builds)
        if n_closed_total < db_closed {
            tracing::error!(
                "Closed position count decreased: {} < {} (skipping incremental sync)",
                n_closed_total, db_closed
            );
            let ms = self.state_db.mirror_to_disk();
            tracing::info!("State saved: {} open, {} closed [{}ms]",
                open_rows.len(), n_closed_total, ms as u64);
            return;
        }
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

            // B3: negRisk capital efficiency — for sell arbs on negRisk markets,
            // collateral = $1.00/unit instead of sum(NO prices), so we can size larger
            let is_sell = opp.method.to_lowercase().contains("sell");

            // Scale to dynamic capital
            let old_cap = opp.optimal_bets.values().sum::<f64>();
            let scale = if old_cap > 0.0 { cap / old_cap } else { 1.0 };
            let scaled_bets: HashMap<String, f64> = opp.optimal_bets.iter()
                .map(|(k, v)| (k.clone(), v * scale))
                .collect();
            let scaled_profit = opp.expected_profit * scale;

            // Store negRisk info in metadata via chain_info=None for new entries
            match self.engine.enter_position(
                &opp.constraint_id, &opp.constraint_id,
                if is_sell { "arb_sell" } else { "arb_buy" }, &opp.method,
                &opp.market_ids, &opp.market_names,
                &opp.current_prices, &opp.current_no_prices,
                &scaled_bets, scaled_profit, opp.expected_profit_pct,
                is_sell,
                None,  // new chain
            ) {
                position::EntryResult::Entered(pos) => {
                    self.held_ids_cache = None;
                    // Tiered WS: migrate assets from Tier B → Tier C on position entry
                    if self.cfg.use_tiered_ws {
                        let entry_assets: Vec<String> = opp.market_ids.iter()
                            .flat_map(|mid| {
                                // Get YES and NO asset IDs for this market from the constraint
                                self.engine.constraints.get(&opp.constraint_id)
                                    .map(|c| c.markets.iter()
                                        .filter(|m| &m.market_id == mid)
                                        .flat_map(|m| vec![m.yes_asset_id.clone(), m.no_asset_id.clone()])
                                        .filter(|a| !a.is_empty())
                                        .collect::<Vec<_>>())
                                    .unwrap_or_default()
                            })
                            .collect();
                        if !entry_assets.is_empty() {
                            self.engine.tiered_on_position_entry(&opp.constraint_id, entry_assets);
                        }
                    }
                    // B3: Store negRisk capital efficiency in position metadata
                    if opp.neg_risk {
                        if let Some(ce) = opp.capital_efficiency {
                            let mut pm = self.engine.positions.lock();
                            if let Some(p) = pm.open_positions_mut().get_mut(&pos.position_id) {
                                p.metadata.insert("is_neg_risk".into(), serde_json::json!(true));
                                p.metadata.insert("capital_efficiency".into(), serde_json::json!(ce));
                                if let Some(cpu) = opp.collateral_per_unit {
                                    p.metadata.insert("collateral_per_unit".into(), serde_json::json!(cpu));
                                }
                            }
                        }
                    }
                    tracing::info!("ENTER: {}... | ${:.2} | exp ${:.2} | {:.1}h | score={:.6} | depth=${:.0}",
                        &opp.constraint_id[..opp.constraint_id.len().min(30)],
                        cap, scaled_profit, hours, score, opp.min_leg_depth_usd);
                    let _ = self.notifier.send(&NotifyEvent::PositionEntry {
                        position_id: pos.position_id.clone(),
                        strategy: opp.method.clone(),
                        capital: cap,
                        profit_pct: opp.expected_profit_pct,
                    });
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
                            // B1 fix: use actual end_date_ts for hours remaining
                            let now_secs = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs_f64();
                            let end_date_ts = pos.metadata.get("end_date_ts")
                                .and_then(|v| v.as_f64()).unwrap_or(0.0);
                            let hours_rem = if end_date_ts > now_secs {
                                ((end_date_ts - now_secs) / 3600.0).max(1.0)
                            } else {
                                1.0 // already past or unknown — treat as imminent
                            };
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
                        // B1.1: Capture chain info from the position being replaced
                        let (chain_info_owned, replace_exit_info): (Option<(String, u32, String)>, Option<(String, Vec<String>)>) = {
                            let pm = self.engine.positions.lock();
                            let chain = pm.get_position(&worst_pid).map(|p| {
                                let chain_id = p.chain_id.clone().unwrap_or_else(|| worst_pid.clone());
                                let next_gen = p.chain_generation + 1;
                                (chain_id, next_gen, worst_pid.clone())
                            });
                            let exit_info = if self.cfg.use_tiered_ws {
                                pm.get_position(&worst_pid).map(|p| {
                                    let cid = p.metadata.get("constraint_id")
                                        .and_then(|v| v.as_str()).unwrap_or("").to_string();
                                    let assets: Vec<String> = p.markets.keys().cloned().collect();
                                    (cid, assets)
                                })
                            } else {
                                None
                            };
                            (chain, exit_info)
                        };

                        // Execute replacement
                        if let Some((net, profit)) = self.engine.liquidate_position(&worst_pid, "replaced", &worst_bids) {
                            // Tiered WS: move replaced position assets back to B if still hot
                            if let Some((cid, assets)) = replace_exit_info {
                                let still_hot = self.last_constraint_to_assets.contains_key(&cid);
                                self.engine.tiered_on_position_exit(&cid, assets, still_hot);
                            }
                            self.held_ids_cache = None;
                            tracing::info!("REPLACE: liquidated {} → freed ${:.2}, profit=${:+.2}",
                                &worst_pid[..worst_pid.len().min(30)], net, profit);

                            // Enter replacement with chain tracking
                            let cap = dynamic_capital(self.engine.total_value(), self.cfg.capital_pct);
                            let old_cap = best_opp.optimal_bets.values().sum::<f64>();
                            let scale = if old_cap > 0.0 { cap / old_cap } else { 1.0 };
                            let scaled_bets: HashMap<String, f64> = best_opp.optimal_bets.iter()
                                .map(|(k, v)| (k.clone(), v * scale))
                                .collect();
                            let is_sell = best_opp.method.to_lowercase().contains("sell");

                            let chain_ref = chain_info_owned.as_ref()
                                .map(|(cid, gen, ppid)| (cid.as_str(), *gen, ppid.as_str()));

                            let _ = self.engine.enter_position(
                                &best_opp.constraint_id, &best_opp.constraint_id,
                                if is_sell { "arb_sell" } else { "arb_buy" }, &best_opp.method,
                                &best_opp.market_ids, &best_opp.market_names,
                                &best_opp.current_prices, &best_opp.current_no_prices,
                                &scaled_bets, best_opp.expected_profit * scale,
                                best_opp.expected_profit_pct, is_sell,
                                chain_ref,
                            );

                            self.last_replacement = now;
                            tracing::info!("  WITH: {}... | score={:.6} | {:.1}h | chain_gen={}",
                                &best_opp.constraint_id[..best_opp.constraint_id.len().min(30)],
                                best_score, best_hours,
                                chain_info_owned.as_ref().map(|(_, g, _)| *g).unwrap_or(0));
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

        // B1.0: Depth gating — skip if any leg has insufficient depth
        if self.cfg.min_depth_per_leg > 0.0 && opp.min_leg_depth_usd < self.cfg.min_depth_per_leg {
            tracing::debug!("SKIP (depth): {}... min_depth=${:.2} < ${:.2}",
                &opp.constraint_id[..opp.constraint_id.len().min(30)],
                opp.min_leg_depth_usd, self.cfg.min_depth_per_leg);
            return false;
        }

        // B1.3: Book staleness — check that all legs have fresh book data
        if self.cfg.max_book_staleness_secs > 0.0 {
            if let Some(constraint) = self.engine.constraints.get(&opp.constraint_id) {
                for mref in &constraint.markets {
                    let is_sell = opp.method.to_lowercase().contains("sell");
                    let asset_id = if is_sell { &mref.no_asset_id } else { &mref.yes_asset_id };
                    let age = self.engine.book.get_book_age_secs(asset_id);
                    if age > self.cfg.max_book_staleness_secs {
                        if age == f64::MAX {
                            tracing::debug!("SKIP (no book): {}... asset {} has no book data",
                                &opp.constraint_id[..opp.constraint_id.len().min(30)],
                                &asset_id[..asset_id.len().min(16)]);
                        } else {
                            tracing::debug!("SKIP (stale book): {}... asset {} is {:.1}s old",
                                &opp.constraint_id[..opp.constraint_id.len().min(30)],
                                &asset_id[..asset_id.len().min(16)], age);
                        }
                        return false;
                    }
                }
            }
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

    /// Extract LIVE bid prices for a position's markets from the order book.
    /// Falls back to entry_price if no book data is available for a market.
    fn get_position_bids_typed(&self, pos: &rust_engine::position::Position) -> HashMap<String, f64> {
        // Get constraint_id from position metadata to look up asset IDs
        let constraint_id = pos.metadata.get("constraint_id")
            .and_then(|v| v.as_str());

        // Load constraint once (if available) for asset_id lookups
        let constraint = constraint_id
            .and_then(|cid| self.engine.constraints.get(cid));

        pos.markets.iter()
            .map(|(mid, leg)| {
                let live_bid = constraint.as_ref().and_then(|c| {
                    // Find the MarketRef matching this market_id
                    c.markets.iter().find(|mref| mref.market_id == *mid).and_then(|mref| {
                        // Use the outcome field to pick the right asset_id:
                        // outcome "yes" → we hold YES shares → need YES bid
                        // outcome "no"  → we hold NO shares  → need NO bid
                        let asset_id = if leg.outcome == "no" {
                            &mref.no_asset_id
                        } else {
                            &mref.yes_asset_id
                        };
                        let bid = self.engine.book.get_best_bid(asset_id);
                        if bid > 0.0 { Some(bid) } else { None }
                    })
                });
                let bid = live_bid.unwrap_or(leg.entry_price);
                (mid.clone(), bid)
            })
            .collect()
    }

    // --- Proactive exits ---

    fn check_proactive_exits(&mut self) {
        let all_bids = self.collect_all_position_bids();
        let exits = self.engine.check_proactive_exits(&all_bids, PROACTIVE_EXIT_MULTIPLIER);
        for exit in &exits {
            tracing::info!("PROACTIVE EXIT: {}... ratio={:.3}",
                &exit.position_id[..exit.position_id.len().min(40)], exit.ratio);

            // Capture tiered WS exit info before liquidation
            let exit_info = if self.cfg.use_tiered_ws {
                let pm = self.engine.positions.lock();
                pm.get_position(&exit.position_id).map(|p| {
                    let cid = p.metadata.get("constraint_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let assets: Vec<String> = p.markets.keys().cloned().collect();
                    (cid, assets)
                })
            } else {
                None
            };

            let bids = self.get_position_bids_by_id(&exit.position_id);
            if let Some((net, profit)) = self.engine.liquidate_position(&exit.position_id, "proactive_exit", &bids) {
                self.held_ids_cache = None;
                // Tiered WS: move assets back to Tier B if still hot
                if let Some((cid, assets)) = exit_info {
                    let still_hot = self.last_constraint_to_assets.contains_key(&cid);
                    self.engine.tiered_on_position_exit(&cid, assets, still_hot);
                }
                tracing::info!("  Sold: freed ${:.2}, profit=${:+.2}", net, profit);
                let _ = self.notifier.send(&NotifyEvent::ProactiveExit {
                    position_id: exit.position_id.clone(),
                    profit,
                    ratio: exit.ratio,
                });
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
        let (q_urg, q_bg) = self.engine.queue_depths();
        let cap = self.engine.current_capital();
        let npos = self.engine.pm_open_count();

        let (lat_p50, lat_p95, lat_max) = if !self.recent_latencies.is_empty() {
            let mut lats: Vec<f64> = self.recent_latencies.iter().copied().collect();
            lats.sort_by(|a, b| a.total_cmp(b));
            let p50 = lats[lats.len() / 2];
            let p95 = lats[(lats.len() as f64 * 0.95) as usize];
            let max = *lats.last().unwrap_or(&0.0);
            (p50, p95, max)
        } else {
            (0.0, 0.0, 0.0)
        };

        if let Some(ts) = self.engine.tiered_stats() {
            tracing::info!(
                "[iter {}] Capital=${:.2} positions={} | TieredWS: B={} conns/{} assets/{} hot, C={} conns/{} assets/{} pos | msgs={} urg={} bg={} lat_μs p50={:.0} p95={:.0} max={:.0}",
                self.iteration, cap, npos,
                ts.tier_b_connections, ts.tier_b_assets, ts.tier_b_hot_constraints,
                ts.tier_c_connections, ts.tier_c_assets, ts.tier_c_position_assets,
                ts.total_msgs, q_urg, q_bg, lat_p50, lat_p95, lat_max,
            );
        } else {
            let ws = self.engine.stats();
            tracing::info!(
                "[iter {}] Capital=${:.2} positions={} | WS: subs={} msgs={} live={} urgent={} bg={} lat_μs p50={:.0} p95={:.0} max={:.0}",
                self.iteration, cap, npos,
                ws.subscribed, ws.total_msgs, ws.live_books,
                q_urg, q_bg, lat_p50, lat_p95, lat_max,
            );
        }

        // Per-segment latency breakdown (when instrumentation enabled)
        if self.engine.latency.is_enabled() {
            let snap = self.engine.latency.snapshot();
            tracing::debug!(
                "Latency breakdown (μs p50/p95/max): ws_net={:.0}/{:.0}/{:.0} ws→q={:.0}/{:.0}/{:.0} q_wait={:.0}/{:.0}/{:.0} eval={:.0}/{:.0}/{:.0} eval→entry={:.0}/{:.0}/{:.0} e2e={:.0}/{:.0}/{:.0}",
                snap.ws_network.p50, snap.ws_network.p95, snap.ws_network.max,
                snap.ws_to_queue.p50, snap.ws_to_queue.p95, snap.ws_to_queue.max,
                snap.queue_wait.p50, snap.queue_wait.p95, snap.queue_wait.max,
                snap.eval_batch.p50, snap.eval_batch.p95, snap.eval_batch.max,
                snap.eval_to_entry.p50, snap.eval_to_entry.p95, snap.eval_to_entry.max,
                snap.e2e.p50, snap.e2e.p95, snap.e2e.max,
            );
        }

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
