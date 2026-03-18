/// WebSocket connection manager — sharded connections to Polymarket.
///
/// Architecture:
///   - N shards, each handling up to `assets_per_shard` assets
///   - Each shard is a tokio task with auto-reconnect + exponential backoff
///   - PING heartbeat every 10s (Polymarket requirement)
///   - Message dispatch → BookMirror → EvalQueue (all in Rust, no GIL)
///
/// Message types handled:
///   - book: full order book snapshot
///   - price_change: bid/ask update
///   - best_bid_ask: lightweight best prices
///   - last_trade_price: ignored (informational only)
///   - market_resolved: resolved events queue for orchestrator
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Notify;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use serde_json::Value;
use crate::book::BookMirror;
use crate::latency::LatencyTracker;
use crate::position::PositionManager;
use crate::queue::EvalQueue;
use crate::types::{BookLevel, EngineConfig};

/// Resolved market event, queued for orchestrator processing.
#[derive(Debug, Clone)]
pub struct ResolvedEvent {
    pub market_cid: String,
    pub asset_id: String,
    pub timestamp: f64,
}

pub struct WsManager {
    config: EngineConfig,
    book: Arc<BookMirror>,
    eval_queue: Arc<EvalQueue>,
    running: Arc<AtomicBool>,
    /// Total WS messages received across all shards.
    total_msgs: Arc<AtomicU64>,
    /// Resolved events accumulator.
    resolved_events: Arc<parking_lot::Mutex<Vec<ResolvedEvent>>>,
    /// Position manager — for direct resolution when market_resolved arrives.
    positions: Arc<parking_lot::Mutex<PositionManager>>,
    /// Assets currently subscribed (for stats).
    subscribed_count: Arc<AtomicU64>,
    /// Notify for shutdown.
    shutdown: Arc<Notify>,
    /// Latency instrumentation tracker.
    latency: Arc<LatencyTracker>,
}

impl WsManager {
    pub fn new(
        config: EngineConfig,
        book: Arc<BookMirror>,
        eval_queue: Arc<EvalQueue>,
        positions: Arc<parking_lot::Mutex<PositionManager>>,
        latency: Arc<LatencyTracker>,
    ) -> Self {
        Self {
            config,
            book,
            eval_queue,
            running: Arc::new(AtomicBool::new(false)),
            total_msgs: Arc::new(AtomicU64::new(0)),
            resolved_events: Arc::new(parking_lot::Mutex::new(Vec::new())),
            positions,
            subscribed_count: Arc::new(AtomicU64::new(0)),
            shutdown: Arc::new(Notify::new()),
            latency,
        }
    }

    /// Start WS shards for the given asset IDs.
    /// Called after constraint detection provides asset lists.
    pub async fn start(&self, all_asset_ids: Vec<String>) {
        self.running.store(true, Ordering::SeqCst);
        let shard_size = self.config.assets_per_shard;
        let chunks: Vec<Vec<String>> = all_asset_ids
            .chunks(shard_size)
            .map(|c| c.to_vec())
            .collect();

        tracing::info!(
            "Starting {} WS shards for {} assets ({} per shard)",
            chunks.len(), all_asset_ids.len(), shard_size
        );

        self.subscribed_count.store(all_asset_ids.len() as u64, Ordering::Relaxed);

        for (shard_id, assets) in chunks.into_iter().enumerate() {
            let book = Arc::clone(&self.book);
            let queue = Arc::clone(&self.eval_queue);
            let running = Arc::clone(&self.running);
            let total_msgs = Arc::clone(&self.total_msgs);
            let resolved = Arc::clone(&self.resolved_events);
            let positions = Arc::clone(&self.positions);
            let shutdown = Arc::clone(&self.shutdown);
            let latency = Arc::clone(&self.latency);
            let ws_url = self.config.ws_url.clone();
            let hb_interval = self.config.heartbeat_interval_secs;

            // Stagger shard connections 150ms apart to avoid thundering herd
            let stagger = Duration::from_millis(150 * shard_id as u64);
            tokio::spawn(async move {
                tokio::time::sleep(stagger).await;
                shard_loop(
                    shard_id, assets, ws_url, hb_interval,
                    book, queue, running, total_msgs, resolved, positions, shutdown,
                    latency,
                ).await;
            });
        }
    }

