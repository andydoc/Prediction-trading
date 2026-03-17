/// Long-lived WebSocket connection pool with dynamic subscribe/unsubscribe.
///
/// Each connection is a persistent tokio task that:
///   - Connects once and stays open (auto-reconnects on error)
///   - Receives SubCommand messages to add/remove subscriptions without reconnecting
///   - Sends PING every heartbeat_interval_secs (biased priority in select!)
///   - Routes incoming messages to BookMirror/EvalQueue/resolved handlers
///
/// The pool distributes assets across connections, keeping each under max_assets_per_connection.
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::book::BookMirror;
use crate::instrument::InstrumentStore;
use crate::latency::LatencyTracker;
use crate::position::PositionManager;
use crate::queue::EvalQueue;
use crate::ws::{ResolvedEvent};

// Re-export for tier modules
pub use crate::ws::{handle_message_shared, extract_origin_ts, parse_f64_field};

/// Command sent to a managed connection to add/remove subscriptions.
#[derive(Debug)]
pub enum SubCommand {
    /// Subscribe to additional asset IDs.
    Subscribe(Vec<String>),
    /// Unsubscribe from asset IDs.
    Unsubscribe(Vec<String>),
}

/// Tier label for logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsTier {
    B,
    C,
}

impl std::fmt::Display for WsTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WsTier::B => write!(f, "B"),
            WsTier::C => write!(f, "C"),
        }
    }
}

/// Configuration for a connection pool.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub ws_url: String,
    pub max_assets_per_connection: usize,
    pub max_connections: usize,
    pub heartbeat_interval_secs: u64,
    pub custom_features: bool,
    pub stagger_ms: u64,
}

/// Per-connection tracking.
struct ManagedConnection {
    _handle: JoinHandle<()>,
    subscribed: Arc<Mutex<HashSet<String>>>,
    cmd_tx: mpsc::UnboundedSender<SubCommand>,
}

/// Stats exposed by the pool.
#[derive(Debug, Clone, Default)]
pub struct PoolStats {
    pub connections_active: Arc<AtomicU32>,
    pub assets_subscribed: Arc<AtomicU64>,
    pub msgs_received: Arc<AtomicU64>,
}

/// A pool of long-lived WebSocket connections with dynamic subscription management.
pub struct ConnectionPool {
    tier: WsTier,
    config: PoolConfig,
    connections: Mutex<Vec<ManagedConnection>>,
    running: Arc<AtomicBool>,
    shutdown: Arc<Notify>,
    /// Stored runtime handle from start() — used for dynamic connection spawns.
    rt_handle: Mutex<Option<tokio::runtime::Handle>>,
    // Shared engine state
    book: Arc<BookMirror>,
    eval_queue: Arc<EvalQueue>,
    resolved_events: Arc<Mutex<Vec<ResolvedEvent>>>,
    positions: Arc<Mutex<PositionManager>>,
    latency: Arc<LatencyTracker>,
    instruments: Option<Arc<InstrumentStore>>,
    pub stats: PoolStats,
}

impl ConnectionPool {
    pub fn new(
        tier: WsTier,
        config: PoolConfig,
        book: Arc<BookMirror>,
        eval_queue: Arc<EvalQueue>,
        resolved_events: Arc<Mutex<Vec<ResolvedEvent>>>,
        positions: Arc<Mutex<PositionManager>>,
        latency: Arc<LatencyTracker>,
        instruments: Option<Arc<InstrumentStore>>,
    ) -> Self {
        Self {
            tier,
            config,
            connections: Mutex::new(Vec::new()),
            running: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(Notify::new()),
            rt_handle: Mutex::new(None),
            book,
            eval_queue,
            resolved_events,
            positions,
            latency,
            instruments,
            stats: PoolStats::default(),
        }
    }

