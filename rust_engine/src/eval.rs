/// Batch constraint evaluator — the core of Phase 8 P4c.
///
/// evaluate_batch() does the ENTIRE hot path in Rust:
///   1. Drain eval queue (urgent first)
///   2. For each constraint: read prices from book mirror
///   3. Run arb math (mutex direct + polytope FW)
///   4. Return list of opportunity dicts
///
/// This eliminates all Python↔Rust round-trips per eval.
use std::collections::HashMap;
use std::collections::HashSet;
use dashmap::DashMap;
use crate::arb::{self, ArbResult};
use crate::book::BookMirror;
use crate::latency::LatencyTracker;
use crate::queue::EvalQueue;


/// Stored constraint definition (loaded after constraint detection).
#[derive(Debug, Clone)]
pub struct Constraint {
    pub constraint_id: String,
    pub constraint_type: String,  // "mutex", "complementary", "logical_implication"
    /// market_id → (yes_asset_id, no_asset_id)
    pub markets: Vec<MarketRef>,
    pub is_neg_risk: bool,
    pub implications: Vec<(usize, usize)>,
    /// Earliest resolution date as unix timestamp (from market end_date). 0 = unknown.
    pub end_date_ts: f64,
    /// Total number of markets in the negRisk group at detection time (including those
    /// without valid CLOB token IDs that were excluded from `markets`). 0 = unknown.
    /// If markets.len() < full_group_size, some group members had no order book at
    /// detection time and are excluded from candidates — the set may be non-exhaustive.
    pub full_group_size: usize,
    /// negRisk group identifier from Polymarket Gamma API (`negRiskMarketID` field).
    /// Empty for non-negRisk constraints. Used by the F-pre-7 / G7 pre-trade Gamma
    /// freshness check to query the live group state at entry time.
    pub neg_risk_market_id: String,
}

#[derive(Debug, Clone)]
pub struct MarketRef {
    pub market_id: String,
    pub yes_asset_id: String,
    pub no_asset_id: String,
    pub name: String,
}

/// Holds all constraint definitions. Populated after constraint detection.
pub struct ConstraintStore {
    constraints: DashMap<String, Constraint>,
}

impl ConstraintStore {
    pub fn new() -> Self {
        Self { constraints: DashMap::new() }
    }

    pub fn set_constraints(&self, constraints: Vec<Constraint>) {
        self.constraints.clear();
        for c in constraints {
            self.constraints.insert(c.constraint_id.clone(), c);
        }
    }

    pub fn get(&self, id: &str) -> Option<Constraint> {
        self.constraints.get(id).map(|r| r.clone())
    }

    pub fn len(&self) -> usize {
        self.constraints.len()
    }

    /// Return all constraints (for strategy tracker iteration).
    pub fn all(&self) -> Vec<Constraint> {
        self.constraints.iter().map(|r| r.value().clone()).collect()
    }
}

impl Default for ConstraintStore {
    fn default() -> Self { Self::new() }
}

/// Config for arb evaluation.
#[derive(Debug, Clone)]
pub struct EvalConfig {
    pub capital: f64,
    pub fee_rate: f64,
    pub min_profit_threshold: f64,
    pub max_profit_threshold: f64,
    pub max_fw_iter: usize,
    pub max_hours: f64,  // Skip constraints resolving further than this (default: 1440 = 60 days)
}

