/// Virtual portfolio strategy tracker.
///
/// Maintains 6 virtual portfolios (Shadow A–F) that observe the same
/// opportunity stream from `evaluate_batch()`. Each portfolio applies its
/// own strategy gates to decide what it *would* trade, tracking P&L via
/// virtual positions that resolve from the same WS/API events as real ones.
///
/// This replaces the need to run 6 separate instances (with 100+ WS
/// connections) — one instance, six virtual portfolios.

use std::collections::HashMap;
use std::path::Path;
use serde::{Serialize, Deserialize};
use crate::eval::{EvalConfig, Opportunity};
use crate::state::StateDB;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyConfig {
    pub name: String,
    pub label: String,
    pub capital_per_trade_pct: f64,
    pub max_concurrent_positions: usize,
    pub max_position_size: f64,
    pub min_profit_threshold: f64,
    pub max_profit_threshold: f64,
    pub min_resolution_time_secs: f64,
    pub max_days_to_resolution: f64,
    pub replacement_cooldown_seconds: f64,
    pub max_exposure_per_market: f64,
    pub initial_capital: f64,
}

/// Load strategy configs from config/instances/shadow-{a..f}.yaml.
pub fn load_strategy_configs(workspace: &Path) -> Vec<StrategyConfig> {
    let instances_dir = workspace.join("config").join("instances");
    let names = [
        ("shadow-a", "Max Diversification"),
        ("shadow-b", "Baseline"),
        ("shadow-c", "Moderate Concentration"),
        ("shadow-d", "High Concentration"),
        ("shadow-e", "Max Concentration"),
        ("shadow-f", "Fast Markets"),
    ];

    let mut configs = Vec::new();
    for (file_stem, label) in &names {
        let path = instances_dir.join(format!("{}.yaml", file_stem));
        let yaml_str = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => {
                tracing::debug!("Strategy config not found: {}", path.display());
                continue;
            }
        };
        let val: serde_json::Value = match serde_yaml_ng::from_str(&yaml_str) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("Failed to parse {}: {}", path.display(), e);
                continue;
            }
        };
        let arb = &val["arbitrage"];
        let display_name = format!("Shadow-{}", file_stem.chars().last().unwrap_or('?').to_uppercase());
        configs.push(StrategyConfig {
            name: display_name,
            label: label.to_string(),
            capital_per_trade_pct: arb["capital_per_trade_pct"].as_f64().unwrap_or(0.10),
            max_concurrent_positions: arb["max_concurrent_positions"].as_u64().unwrap_or(20) as usize,
            max_position_size: arb["max_position_size"].as_f64().unwrap_or(1000.0),
            min_profit_threshold: arb["min_profit_threshold"].as_f64().unwrap_or(0.03),
            max_profit_threshold: arb["max_profit_threshold"].as_f64().unwrap_or(0.30),
            min_resolution_time_secs: arb["min_resolution_time_secs"].as_f64().unwrap_or(300.0),
            max_days_to_resolution: arb["max_days_to_resolution"].as_f64().unwrap_or(60.0),
            replacement_cooldown_seconds: arb["replacement_cooldown_seconds"].as_f64().unwrap_or(60.0),
            max_exposure_per_market: arb["max_exposure_per_market"].as_f64().unwrap_or(500.0),
            initial_capital: 1000.0,
        });
    }
    configs
}

// ---------------------------------------------------------------------------
// Virtual position types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualPosition {
    pub constraint_id: String,
    pub market_ids: Vec<String>,
    #[serde(default)]
    pub market_names: Vec<String>,
    pub short_name: String,
    pub capital_deployed: f64,
    pub expected_profit_pct: f64,
    pub entry_ts: f64,
    pub hours_to_resolve: f64,
    pub is_sell: bool,
    #[serde(default)]
    pub method: String,
    /// Entry prices per market_id (YES ask at entry).
    pub entry_prices: HashMap<String, f64>,
    /// NO prices per market_id at entry (needed for sell_all share calc).
    #[serde(default)]
    pub entry_no_prices: HashMap<String, f64>,
    /// Bet amounts per market_id.
    pub bet_amounts: HashMap<String, f64>,
}

