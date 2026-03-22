/// Notification module (C3).
///
/// Sends alerts via HTTP webhook. Supports multiple backends:
/// - **Telegram**: auto-detected when webhook_url contains `api.telegram.org`.
///   Uses `chat_id` (from `phone_number` config field) + `text` JSON body.
/// - **Generic webhook**: JSON POST with `{"phone": "...", "message": "..."}`.
///   Compatible with WhatsApp Cloud API, Twilio, ntfy.sh, Discord, etc.
///
/// Design principles:
/// - Never panics or blocks trading — all errors are logged via tracing::warn
/// - Rate-limits messages to avoid spam
/// - Backs off after consecutive failures (5 failures → 5-minute cooldown)
/// - Each event type can be independently enabled/disabled

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use parking_lot::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Events that can trigger a notification.
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
    Startup {
        mode: String,
        positions: usize,
        capital: f64,
    },
    /// B3.2: Trade failed on-chain — suspense reversed, opposing legs may need selling.
    TradeFailed {
        trade_id: String,
        position_id: String,
        market_id: String,
        reason: String,
        opposing_legs_sold: bool,
    },
}

/// Configuration for notifications.
/// Loaded from config.yaml `notifications` section.
///
/// For Telegram: set `webhook_url` to `https://api.telegram.org/bot<TOKEN>/sendMessage`
/// and `phone_number` to your chat ID (get it from @userinfobot or /getUpdates).
///
/// S2: `webhook_url` contains the Telegram bot token and `api_key` may contain
/// secrets. A custom Debug impl redacts both fields to prevent leaking into logs.
#[derive(Clone)]
pub struct NotifyConfig {
    pub enabled: bool,
    pub webhook_url: String,
    pub api_key: String,
    /// Phone number (generic webhook) or Telegram chat_id.
    pub phone_number: String,
    pub on_entry: bool,
    pub on_resolution: bool,
    pub on_error: bool,
    pub on_circuit_breaker: bool,
    pub on_daily_summary: bool,
    pub rate_limit_seconds: f64,
    /// Machine hostname — prepended to all messages.
    pub hostname: String,
    /// Instance name (e.g. "shadow-a") — prepended when set.
    pub instance: String,
}

impl std::fmt::Debug for NotifyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotifyConfig")
            .field("enabled", &self.enabled)
            .field("webhook_url", &"[REDACTED]")
            .field("api_key", &"[REDACTED]")
            .field("phone_number", &self.phone_number)
            .field("on_entry", &self.on_entry)
            .field("on_resolution", &self.on_resolution)
            .field("on_error", &self.on_error)
            .field("on_circuit_breaker", &self.on_circuit_breaker)
            .field("on_daily_summary", &self.on_daily_summary)
            .field("rate_limit_seconds", &self.rate_limit_seconds)
            .field("hostname", &self.hostname)
            .field("instance", &self.instance)
            .finish()
    }
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
            hostname: String::new(),
            instance: String::new(),
        }
    }
}

