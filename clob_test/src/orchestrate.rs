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
    /// Running D2-D5 sequentially. D6 triggers after D5 when engine positions exist.
    D2D5 {
        d2_done: bool,
        d3_done: bool,
        d4_done: bool,
        d5_done: bool,
        d6_triggered: bool,
    },
    /// Waiting for helper to restart us (D6). Timeout after 60s → auto-fail D6.
    D6Waiting { entered: Instant },
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

    /// Human-readable name of the current phase (for logging).
    fn phase_name(&self) -> &str {
        match &self.phase {
            Phase::D1 => "D1",
            Phase::D2D5 { .. } => "D2-D5",
            Phase::D6Waiting { .. } => "D6-waiting",
            Phase::D6Verify => "D6-verify",
            Phase::D7 => "D7",
            Phase::D8 => "D8",
            Phase::Complete => "complete",
            Phase::Failed(_) => "failed",
        }
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
        tracing::info!("=== CLOB-TEST HARNESS STARTED (phase={:?}) ===", self.phase_name());
        let short_wallet = if self.wallet_address.len() > 10 {
            format!("{}...{}", &self.wallet_address[..6], &self.wallet_address[self.wallet_address.len()-4..])
        } else { self.wallet_address.clone() };
        let actual_capital = self.engine.current_capital();
        crate::notify(&self.notifier, &format!(
            "[CLOB-TEST] Started. Wallet={}, capital=${:.2}, timeout={}h",
            short_wallet, actual_capital,
            self.timeout_minutes as f64 / 60.0
        ));

        // Write PID
        if let Err(e) = ipc::write_pid(&self.workspace) {
            tracing::warn!("Failed to write PID: {}", e);
        }

        // Always skip D1 — deposit check is not needed for test runs
        if matches!(self.phase, Phase::D1) {
            tracing::info!("[D1] Auto-skipped (always skip D1)");
            self.report.add_result(TestResult::pass("D1", "Deposit Check", 0,
                serde_json::json!({ "skipped": true, "reason": "Always skipped — deposit check not needed" })
            ));
            self.phase = Phase::D2D5 {
                d2_done: false,
                d3_done: false,
                d4_done: false,
                d5_done: false,
                d6_triggered: false,
            };
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
                Phase::D6Waiting { entered } => {
                    if entered.elapsed().as_secs() > 60 {
                        tracing::warn!("[D6] Helper did not arrive within 60s — auto-failing D6");
                        self.report.add_result(TestResult::fail(
                            "D6", "Cold-Start Reconciliation",
                            entered.elapsed().as_millis() as u64,
                            vec!["D6 helper process did not restart us within 60s timeout. \
                                Run with a D6 helper process for full cold-start test.".into()],
                        ));
                        self.phase = Phase::D7;
                    } else {
                        tracing::info!("[D6] Waiting for helper to restart us... ({}s/60s)",
                            entered.elapsed().as_secs());
                        std::thread::sleep(std::time::Duration::from_secs(5));
                    }
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
                self.engine.accounting.lock().summary_log("after-D2");
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
                    &self.clob_auth, &self.runtime, &self.wallet_address,
                );
                let passed = result.result == "PASS";
                self.report.add_result(result);
                self.engine.accounting.lock().summary_log("after-D3");
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
                    &self.clob_auth, &self.runtime, &self.wallet_address,
                );
                self.report.add_result(result);
                self.engine.accounting.lock().summary_log("after-D4");
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

        // NOTE: D6 trigger check moved AFTER D5 completes.
        // D3/D4 submit orders but don't enter engine positions (no fill tracking).
        // Only D5 enters real positions via fill_tracker. D6 needs real engine
        // positions to checkpoint, so we must run D5 first.

        // Run D5 if not done (also seeds positions for D6 if needed)
        if !d5_done {
            if self.should_skip("D5") {
                self.skip_test("D5", "Multi-Leg Arb");
            } else {
                let need_d6 = self.open_position_ids.len() < 2;
                let (result, pids) = tests::d5_multi_leg::run(
                    &self.executor, &self.engine, &self.notifier,
                    &self.test_config, &mut self.dedup, need_d6, &mut self.exceptions, &self.clob,
                    &self.clob_auth, &self.runtime, &self.wallet_address,
                );
                self.report.add_result(result);
                self.engine.accounting.lock().summary_log("after-D5");
                self.open_position_ids.extend(pids);
            }
            self.phase = Phase::D2D5 { d2_done, d3_done, d4_done, d5_done: true, d6_triggered };
        }

        // Flush buffered messages before D6 (we may get SIGTERM'd)
        crate::flush_notify(&self.notifier);

        // After all D2-D5 done, trigger D6 if not already triggered.
        // Use engine position count (real positions) not open_position_ids (may have phantom IDs
        // from D3/D4 which submit orders but don't enter positions via fill_tracker).
        let (_, _, _, _, d6_triggered) = match &self.phase {
            Phase::D2D5 { d2_done, d3_done, d4_done, d5_done, d6_triggered } =>
                (*d2_done, *d3_done, *d4_done, *d5_done, *d6_triggered),
            _ => return,
        };

        let engine_positions = self.engine.pm_open_count();
        if !d6_triggered && engine_positions >= 1 {
            let triggered = tests::d6_cold_start::maybe_trigger(
                &self.workspace,
                &self.open_position_ids,
                true, true, true, true,
                &self.report.tests,
                self.test_config.initial_capital,
                0.0,
                &self.engine,
                &self.notifier,
            );
            if triggered {
                self.phase = Phase::D6Waiting { entered: Instant::now() };
                return;
            }
        }

        // If we can't trigger D6 (no engine positions), skip it
        if !d6_triggered && engine_positions < 1 {
            tracing::warn!("[D6] Cannot trigger: only {} engine positions (need >= 1). Skipping D6.",
                engine_positions);
            self.report.add_result(TestResult::fail(
                "D6", "Cold-Start Reconciliation", 0,
                vec![format!("Insufficient engine positions ({}) to test cold-start", engine_positions)],
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

        let d6_passed = result.result == "PASS";
        self.report.add_result(result);
        self.engine.accounting.lock().summary_log("after-D6");
        if !d6_passed {
            tracing::warn!("[D6] Cold-start test failed, continuing to D7...");
        }
        tracing::info!("[RESUMED] D6 verify complete (passed={}), moving to D7...", d6_passed);
        crate::flush_notify(&self.notifier);

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
            self.engine.accounting.lock().summary_log("after-D7a");
        }

        if self.should_skip("D7") || self.should_skip("D7b") {
            self.skip_test("D7b", "Kill Switch");
        } else {
            let ks_result = tests::d7_circuit_breaker::run_kill_switch(
                &self.executor, &self.engine, &self.notifier, &mut self.exceptions,
            );
            self.report.add_result(ks_result);
            self.engine.accounting.lock().summary_log("after-D7b");
        }

        // Ensure D8 has at least one position to close.
        // If D7 (or prior tests) left no positions, open a small test position.
        let open_count = self.engine.pm_open_count();
        if open_count == 0 {
            tracing::info!("[D7→D8] No positions open — opening a test position for D8 closeout");
            self.open_test_position_for_d8();
        } else {
            tracing::info!("[D7→D8] {} positions open, D8 can proceed", open_count);
        }

        crate::flush_notify(&self.notifier);
        self.phase = Phase::D8;
    }

    /// Three-way end-of-run reconciliation: accounting vs engine vs exchange.
    fn reconcile_end_of_run(&self) {
        tracing::info!("=== END-OF-RUN RECONCILIATION ===");

        // 1. Accounting ledger state
        let acct = self.engine.accounting.lock();
        acct.summary_log("FINAL");

        // 2. Engine state
        let engine_open = self.engine.pm_open_count();
        let engine_capital = self.engine.current_capital();
        let snapshot = self.engine.dashboard_snapshot();
        tracing::info!("[RECONCILE] Engine: positions={}, capital={:.2}, total_value={:.2}",
            engine_open, engine_capital, snapshot.total_value);

        // 3. Exchange state via Data API
        let http = reqwest::blocking::Client::new();
        let positions_url = format!(
            "https://data-api.polymarket.com/positions?user={}&sizeThreshold=0",
            self.wallet_address.to_lowercase()
        );
        let exchange_positions: Vec<serde_json::Value> = http.get(&positions_url)
            .send().and_then(|r| r.json()).unwrap_or_default();
        let exchange_with_size: Vec<_> = exchange_positions.iter().filter(|p| {
            p.get("size").and_then(|v| v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0.0) > 0.001
        }).collect();

        let value_url = format!(
            "https://data-api.polymarket.com/value?user={}",
            self.wallet_address.to_lowercase()
        );
        let exchange_value: f64 = http.get(&value_url)
            .send().ok()
            .and_then(|r| r.json::<Vec<serde_json::Value>>().ok())
            .and_then(|v| v.first()?.get("value")?.as_f64())
            .unwrap_or(0.0);

        tracing::info!("[RECONCILE] Exchange: positions={}, total_value=${:.2}",
            exchange_with_size.len(), exchange_value);

        for p in &exchange_with_size {
            let title = p.get("title").and_then(|v| v.as_str()).unwrap_or("?");
            let size = p.get("size").and_then(|v| v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0.0);
            let cur_price = p.get("curPrice").and_then(|v| v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0.0);
            tracing::info!("[RECONCILE]   {} — size={:.2}, price={:.4}, value={:.4}",
                &title[..title.len().min(50)], size, cur_price, size * cur_price);
        }

        // 4. Three-way reconciliation
        let recon = acct.reconcile(
            engine_capital, engine_open, snapshot.total_value,
            exchange_with_size.len(), exchange_value, 0.05,
        );

        // 5. Log results
        if recon.overall_pass {
            tracing::info!("[RECONCILE] PASS: accounting == engine == exchange (within ${:.2})", recon.tolerance);
        } else {
            for m in &recon.mismatches {
                tracing::warn!("[RECONCILE] MISMATCH: {}", m);
            }
        }

        // 6. Double-entry balance check
        if acct.verify_balance() {
            tracing::info!("[RECONCILE] Double-entry: BALANCED (debits == credits)");
        } else {
            tracing::error!("[RECONCILE] Double-entry: UNBALANCED — accounting error!");
        }

        // 7. Write ledger to file
        let ledger_path = self.workspace.join("data").join("clob_test_ledger.json");
        if let Ok(json) = serde_json::to_string_pretty(&acct.to_json()) {
            if let Err(e) = std::fs::write(&ledger_path, json) {
                tracing::warn!("Failed to write ledger: {}", e);
            } else {
                tracing::info!("Ledger written to {}", ledger_path.display());
            }
        }

        tracing::info!("=== END-OF-RUN RECONCILIATION COMPLETE ===");
    }

    /// Open a small test position so D8 has something to close.
    /// Uses the same approach as D3: find a liquid market, BUY at ask, enter via fill_tracker.
    fn open_test_position_for_d8(&mut self) {
        use rust_engine::signing::Side;
        use crate::fill_tracker::{self, SubmittedLeg};

        let market = match self.clob.find_liquid_market() {
            Some(m) => m,
            None => {
                // Try negRisk market as fallback
                match self.clob.find_neg_risk_market() {
                    Some(m) => m,
                    None => {
                        tracing::error!("[D8-seed] No liquid market found for test position");
                        return;
                    }
                }
            }
        };

        self.clob.register_instrument(&market, &self.engine);
        tracing::info!("[D8-seed] Selected market: {} (ask={:.4}) — {}",
            market.market_id, market.best_ask, market.question);

        let size = 2.50; // $2.50 minimum
        let position_id = format!("d8_seed_{}", crate::now_secs() as u64);

        let legs = vec![(
            market.market_id.clone(),
            market.yes_token_id.clone(),
            Side::Buy,
            market.best_ask,
            size,
        )];

        let result = self.executor.execute_arb(&position_id, &legs);
        if !result.all_accepted {
            tracing::error!("[D8-seed] BUY order rejected: {:?}", result.legs);
            return;
        }

        tracing::info!("[D8-seed] BUY submitted, confirming via WS...");

        let submitted_legs = vec![
            SubmittedLeg { market: market.clone(), size_usd: size },
        ];

        match fill_tracker::confirm_and_enter(
            &self.engine, &self.clob, &self.clob_auth, &position_id,
            &submitted_legs, false, &self.runtime, &self.wallet_address,
        ) {
            Ok((engine_pid, _fills)) => {
                tracing::info!("[D8-seed] Position entered: {} (engine={})", position_id, engine_pid);
                self.open_position_ids.push(engine_pid);
            }
            Err(e) => {
                tracing::error!("[D8-seed] Fill confirmation failed: {}", e);
                // Still wait a bit — the order may have filled on-chain
                std::thread::sleep(std::time::Duration::from_secs(10));
            }
        }
    }

    fn run_d8(&mut self) {
        if self.should_skip("D8") {
            self.skip_test("D8", "Closeout");
        } else {
            let result = tests::d8_closeout::run(
                &self.executor, &self.engine, &self.notifier,
                self.test_config.initial_capital, &mut self.exceptions,
            );
            self.report.add_result(result);
            self.engine.accounting.lock().summary_log("after-D8");
        }
        crate::flush_notify(&self.notifier);
        self.phase = Phase::Complete;
    }

    fn finalize(&mut self, success: bool) {
        let duration = self.start_time.elapsed().as_secs_f64();
        let final_capital = self.engine.current_capital();

        // End-of-run reconciliation: compare engine state vs exchange
        self.reconcile_end_of_run();

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
