/// WebSocket User Channel client — connects to Polymarket's authenticated
/// user channel for real-time trade and order events.
///
/// Endpoint: wss://ws-subscriptions-clob.polymarket.com/ws/user
///
/// Events received:
///   - trade: fill lifecycle (MATCHED → MINED → CONFIRMED / RETRYING → FAILED)
///   - order: placement, update (partial fill), cancellation
///
/// Exposes fills via crossbeam channel for consumers (test harness, executor).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::signing::ClobAuth;

const USER_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/user";

// ---------------------------------------------------------------------------
// Public event types
// ---------------------------------------------------------------------------

/// A confirmed (or failed) trade event from the user channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeEvent {
    pub id: String,
    pub market: String,
    pub asset_id: String,
    pub outcome: String,
    pub side: String,
    pub size: f64,
    pub price: f64,
    pub status: String,
    pub timestamp: f64,
}

/// An order lifecycle event from the user channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderEvent {
    pub id: String,
    pub market: String,
    pub asset_id: String,
    pub event_subtype: String, // PLACEMENT, UPDATE, CANCELLATION
    pub original_size: f64,
    pub size_matched: f64,
    pub outcome: String,
    pub side: String,
    pub price: f64,
    pub timestamp: f64,
}

/// Union of events from the user channel.
#[derive(Debug, Clone)]
pub enum UserEvent {
    Trade(TradeEvent),
    Order(OrderEvent),
}

// ---------------------------------------------------------------------------
// UserChannelClient
// ---------------------------------------------------------------------------

pub struct UserChannelClient {
    running: Arc<AtomicBool>,
    shutdown: Arc<Notify>,
    pub receiver: Receiver<UserEvent>,
    sender: Sender<UserEvent>,
}

impl UserChannelClient {
    /// Create a new client (does not connect yet).
    pub fn new() -> Self {
        let (sender, receiver) = bounded(512);
        Self {
            running: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(Notify::new()),
            receiver,
            sender,
        }
    }

    /// Start the user channel WS connection in a background tokio task.
    /// `market_ids` are the condition_ids to subscribe for.
    pub fn start(&self, auth: &ClobAuth, market_ids: Vec<String>, runtime: &tokio::runtime::Handle) {
        self.running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.running);
        let shutdown = Arc::clone(&self.shutdown);
        let sender = self.sender.clone();
        let auth_headers = auth.build_headers("GET", "/ws/user", None);

        runtime.spawn(async move {
            user_channel_loop(running, shutdown, sender, auth_headers, market_ids).await;
        });
    }

    /// Stop the user channel connection.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.shutdown.notify_waiters();
    }

    /// Wait for a trade event matching the given asset_ids to reach CONFIRMED status.
    /// Returns the matching TradeEvent, or None on timeout.
    pub fn wait_for_confirmed_fills(
        &self,
        asset_ids: &[String],
        timeout: Duration,
    ) -> Vec<TradeEvent> {
        let deadline = std::time::Instant::now() + timeout;
        let mut confirmed = Vec::new();
        let mut pending: std::collections::HashSet<String> = asset_ids.iter().cloned().collect();

        while !pending.is_empty() {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                tracing::warn!("UserChannel: timeout waiting for fills. Got {}/{}, missing: {:?}",
                    confirmed.len(), asset_ids.len(), pending);
                break;
            }

            match self.receiver.recv_timeout(remaining) {
                Ok(UserEvent::Trade(trade)) => {
                    tracing::info!("UserChannel trade: asset={} status={} size={} price={}",
                        trade.asset_id, trade.status, trade.size, trade.price);

                    if (trade.status == "CONFIRMED" || trade.status == "MINED")
                        && pending.contains(&trade.asset_id)
                    {
                        pending.remove(&trade.asset_id);
                        confirmed.push(trade);
                    }
                }
                Ok(UserEvent::Order(order)) => {
                    tracing::debug!("UserChannel order: id={} type={} matched={}",
                        order.id, order.event_subtype, order.size_matched);
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => break,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }

        confirmed
    }

    /// Check if the client is currently running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

impl Drop for UserChannelClient {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// Internal WS loop
// ---------------------------------------------------------------------------

async fn user_channel_loop(
    running: Arc<AtomicBool>,
    shutdown: Arc<Notify>,
    sender: Sender<UserEvent>,
    auth_headers: Vec<(String, String)>,
    market_ids: Vec<String>,
) {
    let mut backoff_ms: u64 = 1000;

    while running.load(Ordering::Relaxed) {
        tracing::info!("UserChannel: connecting to {} ({} markets)...", USER_WS_URL, market_ids.len());

        match user_channel_connect(&running, &shutdown, &sender, &auth_headers, &market_ids).await {
            Ok(()) => {
                tracing::info!("UserChannel: clean disconnect");
                backoff_ms = 1000;
            }
            Err(e) => {
                tracing::warn!("UserChannel: error: {}, reconnecting in {}ms", e, backoff_ms);
            }
        }

        if !running.load(Ordering::Relaxed) {
            break;
        }

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(backoff_ms)) => {}
            _ = shutdown.notified() => { break; }
        }
        backoff_ms = (backoff_ms * 2).min(30_000);
    }

    tracing::info!("UserChannel: loop ended");
}

