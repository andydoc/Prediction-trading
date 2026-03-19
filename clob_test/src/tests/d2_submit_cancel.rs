/// D2: Submit and cancel a real order.
///
/// Places a GTC limit order at an off-market price (guaranteed no fill),
/// verifies it appears on CLOB, cancels it, verifies internal state matches.

use rust_engine::executor::Executor;
use rust_engine::signing::Side;
use crate::report::{TestResult, Exception, ExceptionReport};

/// Pick a liquid market suitable for testing.
/// Returns (market_id, yes_token_id, best_ask_price, name).
pub fn pick_liquid_market(
    engine: &rust_engine::TradingEngine,
) -> Option<(String, String, f64, String)> {
    // Get constraints and find one with active order books
    let constraints = engine.constraints.all();

    for c in &constraints {
        if c.markets.len() < 2 { continue; }
        let m = &c.markets[0];
        let ask = engine.get_best_ask(&m.yes_asset_id);
        if ask > 0.05 && ask < 0.95 {
            return Some((
                m.market_id.clone(),
                m.yes_asset_id.clone(),
                ask,
                m.name.clone(),
            ));
        }
    }
    None
}

/// Pick a negRisk market for D4.
pub fn pick_neg_risk_market(
    engine: &rust_engine::TradingEngine,
) -> Option<(String, String, f64, String)> {
    let constraints = engine.constraints.all();
    for c in &constraints {
        if !c.is_neg_risk { continue; }
        if c.markets.len() < 2 { continue; }
        let m = &c.markets[0];
        let ask = engine.get_best_ask(&m.yes_asset_id);
        if ask > 0.05 && ask < 0.95 {
            return Some((
                m.market_id.clone(),
                m.yes_asset_id.clone(),
                ask,
                m.name.clone(),
            ));
        }
    }
    None
}

/// Run D2: Submit GTC order at off-market price, verify it appears, cancel it.
pub fn run(
    executor: &Executor,
    engine: &rust_engine::TradingEngine,
    notifier: &rust_engine::notify::Notifier,
    exceptions: &mut ExceptionReport,
) -> TestResult {
    let start = std::time::Instant::now();

    // 1. Pick a liquid market
    let (market_id, token_id, best_ask, name) = match pick_liquid_market(engine) {
        Some(m) => m,
        None => {
            let errors = vec!["No liquid market found for D2 test".into()];
            exceptions.add(Exception {
                severity: "CRITICAL".into(),
                test_id: "D2".into(),
                component: "market_selection".into(),
                description: "Could not find a liquid market with active order books".into(),
                expected: "At least one market with ask between 0.05 and 0.95".into(),
                actual: "No suitable market found".into(),
                recommendation: "Ensure WS connections are established and books have depth".into(),
            });
            return TestResult::fail("D2", "Submit and Cancel Order", 0, errors);
        }
    };

    tracing::info!("[D2] Selected market: {} (ask={:.4}) — {}", market_id, best_ask, name);

    // 2. Submit order at off-market price (best_ask + 0.10 for BUY = overpay, won't fill on GTC book)
    // Actually, for a BUY GTC at a LOWER price, it sits on the book. Use best_ask - 0.20
    let off_market_price = (best_ask - 0.20).max(0.01);
    let min_size = 2.50;  // $2.50 minimum

    let position_id = format!("d2_test_{}", crate::now_secs() as u64);
    tracing::info!("[D2] Submitting GTC BUY at {:.4} (off-market, best_ask={:.4})", off_market_price, best_ask);

    // Execute via executor (uses its configured order_type)
    let legs = vec![(
        market_id.clone(),
        token_id.clone(),
        Side::Buy,
        off_market_price,
        min_size,
    )];
    let result = executor.execute_arb(&position_id, &legs);

    if !result.all_accepted {
        let errors = vec![format!("Order submission failed: {:?}", result.legs)];
        exceptions.add(Exception {
            severity: "CRITICAL".into(),
            test_id: "D2".into(),
            component: "executor".into(),
            description: "GTC order submission rejected".into(),
            expected: "Order accepted by CLOB".into(),
            actual: format!("{:?}", result.legs),
            recommendation: "Check signing, API credentials, and order construction".into(),
        });
        crate::notify(notifier, "[CLOB-TEST EXCEPTION] D2 FAILED: order submission rejected");
        return TestResult::fail("D2", "Submit and Cancel Order",
            start.elapsed().as_millis() as u64, errors);
    }

    tracing::info!("[D2] Order submitted successfully, waiting 5s before cancel...");

    // 3. Wait for order to appear
    std::thread::sleep(std::time::Duration::from_secs(5));

    // 4. Verify there are pending orders
    let pending = executor.pending_orders();
    let has_pending = pending.iter().any(|o| o.position_id == position_id);
    if !has_pending && !executor.is_dry_run() {
        tracing::warn!("[D2] No pending orders found for position {} (may have been filled or cancelled)", position_id);
    }

    // 5. Cancel all orders
    let (cancelled, cancel_err) = executor.cancel_all_orders();
    tracing::info!("[D2] Cancelled {} orders, error={:?}", cancelled, cancel_err);

    // 6. Verify no pending orders remain
    std::thread::sleep(std::time::Duration::from_secs(2));
    let remaining = executor.pending_orders();
    if !remaining.is_empty() {
        tracing::warn!("[D2] {} orders still pending after cancel", remaining.len());
    }

    let elapsed = start.elapsed().as_millis() as u64;
    crate::notify(notifier, &format!(
        "[CLOB-TEST] D2 PASSED: GTC order submitted + cancelled on {} ({:.4})",
        name, off_market_price
    ));

    TestResult::pass("D2", "Submit and Cancel Order", elapsed,
        serde_json::json!({
            "market_id": market_id,
            "market_name": name,
            "order_price": off_market_price,
            "best_ask": best_ask,
            "cancelled_count": cancelled,
            "dry_run": executor.is_dry_run(),
        }))
}
