/// D6: Test reconciliation cold-start.
///
/// This module handles the main binary's side of D6:
/// - On first run: writes checkpoint + D6 flag when 2+ positions exist
/// - On resume: verifies cold-start reconciliation detects all positions
///   1. Restore state from checkpoint (positions + accounting)
///   2. Poll Data API until indexer stabilises (two successive identical reads)
///   3. Reconcile checkpoint state vs venue state — detect discrepancies
///   4. Apply reconciliation: update internal state to match venue (source of truth)
///   5. Report discrepancies

use crate::ipc::{self, Checkpoint};
use crate::report::{TestResult, Exception, ExceptionReport};
use std::collections::HashMap;
use std::path::Path;

/// Check if we should trigger D6 (2+ positions open).
/// Returns true if D6 flag was written (main should continue running, helper will kill us).
pub fn maybe_trigger(
    workspace: &Path,
    open_position_ids: &[String],
    d2_done: bool,
    d3_done: bool,
    d4_done: bool,
    d5_done: bool,
    test_results: &[crate::report::TestResult],
    initial_usdc: f64,
    initial_pol: f64,
    engine: &rust_engine::TradingEngine,
    notifier: &rust_engine::notify::Notifier,
) -> bool {
    let engine_open = engine.pm_open_count();
    if engine_open < 1 {
        tracing::warn!("[D6] Cannot trigger: 0 engine positions (IDs={}, but no fill tracking ran)",
            open_position_ids.len());
        return false;
    }

    tracing::info!("[D6] {} engine positions open, writing checkpoint for cold-start test",
        engine_open);

    // Write PID (best-effort — not critical for D6)
    if let Err(e) = ipc::write_pid(workspace) {
        tracing::warn!("[D6] Failed to write PID (non-fatal): {}", e);
    }

    // Serialize open positions for cold-start recovery
    let open_positions_json: Vec<String> = {
        let pm = engine.positions.lock();
        pm.open_positions().values()
            .filter_map(|p| serde_json::to_string(p).ok())
            .collect()
    };
    tracing::info!("[D6] Serialized {} positions for checkpoint", open_positions_json.len());

    // Serialize accounting ledger for checkpoint persistence
    let accounting_json = engine.accounting.lock().serialize_json();
    tracing::info!("[D6] Serialized accounting ledger ({} bytes)", accounting_json.len());

    // Write checkpoint
    let checkpoint = Checkpoint {
        timestamp: chrono::Utc::now().to_rfc3339(),
        phase: "D6_READY".to_string(),
        d2_done,
        d3_done,
        d4_done,
        d5_done,
        open_position_ids: open_position_ids.to_vec(),
        test_results: test_results.to_vec(),
        initial_usdc,
        initial_pol,
        open_positions_json,
        accounting_json,
    };

    if let Err(e) = ipc::write_checkpoint(workspace, &checkpoint) {
        tracing::error!("[D6] Failed to write checkpoint: {}", e);
        return false;
    }

    // Signal helper
    if let Err(e) = ipc::signal_d6_ready(workspace) {
        tracing::error!("[D6] Failed to signal D6 ready: {}", e);
        return false;
    }

    crate::notify(notifier, "[CLOB-TEST] D6: checkpointing positions, waiting for helper restart...");
    tracing::info!("[D6] Checkpoint written, D6 flag set. Waiting for helper to restart us...");
    true
}

