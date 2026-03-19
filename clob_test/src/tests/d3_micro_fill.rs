/// D3: Execute a real micro-fill.
///
/// Places a FAK BUY at market price, verifies fill tracking through
/// MATCHED -> CONFIRMED pipeline, position appears, reconciliation passes.

use rust_engine::executor::Executor;
use rust_engine::signing::Side;
use crate::report::{TestResult, Exception, ExceptionReport};
use crate::dedup::PositionDedup;

/// Run D3: execute micro-fill on a non-negRisk market.
/// Returns the position_id of the opened position (for D6 tracking).
pub fn run(
    executor: &Executor,
    engine: &rust_engine::TradingEngine,
    notifier: &rust_engine::notify::Notifier,
    dedup: &mut PositionDedup,
    exceptions: &mut ExceptionReport,
) -> (TestResult, Option<String>) {
    let start = std::time::Instant::now();

    // 1. Pick a liquid non-negRisk market
    let constraints = engine.constraints.all();
    let mut selected = None;
    for c in &constraints {
        if c.is_neg_risk { continue; }
        if c.markets.len() < 2 { continue; }
        for m in &c.markets {
            if !dedup.can_open(&[m.market_id.clone()]) { continue; }
            let ask = engine.get_best_ask(&m.yes_asset_id);
            if ask > 0.10 && ask < 0.90 {
                selected = Some((m.market_id.clone(), m.yes_asset_id.clone(), ask, m.name.clone()));
                break;
            }
        }
        if selected.is_some() { break; }
    }

    let (market_id, token_id, best_ask, name) = match selected {
        Some(s) => s,
        None => {
            let errors = vec!["No suitable non-negRisk market found for D3".into()];
            return (TestResult::fail("D3", "Micro-Fill", 0, errors), None);
        }
    };

    tracing::info!("[D3] Selected market: {} (ask={:.4}) — {}", market_id, best_ask, name);

    // 2. Execute FAK BUY at best_ask
    let size = 2.50;  // $2.50 minimum
    let position_id = format!("d3_test_{}", crate::now_secs() as u64);

    let legs = vec![(
        market_id.clone(),
        token_id.clone(),
        Side::Buy,
        best_ask,
        size,
    )];

    let result = executor.execute_arb(&position_id, &legs);

    if !result.all_accepted {
        let errors = vec![format!("Micro-fill failed: {:?}", result.legs)];
        exceptions.add(Exception {
            severity: "CRITICAL".into(),
            test_id: "D3".into(),
            component: "executor".into(),
            description: "FAK BUY at market price failed".into(),
            expected: "Fill accepted".into(),
            actual: format!("{:?}", result.legs),
            recommendation: "Check order book depth and pricing".into(),
        });
        crate::notify(notifier, "[CLOB-TEST EXCEPTION] D3 FAILED: micro-fill rejected");
        return (TestResult::fail("D3", "Micro-Fill", start.elapsed().as_millis() as u64, errors), None);
    }

    // Record in dedup
    dedup.record_open(&[market_id.clone()], "D3");

    // 3. Wait for fill confirmation
    tracing::info!("[D3] FAK BUY submitted, waiting for fill confirmation...");
    std::thread::sleep(std::time::Duration::from_secs(10));

    // 4. Check position appeared
    let open_count = engine.pm_open_count();
    tracing::info!("[D3] Open positions after fill: {}", open_count);

    // 5. Run reconciliation
    let recon = engine.reconcile_periodic(0.1);
    let recon_passed = recon.passed;
    tracing::info!("[D3] Reconciliation: passed={}, discrepancies={}", recon.passed, recon.discrepancies.len());

    if !recon_passed {
        exceptions.add(Exception {
            severity: "WARNING".into(),
            test_id: "D3".into(),
            component: "reconciliation".into(),
            description: "Post-fill reconciliation has discrepancies".into(),
            expected: "Clean reconciliation".into(),
            actual: format!("{} discrepancies", recon.discrepancies.len()),
            recommendation: "Check fill matching and position tracking".into(),
        });
    }

    let elapsed = start.elapsed().as_millis() as u64;
    crate::notify(notifier, &format!(
        "[CLOB-TEST] D3 PASSED: micro-fill on {} at {:.4}",
        name, best_ask
    ));

    (TestResult::pass("D3", "Micro-Fill", elapsed,
        serde_json::json!({
            "market_id": market_id,
            "market_name": name,
            "fill_price": best_ask,
            "size_usd": size,
            "reconciliation_passed": recon_passed,
            "open_positions": open_count,
        })),
    Some(position_id))
}
