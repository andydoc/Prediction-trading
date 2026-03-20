/// D5: Test multi-leg arb execution.
///
/// Creates a 2-leg position by buying YES on two markets.
/// Since the test harness doesn't have WS/eval pipeline,
/// we go straight to the forced 2-market buy approach.
///
/// Also used to seed positions for D6.

use rust_engine::executor::Executor;
use rust_engine::signing::{Side, ClobAuth};
use crate::clob_client::ClobClient;
use crate::fill_tracker::{self, SubmittedLeg};
use crate::report::{TestResult, Exception, ExceptionReport};
use crate::dedup::PositionDedup;
use crate::config::MergedTestConfig;

/// Run D5: 2-leg arb execution via forced position creation.
/// Also ensures we have >= 2 open positions for D6.
pub fn run(
    executor: &Executor,
    engine: &rust_engine::TradingEngine,
    notifier: &rust_engine::notify::Notifier,
    _test_config: &MergedTestConfig,
    dedup: &mut PositionDedup,
    _need_positions_for_d6: bool,
    exceptions: &mut ExceptionReport,
    clob: &ClobClient,
    clob_auth: &ClobAuth,
    runtime: &tokio::runtime::Handle,
) -> (TestResult, Vec<String>) {
    let start = std::time::Instant::now();
    let mut opened_position_ids = Vec::new();

    tracing::info!("[D5] Creating 2-leg position via forced buy on two markets...");

    // Find two markets with active order books
    let (m1, m2) = match clob.find_two_outcome_market() {
        Some(pair) => pair,
        None => {
            let errors = vec!["Could not find two markets for 2-leg position".into()];
            exceptions.add(Exception {
                severity: "CRITICAL".into(),
                test_id: "D5".into(),
                component: "market_selection".into(),
                description: "Could not find two active markets via CLOB REST API".into(),
                expected: "Two markets with order books for forced 2-leg buy".into(),
                actual: "Not enough suitable markets found".into(),
                recommendation: "Check CLOB API and market availability".into(),
            });
            crate::notify(notifier, "[CLOB-TEST EXCEPTION] D5 FAILED: no markets for 2-leg");
            return (TestResult::fail("D5", "Multi-Leg Arb", start.elapsed().as_millis() as u64, errors),
                opened_position_ids);
        }
    };

    clob.register_instrument(&m1, engine);
    clob.register_instrument(&m2, engine);

    tracing::info!("[D5] Market 1: {} (ask={:.4}) — {}", m1.market_id, m1.best_ask, m1.question);
    tracing::info!("[D5] Market 2: {} (ask={:.4}) — {}", m2.market_id, m2.best_ask, m2.question);

    let size = 2.50;
    let position_id = format!("d5_forced_{}", crate::now_secs() as u64);

    let legs = vec![
        (m1.market_id.clone(), m1.yes_token_id.clone(), Side::Buy, m1.best_ask, size),
        (m2.market_id.clone(), m2.yes_token_id.clone(), Side::Buy, m2.best_ask, size),
    ];

    let result = executor.execute_arb(&position_id, &legs);

    if !result.all_accepted {
        // Check if at least one leg succeeded
        let accepted_count = result.legs.iter().filter(|l| matches!(l, rust_engine::executor::OrderResult::Accepted(_))).count();
        if accepted_count > 0 {
            tracing::warn!("[D5] Partial success: {}/{} legs accepted", accepted_count, legs.len());
        } else {
            let errors = vec![format!("Both legs failed: {:?}", result.legs)];
            exceptions.add(Exception {
                severity: "CRITICAL".into(),
                test_id: "D5".into(),
                component: "executor".into(),
                description: "2-leg forced buy failed on both legs".into(),
                expected: "Both legs accepted".into(),
                actual: format!("{:?}", result.legs),
                recommendation: "Check order construction, signing, and CLOB API".into(),
            });
            crate::notify(notifier, "[CLOB-TEST EXCEPTION] D5 FAILED: 2-leg execution failed");
            return (TestResult::fail("D5", "Multi-Leg Arb", start.elapsed().as_millis() as u64, errors),
                opened_position_ids);
        }
    }

    dedup.record_open(&[m1.market_id.clone(), m2.market_id.clone()], "D5");
    tracing::info!("[D5] 2-leg orders submitted: {}", position_id);

    // Confirm fills via WS User Channel and enter position in engine
    let submitted_legs = vec![
        SubmittedLeg { market: m1.clone(), size_usd: size },
        SubmittedLeg { market: m2.clone(), size_usd: size },
    ];

    match fill_tracker::confirm_and_enter(engine, clob, clob_auth, &position_id, &submitted_legs, false, runtime) {
        Ok(engine_pid) => {
            opened_position_ids.push(engine_pid.clone());
            tracing::info!("[D5] Position entered in engine: {}", engine_pid);

            let elapsed = start.elapsed().as_millis() as u64;
            crate::notify(notifier, &format!(
                "[CLOB-TEST] D5 PASSED: 2-leg buy on {} + {} — position {}",
                m1.question, m2.question, engine_pid
            ));

            (TestResult::pass("D5", "Multi-Leg Arb", elapsed,
                serde_json::json!({
                    "market_1": { "id": m1.market_id, "name": m1.question, "ask": m1.best_ask },
                    "market_2": { "id": m2.market_id, "name": m2.question, "ask": m2.best_ask },
                    "position_id": engine_pid,
                    "size_per_leg": size,
                    "open_positions": engine.pm_open_count(),
                })),
            opened_position_ids)
        }
        Err(e) => {
            // Orders were accepted but fills not confirmed — partial success
            opened_position_ids.push(position_id.clone());
            tracing::warn!("[D5] Orders accepted but fill confirmation failed: {}", e);

            let elapsed = start.elapsed().as_millis() as u64;
            let errors = vec![format!("Fill confirmation failed: {}", e)];
            exceptions.add(Exception {
                severity: "WARNING".into(),
                test_id: "D5".into(),
                component: "fill_tracker".into(),
                description: "Orders accepted but fills not confirmed within timeout".into(),
                expected: "Confirmed fills within 60s".into(),
                actual: e.clone(),
                recommendation: "Check WS user channel connectivity and order book depth".into(),
            });
            crate::notify(notifier, &format!("[CLOB-TEST] D5 PARTIAL: orders accepted, fills unconfirmed: {}", e));
            (TestResult::fail("D5", "Multi-Leg Arb", elapsed, errors), opened_position_ids)
        }
    }
}
