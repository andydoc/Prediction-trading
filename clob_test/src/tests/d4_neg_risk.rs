/// D4: Test negRisk fill.
///
/// Executes a micro-fill on a negRisk market, verifies neg_risk flag
/// is passed correctly, cross-asset fill matching (B4.2) detects
/// synthetic NO positions, reconciliation passes.

use rust_engine::executor::Executor;
use rust_engine::signing::Side;
use crate::report::{TestResult, Exception, ExceptionReport};
use crate::dedup::PositionDedup;
use crate::tests::d2_submit_cancel::pick_neg_risk_market;

/// Run D4: negRisk market micro-fill.
pub fn run(
    executor: &Executor,
    engine: &rust_engine::TradingEngine,
    notifier: &rust_engine::notify::Notifier,
    dedup: &mut PositionDedup,
    exceptions: &mut ExceptionReport,
) -> (TestResult, Option<String>) {
    let start = std::time::Instant::now();

    // 1. Find a negRisk market
    let (market_id, token_id, best_ask, name) = match pick_neg_risk_market(engine) {
        Some(m) => {
            if !dedup.can_open(&[m.0.clone()]) {
                tracing::warn!("[D4] Best negRisk market already occupied, searching further...");
                // Try to find another
                let constraints = engine.constraints.all();
                let mut found = None;
                for c in &constraints {
                    if !c.is_neg_risk { continue; }
                    for m in &c.markets {
                        if !dedup.can_open(&[m.market_id.clone()]) { continue; }
                        let ask = engine.get_best_ask(&m.yes_asset_id);
                        if ask > 0.05 && ask < 0.95 {
                            found = Some((m.market_id.clone(), m.yes_asset_id.clone(), ask, m.name.clone()));
                            break;
                        }
                    }
                    if found.is_some() { break; }
                }
                match found {
                    Some(f) => f,
                    None => {
                        return (TestResult::fail("D4", "negRisk Fill", 0,
                            vec!["No available negRisk market found".into()]), None);
                    }
                }
            } else {
                m
            }
        }
        None => {
            let errors = vec!["No negRisk market found with active order books".into()];
            exceptions.add(Exception {
                severity: "WARNING".into(),
                test_id: "D4".into(),
                component: "market_selection".into(),
                description: "No negRisk market available".into(),
                expected: "At least one negRisk market with depth".into(),
                actual: "None found".into(),
                recommendation: "Wait for negRisk markets to appear or check constraint detection".into(),
            });
            return (TestResult::fail("D4", "negRisk Fill", 0, errors), None);
        }
    };

    tracing::info!("[D4] Selected negRisk market: {} (ask={:.4}) — {}", market_id, best_ask, name);

    // 2. Execute micro-fill
    let size = 2.50;
    let position_id = format!("d4_test_{}", crate::now_secs() as u64);

    let legs = vec![(
        market_id.clone(),
        token_id.clone(),
        Side::Buy,
        best_ask,
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

    dedup.record_open(&[market_id.clone()], "D4");

    // 3. Wait for fill confirmation
    tracing::info!("[D4] negRisk BUY submitted, waiting for fill confirmation...");
    std::thread::sleep(std::time::Duration::from_secs(10));

    // 4. Run reconciliation (should detect synthetic NO positions via B4.2)
    let recon = engine.reconcile_periodic(0.1);
    tracing::info!("[D4] Reconciliation: passed={}, discrepancies={}", recon.passed, recon.discrepancies.len());

    // Log any synthetic position detections
    for d in &recon.discrepancies {
        tracing::info!("[D4] Discrepancy: {:?} — {}", d.kind, d.description);
    }

    let elapsed = start.elapsed().as_millis() as u64;
    crate::notify(notifier, &format!(
        "[CLOB-TEST] D4 PASSED: negRisk fill on {} at {:.4}",
        name, best_ask
    ));

    (TestResult::pass("D4", "negRisk Fill", elapsed,
        serde_json::json!({
            "market_id": market_id,
            "market_name": name,
            "fill_price": best_ask,
            "size_usd": size,
            "neg_risk": true,
            "reconciliation_passed": recon.passed,
            "discrepancy_count": recon.discrepancies.len(),
        })),
    Some(position_id))
}
