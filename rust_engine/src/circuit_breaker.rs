/// Circuit breaker (C1).
///
/// Auto-pauses trading when safety thresholds are breached:
/// - Portfolio drawdown exceeds max from peak
/// - Consecutive errors exceed threshold within time window
/// - CLOB API unreachable for too long
///
/// Resume requires manual intervention: fix the issue and restart the process.
/// The tripped state is NOT persisted — a restart clears it.

use std::collections::VecDeque;

/// Circuit breaker configuration.
#[derive(Clone, Debug)]
pub struct CircuitBreakerConfig {
    pub enabled: bool,
    pub max_drawdown_pct: f64,
    pub max_consecutive_errors: u32,
    pub error_window_seconds: f64,
    pub api_timeout_seconds: f64,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_drawdown_pct: 0.10,
            max_consecutive_errors: 3,
            error_window_seconds: 300.0,
            api_timeout_seconds: 60.0,
        }
    }
}

/// Why the circuit breaker tripped.
#[derive(Debug, Clone)]
pub enum TripReason {
    Drawdown {
        current_pct: f64,
        threshold_pct: f64,
        peak: f64,
        current: f64,
    },
    ErrorBurst {
        count: u32,
        window_secs: f64,
    },
    ApiUnreachable {
        elapsed_secs: f64,
        threshold_secs: f64,
    },
}

impl std::fmt::Display for TripReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TripReason::Drawdown { current_pct, threshold_pct, peak, current } => {
                write!(f, "Drawdown {:.2}% exceeds {:.2}% threshold (peak=${:.2}, current=${:.2})",
                    current_pct * 100.0, threshold_pct * 100.0, peak, current)
            }
            TripReason::ErrorBurst { count, window_secs } => {
                write!(f, "{} errors in {:.0}s window", count, window_secs)
            }
            TripReason::ApiUnreachable { elapsed_secs, threshold_secs } => {
                write!(f, "API unreachable for {:.0}s (threshold {:.0}s)", elapsed_secs, threshold_secs)
            }
        }
    }
}

/// Circuit breaker state. Owned directly by the orchestrator (no Arc/Mutex needed).
pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    tripped: bool,
    trip_reason: Option<TripReason>,
    trip_timestamp: Option<f64>,
    peak_total_value: f64,
    error_timestamps: VecDeque<f64>,
    last_successful_api_ts: f64,
}

impl CircuitBreaker {
    /// Create a new circuit breaker.
    /// `initial_value` sets the starting peak (typically current total_value).
    /// `now` is the current timestamp in seconds since epoch.
    pub fn new(config: CircuitBreakerConfig, initial_value: f64, now: f64) -> Self {
        Self {
            config,
            tripped: false,
            trip_reason: None,
            trip_timestamp: None,
            peak_total_value: initial_value,
            error_timestamps: VecDeque::new(),
            last_successful_api_ts: now,
        }
    }

    /// Returns true if the circuit breaker has tripped.
    pub fn is_tripped(&self) -> bool {
        self.tripped
    }

    /// Returns true if the circuit breaker is enabled and not tripped.
    pub fn is_trading_allowed(&self) -> bool {
        !self.config.enabled || !self.tripped
    }

