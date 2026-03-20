/// D8: Resolve or sell test positions.
///
/// Sells all test positions at market, verifies capital accounting:
/// initial deposit - fees = remaining balance +/- P&L.

use std::collections::HashMap;
use rust_engine::signing::Side;
use crate::clob_client::ClobClient;
use crate::report::{TestResult, Exception, ExceptionReport};

/// Run D8: closeout all positions and verify accounting.
pub fn run(
    executor: &rust_engine::executor::Executor,
    engine: &rust_engine::TradingEngine,
    notifier: &rust_engine::notify::Notifier,
    initial_usdc: f64,
    exceptions: &mut ExceptionReport,
    clob: &ClobClient,
) -> TestResult {
    let start = std::time::Instant::now();

    let open_count = engine.pm_open_count();
    tracing::info!("[D8] Starting closeout. Open positions: {}", open_count);

    if open_count == 0 {
        // This is now a FAIL — if we reached D8 with no positions, something went wrong
        tracing::warn!("[D8] No positions to close — this indicates D5 fill tracking failed");
        let errors = vec!["No positions to close: D5 fill tracking likely failed".into()];
        exceptions.add(Exception {
            severity: "CRITICAL".into(),
            test_id: "D8".into(),
            component: "closeout".into(),
            description: "No open positions at D8 start".into(),
            expected: "At least 1 open position from D5".into(),
            actual: "0 positions".into(),
            recommendation: "Check D5 fill tracking and WS user channel".into(),
        });
        crate::notify(notifier, "[CLOB-TEST EXCEPTION] D8 FAILED: no positions to close");
        return TestResult::fail("D8", "Closeout", start.elapsed().as_millis() as u64, errors);
    }

    /// Per-leg data needed for SELL orders.
    struct LegData {
        market_id: String,
        token_id: String,
        outcome: String,
        shares: f64,
    }

    // Get all open position IDs and their market legs
    let position_data: Vec<(String, Vec<LegData>)> = {
        let pm = engine.positions.lock();
        pm.open_positions().iter().map(|(pid, pos)| {
            let legs: Vec<LegData> = pos.markets.iter().map(|(mid, leg)| {
                // Look up token_id from instrument store by market_id + outcome
                let token_id = engine.instruments.by_market(mid).iter()
                    .find(|i| i.outcome == leg.outcome)
                    .map(|i| i.token_id.clone())
                    .unwrap_or_default();
                LegData {
                    market_id: mid.clone(),
                    token_id,
                    outcome: leg.outcome.clone(),
                    shares: leg.shares,
                }
            }).collect();
            (pid.clone(), legs)
        }).collect()
    };

    let mut closed_count = 0;
    let mut close_errors = Vec::new();

    for (position_id, legs) in &position_data {
        tracing::info!("[D8] Closing position {} ({} markets)", position_id, legs.len());

        // Submit SELL orders for each leg at current best bid
        let mut sell_legs = Vec::new();
        let mut bids = HashMap::new();
        for leg in legs {
            let token_id = &leg.token_id;
            let mid = &leg.market_id;
            let bid = clob.get_best_bid(token_id);
            if bid > 0.0 {
                bids.insert(mid.clone(), bid);
                let sell_size = leg.shares * bid; // value in USD
                sell_legs.push((mid.clone(), token_id.clone(), Side::Sell, bid, sell_size));
                tracing::info!("[D8] SELL leg: market={} token={} bid={:.4} shares={:.2}",
                    mid, token_id, bid, leg.shares);
            } else {
                tracing::warn!("[D8] No bid for token {} in market {}", token_id, mid);
            }
        }

        if !sell_legs.is_empty() {
            let sell_result = executor.execute_arb(&format!("{}_close", position_id), &sell_legs);
            let sold_count = sell_result.legs.iter()
                .filter(|l| matches!(l, rust_engine::executor::OrderResult::Accepted(_)))
                .count();
            tracing::info!("[D8] Sell orders: {}/{} accepted", sold_count, sell_legs.len());

            // Wait for sells to settle
            std::thread::sleep(std::time::Duration::from_secs(10));
        }

        // Update engine accounting
        match engine.liquidate_position(&position_id, "d8_closeout", &bids) {
            Some((proceeds, profit)) => {
                closed_count += 1;
                tracing::info!("[D8] Closed {}: proceeds={:.4}, profit={:.4}",
                    position_id, proceeds, profit);
            }
            None => {
                close_errors.push(format!("Failed to close {}", position_id));
                tracing::warn!("[D8] Failed to close position {}", position_id);
            }
        }
    }

    // Wait for settlement
    std::thread::sleep(std::time::Duration::from_secs(5));

    // Verify accounting
    let final_capital = engine.current_capital();
    let remaining_open = engine.pm_open_count();

    let snapshot = engine.dashboard_snapshot();
    let total_value = snapshot.total_value;

    tracing::info!("[D8] Closeout complete: closed={}, remaining_open={}, final_capital={:.2}, total_value={:.2}",
        closed_count, remaining_open, final_capital, total_value);

    // Check accounting delta
    let accounting_delta = (total_value - initial_usdc).abs();
    if accounting_delta > 5.0 {
        exceptions.add(Exception {
            severity: "WARNING".into(),
            test_id: "D8".into(),
            component: "accounting".into(),
            description: format!("Capital accounting delta: ${:.2}", accounting_delta),
            expected: format!("Total value ~${:.2} (initial deposit)", initial_usdc),
            actual: format!("Total value=${:.2}", total_value),
            recommendation: "Review fee tracking and P&L calculations".into(),
        });
    }

    if remaining_open > 0 {
        close_errors.push(format!("{} positions still open after closeout", remaining_open));
    }

    let elapsed = start.elapsed().as_millis() as u64;

    if close_errors.is_empty() {
        crate::notify(notifier, &format!(
            "[CLOB-TEST] D8 PASSED: closed {} positions. Final=${:.2} (delta=${:.2})",
            closed_count, final_capital, accounting_delta
        ));
        TestResult::pass("D8", "Closeout", elapsed,
            serde_json::json!({
                "positions_closed": closed_count,
                "remaining_open": remaining_open,
                "initial_capital": initial_usdc,
                "final_capital": final_capital,
                "total_value": total_value,
                "accounting_delta": accounting_delta,
            }))
    } else {
        crate::notify(notifier, &format!(
            "[CLOB-TEST] D8 PARTIAL: {} errors. Final=${:.2}",
            close_errors.len(), final_capital
        ));
        TestResult::fail("D8", "Closeout", elapsed, close_errors)
    }
}
