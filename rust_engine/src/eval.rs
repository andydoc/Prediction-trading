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
use crate::queue::EvalQueue;
use std::sync::Arc;

/// Stored constraint definition (loaded from Python once after constraint detection).
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
}

#[derive(Debug, Clone)]
pub struct MarketRef {
    pub market_id: String,
    pub yes_asset_id: String,
    pub no_asset_id: String,
    pub name: String,
}

/// Holds all constraint definitions. Populated from Python after constraint detection.
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
}

/// Config for arb evaluation.
#[derive(Debug, Clone)]
pub struct EvalConfig {
    pub capital: f64,
    pub fee_rate: f64,
    pub min_profit_threshold: f64,
    pub max_profit_threshold: f64,
    pub max_fw_iter: usize,
}

/// A found opportunity — returned to Python, pre-ranked.
#[derive(Debug, Clone)]
pub struct Opportunity {
    pub constraint_id: String,
    pub market_ids: Vec<String>,
    pub market_names: Vec<String>,
    pub method: String,
    pub expected_profit_pct: f64,
    pub expected_profit: f64,
    pub fees_estimated: f64,
    pub total_capital_required: f64,
    pub current_prices: HashMap<String, f64>,
    pub optimal_bets: HashMap<String, f64>,
    pub neg_risk: bool,
    pub n_scenarios: Option<usize>,
    /// Hours until resolution (0 = unknown)
    pub hours_to_resolve: f64,
    /// Score = profit_pct / effective_hours (higher = better)
    pub score: f64,
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

        // Try direct mutex arb first
        let mut result: Option<ArbResult> = arb::check_mutex_arb(
            &market_ids, &yes_prices, &no_prices,
            config.capital, config.fee_rate,
            config.min_profit_threshold, config.max_profit_threshold,
            constraint.is_neg_risk,
        );

        // If no direct arb, try polytope (catches partial hedges)
        if result.is_none() && constraint.constraint_type == "mutex" {
            result = arb::polytope_arb(
                &market_ids, &yes_prices, &constraint.constraint_type,
                config.capital, config.fee_rate,
                config.min_profit_threshold, config.max_profit_threshold,
                &constraint.implications, config.max_fw_iter,
            );
        }

        if let Some(arb) = result {
            let mut current_prices = HashMap::new();
            let mut optimal_bets = HashMap::new();
            for (mid, bet) in &arb.bets {
                optimal_bets.insert(mid.clone(), *bet);
            }
            for (i, mid) in market_ids.iter().enumerate() {
                current_prices.insert(mid.clone(), yes_prices[i]);
            }

            // Compute hours to resolution
            let hours = if constraint.end_date_ts > now_ts {
                (constraint.end_date_ts - now_ts) / 3600.0
            } else if constraint.end_date_ts > 0.0 {
                0.01  // already past — resolve imminently
            } else {
                24.0 * 30.0  // unknown — assume 30 days (will be filtered by Python max_days)
            };

            // Score: profit_pct / effective_hours (higher = better)
            let score = arb.profit_pct / hours.max(0.01);

            opportunities.push(Opportunity {
                constraint_id: constraint.constraint_id.clone(),
                market_ids: market_ids.clone(),
                market_names,
                method: arb.method.clone(),
                expected_profit_pct: arb.profit_pct,
                expected_profit: arb.net_profit,
                fees_estimated: arb.fees,
                total_capital_required: config.capital,
                current_prices,
                optimal_bets,
                neg_risk: arb.neg_risk,
                n_scenarios: arb.n_scenarios,
                hours_to_resolve: hours,
                score,
            });
        }
    }

    // Sort by score descending, return top N
    opportunities.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    opportunities.truncate(top_n);

    (opportunities, n_urgent, n_bg, entries.len(), n_skipped_held)
}
