/// A5: Constraint Detector — identifies mutex groups from market data.
///
/// Groups markets by `negRiskMarketID`, validates price sums against
/// config-driven bounds, and builds all index maps in one pass.
/// Replaces Python `ConstraintDetector` + `build_index()` + `_load_constraints_into_rust()`.
use std::collections::HashMap;
use crate::eval::{Constraint, MarketRef};

/// Minimal market representation for constraint detection.
pub struct DetectableMarket {
    pub market_id: String,
    pub question: String,
    pub yes_asset_id: String,
    pub no_asset_id: String,
    pub neg_risk: bool,
    pub neg_risk_market_id: String,
    pub yes_price: f64,
    pub end_date_ts: f64,
}

/// Config for completeness guards (read from config.yaml).
pub struct DetectionConfig {
    pub min_price_sum: f64,  // default 0.85
    pub max_price_sum: f64,  // default 1.15
    pub min_markets: usize,  // default 2
}

/// Full result of constraint detection + index building.
pub struct DetectionResult {
    pub constraints: Vec<Constraint>,
    /// asset_id → [constraint_ids] (for BookMirror drift detection)
    pub asset_to_constraints: HashMap<String, Vec<String>>,
    /// asset_id → (market_id, is_yes) (for WS resolution mapping)
    pub asset_to_market: HashMap<String, (String, bool)>,
    /// All asset IDs that should be subscribed to via WS
    pub all_asset_ids: Vec<String>,
    /// constraint_id → [asset_ids] (for Tier B hot constraint management)
    pub constraint_to_assets: HashMap<String, Vec<String>>,
    /// constraint_id → |price_sum - 1.0| (spread tightness; lower = better for arb)
    pub constraint_spread: HashMap<String, f64>,
    /// constraint_id → end_date_ts (earliest resolution unix timestamp; 0 = unknown)
    pub constraint_end_ts: HashMap<String, f64>,
    // Stats
    pub n_markets_input: usize,
    pub n_groups: usize,
    pub n_skipped_incomplete: usize,
    pub n_skipped_overpriced: usize,
}

/// Detect mutex constraints from market data, build all indices.
///
/// Algorithm mirrors Python `ConstraintDetector._detect_neg_risk_mutex()`:
/// 1. Group markets by negRiskMarketID where negRisk==true
/// 2. For each group with >= min_markets: validate price sum, build Constraint
/// 3. Build asset→constraint and asset→market index maps
pub fn detect_constraints(
    markets: &[DetectableMarket],
    config: &DetectionConfig,
) -> DetectionResult {
    let n_markets_input = markets.len();

    // Step 1: Group by negRiskMarketID
    let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, m) in markets.iter().enumerate() {
        if m.neg_risk && !m.neg_risk_market_id.is_empty() {
            groups.entry(&m.neg_risk_market_id)
                .or_default()
                .push(i);
        }
    }
    let n_groups = groups.values().filter(|g| g.len() >= config.min_markets).count();

    // Step 2: Build constraints from valid groups
    let mut constraints = Vec::new();
    let mut constraint_spread: HashMap<String, f64> = HashMap::new();
    let mut constraint_end_ts: HashMap<String, f64> = HashMap::new();
    let mut n_skipped_incomplete = 0usize;
    let mut n_skipped_overpriced = 0usize;

    for (nrm_id, indices) in &groups {
        if indices.len() < config.min_markets {
            continue;
        }

        // Sum YES prices for completeness check
        let price_sum: f64 = indices.iter()
            .map(|&i| markets[i].yes_price)
            .sum();

        if price_sum < config.min_price_sum {
            n_skipped_incomplete += 1;
            continue;
        }
        if price_sum > config.max_price_sum {
            n_skipped_overpriced += 1;
            continue;
        }

        // Build constraint ID: mutex_{first 32 chars of negRiskMarketID}
        let id_len = nrm_id.len().min(32);
        let constraint_id = format!("mutex_{}", &nrm_id[..id_len]);

        // Find earliest end_date_ts across the group
        let mut earliest_end = 0.0f64;
        for &i in indices {
            let ts = markets[i].end_date_ts;
            if ts > 0.0 && (earliest_end == 0.0 || ts < earliest_end) {
                earliest_end = ts;
            }
        }

        // Build MarketRef for each market
        let market_refs: Vec<MarketRef> = indices.iter()
            .filter(|&&i| !markets[i].yes_asset_id.is_empty())
            .map(|&i| {
                let m = &markets[i];
                MarketRef {
                    market_id: m.market_id.clone(),
                    yes_asset_id: m.yes_asset_id.clone(),
                    no_asset_id: m.no_asset_id.clone(),
                    name: m.question.clone(),
                }
            })
            .collect();

        if market_refs.len() < config.min_markets {
            continue;
        }

        let cid = constraint_id.clone();
        constraints.push(Constraint {
            constraint_id,
            constraint_type: "mutex".to_string(),
            markets: market_refs,
            is_neg_risk: true,
            implications: Vec::new(),
            end_date_ts: earliest_end,
        });
        constraint_spread.insert(cid.clone(), (price_sum - 1.0).abs());
        constraint_end_ts.insert(cid, earliest_end);
    }

    // Step 3: Build index maps from constraints
    let mut asset_to_constraints: HashMap<String, Vec<String>> = HashMap::new();
    let mut asset_to_market: HashMap<String, (String, bool)> = HashMap::new();
    let mut constraint_to_assets: HashMap<String, Vec<String>> = HashMap::new();
    let mut all_asset_ids = Vec::new();

    for c in &constraints {
        let mut c_assets = Vec::new();
        for mref in &c.markets {
            // YES asset
            if !mref.yes_asset_id.is_empty() {
                asset_to_constraints.entry(mref.yes_asset_id.clone())
                    .or_default()
                    .push(c.constraint_id.clone());
                asset_to_market.entry(mref.yes_asset_id.clone())
                    .or_insert_with(|| (mref.market_id.clone(), true));
                all_asset_ids.push(mref.yes_asset_id.clone());
                c_assets.push(mref.yes_asset_id.clone());
            }
            // NO asset
            if !mref.no_asset_id.is_empty() {
                asset_to_constraints.entry(mref.no_asset_id.clone())
                    .or_default()
                    .push(c.constraint_id.clone());
                asset_to_market.entry(mref.no_asset_id.clone())
                    .or_insert_with(|| (mref.market_id.clone(), false));
                all_asset_ids.push(mref.no_asset_id.clone());
                c_assets.push(mref.no_asset_id.clone());
            }
        }
        constraint_to_assets.insert(c.constraint_id.clone(), c_assets);
    }

    DetectionResult {
        constraints,
        asset_to_constraints,
        asset_to_market,
        all_asset_ids,
        constraint_to_assets,
        constraint_spread,
        constraint_end_ts,
        n_markets_input,
        n_groups,
        n_skipped_incomplete,
        n_skipped_overpriced,
    }
}