    /// Stop all WS connections.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.shutdown.notify_waiters();
        tracing::info!("WS manager: stop requested");
    }

    /// Drain resolved market events.
    pub fn drain_resolved(&self) -> Vec<ResolvedEvent> {
        let mut events = self.resolved_events.lock();
        std::mem::take(&mut *events)
    }

    /// Stats for monitoring.
    pub fn stats(&self) -> WsStats {
        WsStats {
            total_msgs: self.total_msgs.load(Ordering::Relaxed),
            subscribed: self.subscribed_count.load(Ordering::Relaxed),
            live_books: self.book.live_count() as u64,
            running: self.running.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WsStats {
    pub total_msgs: u64,
    pub subscribed: u64,
    pub live_books: u64,
    pub running: bool,
}

/// Per-shard connection loop with auto-reconnect.
async fn shard_loop(
    shard_id: usize,
    assets: Vec<String>,
    ws_url: String,
    hb_interval_secs: u64,
    book: Arc<BookMirror>,
    queue: Arc<EvalQueue>,
    running: Arc<AtomicBool>,
    total_msgs: Arc<AtomicU64>,
    resolved: Arc<parking_lot::Mutex<Vec<ResolvedEvent>>>,
    positions: Arc<parking_lot::Mutex<PositionManager>>,
    shutdown: Arc<Notify>,
    latency: Arc<LatencyTracker>,
) {
    let mut backoff_ms: u64 = 1000;
    let max_backoff_ms: u64 = 60_000;

    while running.load(Ordering::Relaxed) {
        tracing::info!("Shard {}: connecting ({} assets)...", shard_id, assets.len());

        let connect_time = std::time::Instant::now();
        match connect_and_run(
            shard_id, &assets, &ws_url, hb_interval_secs,
            &book, &queue, &running, &total_msgs, &resolved, &positions, &shutdown,
            &latency,
        ).await {
            Ok(()) => {
                tracing::info!("Shard {}: clean disconnect", shard_id);
                backoff_ms = 1000;
            }
            Err(e) => {
                let lived_secs = connect_time.elapsed().as_secs();
                // Reset backoff if connection was healthy for >30s — not a rapid-fail loop
                if lived_secs > 30 {
                    backoff_ms = 1000;
                }
                tracing::warn!("Shard {}: error after {}s: {}, reconnecting in {}ms",
                    shard_id, lived_secs, e, backoff_ms);
            }
        }

        if !running.load(Ordering::Relaxed) {
            break;
        }

        // Backoff before reconnect
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(backoff_ms)) => {}
            _ = shutdown.notified() => { break; }
        }
        backoff_ms = (backoff_ms * 2).min(max_backoff_ms);
    }
    tracing::info!("Shard {}: loop ended", shard_id);
}

