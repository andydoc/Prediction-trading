/// Telegram bot polling for the `/kill` command (G-KILL / Q2=B).
///
/// **Purpose**: provide a phone-accessible kill switch. Operator types
/// `/kill` to the configured Telegram chat, the bot polls and matches the
/// command, and the engine's existing kill-switch AtomicBool is flipped —
/// the same flag the dashboard `POST /api/kill-switch` sets.
///
/// **Why not webhook**: webhooks need an inbound HTTPS endpoint with a
/// real cert; long-poll works from anywhere with outbound HTTPS only and
/// keeps the design symmetric with how the existing notifier sends
/// messages.
///
/// **Auth**: only commands from the configured `chat_id` are honoured.
/// Polymarket public bot URLs typically have one chat — single-operator
/// model. Other chat_ids are silently ignored.
///
/// **Idempotency**: getUpdates is called with `offset = last_update_id + 1`
/// so each update is processed exactly once. State is in-memory only;
/// after a restart, any backlog is re-read but the kill-switch is already
/// armed (operator-set state survives via the engine's startup logic).
///
/// Created 2026-04-21 for v0.20.3 (G-KILL / Q2=B).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use serde_json::Value;

const POLL_TIMEOUT_SECS: u64 = 25;
const HTTP_TIMEOUT_SECS: u64 = 35; // > poll timeout to avoid client-side cuts

/// Extract the bot token from a Telegram sendMessage URL.
/// Returns None if the URL doesn't match the expected Telegram bot pattern.
fn extract_bot_token(webhook_url: &str) -> Option<String> {
    // Expected: https://api.telegram.org/bot<TOKEN>/sendMessage
    let needle = "/bot";
    let start = webhook_url.find(needle)? + needle.len();
    let rest = &webhook_url[start..];
    let end = rest.find('/').unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Spawn the long-poll loop on the provided tokio runtime handle.
///
/// The loop runs until `running` becomes false. Network errors are logged
/// at debug level and the loop backs off briefly before retrying — we
/// don't want a transient outage to be reported as a fatal error.
///
/// Once `/kill` is matched, the kill-switch is flipped and a confirmation
/// reply is sent. The loop continues running so the operator can confirm
/// state via further messages if desired.
pub fn spawn(
    handle: &tokio::runtime::Handle,
    bot_token: String,
    allowed_chat_id: String,
    kill_switch: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
) {
    handle.spawn(async move {
        if bot_token.is_empty() || allowed_chat_id.is_empty() {
            tracing::warn!("[telegram_kill] disabled: missing bot_token or chat_id");
            return;
        }
        tracing::info!(
            "[telegram_kill] /kill handler armed (chat_id={})",
            allowed_chat_id
        );

        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .min_tls_version(reqwest::tls::Version::TLS_1_2)
            .https_only(true)
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("[telegram_kill] HTTP client build failed: {}", e);
                return;
            }
        };

        let mut last_update_id: i64 = 0;
        loop {
            if !running.load(Ordering::SeqCst) {
                tracing::info!("[telegram_kill] shutting down");
                break;
            }

            let url = format!(
                "https://api.telegram.org/bot{}/getUpdates?offset={}&timeout={}&allowed_updates=%5B%22message%22%5D",
                bot_token, last_update_id + 1, POLL_TIMEOUT_SECS
            );

            let resp = match client.get(&url).send().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("[telegram_kill] getUpdates error: {}", e);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };

            if !resp.status().is_success() {
                tracing::debug!("[telegram_kill] getUpdates status {}", resp.status());
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }

            let body: Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::debug!("[telegram_kill] getUpdates parse error: {}", e);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };

            let updates = body.get("result").and_then(|v| v.as_array());
            let updates = match updates {
                Some(u) => u,
                None => {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };

            for upd in updates {
                let update_id = upd.get("update_id").and_then(|v| v.as_i64()).unwrap_or(0);
                if update_id > last_update_id {
                    last_update_id = update_id;
                }
                let msg = match upd.get("message") {
                    Some(m) => m,
                    None => continue,
                };
                let chat_id = msg.get("chat").and_then(|c| c.get("id")).and_then(|v| v.as_i64());
                let text = msg.get("text").and_then(|v| v.as_str()).unwrap_or("").trim();
                let chat_str = chat_id.map(|i| i.to_string()).unwrap_or_default();

                if chat_str != allowed_chat_id {
                    tracing::debug!("[telegram_kill] ignoring message from unauthorised chat={}", chat_str);
                    continue;
                }

                if text.eq_ignore_ascii_case("/kill") || text.eq_ignore_ascii_case("/kill@") {
                    let was = kill_switch.swap(true, Ordering::SeqCst);
                    let reply = if was {
                        "Kill switch already armed. (No change.)"
                    } else {
                        "Kill switch ARMED. Engine will cancel orders, switch to shadow_only, and stop entering new positions."
                    };
                    let _ = send_reply(&client, &bot_token, &allowed_chat_id, reply).await;
                    tracing::warn!("[telegram_kill] KILL command from chat={} — switch armed (was_already={})", allowed_chat_id, was);
                } else if text.starts_with('/') {
                    let _ = send_reply(
                        &client, &bot_token, &allowed_chat_id,
                        "Only /kill is supported by this bot. Use the dashboard for other actions."
                    ).await;
                }
            }
        }
    });
}

async fn send_reply(
    client: &reqwest::Client,
    bot_token: &str,
    chat_id: &str,
    text: &str,
) -> Result<(), String> {
    let url = format!("https://api.telegram.org/bot{}/sendMessage", bot_token);
    let body = serde_json::json!({
        "chat_id": chat_id,
        "text": text,
        "disable_notification": false,
    });
    client.post(&url).json(&body).send().await
        .map_err(|e| format!("send_reply failed: {}", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_token_basic() {
        let url = "https://api.telegram.org/bot123456:ABCdef/sendMessage";
        assert_eq!(extract_bot_token(url), Some("123456:ABCdef".into()));
    }

    #[test]
    fn extract_token_no_match() {
        assert_eq!(extract_bot_token("https://example.com/webhook"), None);
    }

    #[test]
    fn extract_token_trailing_slash() {
        let url = "https://api.telegram.org/bot999/sendMessage/";
        assert_eq!(extract_bot_token(url), Some("999".into()));
    }
}

/// Public re-export of the token extractor so the supervisor can wire it
/// into `spawn` without re-implementing.
pub fn token_from_webhook(webhook_url: &str) -> Option<String> {
    extract_bot_token(webhook_url)
}