/// A found opportunity, pre-ranked by score.
#[must_use]
#[derive(Debug, Clone)]
pub struct Opportunity {
    pub constraint_id: String,
    pub market_ids: Vec<String>,
    pub market_names: Vec<String>,
    pub method: String,
    pub is_sell: bool,
    pub expected_profit_pct: f64,
    pub expected_profit: f64,
    pub fees_estimated: f64,
    pub total_capital_required: f64,
    pub current_prices: HashMap<String, f64>,
    pub current_no_prices: HashMap<String, f64>,
    pub optimal_bets: HashMap<String, f64>,
    pub neg_risk: bool,
    pub n_scenarios: Option<usize>,
    /// Hours until resolution (0 = unknown)
    pub hours_to_resolve: f64,
    /// Score = profit_pct / effective_hours (higher = better)
    pub score: f64,
    /// Minimum ask depth (USD) across all legs — for depth gating (B1.0)
    pub min_leg_depth_usd: f64,
    /// Capital efficiency for negRisk sell arbs (B3): no_cost / collateral
    pub capital_efficiency: Option<f64>,
    /// Collateral per unit for negRisk sell arbs (B3)
    pub collateral_per_unit: Option<f64>,
    /// Earliest Polymarket server timestamp that triggered this opportunity (for e2e latency).
    pub origin_ts: f64,
}