/// Single connection lifecycle: connect → subscribe → read messages.
async fn connect_and_run(
    shard_id: usize,
    assets: &[String],
    ws_url: &str,
    hb_interval_secs: u64,
    book: &Arc<BookMirror>,
    queue: &Arc<EvalQueue>,
    running: &Arc<AtomicBool>,
    total_msgs: &Arc<AtomicU64>,
    resolved: &Arc<parking_lot::Mutex<Vec<ResolvedEvent>>>,
    positions: &Arc<parking_lot::Mutex<PositionManager>>,
    shutdown: &Arc<Notify>,
    latency: &Arc<LatencyTracker>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (ws_stream, _response) = tokio_tungstenite::connect_async(ws_url).await?;
    let (mut sink, mut stream) = ws_stream.split();

    tracing::info!("Shard {}: connected, subscribing {} assets", shard_id, assets.len());

    // Subscribe — single message (shard size ≤ 400, well under Polymarket's 500 limit)
    let asset_list: Vec<Value> = assets.iter()
        .map(|a| Value::String(a.clone()))
        .collect();
    let sub_msg = serde_json::json!({
        "assets_ids": asset_list,
        "type": "market",
        "custom_feature_enabled": true,
        "initial_dump": true,
    });
    sink.send(WsMessage::Text(sub_msg.to_string())).await?;

    tracing::info!("Shard {}: subscribed {} assets", shard_id, assets.len());

    // Main loop: heartbeat + message read + pong timeout
    let mut heartbeat_interval = tokio::time::interval(
        Duration::from_secs(hb_interval_secs)
    );
    let mut last_pong = std::time::Instant::now();
    let pong_timeout = Duration::from_secs(hb_interval_secs * 3); // 3 missed pongs = dead

    loop {
        tokio::select! {
            // biased: heartbeat MUST fire on time even under heavy message load
            biased;

            // Heartbeat tick — Polymarket expects plain text "PING"
            _ = heartbeat_interval.tick() => {
                // Check if we've received a PONG recently
                if last_pong.elapsed() > pong_timeout {
                    tracing::warn!("Shard {}: no PONG for {}s, treating as dead",
                        shard_id, last_pong.elapsed().as_secs());
                    return Err("pong timeout".into());
                }
                if let Err(e) = sink.send(WsMessage::Text("PING".to_string())).await {
                    tracing::warn!("Shard {}: heartbeat send failed: {}", shard_id, e);
                    return Err(Box::new(e));
                }
            }

            // Incoming message
            msg = stream.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        total_msgs.fetch_add(1, Ordering::Relaxed);
                        // Track PONG responses to our text PING
                        if is_pong(&text) {
                            last_pong = std::time::Instant::now();
                            continue;
                        }
                        handle_message_shared(&format!("Shard {}", shard_id), &text, book, queue, resolved, positions, latency, None);
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        let _ = sink.send(WsMessage::Pong(data)).await;
                        last_pong = std::time::Instant::now(); // server ping = alive
                    }
                    Some(Ok(WsMessage::Pong(_))) => {
                        last_pong = std::time::Instant::now(); // WS-level pong
                    }
                    Some(Ok(WsMessage::Close(_))) => {
                        tracing::info!("Shard {}: server sent Close", shard_id);
                        return Ok(());
                    }
                    Some(Err(e)) => {
                        return Err(Box::new(e));
                    }
                    None => {
                        tracing::info!("Shard {}: stream ended", shard_id);
                        return Ok(());
                    }
                    _ => {} // Binary — ignore
                }
            }

            // Shutdown signal
            _ = shutdown.notified() => {
                tracing::info!("Shard {}: shutdown", shard_id);
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

/// ST1: Shared pong detection — used by both WsManager and ConnectionPool.
pub fn is_pong(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed == "PONG"
        || trimmed.starts_with("{\"type\":\"pong\"")
        || trimmed.starts_with("[{\"type\":\"pong\"")
}

/// Parse and dispatch a single WS message (shared between WsManager and ConnectionPool).
pub fn handle_message_shared(
    label: &str,
    text: &str,
    book: &Arc<BookMirror>,
    queue: &Arc<EvalQueue>,
    resolved: &Arc<parking_lot::Mutex<Vec<ResolvedEvent>>>,
    positions: &Arc<parking_lot::Mutex<PositionManager>>,
    latency: &Arc<LatencyTracker>,
    instruments: Option<&Arc<crate::instrument::InstrumentStore>>,
) {
    // Debug: log first 200 chars of every message for diagnosis
    #[cfg(debug_assertions)]
    tracing::trace!("{} raw msg: {}", label, text.get(..200).unwrap_or(text));

    // Polymarket sends arrays of events
    let messages: Vec<Value> = if text.starts_with('[') {
        match serde_json::from_str(text) {
            Ok(arr) => arr,
            Err(e) => {
                tracing::warn!("{} failed to parse WS array: {}", label, e);
                return;
            }
        }
    } else {
        match serde_json::from_str::<Value>(text) {
            Ok(v) => vec![v],
            Err(e) => {
                tracing::warn!("{} failed to parse WS message: {}", label, e);
                return;
            }
        }
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    for msg in &messages {
        // Extract Polymarket server timestamp (seconds or millis)
        let origin_ts = extract_origin_ts(msg, now);

        // Segment 1: WS network latency (Polymarket server → local receive)
        if origin_ts > 0.0 && latency.is_enabled() {
            let network_us = (now - origin_ts) * 1_000_000.0;
            if network_us > 0.0 && network_us < 60_000_000.0 { // sanity: < 60s
                latency.record_ws_network(network_us);
            }
        }

        let event_type = msg.get("event_type")
            .or_else(|| msg.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match event_type {
            "book" => handle_book(msg, book, queue, now, origin_ts, latency),
            "price_change" => handle_price_change(msg, book, queue, now, origin_ts, latency),
            "best_bid_ask" => handle_best_bid_ask(msg, book, queue, now, origin_ts, latency),
            "market_resolved" => handle_resolved(msg, resolved, positions, now),
            "tick_size_change" => {
                // B2.4: Dynamic tick size handling — update InstrumentStore precision.
                if let (Some(asset), Some(new_ts)) = (
                    msg.get("asset_id").and_then(|v| v.as_str()),
                    msg.get("tick_size").and_then(|v| v.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| v.as_f64())),
                ) {
                    if let Some(store) = instruments {
                        if store.update_tick_size(asset, new_ts) {
                            tracing::info!("B2.4 tick_size_change: asset={} new_tick_size={} — instrument updated", asset, new_ts);
                        } else {
                            tracing::warn!("B2.4 tick_size_change: asset={} new_tick_size={} — unknown token", asset, new_ts);
                        }
                    } else {
                        tracing::info!("WS tick_size_change: asset={} new_tick_size={} (no instrument store)", asset, new_ts);
                    }
                }
            }
            "last_trade_price" | "pong" | "" => {} // ignore
            _ => {
                tracing::trace!("Unknown event_type: '{}' in: {}", event_type, &text[..text.len().min(150)]);
            }
        }
    }
}

/// Handle full book snapshot.
fn handle_book(
    msg: &Value, book: &Arc<BookMirror>, queue: &Arc<EvalQueue>, ts: f64,
    origin_ts: f64, latency: &Arc<LatencyTracker>,
) {
    let asset_id = match msg.get("asset_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return,
    };

    let parse_levels = |key: &str| -> Vec<BookLevel> {
        msg.get(key)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter().filter_map(|level| {
                    let price = level.get("price")
                        .and_then(|v| v.as_str().or_else(|| v.as_f64().map(|_| "")))?;
                    let size = level.get("size")
                        .and_then(|v| v.as_str().or_else(|| v.as_f64().map(|_| "")))?;
                    let p = if price.is_empty() {
                        level.get("price").and_then(|v| v.as_f64())?
                    } else {
                        price.parse::<f64>().ok()?
                    };
                    let s = if size.is_empty() {
                        level.get("size").and_then(|v| v.as_f64())?
                    } else {
                        size.parse::<f64>().ok()?
                    };
                    Some(BookLevel { price: p, size: s })
                }).collect()
            })
            .unwrap_or_default()
    };

    let asks = parse_levels("asks");
    let bids = parse_levels("bids");
    let evals = book.apply_snapshot(asset_id, asks, bids, ts);
    for (cid, urgent) in evals {
        queue.push(&cid, asset_id, urgent, ts, origin_ts);
    }
    // Segment 2: WS handler → queue push
    if latency.is_enabled() {
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64();
        latency.record_ws_to_queue((after - ts) * 1_000_000.0);
    }
}