    /// Start the pool with an initial set of asset IDs.
    /// Distributes assets across connections (up to max_assets_per_connection each).
    pub fn start(&self, initial_assets: Vec<String>, rt: &tokio::runtime::Handle) {
        self.running.store(true, Ordering::SeqCst);
        *self.rt_handle.lock() = Some(rt.clone());

        if initial_assets.is_empty() {
            // Start one warm connection with no subscriptions (for Tier C: new market events)
            self.spawn_connection(0, Vec::new(), rt);
            tracing::info!("Tier {}: started 1 warm connection (no initial assets)", self.tier);
            return;
        }

        let chunk_size = self.config.max_assets_per_connection;
        let chunks: Vec<Vec<String>> = initial_assets
            .chunks(chunk_size)
            .map(|c| c.to_vec())
            .collect();

        let n_conns = chunks.len().min(self.config.max_connections);
        tracing::info!(
            "Tier {}: starting {} connections for {} assets ({} per conn, max {})",
            self.tier, n_conns, initial_assets.len(), chunk_size, self.config.max_connections,
        );

        self.stats.assets_subscribed.store(initial_assets.len() as u64, Ordering::Relaxed);

        for (idx, assets) in chunks.into_iter().enumerate().take(self.config.max_connections) {
            self.spawn_connection(idx, assets, rt);
        }
    }

    /// Subscribe to additional asset IDs. Distributes across connections with capacity.
    /// Spawns new connections dynamically if existing ones are full.
    pub fn subscribe(&self, asset_ids: Vec<String>) {
        let rt = self.rt_handle.lock().clone();
        self.subscribe_with_rt(asset_ids, rt.as_ref());
    }

    /// Subscribe with an explicit runtime handle (used during start when not in async context).
    pub fn subscribe_with_rt(&self, asset_ids: Vec<String>, rt: Option<&tokio::runtime::Handle>) {
        if asset_ids.is_empty() {
            return;
        }

        let mut remaining = asset_ids;

        // Phase 1: fit into existing connections
        {
            let conns = self.connections.lock();
            for conn in conns.iter() {
                if remaining.is_empty() {
                    break;
                }

                let current_count = conn.subscribed.lock().len();
                let capacity = self.config.max_assets_per_connection.saturating_sub(current_count);
                if capacity == 0 {
                    continue;
                }

                let batch_size = remaining.len().min(capacity);
                let batch: Vec<String> = remaining.drain(..batch_size).collect();

                // Update local tracking
                {
                    let mut sub = conn.subscribed.lock();
                    for id in &batch {
                        sub.insert(id.clone());
                    }
                }

                // Send subscribe command to the connection task
                let _ = conn.cmd_tx.send(SubCommand::Subscribe(batch));
            }
        } // Drop conns lock

        // Phase 2: spawn new connections for overflow
        while !remaining.is_empty() {
            let n_conns = self.connections.lock().len();
            if n_conns >= self.config.max_connections {
                tracing::warn!(
                    "Tier {}: {} assets could not be subscribed (hit max_connections={})",
                    self.tier, remaining.len(), self.config.max_connections,
                );
                break;
            }

            let batch_size = remaining.len().min(self.config.max_assets_per_connection);
            let batch: Vec<String> = remaining.drain(..batch_size).collect();

            tracing::info!("Tier {}: spawning connection {} for {} overflow assets",
                self.tier, n_conns, batch.len());

            if let Some(handle) = rt {
                self.spawn_connection(n_conns, batch, handle);
            } else if let Ok(handle) = tokio::runtime::Handle::try_current() {
                self.spawn_connection(n_conns, batch, &handle);
            } else {
                tracing::warn!("Tier {}: no tokio runtime for dynamic connection spawn", self.tier);
                break;
            }
        }

        // Update stats
        let total: u64 = self.connections.lock().iter()
            .map(|c| c.subscribed.lock().len() as u64)
            .sum();
        self.stats.assets_subscribed.store(total, Ordering::Relaxed);
    }

