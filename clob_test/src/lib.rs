/// Milestone D CLOB Integration Test Harness.
///
/// Automates all 8 Milestone D acceptance tests (D1-D8) with real CLOB orders.
/// Uses merged Shadow A-F parameters for maximum arb detection speed.

pub mod config;
pub mod report;
pub mod dedup;
pub mod ipc;
pub mod orchestrate;
pub mod tests;
pub mod clob_client;
pub mod fill_tracker;

/// Convenience wrapper: send a Telegram notification via the Notifier.
/// Uses NotifyEvent::Error variant to carry custom test messages.
pub fn notify(notifier: &rust_engine::notify::Notifier, msg: &str) {
    let _ = notifier.send(&rust_engine::notify::NotifyEvent::Error {
        message: msg.to_string(),
    });
}

/// Get current unix timestamp as f64.
pub fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
