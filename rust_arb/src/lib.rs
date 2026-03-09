use pyo3::prelude::*;
use pyo3::types::PyDict;

/// Fast mutex direct arb check — equivalent to Python _arb_mutex_direct()
///
/// Takes parallel arrays of YES ask prices and NO ask prices for each market,
/// returns a dict with arb results or None if no opportunity.
///
/// This is the hot path: runs for every constraint eval (~1600 constraints,
/// each 80ms in Python → target <1ms in Rust).
#[pyfunction]
fn check_mutex_arb(
    py: Python<'_>,
    market_ids: Vec<String>,
    yes_prices: Vec<f64>,
    no_prices: Vec<f64>,
    capital: f64,
    fee_rate: f64,
    min_profit_threshold: f64,
    max_profit_threshold: f64,
    is_neg_risk: bool,
) -> PyResult<Option<PyObject>> {
    let n = market_ids.len();
    if n < 2 || yes_prices.len() != n || no_prices.len() != n {
        return Ok(None);
    }

    // Skip dead markets
    if yes_prices.iter().any(|&p| p < 0.02) {
        return Ok(None);
    }

    // Price sum sanity (with epsilon for float precision)
    let price_sum: f64 = yes_prices.iter().sum();
    if price_sum < 0.899 {
        return Ok(None); // Incomplete mutex group
    }

    // --- Buy-side: sum(YES asks) < 1.0 ---
    if price_sum < 1.0 {
        let units = capital / price_sum;
        let payout = units; // payout = units * 1.0
        let fees = payout * fee_rate;
        let gross_profit = payout - capital;
        let net_profit = gross_profit - fees;
        let profit_pct = net_profit / capital;

        if profit_pct >= min_profit_threshold && profit_pct <= max_profit_threshold {
            let dict = PyDict::new(py);
            dict.set_item("method", "mutex_buy_all")?;
            dict.set_item("profitable", true)?;
            dict.set_item("profit_pct", profit_pct)?;
            dict.set_item("net_profit", net_profit)?;
            dict.set_item("gross_profit", gross_profit)?;
            dict.set_item("fees", fees)?;
            dict.set_item("price_sum", price_sum)?;
            dict.set_item("neg_risk", is_neg_risk)?;

            // Bets: proportional to price
            let bets = PyDict::new(py);
            for i in 0..n {
                let bet = capital * yes_prices[i] / price_sum;
                bets.set_item(&market_ids[i], bet)?;
            }
            dict.set_item("bets", bets)?;
            return Ok(Some(dict.into()));
        }
    }

    // --- Sell-side: sum(YES asks) > 1.0 ---
    if price_sum > 1.0 {
        let no_cost_per_unit: f64 = no_prices.iter().sum();
        if no_cost_per_unit <= 0.0 {
            return Ok(None);
        }

        // negRisk: collateral = $1.00 per unit set
        let collateral_per_unit = if is_neg_risk { 1.0 } else { no_cost_per_unit };
        let cap_efficiency = no_cost_per_unit / collateral_per_unit;

        let units = capital / no_cost_per_unit;
        let payout = units * (n as f64 - 1.0);
        let fees = payout * fee_rate;
        let gross_profit = payout - capital;
        let net_profit = gross_profit - fees;
        let profit_pct = net_profit / capital;

        if profit_pct >= min_profit_threshold && profit_pct <= max_profit_threshold {
            let dict = PyDict::new(py);
            dict.set_item("method", "mutex_sell_all")?;
            dict.set_item("profitable", true)?;
            dict.set_item("profit_pct", profit_pct)?;
            dict.set_item("net_profit", net_profit)?;
            dict.set_item("gross_profit", gross_profit)?;
            dict.set_item("fees", fees)?;
            dict.set_item("price_sum", price_sum)?;
            dict.set_item("no_price_sum", no_cost_per_unit)?;
            dict.set_item("neg_risk", is_neg_risk)?;
            dict.set_item("collateral_per_unit", collateral_per_unit)?;
            dict.set_item("capital_efficiency", cap_efficiency)?;

            let bets = PyDict::new(py);
            for i in 0..n {
                let bet = capital * no_prices[i] / no_cost_per_unit;
                bets.set_item(&market_ids[i], bet)?;
            }
            dict.set_item("bets", bets)?;
            return Ok(Some(dict.into()));
        }
    }

    Ok(None)
}