/// Handle price_change event (has price, best bid/ask fields).
fn handle_price_change(
    msg: &Value, book: &Arc<BookMirror>, queue: &Arc<EvalQueue>, ts: f64,
    origin_ts: f64, latency: &Arc<LatencyTracker>,
) {
    let asset_id = match msg.get("asset_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return,
    };

    // Extract changes array if present (delta updates)
    if let Some(changes) = msg.get("changes").and_then(|v| v.as_array()) {
        for change in changes {
            let price = parse_f64_field(change, "price").unwrap_or(0.0);
            let size = parse_f64_field(change, "size").unwrap_or(0.0);
            let side = change.get("side").and_then(|v| v.as_str()).unwrap_or("");
            let is_ask = side == "SELL" || side == "sell" || side == "ask";
            let evals = book.apply_delta(asset_id, is_ask, price, size, ts);
            for (cid, urgent) in evals {
                queue.push(&cid, asset_id, urgent, ts, origin_ts);
            }
        }
        // Segment 2: WS handler → queue push
        if latency.is_enabled() {
            let after = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64();
            latency.record_ws_to_queue((after - ts) * 1_000_000.0);
        }
        return;
    }

    // Fallback: use best_bid / best_ask fields
    let bid = parse_f64_field(msg, "best_bid").unwrap_or(0.0);
    let ask = parse_f64_field(msg, "best_ask").unwrap_or(0.0);
    if ask > 0.0 || bid > 0.0 {
        let evals = book.apply_best_prices(asset_id, bid, ask, ts);
        for (cid, urgent) in evals {
            queue.push(&cid, asset_id, urgent, ts, origin_ts);
        }
        // Segment 2
        if latency.is_enabled() {
            let after = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64();
            latency.record_ws_to_queue((after - ts) * 1_000_000.0);
        }
    }
}

