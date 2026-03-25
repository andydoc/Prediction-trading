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
use std::sync::{Arc, LazyLock};
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
use rust_engine::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use rust_engine::gas_monitor::{GasMonitor, GasMonitorConfig, GasCheckResult};
use rust_engine::usdc_monitor::{UsdcMonitor, UsdcMonitorConfig, UsdcCheckResult};
use rust_engine::strategy_tracker::{self, StrategyTracker};
use rust_engine::TradingEngine;

/// Safely truncate a string to at most `n` bytes at a char boundary.
fn truncate(s: &str, n: usize) -> &str {
    s.get(..n).unwrap_or(s)
}

// ---------------------------------------------------------------------------
// Config overlay: deep-merge config.local.yaml on top of config.yaml
// ---------------------------------------------------------------------------

fn deep_merge(base: &mut serde_json::Value, overlay: &serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(base_map), serde_json::Value::Object(overlay_map)) => {
            for (k, v) in overlay_map {
                deep_merge(base_map.entry(k.clone()).or_insert(serde_json::Value::Null), v);
            }
        }
        (base, overlay) => *base = overlay.clone(),
    }
}

// ---------------------------------------------------------------------------
// Delay model (ported from Python trading_engine.py)
// ---------------------------------------------------------------------------

const FALLBACK_P95_DATA: &[(&str, f64)] = &[
    ("football", 14.8), ("us_sports", 33.6), ("esports", 20.0), ("tennis", 20.8),
    ("mma_boxing", 50.3), ("cricket", 21.8), ("rugby", 23.3), ("politics", 350.2),
    ("gov_policy", 44.3), ("crypto", 3.4), ("crypto_price", 0.05), ("sports_props", 6.5),
    ("other", 33.5),
];
const FALLBACK_DEFAULT: f64 = 33.5;

/// P1: Pre-computed fallback table — avoids String allocations on every call.
static FALLBACK_P95: LazyLock<HashMap<String, f64>> = LazyLock::new(|| {
    FALLBACK_P95_DATA.iter().map(|(k, v)| (k.to_string(), *v)).collect()
});

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
// Config — YAML-mirrored serde structs (PY1)
// ---------------------------------------------------------------------------

/// Intermediate structs for serde deserialization from config.yaml.
/// Each struct mirrors a YAML section and provides defaults via `#[serde(default)]`.

#[derive(serde::Deserialize, Default)]
struct ArbitrageYaml {
    #[serde(default = "default_20u64")]
    max_concurrent_positions: u64,
    #[serde(default = "default_60u64")]
    max_days_to_resolution: u64,
    #[serde(default = "default_30u64")]
    max_days_to_replacement: u64,
    #[serde(default = "default_010")]
    capital_per_trade_pct: f64,
    #[serde(default = "default_10f")]
    min_trade_size: f64,
    #[serde(default = "default_300f")]
    min_resolution_time_secs: f64,
    #[serde(default = "default_60f")]
    replacement_cooldown_seconds: f64,
    #[serde(default = "default_020")]
    suspicious_profit_threshold: f64,
    #[serde(default = "default_050")]
    max_neg_risk_exposure_pct: f64,
    #[serde(default)]
    resolution_validation: ResolutionValidationYaml,
}

#[derive(serde::Deserialize, Default)]
struct ResolutionValidationYaml {
    #[serde(default = "default_true")]
    enabled: bool,
}

#[derive(serde::Deserialize, Default)]
struct EngineYaml {
    #[serde(default = "default_30f")]
    state_save_interval_seconds: f64,
    #[serde(default = "default_30f")]
    monitor_interval_seconds: f64,
    #[serde(default = "default_600f")]
    constraint_rebuild_interval_seconds: f64,
    #[serde(default = "default_500u64")]
    max_evals_per_batch: u64,
    #[serde(default = "default_30f")]
    stats_log_interval_seconds: f64,
    #[serde(default = "default_60f")]
    stale_sweep_interval_seconds: f64,
    #[serde(default = "default_30f")]
    stale_asset_threshold_seconds: f64,
    #[serde(default = "default_300f")]
    api_resolution_interval_seconds: f64,
    #[serde(default = "default_30f")]
    max_book_staleness_secs: f64,
    #[serde(default = "default_90u64")]
    closed_position_retention_days: u64,
    #[serde(default)]
    latency_instrumentation: bool,
}

#[derive(serde::Deserialize, Default)]
struct LiveTradingYaml {
    #[serde(default = "default_true")]
    shadow_only: bool,
    #[serde(default)]
    min_depth_per_leg: f64,
    #[serde(default = "default_080")]
    depth_haircut: f64,
    #[serde(default = "default_070")]
    min_profit_ratio: f64,
}

#[derive(serde::Deserialize, Default)]
struct DashboardYaml {
    #[serde(default = "default_5556u64")]
    port: u64,
    #[serde(default = "default_localhost")]
    bind_addr: String,
}

#[derive(serde::Deserialize, Default)]
struct WebsocketYaml {
    #[serde(default)]
    use_tiered_ws: bool,
    #[serde(default = "default_ws_url")]
    market_channel_url: String,
    #[serde(default = "default_10u64")]
    tier_b_max_connections: u64,
    #[serde(default = "default_3u64")]
    tier_b_hysteresis_scans: u64,
    #[serde(default = "default_300u64")]
    tier_b_consolidation_threshold: u64,
    #[serde(default = "default_25f")]
    tier_c_new_market_buffer_secs: f64,
    #[serde(default = "default_150u64")]
    stagger_ms: u64,
    #[serde(default = "default_450u64")]
    max_assets_per_connection: u64,
    #[serde(default = "default_10u64")]
    heartbeat_interval: u64,
    #[serde(default)]
    tier_b_top_n_constraints: u64,
}

#[derive(serde::Deserialize, Default)]
struct SafetyYaml {
    #[serde(default)]
    circuit_breaker: CircuitBreakerYaml,
    #[serde(default)]
    gas_monitor: GasMonitorYaml,
    #[serde(default)]
    usdc_monitor: UsdcMonitorYaml,
}

#[derive(serde::Deserialize, Default)]
struct CircuitBreakerYaml {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default = "default_010")]
    max_drawdown_pct: f64,
    #[serde(default = "default_3u64")]
    max_consecutive_errors: u64,
    #[serde(default = "default_300f")]
    error_window_seconds: f64,
    #[serde(default = "default_60f")]
    api_timeout_seconds: f64,
}

#[derive(serde::Deserialize, Default)]
struct GasMonitorYaml {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_rpc_url")]
    rpc_url: String,
    #[serde(default = "default_3600f")]
    check_interval_seconds: f64,
    #[serde(default = "default_1f")]
    min_pol_balance: f64,
    #[serde(default = "default_01f")]
    critical_pol_balance: f64,
}

#[derive(serde::Deserialize, Default)]
struct UsdcMonitorYaml {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_rpc_url")]
    rpc_url: String,
    #[serde(default = "default_3600f")]
    check_interval_seconds: f64,
    #[serde(default = "default_1f")]
    drift_threshold: f64,
    #[serde(default = "default_10f")]
    warning_balance: f64,
    #[serde(default = "default_1f")]
    critical_balance: f64,
}

#[derive(serde::Deserialize, Default)]
struct AiYaml {
    #[serde(default)]
    postponement: PostponementYaml,
}

#[derive(serde::Deserialize, Default)]
struct PostponementYaml {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_24f")]
    check_interval_hours: f64,
    #[serde(default = "default_14u64")]
    postponement_rescore_days: u64,
}

#[derive(serde::Deserialize, Default)]
struct SportsWsYaml {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_sports_ws_url")]
    url: String,
    #[serde(default = "default_1f")]
    reconnect_base_delay: f64,
    #[serde(default = "default_60f")]
    reconnect_max_delay: f64,
    #[serde(default = "default_7u64")]
    prune_interval_days: u64,
}

#[derive(serde::Deserialize, Default)]
struct StateYaml {
    #[serde(default)]
    db_path: Option<String>,
}

/// Top-level config YAML layout.
#[derive(serde::Deserialize, Default)]
struct ConfigYaml {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    arbitrage: ArbitrageYaml,
    #[serde(default)]
    engine: EngineYaml,
    #[serde(default)]
    live_trading: LiveTradingYaml,
    #[serde(default)]
    dashboard: DashboardYaml,
    #[serde(default)]
    websocket: WebsocketYaml,
    #[serde(default)]
    safety: SafetyYaml,
    #[serde(default)]
    ai: AiYaml,
    #[serde(default)]
    state: StateYaml,
    #[serde(default)]
    sports_ws: SportsWsYaml,
}

// Default value functions for serde
fn default_true() -> bool { true }
fn default_010() -> f64 { 0.10 }
fn default_020() -> f64 { 0.20 }
fn default_050() -> f64 { 0.50 }
fn default_070() -> f64 { 0.70 }
fn default_080() -> f64 { 0.80 }
fn default_01f() -> f64 { 0.1 }
fn default_1f() -> f64 { 1.0 }
fn default_10f() -> f64 { 10.0 }
fn default_24f() -> f64 { 24.0 }
fn default_25f() -> f64 { 2.5 }
fn default_30f() -> f64 { 30.0 }
fn default_60f() -> f64 { 60.0 }
fn default_300f() -> f64 { 300.0 }
fn default_600f() -> f64 { 600.0 }
fn default_3600f() -> f64 { 3600.0 }
fn default_3u64() -> u64 { 3 }
fn default_10u64() -> u64 { 10 }
fn default_14u64() -> u64 { 14 }
fn default_20u64() -> u64 { 20 }
fn default_30u64() -> u64 { 30 }
fn default_60u64() -> u64 { 60 }
fn default_90u64() -> u64 { 90 }
fn default_150u64() -> u64 { 150 }
fn default_300u64() -> u64 { 300 }
fn default_450u64() -> u64 { 450 }
fn default_500u64() -> u64 { 500 }
fn default_5556u64() -> u64 { 5556 }
fn default_localhost() -> String { "127.0.0.1".into() }
fn default_ws_url() -> String { "wss://ws-subscriptions-clob.polymarket.com/ws/market".into() }
fn default_rpc_url() -> String { "https://polygon-rpc.com".into() }
fn default_7u64() -> u64 { 7 }
fn default_sports_ws_url() -> String { "wss://sports-api.polymarket.com/ws".into() }

