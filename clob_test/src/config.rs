/// Merged Shadow A-F configuration for maximum arb detection speed.
///
/// Takes the most permissive value from all 6 parameter sets so the test
/// harness finds arbitrage opportunities as quickly as possible.

use std::path::Path;

/// Merged test configuration — widest gates from all Shadow instances.
#[derive(Debug, Clone)]
pub struct MergedTestConfig {
    pub capital_per_trade_pct: f64,
    pub max_concurrent_positions: usize,
    pub min_profit_threshold: f64,
    pub max_profit_threshold: f64,
    pub min_resolution_time_secs: f64,
    pub max_days_to_resolution: u32,
    pub replacement_cooldown_seconds: f64,
    pub constraint_rebuild_interval: f64,
    pub replacement_protection_hours: f64,
    pub initial_capital: f64,
    pub max_position_size: f64,
    pub max_exposure_per_market: f64,
    pub taker_fee_rate: f64,
}

impl Default for MergedTestConfig {
    fn default() -> Self {
        Self {
            capital_per_trade_pct: 0.05,        // Shadow-A (smallest % = most trades from $50)
            max_concurrent_positions: 50,        // Shadow-F (spec)
            min_profit_threshold: 0.01,          // Shadow-F: 1% (widest gate)
            max_profit_threshold: 0.30,          // All instances
            min_resolution_time_secs: 60.0,      // Shadow-F (spec)
            max_days_to_resolution: 90,          // Shadow-A (widest)
            replacement_cooldown_seconds: 10.0,  // Shadow-F (spec)
            constraint_rebuild_interval: 60.0,   // Shadow-F (spec)
            replacement_protection_hours: 0.1,   // Shadow-F: 6 minutes
            initial_capital: 50.0,               // Test budget
            max_position_size: 5.0,              // Safety: max $5 per trade (10% of $50)
            max_exposure_per_market: 10.0,        // Safety: max $10 per market
            taker_fee_rate: 0.02,                // 2% taker fee
        }
    }
}

impl MergedTestConfig {
    /// Build config by reading instance overlay files and taking the most permissive values.
    /// Falls back to spec defaults for Shadow-F which may not be on disk yet.
    pub fn from_workspace(workspace: &Path) -> Self {
        let mut cfg = Self::default();
        let instances_dir = workspace.join("config").join("instances");

        // Read all shadow instance overlays and merge
        for name in &["shadow-a", "shadow-b", "shadow-c", "shadow-d", "shadow-e", "shadow-f"] {
            let path = instances_dir.join(format!("{}.yaml", name));
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(val) = serde_yaml_ng::from_str::<serde_json::Value>(&content) {
                    Self::merge_overlay(&mut cfg, &val);
                }
            }
        }

        // Override with test-specific safety limits
        cfg.initial_capital = 50.0;
        cfg.max_position_size = 5.0;  // Never risk more than $5 per trade
        cfg
    }

    fn merge_overlay(cfg: &mut MergedTestConfig, val: &serde_json::Value) {
        if let Some(arb) = val.get("arbitrage") {
            // Take the most permissive: MIN for thresholds, MAX for counts/ranges
            if let Some(v) = arb.get("capital_per_trade_pct").and_then(|v| v.as_f64()) {
                if v < cfg.capital_per_trade_pct { cfg.capital_per_trade_pct = v; }
            }
            if let Some(v) = arb.get("max_concurrent_positions").and_then(|v| v.as_u64()) {
                if v as usize > cfg.max_concurrent_positions { cfg.max_concurrent_positions = v as usize; }
            }
            if let Some(v) = arb.get("min_profit_threshold").and_then(|v| v.as_f64()) {
                if v < cfg.min_profit_threshold { cfg.min_profit_threshold = v; }
            }
            if let Some(v) = arb.get("max_profit_threshold").and_then(|v| v.as_f64()) {
                if v > cfg.max_profit_threshold { cfg.max_profit_threshold = v; }
            }
            if let Some(v) = arb.get("min_resolution_time_secs").and_then(|v| v.as_f64()) {
                if v < cfg.min_resolution_time_secs { cfg.min_resolution_time_secs = v; }
            }
            if let Some(v) = arb.get("max_days_to_resolution").and_then(|v| v.as_u64()) {
                if v as u32 > cfg.max_days_to_resolution { cfg.max_days_to_resolution = v as u32; }
            }
            if let Some(v) = arb.get("replacement_cooldown_seconds").and_then(|v| v.as_f64()) {
                if v < cfg.replacement_cooldown_seconds { cfg.replacement_cooldown_seconds = v; }
            }
        }
        if let Some(engine) = val.get("engine") {
            if let Some(v) = engine.get("constraint_rebuild_interval_seconds").and_then(|v| v.as_f64()) {
                if v < cfg.constraint_rebuild_interval { cfg.constraint_rebuild_interval = v; }
            }
        }
    }
}