/// Poll Data API until the indexer stabilises (two successive identical reads).
/// Returns the final venue positions.
fn poll_data_api_until_stable(
    http: &reqwest::blocking::Client,
    clob_host: &str,
    auth: &rust_engine::signing::ClobAuth,
) -> Vec<rust_engine::reconciliation::VenuePosition> {
    use rust_engine::reconciliation::query_clob_positions;

    let poll_interval = std::time::Duration::from_secs(30);
    let max_wait = std::time::Duration::from_secs(300); // 5 min
    let start = std::time::Instant::now();

    let mut prev_shares: HashMap<String, f64> = HashMap::new();
    let mut last_venue = Vec::new();

    loop {
        match query_clob_positions(http, clob_host, Some(auth)) {
            Ok(venue) => {
                let mut current_shares: HashMap<String, f64> = HashMap::new();
                for vp in &venue {
                    *current_shares.entry(vp.market_id.clone()).or_default() += vp.size;
                }

                tracing::info!("[D6] Data API poll ({:.0}s): {} positions, shares={:?}",
                    start.elapsed().as_secs_f64(), venue.len(),
                    current_shares.iter().map(|(k, v)| format!("{}..={:.2}", &k[..k.len().min(12)], v)).collect::<Vec<_>>());

                if !prev_shares.is_empty() && current_shares == prev_shares {
                    tracing::info!("[D6] Data API stabilised after {:.0}s (two successive identical reads)",
                        start.elapsed().as_secs_f64());
                    return venue;
                }

                prev_shares = current_shares;
                last_venue = venue;
            }
            Err(e) => {
                tracing::warn!("[D6] Data API poll failed: {}", e);
            }
        }

        if start.elapsed() > max_wait {
            tracing::warn!("[D6] Data API poll timeout after {:.0}s — using last read", start.elapsed().as_secs_f64());
            return last_venue;
        }

        std::thread::sleep(poll_interval);
    }
}