// ---------------------------------------------------------------------------
// Config — public struct
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

    // Suspicious arb detection (R2 risk mitigation)
    pub suspicious_profit_threshold: f64,

    // negRisk correlated exposure cap (R16 risk mitigation)
    // Max fraction of total capital that can be deployed in negRisk positions (default 0.50 = 50%)
    pub max_neg_risk_exposure_pct: f64,

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

    // Circuit breaker (C1)
    pub cb_enabled: bool,
    pub cb_max_drawdown_pct: f64,
    pub cb_max_consecutive_errors: u32,
    pub cb_error_window_seconds: f64,
    pub cb_api_timeout_seconds: f64,

    // Gas monitor (C1.1)
    pub gas_enabled: bool,
    pub gas_rpc_url: String,
    pub gas_wallet_address: String,
    pub gas_check_interval_seconds: f64,
    pub gas_min_pol_balance: f64,
    pub gas_critical_pol_balance: f64,

    // USDC monitor (B4.6)
    pub usdc_enabled: bool,
    pub usdc_rpc_url: String,
    pub usdc_check_interval_seconds: f64,
    pub usdc_drift_threshold: f64,
    pub usdc_warning_balance: f64,
    pub usdc_critical_balance: f64,

    // Test period (E3/E4)
    pub test_period_secs: f64,

    // CLOB L2 auth (for balance queries, geoblock checks)
    pub clob_auth: Option<rust_engine::signing::ClobAuth>,

    // Sports WS
    pub sports_ws_enabled: bool,
    pub sports_ws_url: String,
    pub sports_ws_reconnect_base: f64,
    pub sports_ws_reconnect_max: f64,
    pub sports_ws_prune_interval_days: u64,
}

impl OrchestratorConfig {
    /// Load from config.yaml + config.local.yaml (overlay) + secrets.yaml at workspace path.
    pub fn load(workspace: &Path) -> Self {
        let config_path = workspace.join("config").join("config.yaml");
        let local_path = workspace.join("config").join("config.local.yaml");
        let secrets_path = workspace.join("config").join("secrets.yaml");

        let mut yaml: serde_json::Value = std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|s| serde_yaml_ng::from_str(&s).ok())
            .unwrap_or_default();

        // Merge config.local.yaml on top (not tracked by git — for per-machine overrides)
        if let Ok(local_str) = std::fs::read_to_string(&local_path) {
            if let Ok(local_val) = serde_yaml_ng::from_str::<serde_json::Value>(&local_str) {
                deep_merge(&mut yaml, &local_val);
                tracing::info!("Applied config.local.yaml overrides");
            }
        }

        let secrets: serde_json::Value = std::fs::read_to_string(&secrets_path)
            .ok()
            .and_then(|s| serde_yaml_ng::from_str(&s).ok())
            .unwrap_or_default();

        // Deserialize each YAML section via serde (PY1: replaces ~130 lines of .and_then().unwrap_or())
        let cfg: ConfigYaml = serde_json::from_value(yaml.clone()).unwrap_or_default();

        // API key: secrets > env
        let api_key = secrets.pointer("/resolution_validation/anthropic_api_key")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
            .unwrap_or_default();

        // Derive wallet address from private key in secrets
        let gas_wallet_address = {
            let pk = secrets.pointer("/polymarket/private_key")
                .and_then(|v| v.as_str()).unwrap_or("");
            if pk.is_empty() {
                String::new()
            } else {
                match rust_engine::signing::OrderSigner::new(pk) {
                    Ok(signer) => format!("{:#x}", signer.address()),
                    Err(e) => {
                        tracing::warn!("Failed to derive wallet address for gas monitor: {}", e);
                        String::new()
                    }
                }
            }
        };

        // Build CLOB L2 auth: derive fresh creds from wallet (like clob-test),
        // falling back to cached creds in secrets.yaml
        let clob_auth = {
            let pk = secrets.pointer("/polymarket/private_key")
                .and_then(|v| v.as_str()).unwrap_or("");
            let clob_host = "https://clob.polymarket.com";

            // Try derive fresh creds from wallet private key
            let fresh_creds = if !pk.is_empty() {
                match rust_engine::signing::OrderSigner::new(pk) {
                    Ok(signer) => match signer.create_or_derive_api_key(clob_host) {
                        Ok(creds) => {
                            tracing::info!("CLOB API key derived fresh (key={}...)", &creds.api_key[..8.min(creds.api_key.len())]);
                            Some(creds)
                        }
                        Err(e) => {
                            tracing::warn!("Failed to derive fresh CLOB API key: {}", e);
                            None
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to create signer for CLOB auth: {}", e);
                        None
                    }
                }
            } else { None };

            // Fall back to cached creds from secrets.yaml
            let creds = fresh_creds.unwrap_or_else(|| {
                let key = secrets.pointer("/polymarket/clob_api_key").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let secret = secrets.pointer("/polymarket/clob_api_secret").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let pass = secrets.pointer("/polymarket/clob_passphrase").and_then(|v| v.as_str()).unwrap_or("").to_string();
                rust_engine::signing::ClobApiCreds { api_key: key, secret, passphrase: pass }
            });

            if !creds.api_key.is_empty() && !creds.secret.is_empty() && !creds.passphrase.is_empty() {
                match rust_engine::signing::ClobAuth::new(&creds, &gas_wallet_address) {
                    Ok(auth) => {
                        tracing::info!("CLOB L2 auth ready (key={}...)", &creds.api_key[..8.min(creds.api_key.len())]);
                        Some(auth)
                    }
                    Err(e) => {
                        tracing::warn!("Failed to build CLOB auth: {}", e);
                        None
                    }
                }
            } else {
                tracing::debug!("No CLOB API credentials available — balance queries will use on-chain fallback");
                None
            }
        };

        Self {
            workspace: workspace.to_path_buf(),
            mode: cfg.mode.unwrap_or_else(|| "dual".into()),
            shadow_only: cfg.live_trading.shadow_only,
            dashboard_port: cfg.dashboard.port as u16,
            dashboard_bind: cfg.dashboard.bind_addr,

            max_positions: cfg.arbitrage.max_concurrent_positions as usize,
            max_days_entry: cfg.arbitrage.max_days_to_resolution as u32,
            max_days_replace: cfg.arbitrage.max_days_to_replacement as u32,
            capital_pct: cfg.arbitrage.capital_per_trade_pct,
            min_trade_size: cfg.arbitrage.min_trade_size,
            min_resolution_secs: cfg.arbitrage.min_resolution_time_secs,
            replacement_cooldown_secs: cfg.arbitrage.replacement_cooldown_seconds,
            suspicious_profit_threshold: cfg.arbitrage.suspicious_profit_threshold,
            max_neg_risk_exposure_pct: cfg.arbitrage.max_neg_risk_exposure_pct,

            state_save_interval: cfg.engine.state_save_interval_seconds,
            monitor_interval: cfg.engine.monitor_interval_seconds,
            constraint_rebuild_interval: cfg.engine.constraint_rebuild_interval_seconds,
            max_evals_per_batch: cfg.engine.max_evals_per_batch as usize,
            stats_log_interval: cfg.engine.stats_log_interval_seconds,
            stale_sweep_interval: cfg.engine.stale_sweep_interval_seconds,
            stale_asset_threshold: cfg.engine.stale_asset_threshold_seconds,
            api_resolution_interval: cfg.engine.api_resolution_interval_seconds,

            min_depth_per_leg: cfg.live_trading.min_depth_per_leg,
            depth_haircut: cfg.live_trading.depth_haircut,
            max_book_staleness_secs: cfg.engine.max_book_staleness_secs,
            min_profit_ratio: cfg.live_trading.min_profit_ratio,
            closed_retention_days: cfg.engine.closed_position_retention_days as u32,

            resolution_validation_enabled: cfg.arbitrage.resolution_validation.enabled,
            anthropic_api_key: api_key,
            postponement_enabled: cfg.ai.postponement.enabled,
            postponement_check_interval: cfg.ai.postponement.check_interval_hours * 3600.0,
            postponement_rescore_days: cfg.ai.postponement.postponement_rescore_days as u32,
            state_db_path: cfg.state.db_path
                .map(|s| workspace.join(s))
                .unwrap_or_else(|| workspace.join("data").join("system_state").join("execution_state.db")),
            latency_instrumentation: cfg.engine.latency_instrumentation,

            use_tiered_ws: cfg.websocket.use_tiered_ws,
            tiered_ws_url: cfg.websocket.market_channel_url,
            tier_b_max_connections: cfg.websocket.tier_b_max_connections as usize,
            tier_b_hysteresis_scans: cfg.websocket.tier_b_hysteresis_scans as u32,
            tier_b_consolidation_threshold: cfg.websocket.tier_b_consolidation_threshold as usize,
            tier_c_new_market_buffer_secs: cfg.websocket.tier_c_new_market_buffer_secs,
            ws_stagger_ms: cfg.websocket.stagger_ms,
            ws_max_assets_per_connection: cfg.websocket.max_assets_per_connection as usize,
            ws_heartbeat_interval_secs: cfg.websocket.heartbeat_interval,
            tier_b_top_n_constraints: cfg.websocket.tier_b_top_n_constraints as usize,

            cb_enabled: cfg.safety.circuit_breaker.enabled,
            cb_max_drawdown_pct: cfg.safety.circuit_breaker.max_drawdown_pct,
            cb_max_consecutive_errors: cfg.safety.circuit_breaker.max_consecutive_errors as u32,
            cb_error_window_seconds: cfg.safety.circuit_breaker.error_window_seconds,
            cb_api_timeout_seconds: cfg.safety.circuit_breaker.api_timeout_seconds,

            gas_enabled: cfg.safety.gas_monitor.enabled,
            gas_rpc_url: cfg.safety.gas_monitor.rpc_url,
            gas_wallet_address,
            gas_check_interval_seconds: cfg.safety.gas_monitor.check_interval_seconds,
            gas_min_pol_balance: cfg.safety.gas_monitor.min_pol_balance,
            gas_critical_pol_balance: cfg.safety.gas_monitor.critical_pol_balance,

            usdc_enabled: cfg.safety.usdc_monitor.enabled,
            usdc_rpc_url: cfg.safety.usdc_monitor.rpc_url,
            usdc_check_interval_seconds: cfg.safety.usdc_monitor.check_interval_seconds,
            usdc_drift_threshold: cfg.safety.usdc_monitor.drift_threshold,
            usdc_warning_balance: cfg.safety.usdc_monitor.warning_balance,
            usdc_critical_balance: cfg.safety.usdc_monitor.critical_balance,
            test_period_secs: 0.0, // Set by CLI --test-period
            clob_auth,

            sports_ws_enabled: cfg.sports_ws.enabled,
            sports_ws_url: cfg.sports_ws.url,
            sports_ws_reconnect_base: cfg.sports_ws.reconnect_base_delay,
            sports_ws_reconnect_max: cfg.sports_ws.reconnect_max_delay,
            sports_ws_prune_interval_days: cfg.sports_ws.prune_interval_days,
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
    last_reconciliation: f64,
    iteration: u64,
    recent_latencies: std::collections::VecDeque<f64>,

    // E2.5: Cumulative counters for stress test metrics
    evals_total: u64,
    opps_found: u64,
    stale_sweeps: u64,
    stale_assets_swept: u64,

    // Delay table
    p95_table: HashMap<String, f64>,
    p95_default: f64,

    // Background disk save thread handle
    disk_save_handle: Option<std::thread::JoinHandle<()>>,

    // Telegram notifier (C3)
    notifier: Arc<Notifier>,

    // Circuit breaker (C1)
    circuit_breaker: CircuitBreaker,

    // Gas monitor (C1.1)
    gas_monitor: GasMonitor,
    last_gas_check: f64,

    // USDC monitor (B4.6)
    usdc_monitor: UsdcMonitor,
    last_usdc_check: f64,

    // P1: Cached held IDs (invalidated on position entry/exit/resolution)
    held_ids_cache: Option<(HashSet<String>, HashSet<String>)>,

    /// Tiered WS: last constraint→assets map (for Tier B hot constraint diffing).
    last_constraint_to_assets: HashMap<String, Vec<String>>,

    // C2: Kill switch already activated (prevents re-triggering)
    kill_switch_activated: bool,

    // C4: Daily P&L report — tracks last UTC day boundary we reported on
    last_daily_report_day: i64,

    /// Strategy tracker for virtual portfolios (Shadow A-F).
    strategy_tracker: Option<StrategyTracker>,

    /// Sports WebSocket manager — real-time game status for postponement pre-screening.
    sports_ws: Option<Arc<rust_engine::sports_ws::SportsWsManager>>,
    last_sports_prune: f64,

    /// CLOB API key daily refresh — keys can expire or be revoked
    last_clob_auth_refresh: f64,
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
            let table: HashMap<String, f64> = FALLBACK_P95.clone();
            (table, FALLBACK_DEFAULT)
        };

        // Cache parsed config.yaml + local overlay at startup
        let config_path = cfg.workspace.join("config").join("config.yaml");
        let mut cached_yaml: serde_json::Value = std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|s| serde_yaml_ng::from_str(&s).ok())
            .unwrap_or_default();
        let local_path = cfg.workspace.join("config").join("config.local.yaml");
        if let Ok(local_str) = std::fs::read_to_string(&local_path) {
            if let Ok(local_val) = serde_yaml_ng::from_str::<serde_json::Value>(&local_str) {
                deep_merge(&mut cached_yaml, &local_val);
            }
        }