#[derive(Debug, Clone)]
pub struct ClosedVirtualPosition {
    pub capital_deployed: f64,
    pub actual_profit: f64,
    pub actual_profit_pct: f64,
    pub entry_ts: f64,
    pub close_ts: f64,
    pub is_win: bool,
    pub short_name: String,
    pub method: String,
    pub is_sell: bool,
}

// ---------------------------------------------------------------------------
// Virtual portfolio
// ---------------------------------------------------------------------------

pub struct VirtualPortfolio {
    pub config: StrategyConfig,
    pub current_capital: f64,
    pub open_positions: HashMap<String, VirtualPosition>,
    pub market_exposure: HashMap<String, f64>,
    pub closed_positions: Vec<ClosedVirtualPosition>,
    pub total_entered: u64,
    pub total_wins: u64,
    pub total_losses: u64,
    pub evals_seen: u64,
    pub evals_rejected: u64,
}

impl VirtualPortfolio {
    fn new(config: StrategyConfig) -> Self {
        let capital = config.initial_capital;
        Self {
            config,
            current_capital: capital,
            open_positions: HashMap::new(),
            market_exposure: HashMap::new(),
            closed_positions: Vec::new(),
            total_entered: 0,
            total_wins: 0,
            total_losses: 0,
            evals_seen: 0,
            evals_rejected: 0,
        }
    }

    fn total_value(&self) -> f64 {
        self.current_capital + self.open_positions.values().map(|p| p.capital_deployed).sum::<f64>()
    }

    /// Check if an opportunity passes this strategy's gates.
    fn passes_gates(&self, opp: &Opportunity) -> bool {
        let cfg = &self.config;

        // Gate 1: Profit threshold
        // expected_profit_pct is already a decimal ratio (0.02 = 2%), matching config thresholds
        let pct = opp.expected_profit_pct;
        if pct < cfg.min_profit_threshold || pct > cfg.max_profit_threshold {
            return false;
        }

        // Gate 2: Resolution time
        let hours = opp.hours_to_resolve;
        let min_hours = cfg.min_resolution_time_secs / 3600.0;
        let max_hours = cfg.max_days_to_resolution * 24.0;
        if hours < min_hours || hours > max_hours {
            return false;
        }

        // Gate 3: Not already held
        if self.open_positions.contains_key(&opp.constraint_id) {
            return false;
        }

        // Gate 4: Max concurrent positions
        if self.open_positions.len() >= cfg.max_concurrent_positions {
            return false;
        }

        // Gate 5: Capital available
        let trade_size = self.compute_trade_size();
        if trade_size < 10.0 || self.current_capital < trade_size {
            return false;
        }

        // Gate 6: Per-market exposure
        let n_markets = opp.market_ids.len().max(1) as f64;
        let per_market = trade_size / n_markets;
        for mid in &opp.market_ids {
            let current = self.market_exposure.get(mid).copied().unwrap_or(0.0);
            if current + per_market > cfg.max_exposure_per_market {
                return false;
            }
        }

        true
    }

    fn compute_trade_size(&self) -> f64 {
        let raw = self.total_value() * self.config.capital_per_trade_pct;
        raw.min(self.config.max_position_size).max(0.0)
    }

    /// Enter a virtual position from an opportunity.
    fn enter(&mut self, opp: &Opportunity) {
        let trade_size = self.compute_trade_size().min(self.current_capital);
        if trade_size < 10.0 { return; }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);

        // Scale bet amounts proportionally
        let total_opp_capital = opp.total_capital_required.max(1.0);
        let scale = trade_size / total_opp_capital;

        let mut bet_amounts = HashMap::new();
        for (mid, &amt) in &opp.optimal_bets {
            bet_amounts.insert(mid.clone(), amt * scale);
        }

        let short_name = opp.market_names.first()
            .map(|n| if n.len() > 40 { format!("{}...", &n[..37]) } else { n.clone() })
            .unwrap_or_else(|| opp.constraint_id[..8.min(opp.constraint_id.len())].to_string());