    /// Check all conditions. Returns the trip reason string if the breaker just tripped.
    /// Should be called once per tick with the current total portfolio value.
    pub fn check(&mut self, total_value: f64, now: f64) -> Option<String> {
        if !self.config.enabled || self.tripped {
            return None;
        }

        // Update peak
        if total_value > self.peak_total_value {
            self.peak_total_value = total_value;
        }

        // Check drawdown
        if self.peak_total_value > 0.0 {
            let drawdown = (self.peak_total_value - total_value) / self.peak_total_value;
            if drawdown >= self.config.max_drawdown_pct {
                let reason = TripReason::Drawdown {
                    current_pct: drawdown,
                    threshold_pct: self.config.max_drawdown_pct,
                    peak: self.peak_total_value,
                    current: total_value,
                };
                return Some(self.trip(reason, now));
            }
        }

        // Check error burst (sliding window)
        self.expire_old_errors(now);
        let error_count = self.error_timestamps.len() as u32;
        if error_count >= self.config.max_consecutive_errors {
            let reason = TripReason::ErrorBurst {
                count: error_count,
                window_secs: self.config.error_window_seconds,
            };
            return Some(self.trip(reason, now));
        }

        // Check API reachability
        let api_elapsed = now - self.last_successful_api_ts;
        if api_elapsed >= self.config.api_timeout_seconds {
            let reason = TripReason::ApiUnreachable {
                elapsed_secs: api_elapsed,
                threshold_secs: self.config.api_timeout_seconds,
            };
            return Some(self.trip(reason, now));
        }

        None
    }

    /// Record an error (e.g., failed API call, failed reconciliation).
    pub fn record_error(&mut self, now: f64) {
        self.error_timestamps.push_back(now);
        self.expire_old_errors(now);
    }

    /// Record a successful API interaction (resets the API timeout clock).
    pub fn record_api_success(&mut self, now: f64) {
        self.last_successful_api_ts = now;
        // A successful API call also clears error history
        self.error_timestamps.clear();
    }

    /// Get the current peak total value (for persistence).
    pub fn peak_total_value(&self) -> f64 {
        self.peak_total_value
    }

    /// Restore peak from persistence (called during load_state).
    pub fn set_peak(&mut self, peak: f64) {
        self.peak_total_value = peak;
    }

    /// Get trip info for dashboard/logging: (reason_string, timestamp).
    pub fn trip_info(&self) -> Option<(String, f64)> {
        self.trip_reason.as_ref().map(|r| {
            (r.to_string(), self.trip_timestamp.unwrap_or(0.0))
        })
    }

    /// Current drawdown from peak as a fraction (0.0 = no drawdown).
    pub fn current_drawdown(&self, total_value: f64) -> f64 {
        if self.peak_total_value > 0.0 {
            ((self.peak_total_value - total_value) / self.peak_total_value).max(0.0)
        } else {
            0.0
        }
    }

    // --- Private ---

    fn trip(&mut self, reason: TripReason, now: f64) -> String {
        let msg = reason.to_string();
        self.tripped = true;
        self.trip_reason = Some(reason);
        self.trip_timestamp = Some(now);
        msg
    }

    fn expire_old_errors(&mut self, now: f64) {
        let cutoff = now - self.config.error_window_seconds;
        while self.error_timestamps.front().map_or(false, |&t| t < cutoff) {
            self.error_timestamps.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            enabled: true,
            max_drawdown_pct: 0.10,
            max_consecutive_errors: 3,
            error_window_seconds: 300.0,
            api_timeout_seconds: 60.0,
        }
    }

    #[test]
    fn no_trip_when_healthy() {
        let mut cb = CircuitBreaker::new(default_config(), 1000.0, 0.0);
        assert!(!cb.is_tripped());
        assert!(cb.is_trading_allowed());
        // Small drawdown, no errors, recent API
        assert!(cb.check(950.0, 10.0).is_none());
        assert!(!cb.is_tripped());
    }

    #[test]
    fn trip_on_drawdown() {
        let mut cb = CircuitBreaker::new(default_config(), 1000.0, 0.0);
        // 10% drawdown = threshold
        let result = cb.check(900.0, 10.0);
        assert!(result.is_some());
        assert!(cb.is_tripped());
        assert!(!cb.is_trading_allowed());
        let msg = result.unwrap();
        assert!(msg.contains("Drawdown"));
    }

    #[test]
    fn no_trip_below_drawdown_threshold() {
        let mut cb = CircuitBreaker::new(default_config(), 1000.0, 0.0);
        // 9% drawdown = below threshold
        assert!(cb.check(910.0, 10.0).is_none());
        assert!(!cb.is_tripped());
    }