/// Frank-Wolfe optimal bet sizing for polytope arbitrage.
///
/// Given prices and valid outcome scenarios, finds the bet vector that
/// maximises guaranteed profit (worst-case payout across all scenarios).
///
/// Returns (optimal_bets, guaranteed_profit) or None if not profitable.
#[pyfunction]
fn frank_wolfe_bets(
    _py: Python<'_>,
    prices: Vec<f64>,
    scenarios: Vec<Vec<f64>>,  // S × N matrix (each row is a valid scenario)
    capital: f64,
    max_iter: usize,
    tol: f64,
) -> PyResult<Option<(Vec<f64>, f64)>> {
    let n = prices.len();
    let s = scenarios.len();
    if n == 0 || s == 0 {
        return Ok(None);
    }

    // Clip prices
    let p: Vec<f64> = prices.iter().map(|&x| x.max(1e-6).min(1.0)).collect();

    // Initialise: equal allocation
    let mut y: Vec<f64> = vec![0.0; n];
    let sum_p: f64 = p.iter().sum();
    for i in 0..n {
        y[i] = (capital / sum_p) / p[i]; // shares proportional to 1/price
        y[i] = y[i].max(0.0);
    }

    // Helper: guaranteed payout (worst case across scenarios)
    let guaranteed_payout = |y_vec: &[f64]| -> (f64, usize) {
        let mut min_payout = f64::INFINITY;
        let mut worst_idx = 0;
        for (si, scenario) in scenarios.iter().enumerate() {
            let payout: f64 = scenario.iter().zip(y_vec.iter())
                .map(|(&s, &y)| s * y).sum();
            if payout < min_payout {
                min_payout = payout;
                worst_idx = si;
            }
        }
        (min_payout, worst_idx)
    };

    // Frank-Wolfe iterations
    for t in 0..max_iter {
        let (_g_val, worst_idx) = guaranteed_payout(&y);
        let grad = &scenarios[worst_idx]; // subgradient = worst-case scenario

        // Linear oracle: put all capital on market with highest grad_i / p_i
        let mut best_i = 0;
        let mut best_ratio = f64::NEG_INFINITY;
        for i in 0..n {
            let ratio = grad[i] / p[i].max(1e-9);
            if ratio > best_ratio {
                best_ratio = ratio;
                best_i = i;
            }
        }

        // Oracle solution: all capital on best market
        let mut s_vec = vec![0.0; n];
        s_vec[best_i] = capital / p[best_i];

        // Step size (standard FW decay)
        let gamma = 2.0 / (t as f64 + 2.0);

        // Update
        let mut y_new = vec![0.0; n];
        let mut diff_norm = 0.0;
        for i in 0..n {
            y_new[i] = (1.0 - gamma) * y[i] + gamma * s_vec[i];
            let d = y_new[i] - y[i];
            diff_norm += d * d;
        }

        if diff_norm.sqrt() < tol {
            y = y_new;
            break;
        }
        y = y_new;
    }

    // Ensure non-negative
    for v in y.iter_mut() {
        *v = v.max(0.0);
    }

    let (profit, _) = guaranteed_payout(&y);
    let net = profit - capital;
    if net <= 0.0 {
        return Ok(None);
    }

    Ok(Some((y, net)))
}

/// Compute effective fill price (VWAP) for a given trade size.
/// Walks the ask book levels. Returns 0.0 if insufficient depth.
/// Called on every WS price event — must be very fast.
#[pyfunction]
fn effective_fill_price(
    ask_prices: Vec<f64>,
    ask_sizes: Vec<f64>,
    trade_size_usd: f64,
) -> f64 {
    if ask_prices.is_empty() || trade_size_usd <= 0.0 {
        return 0.0;
    }
    let mut remaining = trade_size_usd;
    let mut total_shares = 0.0;
    let mut total_cost = 0.0;

    for i in 0..ask_prices.len() {
        let price = ask_prices[i];
        let size = ask_sizes[i];
        let level_usd = price * size;
        if level_usd <= 0.0 {
            continue;
        }
        if level_usd >= remaining {
            let shares = remaining / price;
            total_shares += shares;
            total_cost += remaining;
            remaining = 0.0;
            break;
        } else {
            total_shares += size;
            total_cost += level_usd;
            remaining -= level_usd;
        }
    }
    if total_shares <= 0.0 || remaining > 0.0 {
        return 0.0; // Insufficient depth
    }
    total_cost / total_shares
}

/// Python module registration
#[pymodule]
fn rust_arb(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(check_mutex_arb, m)?)?;
    m.add_function(wrap_pyfunction!(frank_wolfe_bets, m)?)?;
    m.add_function(wrap_pyfunction!(effective_fill_price, m)?)?;
    Ok(())
}