        let vp = VirtualPosition {
            constraint_id: opp.constraint_id.clone(),
            market_ids: opp.market_ids.clone(),
            market_names: opp.market_names.clone(),
            short_name,
            capital_deployed: trade_size,
            expected_profit_pct: opp.expected_profit_pct,
            entry_ts: now,
            hours_to_resolve: opp.hours_to_resolve,
            is_sell: opp.is_sell,
            method: opp.method.clone(),
            entry_prices: opp.current_prices.clone(),
            entry_no_prices: opp.current_no_prices.clone(),
            bet_amounts,
        };

        // Update exposure
        let n_markets = vp.market_ids.len().max(1) as f64;
        let per_market = trade_size / n_markets;
        for mid in &vp.market_ids {
            *self.market_exposure.entry(mid.clone()).or_insert(0.0) += per_market;
        }

        self.current_capital -= trade_size;
        self.open_positions.insert(opp.constraint_id.clone(), vp);
        self.total_entered += 1;
    }

    /// Resolve a virtual position by constraint_id.
    /// Uses the same payout logic: buy arb → winner's shares pay $1 each.
    fn resolve(&mut self, constraint_id: &str, winning_market_id: &str) -> Option<ClosedVirtualPosition> {
        let vp = self.open_positions.remove(constraint_id)?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);

        // Calculate payout: for buy arbs, we hold shares in all legs.
        // The winning market's shares pay $1 each; losers pay $0.
        // For sell arbs (negRisk), the losing markets' shares pay $1.
        let payout = if vp.is_sell {
            // Sell arb: we sold shares, so losers resolve to $1 (our liability)
            // and winner resolves to $1 (we keep the difference)
            // Simplified: payout ≈ capital + expected profit (approximation)
            vp.capital_deployed * (1.0 + vp.expected_profit_pct)
        } else {
            // Buy arb: shares in winning market pay $1 each
            let winning_shares = vp.bet_amounts.get(winning_market_id).copied().unwrap_or(0.0);
            let entry_price = vp.entry_prices.get(winning_market_id).copied().unwrap_or(0.5);
            if entry_price > 0.0 {
                winning_shares / entry_price // shares = bet / price, payout = shares * $1
            } else {
                vp.capital_deployed
            }
        };

        let profit = payout - vp.capital_deployed;
        let profit_pct = if vp.capital_deployed > 0.0 {
            profit / vp.capital_deployed * 100.0
        } else { 0.0 };
        let is_win = profit > 0.0;

        // Return capital + profit
        self.current_capital += payout;

        // Update exposure
        let n_markets = vp.market_ids.len().max(1) as f64;
        let per_market = vp.capital_deployed / n_markets;
        for mid in &vp.market_ids {
            if let Some(exp) = self.market_exposure.get_mut(mid) {
                *exp = (*exp - per_market).max(0.0);
                if *exp < 0.01 {
                    self.market_exposure.remove(mid);
                }
            }
        }

        if is_win { self.total_wins += 1; } else { self.total_losses += 1; }

        let closed = ClosedVirtualPosition {
            capital_deployed: vp.capital_deployed,
            actual_profit: profit,
            actual_profit_pct: profit_pct,
            entry_ts: vp.entry_ts,
            close_ts: now,
            is_win,
            short_name: vp.short_name.clone(),
            method: vp.method.clone(),
            is_sell: vp.is_sell,
        };
        self.closed_positions.push(closed.clone());
        Some(closed)
    }

    // --- Metrics ---

    fn rolling_pnl(&self, days: u32) -> f64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let cutoff = now - (days as f64 * 86400.0);
        self.closed_positions.iter()
            .filter(|p| p.close_ts >= cutoff)
            .map(|p| p.actual_profit)
            .sum()
    }

    fn rolling_pnl_pct(&self, days: u32) -> f64 {
        if self.config.initial_capital <= 0.0 { return 0.0; }
        self.rolling_pnl(days) / self.config.initial_capital * 100.0
    }

    fn win_rate(&self) -> f64 {
        let total = self.total_wins + self.total_losses;
        if total == 0 { return 0.0; }
        self.total_wins as f64 / total as f64 * 100.0
    }

    fn avg_hold_hours(&self) -> f64 {
        if self.closed_positions.is_empty() { return 0.0; }
        let sum: f64 = self.closed_positions.iter()
            .map(|p| (p.close_ts - p.entry_ts) / 3600.0)
            .sum();
        sum / self.closed_positions.len() as f64
    }

    fn sharpe(&self, days: u32) -> f64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let cutoff = now - (days as f64 * 86400.0);
        let returns: Vec<f64> = self.closed_positions.iter()
            .filter(|p| p.close_ts >= cutoff)
            .map(|p| p.actual_profit_pct)
            .collect();
        if returns.len() < 2 { return 0.0; }
        let mean = returns.iter().sum::<f64>() / returns.len() as f64;
        let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (returns.len() - 1) as f64;
        let std_dev = variance.sqrt();
        if std_dev < 1e-10 { return 0.0; }
        mean / std_dev
    }

    /// Sortino ratio: like Sharpe but only penalises downside deviation.
    fn sortino(&self, days: u32) -> f64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let cutoff = now - (days as f64 * 86400.0);
        let returns: Vec<f64> = self.closed_positions.iter()
            .filter(|p| p.close_ts >= cutoff)
            .map(|p| p.actual_profit_pct)
            .collect();
        if returns.len() < 2 { return 0.0; }
        let mean = returns.iter().sum::<f64>() / returns.len() as f64;
        let downside_var = returns.iter()
            .map(|r| if *r < mean { (r - mean).powi(2) } else { 0.0 })
            .sum::<f64>() / (returns.len() - 1) as f64;
        let downside_dev = downside_var.sqrt();
        if downside_dev < 1e-10 { return 0.0; }
        mean / downside_dev
    }

    /// Recovery factor: cumulative return / max drawdown.
    fn recovery_factor(&self, days: u32) -> f64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let cutoff = now - (days as f64 * 86400.0);
        let mut sorted: Vec<&ClosedVirtualPosition> = self.closed_positions.iter()
            .filter(|p| p.close_ts >= cutoff)
            .collect();
        if sorted.is_empty() { return 0.0; }
        sorted.sort_by(|a, b| a.close_ts.total_cmp(&b.close_ts));

        let mut cumulative = 0.0f64;
        let mut peak = 0.0f64;
        let mut max_drawdown = 0.0f64;
        for p in &sorted {
            cumulative += p.actual_profit;
            if cumulative > peak { peak = cumulative; }
            let dd = peak - cumulative;
            if dd > max_drawdown { max_drawdown = dd; }
        }
        if max_drawdown < 0.01 {
            if cumulative > 0.0 { return 99.0; } // no drawdown, positive return
            return 0.0;
        }
        cumulative / max_drawdown
    }

    /// Profit factor: gross wins / gross losses.
    fn profit_factor(&self, days: u32) -> f64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let cutoff = now - (days as f64 * 86400.0);
        let mut gross_wins = 0.0f64;
        let mut gross_losses = 0.0f64;
        for p in &self.closed_positions {
            if p.close_ts < cutoff { continue; }
            if p.actual_profit > 0.0 {
                gross_wins += p.actual_profit;
            } else {
                gross_losses += p.actual_profit.abs();
            }
        }
        if gross_losses < 0.01 {
            if gross_wins > 0.0 { return 99.0; }
            return 0.0;
        }
        gross_wins / gross_losses
    }

    /// Capital utilisation: deployed / initial * 100.
    fn capital_utilisation_pct(&self) -> f64 {
        if self.config.initial_capital <= 0.0 { return 0.0; }
        let deployed: f64 = self.open_positions.values().map(|p| p.capital_deployed).sum();
        deployed / self.config.initial_capital * 100.0
    }

    /// Turnover rate: resolved trades per day.
    fn turnover_rate(&self, days: u32) -> f64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let cutoff = now - (days as f64 * 86400.0);
        let count = self.closed_positions.iter()
            .filter(|p| p.close_ts >= cutoff)
            .count();
        count as f64 / days as f64
    }

    /// Max drawdown (absolute $) over a period.
    fn max_drawdown(&self, days: u32) -> f64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let cutoff = now - (days as f64 * 86400.0);
        let mut sorted: Vec<&ClosedVirtualPosition> = self.closed_positions.iter()
            .filter(|p| p.close_ts >= cutoff)
            .collect();
        if sorted.is_empty() { return 0.0; }
        sorted.sort_by(|a, b| a.close_ts.total_cmp(&b.close_ts));

        let mut cumulative = 0.0f64;
        let mut peak = 0.0f64;
        let mut max_dd = 0.0f64;
        for p in &sorted {
            cumulative += p.actual_profit;
            if cumulative > peak { peak = cumulative; }
            let dd = peak - cumulative;
            if dd > max_dd { max_dd = dd; }
        }
        max_dd
    }
}

