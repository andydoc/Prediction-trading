/// State-machine driven orchestration for Milestone D tests.
///
/// Unlike the production orchestrator (perpetual event loop), this runs
/// a progression through D1 → D2-D5 → D6 → D7 → D8 → Complete.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use rust_engine::TradingEngine;
use rust_engine::executor::Executor;
use rust_engine::notify::Notifier;
use rust_engine::signing::ClobAuth;

use crate::clob_client::ClobClient;
use crate::config::MergedTestConfig;
use crate::dedup::PositionDedup;
use crate::ipc;
use crate::report::{TestReport, TestResult, ExceptionReport};
use crate::tests;

/// Test harness state machine phases.
#[derive(Debug, Clone)]
enum Phase {
    /// Waiting for deposit confirmation.
    D1,
    /// Running D2-D5 in parallel. D6 triggers when 2+ positions exist.
    D2D5 {
        d2_done: bool,
        d3_done: bool,
        d4_done: bool,
        d5_done: bool,
        d6_triggered: bool,
    },
    /// Waiting for helper to restart us (D6).
    D6Waiting,
    /// Resumed after D6 restart — verify cold-start reconciliation.
    D6Verify,
    /// Circuit breaker + kill switch tests.
    D7,
    /// Closeout: sell all positions, verify accounting.
    D8,
    /// All tests complete.
    Complete,
    /// Fatal error — ceased trading.
    Failed(String),
}

pub struct TestHarness {
    phase: Phase,
    engine: TradingEngine,
    executor: Executor,
    notifier: Arc<Notifier>,
    test_config: MergedTestConfig,
    workspace: PathBuf,
    report: TestReport,
    exceptions: ExceptionReport,
    dedup: PositionDedup,
    start_time: Instant,
    timeout_minutes: u64,
    skip_deposit_check: bool,
    wallet_address: String,
    clob_host: String,
    /// Position IDs from D2-D5 (for D6 tracking).
    open_position_ids: Vec<String>,
    /// Checkpoint for D6 resume.
    checkpoint: Option<ipc::Checkpoint>,
    /// CLOB REST client for market discovery.
    clob: ClobClient,
    /// CLOB L2 auth (HMAC-signed headers for authenticated endpoints).
    clob_auth: ClobAuth,
    /// Test IDs to skip (e.g., ["D2", "D3", "D4", "D7"]).
    skip_tests: Vec<String>,
    /// Tokio runtime handle for async WS tasks.
    runtime: tokio::runtime::Handle,
}

impl TestHarness {
    pub fn new(
        engine: TradingEngine,
        executor: Executor,
        notifier: Arc<Notifier>,
        test_config: MergedTestConfig,
        workspace: PathBuf,
        wallet_address: String,
        clob_host: String,
        timeout_minutes: u64,
        skip_deposit_check: bool,
        clob_auth: ClobAuth,
        skip_tests: Vec<String>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let report = TestReport::new(&wallet_address, test_config.initial_capital, 0.0);
        let clob = ClobClient::new(&clob_host);
        Self {
            phase: Phase::D1,
            engine,
            executor,
            notifier,
            test_config,
            workspace,
            report,
            exceptions: ExceptionReport::new(),
            dedup: PositionDedup::new(),
            start_time: Instant::now(),
            timeout_minutes,
            skip_deposit_check,
            wallet_address,
            clob_host,
            open_position_ids: Vec::new(),
            checkpoint: None,
            clob,
            clob_auth,
            skip_tests,
            runtime,
        }
    }

    /// Resume from a D6 checkpoint.
    pub fn resume_from_checkpoint(
        mut self,
        checkpoint: ipc::Checkpoint,
    ) -> Self {
        self.phase = Phase::D6Verify;
        // Restore previous test results
        for tr in &checkpoint.test_results {
            self.report.add_result(tr.clone());
        }
        self.open_position_ids = checkpoint.open_position_ids.clone();
        self.checkpoint = Some(checkpoint);
        self
    }

