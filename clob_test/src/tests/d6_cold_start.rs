/// D6: Test reconciliation cold-start.
///
/// This module handles the main binary's side of D6:
/// - On first run: writes checkpoint + D6 flag when 2+ positions exist
/// - On resume: verifies cold-start reconciliation detects all positions

use crate::ipc::{self, Checkpoint};
use crate::report::{TestResult, Exception, ExceptionReport};
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
) -> bool {
    if open_position_ids.len() < 2 {
        return false;
    }

    tracing::info!("[D6] 2+ positions open ({}), writing checkpoint for cold-start test",
        open_position_ids.len());

    // Write PID
    if let Err(e) = ipc::write_pid(workspace) {
        tracing::error!("[D6] Failed to write PID: {}", e);
        return false;
    }

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

    tracing::info!("[D6] Checkpoint written, D6 flag set. Waiting for helper to restart us...");
    true
}

/// Run D6 verification after resume from checkpoint.
/// Checks that cold-start reconciliation detects the expected positions.
pub fn verify_cold_start(
    engine: &rust_engine::TradingEngine,
    checkpoint: &Checkpoint,
    notifier: &rust_engine::notify::Notifier,
    exceptions: &mut ExceptionReport,
) -> TestResult {
    let start = std::time::Instant::now();

    // The helper closed one position before restarting us.
    // We should detect N-1 positions via reconciliation.
    let expected_count = checkpoint.open_position_ids.len().saturating_sub(1);

    tracing::info!("[D6] Verifying cold-start reconciliation. Expected positions: {} (was {}, helper closed 1)",
        expected_count, checkpoint.open_position_ids.len());

    // Run startup reconciliation
    let recon = engine.reconcile_startup(0.1);

    tracing::info!("[D6] Reconciliation result: passed={}, checked={}, matched={}",
        recon.passed, recon.positions_checked, recon.positions_matched);

    for d in &recon.discrepancies {
        tracing::info!("[D6] Discrepancy: {:?} — {}", d.kind, d.description);
    }

    // Verify internal state
    let open_count = engine.pm_open_count();
    tracing::info!("[D6] Open positions in engine: {}", open_count);

    // The key check: did we detect the positions that survived?
    // Since the helper sold one position via CLOB, the engine should have loaded
    // N-1 positions from its state DB, and reconciliation should confirm them.
    let mut errors = Vec::new();

    if open_count == 0 && expected_count > 0 {
        errors.push(format!(
            "No positions loaded on restart (expected ~{})",
            expected_count
        ));
        exceptions.add(Exception {
            severity: "CRITICAL".into(),
            test_id: "D6".into(),
            component: "state_recovery".into(),
            description: "Cold-start failed to load positions from state DB".into(),
            expected: format!("{} positions", expected_count),
            actual: "0 positions".into(),
            recommendation: "Check state.load_state() and SQLite DB integrity".into(),
        });
    }

    if !recon.passed {
        errors.push(format!("Reconciliation failed with {} discrepancies", recon.discrepancies.len()));
    }

    let elapsed = start.elapsed().as_millis() as u64;

    if errors.is_empty() {
        crate::notify(notifier, &format!(
            "[CLOB-TEST] D6 PASSED: cold-start reconciliation detected {} positions",
            open_count
        ));
        TestResult::pass("D6", "Cold-Start Reconciliation", elapsed,
            serde_json::json!({
                "expected_positions": expected_count,
                "loaded_positions": open_count,
                "reconciliation_passed": recon.passed,
                "discrepancy_count": recon.discrepancies.len(),
            }))
    } else {
        crate::notify(notifier, &format!(
            "[CLOB-TEST EXCEPTION] D6 FAILED: {}",
            errors.join("; ")
        ));
        TestResult::fail("D6", "Cold-Start Reconciliation", elapsed, errors)
    }
}