    /// Unsubscribe from asset IDs. Finds which connection owns each and sends unsubscribe.
    pub fn unsubscribe(&self, asset_ids: Vec<String>) {
        if asset_ids.is_empty() {
            return;
        }

        let conns = self.connections.lock();
        let to_remove: HashSet<&String> = asset_ids.iter().collect();

        // Group removals by connection
        let mut conn_removals: HashMap<usize, Vec<String>> = HashMap::new();
        for (idx, conn) in conns.iter().enumerate() {
            let sub = conn.subscribed.lock();
            for id in &to_remove {
                if sub.contains(*id) {
                    conn_removals.entry(idx).or_default().push((*id).clone());
                }
            }
        }

        // Send unsubscribe commands and update tracking
        for (idx, ids) in conn_removals {
            {
                let mut sub = conns[idx].subscribed.lock();
                for id in &ids {
                    sub.remove(id);
                }
            }
            let _ = conns[idx].cmd_tx.send(SubCommand::Unsubscribe(ids));
        }

        // Update stats
        let total: u64 = conns.iter()
            .map(|c| c.subscribed.lock().len() as u64)
            .sum();
        self.stats.assets_subscribed.store(total, Ordering::Relaxed);
    }

    /// Update subscriptions to match a new target set. Computes diff and sends
    /// incremental subscribe/unsubscribe messages. No reconnections.
    pub fn update_subscriptions(&self, new_assets: HashSet<String>) {
        let conns = self.connections.lock();

        // Collect current set across all connections
        let mut current: HashSet<String> = HashSet::new();
        for conn in conns.iter() {
            let sub = conn.subscribed.lock();
            current.extend(sub.iter().cloned());
        }

        let to_add: Vec<String> = new_assets.difference(&current).cloned().collect();
        let to_remove: Vec<String> = current.difference(&new_assets).cloned().collect();

        drop(conns); // Release lock before calling subscribe/unsubscribe

        if !to_add.is_empty() {
            tracing::info!("Tier {}: subscribing {} new assets", self.tier, to_add.len());
            self.subscribe(to_add);
        }
        if !to_remove.is_empty() {
            tracing::info!("Tier {}: unsubscribing {} stale assets", self.tier, to_remove.len());
            self.unsubscribe(to_remove);
        }
    }

    /// Get the current set of subscribed assets across all connections.
    pub fn subscribed_assets(&self) -> HashSet<String> {
        let conns = self.connections.lock();
        let mut all = HashSet::new();
        for conn in conns.iter() {
            all.extend(conn.subscribed.lock().iter().cloned());
        }
        all
    }

    /// Get number of active connections.
    pub fn connection_count(&self) -> u32 {
        self.stats.connections_active.load(Ordering::Relaxed)
    }

    /// Consolidate connections if underutilised.
    /// If total subscriptions < (num_conns - 1) * consolidation_threshold,
    /// redistribute assets into fewer connections and shut down empties.
    /// Call this periodically (e.g., hourly from the orchestrator).
    pub fn consolidate(&self, threshold_per_conn: usize) {
        let conns = self.connections.lock();
        let n_conns = conns.len();
        if n_conns <= 1 {
            return;
        }

        let total_subs: usize = conns.iter()
            .map(|c| c.subscribed.lock().len())
            .sum();

        let needed = (total_subs + self.config.max_assets_per_connection - 1)
            / self.config.max_assets_per_connection.max(1);
        let needed = needed.max(1); // always keep at least 1

        if needed >= n_conns || total_subs >= (n_conns - 1) * threshold_per_conn {
            return; // Not underutilised enough to bother
        }

        tracing::info!(
            "Tier {}: consolidating {} conns → {} (total {} assets, threshold {})",
            self.tier, n_conns, needed, total_subs, threshold_per_conn,
        );

        // Collect all assets and which connection they're on
        let mut all_assets: Vec<String> = Vec::with_capacity(total_subs);
        for conn in conns.iter() {
            all_assets.extend(conn.subscribed.lock().iter().cloned());
        }

        // Unsubscribe everything from connections that will be emptied
        for (idx, conn) in conns.iter().enumerate() {
            if idx < needed {
                continue; // Keep this connection
            }
            let assets: Vec<String> = conn.subscribed.lock().drain().collect();
            if !assets.is_empty() {
                let _ = conn.cmd_tx.send(SubCommand::Unsubscribe(assets));
            }
        }

        // Redistribute into the first `needed` connections
        let chunks: Vec<Vec<String>> = all_assets
            .chunks(self.config.max_assets_per_connection)
            .map(|c| c.to_vec())
            .collect();

        for (idx, chunk) in chunks.into_iter().enumerate().take(needed) {
            if idx >= conns.len() {
                break;
            }
            let current: HashSet<String> = conns[idx].subscribed.lock().clone();
            let new_set: HashSet<String> = chunk.iter().cloned().collect();

            let to_add: Vec<String> = new_set.difference(&current).cloned().collect();
            let to_remove: Vec<String> = current.difference(&new_set).cloned().collect();

            if !to_remove.is_empty() {
                {
                    let mut sub = conns[idx].subscribed.lock();
                    for id in &to_remove { sub.remove(id); }
                }
                let _ = conns[idx].cmd_tx.send(SubCommand::Unsubscribe(to_remove));
            }
            if !to_add.is_empty() {
                {
                    let mut sub = conns[idx].subscribed.lock();
                    for id in &to_add { sub.insert(id.clone()); }
                }
                let _ = conns[idx].cmd_tx.send(SubCommand::Subscribe(to_add));
            }
        }

        // Update stats
        let new_total: u64 = conns.iter()
            .map(|c| c.subscribed.lock().len() as u64)
            .sum();
        self.stats.assets_subscribed.store(new_total, Ordering::Relaxed);
    }