    /// Check if a test should be skipped.
    fn should_skip(&self, test_id: &str) -> bool {
        self.skip_tests.iter().any(|s| s.eq_ignore_ascii_case(test_id))
    }

    /// Auto-pass a skipped test.
    fn skip_test(&mut self, id: &str, name: &str) {
        tracing::info!("[{}] SKIPPED via --skip-tests", id);
        self.report.add_result(TestResult::pass(id, name, 0,
            serde_json::json!({ "skipped": true, "reason": "Skipped via --skip-tests" })
        ));
    }

    /// Run the full test sequence. Returns when all tests complete or a fatal error occurs.
    pub fn run(&mut self) {
        tracing::info!("=== CLOB-TEST HARNESS STARTED ===");
        crate::notify(&self.notifier, &format!(
            "[CLOB-TEST] Started. Wallet={}, capital=${:.0}, timeout={}h",
            self.wallet_address, self.test_config.initial_capital,
            self.timeout_minutes as f64 / 60.0
        ));

        // Write PID
        if let Err(e) = ipc::write_pid(&self.workspace) {
            tracing::warn!("Failed to write PID: {}", e);
        }

        loop {
            // Check timeout
            if self.start_time.elapsed().as_secs() > self.timeout_minutes * 60 {
                tracing::error!("Test harness timeout after {} minutes", self.timeout_minutes);
                self.phase = Phase::Failed("Timeout".into());
            }

            match self.phase.clone() {
                Phase::D1 => self.run_d1(),
                Phase::D2D5 { .. } => self.run_d2d5(),
                Phase::D6Waiting => {
                    // We're waiting for the helper to kill us — just sleep
                    tracing::info!("[D6] Waiting for helper to restart us...");
                    std::thread::sleep(std::time::Duration::from_secs(5));
                }
                Phase::D6Verify => self.run_d6_verify(),
                Phase::D7 => self.run_d7(),
                Phase::D8 => self.run_d8(),
                Phase::Complete => {
                    self.finalize(true);
                    break;
                }
                Phase::Failed(reason) => {
                    tracing::error!("=== CLOB-TEST FAILED: {} ===", reason);
                    self.finalize(false);
                    break;
                }
            }
        }

        // Cleanup
        ipc::cleanup(&self.workspace);
    }

    fn run_d1(&mut self) {
        let result = tests::d1_deposit::run(
            &self.wallet_address,
            &self.clob_host,
            self.engine.current_capital(),
            &self.engine.http_client,
            &self.notifier,
            self.skip_deposit_check,
            &mut self.exceptions,
        );

        if result.result == "FAIL" {
            self.report.add_result(result);
            self.phase = Phase::Failed("D1 deposit check failed".into());
            return;
        }

        self.report.add_result(result);
        self.phase = Phase::D2D5 {
            d2_done: false,
            d3_done: false,
            d4_done: false,
            d5_done: false,
            d6_triggered: false,
        };
    }

