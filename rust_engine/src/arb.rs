/// Pure Rust arb math — extracted from rust_arb, no PyO3 dependencies.
/// Used by evaluate_batch() to run the entire hot path in Rust.

/// Constraint type enum — replaces string-based dispatch for type safety.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintType {
    Mutex,
    Complementary,
    LogicalImplication,
}

impl ConstraintType {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "mutual_exclusivity" | "mutex" => Some(Self::Mutex),
            "complementary" => Some(Self::Complementary),
            "logical_implication" => Some(Self::LogicalImplication),
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Mutex => "mutex",
            Self::Complementary => "complementary",
            Self::LogicalImplication => "logical_implication",
        }
    }
}

/// Minimum price sum to consider mutex arb (filters garbage data).
const MIN_PRICE_SUM_THRESHOLD: f64 = 0.899;
/// Maximum markets for polytope optimization.
const MAX_POLYTOPE_MARKETS: usize = 12;

/// Result of a mutex arb check.
#[derive(Debug, Clone)]
pub struct ArbResult {
    pub method: String,        // "mutex_buy_all", "mutex_sell_all", or "polytope_fw"
    pub is_sell: bool,         // true for sell strategies (no substring matching needed)
    pub profitable: bool,
    pub profit_pct: f64,
    pub net_profit: f64,
    pub gross_profit: f64,
    pub fees: f64,
    pub price_sum: f64,
    pub neg_risk: bool,
    /// market_id → bet amount
    pub bets: Vec<(String, f64)>,
    // Sell-specific
    pub no_price_sum: Option<f64>,
    pub collateral_per_unit: Option<f64>,
    pub capital_efficiency: Option<f64>,
    // Polytope-specific
    pub constraint_type: Option<String>,
    pub n_scenarios: Option<usize>,
}

/// Check for direct mutex arb (buy-all or sell-all).
pub fn check_mutex_arb(
    market_ids: &[String],
    yes_prices: &[f64],
    no_prices: &[f64],
    capital: f64,
    fee_rate: f64,
    min_profit: f64,
    max_profit: f64,
    is_neg_risk: bool,
) -> Option<ArbResult> {
    let n = market_ids.len();
    if n < 2 || yes_prices.len() != n || no_prices.len() != n {
        return None;
    }
    if yes_prices.iter().any(|&p| p < 0.02) { return None; }

    let price_sum: f64 = yes_prices.iter().sum();
    if price_sum < MIN_PRICE_SUM_THRESHOLD { return None; }

    // --- Buy-side: sum(YES asks) < 1.0 ---
    if price_sum < 1.0 {
        let units = capital / price_sum;
        let payout = units;
        let fees = payout * fee_rate;
        let gross_profit = payout - capital;
        let net_profit = gross_profit - fees;
        let profit_pct = net_profit / capital;

        if profit_pct >= min_profit && profit_pct <= max_profit {
            let bets: Vec<(String, f64)> = market_ids.iter().enumerate()
                .map(|(i, mid)| (mid.clone(), capital * yes_prices[i] / price_sum))
                .collect();
            return Some(ArbResult {
                method: "mutex_buy_all".into(), is_sell: false, profitable: true,
                profit_pct, net_profit, gross_profit, fees, price_sum,
                neg_risk: is_neg_risk, bets,
                no_price_sum: None, collateral_per_unit: None,
                capital_efficiency: None, constraint_type: None, n_scenarios: None,
            });
        }
    }

    // --- Sell-side: sum(YES asks) > 1.0 ---
    if price_sum > 1.0 {
        let no_cost: f64 = no_prices.iter().sum();
        if no_cost <= 0.0 { return None; }
        let collateral = if is_neg_risk { 1.0 } else { no_cost };
        let cap_eff = no_cost / collateral;
        let units = capital / no_cost;
        let payout = units * (n as f64 - 1.0);
        let fees = payout * fee_rate;
        let gross_profit = payout - capital;
        let net_profit = gross_profit - fees;
        let profit_pct = net_profit / capital;

        if profit_pct >= min_profit && profit_pct <= max_profit {
            let bets: Vec<(String, f64)> = market_ids.iter().enumerate()
                .map(|(i, mid)| (mid.clone(), capital * no_prices[i] / no_cost))
                .collect();
            return Some(ArbResult {
                method: "mutex_sell_all".into(), is_sell: true, profitable: true,
                profit_pct, net_profit, gross_profit, fees, price_sum,
                neg_risk: is_neg_risk, bets,
                no_price_sum: Some(no_cost), collateral_per_unit: Some(collateral),
                capital_efficiency: Some(cap_eff), constraint_type: None, n_scenarios: None,
            });
        }
    }
    None
}

