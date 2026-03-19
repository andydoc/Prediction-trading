/// D8: Resolve or sell test positions.
///
/// Sells all test positions at market, verifies capital accounting:
/// initial deposit - fees = remaining balance +/- P&L.

use std::collections::HashMap;
use crate::report::{TestResult, Exception, ExceptionReport};

/// Run D8: closeout all positions and verify accounting.
pub fn run(
    _executor: &rust_engine::executor::Executor,
    engine: &rust_engine::TradingEngine,
    notifier: &rust_engine::notify::Notifier,
    initial_usdc: f64,
    exceptions: &mut ExceptionReport,
) -> TestResult {
    let start = std::time::Instant::now();

    let open_count = engine.pm_open_count();
    tracing::info!("[D8] Starting closeout. Open positions: {}", open_count);

    if open_count == 0 {
        tracing::info!("[D8] No positions to close");
        let final_capital = engine.current_capital();

        let elapsed = start.elapsed().as_millis() as u64;
        crate::notify(notifier, &format!(
            "[CLOB-TEST] D8 PASSED: no positions to close. Capital=${:.2}",
            final_capital
        ));
        return TestResult::pass("D8", "Closeout", elapsed,
            serde_json::json!({
                "positions_closed": 0,
                "final_capital": final_capital,
                "initial_capital": initial_usdc,
            }));
    }

    // Get all open position IDs and their market legs
    let position_data: Vec<(String, Vec<(String, String)>)> = {
        let pm = engine.positions.lock();
        pm.open_positions().iter().map(|(pid, pos)| {
            let legs: Vec<(String, String)> = pos.markets.iter()
                .map(|(mid, leg)| (mid.clone(), leg.outcome.clone()))
                .collect();
            (pid.clone(), legs)
        }).collect()
    };

    let mut closed_count = 0;
    let mut close_errors = Vec::new();
    let constraints = engine.constraints.all();

    for (position_id, legs) in &position_data {
        tracing::info!("[D8] Closing position {} ({} markets)", position_id, legs.len());

        // Build bids map from current book
        let mut bids = HashMap::new();
        for (mid, outcome) in legs {
            // Find the asset_id for this market/outcome combination
            for c in &constraints {
                for m in &c.markets {
                    if m.market_id == *mid {
                        let bid = if outcome == "yes" {
                            engine.get_best_bid(&m.yes_asset_id)
                        } else {
                            engine.get_best_bid(&m.no_asset_id)
                        };
                        if bid > 0.0 {
                            bids.insert(mid.clone(), bid);
                        }
                        break;
                    }
                }
            }
        }

        // Attempt to liquidate
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
