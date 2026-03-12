/// Rust Position Manager — replaces paper_trading.py (Phase 8q-1)
///
/// Manages the full position lifecycle: entry, monitoring, resolution, liquidation.
/// All capital accounting happens here. State persisted via RustStateDB.
///
/// JSON-serializable to match existing dashboard SSE protocol and execution_state format.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use serde::{Serialize, Deserialize};
use parking_lot::Mutex;

fn now_ts() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketLeg {
    pub name: String,
    pub entry_price: f64,
    pub bet_amount: f64,
    pub outcome: String,  // "yes" or "no"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub position_id: String,
    pub opportunity_id: String,
    pub markets: HashMap<String, MarketLeg>,  // market_id → leg
    pub total_capital: f64,
    pub expected_profit: f64,
    pub expected_profit_pct: f64,
    pub fees_paid: f64,

    pub entry_timestamp: String,  // ISO format for dashboard compat
    pub entry_prices: HashMap<String, f64>,  // market_id → price at entry
    pub status: String,  // "open", "monitoring", "closed"
    pub last_check: String,
    pub price_drift: HashMap<String, f64>,

    // Resolution
    pub resolved_at: Option<String>,
    pub close_timestamp: Option<f64>,  // Unix ts
    pub winning_market: Option<String>,
    pub actual_payout: f64,
    pub actual_profit: f64,
    pub actual_profit_pct: f64,
    pub profit_delta: f64,
    pub profit_accuracy: f64,
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Result of attempting to enter a position
#[derive(Debug)]
pub enum EntryResult {
    Entered(Position),
    InsufficientCapital { available: f64, required: f64 },
}

/// Result of resolution check
#[derive(Debug)]
pub struct ResolutionEvent {
    pub position_id: String,
    pub winning_market_id: String,
    pub winning_outcome: String,
    pub payout: f64,
    pub profit: f64,
}

/// Core position manager — thread-safe, owns all capital accounting
pub struct PositionManager {
    inner: Mutex<PositionManagerInner>,
}

struct PositionManagerInner {
    current_capital: f64,
    initial_capital: f64,
    taker_fee: f64,
    open_positions: HashMap<String, Position>,
    closed_positions: Vec<Position>,
    total_trades: u64,
    winning_trades: u64,
    losing_trades: u64,
    total_actual_profit: f64,
    total_expected_profit: f64,
}

impl PositionManager {
    pub fn new(initial_capital: f64, taker_fee: f64) -> Self {
        Self {
            inner: Mutex::new(PositionManagerInner {
                current_capital: initial_capital,
                initial_capital,
                taker_fee,
                open_positions: HashMap::new(),
                closed_positions: Vec::new(),
                total_trades: 0,
                winning_trades: 0,
                losing_trades: 0,
                total_actual_profit: 0.0,
                total_expected_profit: 0.0,
            }),
        }
    }

    // --- Capital queries ---

    pub fn current_capital(&self) -> f64 {
        self.inner.lock().current_capital
    }

    pub fn initial_capital(&self) -> f64 {
        self.inner.lock().initial_capital
    }

    pub fn open_count(&self) -> usize {
        self.inner.lock().open_positions.len()
    }

    pub fn closed_count(&self) -> usize {
        self.inner.lock().closed_positions.len()
    }

    // --- Position entry ---

    pub fn enter_position(
        &self,
        opportunity_id: &str,
        constraint_id: &str,
        strategy: &str,
        method: &str,
        market_ids: &[String],
        market_names: &[String],
        current_prices: &HashMap<String, f64>,
        optimal_bets: &HashMap<String, f64>,
        expected_profit: f64,
        expected_profit_pct: f64,
        is_sell: bool,
    ) -> EntryResult {
        let mut mgr = self.inner.lock();

        let total_cost: f64 = optimal_bets.values().sum();
        let fees = total_cost * mgr.taker_fee;
        let required = total_cost + fees;

        if mgr.current_capital < required {
            return EntryResult::InsufficientCapital {
                available: mgr.current_capital,
                required,
            };
        }

        // Build market legs
        let mut markets = HashMap::new();
        let outcome = if is_sell { "no" } else { "yes" };
        for (i, mid) in market_ids.iter().enumerate() {
            let name = market_names.get(i).cloned().unwrap_or_default();
            let price = current_prices.get(mid).copied().unwrap_or(0.0);
            let bet = optimal_bets.get(mid).copied().unwrap_or(0.0);
            markets.insert(mid.clone(), MarketLeg {
                name, entry_price: price, bet_amount: bet,
                outcome: outcome.to_string(),
            });
        }

        let ts = now_ts();
        let ts_iso = format!("{:.6}", ts);  // Will be formatted properly by dashboard
        let pid = format!("paper_{}_{}", ts, opportunity_id);

        let mut meta = HashMap::new();
        meta.insert("constraint_id".into(), serde_json::Value::String(constraint_id.to_string()));
        meta.insert("strategy".into(), serde_json::Value::String(strategy.to_string()));
        meta.insert("method".into(), serde_json::Value::String(method.to_string()));

        let position = Position {
            position_id: pid.clone(),
            opportunity_id: opportunity_id.to_string(),
            markets,
            total_capital: total_cost,
            expected_profit,
            expected_profit_pct,
            fees_paid: fees,
            entry_timestamp: ts_iso.clone(),
            entry_prices: current_prices.clone(),
            status: "monitoring".to_string(),
            last_check: ts_iso,
            price_drift: HashMap::new(),
            resolved_at: None,
            close_timestamp: None,
            winning_market: None,
            actual_payout: 0.0,
            actual_profit: 0.0,
            actual_profit_pct: 0.0,
            profit_delta: 0.0,
            profit_accuracy: 0.0,
            metadata: meta,
        };

        // Deduct capital
        mgr.current_capital -= required;
        mgr.total_trades += 1;
        mgr.total_expected_profit += expected_profit;

        let result = position.clone();
        mgr.open_positions.insert(pid, position);

        EntryResult::Entered(result)
    }

    // --- Resolution: close position when market resolves ---

    pub fn close_on_resolution(
        &self,
        position_id: &str,
        winning_market_id: &str,
    ) -> Option<ResolutionEvent> {
        let mut mgr = self.inner.lock();

        let mut position = match mgr.open_positions.remove(position_id) {
            Some(p) => p,
            None => return None,
        };

        let method = position.metadata.get("method")
            .and_then(|v| v.as_str()).unwrap_or("");
        let is_sell = method.contains("sell");

        let payout = if is_sell {
            // SELL arb: bought NO on all markets. Winner's NO loses, rest pay $1/share.
            let mut pay = 0.0;
            for (mid, leg) in &position.markets {
                if mid != winning_market_id {
                    let no_price = 1.0 - leg.entry_price;
                    if no_price > 0.0 {
                        pay += leg.bet_amount / no_price;  // shares_NO * $1
                    }
                }
            }
            pay
        } else {
            // BUY arb: bought YES on all markets. Only winner pays.
            match position.markets.get(winning_market_id) {
                Some(leg) => {
                    if leg.entry_price > 0.0 {
                        leg.bet_amount / leg.entry_price  // shares_YES * $1
                    } else { 0.0 }
                }
                None => 0.0,
            }
        };

        let total_invested = position.total_capital + position.fees_paid;
        let profit = payout - total_invested;
        let profit_pct = if total_invested > 0.0 { profit / total_invested } else { 0.0 };

        position.status = "closed".to_string();
        position.close_timestamp = Some(now_ts());
        position.winning_market = Some(winning_market_id.to_string());
        position.actual_payout = payout;
        position.actual_profit = profit;
        position.actual_profit_pct = profit_pct;
        position.profit_delta = profit - position.expected_profit;
        position.profit_accuracy = if position.expected_profit != 0.0 {
            profit / position.expected_profit
        } else { 0.0 };
        position.metadata.insert("close_reason".into(),
            serde_json::Value::String("resolved".to_string()));

        // Update capital + stats
        mgr.current_capital += payout;
        mgr.total_actual_profit += profit;
        if profit > 0.001 { mgr.winning_trades += 1; }
        else if profit < -0.001 { mgr.losing_trades += 1; }

        let event = ResolutionEvent {
            position_id: position.position_id.clone(),
            winning_market_id: winning_market_id.to_string(),
            winning_outcome: "yes".to_string(),
            payout,
            profit,
        };

        mgr.closed_positions.push(position);
        Some(event)
    }

    // --- Liquidation (replacement close) ---

    pub fn liquidate_position(
        &self, position_id: &str, reason: &str,
    ) -> Option<f64> {
        let mut mgr = self.inner.lock();
        let mut position = match mgr.open_positions.remove(position_id) {
            Some(p) => p,
            None => return None,
        };

        // Liquidation: return capital minus fees (no payout from resolution)
        let refund = position.total_capital;  // Return deployed capital
        let fees_lost = position.fees_paid;
        let profit = -fees_lost;  // Only loss is the entry fees

        position.status = "closed".to_string();
        position.close_timestamp = Some(now_ts());
        position.actual_payout = refund;
        position.actual_profit = profit;
        position.actual_profit_pct = profit / (position.total_capital + fees_lost);
        position.metadata.insert("close_reason".into(),
            serde_json::Value::String(reason.to_string()));

        mgr.current_capital += refund;
        mgr.total_actual_profit += profit;
        mgr.closed_positions.push(position);

        Some(profit)
    }

    // --- Resolution checking: scan open positions for resolved markets ---

    /// Check all open positions against current market prices.
    /// Returns positions that have fully resolved (all markets at YES>=0.95 or missing).
    pub fn check_resolutions(
        &self,
        market_prices: &HashMap<String, HashMap<String, f64>>,  // market_id → {outcome → price}
    ) -> Vec<(String, String)> {  // (position_id, winning_market_id)
        let mgr = self.inner.lock();
        let mut resolved = Vec::new();

        for (pid, position) in &mgr.open_positions {
            let total_markets = position.markets.len();
            let mut resolved_count = 0;
            let mut winning_market_id = String::new();

            for market_id in position.markets.keys() {
                match market_prices.get(market_id) {
                    None => {
                        // Market missing from data — treat as resolved
                        resolved_count += 1;
                    }
                    Some(outcomes) => {
                        let mut max_price = 0.0f64;
                        let mut max_outcome = String::new();
                        for (outcome, price) in outcomes {
                            if *price > max_price {
                                max_price = *price;
                                max_outcome = outcome.clone();
                            }
                        }
                        if max_price >= 0.95 {
                            resolved_count += 1;
                            if max_outcome.to_lowercase() == "yes" {
                                winning_market_id = market_id.clone();
                            }
                        }
                    }
                }
            }

            if resolved_count == total_markets && !winning_market_id.is_empty() {
                resolved.push((pid.clone(), winning_market_id));
            }
        }
        resolved
    }

    // --- Data export (for dashboard SSE + state persistence) ---

    pub fn get_open_positions_json(&self) -> Vec<String> {
        let mgr = self.inner.lock();
        mgr.open_positions.values()
            .map(|p| serde_json::to_string(p).unwrap_or_default())
            .collect()
    }

    pub fn get_closed_positions_json(&self) -> Vec<String> {
        let mgr = self.inner.lock();
        mgr.closed_positions.iter()
            .map(|p| serde_json::to_string(p).unwrap_or_default())
            .collect()
    }

    pub fn get_open_position_ids(&self) -> Vec<String> {
        let mgr = self.inner.lock();
        mgr.open_positions.keys().cloned().collect()
    }

    pub fn get_held_constraint_ids(&self) -> std::collections::HashSet<String> {
        let mgr = self.inner.lock();
        mgr.open_positions.values()
            .filter_map(|p| p.metadata.get("constraint_id")?.as_str().map(|s| s.to_string()))
            .collect()
    }

    pub fn get_held_market_ids(&self) -> std::collections::HashSet<String> {
        let mgr = self.inner.lock();
        mgr.open_positions.values()
            .flat_map(|p| p.markets.keys().cloned())
            .collect()
    }

    pub fn get_performance_metrics(&self) -> HashMap<String, f64> {
        let mgr = self.inner.lock();
        let mut m = HashMap::new();
        m.insert("current_capital".into(), mgr.current_capital);
        m.insert("initial_capital".into(), mgr.initial_capital);
        m.insert("total_trades".into(), mgr.total_trades as f64);
        m.insert("winning_trades".into(), mgr.winning_trades as f64);
        m.insert("losing_trades".into(), mgr.losing_trades as f64);
        m.insert("total_actual_profit".into(), mgr.total_actual_profit);
        m.insert("total_expected_profit".into(), mgr.total_expected_profit);
        m.insert("open_count".into(), mgr.open_positions.len() as f64);
        m.insert("closed_count".into(), mgr.closed_positions.len() as f64);
        m
    }

    // --- State import (from existing JSON execution_state) ---

    pub fn import_positions_json(
        &self, open_json: &[String], closed_json: &[String],
        capital: f64, initial_capital: f64,
    ) {
        let mut mgr = self.inner.lock();
        mgr.current_capital = capital;
        mgr.initial_capital = initial_capital;
        mgr.open_positions.clear();
        mgr.closed_positions.clear();

        for j in open_json {
            if let Ok(p) = serde_json::from_str::<Position>(j) {
                mgr.open_positions.insert(p.position_id.clone(), p);
            }
        }
        for j in closed_json {
            if let Ok(p) = serde_json::from_str::<Position>(j) {
                mgr.closed_positions.push(p);
            }
        }

        // Restore stats from closed positions
        mgr.total_trades = (mgr.open_positions.len() + mgr.closed_positions.len()) as u64;
        mgr.total_actual_profit = mgr.closed_positions.iter()
            .map(|p| p.actual_profit).sum();
        mgr.winning_trades = mgr.closed_positions.iter()
            .filter(|p| p.actual_profit > 0.001).count() as u64;
        mgr.losing_trades = mgr.closed_positions.iter()
            .filter(|p| p.actual_profit < -0.001).count() as u64;
        mgr.total_expected_profit = mgr.open_positions.values()
            .chain(mgr.closed_positions.iter())
            .map(|p| p.expected_profit).sum();
    }
}
