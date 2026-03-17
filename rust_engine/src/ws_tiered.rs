/// TieredWsManager — facade coordinating Tier B and Tier C WebSocket connections.
///
/// Provides a unified API for the orchestrator to manage WebSocket subscriptions
/// across both tiers, including asset migration between tiers on position entry/exit.
///
/// Architecture:
///   Tier A: REST only (scanner, every ~10min) — not managed here
///   Tier B: Hot constraint monitoring (5-10 connections, ~2,000-3,000 assets)
///   Tier C: Open positions + command connection (1 connection, ~30-40 assets)
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use parking_lot::Mutex;

use crate::book::BookMirror;
use crate::latency::LatencyTracker;
use crate::position::PositionManager;
use crate::queue::EvalQueue;
use crate::ws::ResolvedEvent;
use crate::ws_tier_b::{TierB, TierBConfig};
use crate::ws_tier_c::{TierC, TierCConfig, NewMarketBurst};

/// Stats for the tiered WS system.
#[derive(Debug, Clone)]
pub struct TieredWsStats {
    pub tier_b_connections: u32,
    pub tier_b_assets: u64,
    pub tier_b_msgs: u64,
    pub tier_b_hot_constraints: usize,
    pub tier_c_connections: u32,
    pub tier_c_assets: u64,
    pub tier_c_msgs: u64,
    pub tier_c_position_assets: usize,
    pub total_msgs: u64,
    pub total_connections: u32,
    pub total_assets: u64,
}

/// Configuration for the tiered WS manager.
#[derive(Debug, Clone)]
pub struct TieredWsConfig {
    pub ws_url: String,
    pub heartbeat_interval_secs: u64,
    pub max_assets_per_connection: usize,
    pub stagger_ms: u64,
    // Tier B
    pub tier_b_max_connections: usize,
    pub tier_b_hysteresis_scans: u32,
    pub tier_b_consolidation_threshold: usize,
    // Tier C
    pub tier_c_new_market_buffer_secs: f64,
}

/// The tiered WebSocket manager coordinating Tier B and Tier C.
pub struct TieredWsManager {
    tier_b: TierB,
    tier_c: TierC,
}

impl TieredWsManager {
    pub fn new(
        config: TieredWsConfig,
        book: Arc<BookMirror>,
        eval_queue: Arc<EvalQueue>,
        resolved_events: Arc<Mutex<Vec<ResolvedEvent>>>,
        positions: Arc<Mutex<PositionManager>>,
        latency: Arc<LatencyTracker>,
        instruments: Option<Arc<crate::instrument::InstrumentStore>>,
    ) -> Self {
        let tier_b = TierB::new(
            TierBConfig {
                ws_url: config.ws_url.clone(),
                heartbeat_interval_secs: config.heartbeat_interval_secs,
                max_assets_per_connection: config.max_assets_per_connection,
                max_connections: config.tier_b_max_connections,
                hysteresis_scans: config.tier_b_hysteresis_scans,
                stagger_ms: config.stagger_ms,
                consolidation_threshold: config.tier_b_consolidation_threshold,
            },
            Arc::clone(&book),
            Arc::clone(&eval_queue),
            Arc::clone(&resolved_events),
            Arc::clone(&positions),
            Arc::clone(&latency),
            instruments.clone(),
        );

        let tier_c = TierC::new(
            TierCConfig {
                ws_url: config.ws_url,
                heartbeat_interval_secs: config.heartbeat_interval_secs,
                max_assets_per_connection: config.max_assets_per_connection,
                new_market_buffer_secs: config.tier_c_new_market_buffer_secs,
                stagger_ms: config.stagger_ms,
            },
            book,
            eval_queue,
            resolved_events,
            positions,
            latency,
            instruments,
        );

        Self { tier_b, tier_c }
    }