    /// Stop all connections gracefully.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.shutdown.notify_waiters();
        tracing::info!("Tier {}: stop requested", self.tier);
    }

    /// Spawn a single managed connection task.
    fn spawn_connection(&self, idx: usize, initial_assets: Vec<String>, rt: &tokio::runtime::Handle) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let subscribed = Arc::new(Mutex::new(initial_assets.iter().cloned().collect::<HashSet<_>>()));

        let tier = self.tier;
        let config = self.config.clone();
        let running = Arc::clone(&self.running);
        let shutdown = Arc::clone(&self.shutdown);
        let book = Arc::clone(&self.book);
        let eval_queue = Arc::clone(&self.eval_queue);
        let resolved = Arc::clone(&self.resolved_events);
        let positions = Arc::clone(&self.positions);
        let latency = Arc::clone(&self.latency);
        let instruments = self.instruments.clone();
        let stats = self.stats.clone();
        let sub_clone = Arc::clone(&subscribed);
        let stagger = Duration::from_millis(self.config.stagger_ms * idx as u64);

        let handle = rt.spawn(async move {
            tokio::time::sleep(stagger).await;
            connection_loop(
                tier, idx, config, cmd_rx, sub_clone,
                book, eval_queue, resolved, positions, latency, instruments,
                running, shutdown, stats,
            ).await;
        });

        self.connections.lock().push(ManagedConnection {
            _handle: handle,
            subscribed,
            cmd_tx,
        });
    }
}

