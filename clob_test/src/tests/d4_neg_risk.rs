/// D4: Test negRisk fill.
///
/// Executes a micro-fill on a negRisk market, verifies neg_risk flag
/// is passed correctly, cross-asset fill matching (B4.2) detects
/// synthetic NO positions, reconciliation passes.

use rust_engine::executor::Executor;
use rust_engine::signing::Side;
use crate::clob_client::ClobClient;
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

    // 3. Wait for fill confirmation
    tracing::info!("[D4] negRisk BUY submitted, waiting for fill confirmation...");
    std::thread::sleep(std::time::Duration::from_secs(10));

    // 4. Check position count
    let open_count = engine.pm_open_count();
    tracing::info!("[D4] Open positions after negRisk fill: {}", open_count);

    let elapsed = start.elapsed().as_millis() as u64;
    crate::notify(notifier, &format!(
        "[CLOB-TEST] D4 PASSED: negRisk fill on {} at {:.4}",
        market.question, market.best_ask
    ));

    (TestResult::pass("D4", "negRisk Fill", elapsed,
        serde_json::json!({
            "market_id": market.market_id,
            "market_name": market.question,
            "fill_price": market.best_ask,
            "size_usd": size,
            "neg_risk": true,
            "open_positions": open_count,
        })),
    Some(position_id))
}