/// Build valid outcome scenarios for a constraint type.
fn build_scenarios(n: usize, constraint_type: ConstraintType, implications: &[(usize, usize)]) -> Vec<Vec<f64>> {
    match constraint_type {
        ConstraintType::Mutex => {
            (0..n).map(|i| { let mut r = vec![0.0; n]; r[i] = 1.0; r }).collect()
        }
        ConstraintType::Complementary => {
            let mut s: Vec<Vec<f64>> = (0..n).map(|i| { let mut r = vec![0.0; n]; r[i] = 1.0; r }).collect();
            if n > 2 { s.push(vec![0.0; n]); }
            s
        }
        ConstraintType::LogicalImplication => {
            let total = 1usize << n;
            let mut scenarios = Vec::new();
            for mask in 0..total {
                let outcome: Vec<f64> = (0..n).map(|bit| if mask & (1 << bit) != 0 { 1.0 } else { 0.0 }).collect();
                let valid = implications.iter().all(|&(i, j)| i >= n || j >= n || outcome[i] == 0.0 || outcome[j] == 1.0);
                if valid { scenarios.push(outcome); }
            }
            scenarios
        }
    }
}

/// Full polytope arb: build scenarios + Frank-Wolfe optimisation.
pub fn polytope_arb(
    market_ids: &[String],
    yes_prices: &[f64],
    constraint_type: ConstraintType,
    capital: f64,
    fee_rate: f64,
    min_profit: f64,
    max_profit: f64,
    implications: &[(usize, usize)],
    max_fw_iter: usize,
) -> Option<ArbResult> {
    let n = market_ids.len();
    if n < 2 || yes_prices.len() != n { return None; }
    if yes_prices.iter().any(|&p| p < 0.02) { return None; }
    if n > MAX_POLYTOPE_MARKETS { return None; }

    let price_sum: f64 = yes_prices.iter().sum();
    let ct = constraint_type;
    // B20: Threshold logic — these ranges are intentionally overlapping:
    //   - mutex: price_sum must be >= 0.90 (tighter filter for the common case)
    //   - all types: price_sum must be in [0.30, 1.40] (rejects garbage data)
    //   - 2-market: additional floor at 0.80 (2-market arbs need stronger signal)
    if ct == ConstraintType::Mutex && price_sum < 0.90 { return None; }
    if price_sum < 0.30 || price_sum > 1.40 { return None; }
    if n == 2 && price_sum < 0.80 { return None; }

    let scenarios = build_scenarios(n, ct, implications);
    if scenarios.is_empty() { return None; }

    let p: Vec<f64> = yes_prices.iter().map(|&x| x.max(1e-6).min(1.0)).collect();
    let sum_p: f64 = p.iter().sum();
    let mut y: Vec<f64> = (0..n).map(|i| ((capital / sum_p) / p[i]).max(0.0)).collect();

    // Frank-Wolfe iterations
    let guaranteed_payout = |y_vec: &[f64]| -> (f64, usize) {
        let mut min_p = f64::INFINITY;
        let mut worst = 0;
        for (si, sc) in scenarios.iter().enumerate() {
            let pay: f64 = sc.iter().zip(y_vec.iter()).map(|(&s, &yv)| s * yv).sum();
            if pay < min_p { min_p = pay; worst = si; }
        }
        (min_p, worst)
    };

    for t in 0..max_fw_iter {
        let (_, worst) = guaranteed_payout(&y);
        let grad = &scenarios[worst];
        let mut best_i = 0;
        let mut best_r = f64::NEG_INFINITY;
        for i in 0..n {
            let r = grad[i] / p[i].max(1e-9);
            if r > best_r { best_r = r; best_i = i; }
        }
        let mut s_vec = vec![0.0; n];
        s_vec[best_i] = capital / p[best_i];
        let gamma = 2.0 / (t as f64 + 2.0);
        let mut y_new = vec![0.0; n];
        let mut diff = 0.0;
        for i in 0..n {
            y_new[i] = (1.0 - gamma) * y[i] + gamma * s_vec[i];
            let d = y_new[i] - y[i]; diff += d * d;
        }
        if diff.sqrt() < 1e-6 { y = y_new; break; }
        y = y_new;
    }
    for v in y.iter_mut() { *v = v.max(0.0); }

    let (guaranteed, _) = guaranteed_payout(&y);
    let eff_fee = fee_rate * n as f64;
    let fees = capital * eff_fee;
    let gross_profit = guaranteed - capital;
    let net_profit = gross_profit - fees;
    let profit_pct = net_profit / capital;

    // B13: Guard against NaN/Inf propagation from degenerate price inputs
    if !profit_pct.is_finite() { return None; }

    if profit_pct < min_profit || profit_pct > max_profit { return None; }

    let bets: Vec<(String, f64)> = market_ids.iter().enumerate()
        .map(|(i, mid)| (mid.clone(), y[i] * p[i]))
        .collect();

    Some(ArbResult {
        method: "polytope_fw".into(), is_sell: false, profitable: true,
        profit_pct, net_profit, gross_profit, fees, price_sum,
        neg_risk: false, bets,
        no_price_sum: None, collateral_per_unit: None, capital_efficiency: None,
        constraint_type: Some(constraint_type.as_str().to_string()),
        n_scenarios: Some(scenarios.len()),
    })
}