    /// Start both tiers. Tier C starts first (command connection), then Tier B with stagger.
    pub fn start(
        &self,
        hot_asset_ids: Vec<String>,
        position_asset_ids: Vec<String>,
        rt: &tokio::runtime::Handle,
    ) {
        tracing::info!(
            "TieredWS: starting Tier C ({} position assets), Tier B ({} hot assets)",
            position_asset_ids.len(), hot_asset_ids.len(),
        );

        // Tier C first — command connection must be up for global events
        self.tier_c.start(position_asset_ids, rt);

        // Tier B — hot constraint monitoring
        self.tier_b.start(hot_asset_ids, rt);
    }

    /// Stop both tiers.
    pub fn stop(&self) {
        self.tier_c.stop();
        self.tier_b.stop();
        tracing::info!("TieredWS: stopped");
    }

    /// Update Tier B hot constraints after scanner rebuild.
    /// `hot_constraints` maps constraint_id → asset_ids for all currently overpriced constraints.
    pub fn update_tier_b(&self, hot_constraints: HashMap<String, Vec<String>>) {
        self.tier_b.update_hot_constraints(hot_constraints);
    }

    /// Handle position entry: subscribe assets on C, then unsubscribe from B.
    /// Subscribe on C first to ensure zero-gap coverage.
    pub fn on_position_entry(&self, _constraint_id: &str, asset_ids: Vec<String>) {
        // 1. Subscribe on Tier C first (overlap > gap)
        self.tier_c.add_position_assets(asset_ids.clone());
        // 2. Unsubscribe from Tier B
        self.tier_b.promote_to_c(&asset_ids);
    }

    /// Handle position exit: optionally move assets back to B, then unsubscribe from C.
    pub fn on_position_exit(
        &self,
        constraint_id: &str,
        asset_ids: Vec<String>,
        still_hot: bool,
    ) {
        if still_hot {
            // 1. Re-subscribe on B first (overlap > gap)
            self.tier_b.demote_from_c(&asset_ids, constraint_id);
        }
        // 2. Unsubscribe from C
        self.tier_c.remove_position_assets(asset_ids);
    }

    /// Add a new hot constraint discovered via Tier C's new market detection.
    pub fn add_new_market_constraint(&self, constraint_id: String, asset_ids: Vec<String>) {
        self.tier_b.add_from_new_market(constraint_id, asset_ids);
    }

    /// Drain resolved events (from Tier C's connection).
    /// Note: resolved events are still accumulated in the shared resolved_events Vec
    /// which is drained by the old WsManager path. This is for future use when
    /// TieredWsManager fully replaces WsManager.
    pub fn drain_new_market_bursts(&self) -> Vec<NewMarketBurst> {
        self.tier_c.drain_new_market_bursts()
    }

    /// Flush Tier C's new market buffer if ready.
    /// Call this from the orchestrator tick loop.
    pub fn flush_new_markets(&self) -> Vec<NewMarketBurst> {
        self.tier_c.flush_if_ready()
    }

    /// Periodic maintenance: consolidation check, etc.
    /// Call this from the orchestrator tick loop.
    pub fn periodic_maintenance(&self) {
        self.tier_b.maybe_consolidate();
    }

    /// Get combined stats across both tiers.
    pub fn stats(&self) -> TieredWsStats {
        let b = self.tier_b.stats();
        let c = self.tier_c.stats();
        let b_msgs = b.msgs_received.load(Ordering::Relaxed);
        let c_msgs = c.msgs_received.load(Ordering::Relaxed);
        let b_assets = b.assets_subscribed.load(Ordering::Relaxed);
        let c_assets = c.assets_subscribed.load(Ordering::Relaxed);
        let b_conns = b.connections_active.load(Ordering::Relaxed);
        let c_conns = c.connections_active.load(Ordering::Relaxed);

        TieredWsStats {
            tier_b_connections: b_conns,
            tier_b_assets: b_assets,
            tier_b_msgs: b_msgs,
            tier_b_hot_constraints: self.tier_b.hot_constraint_count(),
            tier_c_connections: c_conns,
            tier_c_assets: c_assets,
            tier_c_msgs: c_msgs,
            tier_c_position_assets: self.tier_c.position_asset_count(),
            total_msgs: b_msgs + c_msgs,
            total_connections: b_conns + c_conns,
            total_assets: b_assets + c_assets,
        }
    }
}