/// Long-lived connection loop. Connects, subscribes, handles messages,
/// processes subscribe/unsubscribe commands, and auto-reconnects on error.
async fn connection_loop(
    tier: WsTier,
    idx: usize,
    config: PoolConfig,
    mut cmd_rx: mpsc::UnboundedReceiver<SubCommand>,
    subscribed: Arc<Mutex<HashSet<String>>>,
    book: Arc<BookMirror>,
    eval_queue: Arc<EvalQueue>,
    resolved: Arc<Mutex<Vec<ResolvedEvent>>>,
    positions: Arc<Mutex<PositionManager>>,
    latency: Arc<LatencyTracker>,
    instruments: Option<Arc<InstrumentStore>>,
    running: Arc<AtomicBool>,
    shutdown: Arc<Notify>,
    stats: PoolStats,
) {
    let label = format!("{}:{}", tier, idx);
    let mut backoff_ms: u64 = 1000;
    let max_backoff_ms: u64 = 60_000;

    while running.load(Ordering::Relaxed) {
        let asset_count = subscribed.lock().len();
        tracing::info!("{}: connecting ({} assets)...", label, asset_count);

        stats.connections_active.fetch_add(1, Ordering::Relaxed);
        let connect_time = std::time::Instant::now();

        match connection_session(
            &label, &config, &mut cmd_rx, &subscribed,
            &book, &eval_queue, &resolved, &positions, &latency,
            &instruments,
            &running, &shutdown, &stats,
        ).await {
            Ok(()) => {
                tracing::info!("{}: clean disconnect", label);
                backoff_ms = 1000;
            }
            Err(e) => {
                let lived_secs = connect_time.elapsed().as_secs();
                if lived_secs > 30 {
                    backoff_ms = 1000;
                }
                tracing::warn!("{}: error after {}s: {}, reconnecting in {}ms",
                    label, lived_secs, e, backoff_ms);
            }
        }

        stats.connections_active.fetch_sub(1, Ordering::Relaxed);

        if !running.load(Ordering::Relaxed) {
            break;
        }

        // Drain any commands that arrived while disconnected (so they take effect on reconnect)
        while let Ok(cmd) = cmd_rx.try_recv() {
            let mut sub = subscribed.lock();
            match cmd {
                SubCommand::Subscribe(ids) => { for id in ids { sub.insert(id); } }
                SubCommand::Unsubscribe(ids) => { for id in &ids { sub.remove(id); } }
            }
        }

        // Jitter: spread reconnects to avoid stampede when Polymarket mass-disconnects
        let jitter_ms = {
            let base_jitter = (backoff_ms as f64 * 0.5 * rand::random::<f64>()) as u64;
            let stagger = idx as u64 * config.stagger_ms;
            base_jitter + stagger
        };
        let delay_ms = backoff_ms + jitter_ms;
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
            _ = shutdown.notified() => { break; }
        }
        backoff_ms = (backoff_ms * 2).min(max_backoff_ms);
    }

    tracing::info!("{}: loop ended", label);
}

