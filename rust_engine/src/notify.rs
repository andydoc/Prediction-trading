/// WhatsApp notification module (C3).
///
/// Sends alerts via a generic HTTP webhook (compatible with WhatsApp Cloud API,
/// Twilio, CallMeBot, or any service accepting JSON POST with phone + message).
///
/// Design principles:
/// - Never panics or blocks trading — all errors are logged via tracing::warn
/// - Rate-limits messages to avoid spam
/// - Backs off after consecutive failures (5 failures → 5-minute cooldown)
/// - Each event type can be independently enabled/disabled

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use parking_lot::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Events that can trigger a WhatsApp notification.
pub enum NotifyEvent {
    PositionEntry {
        position_id: String,
        strategy: String,
        capital: f64,
        profit_pct: f64,
    },
    PositionResolved {
        position_id: String,
        profit: f64,
        method: String,
    },
    ProactiveExit {
        position_id: String,
        profit: f64,
        ratio: f64,
    },
    Error {
        message: String,
    },
    CircuitBreaker {
        reason: String,
    },
    DailySummary {
        entries: u32,
        exits: u32,
        fees: f64,
        net_pnl: f64,
        capital_util_pct: f64,
        drawdown_pct: f64,
    },
}

/// Configuration for WhatsApp notifications.
/// Loaded from config.yaml `notifications` section.
#[derive(Clone, Debug)]
pub struct NotifyConfig {
    pub enabled: bool,
    pub webhook_url: String,
    pub api_key: String,
    pub phone_number: String,
    pub on_entry: bool,
    pub on_resolution: bool,
    pub on_error: bool,
    pub on_circuit_breaker: bool,
    pub on_daily_summary: bool,
    pub rate_limit_seconds: f64,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            webhook_url: String::new(),
            api_key: String::new(),
            phone_number: String::new(),
            on_entry: true,
            on_resolution: true,
            on_error: true,
            on_circuit_breaker: true,
            on_daily_summary: true,
            rate_limit_seconds: 10.0,
        }
    }
}

/// WhatsApp notifier. Thread-safe; designed to be wrapped in Arc and shared.
pub struct Notifier {
    config: NotifyConfig,
    client: Option<reqwest::blocking::Client>,
    last_send: Mutex<f64>,
    consecutive_failures: AtomicU32,
    backoff_until: Mutex<f64>,
    disabled: AtomicBool,
}

const MAX_CONSECUTIVE_FAILURES: u32 = 5;
const BACKOFF_SECONDS: f64 = 300.0; // 5 minutes

impl Notifier {
    /// Create a new notifier with the given config.
    /// If config.enabled is false or webhook_url is empty, the notifier
    /// will be in noop mode (all sends are silently skipped).
    pub fn new(config: NotifyConfig) -> Self {
        let client = if config.enabled && !config.webhook_url.is_empty() {
            match reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
            {
                Ok(c) => {
                    tracing::info!(
                        "WhatsApp notifier enabled: phone={}, url={}",
                        config.phone_number,
                        config.webhook_url
                    );
                    Some(c)
                }
                Err(e) => {
                    tracing::warn!("Failed to create HTTP client for notifier: {}", e);
                    None
                }
            }
        } else {
            if config.enabled {
                tracing::warn!("Notifications enabled but webhook_url is empty — running in noop mode");
            }
            None
        };

        Self {
            config,
            client,
            last_send: Mutex::new(0.0),
            consecutive_failures: AtomicU32::new(0),
            backoff_until: Mutex::new(0.0),
            disabled: AtomicBool::new(false),
        }
    }

    /// Create a noop notifier that never sends anything.
    pub fn noop() -> Self {
        Self {
            config: NotifyConfig::default(),
            client: None,
            last_send: Mutex::new(0.0),
            consecutive_failures: AtomicU32::new(0),
            backoff_until: Mutex::new(0.0),
            disabled: AtomicBool::new(true),
        }
    }

    /// Returns true if this notifier is effectively disabled (noop or no client).
    pub fn is_active(&self) -> bool {
        !self.disabled.load(Ordering::Relaxed) && self.client.is_some()
    }

    /// Send a notification for the given event. Returns Ok(()) on success or
    /// if the message was intentionally skipped (disabled, rate-limited, wrong event type).
    /// Returns Err only for unexpected failures (which are also logged).
    pub fn send(&self, event: &NotifyEvent) -> Result<(), String> {
        // Quick exit if disabled
        if !self.is_active() {
            return Ok(());
        }

        // Check if this event type is enabled
        if !self.is_event_enabled(event) {
            return Ok(());
        }

        let now = now_secs();

        // Check backoff (after consecutive failures)
        {
            let backoff = self.backoff_until.lock();
            if now < *backoff {
                tracing::debug!(
                    "Notifier in backoff until {:.0} (now={:.0}), skipping",
                    *backoff,
                    now
                );
                return Ok(());
            }
        }

        // Rate limiting
        {
            let mut last = self.last_send.lock();
            let elapsed = now - *last;
            if elapsed < self.config.rate_limit_seconds {
                tracing::debug!(
                    "Rate-limited: {:.1}s since last send (limit={:.1}s)",
                    elapsed,
                    self.config.rate_limit_seconds
                );
                return Ok(());
            }
            *last = now;
        }

        let message = self.format_message(event);
        self.do_send(&message)
    }

    /// Check whether a particular event type is enabled in config.
    fn is_event_enabled(&self, event: &NotifyEvent) -> bool {
        match event {
            NotifyEvent::PositionEntry { .. } => self.config.on_entry,
            NotifyEvent::PositionResolved { .. } | NotifyEvent::ProactiveExit { .. } => {
                self.config.on_resolution
            }
            NotifyEvent::Error { .. } => self.config.on_error,
            NotifyEvent::CircuitBreaker { .. } => self.config.on_circuit_breaker,
            NotifyEvent::DailySummary { .. } => self.config.on_daily_summary,
        }
    }

