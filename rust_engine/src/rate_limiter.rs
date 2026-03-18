/// Token-bucket rate limiter for Polymarket CLOB API (B3.3).
///
/// Enforces multiple rate limit tiers simultaneously:
///   - Trading: 60 orders/min
///   - Public:  100 req/min
///   - Auth:    300 req/min
///   - Global:  3,000 req/10min
///
/// Thread-safe via parking_lot::Mutex. Logs when throttled.

use std::time::Instant;
use parking_lot::Mutex;

/// A single token bucket.
struct Bucket {
    name: &'static str,
    /// Maximum tokens (burst capacity).
    capacity: u32,
    /// Current available tokens.
    tokens: f64,
    /// Tokens added per second (capacity / window_secs).
    refill_rate: f64,
    /// Last refill timestamp.
    last_refill: Instant,
}

impl Bucket {
    fn new(name: &'static str, capacity: u32, window_secs: f64) -> Self {
        Self {
            name,
            capacity,
            tokens: capacity as f64,
            refill_rate: capacity as f64 / window_secs,
            last_refill: Instant::now(),
        }
    }

    /// Refill tokens based on elapsed time.
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.capacity as f64);
        self.last_refill = now;
    }

    /// Try to consume one token. Returns true if allowed.
    fn try_consume(&mut self) -> bool {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Seconds until a token is available.
    fn wait_time(&mut self) -> f64 {
        self.refill();
        if self.tokens >= 1.0 {
            0.0
        } else {
            (1.0 - self.tokens) / self.refill_rate
        }
    }
}

/// Rate limit category for CLOB API requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateCategory {
    /// Order placement/cancellation (60/min).
    Trading,
    /// Public endpoints — markets, books (100/min).
    Public,
    /// Authenticated endpoints — balances, positions (300/min).
    Auth,
}

/// Thread-safe multi-tier rate limiter.
pub struct RateLimiter {
    trading: Mutex<Bucket>,
    public: Mutex<Bucket>,
    auth: Mutex<Bucket>,
    global: Mutex<Bucket>,
}

impl RateLimiter {
    /// Create with Polymarket's default limits.
    pub fn new() -> Self {
        Self {
            trading: Mutex::new(Bucket::new("trading", 60, 60.0)),
            public: Mutex::new(Bucket::new("public", 100, 60.0)),
            auth: Mutex::new(Bucket::new("auth", 300, 60.0)),
            global: Mutex::new(Bucket::new("global", 3000, 600.0)),
        }
    }

    /// Create with custom limits (for testing).
    pub fn with_limits(trading: u32, public: u32, auth: u32, global: u32) -> Self {
        Self {
            trading: Mutex::new(Bucket::new("trading", trading, 60.0)),
            public: Mutex::new(Bucket::new("public", public, 60.0)),
            auth: Mutex::new(Bucket::new("auth", auth, 60.0)),
            global: Mutex::new(Bucket::new("global", global, 600.0)),
        }
    }

    /// Check if a request of the given category is allowed.
    /// Returns Ok(()) if allowed, Err(wait_secs) if throttled.
    pub fn check(&self, category: RateCategory) -> Result<(), f64> {
        // Check category-specific bucket first
        let category_ok = match category {
            RateCategory::Trading => self.trading.lock().try_consume(),
            RateCategory::Public => self.public.lock().try_consume(),
            RateCategory::Auth => self.auth.lock().try_consume(),
        };

        if !category_ok {
            let wait = match category {
                RateCategory::Trading => self.trading.lock().wait_time(),
                RateCategory::Public => self.public.lock().wait_time(),
                RateCategory::Auth => self.auth.lock().wait_time(),
            };
            tracing::warn!("Rate limited ({:?}): wait {:.2}s", category, wait);
            return Err(wait);
        }

        // Check global bucket
        if !self.global.lock().try_consume() {
            let wait = self.global.lock().wait_time();
            tracing::warn!("Rate limited (global): wait {:.2}s", wait);
            // Refund the category token since we can't proceed — the category
            // bucket already consumed a token above, but the global bucket rejected
            // the request, so we must restore the category token to avoid double-counting.
            match category {
                RateCategory::Trading => self.trading.lock().tokens += 1.0,
                RateCategory::Public => self.public.lock().tokens += 1.0,
                RateCategory::Auth => self.auth.lock().tokens += 1.0,
            }
            return Err(wait);
        }

        Ok(())
    }

    /// Blocking wait until a request is allowed. Returns immediately if not throttled.
    pub fn wait_and_consume(&self, category: RateCategory) {
        loop {
            match self.check(category) {
                Ok(()) => return,
                Err(wait_secs) => {
                    let ms = (wait_secs * 1000.0).ceil() as u64;
                    std::thread::sleep(std::time::Duration::from_millis(ms.max(10)));
                }
            }
        }
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_allow() {
        let rl = RateLimiter::new();
        // First request should always succeed
        assert!(rl.check(RateCategory::Trading).is_ok());
        assert!(rl.check(RateCategory::Public).is_ok());
        assert!(rl.check(RateCategory::Auth).is_ok());
    }

    #[test]
    fn test_trading_limit() {
        let rl = RateLimiter::with_limits(3, 100, 300, 3000);
        assert!(rl.check(RateCategory::Trading).is_ok());
        assert!(rl.check(RateCategory::Trading).is_ok());
        assert!(rl.check(RateCategory::Trading).is_ok());
        // 4th should fail
        assert!(rl.check(RateCategory::Trading).is_err());
        // Other categories still work
        assert!(rl.check(RateCategory::Public).is_ok());
    }

    #[test]
    fn test_global_limit() {
        let rl = RateLimiter::with_limits(100, 100, 300, 2);
        assert!(rl.check(RateCategory::Trading).is_ok());
        assert!(rl.check(RateCategory::Public).is_ok());
        // Global exhausted (2 tokens used)
        assert!(rl.check(RateCategory::Auth).is_err());
    }
}