    fn run_d2d5(&mut self) {
        // Get current state
        let (d2_done, d3_done, d4_done, d5_done, d6_triggered) = match &self.phase {
            Phase::D2D5 { d2_done, d3_done, d4_done, d5_done, d6_triggered } =>
                (*d2_done, *d3_done, *d4_done, *d5_done, *d6_triggered),
            _ => unreachable!(),
        };

        // Run D2 if not done
        if !d2_done {
            if self.should_skip("D2") {
                self.skip_test("D2", "Submit/Cancel");
            } else {
                let result = tests::d2_submit_cancel::run(
                    &self.executor, &self.engine, &self.notifier, &mut self.exceptions, &self.clob,
                );
                let passed = result.result == "PASS";
                self.report.add_result(result);
                if !passed {
                    self.phase = Phase::Failed("D2 submit/cancel test failed".into());
                    return;
                }
            }
            self.phase = Phase::D2D5 { d2_done: true, d3_done, d4_done, d5_done, d6_triggered };
        }

        // Get updated state
        let (d2_done, d3_done, d4_done, d5_done, d6_triggered) = match &self.phase {
            Phase::D2D5 { d2_done, d3_done, d4_done, d5_done, d6_triggered } =>
                (*d2_done, *d3_done, *d4_done, *d5_done, *d6_triggered),
            _ => return,
        };

        // Run D3 if not done
        if !d3_done {
            if self.should_skip("D3") {
                self.skip_test("D3", "Micro-Fill");
            } else {
                let (result, pid) = tests::d3_micro_fill::run(
                    &self.executor, &self.engine, &self.notifier,
                    &mut self.dedup, &mut self.exceptions, &self.clob,
                );
                let passed = result.result == "PASS";
                self.report.add_result(result);
                if let Some(pid) = pid {
                    self.open_position_ids.push(pid);
                }
                if !passed {
                    tracing::warn!("[D3] Non-fatal failure, continuing...");
                }
            }
            self.phase = Phase::D2D5 { d2_done, d3_done: true, d4_done, d5_done, d6_triggered };
        }

        let (d2_done, d3_done, d4_done, d5_done, d6_triggered) = match &self.phase {
            Phase::D2D5 { d2_done, d3_done, d4_done, d5_done, d6_triggered } =>
                (*d2_done, *d3_done, *d4_done, *d5_done, *d6_triggered),
            _ => return,
        };

        // Run D4 if not done
        if !d4_done {
            if self.should_skip("D4") {
                self.skip_test("D4", "NegRisk Fill");
            } else {
                let (result, pid) = tests::d4_neg_risk::run(
                    &self.executor, &self.engine, &self.notifier,
                    &mut self.dedup, &mut self.exceptions, &self.clob,
                );
                self.report.add_result(result);
                if let Some(pid) = pid {
                    self.open_position_ids.push(pid);
                }
            }
            self.phase = Phase::D2D5 { d2_done, d3_done, d4_done: true, d5_done, d6_triggered };
        }

        let (d2_done, d3_done, d4_done, d5_done, d6_triggered) = match &self.phase {
            Phase::D2D5 { d2_done, d3_done, d4_done, d5_done, d6_triggered } =>
                (*d2_done, *d3_done, *d4_done, *d5_done, *d6_triggered),
            _ => return,
        };

        // Check if we need to trigger D6 before D5
        if !d6_triggered && self.open_position_ids.len() >= 2 {
            let triggered = tests::d6_cold_start::maybe_trigger(
                &self.workspace,
                &self.open_position_ids,
                d2_done, d3_done, d4_done, d5_done,
                &self.report.tests,
                self.test_config.initial_capital,
                0.0,
                &self.engine,
            );
            if triggered {
                self.phase = Phase::D6Waiting;
                return;
            }
        }

        // Run D5 if not done (also seeds positions for D6 if needed)
        if !d5_done {
            if self.should_skip("D5") {
                self.skip_test("D5", "Multi-Leg Arb");
            } else {
                let need_d6 = self.open_position_ids.len() < 2;
                let (result, pids) = tests::d5_multi_leg::run(
                    &self.executor, &self.engine, &self.notifier,
                    &self.test_config, &mut self.dedup, need_d6, &mut self.exceptions, &self.clob,
                    &self.clob_auth, &self.runtime,
                );
                self.report.add_result(result);
                self.open_position_ids.extend(pids);
            }
            self.phase = Phase::D2D5 { d2_done, d3_done, d4_done, d5_done: true, d6_triggered };
        }

        // After all D2-D5 done, trigger D6 if not already triggered
        let (_, _, _, _, d6_triggered) = match &self.phase {
            Phase::D2D5 { d2_done, d3_done, d4_done, d5_done, d6_triggered } =>
                (*d2_done, *d3_done, *d4_done, *d5_done, *d6_triggered),
            _ => return,
        };

        if !d6_triggered && self.open_position_ids.len() >= 2 {
            let triggered = tests::d6_cold_start::maybe_trigger(
                &self.workspace,
                &self.open_position_ids,
                true, true, true, true,
                &self.report.tests,
                self.test_config.initial_capital,
                0.0,
                &self.engine,
            );
            if triggered {
                self.phase = Phase::D6Waiting;
                return;
            }
        }

        // If we can't trigger D6 (not enough positions), skip it
        if !d6_triggered && self.open_position_ids.len() < 2 {
            tracing::warn!("[D6] Cannot trigger: only {} positions open. Skipping D6.",
                self.open_position_ids.len());
            self.report.add_result(TestResult::fail(
                "D6", "Cold-Start Reconciliation", 0,
                vec!["Insufficient positions to test cold-start".into()],
            ));
            self.phase = Phase::D7;
        }
    }

