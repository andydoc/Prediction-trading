/// D4: Test negRisk fill.
///
/// Executes a micro-fill on a negRisk market, confirms fill via WS User Channel,
/// enters position in engine via fill_tracker.

use rust_engine::executor::Executor;
use rust_engine::signing::{Side, ClobAuth};
use crate::clob_client::ClobClient;
use crate::fill_tracker::{self, SubmittedLeg};
use crate::report::{TestResult, Exception, ExceptionReport};
use crate::dedup::PositionDedup;

/// Run D4: negRisk market micro-fill.
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
) -> (TestResult, Option<String>) {
    let start = std::time::Instant::now();

    // 1. Find a negRisk market via REST API
    let market = match clob.find_neg_risk_market() {
        Some(m) => m,
        None => {
            let errors = vec!["No negRisk market found with active order books".into()];
            exceptions.add(Exception {
                severity: "WARNING".into(),
                test_id: "D4".into(),
                component: "market_selection".into(),
                description: "No negRisk market available via CLOB REST API".into(),
                expected: "At least one negRisk market with depth".into(),
                actual: "None found".into(),
                recommendation: "Check CLOB API connectivity and negRisk market availability".into(),
            });
            return (TestResult::fail("D4", "negRisk Fill", 0, errors), None);
        }
    };

    clob.register_instrument(&market, engine);

    tracing::info!("[D4] Selected negRisk market: {} (ask={:.4}) — {}", market.market_id, market.best_ask, market.question);

    // 2. Execute micro-fill
    let size = 2.50;
    let position_id = format!("d4_test_{}", crate::now_secs() as u64);

    let legs = vec![(
        market.market_id.clone(),
        market.yes_token_id.clone(),
        Side::Buy,
        market.best_ask,
        size,
    )];

    let result = executor.execute_arb(&position_id, &legs);

    if !result.all_accepted {
        let errors = vec![format!("negRisk fill failed: {:?}", result.legs)];
        exceptions.add(Exception {
            severity: "CRITICAL".into(),
            test_id: "D4".into(),
            component: "executor".into(),
            description: "negRisk FAK BUY failed".into(),
            expected: "Fill accepted with neg_risk=true".into(),
            actual: format!("{:?}", result.legs),
            recommendation: "Check negRisk signing (domain separator) and instrument flags".into(),
        });
        crate::notify(notifier, "[CLOB-TEST EXCEPTION] D4 FAILED: negRisk fill rejected");
        return (TestResult::fail("D4", "negRisk Fill", start.elapsed().as_millis() as u64, errors), None);
    }

    dedup.record_open(&[market.market_id.clone()], "D4");

    // 3. Confirm fill via WS User Channel and enter position
    tracing::info!("[D4] negRisk BUY submitted, confirming via WS...");

    let submitted_legs = vec![
        SubmittedLeg { market: market.clone(), size_usd: size },
    ];

    match fill_tracker::confirm_and_enter(engine, clob, clob_auth, &position_id, &submitted_legs, false, runtime, wallet_address) {
        Ok((engine_pid, fills)) => {
            let fill_price = fills.first().map(|f| f.price).unwrap_or(market.best_ask);
            let fill_size = fills.first().map(|f| f.size).unwrap_or(0.0);
            let open_count = engine.pm_open_count();
            tracing::info!("[D4] Position entered: {} (fill: {} shares @ {:.4})", engine_pid, fill_size, fill_price);

            let elapsed = start.elapsed().as_millis() as u64;
            crate::notify(notifier, &format!(
                "[CLOB-TEST] D4 PASSED: negRisk fill on {} at {:.4}",
                market.question, fill_price
            ));

            (TestResult::pass("D4", "negRisk Fill", elapsed,
                serde_json::json!({
                    "market_id": market.market_id,
                    "market_name": market.question,
                    "fill_price": fill_price,
                    "fill_size": fill_size,
                    "size_usd": size,
                    "neg_risk": true,
                    "open_positions": open_count,
                    "position_id": engine_pid,
                })),
            Some(engine_pid))
        }
        Err(e) => {
            tracing::warn!("[D4] Fill confirmation failed: {}", e);
            let elapsed = start.elapsed().as_millis() as u64;
            exceptions.add(Exception {
                severity: "WARNING".into(),
                test_id: "D4".into(),
                component: "fill_tracker".into(),
                description: "negRisk order accepted but fill not confirmed via WS".into(),
                expected: "Confirmed fill within 60s".into(),
                actual: e.clone(),
                recommendation: "Check WS user channel auth and negRisk market subscription".into(),
            });
            crate::notify(notifier, &format!("[CLOB-TEST] D4 PARTIAL: order accepted, fill unconfirmed: {}", e));
            (TestResult::fail("D4", "negRisk Fill", elapsed, vec![e]), Some(position_id))
        }
    }
}
