/// Tier B — Hot Constraint Monitoring WebSocket connections.
///
/// 5-10 long-lived connections (max 450 assets each) monitoring assets from
/// overpriced constraints where arbitrage opportunities may exist.
///
/// Features:
///   - Dynamic subscription management (add/remove via WS messages, no reconnection)
///   - Hysteresis on removal: constraints must be cold for N consecutive scans before unsubscribe
///   - Auto-scales connections up when hot constraint count grows
///   - Hourly consolidation when connections are underutilised
///   - Assets promoted to Tier C on position entry, demoted back on resolution
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use parking_lot::Mutex;

use crate::book::BookMirror;
use crate::latency::LatencyTracker;
use crate::position::PositionManager;
use crate::queue::EvalQueue;
use crate::ws::ResolvedEvent;
use crate::ws_pool::{ConnectionPool, PoolConfig, PoolStats, WsTier};

/// Info about a hot constraint being monitored.
#[derive(Debug, Clone)]
struct HotConstraint {
    /// Asset IDs belonging to this constraint (both YES and NO tokens).
    asset_ids: Vec<String>,
    /// When this constraint was added to monitoring.
    added_at: Instant,
}

/// Configuration for Tier B.
#[derive(Debug, Clone)]
pub struct TierBConfig {
    pub ws_url: String,
    pub heartbeat_interval_secs: u64,
    pub max_assets_per_connection: usize,
    pub max_connections: usize,
    pub hysteresis_scans: u32,
    pub stagger_ms: u64,
    /// Threshold for hourly consolidation (assets per conn before consolidating).
    pub consolidation_threshold: usize,
}

/// Tier B manager — wraps a ConnectionPool with hot constraint tracking.
pub struct TierB {
    pool: ConnectionPool,
    /// Currently monitored hot constraints: constraint_id → info.
    hot_constraints: Mutex<HashMap<String, HotConstraint>>,
    /// Constraints that dropped from hot set: constraint_id → consecutive cold scans.
    removal_candidates: Mutex<HashMap<String, u32>>,
    /// Number of scans a constraint must be cold before removal.
    hysteresis_threshold: u32,
    /// Assets currently promoted to Tier C (excluded from B subscriptions).
    promoted_to_c: Mutex<HashSet<String>>,
    /// Consolidation threshold per connection.
    consolidation_threshold: usize,
    /// Last consolidation check time.
    last_consolidation: Mutex<Instant>,
}

impl TierB {
    pub fn new(
        config: TierBConfig,
        book: Arc<BookMirror>,
        eval_queue: Arc<EvalQueue>,
        resolved_events: Arc<Mutex<Vec<ResolvedEvent>>>,
        positions: Arc<Mutex<PositionManager>>,
        latency: Arc<LatencyTracker>,
    ) -> Self {
        let pool_config = PoolConfig {
            ws_url: config.ws_url,
            max_assets_per_connection: config.max_assets_per_connection,
            max_connections: config.max_connections,
            heartbeat_interval_secs: config.heartbeat_interval_secs,
            custom_features: true, // best_bid_ask events
            stagger_ms: config.stagger_ms,
        };

        let pool = ConnectionPool::new(
            WsTier::B,
            pool_config,
            book,
            eval_queue,
            resolved_events,
            positions,
            latency,
        );

        Self {
            pool,
            hot_constraints: Mutex::new(HashMap::new()),
            removal_candidates: Mutex::new(HashMap::new()),
            hysteresis_threshold: config.hysteresis_scans,
            promoted_to_c: Mutex::new(HashSet::new()),
            consolidation_threshold: config.consolidation_threshold,
            last_consolidation: Mutex::new(Instant::now()),
        }
    }

    /// Start Tier B with initial hot constraint assets.
    pub fn start(&self, hot_asset_ids: Vec<String>, rt: &tokio::runtime::Handle) {
        self.pool.start(hot_asset_ids, rt);
    }