        // Load notification config (C3)
        // Telegram bot token loaded from secrets.yaml; chat_id from config.yaml phone_number field
        let notify_cfg = {
            let n = cached_yaml.get("notifications").cloned().unwrap_or_default();
            let secrets_path = cfg.workspace.join("config").join("secrets.yaml");
            let secrets: serde_json::Value = std::fs::read_to_string(&secrets_path)
                .ok()
                .and_then(|s| serde_yaml_ng::from_str(&s).ok())
                .unwrap_or_default();

            // Build webhook URL: secrets telegram_bot_token takes precedence over config webhook_url
            let cfg_url = n.get("webhook_url").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let webhook_url = secrets.get("telegram_bot_token")
                .and_then(|v| v.as_str())
                .filter(|t| !t.is_empty())
                .map(|token| format!("https://api.telegram.org/bot{}/sendMessage", token))
                .unwrap_or(cfg_url);

            // Hostname: auto-detect via sysinfo, config override available
            let hostname = n.get("hostname").and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    std::fs::read_to_string("/etc/hostname")
                        .unwrap_or_default().trim().to_string()
                });

            // Instance: from notifications config or --instance CLI (via env)
            let instance = n.get("instance_name").and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .or_else(|| std::env::var("PT_INSTANCE").ok())
                .unwrap_or_default();

            NotifyConfig {
                enabled: n.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false),
                webhook_url,
                api_key: n.get("api_key").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                phone_number: n.get("phone_number").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                on_entry: n.get("on_entry").and_then(|v| v.as_bool()).unwrap_or(true),
                on_resolution: n.get("on_resolution").and_then(|v| v.as_bool()).unwrap_or(true),
                on_error: n.get("on_error").and_then(|v| v.as_bool()).unwrap_or(true),
                on_circuit_breaker: n.get("on_circuit_breaker").and_then(|v| v.as_bool()).unwrap_or(true),
                on_daily_summary: n.get("on_daily_summary").and_then(|v| v.as_bool()).unwrap_or(true),
                rate_limit_seconds: n.get("rate_limit_seconds").and_then(|v| v.as_f64()).unwrap_or(10.0),
                hostname,
                instance,
            }
        };
        let notifier = Arc::new(Notifier::new(notify_cfg));

        // Build circuit breaker before moving cfg into Self
        let circuit_breaker = CircuitBreaker::new(
            CircuitBreakerConfig {
                enabled: cfg.cb_enabled,
                max_drawdown_pct: cfg.cb_max_drawdown_pct,
                max_consecutive_errors: cfg.cb_max_consecutive_errors,
                error_window_seconds: cfg.cb_error_window_seconds,
                api_timeout_seconds: cfg.cb_api_timeout_seconds,
            },
            0.0, // peak set after state load
            now_secs(),
        );

        // Build gas monitor (C1.1)
        let gas_monitor = GasMonitor::new(GasMonitorConfig {
            enabled: cfg.gas_enabled,
            rpc_url: cfg.gas_rpc_url.clone(),
            wallet_address: cfg.gas_wallet_address.clone(),
            check_interval_seconds: cfg.gas_check_interval_seconds,
            min_pol_balance: cfg.gas_min_pol_balance,
            critical_pol_balance: cfg.gas_critical_pol_balance,
        });

        if gas_monitor.is_enabled() {
            tracing::info!("Gas monitor enabled: wallet={}, warn<{} POL, critical<{} POL",
                cfg.gas_wallet_address, cfg.gas_min_pol_balance, cfg.gas_critical_pol_balance);
        }

        // Build USDC monitor (B4.6)
        let usdc_monitor = UsdcMonitor::new(UsdcMonitorConfig {
            enabled: cfg.usdc_enabled,
            rpc_url: cfg.usdc_rpc_url.clone(),
            wallet_address: cfg.gas_wallet_address.clone(), // same wallet as gas
            check_interval_seconds: cfg.usdc_check_interval_seconds,
            drift_threshold: cfg.usdc_drift_threshold,
            warning_balance: cfg.usdc_warning_balance,
            critical_balance: cfg.usdc_critical_balance,
            clob_host: "https://clob.polymarket.com".to_string(),
        });

        if usdc_monitor.is_enabled() {
            tracing::info!("USDC monitor enabled: drift>{}, warn<${}, critical<${}",
                cfg.usdc_drift_threshold, cfg.usdc_warning_balance, cfg.usdc_critical_balance);
        }

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
            last_reconciliation: 0.0,
            iteration: 0,
            recent_latencies: std::collections::VecDeque::with_capacity(MAX_LATENCY_SAMPLES),
            evals_total: 0,
            opps_found: 0,
            stale_sweeps: 0,
            stale_assets_swept: 0,
            p95_table, p95_default,
            disk_save_handle: None,
            notifier,
            circuit_breaker,
            gas_monitor,
            last_gas_check: 0.0,
            usdc_monitor,
            last_usdc_check: 0.0,
            held_ids_cache: None,
            last_constraint_to_assets: HashMap::new(),
            kill_switch_activated: false,
            // C4: Start at current UTC day so we don't fire immediately on startup
            last_daily_report_day: (now_secs() / 86400.0).floor() as i64,
            strategy_tracker: None,
            sports_ws: None,
            last_sports_prune: 0.0,
            last_clob_auth_refresh: now_secs(),
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
                self.circuit_breaker.record_api_success(now_secs());
                self.ingest_scan_result(&result);
                tracing::info!("Scanner refresh: {} markets", result.count);
            }
            Err(e) => tracing::warn!("Scanner refresh failed (using cache): {}", e),
        }

        // 2. Detect constraints
        let (all_assets, constraint_to_assets) = self.detect_constraints();

        // 3. Load delay table into engine
        // P3: clone is required — engine API takes Vec<(String, f64)>, table is borrowed here
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

        // 5b. B4.1: Startup reconciliation — compare loaded positions against venue
        if self.engine.pm_open_count() > 0 {
            let report = self.engine.reconcile_startup(self.cfg.min_trade_size);
            if !report.passed {
                tracing::warn!("B4.1 startup reconciliation found {} critical discrepancies",
                    report.critical_count());
            }
        }

        // 6. Check API for missed resolutions
        if self.cfg.shadow_only {
            let (results, disputes) = self.engine.check_api_resolutions();
            if !results.is_empty() {
                for r in &results {
                    tracing::info!("API resolution: {} → winner={}, profit=${:.4}",
                        r.position_id, r.winning_market_id, r.profit);
                }
            }
            for d in &disputes {
                tracing::warn!("UMA DISPUTE: position={} market={} status={}",
                    d.position_id, d.market_id, d.uma_status);
            }
        }

        // 6b. Load strategy tracker for virtual portfolios (Shadow A-F)
        let strategy_configs = strategy_tracker::load_strategy_configs(&self.cfg.workspace);
        if !strategy_configs.is_empty() {
            let tracker = StrategyTracker::load_or_new(&self.state_db, strategy_configs);
            // Widen eval gates so all strategy opportunities are visible
            let mut ec = self.engine.eval_config.lock().clone();
            tracker.apply_widest_gates(&mut ec);
            *self.engine.eval_config.lock() = ec;
            tracing::info!("Strategy tracker: {} virtual portfolios loaded", tracker.len());
            // Initial summary for dashboard
            *self.engine.strategy_summary.lock() = tracker.build_summary();
            self.strategy_tracker = Some(tracker);
        }

        // 6c. Start Sports WebSocket for postponement pre-screening
        if self.cfg.sports_ws_enabled {
            let sports_cfg = rust_engine::sports_ws::SportsWsConfig {
                enabled: true,
                url: self.cfg.sports_ws_url.clone(),
                reconnect_base_delay: self.cfg.sports_ws_reconnect_base,
                reconnect_max_delay: self.cfg.sports_ws_reconnect_max,
            };
            let db_path = self.cfg.workspace.join("data").join("sports_ws.db");
            let mgr = Arc::new(rust_engine::sports_ws::SportsWsManager::new(
                sports_cfg, db_path.to_str().unwrap_or("data/sports_ws.db"),
            ));
            mgr.start(self.engine.runtime.handle());
            self.sports_ws = Some(mgr);
            tracing::info!("Sports WS: started for postponement pre-screening");
        }

        let mode_str = if self.cfg.shadow_only { "SHADOW" } else { "LIVE" };
        tracing::info!("Orchestrator ready [{}] — entering event loop", mode_str);

        // Send startup notification
        let _ = self.notifier.send(&NotifyEvent::Startup {
            mode: mode_str.to_string(),
            positions: self.engine.pm_open_count(),
            capital: self.engine.current_capital(),
        });

        // E3: Record test period end time for dashboard countdown
        let start_time = now_secs();
        let test_period_end = if self.cfg.test_period_secs > 0.0 {
            let end = start_time + self.cfg.test_period_secs;
            self.engine.engine_metrics.lock().test_period_end_ts = end;
            tracing::info!("Test period ends at {} ({:.0}s from now)",
                chrono::DateTime::from_timestamp(end as i64, 0)
                    .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                    .unwrap_or_default(),
                self.cfg.test_period_secs);
            end
        } else { 0.0 };
        let mut test_period_stop_sent = false;

        // 7. Event loop
        while running.load(Ordering::Relaxed) {
            self.iteration += 1;
            let now = now_secs();

            // E3: Test period auto-stop
            if test_period_end > 0.0 && now >= test_period_end && !test_period_stop_sent {
                tracing::info!("TEST PERIOD EXPIRED after {:.0}s — stopping",
                    now - start_time);
                let _ = self.notifier.send(&NotifyEvent::CircuitBreaker {
                    reason: format!("Test period expired ({:.0}h)", self.cfg.test_period_secs / 3600.0),
                });
                running.store(false, Ordering::Relaxed);
                test_period_stop_sent = true;
            }

            if let Err(e) = self.tick(now) {
                tracing::error!("[iter {}] Error: {}", self.iteration, e);
                self.circuit_breaker.record_error(now);
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

            // Before resolving, capture constraint info for tiered WS exit hooks + strategy tracker
            let pre_resolve_info: Vec<(String, String, Vec<String>)> = {
                let pm = self.engine.positions.lock();
                pm.open_positions().values()
                    .map(|p| {
                        let cid = p.metadata.get("constraint_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        // P2: clone required — assets outlive the position lock
                        let assets: Vec<String> = p.markets.keys().cloned().collect();
                        (p.position_id.clone(), cid, assets)
                    })
                    .collect()
            };

            let closed = self.engine.resolve_by_ws_events(&events);
            if !closed.is_empty() { self.held_ids_cache = None; }
            for (pid, winner) in &closed {
                tracing::info!("WS RESOLUTION: {} → winner={}", pid, winner);

                // Forward to strategy tracker (resolve virtual positions)
                if let Some((_, cid, _)) = pre_resolve_info.iter().find(|(p, _, _)| p == pid) {
                    if !cid.is_empty() {
                        if let Some(ref mut tracker) = self.strategy_tracker {
                            tracker.resolve_with_db(cid, winner, &self.state_db);
                        }
                    }
                }

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

        // --- C2: Kill switch check ---
        if !self.kill_switch_activated {
            self.check_kill_switch(now);
        }

        // --- Circuit breaker check (C1) ---
        if let Some(reason) = self.circuit_breaker.check(self.engine.total_value(), now) {
            tracing::error!("CIRCUIT BREAKER TRIPPED: {}", reason);
            let _ = self.notifier.send(&NotifyEvent::CircuitBreaker { reason });
        }
        if !self.circuit_breaker.is_trading_allowed() {
            // Skip trading but continue housekeeping (state save, WS, reconciliation, etc.)
            self.do_periodic_tasks(now);
            return Ok(());
        }

        // --- Evaluate batch ---
        // P1: Use cached held IDs (rebuilt only when positions change)
        if self.held_ids_cache.is_none() {
            self.held_ids_cache = Some(self.engine.get_held_ids());
        }
        // P3: Clone the cache contents so we don't hold an immutable borrow on self
        // while try_enter_or_replace needs &mut self.
        let (held_cids, held_mids) = self.held_ids_cache.as_ref().unwrap().clone();

        // When strategy tracker is active, eval with EMPTY held sets so virtual
        // portfolios see ALL opportunities (including markets the main trader holds).
        // The main trader's held filter is applied later in try_enter_or_replace.
        let t0 = Instant::now();
        let result = if self.strategy_tracker.is_some() {
            let empty_cids = HashSet::new();
            let empty_mids = HashSet::new();
            self.engine.evaluate_batch(
                self.cfg.max_evals_per_batch,
                &empty_cids, &empty_mids, 20,
                self.cfg.depth_haircut,
            )
        } else {
            self.engine.evaluate_batch(
                self.cfg.max_evals_per_batch,
                &held_cids, &held_mids, 20,
                self.cfg.depth_haircut,
            )
        };
        let batch_us = t0.elapsed().as_micros() as f64;

        // E2.5: Accumulate eval/opp counters for stress test metrics
        self.evals_total += result.n_evaluated as u64;
        self.opps_found += result.opportunities.len() as u64;

        if result.n_evaluated > 0 {
            // Segment 4: eval batch duration
            self.recent_latencies.push_back(batch_us);
            while self.recent_latencies.len() > MAX_LATENCY_SAMPLES {
                self.recent_latencies.pop_front();
            }
            self.engine.latency.record_eval_batch(batch_us);
        }

        // Feed ALL opportunities to strategy tracker (unfiltered by held)
        if let Some(ref mut tracker) = self.strategy_tracker {
            if !result.opportunities.is_empty() {
                let fee_rate = self.engine.eval_config.lock().fee_rate;
                tracker.process_opportunities(&result.opportunities, fee_rate);
                *self.engine.strategy_summary.lock() = tracker.build_summary();
            }
        }

        // Log all evaluated opportunities to SQLite for post-run analysis
        if !result.opportunities.is_empty() {
            let now_ts = now_secs();
            // Collect which strategies accepted each opp
            let strat_accepted: Vec<String> = if let Some(ref tracker) = self.strategy_tracker {
                result.opportunities.iter().map(|opp| {
                    let names: Vec<&str> = tracker.portfolios.iter()
                        .filter(|p| p.open_positions.contains_key(&opp.constraint_id))
                        .map(|p| p.config.name.as_str())
                        .collect();
                    names.join(",")
                }).collect()
            } else {
                vec![String::new(); result.opportunities.len()]
            };
            for (i, opp) in result.opportunities.iter().enumerate() {
                let accepted = if strat_accepted[i].is_empty() { None } else { Some(strat_accepted[i].as_str()) };
                self.state_db.log_opportunity(
                    now_ts, &opp.constraint_id, &opp.method, opp.market_ids.len(),
                    opp.expected_profit, opp.expected_profit_pct, opp.total_capital_required,
                    opp.hours_to_resolve, opp.expected_profit_pct / opp.hours_to_resolve.max(0.01),
                    false, None, accepted,
                );
            }
        }

        // Filter to non-held opportunities for the main trader
        let main_opps: Vec<Opportunity> = if self.strategy_tracker.is_some() {
            result.opportunities.iter()
                .filter(|o| !held_cids.contains(&o.constraint_id)
                    && !o.market_ids.iter().any(|m| held_mids.contains(m)))
                .cloned()
                .collect()
        } else {
            result.opportunities.clone()
        };

        if !main_opps.is_empty() {
            let entry_t0 = Instant::now();
            self.try_enter_or_replace(&main_opps, &held_cids, &held_mids);
            // Segment 5: eval → entry decision
            let entry_us = entry_t0.elapsed().as_micros() as f64;
            self.engine.latency.record_eval_to_entry(entry_us);

            // Segment 6 (e2e): origin_ts → now for each opportunity that had an entry attempt
            if self.engine.latency.is_enabled() {
                let now = now_secs();
                for opp in &main_opps {
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

        // --- Periodic tasks (run even when circuit breaker is tripped) ---
        self.do_periodic_tasks(now);

        Ok(())
    }

    /// C2: Check for kill switch activation (dashboard button or file-based trigger).
    fn check_kill_switch(&mut self, _now: f64) {
        use std::sync::atomic::Ordering;

        // Check 1: Dashboard atomic flag
        let dashboard_triggered = self.engine.kill_switch.load(Ordering::SeqCst);

        // Check 2: File-based trigger (kill.sh --emergency writes this file)
        let flag_path = self.cfg.workspace.join("data").join("kill_switch.flag");
        let file_triggered = flag_path.exists();

        if !dashboard_triggered && !file_triggered {
            return;
        }

        // --- Kill switch activated ---
        self.kill_switch_activated = true;
        let source = if dashboard_triggered && file_triggered {
            "dashboard + file"
        } else if dashboard_triggered {
            "dashboard"
        } else {
            "file (kill.sh --emergency)"
        };

        tracing::error!("KILL SWITCH ACTIVATED via {} — cancelling orders, switching to shadow", source);

        // (a) Cancel all open CLOB orders
        // The executor is owned by the orchestrator (or accessible via engine).
        // In shadow mode, this is a no-op since there are no real CLOB orders.
        // When live trading is enabled (Milestone D), this will cancel via the CLOB API.
        let cancelled_msg = if !self.cfg.shadow_only {
            // TODO(D): Access executor and call cancel_all_orders()
            // For now, log that we would cancel.
            "CLOB cancel attempted (live mode)".to_string()
        } else {
            "No CLOB orders to cancel (shadow mode)".to_string()
        };
        tracing::warn!("[KILL] {}", cancelled_msg);

        // (b) Set mode to shadow
        if !self.cfg.shadow_only {
            self.cfg.shadow_only = true;
            tracing::warn!("[KILL] Mode set to SHADOW — no live trades will be placed");
        }

        // (c) Send Telegram notification
        let _ = self.notifier.send(&NotifyEvent::Error {
            message: format!(
                "KILL SWITCH activated via {}. {}. Mode set to SHADOW. Positions: {}, Capital: ${:.2}",
                source, cancelled_msg,
                self.engine.pm_open_count(),  // single-value calls acceptable here (error path)
                self.engine.current_capital(),
            ),
        });

        // Clean up the file trigger (so it doesn't re-trigger on restart)
        if file_triggered {
            if let Err(e) = std::fs::remove_file(&flag_path) {
                tracing::warn!("[KILL] Failed to remove flag file: {}", e);
            }
        }

        // Reset the dashboard atomic flag
        self.engine.kill_switch.store(false, Ordering::SeqCst);
    }

    /// C4: Generate and send daily P&L report for the previous UTC day.
    fn generate_daily_report(&self, now: f64, prev_day: i64) {
        let day_start = prev_day as f64 * 86400.0;
        let day_end = day_start + 86400.0;

        let pm = self.engine.positions.lock();

        // Count entries: positions with entry_timestamp in [day_start, day_end)
        let entries = pm.open_positions().values()
            .chain(pm.closed_positions().iter())
            .filter(|p| {
                parse_entry_ts(&p.entry_timestamp)
                    .map(|ts| ts >= day_start && ts < day_end)
                    .unwrap_or(false)
            })
            .count() as u32;

        // Count exits + sum fees/pnl: positions with close_timestamp in [day_start, day_end)
        let closed_today: Vec<&rust_engine::position::Position> = pm.closed_positions().iter()
            .filter(|p| {
                p.close_timestamp
                    .map(|ts| ts >= day_start && ts < day_end)
                    .unwrap_or(false)
            })
            .collect();

        let exits = closed_today.len() as u32;
        let fees: f64 = closed_today.iter().map(|p| p.fees_paid).sum();
        let net_pnl: f64 = closed_today.iter().map(|p| p.actual_profit).sum();

        // Capital utilisation: deployed / total_value
        let metrics = pm.get_performance_metrics();
        let current_capital = metrics.get("current_capital").copied().unwrap_or(0.0);
        let initial_capital = metrics.get("initial_capital").copied().unwrap_or(0.0);
        let open_count = pm.open_positions().len();
        let deployed: f64 = pm.open_positions().values().map(|p| p.total_capital).sum();
        let total_value = current_capital + deployed;
        let capital_util_pct = if total_value > 0.0 { deployed / total_value } else { 0.0 };

        // Drawdown from peak — use monitor time series
        let drawdown_pct = self.engine.monitor.lock()
            .drawdown_pct.latest().unwrap_or(0.0) / 100.0; // monitor stores as %, we want fraction

        drop(pm); // Release position lock before I/O

        // Format report date as YYYY-MM-DD
        let report_date = {
            let secs = prev_day * 86400;
            let dt = chrono::DateTime::from_timestamp(secs, 0)
                .unwrap_or_else(|| chrono::Utc::now());
            dt.format("%Y-%m-%d").to_string()
        };

        tracing::info!(
            "[DAILY REPORT] {} — entries={}, exits={}, fees=${:.4}, net_pnl=${:.4}, util={:.1}%, dd={:.2}%",
            report_date, entries, exits, fees, net_pnl, capital_util_pct * 100.0, drawdown_pct * 100.0
        );

        // Send Telegram notification
        let _ = self.notifier.send(&NotifyEvent::DailySummary {
            entries,
            exits,
            fees,
            net_pnl,
            capital_util_pct,
            drawdown_pct,
        });

        // Persist to SQLite
        let data_json = serde_json::json!({
            "report_date": report_date,
            "entries": entries,
            "exits": exits,
            "fees": fees,
            "net_pnl": net_pnl,
            "capital_util_pct": capital_util_pct,
            "drawdown_pct": drawdown_pct,
            "current_capital": current_capital,
            "initial_capital": initial_capital,
            "open_positions": open_count,
            "total_value": total_value,
        });
        let data_str = data_json.to_string();
        self.state_db.save_daily_report(
            &report_date, now, entries, exits, fees, net_pnl,
            capital_util_pct, drawdown_pct, Some(&data_str),
        );
    }

    /// Periodic housekeeping tasks. Run every tick regardless of circuit breaker state.
    fn do_periodic_tasks(&mut self, now: f64) {
        // --- Postponement check ---
        if (now - self.last_postponement_check) >= self.cfg.postponement_check_interval {
            if self.engine.pm_open_count() > 0 && self.postponement_detector.is_some() {
                self.check_postponements();
            }
            self.last_postponement_check = now;
        }

        // --- Sports WS: prune ended games periodically ---
        let prune_interval = self.cfg.sports_ws_prune_interval_days as f64 * 86400.0;
        if prune_interval > 0.0 && (now - self.last_sports_prune) >= prune_interval {
            if let Some(ref sports) = self.sports_ws {
                let pruned = sports.prune_ended(prune_interval);
                if pruned > 0 {
                    tracing::info!("Sports WS: pruned {} ended games, {} remaining",
                        pruned, sports.game_count());
                }
            }
            self.last_sports_prune = now;
        }

        // --- Constraint rebuild ---
        if (now - self.last_constraint_rebuild) >= self.cfg.constraint_rebuild_interval {
            match self.scanner.scan() {
                Ok(result) => {
                    self.circuit_breaker.record_api_success(now);
                    self.ingest_scan_result(&result);
                    let (all_assets, constraint_to_assets) = self.detect_constraints();
                    if !all_assets.is_empty() {
                        if self.cfg.use_tiered_ws {
                            self.engine.update_tier_b(constraint_to_assets.clone());
                            self.last_constraint_to_assets = constraint_to_assets;
                        } else {
                            self.engine.start(all_assets, 0, &self.cfg.dashboard_bind);
                        }
                    }
                }
                Err(e) => {
                    self.circuit_breaker.record_error(now);
                    tracing::warn!("Constraint rebuild scan failed: {}", e);
                }
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
            self.stale_sweeps += 1;
            self.stale_assets_swept += stale.len() as u64;
            if !stale.is_empty() {
                tracing::trace!("Stale assets: {} > {:.0}s old", stale.len(), self.cfg.stale_asset_threshold);
            }
            self.last_stale_sweep = now;
        }

        // --- Tiered WS: flush new markets + periodic maintenance ---
        if self.cfg.use_tiered_ws {
            let bursts = self.engine.tiered_flush_new_markets();
            for burst in &bursts {
                tracing::info!("New market burst: event='{}' ({} markets)",
                    burst.event_title, burst.markets.len());
                let all_burst_assets: Vec<String> = burst.markets.iter()
                    .flat_map(|m| m.asset_ids.iter().cloned())
                    .collect();
                if !all_burst_assets.is_empty() {
                    let synthetic_cid = format!("newmkt_{}", truncate(&burst.event_id, 32));
                    self.engine.tiered_add_new_market_constraint(synthetic_cid, all_burst_assets);
                }
            }
            self.engine.tiered_periodic_maintenance();
        }

        // --- API resolution poll (safety net for missed WS events) ---
        if (now - self.last_api_resolution_check) >= self.cfg.api_resolution_interval {
            if self.engine.pm_open_count() > 0 {
                // Capture pid→constraint_id before API resolves them
                let pid_to_cid: HashMap<String, String> = if self.strategy_tracker.is_some() {
                    let pm = self.engine.positions.lock();
                    pm.open_positions().values()
                        .map(|p| {
                            let cid = p.metadata.get("constraint_id")
                                .and_then(|v| v.as_str()).unwrap_or("").to_string();
                            (p.position_id.clone(), cid)
                        })
                        .collect()
                } else { HashMap::new() };

                let (results, disputes) = self.engine.check_api_resolutions();
                if !results.is_empty() {
                    self.held_ids_cache = None;
                    self.circuit_breaker.record_api_success(now);
                    for r in &results {
                        tracing::info!("API RESOLUTION: {} → winner={}, profit=${:.4}",
                            r.position_id, r.winning_market_id, r.profit);

                        // Forward to strategy tracker
                        if let Some(cid) = pid_to_cid.get(&r.position_id) {
                            if !cid.is_empty() {
                                if let Some(ref mut tracker) = self.strategy_tracker {
                                    tracker.resolve_with_db(cid, &r.winning_market_id, &self.state_db);
                                }
                            }
                        }

                        let _ = self.notifier.send(&NotifyEvent::PositionResolved {
                            position_id: r.position_id.clone(),
                            profit: r.profit,
                            method: format!("api_resolution({})", r.winning_market_id),
                        });
                    }
                } else {
                    // No resolutions but API was reachable
                    self.circuit_breaker.record_api_success(now);
                }

                // R14: Log and flag disputed positions
                for d in &disputes {
                    tracing::warn!("UMA DISPUTE: position={} market={} status={} — excluding from replacement",
                        d.position_id, d.market_id, d.uma_status);
                    // Store dispute flag in position metadata
                    let mut pm = self.engine.positions.lock();
                    if let Some(p) = pm.open_positions_mut().get_mut(&d.position_id) {
                        p.metadata.insert("uma_disputed".into(), serde_json::json!(true));
                        p.metadata.insert("uma_status".into(), serde_json::json!(d.uma_status));
                    }
                }
            } else {
                // No positions to check, but mark API as reachable
                self.circuit_breaker.record_api_success(now);
            }
            self.last_api_resolution_check = now;
        }

        // --- B4.0: Periodic reconciliation (every 5 min) ---
        if (now - self.last_reconciliation) >= 300.0 {
            if self.engine.pm_open_count() > 0 {
                let report = self.engine.reconcile_periodic(self.cfg.min_trade_size);
                if !report.passed {
                    tracing::warn!("B4.0 reconciliation: {} critical, {} warnings",
                        report.critical_count(), report.warning_count());
                }
            }
            self.last_reconciliation = now;
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

        // --- C4: Daily P&L report (at midnight UTC) ---
        {
            let current_day = (now / 86400.0).floor() as i64;
            if current_day > self.last_daily_report_day {
                self.generate_daily_report(now, self.last_daily_report_day);
                self.last_daily_report_day = current_day;
            }
        }

        // --- C1.1: POL gas balance check ---
        if self.gas_monitor.is_enabled()
            && (now - self.last_gas_check) >= self.gas_monitor.check_interval()
        {
            match self.gas_monitor.check_balance(&self.engine.http_client) {
                GasCheckResult::Ok(bal) => {
                    tracing::info!("POL gas balance: {:.4} POL (healthy)", bal);
                }
                GasCheckResult::Warning(bal) => {
                    tracing::warn!("POL gas balance LOW: {:.4} POL (threshold {:.1})",
                        bal, self.gas_monitor.min_balance());
                    let _ = self.notifier.send(&NotifyEvent::Error {
                        message: format!("⚠️ POL gas balance low: {:.4} POL (warn < {:.1})",
                            bal, self.gas_monitor.min_balance()),
                    });
                }
                GasCheckResult::Critical(bal) => {
                    tracing::error!("POL gas balance CRITICAL: {:.4} POL (threshold {:.2})",
                        bal, self.gas_monitor.critical_balance());
                    let reason = format!("POL gas critically low: {:.4} POL (critical < {:.2})",
                        bal, self.gas_monitor.critical_balance());
                    if let Some(trip_msg) = self.circuit_breaker.check_gas_critical(&reason, now) {
                        tracing::error!("CIRCUIT BREAKER TRIPPED: {}", trip_msg);
                        let _ = self.notifier.send(&NotifyEvent::CircuitBreaker { reason: trip_msg });
                    }
                }
                GasCheckResult::Error(e) => {
                    tracing::warn!("Gas balance check failed: {}", e);
                }
            }
            self.last_gas_check = now;
        }

        // --- B4.6: USDC.e balance check ---
        if self.usdc_monitor.is_enabled()
            && (now - self.last_usdc_check) >= self.usdc_monitor.check_interval()
        {
            let accounting_cash = self.engine.accounting.lock().cash_balance();
            match self.usdc_monitor.check_balance_with_auth(
                &self.engine.http_client, accounting_cash, self.cfg.clob_auth.as_ref(),
            ) {
                UsdcCheckResult::Ok { on_chain, .. } => {
                    tracing::info!("USDC.e balance: ${:.2} (accounting: ${:.2})", on_chain, accounting_cash);
                }
                UsdcCheckResult::DriftWarning { on_chain, accounting, drift } => {
                    tracing::warn!("USDC.e drift: on-chain=${:.2} vs accounting=${:.2} (${:.2} drift)",
                        on_chain, accounting, drift);
                    let _ = self.notifier.send(&NotifyEvent::Error {
                        message: format!("USDC drift: on-chain=${:.2} vs accounting=${:.2} (${:.2} drift)",
                            on_chain, accounting, drift),
                    });
                }
                UsdcCheckResult::LowBalance(bal) => {
                    tracing::warn!("USDC.e balance LOW: ${:.2}", bal);
                    let _ = self.notifier.send(&NotifyEvent::Error {
                        message: format!("USDC.e balance low: ${:.2}", bal),
                    });
                }
                UsdcCheckResult::CriticalBalance(bal) => {
                    tracing::error!("USDC.e balance CRITICAL: ${:.2}", bal);
                    let reason = format!("USDC.e critically low: ${:.2}", bal);
                    if let Some(trip_msg) = self.circuit_breaker.check_gas_critical(&reason, now) {
                        tracing::error!("CIRCUIT BREAKER TRIPPED: {}", trip_msg);
                        let _ = self.notifier.send(&NotifyEvent::CircuitBreaker { reason: trip_msg });
                    }
                }
                UsdcCheckResult::Error(e) => {
                    tracing::warn!("USDC balance check failed: {}", e);
                }
            }
            self.last_usdc_check = now;

            // R4: Geoblock check — piggyback on USDC check interval
            match self.engine.http_client.get("https://clob.polymarket.com/time")
                .timeout(std::time::Duration::from_secs(10))
                .send()
            {
                Ok(resp) if resp.status().as_u16() == 403 => {
                    tracing::error!("GEOBLOCK DETECTED: CLOB API returned 403 Forbidden");
                    let _ = self.notifier.send(&NotifyEvent::Error {
                        message: "GEOBLOCK: CLOB API returned 403 — VPS IP may be blocked".into(),
                    });
                    if let Some(trip_msg) = self.circuit_breaker.check_gas_critical(
                        "Geoblock: CLOB API 403 Forbidden", now,
                    ) {
                        tracing::error!("CIRCUIT BREAKER TRIPPED: {}", trip_msg);
                        let _ = self.notifier.send(&NotifyEvent::CircuitBreaker { reason: trip_msg });
                    }
                }
                Ok(_) => {} // 200 OK — not geoblocked
                Err(e) => {
                    tracing::debug!("CLOB geoblock check failed (network): {}", e);
                }
            }
        }

        // --- Daily CLOB API key refresh (keys can expire) ---
        const CLOB_AUTH_REFRESH_SECS: f64 = 86400.0; // 24 hours
        if (now - self.last_clob_auth_refresh) >= CLOB_AUTH_REFRESH_SECS {
            let secrets_path = self.cfg.workspace.join("config").join("secrets.yaml");
            if let Ok(raw) = std::fs::read_to_string(&secrets_path) {
                let secrets: serde_json::Value = serde_yaml_ng::from_str(&raw).unwrap_or_default();
                let pk_str = secrets.pointer("/polymarket/private_key")
                    .and_then(|v: &serde_json::Value| v.as_str()).unwrap_or("");
                if !pk_str.is_empty() {
                    if let Ok(signer) = rust_engine::signing::OrderSigner::new(pk_str) {
                        match signer.create_or_derive_api_key("https://clob.polymarket.com") {
                            Ok(creds) => {
                                let addr = format!("{:#x}", signer.address());
                                match rust_engine::signing::ClobAuth::new(&creds, &addr) {
                                    Ok(auth) => {
                                        self.cfg.clob_auth = Some(auth);
                                        tracing::info!("CLOB API key refreshed (key={}...)", &creds.api_key[..8.min(creds.api_key.len())]);
                                    }
                                    Err(e) => tracing::warn!("CLOB auth rebuild failed: {}", e),
                                }
                            }
                            Err(e) => tracing::warn!("CLOB API key refresh failed: {}", e),
                        }
                    }
                }
            }
            self.last_clob_auth_refresh = now;
        }

        // --- Stats ---
        if (now - self.last_stats_log) >= self.cfg.stats_log_interval {
            self.log_stats();
            self.last_stats_log = now;
        }
    }

    // --- Market loading ---

    fn load_markets_cached(&mut self) {
        let cached = self.scanner.load_cached();
        if cached.count > 0 {
            self.ingest_scan_result(&cached);
        }
    }

    fn ingest_scan_result(&mut self, result: &rust_engine::scanner::ScanResult) {
        // P5: clear+rebuild is correct; pre-allocate to avoid rehashing
        self.market_lookup.clear();
        self.market_lookup.reserve(result.markets.len());
        for m in &result.markets {
            if let Some(mid) = m.get("market_id").and_then(|v| v.as_str()) {
                self.market_lookup.insert(mid.to_string(), m.clone());
            }
        }
        // Load instruments from market data (token_id → tick_size, rounding, neg_risk, etc.)
        self.engine.load_instruments(&self.market_lookup);
        // Persist instruments to SQLite so they survive restart (B4 warm cache)
        self.engine.instruments.save_to_db(&self.state_db);
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

        // Filter to top N constraints by composite score for Tier B.
        // Score = spread / time_factor  (lower = better).
        // spread = |price_sum - 1.0|  (tighter = better)
        // time_factor = max(hours_to_resolution, 1.0)  (sooner = better, divides score down)
        // So constraints with tight spreads resolving soon rank highest.
        let top_n = self.cfg.tier_b_top_n_constraints;
        let constraint_to_assets = if top_n > 0 && result.constraint_to_assets.len() > top_n {
            let spread = &result.constraint_spread;
            let end_ts = &result.constraint_end_ts;
            let now_ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);

            // P7: Sort references, clone only the top N keys we keep
            let mut ranked: Vec<&String> = result.constraint_to_assets.keys().collect();
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
            // P6: Build filtered map directly from references — no full clone
            let total = result.constraint_to_assets.len();
            let filtered: HashMap<String, Vec<String>> = ranked.into_iter()
                .take(top_n)
                .filter_map(|k| {
                    result.constraint_to_assets.get(k)
                        .map(|v| (k.clone(), v.clone()))
                })
                .collect();
            let removed = total - filtered.len();
            let hot_assets: usize = filtered.values().map(|v| v.len()).sum();
            tracing::info!("Tier B filter: top {} constraints ({} assets), {} demoted to Tier A (scored by spread/time_to_resolve)",
                top_n, hot_assets, removed);
            filtered
        } else {
            result.constraint_to_assets.clone()
        };

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

        // Restore instruments from SQLite (warm cache for gap before scanner runs)
        self.engine.instruments.load_from_db(&self.state_db);

        // Restore accounting ledger from checkpoint (overwrites the fresh one from init_positions)
        if let Some(acct_json) = self.state_db.load_checkpoint("accounting_ledger") {
            match rust_engine::accounting::AccountingLedger::deserialize_json(&acct_json) {
                Ok(ledger) => {
                    *self.engine.accounting.lock() = ledger;
                    tracing::info!("Accounting ledger restored from checkpoint");
                }
                Err(e) => tracing::warn!("Failed to restore accounting ledger: {}", e),
            }
        }

        // Restore circuit breaker peak from persistence
        let total_value = self.engine.total_value();
        if let Some(peak) = self.state_db.get_scalar("cb_peak_total_value") {
            // Use whichever is higher: persisted peak or current value
            self.circuit_breaker.set_peak(peak.max(total_value));
        } else {
            self.circuit_breaker.set_peak(total_value);
        }
        // Reset API success timestamp so we don't false-trip on startup
        self.circuit_breaker.record_api_success(now_secs());

        let snap = self.engine.dashboard_snapshot();
        tracing::info!("State loaded: ${:.2} capital, {} open, {} closed (CB peak=${:.2})",
            snap.current_capital, snap.open_count, snap.closed_count,
            self.circuit_breaker.peak_total_value());
    }

    fn save_state(&mut self) {
        let t0 = Instant::now();

        let snap = self.engine.dashboard_snapshot();
        let perf = self.engine.get_performance_metrics();

        self.state_db.set_scalars(&[
            ("current_capital".to_string(), snap.current_capital),
            ("initial_capital".to_string(), snap.initial_capital),
            ("cb_peak_total_value".to_string(), self.circuit_breaker.peak_total_value()),
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
            // P2: clone required — live_ids outlive the position lock for set difference below
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

        // Safety guard: refuse to wipe all positions if DB had some but runtime has none.
        // This prevents accidental state loss from a bad restart or missed load_state.
        if live_ids.is_empty() && !db_open_ids.is_empty() {
            tracing::error!(
                "STATE GUARD: refusing to delete {} open positions (runtime has 0). \
                 Skipping save to prevent state loss. Check load_state or .db.bak backup.",
                db_open_ids.len()
            );
            return;
        }

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

        // Checkpoint accounting ledger + flush journal entries to SQLite
        {
            let mut acct = self.engine.accounting.lock();
            let unflushed = acct.unflushed_entries();
            if !unflushed.is_empty() {
                self.state_db.save_journal_entries(unflushed);
                let n = unflushed.len();
                acct.mark_flushed(n);
            }
            let acct_json = acct.serialize_json();
            drop(acct); // release lock before checkpoint write
            self.state_db.save_checkpoint("accounting_ledger", &acct_json);
        }

        // Save strategy tracker state + update dashboard summary
        if let Some(ref mut tracker) = self.strategy_tracker {
            tracker.prune_old_closed_with_db(&self.state_db);
            tracker.save_state(&self.state_db);
            *self.engine.strategy_summary.lock() = tracker.build_summary();
        }

        // Disk mirrors run in a background thread to avoid blocking the tick loop.
        // Only spawn if previous save has completed.
        if self.disk_save_handle.as_ref().map_or(true, |h| h.is_finished()) {
            let db = Arc::clone(&self.state_db);
            let rv = self.resolution_validator.as_ref().map(Arc::clone);
            let pd = self.postponement_detector.as_ref().map(Arc::clone);
            let sc = Arc::clone(&self.scanner);
            let sw = self.sports_ws.as_ref().map(Arc::clone);
            let n_open = open_rows.len();
            self.disk_save_handle = Some(std::thread::spawn(move || {
                db.mirror_to_disk();
                if let Some(ref rv) = rv { rv.mirror_to_disk(); }
                if let Some(ref pd) = pd { pd.mirror_to_disk(); }
                sc.mirror_to_disk();
                if let Some(ref sw) = sw { sw.mirror_to_disk(); }
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

            // R2 risk mitigation: flag suspiciously large arbs
            if opp.expected_profit_pct >= self.cfg.suspicious_profit_threshold {
                tracing::warn!(
                    "SUSPICIOUS ARB: {:.1}% profit on {}... — large arbs are rare in liquid markets, may indicate detection error",
                    opp.expected_profit_pct * 100.0, truncate(&opp.constraint_id, 30),
                );
            }

            // R16: negRisk correlated exposure cap — scale back position size to fit
            let effective_cap = if opp.neg_risk && self.cfg.max_neg_risk_exposure_pct > 0.0 {
                let total_val = self.engine.total_value();
                let nr_exposure = self.engine.positions.lock().neg_risk_exposure();
                if total_val > 0.0 {
                    let available_nr = (total_val * self.cfg.max_neg_risk_exposure_pct) - nr_exposure;
                    if available_nr < self.cfg.min_trade_size {
                        tracing::debug!("SKIP (negRisk cap full): {}... ${:.2} available < ${:.2} min",
                            truncate(&opp.constraint_id, 30), available_nr, self.cfg.min_trade_size);
                        continue;
                    }
                    let capped = cap.min(available_nr);
                    if capped < cap {
                        tracing::debug!("negRisk cap: scaling {}... from ${:.2} to ${:.2} (nr_exposure=${:.2})",
                            truncate(&opp.constraint_id, 30), cap, capped, nr_exposure);
                    }
                    capped
                } else { cap }
            } else { cap };

            // B3: negRisk capital efficiency — for sell arbs on negRisk markets,
            // collateral = $1.00/unit instead of sum(NO prices), so we can size larger
            let is_sell = opp.is_sell;

            // Scale to dynamic capital (using effective_cap which may be reduced by negRisk limit)
            let old_cap = opp.optimal_bets.values().sum::<f64>();
            let scale = if old_cap > 0.0 { effective_cap / old_cap } else { 1.0 };
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
                    // E5: Store book depth at entry for fill quality analysis
                    {
                        let mut pm = self.engine.positions.lock();
                        if let Some(p) = pm.open_positions_mut().get_mut(&pos.position_id) {
                            p.metadata.insert("entry_min_depth_usd".into(), serde_json::json!(opp.min_leg_depth_usd));
                            p.metadata.insert("entry_score".into(), serde_json::json!(score));
                        }
                    }
                    tracing::info!("ENTER: {}... | ${:.2} | exp ${:.2} | {:.1}h | score={:.6} | depth=${:.0}",
                        truncate(&opp.constraint_id, 30),
                        effective_cap, scaled_profit, hours, score, opp.min_leg_depth_usd);
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
                        // R14: Skip disputed positions — don't replace while UMA dispute active
                        if pos.metadata.get("uma_disputed").and_then(|v| v.as_bool()).unwrap_or(false) {
                            tracing::debug!("SKIP replacement for {} — UMA dispute active", truncate(pid, 20));
                            continue;
                        }
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

                            if worst.as_ref().map_or(true, |w| rem_score < w.1) {
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
                                    // P2: clone required — assets outlive the position lock
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
                                truncate(&worst_pid, 30), net, profit);

                            // Enter replacement with chain tracking
                            let cap = dynamic_capital(self.engine.total_value(), self.cfg.capital_pct);
                            let old_cap = best_opp.optimal_bets.values().sum::<f64>();
                            let scale = if old_cap > 0.0 { cap / old_cap } else { 1.0 };
                            let scaled_bets: HashMap<String, f64> = best_opp.optimal_bets.iter()
                                .map(|(k, v)| (k.clone(), v * scale))
                                .collect();
                            let is_sell = best_opp.is_sell;

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
                                truncate(&best_opp.constraint_id, 30),
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
                truncate(&opp.constraint_id, 30),
                opp.min_leg_depth_usd, self.cfg.min_depth_per_leg);
            return false;
        }

        // B1.3: Book staleness — check that all legs have fresh book data
        if self.cfg.max_book_staleness_secs > 0.0 {
            if let Some(constraint) = self.engine.constraints.get(&opp.constraint_id) {
                for mref in &constraint.markets {
                    let asset_id = if opp.is_sell { &mref.no_asset_id } else { &mref.yes_asset_id };
                    let age = self.engine.book.get_book_age_secs(asset_id);
                    if age > self.cfg.max_book_staleness_secs {
                        if age == f64::MAX {
                            tracing::debug!("SKIP (no book): {}... asset {} has no book data",
                                truncate(&opp.constraint_id, 30),
                                truncate(asset_id, 16));
                        } else {
                            tracing::debug!("SKIP (stale book): {}... asset {} is {:.1}s old",
                                truncate(&opp.constraint_id, 30),
                                truncate(asset_id, 16), age);
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
                    tracing::info!("SKIP (unrepresented outcome): {}...", truncate(&opp.constraint_id, 30));
                    return false;
                }
                if let Ok(vd) = chrono::NaiveDate::parse_from_str(&validation.latest_resolution_date, "%Y-%m-%d") {
                    let today = chrono::Utc::now().date_naive();
                    let days = (vd - today).num_days();
                    if days > self.cfg.max_days_entry as i64 {
                        tracing::debug!("SKIP (AI date): {} resolves in {}d > {}d",
                            truncate(&opp.constraint_id, 30),
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
                truncate(&exit.position_id, 40), exit.ratio);

            // Capture tiered WS exit info before liquidation
            let exit_info = if self.cfg.use_tiered_ws {
                let pm = self.engine.positions.lock();
                pm.get_position(&exit.position_id).map(|p| {
                    let cid = p.metadata.get("constraint_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    // P2: clone required — assets outlive the position lock
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
                // P2: clone required — market_ids outlive the position lock
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
                            expected_date = Some(truncate(ed, 10).to_string());
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

            // Sports WS pre-screen: check if we already know the game status
            if let Some(ref sports) = self.sports_ws {
                use rust_engine::sports_ws::SportsCheckResult;
                match sports.check_postponement(market_names) {
                    SportsCheckResult::Postponed { game_id, status, league } => {
                        let display_name = market_names.first()
                            .map(|n| truncate(n, 40))
                            .unwrap_or("?");
                        tracing::info!("Sports WS postponement: {}... | {} ({}) game={}",
                            display_name, status, league, game_id);
                        checked += 1;
                        continue; // Skip AI call — Sports WS already confirmed
                    }
                    SportsCheckResult::Active { game_id, status } => {
                        tracing::debug!("Sports WS active: {}... status={} game={} — skipping AI",
                            truncate(pid, 20), status, game_id);
                        continue; // Skip AI call — game is live or finished
                    }
                    SportsCheckResult::NoMatch => {
                        // Fall through to AI call below
                    }
                }
            }

            if let Some(result) = pd.check(pid, market_names, &expected_date) {
                if result.effective_resolution_date.is_some() {
                    let display_name = market_names.first()
                        .map(|n| truncate(n, 40))
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
        let snap = self.engine.dashboard_snapshot();
        let cap = snap.current_capital;
        let npos = snap.open_count;
        let gas_str = self.gas_monitor.last_balance()
            .map(|b| format!(" POL={:.4}", b))
            .unwrap_or_default();
        let usdc_str = self.usdc_monitor.last_on_chain_balance()
            .map(|b| format!(" USDC={:.2}", b))
            .unwrap_or_default();
        let gas_str = format!("{}{}", gas_str, usdc_str);

        let (lat_p50, lat_p95, lat_max) = if !self.recent_latencies.is_empty() {
            let mut lats: Vec<f64> = self.recent_latencies.iter().copied().collect();
            lats.sort_by(|a, b| a.total_cmp(b));
            let p50 = lats[lats.len() / 2];
            let p95 = lats[((lats.len() as f64 * 0.95).ceil() as usize).saturating_sub(1).min(lats.len() - 1)];
            let max = *lats.last().unwrap_or(&0.0);
            (p50, p95, max)
        } else {
            (0.0, 0.0, 0.0)
        };

        if let Some(ts) = self.engine.tiered_stats() {
            tracing::info!(
                "[iter {}] Capital=${:.2} positions={}{} | TieredWS: B={} conns/{} assets/{} hot, C={} conns/{} assets/{} pos | msgs={} urg={} bg={} lat_μs p50={:.0} p95={:.0} max={:.0}",
                self.iteration, cap, npos, gas_str,
                ts.tier_b_connections, ts.tier_b_assets, ts.tier_b_hot_constraints,
                ts.tier_c_connections, ts.tier_c_assets, ts.tier_c_position_assets,
                ts.total_msgs, q_urg, q_bg, lat_p50, lat_p95, lat_max,
            );
        } else {
            let ws = self.engine.stats();
            tracing::info!(
                "[iter {}] Capital=${:.2} positions={}{} | WS: subs={} msgs={} live={} urgent={} bg={} lat_μs p50={:.0} p95={:.0} max={:.0}",
                self.iteration, cap, npos, gas_str,
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
        let engine_status = if self.circuit_breaker.is_tripped() { "CIRCUIT_BREAKER" } else { "running" };
        self.engine.update_dashboard_metrics(
            self.iteration,
            lat_p50 as u64, lat_p95 as u64, lat_max as u64,
            "running", &chrono::Utc::now().format("%d/%m/%Y %H:%M:%S").to_string(),
            engine_status, &chrono::Utc::now().format("%d/%m/%Y %H:%M:%S").to_string(),
            self.gas_monitor.last_balance(),
            self.usdc_monitor.last_on_chain_balance(),
        );
        self.engine.update_stress_counters(
            self.evals_total, self.opps_found,
            self.stale_sweeps, self.stale_assets_swept,
        );

        // Update Sports WS metrics
        if let Some(ref sports) = self.sports_ws {
            let mut m = self.engine.engine_metrics.lock();
            m.sports_ws_connected = sports.is_connected();
            m.sports_ws_games = sports.game_count() as u64;
            m.sports_ws_msgs = sports.total_messages();
            m.sports_ws_postponed = sports.postponed_count() as u64;
        }

        // Log circuit breaker state if tripped
        if let Some((reason, ts)) = self.circuit_breaker.trip_info() {
            tracing::warn!("Circuit breaker TRIPPED at {:.0}: {}", ts, reason);
        }
    }
}

// --- Helpers ---

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Parse entry_timestamp — supports ISO 8601 or bare Unix float.
fn parse_entry_ts(entry: &str) -> Option<f64> {
    if let Ok(ts) = entry.parse::<f64>() {
        return Some(ts);
    }
    chrono::DateTime::parse_from_rfc3339(entry)
        .ok()
        .map(|dt| dt.timestamp() as f64 + dt.timestamp_subsec_millis() as f64 / 1000.0)
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(entry, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|ndt| ndt.and_utc().timestamp() as f64)
        })
}

fn parse_end_date_ts(s: &str) -> f64 {
    if s.is_empty() { return 0.0; }
    chrono::DateTime::parse_from_rfc3339(s)
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(&s.replace("Z", "+00:00")))
        .map(|dt| dt.timestamp() as f64)
        .unwrap_or(0.0)
}
