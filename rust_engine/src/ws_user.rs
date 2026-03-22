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
    pub taker_order_id: String,  // Our order ID when we're the taker (FAK)
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
        // WS user channel wants raw API credentials, NOT HMAC signatures.
        // build_headers() computes HMAC which is only for REST endpoints.
        let api_key = auth.api_key().to_string();
        let secret = auth.raw_secret_b64();
        let passphrase = auth.passphrase().to_string();

        runtime.spawn(async move {
            user_channel_loop(running, shutdown, sender, api_key, secret, passphrase, market_ids).await;
        });
    }

    /// Stop the user channel connection.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.shutdown.notify_waiters();
    }

    /// Wait for trade fills matching the given asset_ids.
    ///
    /// Tracks the full lifecycle: MATCHED → MINED → CONFIRMED.
    /// Uses `id` (trade event UUID) as correlation key — consistent across lifecycle.
    /// Uses `taker_order_id` to identify our orders.
    ///
    /// Accepts MATCHED as sufficient (CONFIRMED may never arrive for ~20% of trades
    /// per WS investigation 2026-03-22). Deduplicates by trade `id`.
    pub fn wait_for_confirmed_fills(
        &self,
        asset_ids: &[String],
        timeout: Duration,
    ) -> Vec<TradeEvent> {
        let deadline = std::time::Instant::now() + timeout;
        let mut fills: Vec<TradeEvent> = Vec::new();
        let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut pending: std::collections::HashSet<String> = asset_ids.iter().cloned().collect();

        // Phase 1: collect MATCHED events for all assets (fast — usually <1s)
        // Phase 2: wait for MINED/CONFIRMED to upgrade them (may not arrive for all)
        let mut matched_assets: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut matched_events: std::collections::HashMap<String, TradeEvent> = std::collections::HashMap::new(); // asset_id → MATCHED event

        while !pending.is_empty() {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                tracing::warn!("UserChannel: timeout. confirmed={}/{}, matched_only={}, missing={:?}",
                    fills.len(), asset_ids.len(), matched_assets.len(), pending);
                break;
            }

            match self.receiver.recv_timeout(remaining.min(Duration::from_secs(5))) {
                Ok(UserEvent::Trade(trade)) => {
                    let dominated = trade.status == "MATCHED"
                        || trade.status == "MINED"
                        || trade.status == "CONFIRMED";

                    if !dominated { continue; }

                    let is_our_asset = pending.contains(&trade.asset_id)
                        || matched_assets.contains(&trade.asset_id);

                    tracing::info!("UserChannel trade: id={}... status={} taker={}... asset={}... size={} price={} ours={}",
                        trade.id.get(..12).unwrap_or(&trade.id),
                        trade.status,
                        trade.taker_order_id.get(..12).unwrap_or(&trade.taker_order_id),
                        trade.asset_id.get(..12).unwrap_or(&trade.asset_id),
                        trade.size, trade.price, is_our_asset);

                    if !is_our_asset { continue; }

                    match trade.status.as_str() {
                        "MATCHED" => {
                            if !seen_ids.contains(&trade.id) {
                                seen_ids.insert(trade.id.clone());
                                matched_assets.insert(trade.asset_id.clone());
                                matched_events.insert(trade.asset_id.clone(), trade);
                                // Don't remove from pending yet — wait for MINED/CONFIRMED
                            }
                        }
                        "MINED" | "CONFIRMED" => {
                            // Dedup: only record first MINED or CONFIRMED per trade id
                            if !seen_ids.contains(&trade.id) || !fills.iter().any(|f| f.id == trade.id) {
                                seen_ids.insert(trade.id.clone());
                                pending.remove(&trade.asset_id);
                                matched_assets.remove(&trade.asset_id);
                                // Dedup: replace any existing fill for same asset
                                fills.retain(|f| f.asset_id != trade.asset_id);
                                fills.push(trade);
                            }
                        }
                        _ => {}
                    }
                }
                Ok(UserEvent::Order(order)) => {
                    tracing::debug!("UserChannel order: id={} type={} matched={}",
                        order.id, order.event_subtype, order.size_matched);
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    // If all assets have been MATCHED but not CONFIRMED, accept MATCHED as fills.
                    // ~20% of trades never get CONFIRMED via WS (investigation 2026-03-22).
                    if pending.is_subset(&matched_assets) && !matched_assets.is_empty() {
                        tracing::info!("UserChannel: all {} assets MATCHED but not yet CONFIRMED — promoting to fills",
                            matched_assets.len());
                        for asset in matched_assets.drain() {
                            pending.remove(&asset);
                            if let Some(evt) = matched_events.remove(&asset) {
                                fills.retain(|f| f.asset_id != evt.asset_id);
                                fills.push(evt);
                            }
                        }
                        break;
                    }
                    continue;
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }

        tracing::info!("UserChannel: {} fills confirmed, {} matched-only",
            fills.len(), matched_assets.len());
        fills
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
    api_key: String,
    secret: String,
    passphrase: String,
    market_ids: Vec<String>,
) {
    let mut backoff_ms: u64 = 1000;

    while running.load(Ordering::Relaxed) {
        tracing::info!("UserChannel: connecting to {} ({} markets)...", USER_WS_URL, market_ids.len());

        match user_channel_connect(&running, &shutdown, &sender, &api_key, &secret, &passphrase, &market_ids).await {
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
    api_key: &str,
    secret: &str,
    passphrase: &str,
    market_ids: &[String],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (ws_stream, _) = tokio_tungstenite::connect_async(USER_WS_URL).await?;
    let (mut sink, mut stream) = ws_stream.split();

    tracing::info!("UserChannel: connected, sending auth + subscribe");

    // Subscribe message per Polymarket docs + official rs-clob-client.
    // WS wants raw API credentials (apiKey, secret, passphrase) — NOT HMAC signatures.
    // The secret field is the base64url-encoded API secret, not a computed signature.
    let sub_msg = serde_json::json!({
        "auth": {
            "apiKey": api_key,
            "secret": secret,
            "passphrase": passphrase,
        },
        "markets": market_ids,
        "type": "user",
    });

    sink.send(WsMessage::Text(sub_msg.to_string())).await?;
    tracing::info!("UserChannel: subscribed to {} markets", market_ids.len());

    let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
    let mut last_pong = std::time::Instant::now();
    let pong_timeout = Duration::from_secs(30); // API-6: detect stale connections

    loop {
        tokio::select! {
            biased;

            _ = heartbeat.tick() => {
                // API-6: Check for missed PONGs before sending next PING
                if last_pong.elapsed() > pong_timeout {
                    tracing::warn!("UserChannel: no PONG for {}s, treating as dead",
                        last_pong.elapsed().as_secs());
                    return Err("pong timeout".into());
                }
                if let Err(e) = sink.send(WsMessage::Text("PING".to_string())).await {
                    return Err(Box::new(e));
                }
            }

            msg = stream.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        if crate::ws::is_pong(&text) {
                            last_pong = std::time::Instant::now();
                            continue;
                        }
                        parse_and_dispatch(&text, sender);
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        let _ = sink.send(WsMessage::Pong(data)).await;
                        last_pong = std::time::Instant::now();
                    }
                    Some(Ok(WsMessage::Pong(_))) => {
                        last_pong = std::time::Instant::now();
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
        taker_order_id: parse_str(msg, "taker_order_id"),
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
