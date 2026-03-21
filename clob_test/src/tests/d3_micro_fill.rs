/// D3: Execute a real micro-fill.
///
/// Places a FAK BUY at market price, confirms fill via WS User Channel,
/// enters position in engine via fill_tracker.

use rust_engine::executor::Executor;
use rust_engine::signing::{Side, ClobAuth};
use crate::clob_client::ClobClient;
use crate::fill_tracker::{self, SubmittedLeg};
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
    clob: &ClobClient,
    clob_auth: &ClobAuth,
    runtime: &tokio::runtime::Handle,
    wallet_address: &str,
) -> (TestResult, Option<String>) {
    let start = std::time::Instant::now();

    // 1. Pick a liquid non-negRisk market via REST API
    let market = match clob.find_liquid_market() {
        Some(m) => {
            if !dedup.can_open(&[m.market_id.clone()]) {
                tracing::warn!("[D3] Best market already occupied, will use it anyway for D3");
            }
            m
        }
        None => {
            let errors = vec!["No suitable non-negRisk market found for D3".into()];
            return (TestResult::fail("D3", "Micro-Fill", 0, errors), None);
        }
    };

    clob.register_instrument(&market, engine);

    tracing::info!("[D3] Selected market: {} (ask={:.4}) — {}", market.market_id, market.best_ask, market.question);

    // 2. Execute FAK BUY at best_ask
    let size = 2.50;  // $2.50 minimum
    let position_id = format!("d3_test_{}", crate::now_secs() as u64);

    let legs = vec![(
        market.market_id.clone(),
        market.yes_token_id.clone(),
        Side::Buy,
        market.best_ask,
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
    dedup.record_open(&[market.market_id.clone()], "D3");

    // 3. Confirm fill via WS User Channel and enter position in engine
    tracing::info!("[D3] FAK BUY submitted, confirming via WS...");

    let submitted_legs = vec![
        SubmittedLeg { market: market.clone(), size_usd: size },
    ];

    match fill_tracker::confirm_and_enter(engine, clob, clob_auth, &position_id, &submitted_legs, false, runtime, wallet_address) {
        Ok((engine_pid, fills)) => {
            let fill_price = fills.first().map(|f| f.price).unwrap_or(market.best_ask);
            let fill_size = fills.first().map(|f| f.size).unwrap_or(0.0);
            let open_count = engine.pm_open_count();
            tracing::info!("[D3] Position entered: {} (fill: {} shares @ {:.4})", engine_pid, fill_size, fill_price);

            let elapsed = start.elapsed().as_millis() as u64;
            crate::notify(notifier, &format!(
                "[CLOB-TEST] D3 PASSED: micro-fill on {} at {:.4}",
                market.question, fill_price
            ));

            (TestResult::pass("D3", "Micro-Fill", elapsed,
                serde_json::json!({
                    "market_id": market.market_id,
                    "market_name": market.question,
                    "fill_price": fill_price,
                    "fill_size": fill_size,
                    "size_usd": size,
                    "open_positions": open_count,
                    "position_id": engine_pid,
                })),
            Some(engine_pid))
        }
        Err(e) => {
            tracing::warn!("[D3] Fill confirmation failed: {}", e);
            let elapsed = start.elapsed().as_millis() as u64;
            exceptions.add(Exception {
                severity: "WARNING".into(),
                test_id: "D3".into(),
                component: "fill_tracker".into(),
                description: "Order accepted but fill not confirmed via WS".into(),
                expected: "Confirmed fill within 60s".into(),
                actual: e.clone(),
                recommendation: "Check WS user channel auth and market subscription".into(),
            });
            crate::notify(notifier, &format!("[CLOB-TEST] D3 PARTIAL: order accepted, fill unconfirmed: {}", e));
            (TestResult::fail("D3", "Micro-Fill", elapsed, vec![e]), Some(position_id))
        }
    }
}
