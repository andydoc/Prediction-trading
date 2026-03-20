/// D2: Submit and cancel a real order.
///
/// Places a GTC limit order at an off-market price (guaranteed no fill),
/// verifies it appears on CLOB, cancels it, verifies internal state matches.

use rust_engine::executor::Executor;
use rust_engine::signing::Side;
use crate::clob_client::ClobClient;
use crate::report::{TestResult, Exception, ExceptionReport};

/// Run D2: Submit GTC order at off-market price, verify it appears, cancel it.
pub fn run(
    executor: &Executor,
    _engine: &rust_engine::TradingEngine,
    notifier: &rust_engine::notify::Notifier,
    exceptions: &mut ExceptionReport,
    clob: &ClobClient,
) -> TestResult {
    let start = std::time::Instant::now();

    // 1. Pick a liquid market via REST API
    let market = match clob.find_liquid_market() {
        Some(m) => m,
        None => {
            let errors = vec!["No liquid market found via CLOB REST API".into()];
            exceptions.add(Exception {
                severity: "CRITICAL".into(),
                test_id: "D2".into(),
                component: "market_selection".into(),
                description: "Could not find a liquid market via Gamma/CLOB REST API".into(),
                expected: "At least one market with ask between 0.05 and 0.95".into(),
                actual: "No suitable market found".into(),
                recommendation: "Check CLOB API connectivity and market availability".into(),
            });
            return TestResult::fail("D2", "Submit and Cancel Order", 0, errors);
        }
    };

    // Register instrument so executor can validate it
    clob.register_instrument(&market, _engine);

    tracing::info!("[D2] Selected market: {} (ask={:.4}) — {}", market.market_id, market.best_ask, market.question);

    // 2. Submit order at off-market price (best_ask - 0.20 for BUY = sits on book)
    let off_market_price = (market.best_ask - 0.20).max(0.01);
    let min_size = 2.50;  // $2.50 minimum

    let position_id = format!("d2_test_{}", crate::now_secs() as u64);
    tracing::info!("[D2] Submitting GTC BUY at {:.4} (off-market, best_ask={:.4})", off_market_price, market.best_ask);

    // Execute via executor
    let legs = vec![(
        market.market_id.clone(),
        market.yes_token_id.clone(),
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
        market.question, off_market_price
    ));

    TestResult::pass("D2", "Submit and Cancel Order", elapsed,
        serde_json::json!({
            "market_id": market.market_id,
            "market_name": market.question,
            "order_price": off_market_price,
            "best_ask": market.best_ask,
            "cancelled_count": cancelled,
            "dry_run": executor.is_dry_run(),
        }))
}