/// Run D6 verification after resume from checkpoint.
///
/// 1. Restore state from checkpoint (positions + accounting)
/// 2. Poll Data API until indexer stabilises
/// 3. Reconcile checkpoint vs venue — detect discrepancies
/// 4. Apply reconciliation: update internal state to match venue
/// 5. Report
pub fn verify_cold_start(
    engine: &rust_engine::TradingEngine,
    checkpoint: &Checkpoint,
    notifier: &rust_engine::notify::Notifier,
    exceptions: &mut ExceptionReport,
    clob_host: &str,
    clob_auth: &rust_engine::signing::ClobAuth,
) -> TestResult {
    let start = std::time::Instant::now();
    crate::notify(notifier, "[CLOB-TEST] D6: verifying cold-start reconciliation...");

    // === STEP 1: Restore state from checkpoint ===

    if !checkpoint.accounting_json.is_empty() {
        match rust_engine::accounting::AccountingLedger::deserialize_json(&checkpoint.accounting_json) {
            Ok(restored) => {
                let mut acct = engine.accounting.lock();
                *acct = restored;
                tracing::info!("[D6] Accounting restored (cash=${:.2}, {} entries)",
                    acct.cash_balance(), acct.entries().len());
            }
            Err(e) => {
                tracing::warn!("[D6] Failed to restore accounting: {}", e);
            }
        }
    }

    if !checkpoint.open_positions_json.is_empty() {
        let acct_cash = engine.accounting.lock().cash_balance();
        tracing::info!("[D6] Importing {} positions (capital=${:.2})...",
            checkpoint.open_positions_json.len(), acct_cash);
        {
            let mut pm = engine.positions.lock();
            pm.import_positions_json(
                &checkpoint.open_positions_json, &[],
                acct_cash, checkpoint.initial_usdc,
            );
        }
        tracing::info!("[D6] Import complete: {} open positions", engine.pm_open_count());
    }

    let expected_count = checkpoint.open_positions_json.len();

    // === STEP 2: Poll Data API until indexer stabilises ===

    tracing::info!("[D6] Waiting for Data API indexer to stabilise...");
    let venue_positions = poll_data_api_until_stable(
        &engine.http_client, clob_host, clob_auth,
    );

    // === STEP 3: Reconcile checkpoint vs venue ===

    tracing::info!("[D6] Running startup reconciliation against stable venue data...");
    let recon = engine.reconcile_startup_with_auth(clob_host, Some(clob_auth), 0.1);

    tracing::info!("[D6] Reconciliation: passed={}, checked={}, matched={}, discrepancies={}",
        recon.passed, recon.positions_checked, recon.positions_matched, recon.discrepancies.len());
    for d in &recon.discrepancies {
        tracing::info!("[D6]   {:?} — {}", d.kind, d.description);
    }

    // === STEP 4: Apply reconciliation — update state to venue truth ===

    let adjustments = engine.apply_reconciliation(&recon, &venue_positions);
    for adj in &adjustments {
        tracing::info!("[D6] Adjustment: {}", adj);
    }

    // === STEP 5: Report ===

    let open_count = engine.pm_open_count();
    let mut errors = Vec::new();

    // Check 1: Did positions restore from checkpoint?
    if open_count == 0 && expected_count > 0 {
        errors.push(format!("No positions loaded on restart (expected {})", expected_count));
        exceptions.add(Exception {
            severity: "CRITICAL".into(),
            test_id: "D6".into(),
            component: "state_recovery".into(),
            description: "Cold-start failed to load positions from checkpoint".into(),
            expected: format!("{} positions from checkpoint", expected_count),
            actual: "0 positions".into(),
            recommendation: "Check import_positions_json and checkpoint serialization".into(),
        });
    }

    // Check 2: Did the helper change venue state during shutdown?
    // Evidence = reconciliation found any discrepancy (quantity mismatch, missing position, etc.)
    let venue_change_detected = recon.discrepancies.iter().any(|d| {
        matches!(d.kind,
            rust_engine::reconciliation::DiscrepancyKind::QuantityMismatch |
            rust_engine::reconciliation::DiscrepancyKind::PositionMissingOnVenue |
            rust_engine::reconciliation::DiscrepancyKind::PositionMissingInternal
        )
    });

    if venue_change_detected {
        tracing::info!("[D6] Venue state change detected ({} discrepancies, {} adjustments applied)",
            recon.discrepancies.len(), adjustments.len());
        crate::notify(notifier, &format!(
            "[CLOB-TEST] D6: {} discrepancies found, {} adjustments applied",
            recon.discrepancies.len(), adjustments.len()
        ));
    } else if expected_count > 0 {
        tracing::warn!("[D6] No venue state change detected (engine={}, checkpoint={}, 0 discrepancies)",
            open_count, expected_count);
        errors.push("No venue state change during shutdown — reconciliation found no discrepancies".into());
        exceptions.add(Exception {
            severity: "WARNING".into(),
            test_id: "D6".into(),
            component: "venue_change".into(),
            description: "No venue state change detected during D6 shutdown cycle".into(),
            expected: "At least 1 reconciliation discrepancy (helper should have traded)".into(),
            actual: format!("{} positions, 0 discrepancies", open_count),
            recommendation: "Verify helper FAK order filled (check WS MATCHED → CONFIRMED)".into(),
        });
    }

    // Accounting summary after reconciliation adjustments
    engine.accounting.lock().summary_log("after-D6");

    let elapsed = start.elapsed().as_millis() as u64;

    if errors.is_empty() {
        crate::notify(notifier, &format!(
            "[CLOB-TEST] D6 PASSED: cold-start reconciliation OK ({} positions, {} discrepancies)",
            open_count, recon.discrepancies.len()
        ));
        TestResult::pass("D6", "Cold-Start Reconciliation", elapsed,
            serde_json::json!({
                "checkpoint_positions": expected_count,
                "loaded_positions": open_count,
                "accounting_restored": !checkpoint.accounting_json.is_empty(),
                "venue_change_detected": venue_change_detected,
                "discrepancies": recon.discrepancies.len(),
                "adjustments": adjustments.len(),
                "reconciliation_passed": recon.passed,
            }))
    } else {
        crate::notify(notifier, &format!(
            "[CLOB-TEST EXCEPTION] D6 FAILED: {}",
            errors.join("; ")
        ));
        TestResult::fail("D6", "Cold-Start Reconciliation", elapsed, errors)
    }
}