/// Notifier. Thread-safe; designed to be wrapped in Arc and shared.
pub struct Notifier {
    config: NotifyConfig,
    client: Option<reqwest::blocking::Client>,
    last_send: Mutex<f64>,
    consecutive_failures: AtomicU32,
    backoff_until: Mutex<f64>,
    disabled: AtomicBool,
    /// Message buffer for batched sending. Flushed every 30s via flush_buffer().
    buffer: Mutex<Vec<String>>,
    buffer_last_flush: Mutex<f64>,
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
                    let backend = if config.webhook_url.contains("api.telegram.org") {
                        "Telegram"
                    } else {
                        "webhook"
                    };
                    tracing::info!(
                        "Notifier enabled ({}): recipient={}, url={}",
                        backend,
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
                tracing::warn!("Notifications enabled but webhook_url is empty - running in noop mode");
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
            buffer: Mutex::new(Vec::new()),
            buffer_last_flush: Mutex::new(0.0),
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
            buffer: Mutex::new(Vec::new()),
            buffer_last_flush: Mutex::new(0.0),
        }
    }

    /// Returns true if this notifier is effectively disabled (noop or no client).
    pub fn is_active(&self) -> bool {
        !self.disabled.load(Ordering::Relaxed) && self.client.is_some()
    }

    /// Add a message to the buffer. Call flush_buffer() to send.
    pub fn buffer_message(&self, msg: &str) {
        if !self.is_active() { return; }
        let mut buf = self.buffer.lock();
        buf.push(msg.to_string());
    }

    /// Flush the message buffer: if non-empty, concatenate all messages
    /// and send as a single notification. Returns true if a message was sent.
    pub fn flush_buffer(&self) -> bool {
        if !self.is_active() { return false; }
        let messages: Vec<String> = {
            let mut buf = self.buffer.lock();
            if buf.is_empty() { return false; }
            std::mem::take(&mut *buf)
        };
        let combined = messages.join("\n\n");
        let pfx = self.prefix();
        let full_msg = format!("{}{}", pfx, combined);
        match self.do_send(&full_msg) {
            Ok(()) => {
                let mut last = self.buffer_last_flush.lock();
                *last = now_secs();
                true
            }
            Err(e) => {
                tracing::warn!("Buffer flush failed: {}", e);
                false
            }
        }
    }

    /// Check if enough time has passed since last flush (30s default).
    /// If so, flush automatically.
    pub fn maybe_flush(&self) {
        let elapsed = {
            let last = self.buffer_last_flush.lock();
            now_secs() - *last
        };
        if elapsed >= 30.0 {
            self.flush_buffer();
        }
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
            NotifyEvent::Startup { .. } => true, // always send
            NotifyEvent::TradeFailed { .. } => self.config.on_error, // always alert on trade failures
        }
    }

    /// Build the prefix line: "[hostname/instance] " or "[hostname] ".
    fn prefix(&self) -> String {
        let h = &self.config.hostname;
        let i = &self.config.instance;
        if !h.is_empty() && !i.is_empty() {
            format!("[{}/{}] ", h, i)
        } else if !h.is_empty() {
            format!("[{}] ", h)
        } else if !i.is_empty() {
            format!("[{}] ", i)
        } else {
            String::new()
        }
    }

    /// Format a human-readable message for the given event.
    fn format_message(&self, event: &NotifyEvent) -> String {
        let pfx = self.prefix();
        match event {
            NotifyEvent::PositionEntry {
                position_id,
                strategy,
                capital,
                profit_pct,
            } => {
                format!(
                    "{}[ENTRY] New position {}\nStrategy: {}\nCapital: ${:.2}\nExpected profit: {:.2}%",
                    pfx, position_id, strategy, capital, profit_pct * 100.0
                )
            }
            NotifyEvent::PositionResolved {
                position_id,
                profit,
                method,
            } => {
                let icon = if *profit >= 0.0 { "[WIN]" } else { "[LOSS]" };
                format!(
                    "{}{} Position {} resolved\nMethod: {}\nProfit: ${:.4}",
                    pfx, icon, position_id, method, profit
                )
            }
            NotifyEvent::ProactiveExit {
                position_id,
                profit,
                ratio,
            } => {
                format!(
                    "{}[EXIT] Proactive exit {}\nProfit: ${:.4}\nRatio: {:.2}x",
                    pfx, position_id, profit, ratio
                )
            }
            NotifyEvent::Error { message } => {
                // Messages that already have their own prefix (e.g. [CLOB-TEST])
                // don't need the [ERROR] tag — they're informational, not errors.
                if message.starts_with("[CLOB-TEST]") || message.starts_with("[CLOB-TEST EXCEPTION]") {
                    format!("{}{}", pfx, message)
                } else {
                    format!("{}[ERROR] {}", pfx, message)
                }
            }
            NotifyEvent::CircuitBreaker { reason } => {
                format!("{}[CIRCUIT BREAKER] Trading halted: {}", pfx, reason)
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
                    "{}[DAILY SUMMARY]\nEntries: {}\nExits: {}\nFees: ${:.4}\nNet P&L: ${:.4}\nCapital util: {:.1}%\nDrawdown: {:.2}%",
                    pfx, entries, exits, fees, net_pnl, capital_util_pct * 100.0, drawdown_pct * 100.0
                )
            }
            NotifyEvent::Startup { mode, positions, capital } => {
                format!(
                    "{}[STARTUP] Engine started\nMode: {}\nOpen positions: {}\nCapital: ${:.2}",
                    pfx, mode, positions, capital
                )
            }
            NotifyEvent::TradeFailed {
                trade_id,
                position_id,
                market_id,
                reason,
                opposing_legs_sold,
            } => {
                let legs_msg = if *opposing_legs_sold {
                    "Opposing arb legs queued for sell"
                } else {
                    "No opposing legs to unwind"
                };
                format!(
                    "{}[TRADE FAILED] Trade {} on position {}\nMarket: {}\nReason: {}\n{}",
                    pfx, trade_id, position_id, market_id, reason, legs_msg
                )
            }
        }
    }

    /// Returns true if the webhook URL points to the Telegram Bot API.
    fn is_telegram(&self) -> bool {
        self.config.webhook_url.contains("api.telegram.org")
    }

    /// Actually POST the message to the webhook URL.
    fn do_send(&self, message: &str) -> Result<(), String> {
        let client = match &self.client {
            Some(c) => c,
            None => return Ok(()),
        };

        let body = if self.is_telegram() {
            // No parse_mode — plain text handles <, >, & and emojis without escaping.
            serde_json::json!({
                "chat_id": self.config.phone_number,
                "text": message,
            })
        } else {
            serde_json::json!({
                "phone": self.config.phone_number,
                "message": message,
            })
        };

        let mut request = client
            .post(&self.config.webhook_url)
            .header("Content-Type", "application/json");

        // Add API key as Bearer token if provided (not needed for Telegram)
        if !self.config.api_key.is_empty() && !self.is_telegram() {
            request = request.header("Authorization", format!("Bearer {}", self.config.api_key));
        }

        match request.json(&body).send() {
            Ok(resp) => {
                if resp.status().is_success() {
                    self.consecutive_failures.store(0, Ordering::Relaxed);
                    tracing::debug!("Notification sent: {}", message.get(..60).unwrap_or(message));
                    Ok(())
                } else {
                    let status = resp.status();
                    let body_text = resp.text().unwrap_or_default();
                    self.record_failure();
                    let err = format!("Notification webhook returned {}: {}", status, body_text);
                    tracing::warn!("{}", err);
                    Err(err)
                }
            }
            Err(e) => {
                self.record_failure();
                let err = format!("Notification webhook request failed: {}", e);
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
                "Notifier: {} consecutive failures, backing off for {}s",
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
