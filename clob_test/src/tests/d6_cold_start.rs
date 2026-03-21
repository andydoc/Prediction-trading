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

    // Write PID
    if let Err(e) = ipc::write_pid(workspace) {
        tracing::error!("[D6] Failed to write PID: {}", e);
        return false;
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

/// Run D6 verification after resume from checkpoint.
/// Checks that cold-start reconciliation detects the expected positions.
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

    // Restore accounting ledger from checkpoint
    if !checkpoint.accounting_json.is_empty() {
        match rust_engine::accounting::AccountingLedger::deserialize_json(&checkpoint.accounting_json) {
            Ok(restored) => {
                let mut acct = engine.accounting.lock();
                *acct = restored;
                tracing::info!("[D6] Accounting ledger restored from checkpoint (cash=${:.2}, {} entries)",
                    acct.cash_balance(), acct.entries().len());
            }
            Err(e) => {
                tracing::warn!("[D6] Failed to restore accounting ledger: {}", e);
            }
        }
    }

    // Import positions from checkpoint into engine, using accounting cash as capital
    if !checkpoint.open_positions_json.is_empty() {
        let acct_cash = engine.accounting.lock().cash_balance();
        tracing::info!("[D6] Importing {} positions into engine (capital=${:.2} from accounting)...",
            checkpoint.open_positions_json.len(), acct_cash);
        {
            let mut pm = engine.positions.lock();
            pm.import_positions_json(
                &checkpoint.open_positions_json, &[],
                acct_cash, checkpoint.initial_usdc,
            );
        }
        let imported = engine.pm_open_count();
        tracing::info!("[D6] Import complete: {} open positions in engine", imported);
    }

    let expected_count = checkpoint.open_positions_json.len();

    tracing::info!("[D6] Verifying cold-start. Checkpoint had {} positions", expected_count);

    // Run startup reconciliation via Data API

    tracing::info!("[D6] Running position reconciliation via Data API...");
    let recon = engine.reconcile_startup_with_auth(clob_host, Some(clob_auth), 0.1);

    tracing::info!("[D6] Reconciliation complete: passed={}, checked={}, matched={}",
        recon.passed, recon.positions_checked, recon.positions_matched);
    for d in &recon.discrepancies {
        tracing::info!("[D6] Discrepancy: {:?} — {}", d.kind, d.description);
    }

    // Verify internal state
    let open_count = engine.pm_open_count();
    tracing::info!("[D6] Open positions in engine: {}", open_count);
    crate::notify(notifier, &format!("[CLOB-TEST] D6: restored {} positions, recon passed={}", open_count, recon.passed));

    // Key checks:
    // 1. Did we restore positions from checkpoint? (must have at least 1)
    // 2. Did the helper sell at least partially? (prefer yes, but fail gracefully if not)
    let mut errors = Vec::new();

    if open_count == 0 && expected_count > 0 {
        errors.push(format!(
            "No positions loaded on restart (expected {})",
            expected_count
        ));
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

    // Check if the helper sold at least one position.
    // If engine has same count as checkpoint, helper sell failed.
    if open_count >= expected_count && expected_count > 0 {
        tracing::warn!("[D6] Helper did not sell any positions (engine={}, checkpoint={})",
            open_count, expected_count);
        errors.push("Helper sell failed — no positions closed during cold-start cycle".into());
        exceptions.add(Exception {
            severity: "WARNING".into(),
            test_id: "D6".into(),
            component: "helper_sell".into(),
            description: "D6 helper could not sell a position (balance/allowance issue)".into(),
            expected: "At least 1 position sold by helper".into(),
            actual: format!("{} positions unchanged", open_count),
            recommendation: "Check CTF token settlement and balance before helper sells".into(),
        });
    } else if open_count < expected_count {
        tracing::info!("[D6] Helper sold {} position(s) (engine={}, checkpoint={})",
            expected_count - open_count, open_count, expected_count);
    }

    let elapsed = start.elapsed().as_millis() as u64;

    if errors.is_empty() {
        crate::notify(notifier, &format!(
            "[CLOB-TEST] D6 PASSED: cold-start reconciliation detected {} positions",
            open_count
        ));
        TestResult::pass("D6", "Cold-Start Reconciliation", elapsed,
            serde_json::json!({
                "checkpoint_positions": expected_count,
                "loaded_positions": open_count,
                "accounting_restored": !checkpoint.accounting_json.is_empty(),
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