// ---------------------------------------------------------------------------
// Strategy tracker (top-level)
// ---------------------------------------------------------------------------

pub struct StrategyTracker {
    pub portfolios: Vec<VirtualPortfolio>,
}

impl StrategyTracker {
    pub fn new(configs: Vec<StrategyConfig>) -> Self {
        let portfolios = configs.into_iter().map(VirtualPortfolio::new).collect();
        Self { portfolios }
    }

    pub fn len(&self) -> usize {
        self.portfolios.len()
    }

    /// Compute the widest eval gates across all strategies and apply to EvalConfig.
    /// This ensures evaluate_batch() returns opportunities that ANY strategy might accept.
    pub fn apply_widest_gates(&self, eval_config: &mut EvalConfig) {
        if self.portfolios.is_empty() { return; }

        let min_profit = self.portfolios.iter()
            .map(|p| p.config.min_profit_threshold)
            .fold(f64::MAX, f64::min);
        let max_hours = self.portfolios.iter()
            .map(|p| p.config.max_days_to_resolution * 24.0)
            .fold(0.0_f64, f64::max);

        let old_min = eval_config.min_profit_threshold;
        let old_max_h = eval_config.max_hours;

        if min_profit < eval_config.min_profit_threshold {
            eval_config.min_profit_threshold = min_profit;
        }
        if max_hours > eval_config.max_hours {
            eval_config.max_hours = max_hours;
        }

        if eval_config.min_profit_threshold != old_min || eval_config.max_hours != old_max_h {
            tracing::info!(
                "Strategy tracker widened eval gates: min_profit {:.2}%→{:.2}%, max_hours {:.0}→{:.0}",
                old_min * 100.0, eval_config.min_profit_threshold * 100.0,
                old_max_h, eval_config.max_hours,
            );
        }
    }