async fn user_channel_connect(
    running: &Arc<AtomicBool>,
    shutdown: &Arc<Notify>,
    sender: &Sender<UserEvent>,
    auth_headers: &[(String, String)],
    market_ids: &[String],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (ws_stream, _) = tokio_tungstenite::connect_async(USER_WS_URL).await?;
    let (mut sink, mut stream) = ws_stream.split();

    tracing::info!("UserChannel: connected, sending auth + subscribe");

    // Build auth object from headers
    let mut api_key = String::new();
    let mut passphrase = String::new();
    let mut signature = String::new();
    let mut timestamp = String::new();
    for (k, v) in auth_headers {
        match k.as_str() {
            "POLY_API_KEY" => api_key = v.clone(),
            "POLY_PASSPHRASE" => passphrase = v.clone(),
            "POLY_SIGNATURE" => signature = v.clone(),
            "POLY_TIMESTAMP" => timestamp = v.clone(),
            _ => {}
        }
    }

    // Subscribe message per Polymarket docs
    let sub_msg = serde_json::json!({
        "auth": {
            "apiKey": api_key,
            "secret": signature,
            "passphrase": passphrase,
        },
        "markets": market_ids,
        "type": "user",
    });

    sink.send(WsMessage::Text(sub_msg.to_string())).await?;
    tracing::info!("UserChannel: subscribed to {} markets", market_ids.len());

    let mut heartbeat = tokio::time::interval(Duration::from_secs(10));

    loop {
        tokio::select! {
            biased;

            _ = heartbeat.tick() => {
                if let Err(e) = sink.send(WsMessage::Text("PING".to_string())).await {
                    return Err(Box::new(e));
                }
            }

            msg = stream.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        if crate::ws::is_pong(&text) {
                            continue;
                        }
                        parse_and_dispatch(&text, sender);
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        let _ = sink.send(WsMessage::Pong(data)).await;
                    }
                    Some(Ok(WsMessage::Close(_))) => return Ok(()),
                    Some(Err(e)) => return Err(Box::new(e)),
                    None => return Ok(()),
                    _ => {}
                }
            }

            _ = shutdown.notified() => {
                let _ = sink.send(WsMessage::Close(None)).await;
                return Ok(());
            }
        }

        if !running.load(Ordering::Relaxed) {
            let _ = sink.send(WsMessage::Close(None)).await;
            return Ok(());
        }
    }
}

fn parse_and_dispatch(text: &str, sender: &Sender<UserEvent>) {
    let messages: Vec<serde_json::Value> = if text.starts_with('[') {
        serde_json::from_str(text).unwrap_or_default()
    } else {
        match serde_json::from_str(text) {
            Ok(v) => vec![v],
            Err(_) => return,
        }
    };

    for msg in &messages {
        let event_type = msg.get("event_type")
            .or_else(|| msg.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match event_type {
            "trade" => {
                if let Some(trade) = parse_trade_event(msg) {
                    let _ = sender.try_send(UserEvent::Trade(trade));
                }
            }
            "order" => {
                if let Some(order) = parse_order_event(msg) {
                    let _ = sender.try_send(UserEvent::Order(order));
                }
            }
            _ => {
                tracing::trace!("UserChannel unknown event: {}", event_type);
            }
        }
    }
}

fn parse_f64(v: &serde_json::Value, key: &str) -> f64 {
    v.get(key)
        .and_then(|x| x.as_str().and_then(|s| s.parse().ok()).or_else(|| x.as_f64()))
        .unwrap_or(0.0)
}

fn parse_str(v: &serde_json::Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn parse_trade_event(msg: &serde_json::Value) -> Option<TradeEvent> {
    Some(TradeEvent {
        id: parse_str(msg, "id"),
        market: parse_str(msg, "market"),
        asset_id: parse_str(msg, "asset_id"),
        outcome: parse_str(msg, "outcome"),
        side: parse_str(msg, "side"),
        size: parse_f64(msg, "size"),
        price: parse_f64(msg, "price"),
        status: parse_str(msg, "status"),
        timestamp: parse_f64(msg, "timestamp"),
    })
}

fn parse_order_event(msg: &serde_json::Value) -> Option<OrderEvent> {
    // Order events use "type" for the subtype (PLACEMENT/UPDATE/CANCELLATION)
    // but top-level "event_type" is "order", so we read the nested "type"
    let subtype = msg.get("type")
        .and_then(|v| v.as_str())
        .filter(|s| *s != "order") // don't confuse event_type with subtype
        .unwrap_or("UNKNOWN")
        .to_string();

    Some(OrderEvent {
        id: parse_str(msg, "id"),
        market: parse_str(msg, "market"),
        asset_id: parse_str(msg, "asset_id"),
        event_subtype: subtype,
        original_size: parse_f64(msg, "original_size"),
        size_matched: parse_f64(msg, "size_matched"),
        outcome: parse_str(msg, "outcome"),
        side: parse_str(msg, "side"),
        price: parse_f64(msg, "price"),
        timestamp: parse_f64(msg, "timestamp"),
    })
}
