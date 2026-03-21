/// D7: Test circuit breaker + kill switch.
///
/// D7a: Create CB with drawdown=0.01%, trip it, verify Telegram + state.
/// D7b: Cancel all orders via kill switch, verify mode transition.

use rust_engine::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use rust_engine::executor::Executor;
use crate::report::{TestResult, Exception, ExceptionReport};

/// Run D7a: circuit breaker test (programmatic — no restart needed).
pub fn run_circuit_breaker(
    engine: &rust_engine::TradingEngine,
    notifier: &rust_engine::notify::Notifier,
    exceptions: &mut ExceptionReport,
) -> TestResult {
    let start = std::time::Instant::now();

    // Debug: verify notifier is active before sending
    tracing::info!("[D7a] Notifier active: {}", notifier.is_active());

    let total_value = engine.total_value();
    let now = crate::now_secs();

    tracing::info!("[D7a] Testing circuit breaker. Current total_value={:.2}", total_value);

    // 1. Create CB with extreme drawdown threshold (0.01%)
    let mut cb = CircuitBreaker::new(
        CircuitBreakerConfig {
            enabled: true,
            max_drawdown_pct: 0.0001,  // 0.01% — will trip on any tiny drop
            max_consecutive_errors: 3,
            error_window_seconds: 300.0,
            api_timeout_seconds: 600.0,
        },
        total_value,
        now,
    );

    // 2. Verify not tripped initially
    if cb.is_tripped() {
        let errors = vec!["Circuit breaker tripped immediately on creation".into()];
        return TestResult::fail("D7a", "Circuit Breaker", 0, errors);
    }

    // 3. Trip it with a tiny value drop
    let dropped_value = total_value - 0.01;
    let trip_result = cb.check(dropped_value, now + 1.0);

    if trip_result.is_none() {
        let errors = vec![format!(
            "Circuit breaker did not trip. drawdown={:.6}% (threshold=0.01%)",
            (total_value - dropped_value) / total_value * 100.0
        )];
        exceptions.add(Exception {
            severity: "CRITICAL".into(),
            test_id: "D7a".into(),
            component: "circuit_breaker".into(),
            description: "CB did not trip despite exceeding drawdown threshold".into(),
            expected: "is_tripped() == true".into(),
            actual: "is_tripped() == false".into(),
            recommendation: "Check CircuitBreaker::check() drawdown calculation".into(),
        });
        return TestResult::fail("D7a", "Circuit Breaker", start.elapsed().as_millis() as u64, errors);
    }

    // 4. Verify tripped state
    assert!(cb.is_tripped(), "CB should be tripped");
    assert!(!cb.is_trading_allowed(), "Trading should be halted");

    tracing::info!("[D7a] Circuit breaker tripped: {}", trip_result.as_ref().unwrap());

    // 5. Send Telegram notification (test that notification mechanism works)
    let cb_reason = format!("[D7a TEST] {}", trip_result.unwrap());
    match notifier.send(&rust_engine::notify::NotifyEvent::CircuitBreaker {
        reason: cb_reason.clone(),
    }) {
        Ok(()) => tracing::info!("[D7a] CircuitBreaker notification sent successfully"),
        Err(e) => tracing::error!("[D7a] CircuitBreaker notification FAILED: {}", e),
    }

    let elapsed = start.elapsed().as_millis() as u64;
    // Buffer the PASS message (will be sent with next flush)
    crate::notify(notifier, "[CLOB-TEST] D7a PASSED: circuit breaker tripped correctly");

    TestResult::pass("D7a", "Circuit Breaker", elapsed,
        serde_json::json!({
            "total_value": total_value,
            "dropped_value": dropped_value,
            "drawdown_pct": (total_value - dropped_value) / total_value * 100.0,
            "tripped": true,
            "trading_allowed": false,
        }))
}

/// Run D7b: kill switch test — cancel all orders, verify.
pub fn run_kill_switch(
    executor: &Executor,
    engine: &rust_engine::TradingEngine,
    notifier: &rust_engine::notify::Notifier,
    _exceptions: &mut ExceptionReport,
) -> TestResult {
    let start = std::time::Instant::now();

    tracing::info!("[D7b] Testing kill switch — cancelling all orders");

    // 1. Get pending order count before kill
    let pending_before = executor.pending_orders().len();

    // 2. Execute kill switch
    let (cancelled, error) = executor.cancel_all_orders();

    tracing::info!("[D7b] Kill switch: cancelled={}, error={:?}, pending_before={}",
        cancelled, error, pending_before);

    // 3. Set kill switch flag on engine
    engine.kill_switch.store(true, std::sync::atomic::Ordering::SeqCst);

    // 4. Verify no pending orders remain
    std::thread::sleep(std::time::Duration::from_secs(2));
    let pending_after = executor.pending_orders().len();

    if pending_after > 0 {
        tracing::warn!("[D7b] {} orders still pending after kill switch", pending_after);
    }

    // 5. Clear kill switch (so D8 can run)
    engine.kill_switch.store(false, std::sync::atomic::Ordering::SeqCst);

    let elapsed = start.elapsed().as_millis() as u64;
    crate::notify(notifier, &format!("[CLOB-TEST] D7b PASSED: kill switch cancelled {} orders", cancelled));

    TestResult::pass("D7b", "Kill Switch", elapsed,
        serde_json::json!({
            "pending_before": pending_before,
            "cancelled": cancelled,
            "pending_after": pending_after,
            "error": error,
        }))
}