    /// Process opportunities through all strategy gates.
    /// Called after evaluate_batch() returns, before try_enter_or_replace().
    pub fn process_opportunities(&mut self, opps: &[Opportunity], _fee_rate: f64) {
        for portfolio in &mut self.portfolios {
            for opp in opps {
                portfolio.evals_seen += 1;
                if portfolio.passes_gates(opp) {
                    portfolio.enter(opp);
                } else {
                    portfolio.evals_rejected += 1;
                }
            }
        }
    }

    /// Forward a resolution event to all virtual portfolios.
    /// Called when a real position resolves (WS or API).
    pub fn resolve(&mut self, constraint_id: &str, winning_market_id: &str) {
        for portfolio in &mut self.portfolios {
            if portfolio.open_positions.contains_key(constraint_id) {
                if let Some(closed) = portfolio.resolve(constraint_id, winning_market_id) {
                    tracing::debug!(
                        "Strategy {} resolved {}: profit={:.2} ({:.1}%)",
                        portfolio.config.name, &constraint_id[..8.min(constraint_id.len())],
                        closed.actual_profit, closed.actual_profit_pct,
                    );
                }
            }
        }
    }

    /// Prune closed positions older than 30 days from all portfolios.
    pub fn prune_old_closed(&mut self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let cutoff = now - 30.0 * 86400.0;
        for portfolio in &mut self.portfolios {
            portfolio.closed_positions.retain(|p| p.close_ts >= cutoff);
        }
    }