    #[test]
    fn peak_tracks_upward() {
        let mut cb = CircuitBreaker::new(default_config(), 1000.0, 0.0);
        cb.check(1100.0, 10.0); // new peak
        assert_eq!(cb.peak_total_value(), 1100.0);
        // Now 10% drawdown from 1100 = 990
        assert!(cb.check(990.0, 20.0).is_some());
    }

    #[test]
    fn trip_on_error_burst() {
        let mut cb = CircuitBreaker::new(default_config(), 1000.0, 0.0);
        cb.record_error(10.0);
        cb.record_error(11.0);
        assert!(cb.check(1000.0, 12.0).is_none()); // 2 errors, need 3
        cb.record_error(12.0);
        let result = cb.check(1000.0, 13.0);
        assert!(result.is_some());
        assert!(result.unwrap().contains("errors"));
    }

    #[test]
    fn errors_expire_outside_window() {
        let mut cb = CircuitBreaker::new(default_config(), 1000.0, 0.0);
        cb.record_error(10.0);
        cb.record_error(11.0);
        cb.record_error(12.0);
        // Keep API alive so we only test error expiry
        cb.record_api_success(310.0);
        // Check at t=320 — all errors outside 300s window
        assert!(cb.check(1000.0, 320.0).is_none());
        assert!(!cb.is_tripped());
    }

    #[test]
    fn api_success_clears_errors() {
        let mut cb = CircuitBreaker::new(default_config(), 1000.0, 0.0);
        cb.record_error(10.0);
        cb.record_error(11.0);
        cb.record_api_success(12.0);
        cb.record_error(13.0);
        // Only 1 error after the clear
        assert!(cb.check(1000.0, 14.0).is_none());
    }

    #[test]
    fn trip_on_api_timeout() {
        let mut cb = CircuitBreaker::new(default_config(), 1000.0, 0.0);
        // Last success at t=0, check at t=60
        let result = cb.check(1000.0, 60.0);
        assert!(result.is_some());
        assert!(result.unwrap().contains("API unreachable"));
    }

    #[test]
    fn api_success_resets_timeout() {
        let mut cb = CircuitBreaker::new(default_config(), 1000.0, 0.0);
        cb.record_api_success(50.0);
        // Check at t=60 — only 10s since last success
        assert!(cb.check(1000.0, 60.0).is_none());
    }

    #[test]
    fn disabled_breaker_never_trips() {
        let config = CircuitBreakerConfig { enabled: false, ..default_config() };
        let mut cb = CircuitBreaker::new(config, 1000.0, 0.0);
        assert!(cb.is_trading_allowed());
        // Even with massive drawdown
        assert!(cb.check(0.0, 100.0).is_none());
        assert!(cb.is_trading_allowed());
    }

    #[test]
    fn once_tripped_stays_tripped() {
        let mut cb = CircuitBreaker::new(default_config(), 1000.0, 0.0);
        cb.check(900.0, 10.0); // trip
        assert!(cb.is_tripped());
        // Subsequent checks don't return a new reason
        assert!(cb.check(1000.0, 20.0).is_none());
        assert!(cb.is_tripped()); // still tripped
    }

    #[test]
    fn trip_info_available() {
        let mut cb = CircuitBreaker::new(default_config(), 1000.0, 0.0);
        assert!(cb.trip_info().is_none());
        cb.check(900.0, 42.0);
        let (reason, ts) = cb.trip_info().unwrap();
        assert!(reason.contains("Drawdown"));
        assert_eq!(ts, 42.0);
    }

    #[test]
    fn set_peak_from_persistence() {
        let mut cb = CircuitBreaker::new(default_config(), 500.0, 0.0);
        cb.set_peak(2000.0);
        assert_eq!(cb.peak_total_value(), 2000.0);
        // 10% drawdown from 2000 = 1800
        assert!(cb.check(1800.0, 10.0).is_some());
    }
}