    /// Format a human-readable message for the given event.
    fn format_message(&self, event: &NotifyEvent) -> String {
        match event {
            NotifyEvent::PositionEntry {
                position_id,
                strategy,
                capital,
                profit_pct,
            } => {
                format!(
                    "[ENTRY] New position {}\nStrategy: {}\nCapital: ${:.2}\nExpected profit: {:.2}%",
                    position_id, strategy, capital, profit_pct * 100.0
                )
            }
            NotifyEvent::PositionResolved {
                position_id,
                profit,
                method,
            } => {
                let icon = if *profit >= 0.0 { "[WIN]" } else { "[LOSS]" };
                format!(
                    "{} Position {} resolved\nMethod: {}\nProfit: ${:.4}",
                    icon, position_id, method, profit
                )
            }
            NotifyEvent::ProactiveExit {
                position_id,
                profit,
                ratio,
            } => {
                format!(
                    "[EXIT] Proactive exit {}\nProfit: ${:.4}\nRatio: {:.2}x",
                    position_id, profit, ratio
                )
            }
            NotifyEvent::Error { message } => {
                format!("[ERROR] {}", message)
            }
            NotifyEvent::CircuitBreaker { reason } => {
                format!("[CIRCUIT BREAKER] Trading halted: {}", reason)
            }
            NotifyEvent::DailySummary {
                entries,
                exits,
                fees,
                net_pnl,
                capital_util_pct,
                drawdown_pct,
            } => {
                format!(
                    "[DAILY SUMMARY]\nEntries: {}\nExits: {}\nFees: ${:.4}\nNet P&L: ${:.4}\nCapital util: {:.1}%\nDrawdown: {:.2}%",
                    entries, exits, fees, net_pnl, capital_util_pct * 100.0, drawdown_pct * 100.0
                )
            }
        }
    }

    /// Actually POST the message to the webhook URL.
    fn do_send(&self, message: &str) -> Result<(), String> {
        let client = match &self.client {
            Some(c) => c,
            None => return Ok(()),
        };

        let body = serde_json::json!({
            "phone": self.config.phone_number,
            "message": message,
        });

        let mut request = client
            .post(&self.config.webhook_url)
            .header("Content-Type", "application/json");

        // Add API key as Bearer token if provided
        if !self.config.api_key.is_empty() {
            request = request.header("Authorization", format!("Bearer {}", self.config.api_key));
        }

        match request.json(&body).send() {
            Ok(resp) => {
                if resp.status().is_success() {
                    self.consecutive_failures.store(0, Ordering::Relaxed);
                    tracing::debug!("WhatsApp notification sent: {}", &message[..message.len().min(60)]);
                    Ok(())
                } else {
                    let status = resp.status();
                    let body_text = resp.text().unwrap_or_default();
                    self.record_failure();
                    let err = format!("WhatsApp webhook returned {}: {}", status, body_text);
                    tracing::warn!("{}", err);
                    Err(err)
                }
            }
            Err(e) => {
                self.record_failure();
                let err = format!("WhatsApp webhook request failed: {}", e);
                tracing::warn!("{}", err);
                Err(err)
            }
        }
    }

    /// Record a failure and enter backoff if threshold is exceeded.
    fn record_failure(&self) {
        let count = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if count >= MAX_CONSECUTIVE_FAILURES {
            let until = now_secs() + BACKOFF_SECONDS;
            {
                let mut bo = self.backoff_until.lock();
                *bo = until;
            }
            tracing::warn!(
                "WhatsApp notifier: {} consecutive failures, backing off for {}s",
                count,
                BACKOFF_SECONDS
            );
        }
    }
}

/// Current time as seconds since UNIX epoch.
fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_notifier_skips_all() {
        let n = Notifier::noop();
        assert!(!n.is_active());
        let result = n.send(&NotifyEvent::Error {
            message: "test".into(),
        });
        assert!(result.is_ok());
    }

    #[test]
    fn default_config_is_disabled() {
        let cfg = NotifyConfig::default();
        assert!(!cfg.enabled);
        let n = Notifier::new(cfg);
        assert!(!n.is_active());
    }

    #[test]
    fn format_messages() {
        let n = Notifier::noop();
        let msg = n.format_message(&NotifyEvent::PositionEntry {
            position_id: "pos123".into(),
            strategy: "mutex".into(),
            capital: 50.0,
            profit_pct: 0.05,
        });
        assert!(msg.contains("[ENTRY]"));
        assert!(msg.contains("pos123"));
        assert!(msg.contains("5.00%"));

        let msg = n.format_message(&NotifyEvent::DailySummary {
            entries: 3,
            exits: 2,
            fees: 0.15,
            net_pnl: 4.50,
            capital_util_pct: 0.65,
            drawdown_pct: 0.02,
        });
        assert!(msg.contains("[DAILY SUMMARY]"));
        assert!(msg.contains("Entries: 3"));
    }

    #[test]
    fn event_type_filtering() {
        let cfg = NotifyConfig {
            enabled: true,
            on_entry: false,
            on_error: true,
            ..NotifyConfig::default()
        };
        let n = Notifier::new(cfg);
        assert!(!n.is_event_enabled(&NotifyEvent::PositionEntry {
            position_id: "x".into(),
            strategy: "y".into(),
            capital: 1.0,
            profit_pct: 0.01,
        }));
        assert!(n.is_event_enabled(&NotifyEvent::Error {
            message: "boom".into(),
        }));
    }
}