    /// Build JSON summary for the dashboard SSE event.
    pub fn build_summary(&self) -> serde_json::Value {
        let strategies: Vec<serde_json::Value> = self.portfolios.iter().map(|p| {
            let positions: Vec<serde_json::Value> = p.open_positions.values().map(|vp| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64()).unwrap_or(0.0);
                let elapsed_hours = (now - vp.entry_ts) / 3600.0;
                let hours_left = (vp.hours_to_resolve - elapsed_hours).max(0.0);
                serde_json::json!({
                    "name": vp.short_name,
                    "capital": (vp.capital_deployed * 100.0).round() / 100.0,
                    "profit_pct": (vp.expected_profit_pct * 1000.0).round() / 10.0,
                    "hours_left": (hours_left * 10.0).round() / 10.0,
                    // Rich fields for positions tab
                    "constraint_id": vp.constraint_id,
                    "market_ids": vp.market_ids,
                    "market_names": vp.market_names,
                    "entry_prices": vp.entry_prices,
                    "entry_no_prices": vp.entry_no_prices,
                    "bet_amounts": vp.bet_amounts,
                    "is_sell": vp.is_sell,
                    "method": vp.method,
                    "entry_ts": vp.entry_ts,
                    "hours_to_resolve": vp.hours_to_resolve,
                    "expected_profit_pct": vp.expected_profit_pct,
                })
            }).collect();

            serde_json::json!({
                "name": p.config.name,
                "label": p.config.label,
                "capital": (p.current_capital * 100.0).round() / 100.0,
                "initial_capital": p.config.initial_capital,
                "total_value": (p.total_value() * 100.0).round() / 100.0,
                "open_count": p.open_positions.len(),
                "max_positions": p.config.max_concurrent_positions,
                // Rolling P&L
                "pnl_7d": (p.rolling_pnl_pct(7) * 100.0).round() / 100.0,
                "pnl_14d": (p.rolling_pnl_pct(14) * 100.0).round() / 100.0,
                "pnl_28d": (p.rolling_pnl_pct(28) * 100.0).round() / 100.0,
                // Core ratios
                "sharpe_28d": (p.sharpe(28) * 100.0).round() / 100.0,
                "sortino_28d": (p.sortino(28) * 100.0).round() / 100.0,
                "recovery_factor_28d": (p.recovery_factor(28) * 100.0).round() / 100.0,
                "profit_factor_28d": (p.profit_factor(28) * 100.0).round() / 100.0,
                "max_drawdown_28d": (p.max_drawdown(28) * 100.0).round() / 100.0,
                // Operational
                "win_rate": (p.win_rate() * 10.0).round() / 10.0,
                "avg_hold_hours": (p.avg_hold_hours() * 10.0).round() / 10.0,
                "capital_util_pct": (p.capital_utilisation_pct() * 10.0).round() / 10.0,
                "turnover_rate_14d": (p.turnover_rate(14) * 100.0).round() / 100.0,
                "total_entered": p.total_entered,
                "total_resolved": p.total_wins + p.total_losses,
                "evals_seen": p.evals_seen,
                "evals_rejected": p.evals_rejected,
                "evals_accepted": p.evals_seen - p.evals_rejected,
                "total_realized": p.closed_positions.iter().map(|c| c.actual_profit).sum::<f64>(),
                "positions": positions,
                "closed": p.closed_positions.iter().map(|c| {
                    let hold_secs = c.close_ts - c.entry_ts;
                    serde_json::json!({
                        "name": c.short_name,
                        "method": c.method,
                        "is_sell": c.is_sell,
                        "deployed": (c.capital_deployed * 100.0).round() / 100.0,
                        "profit": (c.actual_profit * 100.0).round() / 100.0,
                        "profit_pct": (c.actual_profit_pct * 1000.0).round() / 10.0,
                        "entry_ts": c.entry_ts,
                        "close_ts": c.close_ts,
                        "hold_secs": hold_secs,
                        "is_win": c.is_win,
                    })
                }).collect::<Vec<_>>(),
            })
        }).collect();

        serde_json::json!({ "strategies": strategies })
    }

    // --- SQLite persistence ---

    /// Save all portfolio state to SQLite.
    pub fn save_state(&self, db: &StateDB) {
        for p in &self.portfolios {
            db.save_strategy_portfolio(
                &p.config.name, p.current_capital,
                p.total_entered, p.total_wins, p.total_losses,
                p.evals_seen, p.evals_rejected,
            );

            // Save open positions as (constraint_id, json)
            let open_rows: Vec<(String, String)> = p.open_positions.iter()
                .filter_map(|(cid, vp)| {
                    serde_json::to_string(vp).ok().map(|json| (cid.clone(), json))
                })
                .collect();
            db.save_strategy_open_positions(&p.config.name, &open_rows);

            // Closed positions are written incrementally (on resolve), not bulk-saved.
            // But on first save after restart with loaded closed positions, write any
            // that came from memory (load_state populates them).
        }
    }

    /// Load state from SQLite, matching by strategy name.
    pub fn load_state(&mut self, db: &StateDB) {
        let portfolios_data = db.load_strategy_portfolios();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let since_ts = now - 30.0 * 86400.0;

        for p in &mut self.portfolios {
            // Restore portfolio summary
            if let Some((_, capital, entered, wins, losses, evals, rejected)) = portfolios_data.iter()
                .find(|(name, ..)| name == &p.config.name)
            {
                p.current_capital = *capital;
                p.total_entered = *entered as u64;
                p.total_wins = *wins as u64;
                p.total_losses = *losses as u64;
                p.evals_seen = *evals as u64;
                p.evals_rejected = *rejected as u64;
            }

            // Restore open positions
            let open_rows = db.load_strategy_open_positions(&p.config.name);
            for (cid, json) in open_rows {
                if let Ok(vp) = serde_json::from_str::<VirtualPosition>(&json) {
                    // Rebuild market exposure
                    let n_markets = vp.market_ids.len().max(1) as f64;
                    let per_market = vp.capital_deployed / n_markets;
                    for mid in &vp.market_ids {
                        *p.market_exposure.entry(mid.clone()).or_insert(0.0) += per_market;
                    }
                    p.open_positions.insert(cid, vp);
                }
            }

            // Restore closed positions (last 30 days)
            let closed_rows = db.load_strategy_closed_positions(&p.config.name, since_ts);
            for (capital, profit, profit_pct, entry_ts, close_ts, is_win, short_name, method, is_sell) in closed_rows {
                p.closed_positions.push(ClosedVirtualPosition {
                    capital_deployed: capital,
                    actual_profit: profit,
                    actual_profit_pct: profit_pct,
                    entry_ts,
                    close_ts,
                    is_win,
                    short_name,
                    method,
                    is_sell,
                });
            }

            tracing::info!(
                "Strategy {} loaded: ${:.2} capital, {} open, {} closed (30d)",
                p.config.name, p.current_capital,
                p.open_positions.len(), p.closed_positions.len(),
            );
        }
    }

    /// Create tracker and load state from DB if available.
    pub fn load_or_new(db: &StateDB, configs: Vec<StrategyConfig>) -> Self {
        let mut tracker = Self::new(configs);
        tracker.load_state(db);
        tracker
    }

    /// Save a newly closed position to SQLite (called during resolve).
    pub fn save_closed_to_db(&self, db: &StateDB, strategy_name: &str, closed: &ClosedVirtualPosition) {
        db.save_strategy_closed_position(
            strategy_name, closed.capital_deployed,
            closed.actual_profit, closed.actual_profit_pct,
            closed.entry_ts, closed.close_ts, closed.is_win,
            &closed.short_name, &closed.method, closed.is_sell,
        );
    }

    /// Resolve with DB persistence — saves closed position immediately.
    pub fn resolve_with_db(&mut self, constraint_id: &str, winning_market_id: &str, db: &StateDB) {
        for portfolio in &mut self.portfolios {
            if portfolio.open_positions.contains_key(constraint_id) {
                let name = portfolio.config.name.clone();
                if let Some(closed) = portfolio.resolve(constraint_id, winning_market_id) {
                    tracing::info!(
                        "Strategy {} resolved {}: profit=${:.2} ({:.1}%)",
                        name, &constraint_id[..8.min(constraint_id.len())],
                        closed.actual_profit, closed.actual_profit_pct,
                    );
                    db.save_strategy_closed_position(
                        &name, closed.capital_deployed,
                        closed.actual_profit, closed.actual_profit_pct,
                        closed.entry_ts, closed.close_ts, closed.is_win,
                        &closed.short_name, &closed.method, closed.is_sell,
                    );
                }
            }
        }
    }

    /// Prune old closed positions from SQLite too.
    pub fn prune_old_closed_with_db(&mut self, db: &StateDB) {
        self.prune_old_closed();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        db.prune_strategy_closed_positions(now - 30.0 * 86400.0);
    }
}