/// Handle best_bid_ask event.
fn handle_best_bid_ask(
    msg: &Value, book: &Arc<BookMirror>, queue: &Arc<EvalQueue>, ts: f64,
    origin_ts: f64, latency: &Arc<LatencyTracker>,
) {
    let asset_id = match msg.get("asset_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return,
    };
    let bid = parse_f64_field(msg, "best_bid").unwrap_or(0.0);
    let ask = parse_f64_field(msg, "best_ask").unwrap_or(0.0);
    if ask > 0.0 || bid > 0.0 {
        let evals = book.apply_best_prices(asset_id, bid, ask, ts);
        for (cid, urgent) in evals {
            queue.push(&cid, asset_id, urgent, ts, origin_ts);
        }
        // Segment 2
        if latency.is_enabled() {
            let after = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64();
            latency.record_ws_to_queue((after - ts) * 1_000_000.0);
        }
    }
}

/// Handle market_resolved event.
/// Accumulates the event, then attempts to resolve any affected positions directly.
fn handle_resolved(
    msg: &Value,
    resolved: &Arc<parking_lot::Mutex<Vec<ResolvedEvent>>>,
    positions: &Arc<parking_lot::Mutex<PositionManager>>,
    ts: f64,
) {
    let asset_id = msg.get("asset_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let market_cid = msg.get("condition_id")
        .or_else(|| msg.get("market_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if asset_id.is_empty() && market_cid.is_empty() {
        return;
    }

    tracing::info!("Market resolved: cid={}, asset={}", market_cid, asset_id);

    // Accumulate event
    let events = {
        let mut queue = resolved.lock();
        queue.push(ResolvedEvent {
            market_cid: market_cid.to_string(),
            asset_id: asset_id.to_string(),
            timestamp: ts,
        });
        // Snapshot all accumulated events for resolution attempt
        queue.iter()
            .map(|e| (e.market_cid.clone(), e.asset_id.clone()))
            .collect::<Vec<_>>()
    };

    // Try to resolve positions using ALL accumulated events.
    // Single outer lock provides all synchronization (no inner lock in PositionManager).
    let mut pm = positions.lock();
    if pm.open_count() == 0 {
        return;
    }
    let resolved_positions = pm.resolve_by_ws_events(&events);
    for (pid, winning_mid) in &resolved_positions {
        if let Some(res) = pm.close_on_resolution(pid, winning_mid) {
            tracing::info!(
                "RESOLVED {}: payout=${:.2} profit=${:.2}",
                &pid[..pid.len().min(40)], res.payout, res.profit
            );
        }
    }
    drop(pm);

    // Always clear after processing — events for unrelated markets shouldn't accumulate
    resolved.lock().clear();
}

/// Parse a JSON field that might be string or number.
pub fn parse_f64_field(val: &Value, key: &str) -> Option<f64> {
    val.get(key).and_then(|v| {
        v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
    })
}

/// Extract Polymarket server timestamp from a WS message.
/// Tries "timestamp" field — may be seconds, millis, or ISO string.
/// Returns 0.0 if not available.
pub fn extract_origin_ts(msg: &Value, now: f64) -> f64 {
    if let Some(ts) = parse_f64_field(msg, "timestamp") {
        // Polymarket sends millis (13-digit) or seconds (10-digit)
        if ts > 1_000_000_000_000.0 {
            ts / 1000.0  // millis → seconds
        } else if ts > 1_000_000_000.0 {
            ts  // already seconds
        } else {
            0.0  // too small, probably not a timestamp
        }
    } else if let Some(ts_str) = msg.get("timestamp").and_then(|v| v.as_str()) {
        // Try ISO 8601 parse
        chrono::DateTime::parse_from_rfc3339(ts_str)
            .map(|dt| dt.timestamp_millis() as f64 / 1000.0)
            .unwrap_or(0.0)
    } else {
        // No server timestamp — fall back to local receive time
        // (segment 1 won't be measured, but origin_ts carries through for e2e)
        now
    }
}