/// Single connection session: connect → subscribe → message loop.
/// Returns Ok(()) on clean close, Err on error (triggers reconnect).
async fn connection_session(
    label: &str,
    config: &PoolConfig,
    cmd_rx: &mut mpsc::UnboundedReceiver<SubCommand>,
    subscribed: &Arc<Mutex<HashSet<String>>>,
    book: &Arc<BookMirror>,
    eval_queue: &Arc<EvalQueue>,
    resolved: &Arc<Mutex<Vec<ResolvedEvent>>>,
    positions: &Arc<Mutex<PositionManager>>,
    latency: &Arc<LatencyTracker>,
    instruments: &Option<Arc<InstrumentStore>>,
    running: &Arc<AtomicBool>,
    shutdown: &Arc<Notify>,
    stats: &PoolStats,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (ws_stream, _response) = tokio_tungstenite::connect_async(&config.ws_url).await?;
    let (mut sink, mut stream) = ws_stream.split();

    // Send initial subscribe for all currently tracked assets
    let current_assets: Vec<String> = subscribed.lock().iter().cloned().collect();
    if !current_assets.is_empty() {
        send_initial_subscribe(&mut sink, &current_assets, config.custom_features).await?;
        tracing::info!("{}: subscribed {} assets", label, current_assets.len());
    } else if config.custom_features {
        // Even with no assets, connect with custom_feature_enabled for global events
        send_initial_subscribe(&mut sink, &[], config.custom_features).await?;
        tracing::info!("{}: connected (custom features, no assets)", label);
    }

    // Main loop
    let mut heartbeat_interval = tokio::time::interval(
        Duration::from_secs(config.heartbeat_interval_secs)
    );
    let mut last_pong = std::time::Instant::now();
    let pong_timeout = Duration::from_secs(config.heartbeat_interval_secs * 3);

    loop {
        tokio::select! {
            biased;

            // PING heartbeat — highest priority
            _ = heartbeat_interval.tick() => {
                if last_pong.elapsed() > pong_timeout {
                    tracing::warn!("{}: no PONG for {}s, treating as dead",
                        label, last_pong.elapsed().as_secs());
                    return Err("pong timeout".into());
                }
                if let Err(e) = sink.send(WsMessage::Text("PING".to_string())).await {
                    tracing::warn!("{}: heartbeat send failed: {}", label, e);
                    return Err(Box::new(e));
                }
            }

            // Subscription commands from pool
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    SubCommand::Subscribe(ids) => {
                        if !ids.is_empty() {
                            tracing::debug!("{}: subscribing {} assets", label, ids.len());
                            send_dynamic_subscribe(&mut sink, &ids, config.custom_features).await?;
                        }
                    }
                    SubCommand::Unsubscribe(ids) => {
                        if !ids.is_empty() {
                            tracing::debug!("{}: unsubscribing {} assets", label, ids.len());
                            send_unsubscribe(&mut sink, &ids).await?;
                        }
                    }
                }
            }

            // Incoming WS messages
            msg = stream.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        stats.msgs_received.fetch_add(1, Ordering::Relaxed);
                        let trimmed = text.trim();
                        if trimmed == "PONG"
                            || trimmed.starts_with("{\"type\":\"pong\"")
                            || trimmed.starts_with("[{\"type\":\"pong\"")
                        {
                            last_pong = std::time::Instant::now();
                            continue;
                        }
                        handle_message_shared(
                            label, &text, book, eval_queue, resolved, positions, latency,
                            instruments.as_ref(),
                        );
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        let _ = sink.send(WsMessage::Pong(data)).await;
                        last_pong = std::time::Instant::now();
                    }
                    Some(Ok(WsMessage::Pong(_))) => {
                        last_pong = std::time::Instant::now();
                    }
                    Some(Ok(WsMessage::Close(_))) => {
                        tracing::info!("{}: server sent Close", label);
                        return Ok(());
                    }
                    Some(Err(e)) => {
                        return Err(Box::new(e));
                    }
                    None => {
                        tracing::info!("{}: stream ended", label);
                        return Ok(());
                    }
                    _ => {} // Binary — ignore
                }
            }

            // Shutdown signal
            _ = shutdown.notified() => {
                tracing::info!("{}: shutdown", label);
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

/// Send an initial subscribe message (first subscription on a fresh connection).
/// Uses `type` field (no `operation`) per Polymarket docs.
async fn send_initial_subscribe<S>(
    sink: &mut S,
    asset_ids: &[String],
    custom_features: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    S: SinkExt<WsMessage, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let asset_list: Vec<Value> = asset_ids.iter()
        .map(|a| Value::String(a.clone()))
        .collect();
    let sub_msg = serde_json::json!({
        "assets_ids": asset_list,
        "type": "market",
        "custom_feature_enabled": custom_features,
    });
    sink.send(WsMessage::Text(sub_msg.to_string())).await?;
    Ok(())
}

/// Send a dynamic subscribe message (adding assets to an existing connection).
/// Uses `operation: "subscribe"` per Polymarket docs.
async fn send_dynamic_subscribe<S>(
    sink: &mut S,
    asset_ids: &[String],
    custom_features: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    S: SinkExt<WsMessage, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let asset_list: Vec<Value> = asset_ids.iter()
        .map(|a| Value::String(a.clone()))
        .collect();
    let sub_msg = serde_json::json!({
        "assets_ids": asset_list,
        "operation": "subscribe",
        "custom_feature_enabled": custom_features,
    });
    sink.send(WsMessage::Text(sub_msg.to_string())).await?;
    Ok(())
}

/// Send an unsubscribe message for the given asset IDs.
async fn send_unsubscribe<S>(
    sink: &mut S,
    asset_ids: &[String],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    S: SinkExt<WsMessage, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let asset_list: Vec<Value> = asset_ids.iter()
        .map(|a| Value::String(a.clone()))
        .collect();
    let unsub_msg = serde_json::json!({
        "assets_ids": asset_list,
        "operation": "unsubscribe",
    });
    sink.send(WsMessage::Text(unsub_msg.to_string())).await?;
    Ok(())
}