/// Evaluate a batch of constraints from the queue.
/// Filters held positions, scores by profit/hours, returns top-N ranked.
/// Returns (opportunities, n_urgent, n_background, n_evaluated, n_skipped_held).
pub fn evaluate_batch(
    queue: &EvalQueue,
    book: &BookMirror,
    store: &ConstraintStore,
    config: &EvalConfig,
    max_evals: usize,
    held_cids: &HashSet<String>,
    held_mids: &HashSet<String>,
    top_n: usize,
    depth_haircut: f64,
    latency: &LatencyTracker,
) -> (Vec<Opportunity>, usize, usize, usize, usize) {
    let entries = queue.drain(max_evals);
    if entries.is_empty() {
        return (vec![], 0, 0, 0, 0);
    }

    let n_urgent = entries.iter().filter(|e| e.urgent).count();
    let n_bg = entries.len() - n_urgent;
    let mut opportunities = Vec::new();
    let mut n_skipped_held = 0usize;

    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    // Segment 3: queue wait — time from push to drain
    const MAX_SANE_LATENCY_US: f64 = 60_000_000.0; // 60 seconds — sanity cap for queue wait
    if latency.is_enabled() {
        for entry in &entries {
            if entry.queued_at > 0.0 {
                let wait_us = (now_ts - entry.queued_at) * 1_000_000.0;
                if wait_us > 0.0 && wait_us < MAX_SANE_LATENCY_US {
                    latency.record_queue_wait(wait_us);
                }
            }
        }
    }

    for entry in &entries {
        // Skip constraints already held
        if held_cids.contains(&entry.constraint_id) {
            n_skipped_held += 1;
            continue;
        }

        let constraint = match store.get(&entry.constraint_id) {
            Some(c) => c,
            None => continue,
        };

        let n = constraint.markets.len();
        if n < 2 { continue; }

        // Skip constraints with no resolution date — untradeable (can't score, can't return capital)
        if constraint.end_date_ts <= 0.0 { continue; }

        // Skip constraints resolving too far in the future or already past
        let hours_to_end = (constraint.end_date_ts - now_ts) / 3600.0;
        if hours_to_end < 0.0 || hours_to_end > config.max_hours { continue; }

        // Skip if any market in this constraint is already held
        let has_held_market = constraint.markets.iter()
            .any(|m| held_mids.contains(&m.market_id));
        if has_held_market {
            n_skipped_held += 1;
            continue;
        }

        // Read YES ask prices from book mirror
        let mut yes_prices = Vec::with_capacity(n);
        let mut no_prices = Vec::with_capacity(n);
        let mut market_ids = Vec::with_capacity(n);
        let mut market_names = Vec::with_capacity(n);
        let mut all_live = true;

        for mref in &constraint.markets {
            let yes_ask = book.get_best_ask(&mref.yes_asset_id);
            let no_ask = book.get_best_ask(&mref.no_asset_id);
            if yes_ask <= 0.0 {
                all_live = false;
                break;
            }
            let no_p = if no_ask > 0.0 { no_ask } else { 1.0 - yes_ask };
            yes_prices.push(yes_ask);
            no_prices.push(no_p);
            market_ids.push(mref.market_id.clone());
            market_names.push(mref.name.clone());
        }
        if !all_live { continue; }

        // M1: Verify price arrays match market count (built in same loop, should always hold)
        debug_assert_eq!(yes_prices.len(), market_ids.len(), "price array length mismatch");

        // Exhaustiveness guard: all markets in the negRisk group must be candidates.
        // If full_group_size > candidates, some group members had no CLOB order book at
        // detection time and were excluded. One of those excluded markets could resolve YES,
        // making all our candidate YES tokens worthless. Skip the constraint in this case.
        if constraint.full_group_size > 0 && n < constraint.full_group_size {
            tracing::debug!(
                "SKIP (non-exhaustive): {} has {}/{} group markets as candidates",
                &constraint.constraint_id[..constraint.constraint_id.len().min(40)],
                n, constraint.full_group_size
            );
            continue;
        }

        // Try direct mutex arb first
        let mut result: Option<ArbResult> = arb::check_mutex_arb(
            &market_ids, &yes_prices, &no_prices,
            config.capital, config.fee_rate,
            config.min_profit_threshold, config.max_profit_threshold,
            constraint.is_neg_risk,
        );

        // If no direct arb, try polytope (catches partial hedges)
        let ct = arb::ConstraintType::from_str(&constraint.constraint_type);
        if result.is_none() && ct == Some(arb::ConstraintType::Mutex) {
            result = arb::polytope_arb(
                &market_ids, &yes_prices, ct.unwrap(),
                config.capital, config.fee_rate,
                config.min_profit_threshold, config.max_profit_threshold,
                &constraint.implications, config.max_fw_iter,
            );
        }

        if let Some(arb) = result {
            // Build price/bet maps. We clone market_id strings into price maps
            // and consume arb.bets (moved, no clone needed for bet keys).
            let mut current_prices = HashMap::with_capacity(market_ids.len());
            let mut current_no_prices = HashMap::with_capacity(market_ids.len());
            for (i, mid) in market_ids.iter().enumerate() {
                current_prices.insert(mid.clone(), yes_prices[i]);
                current_no_prices.insert(mid.clone(), no_prices[i]);
            }
            let optimal_bets: HashMap<String, f64> = arb.bets.into_iter().collect();

            // Compute minimum ask depth across all legs (B1.0)
            let min_leg_depth_usd = constraint.markets.iter()
                .map(|mref| {
                    let asset_id = if arb.is_sell { &mref.no_asset_id } else { &mref.yes_asset_id };
                    book.get_ask_depth_usd(asset_id, depth_haircut)
                })
                .fold(f64::INFINITY, f64::min);

            // Compute hours to resolution (end_date_ts > 0 guaranteed by filter above)
            let hours = if constraint.end_date_ts > now_ts {
                (constraint.end_date_ts - now_ts) / 3600.0
            } else {
                0.01  // already past — resolve imminently
            };

            // Score: profit_pct / effective_hours (higher = better)
            let score = arb.profit_pct / hours.max(0.01);

            opportunities.push(Opportunity {
                constraint_id: constraint.constraint_id.clone(),
                market_ids,  // moved, not cloned
                market_names,
                method: arb.method,  // moved, not cloned
                is_sell: arb.is_sell,
                expected_profit_pct: arb.profit_pct,
                expected_profit: arb.net_profit,
                fees_estimated: arb.fees,
                total_capital_required: config.capital,
                current_prices,
                current_no_prices,
                optimal_bets,
                neg_risk: arb.neg_risk,
                n_scenarios: arb.n_scenarios,
                hours_to_resolve: hours,
                score,
                min_leg_depth_usd: if min_leg_depth_usd.is_infinite() { 0.0 } else { min_leg_depth_usd },
                capital_efficiency: arb.capital_efficiency,
                collateral_per_unit: arb.collateral_per_unit,
                origin_ts: entry.origin_ts,
            });
        }
    }

    // Sort by score descending, return top N
    opportunities.sort_by(|a, b| b.score.total_cmp(&a.score));
    opportunities.truncate(top_n);

    (opportunities, n_urgent, n_bg, entries.len(), n_skipped_held)
}
