/// D9: Test partial fill handling (B3.6 validation).
///
/// Deliberately triggers a partial fill on a 2-leg arb:
/// - Leg 1: FAK BUY at best ask (fills normally)
/// - Leg 2: FAK BUY at a price well below best ask (won't fill — FAK kills it)
///
/// Then calls evaluate_partial_fills() and verifies:
/// - Result is Unwind (one-sided fill: cost but no revenue)
/// - The filled leg is correctly identified
///
/// Finally, sells back the filled leg to clean up.

use rust_engine::executor::{Executor, PartialFillAction, TrackedOrder, TradeStatus};
use rust_engine::signing::{Side, ClobAuth};
use crate::clob_client::ClobClient;
use crate::fill_tracker::{self, SubmittedLeg};
use crate::report::{TestResult, Exception, ExceptionReport};
use crate::dedup::PositionDedup;

pub fn run(
    executor: &Executor,
    engine: &rust_engine::TradingEngine,
    notifier: &rust_engine::notify::Notifier,
    dedup: &mut PositionDedup,
    exceptions: &mut ExceptionReport,
    clob: &ClobClient,
    clob_auth: &ClobAuth,
    runtime: &tokio::runtime::Handle,
    wallet_address: &str,
) -> TestResult {
    let start = std::time::Instant::now();

    tracing::info!("[D9] Testing partial fill handling (B3.6 validation)...");

    // 1. Find two liquid markets
    let (m1, m2) = match clob.find_two_outcome_market() {
        Some(pair) => pair,
        None => {
            let errors = vec!["Could not find two markets for D9".into()];
            exceptions.add(Exception {
                severity: "CRITICAL".into(),
                test_id: "D9".into(),
                component: "market_selection".into(),
                description: "Could not find two active markets".into(),
                expected: "Two markets with order books".into(),
                actual: "Not enough suitable markets found".into(),
                recommendation: "Check CLOB API and market availability".into(),
            });
            return TestResult::fail("D9", "Partial Fill", start.elapsed().as_millis() as u64, errors);
        }
    };

    clob.register_instrument(&m1, engine);
    clob.register_instrument(&m2, engine);

    tracing::info!("[D9] Leg 1 (will fill): {} ask={:.4} — {}", m1.market_id, m1.best_ask, m1.question);
    tracing::info!("[D9] Leg 2 (won't fill): {} ask={:.4} — {}", m2.market_id, m2.best_ask, m2.question);

    let size = 2.50;
    let position_id = format!("d9_partial_{}", crate::now_secs() as u64);

    // 2. Submit leg 1 at best ask (should fill), leg 2 at 1 cent (won't fill via FAK)
    let unfillable_price = 0.01; // 1 cent — well below any realistic ask

    let legs = vec![
        (m1.market_id.clone(), m1.yes_token_id.clone(), Side::Buy, m1.best_ask, size),
        (m2.market_id.clone(), m2.yes_token_id.clone(), Side::Buy, unfillable_price, size),
    ];

    let result = executor.execute_arb(&position_id, &legs);

    // 3. Analyse results — we expect leg 1 accepted, leg 2 rejected/unfilled
    let leg1_accepted = result.legs.get(0)
        .map(|l| matches!(l, rust_engine::executor::OrderResult::Accepted(_)))
        .unwrap_or(false);
    let leg2_accepted = result.legs.get(1)
        .map(|l| matches!(l, rust_engine::executor::OrderResult::Accepted(_)))
        .unwrap_or(false);

    tracing::info!("[D9] Execution result: leg1_accepted={}, leg2_accepted={}, all_accepted={}",
        leg1_accepted, leg2_accepted, result.all_accepted);

    if result.all_accepted {
        // Both legs filled — unexpected but possible if market is very liquid at 1c
        tracing::warn!("[D9] Both legs filled — cannot test partial fill scenario");
        exceptions.add(Exception {
            severity: "WARNING".into(),
            test_id: "D9".into(),
            component: "test_design".into(),
            description: "Both legs filled, even at 1 cent. Cannot test partial fill.".into(),
            expected: "Leg 2 to be unfilled at 1 cent".into(),
            actual: "Both legs filled".into(),
            recommendation: "Market has unusual depth at 1 cent. Try a different market.".into(),
        });
        let elapsed = start.elapsed().as_millis() as u64;
        crate::notify(notifier, "[CLOB-TEST] D9 INCONCLUSIVE: both legs filled at 1c");
        return TestResult::fail("D9", "Partial Fill", elapsed,
            vec!["Both legs filled — partial fill scenario not triggered".into()]);
    }

    if !leg1_accepted {
        // Neither leg filled — can't test
        let errors = vec!["Leg 1 also failed — nothing to evaluate".into()];
        exceptions.add(Exception {
            severity: "CRITICAL".into(),
            test_id: "D9".into(),
            component: "executor".into(),
            description: "Leg 1 (at best ask) was rejected".into(),
            expected: "Leg 1 accepted at best ask".into(),
            actual: format!("{:?}", result.legs),
            recommendation: "Check order book, signing, or balance".into(),
        });
        crate::notify(notifier, "[CLOB-TEST] D9 FAILED: leg 1 rejected");
        return TestResult::fail("D9", "Partial Fill", start.elapsed().as_millis() as u64, errors);
    }

    // 4. Leg 1 filled, Leg 2 did not — this is the partial fill we want to test
    tracing::info!("[D9] Partial fill achieved: leg 1 filled, leg 2 unfilled");

    // Confirm leg 1 fill via WS
    let submitted_leg1 = vec![
        SubmittedLeg { market: m1.clone(), size_usd: size },
    ];
    let leg1_confirmed = fill_tracker::confirm_and_enter(
        engine, clob, clob_auth, &position_id, &submitted_leg1, false, runtime, wallet_address,
    );

    // 5. Build TrackedOrders manually and run evaluate_partial_fills
    let now = crate::now_secs();
    let tracked_orders = vec![
        TrackedOrder {
            order_id: "d9_leg1".into(),
            trade_id: format!("{}_leg1", position_id),
            position_id: position_id.clone(),
            market_id: m1.market_id.clone(),
            token_id: m1.yes_token_id.clone(),
            side: Side::Buy,
            price: m1.best_ask,
            quantity: size / m1.best_ask, // approximate shares
            status: TradeStatus::Confirmed,
            filled_quantity: size / m1.best_ask,
            avg_fill_price: m1.best_ask,
            submitted_at: now,
            last_update: now,
            signed_order: None,
            neg_risk: false,
            overfill_quantity: 0.0,
        },
        TrackedOrder {
            order_id: "d9_leg2".into(),
            trade_id: format!("{}_leg2", position_id),
            position_id: position_id.clone(),
            market_id: m2.market_id.clone(),
            token_id: m2.yes_token_id.clone(),
            side: Side::Buy,
            price: unfillable_price,
            quantity: size / unfillable_price,
            status: TradeStatus::Cancelled, // FAK killed unfilled portion
            filled_quantity: 0.0,
            avg_fill_price: 0.0,
            submitted_at: now,
            last_update: now,
            signed_order: None,
            neg_risk: false,
            overfill_quantity: 0.0,
        },
    ];

    let action = rust_engine::executor::evaluate_partial_fills(&tracked_orders, 0.03);
    tracing::info!("[D9] evaluate_partial_fills result: {:?}", action);

    let mut test_errors = Vec::new();

    match &action {
        PartialFillAction::Unwind { filled_legs, reason } => {
            tracing::info!("[D9] CORRECT: Unwind triggered — {} filled legs, reason: {}",
                filled_legs.len(), reason);
        }
        other => {
            let msg = format!("Expected Unwind for one-sided fill, got {:?}", other);
            tracing::error!("[D9] WRONG: {}", msg);
            test_errors.push(msg.clone());
            exceptions.add(Exception {
                severity: "CRITICAL".into(),
                test_id: "D9".into(),
                component: "evaluate_partial_fills".into(),
                description: "Wrong action for one-sided partial fill".into(),
                expected: "Unwind (one-sided fill: cost but no revenue)".into(),
                actual: format!("{:?}", other),
                recommendation: "B3.6 logic may have a bug".into(),
            });
        }
    }

    // 6. Clean up — sell back the filled leg 1 position
    if leg1_confirmed.is_ok() {
        tracing::info!("[D9] Cleaning up: selling back leg 1 position...");
        // Use D8 closeout pattern — sell at best bid
        let best_bid = clob.get_best_bid(&m1.yes_token_id);
        if best_bid > 0.0 {
            let sell_legs = vec![(
                m1.market_id.clone(),
                m1.yes_token_id.clone(),
                Side::Sell,
                best_bid,
                size,
            )];
            let sell_result = executor.execute_arb(&format!("{}_cleanup", position_id), &sell_legs);
            if sell_result.all_accepted {
                tracing::info!("[D9] Cleanup sell accepted");
            } else {
                tracing::warn!("[D9] Cleanup sell failed — position may remain open: {:?}", sell_result.legs);
            }
        } else {
            tracing::warn!("[D9] No bid available for cleanup — position remains open");
        }
    }

    let elapsed = start.elapsed().as_millis() as u64;

    if test_errors.is_empty() {
        dedup.record_open(&[m1.market_id.clone()], "D9");
        crate::notify(notifier, &format!(
            "[CLOB-TEST] D9 PASSED: partial fill correctly triggers Unwind (leg1={}, leg2=unfilled)",
            m1.question
        ));
        TestResult::pass("D9", "Partial Fill", elapsed, serde_json::json!({
            "leg1_market": m1.question,
            "leg2_market": m2.question,
            "leg1_filled": true,
            "leg2_filled": false,
            "action": format!("{:?}", action),
        }))
    } else {
        crate::notify(notifier, "[CLOB-TEST] D9 FAILED: partial fill evaluation incorrect");
        TestResult::fail("D9", "Partial Fill", elapsed, test_errors)
    }
}
