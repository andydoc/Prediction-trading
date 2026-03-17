/// Tier C — Trade & Command WebSocket connection.
///
/// Single dedicated connection (max 450 assets) for:
///   - Open position asset monitoring (book/price updates)
///   - New market event detection (global broadcast via custom_feature_enabled)
///   - Market resolved event detection (instant resolution)
///
/// New market events are buffered for 2.5s (configurable) to collect full bursts
/// (all outcomes in an event) before evaluating constraint potential.
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use parking_lot::Mutex;
use serde_json::Value;

use crate::book::BookMirror;
use crate::latency::LatencyTracker;
use crate::position::PositionManager;
use crate::queue::EvalQueue;
use crate::ws::ResolvedEvent;
use crate::ws_pool::{ConnectionPool, PoolConfig, PoolStats, WsTier};

/// A new market event received via WS.
#[derive(Debug, Clone)]
pub struct NewMarketEvent {
    /// Polymarket condition ID (market ID).
    pub condition_id: String,
    /// Human-readable question.
    pub question: String,
    /// Asset IDs for this market (typically YES and NO tokens).
    pub asset_ids: Vec<String>,
    /// Outcome labels (e.g., ["Yes", "No"]).
    pub outcomes: Vec<String>,
    /// Parent event ID — markets in the same event form potential constraints.
    pub event_id: String,
    /// Event title.
    pub event_title: String,
    /// Raw timestamp from Polymarket.
    pub timestamp: String,
}

/// A buffered burst of new markets, ready for constraint evaluation.
#[derive(Debug, Clone)]
pub struct NewMarketBurst {
    /// All new market events grouped by event_id.
    pub markets: Vec<NewMarketEvent>,
    /// The shared event_id.
    pub event_id: String,
    /// The event title.
    pub event_title: String,
}

/// Configuration for Tier C.
#[derive(Debug, Clone)]
pub struct TierCConfig {
    pub ws_url: String,
    pub heartbeat_interval_secs: u64,
    pub max_assets_per_connection: usize,
    pub new_market_buffer_secs: f64,
    pub stagger_ms: u64,
}

/// Tier C manager — wraps a single ConnectionPool connection.
pub struct TierC {
    pool: ConnectionPool,
    /// Assets currently subscribed for open position monitoring.
    position_assets: Mutex<HashSet<String>>,
    /// Buffered new market events, keyed by event_id.
    new_market_buffer: Mutex<Vec<NewMarketEvent>>,
    /// Timestamp (secs since epoch) when buffer started filling.
    buffer_start: Mutex<Option<f64>>,
    /// How long to wait for a burst to complete (seconds).
    buffer_timeout_secs: f64,
    /// Flushed bursts ready for the orchestrator to consume.
    ready_bursts: Mutex<Vec<NewMarketBurst>>,
}

impl TierC {
    pub fn new(
        config: TierCConfig,
        book: Arc<BookMirror>,
        eval_queue: Arc<EvalQueue>,
        resolved_events: Arc<Mutex<Vec<ResolvedEvent>>>,
        positions: Arc<Mutex<PositionManager>>,
        latency: Arc<LatencyTracker>,
        instruments: Option<Arc<crate::instrument::InstrumentStore>>,
    ) -> Self {
        let pool_config = PoolConfig {
            ws_url: config.ws_url,
            max_assets_per_connection: config.max_assets_per_connection,
            max_connections: 1, // Tier C is always 1 connection
            heartbeat_interval_secs: config.heartbeat_interval_secs,
            custom_features: true, // Required for new_market + market_resolved
            stagger_ms: config.stagger_ms,
        };

        let pool = ConnectionPool::new(
            WsTier::C,
            pool_config,
            book,
            eval_queue,
            resolved_events,
            positions,
            latency,
            instruments,
        );

        Self {
            pool,
            position_assets: Mutex::new(HashSet::new()),
            new_market_buffer: Mutex::new(Vec::new()),
            buffer_start: Mutex::new(None),
            buffer_timeout_secs: config.new_market_buffer_secs,
            ready_bursts: Mutex::new(Vec::new()),
        }
    }

    /// Start Tier C with initial position assets.
    pub fn start(&self, position_asset_ids: Vec<String>, rt: &tokio::runtime::Handle) {
        *self.position_assets.lock() = position_asset_ids.iter().cloned().collect();
        self.pool.start(position_asset_ids, rt);
    }

    /// Add assets when a new position is entered.
    /// Called after entry: subscribe on C, then caller unsubscribes from B.
    pub fn add_position_assets(&self, asset_ids: Vec<String>) {
        {
            let mut pa = self.position_assets.lock();
            for id in &asset_ids {
                pa.insert(id.clone());
            }
        }
        self.pool.subscribe(asset_ids);
        tracing::info!("Tier C: added {} position assets (total: {})",
            self.position_assets.lock().len(),
            self.pool.stats.assets_subscribed.load(Ordering::Relaxed));
    }

    /// Remove assets when a position is resolved or exited.
    /// Returns the removed asset IDs for the caller to optionally re-add to Tier B.
    pub fn remove_position_assets(&self, asset_ids: Vec<String>) -> Vec<String> {
        let removed: Vec<String> = {
            let mut pa = self.position_assets.lock();
            asset_ids.iter()
                .filter(|id| pa.remove(*id))
                .cloned()
                .collect()
        };
        if !removed.is_empty() {
            self.pool.unsubscribe(removed.clone());
            tracing::info!("Tier C: removed {} position assets (remaining: {})",
                removed.len(),
                self.position_assets.lock().len());
        }
        removed
    }