    /// Update hot constraints after a scanner rebuild.
    ///
    /// `current_hot` maps constraint_id → asset_ids for all currently overpriced constraints.
    /// Applies hysteresis: constraints that disappear must stay cold for `hysteresis_threshold`
    /// consecutive scans before their assets are unsubscribed.
    pub fn update_hot_constraints(
        &self,
        current_hot: HashMap<String, Vec<String>>,
    ) {
        let mut hot = self.hot_constraints.lock();
        let mut removal = self.removal_candidates.lock();
        let promoted = self.promoted_to_c.lock();

        let current_ids: HashSet<&String> = current_hot.keys().collect();
        let existing_ids: HashSet<String> = hot.keys().cloned().collect();

        // 1. New hot constraints — subscribe immediately
        let mut to_subscribe: Vec<String> = Vec::new();
        for (cid, asset_ids) in &current_hot {
            if !hot.contains_key(cid) {
                // Filter out any assets currently on Tier C
                let filtered: Vec<String> = asset_ids.iter()
                    .filter(|a| !promoted.contains(*a))
                    .cloned()
                    .collect();
                to_subscribe.extend(filtered);
                hot.insert(cid.clone(), HotConstraint {
                    asset_ids: asset_ids.clone(),
                    added_at: Instant::now(),
                });
                // Clear from removal candidates if it came back
                removal.remove(cid);
            }
        }

        // 2. Disappeared constraints — apply hysteresis
        let mut to_unsubscribe: Vec<String> = Vec::new();
        let mut removed_cids: Vec<String> = Vec::new();
        for cid in &existing_ids {
            if current_ids.contains(cid) {
                // Still hot — reset removal counter
                removal.remove(cid);
                continue;
            }

            // Not in current hot set — increment cold counter
            let count = removal.entry(cid.clone()).or_insert(0);
            *count += 1;

            if *count >= self.hysteresis_threshold {
                // Cold for long enough — actually remove
                if let Some(info) = hot.get(cid) {
                    let assets: Vec<String> = info.asset_ids.iter()
                        .filter(|a| !promoted.contains(*a))
                        .cloned()
                        .collect();
                    to_unsubscribe.extend(assets);
                }
                removed_cids.push(cid.clone());
            } else {
                tracing::debug!(
                    "Tier B: constraint {} cold for {}/{} scans (keeping subscribed)",
                    cid, count, self.hysteresis_threshold,
                );
            }
        }

        // 3. Clean up fully removed constraints
        for cid in &removed_cids {
            hot.remove(cid);
            removal.remove(cid);
        }

        drop(hot);
        drop(removal);
        drop(promoted);

        // 4. Apply subscription changes
        if !to_subscribe.is_empty() {
            tracing::info!("Tier B: subscribing {} assets from {} new hot constraints",
                to_subscribe.len(),
                current_hot.len().saturating_sub(existing_ids.len()));
            self.pool.subscribe(to_subscribe);
        }
        if !to_unsubscribe.is_empty() {
            tracing::info!("Tier B: unsubscribing {} assets from {} cooled constraints",
                to_unsubscribe.len(), removed_cids.len());
            self.pool.unsubscribe(to_unsubscribe);
        }

        let total = self.pool.stats.assets_subscribed.load(Ordering::Relaxed);
        let n_hot = self.hot_constraints.lock().len();
        tracing::info!("Tier B: {} hot constraints, {} assets subscribed, {} connections",
            n_hot, total, self.pool.connection_count());
    }

    /// Promote assets to Tier C (position entered).
    /// Unsubscribes from B — caller should subscribe on C first (overlap > gap).
    pub fn promote_to_c(&self, asset_ids: &[String]) {
        {
            let mut promoted = self.promoted_to_c.lock();
            for id in asset_ids {
                promoted.insert(id.clone());
            }
        }
        self.pool.unsubscribe(asset_ids.to_vec());
        tracing::info!("Tier B: promoted {} assets to Tier C", asset_ids.len());
    }

    /// Demote assets from Tier C back to B (position resolved, constraint still hot).
    pub fn demote_from_c(&self, asset_ids: &[String], constraint_id: &str) {
        let is_still_hot = self.hot_constraints.lock().contains_key(constraint_id);

        {
            let mut promoted = self.promoted_to_c.lock();
            for id in asset_ids {
                promoted.remove(id);
            }
        }

        if is_still_hot {
            self.pool.subscribe(asset_ids.to_vec());
            tracing::info!("Tier B: demoted {} assets from Tier C (constraint {} still hot)",
                asset_ids.len(), constraint_id);
        } else {
            tracing::info!("Tier B: dropped {} assets (constraint {} no longer hot)",
                asset_ids.len(), constraint_id);
        }
    }

    /// Add a hot constraint discovered via Tier C's new market detection.
    /// Immediately subscribes without waiting for the next scanner cycle.
    pub fn add_from_new_market(&self, constraint_id: String, asset_ids: Vec<String>) {
        let promoted = self.promoted_to_c.lock();
        let filtered: Vec<String> = asset_ids.iter()
            .filter(|a| !promoted.contains(*a))
            .cloned()
            .collect();
        drop(promoted);

        self.hot_constraints.lock().insert(constraint_id.clone(), HotConstraint {
            asset_ids: asset_ids.clone(),
            added_at: Instant::now(),
        });

        if !filtered.is_empty() {
            self.pool.subscribe(filtered);
            tracing::info!("Tier B: added new market constraint {} ({} assets)",
                constraint_id, asset_ids.len());
        }
    }

    /// Check if hourly consolidation is needed and run it if so.
    /// Call this from the orchestrator tick loop.
    pub fn maybe_consolidate(&self) {
        let mut last = self.last_consolidation.lock();
        if last.elapsed().as_secs() < 3600 {
            return;
        }
        *last = Instant::now();
        drop(last);

        self.pool.consolidate(self.consolidation_threshold);
    }

    /// Get number of hot constraints being monitored.
    pub fn hot_constraint_count(&self) -> usize {
        self.hot_constraints.lock().len()
    }

    /// Get pool stats.
    pub fn stats(&self) -> &PoolStats {
        &self.pool.stats
    }

    /// Stop Tier B.
    pub fn stop(&self) {
        self.pool.stop();
    }
}