    fn run_d6_verify(&mut self) {
        let checkpoint = match &self.checkpoint {
            Some(c) => c.clone(),
            None => {
                tracing::error!("[D6] No checkpoint found for verification");
                self.phase = Phase::Failed("D6 resume without checkpoint".into());
                return;
            }
        };

        let result = tests::d6_cold_start::verify_cold_start(
            &self.engine, &checkpoint, &self.notifier, &mut self.exceptions,
            &self.clob_host, &self.clob_auth,
        );

        if result.result == "FAIL" {
            self.report.add_result(result);
            // Don't fail the whole suite — D6 failure is informative
            tracing::warn!("[D6] Cold-start test failed, continuing to D7...");
        } else {
            self.report.add_result(result);
        }

        self.phase = Phase::D7;
    }

    fn run_d7(&mut self) {
        if self.should_skip("D7") || self.should_skip("D7a") {
            self.skip_test("D7a", "Circuit Breaker");
        } else {
            let cb_result = tests::d7_circuit_breaker::run_circuit_breaker(
                &self.engine, &self.notifier, &mut self.exceptions,
            );
            self.report.add_result(cb_result);
        }

        if self.should_skip("D7") || self.should_skip("D7b") {
            self.skip_test("D7b", "Kill Switch");
        } else {
            let ks_result = tests::d7_circuit_breaker::run_kill_switch(
                &self.executor, &self.engine, &self.notifier, &mut self.exceptions,
            );
            self.report.add_result(ks_result);
        }

        self.phase = Phase::D8;
    }

    fn run_d8(&mut self) {
        if self.should_skip("D8") {
            self.skip_test("D8", "Closeout");
        } else {
            let result = tests::d8_closeout::run(
                &self.executor, &self.engine, &self.notifier,
                self.test_config.initial_capital, &mut self.exceptions,
                &self.clob,
            );
            self.report.add_result(result);
        }
        self.phase = Phase::Complete;
    }

    fn finalize(&mut self, success: bool) {
        let duration = self.start_time.elapsed().as_secs_f64();
        let final_capital = self.engine.current_capital();

        self.report.finalize(duration, final_capital, 0.0);

        // Write reports
        if let Err(e) = self.report.write(&self.workspace) {
            tracing::error!("Failed to write test report: {}", e);
        }
        if let Err(e) = self.exceptions.write(&self.workspace) {
            tracing::error!("Failed to write exception report: {}", e);
        }

        // Send summary
        let test_summary: String = self.report.tests.iter()
            .map(|t| format!("  {} {}: {}", t.id, t.name, t.result))
            .collect::<Vec<_>>()
            .join("\n");

        let msg = if success {
            format!(
                "[CLOB-TEST] ALL TESTS COMPLETE in {:.0}s. Result: {}\nFinal capital: ${:.2}\n{}",
                duration, self.report.overall, final_capital, test_summary
            )
        } else {
            format!(
                "[CLOB-TEST] TERMINATED after {:.0}s. Result: {}\nFinal capital: ${:.2}\n{}\nExceptions: {}",
                duration, self.report.overall, final_capital, test_summary,
                self.exceptions.exceptions.len()
            )
        };
        crate::notify(&self.notifier, &msg);

        tracing::info!("=== CLOB-TEST {} ({:.0}s) ===", self.report.overall, duration);
    }
}