    /// Buffer a new market event. Call this from the message handler
    /// when `event_type == "new_market"` is received.
    pub fn buffer_new_market(&self, event: NewMarketEvent) {
        let mut buf = self.new_market_buffer.lock();
        let mut start = self.buffer_start.lock();

        if start.is_none() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            *start = Some(now);
        }

        tracing::info!("Tier C: new market event: {} (event: {})",
            event.question, event.event_id);
        buf.push(event);
    }

    /// Check if the buffer should be flushed (called periodically from orchestrator tick).
    /// If enough time has passed since the first event in the buffer, flush and
    /// group by event_id into bursts.
    pub fn flush_if_ready(&self) -> Vec<NewMarketBurst> {
        let start = *self.buffer_start.lock();
        let start = match start {
            Some(s) => s,
            None => return Vec::new(),
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        if now - start < self.buffer_timeout_secs {
            return Vec::new(); // Not ready yet
        }

        // Flush
        let events: Vec<NewMarketEvent> = {
            let mut buf = self.new_market_buffer.lock();
            *self.buffer_start.lock() = None;
            std::mem::take(&mut *buf)
        };

        if events.is_empty() {
            return Vec::new();
        }

        // Group by event_id
        let mut groups: std::collections::HashMap<String, Vec<NewMarketEvent>> =
            std::collections::HashMap::new();
        for evt in events {
            groups.entry(evt.event_id.clone()).or_default().push(evt);
        }

        let bursts: Vec<NewMarketBurst> = groups.into_iter()
            .map(|(event_id, markets)| {
                let event_title = markets.first()
                    .map(|m| m.event_title.clone())
                    .unwrap_or_default();
                NewMarketBurst { markets, event_id, event_title }
            })
            .collect();

        tracing::info!("Tier C: flushed {} new market bursts ({} total markets)",
            bursts.len(),
            bursts.iter().map(|b| b.markets.len()).sum::<usize>());

        // Also store in ready_bursts for drain
        {
            let mut ready = self.ready_bursts.lock();
            ready.extend(bursts.clone());
        }

        bursts
    }

    /// Drain ready bursts (called by orchestrator).
    pub fn drain_new_market_bursts(&self) -> Vec<NewMarketBurst> {
        std::mem::take(&mut *self.ready_bursts.lock())
    }

    /// Get current position asset count.
    pub fn position_asset_count(&self) -> usize {
        self.position_assets.lock().len()
    }

    /// Get pool stats.
    pub fn stats(&self) -> &PoolStats {
        &self.pool.stats
    }

    /// Stop Tier C.
    pub fn stop(&self) {
        self.pool.stop();
    }
}

/// Parse a `new_market` event from Polymarket WS JSON into a NewMarketEvent.
/// Returns None if required fields are missing.
///
/// Expected format:
/// ```json
/// {
///   "event_type": "new_market",
///   "id": "condition_id",
///   "question": "Will X happen?",
///   "market": "condition_id",
///   "assets_ids": ["0x...", "0x..."],
///   "outcomes": ["Yes", "No"],
///   "event_message": {
///     "id": "event_id",
///     "title": "Event Title"
///   },
///   "timestamp": "2026-03-17T..."
/// }
/// ```
pub fn parse_new_market_event(msg: &Value) -> Option<NewMarketEvent> {
    let condition_id = msg.get("id")
        .or_else(|| msg.get("market"))
        .and_then(|v| v.as_str())?
        .to_string();

    let question = msg.get("question")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let asset_ids: Vec<String> = msg.get("assets_ids")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect())
        .unwrap_or_default();

    let outcomes: Vec<String> = msg.get("outcomes")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect())
        .unwrap_or_default();

    let event_msg = msg.get("event_message");
    let event_id = event_msg
        .and_then(|e| e.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let event_title = event_msg
        .and_then(|e| e.get("title"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let timestamp = msg.get("timestamp")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if asset_ids.is_empty() {
        return None;
    }

    Some(NewMarketEvent {
        condition_id,
        question,
        asset_ids,
        outcomes,
        event_id,
        event_title,
        timestamp,
    })
}

/// Parse an enhanced `market_resolved` event from Polymarket WS JSON.
/// The documented format includes `winning_asset_id` and `winning_outcome`.
///
/// Expected format:
/// ```json
/// {
///   "event_type": "market_resolved",
///   "id": "condition_id",
///   "market": "condition_id",
///   "assets_ids": ["0x...", "0x..."],
///   "winning_asset_id": "0x...",
///   "winning_outcome": "Yes",
///   "event_message": { "id": "event_id", "title": "..." },
///   "timestamp": "2026-03-17T..."
/// }
/// ```
pub fn parse_market_resolved_event(msg: &Value) -> Option<ResolvedEvent> {
    let market_cid = msg.get("id")
        .or_else(|| msg.get("market"))
        .or_else(|| msg.get("condition_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let asset_id = msg.get("winning_asset_id")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            // Fallback to old format
            msg.get("asset_id").and_then(|v| v.as_str()).unwrap_or("")
        });

    if market_cid.is_empty() && asset_id.is_empty() {
        return None;
    }

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    Some(ResolvedEvent {
        market_cid: market_cid.to_string(),
        asset_id: asset_id.to_string(),
        timestamp: ts,
    })
}
