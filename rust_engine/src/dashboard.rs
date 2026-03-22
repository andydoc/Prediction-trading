/// Axum-based dashboard server — serves HTML + SSE events from in-memory state.
///
/// Runs on the same tokio runtime as the WS engine. Reads directly from
/// Arc-wrapped PositionManager, BookMirror, EvalQueue etc. Zero disk reads.
///
/// Routes:
///   GET /         → static HTML shell (client JS handles rendering)
///   GET /stream   → SSE events: stats(5s), positions(5s), opportunities(15s),
///                   system(10s), closed(60s)
///   GET /state    → JSON snapshot of full execution state
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use axum::{Router, Json, response::{Html, Sse, sse}};
use axum::extract::State;
use serde_json::{json, Value};
use parking_lot::Mutex;

use crate::position::PositionManager;
use crate::book::BookMirror;
use crate::latency::LatencyTracker;
use crate::queue::EvalQueue;
use crate::eval::ConstraintStore;
use crate::ws::WsManager;
use crate::monitor::MonitorState;

/// Shared state passed to all axum handlers via Arc.
#[derive(Clone)]
pub struct DashboardState {
    pub positions: Arc<Mutex<PositionManager>>,
    pub book: Arc<BookMirror>,
    pub eval_queue: Arc<EvalQueue>,
    pub ws: Arc<WsManager>,
    pub constraints: Arc<ConstraintStore>,
    /// Engine metrics updated each stats cycle
    pub engine_metrics: Arc<Mutex<EngineMetrics>>,
    /// Recent opportunities (last batch from evaluate_batch)
    pub recent_opps: Arc<Mutex<Vec<Value>>>,
    /// Mode: "shadow" or "live"
    pub mode: String,
    /// Start time (for uptime display)
    pub start_time: chrono::DateTime<chrono::Utc>,
    /// P95 delay table: category → p95_hours (loaded from SQLite at startup)
    pub delay_table: Arc<Mutex<(std::collections::HashMap<String, f64>, f64)>>,
    /// Latency instrumentation tracker
    pub latency: Arc<LatencyTracker>,
    /// Monitor state for time-series metrics and system resources
    pub monitor: Arc<Mutex<MonitorState>>,
    /// Separate log ring buffer (avoids monitor lock contention)
    pub log_ring: Arc<Mutex<crate::monitor::LogRing>>,
    /// C2: Kill switch flag — set by POST /api/kill-switch, read by orchestrator
    pub kill_switch: Arc<AtomicBool>,
    /// Strategy tracker summary JSON — updated by orchestrator
    pub strategy_summary: Arc<Mutex<Value>>,
}

/// Metrics updated by the Rust orchestrator each stats cycle.
#[derive(Default, Clone)]
pub struct EngineMetrics {
    pub iteration: u64,
    pub lat_p50_us: u64,
    pub lat_p95_us: u64,
    pub lat_max_us: u64,
    pub scanner_status: String,
    pub scanner_ts: String,
    pub engine_status: String,
    pub engine_ts: String,
    // Tiered WS stats (set by orchestrator each stats cycle)
    pub ws_subscribed: u64,
    pub ws_total_msgs: u64,
    pub ws_live_books: u64,
    pub ws_connections: u32,
    // Gas monitor (C1.1)
    pub pol_balance: Option<f64>,
    // E2.5: Stress test failure signal counters
    pub ws_reconnects: u64,
    pub ws_pong_timeouts: u64,
    pub ws_heartbeat_failures: u64,
    pub evals_total: u64,
    pub opps_found: u64,
    pub stale_sweeps: u64,
    pub stale_assets_swept: u64,
}

/// Static HTML — loaded at compile time from the extracted template.
const DASHBOARD_HTML: &str = include_str!("../static/dashboard.html");

/// Start the dashboard server on the given bind address and port.
/// Must be called from within a tokio runtime (spawned as async task).
pub async fn start(state: DashboardState, port: u16, bind_addr: &str) {
    let app = Router::new()
        .route("/", axum::routing::get(handle_html))
        .route("/stream", axum::routing::get(handle_sse))
        .route("/state", axum::routing::get(handle_state))
        .route("/metrics", axum::routing::get(handle_metrics))
        .route("/api/kill-switch", axum::routing::post(handle_kill_switch))
        .with_state(Arc::new(state));

    let addr = format!("{}:{}", bind_addr, port);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("Dashboard failed to bind {}: {}", addr, e);
            return;
        }
    };
    tracing::info!("Dashboard server started on {}", addr);
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!("Dashboard server error: {}", e);
    }
}

// =====================================================================
// Route handlers
// =====================================================================

async fn handle_html(State(s): State<Arc<DashboardState>>) -> Html<String> {
    let html = DASHBOARD_HTML
        .replace("{{MODE_LABEL}}", &s.mode.to_uppercase())
        .replace("{{MODE_CLASS}}", &format!("mode-{}", s.mode))
        .replace("{{VERSION}}", &format!("v{}", env!("CARGO_PKG_VERSION")));
    Html(html)
}

async fn handle_state(State(s): State<Arc<DashboardState>>) -> Json<Value> {
    Json(build_state_snapshot(&s))
}

/// E2.5: Flat metrics endpoint for stress test harness.
async fn handle_metrics(State(s): State<Arc<DashboardState>>) -> Json<Value> {
    Json(build_metrics(&s))
}

/// C2: Kill switch endpoint — POST /api/kill-switch
/// Sets the atomic flag that the orchestrator checks each tick.
/// Idempotent: returns success even if already triggered.
async fn handle_kill_switch(State(s): State<Arc<DashboardState>>) -> Json<Value> {
    let was_set = s.kill_switch.swap(true, Ordering::SeqCst);
    if was_set {
        tracing::warn!("[KILL] Kill switch triggered via dashboard (already active)");
        Json(json!({"status": "ok", "message": "Kill switch already active"}))
    } else {
        tracing::error!("[KILL] Kill switch triggered via dashboard");
        Json(json!({"status": "ok", "message": "Kill switch activated"}))
    }
}

async fn handle_sse(
    State(s): State<Arc<DashboardState>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<sse::Event, std::convert::Infallible>>> {
    // Send all events immediately on connect, then at intervals
    let stream = async_stream::stream! {
        // Initial burst: all events at once
        yield Ok(sse::Event::default().event("stats").data(
            serde_json::to_string(&build_stats(&s)).unwrap_or_default()));
        yield Ok(sse::Event::default().event("positions").data(
            serde_json::to_string(&build_positions(&s)).unwrap_or_default()));
        yield Ok(sse::Event::default().event("opportunities").data(
            serde_json::to_string(&build_opportunities(&s)).unwrap_or_default()));
        yield Ok(sse::Event::default().event("system").data(
            serde_json::to_string(&build_system(&s)).unwrap_or_default()));
        yield Ok(sse::Event::default().event("closed").data(
            serde_json::to_string(&build_closed(&s)).unwrap_or_default()));
        // Monitor: full snapshot on connect
        yield Ok(sse::Event::default().event("monitor").data(
            serde_json::to_string(&build_monitor(&s, true)).unwrap_or_default()));
        // Strategies: initial snapshot
        {
            let strat = s.strategy_summary.lock().clone();
            if !strat.is_null() {
                yield Ok(sse::Event::default().event("strategies").data(
                    serde_json::to_string(&strat).unwrap_or_default()));
            }
        }

        let mut tick = 0u64;
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            tick += 1;

            // Stats + positions: every 5s (every tick)
            yield Ok(sse::Event::default().event("stats").data(
                serde_json::to_string(&build_stats(&s)).unwrap_or_default()));
            yield Ok(sse::Event::default().event("positions").data(
                serde_json::to_string(&build_positions(&s)).unwrap_or_default()));

            // System: every 10s (every 2nd tick)
            if tick % 2 == 0 {
                yield Ok(sse::Event::default().event("system").data(
                    serde_json::to_string(&build_system(&s)).unwrap_or_default()));
                // Monitor: delta every 10s
                yield Ok(sse::Event::default().event("monitor").data(
                    serde_json::to_string(&build_monitor(&s, false)).unwrap_or_default()));
            }

            // Opportunities + strategies: every 15s (every 3rd tick)
            if tick % 3 == 0 {
                yield Ok(sse::Event::default().event("opportunities").data(
                    serde_json::to_string(&build_opportunities(&s)).unwrap_or_default()));
                let strat = s.strategy_summary.lock().clone();
                if !strat.is_null() {
                    yield Ok(sse::Event::default().event("strategies").data(
                        serde_json::to_string(&strat).unwrap_or_default()));
                }
            }

            // C4.1: Send closed alongside positions (every 5s) to eliminate visual gap
            yield Ok(sse::Event::default().event("closed").data(
                serde_json::to_string(&build_closed(&s)).unwrap_or_default()));
        }
    };

    Sse::new(stream).keep_alive(
        sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping")
    )
}

// =====================================================================
// Shared helpers
// =====================================================================

/// Build a JSON leg object from a typed MarketLeg. Used by both open and closed position builders.
fn build_leg_json(leg: &crate::position::MarketLeg, is_sell: bool) -> Value {
    let ep = leg.entry_price;
    let bet = leg.bet_amount;
    let shares = if leg.shares > 0.0 {
        leg.shares
    } else if is_sell {
        let no_price = (1.0 - ep).max(0.001);
        bet / no_price
    } else if ep > 0.0 {
        bet / ep
    } else {
        0.0
    };
    let side = if is_sell { "NO" } else { "YES" };
    let actual_price = if shares > 0.0 { bet / shares } else { ep };
    json!({
        "name": leg.name, "bet": (bet * 100.0).round() / 100.0,
        "side": side, "price": (actual_price * 1000.0).round() / 1000.0,
        "shares": format!("{:.2}", shares),
        "payout": (shares * 100.0).round() / 100.0,
    })
}

// =====================================================================
// Data builders — read directly from in-memory Rust structs
// =====================================================================

fn build_state_snapshot(s: &DashboardState) -> Value {
    let pm = s.positions.lock();
    let perf = pm.get_performance_metrics();
    json!({
        "current_capital": pm.current_capital(),
        "initial_capital": pm.initial_capital(),
        "open_positions": pm.open_positions().values().collect::<Vec<_>>(),
        "closed_positions": pm.closed_positions(),
        "performance": perf,
        "recent_opps": s.recent_opps.lock().clone(),
    })
}

/// E2.5: Flat metrics snapshot for the stress test harness.
/// Returns all metrics needed to evaluate failure signals for each parameter.
fn build_metrics(s: &DashboardState) -> Value {
    let m = s.engine_metrics.lock().clone();
    let (q_urg, q_bg) = s.eval_queue.depths();
    let n_constraints = s.constraints.len();
    let n_markets = s.book.live_count();
    let lat_snap = s.latency.snapshot();

    // System + app metrics from monitor (collect both so ws_msg_rate gets updated)
    let mut mon = s.monitor.lock();
    mon.collect_system_metrics();
    mon.collect_app_metrics(
        m.ws_subscribed, m.ws_total_msgs, m.ws_live_books,
        n_constraints, n_markets,
        m.lat_p50_us, m.lat_p95_us,
        q_urg, q_bg,
    );
    let cpu_pct = mon.cpu_pct.latest().unwrap_or(0.0);
    let mem_mb = mon.mem_used_mb.latest().unwrap_or(0.0);
    let disk_used_gb = mon.disk_used_gb.latest().unwrap_or(0.0);
    let disk_total_gb = mon.disk_total_gb;
    let ws_msg_rate = mon.ws_msg_rate.latest().unwrap_or(0.0);
    drop(mon);

    // Stale book counts at two thresholds (failure signals for stale_sweep/stale_asset params)
    let stale_30 = s.book.get_stale_assets(30.0).len();
    let stale_60 = s.book.get_stale_assets(60.0).len();

    json!({
        "iteration": m.iteration,
        "cpu_pct": (cpu_pct * 10.0).round() / 10.0,
        "mem_mb": (mem_mb * 10.0).round() / 10.0,
        "disk_used_gb": (disk_used_gb * 100.0).round() / 100.0,
        "disk_total_gb": (disk_total_gb * 100.0).round() / 100.0,
        "queue_urgent": q_urg,
        "queue_background": q_bg,
        "queue_total": q_urg + q_bg,
        "lat_p50": m.lat_p50_us,
        "lat_p95": m.lat_p95_us,
        "lat_max": m.lat_max_us,
        "lat_e2e_p50": lat_snap.e2e.p50 as u64,
        "lat_e2e_p95": lat_snap.e2e.p95 as u64,
        "lat_eval_p50": lat_snap.eval_batch.p50 as u64,
        "lat_eval_p95": lat_snap.eval_batch.p95 as u64,
        "lat_queue_wait_p50": lat_snap.queue_wait.p50 as u64,
        "lat_queue_wait_p95": lat_snap.queue_wait.p95 as u64,
        "lat_ws_net_p50": lat_snap.ws_network.p50 as u64,
        "lat_ws_net_p95": lat_snap.ws_network.p95 as u64,
        "ws_msgs": m.ws_total_msgs,
        "ws_live": m.ws_live_books,
        "ws_connections": m.ws_connections,
        "ws_subscribed": m.ws_subscribed,
        "ws_msg_rate": (ws_msg_rate * 10.0).round() / 10.0,
        "ws_reconnects": m.ws_reconnects,
        "ws_pong_timeouts": m.ws_pong_timeouts,
        "ws_heartbeat_failures": m.ws_heartbeat_failures,
        "constraints": n_constraints,
        "markets": n_markets,
        "stale_books_30s": stale_30,
        "stale_books_60s": stale_60,
        "evals_total": m.evals_total,
        "opps_found": m.opps_found,
        "stale_sweeps": m.stale_sweeps,
        "stale_assets_swept": m.stale_assets_swept,
    })
}

/// Compute guaranteed (minimum-scenario) payout from leg payouts.
/// For sell-all: when outcome i wins, leg i is lost, all others pay out.
/// For buy-all: each leg pays if it wins, guaranteed = min across legs.
fn compute_guaranteed_payout(legs: &[Value], is_sell: bool, fallback: f64) -> f64 {
    if is_sell && legs.len() > 1 {
        let share_vals: Vec<f64> = legs.iter()
            .map(|l| l["payout"].as_f64().unwrap_or(0.0))
            .collect();
        let total_shares: f64 = share_vals.iter().sum();
        share_vals.iter()
            .map(|s| total_shares - s)
            .fold(f64::MAX, f64::min)
    } else if !is_sell && !legs.is_empty() {
        legs.iter()
            .map(|l| l["payout"].as_f64().unwrap_or(0.0))
            .fold(f64::MAX, f64::min)
    } else {
        fallback
    }
}

fn fmt_ts(ts: f64) -> String {
    if ts <= 0.0 { return "?".into(); }
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|dt| dt.format("%d/%m/%Y %H:%M").to_string())
        .unwrap_or_else(|| "?".into())
}

fn fmt_ts_sec(ts: f64) -> String {
    if ts <= 0.0 { return "?".into(); }
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|dt| dt.format("%d/%m/%Y %H:%M:%S").to_string())
        .unwrap_or_else(|| "?".into())
}

/// Parse entry_timestamp from a string (ISO or Unix float).
///
/// Two formats exist because legacy positions (imported from Python-era state) store
/// entry_timestamp as a Unix float (e.g. "1773397067.088904"), while newer positions
/// created by the Rust engine use ISO 8601 / RFC 3339 (e.g. "2026-03-17T12:00:00Z").
/// Both must be supported to correctly display positions across engine upgrades.
/// Returns 0.0 if the string is unparseable.
fn parse_entry_ts_str(s: &str) -> f64 {
    // Try ISO first (newer format)
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return dt.timestamp() as f64;
    }
    // Fallback: Unix float string (legacy format)
    s.parse::<f64>().unwrap_or(0.0)
}

/// Format strategy for display.
/// Legacy fallback on method string — remove after all pre-v3 positions have resolved.
fn fmt_strategy(strategy: &str, method: &str) -> String {
    if method.contains("sell") { "Mutex Sell All".into() }
    else if method.contains("buy") { "Mutex Buy All".into() }
    else if strategy == "arb_sell" { "Mutex Sell All".into() }
    else if strategy == "arb_buy" { "Mutex Buy All".into() }
    else { strategy.to_string() }
}

fn build_stats(s: &DashboardState) -> Value {
    // Lock scope: extract all needed data from PositionManager in one locked section,
    // then drop the lock before doing any further computation or accessing other locks.
    let (cap, init_cap, perf, open_count, closed_count, total_fees,
         open_snapshot, first_entry_ts, total_realized) = {
        let pm = s.positions.lock();
        let cap = pm.current_capital();
        let init_cap = pm.initial_capital();
        let perf = pm.get_performance_metrics();
        let open_count = pm.open_count();
        let closed_count = pm.closed_count();
        let total_fees = pm.total_fees();
        let open_snapshot: Vec<crate::position::Position> = pm.open_positions().values().cloned().collect();

        let mut first_entry_ts = f64::MAX;
        for p in &open_snapshot {
            let ts = parse_entry_ts_str(&p.entry_timestamp);
            if ts > 0.0 && ts < first_entry_ts { first_entry_ts = ts; }
        }
        for p in pm.closed_positions() {
            let ts = parse_entry_ts_str(&p.entry_timestamp);
            if ts > 0.0 && ts < first_entry_ts { first_entry_ts = ts; }
        }
        let total_realized = perf.get("total_actual_profit").copied().unwrap_or(0.0);

        (cap, init_cap, perf, open_count, closed_count, total_fees,
         open_snapshot, first_entry_ts, total_realized)
    }; // pm lock dropped here

    // Calculate deployed capital from snapshot (no lock held)
    let mut deployed = 0.0f64;
    for p in &open_snapshot {
        deployed += p.total_capital;
    }

    // B4.4: Total unrealized P&L (mark-to-market across all open positions)
    let mut total_unrealized = 0.0f64;
    for p in &open_snapshot {
        let cid = p.metadata.get("constraint_id").and_then(|v| v.as_str()).unwrap_or("");
        let constraint = s.constraints.get(cid);
        let mut sale_proceeds = 0.0f64;
        for (mid, leg) in &p.markets {
            let live_bid = constraint.as_ref().and_then(|c| {
                c.markets.iter().find(|mref| mref.market_id == *mid).and_then(|mref| {
                    let asset_id = if leg.outcome == "no" { &mref.no_asset_id } else { &mref.yes_asset_id };
                    let bid = s.book.get_best_bid(asset_id);
                    if bid > 0.0 { Some(bid) } else { None }
                })
            });
            sale_proceeds += leg.shares * live_bid.unwrap_or(leg.entry_price);
        }
        total_unrealized += sale_proceeds - p.total_capital;
    }

    let total_value = cap + deployed;
    let ret_pct = if init_cap > 0.0 { (total_value - init_cap) / init_cap * 100.0 } else { 0.0 };
    let trades = perf.get("total_trades").copied().unwrap_or(0.0) as u64;

    // Annualized return
    let first_trade_str = if first_entry_ts < f64::MAX { fmt_ts(first_entry_ts) } else { "N/A".into() };
    let now_ts = chrono::Utc::now().timestamp() as f64;
    let days_running = if first_entry_ts < f64::MAX {
        (now_ts - first_entry_ts) / 86400.0
    } else { 0.0 };
    let min_days = 1.0 / 24.0; // 1 hour minimum
    let annualized_ret = if days_running > min_days {
        ((total_value / init_cap).powf(365.0 / days_running) - 1.0) * 100.0
    } else { 0.0 };
    // Format time period label
    let period_label = if days_running < 1.0 {
        let hours = (days_running * 24.0).max(1.0) as u32;
        format!("{}h", hours)
    } else {
        format!("{}d", days_running as u32)
    };

    let annualized_str = if days_running > min_days {
        if days_running < 7.0 {
            // Under 7 days: show simple daily rate × 365 (linear extrapolation)
            let daily_ret = ret_pct / days_running;
            let linear_annual = daily_ret * 365.0;
            let sign = if linear_annual < 0.0 { "-" } else { "+" };
            format!("{}{:.0}% <span style=\"font-size:0.7em\">({} data)</span>", sign, linear_annual.abs(), period_label)
        } else if annualized_ret.is_nan() || annualized_ret.is_infinite() || annualized_ret.abs() > 1e15 {
            let sign = if annualized_ret < 0.0 { "-" } else { "+" };
            format!("{}<span style=\"font-size:2.2em;vertical-align:middle;line-height:0\">∞</span> <span style=\"font-size:0.7em\">({} data)</span>", sign, period_label)
        } else {
            let abs_val = annualized_ret.abs();
            if abs_val == 0.0 {
                format!("0.00% <span style=\"font-size:0.7em\">({} data)</span>", period_label)
            } else if abs_val < 10000.0 {
                let sign = if annualized_ret < 0.0 { "-" } else { "+" };
                format!("{}{:.1}% <span style=\"font-size:0.7em\">({} data)</span>", sign, abs_val, period_label)
            } else {
                let sign = if annualized_ret < 0.0 { "-" } else { "+" };
                let exp = if abs_val > 0.0 { abs_val.log10().floor() as i32 } else { 0 };
                let mantissa = if exp != 0 { abs_val / 10f64.powi(exp) } else { 0.0 };
                format!("{}{:.2}e{}% <span style=\"font-size:0.7em\">({} data)</span>", sign, mantissa, exp, period_label)
            }
        }
    } else { "N/A".into() };
    let now_str = chrono::Utc::now().format("%d/%m/%Y %H:%M:%S").to_string();
    let start_str = s.start_time.format("%d/%m/%Y %H:%M").to_string();

    json!({
        "cash": cap, "deployed": deployed, "total_value": total_value,
        "init_cap": init_cap, "fees": total_fees, "ret_pct": ret_pct,
        "trades": trades, "open_count": open_count, "closed_count": closed_count,
        "realized": total_realized,
        "unrealized": (total_unrealized * 100.0).round() / 100.0,
        "annualized": annualized_str, "annualized_ret": annualized_ret,
        "mode_label": s.mode.to_uppercase(), "mode_class": format!("mode-{}", s.mode),
        "first_trade": first_trade_str, "start_time": start_str,
        "live_balance": null,
        "timestamp": now_str,
        // Engine metrics (passed through from Python via update_metrics)
        "ws_subs": 0, "ws_msgs": 0, "ws_live": 0, "ws_total": 0, "ws_pct": 0,
        "q_bg": 0, "q_urg": 0,
        "lat_p50": 0, "lat_p95": 0, "lat_max": 0,
        "has_rust": true, "constraints": 0, "iteration": 0,
    })
}

// =====================================================================
// Resolution delay model — P95 delay by event category
// =====================================================================

/// Format hours remaining with P95 delay added. Returns (display_string, sort_seconds).
/// Reads delay table from DashboardState (loaded from SQLite at startup).
fn resolve_with_delay(
    end_date_ts: f64, now_ts: f64, market_names: &[String],
    delay_table: &parking_lot::Mutex<(std::collections::HashMap<String, f64>, f64)>,
) -> (String, f64) {
    if end_date_ts <= 0.0 {
        return ("?".into(), 9999999.0);
    }
    let (ref p95_table, default_p95) = *delay_table.lock();
    let category = crate::types::classify_category(market_names);
    let delay_hours = p95_table.get(category).copied().unwrap_or(default_p95);
    let expected_resolve_ts = end_date_ts + delay_hours * 3600.0;
    let hours_remaining = (expected_resolve_ts - now_ts) / 3600.0;
    let display = if hours_remaining <= 0.0 {
        "overdue".into()
    } else if hours_remaining < 24.0 {
        format!("{:.1}h", hours_remaining)
    } else {
        format!("{:.1}d", hours_remaining / 24.0)
    };
    let sort_secs = hours_remaining * 3600.0;
    (display, sort_secs)
}

fn build_positions(s: &DashboardState) -> Value {
    let mut positions = Vec::new();
    // Clone positions under lock, then release — avoids blocking WS resolution events
    let open_snapshot: Vec<crate::position::Position> = {
        let pm = s.positions.lock();
        pm.open_positions().values().cloned().collect()
    };
    for (idx, p) in open_snapshot.iter().enumerate() {
        let mkt_vals: Vec<&crate::position::MarketLeg> = p.markets.values().collect();
        let short_name = mkt_vals.first()
            .map(|m| m.name.chars().take(40).collect::<String>())
            .unwrap_or_else(|| "?".into());
        let full_names: Vec<String> = mkt_vals.iter()
            .map(|m| m.name.clone())
            .collect();

        let total_cap = p.total_capital;
        let exp_profit = p.expected_profit;
        let exp_pct = if total_cap > 0.0 { exp_profit / total_cap * 100.0 } else { 0.0 };

        let strategy = p.metadata.get("strategy")
            .and_then(|v| v.as_str()).unwrap_or("?");
        let method = p.metadata.get("method")
            .and_then(|v| v.as_str()).unwrap_or("");
        let strategy_fmt = fmt_strategy(strategy, method);
        let is_sell = p.metadata.get("is_sell")
            .and_then(|v| v.as_bool())
            .unwrap_or_else(|| method.to_lowercase().contains("sell"));

        // Entry timestamp
        let entry_epoch = parse_entry_ts_str(&p.entry_timestamp);
        let entry_ts_fmt = fmt_ts(entry_epoch);

        // Build legs — read shares from position data (computed at entry with correct prices)
        let mut legs = Vec::new();
        for (_mid, leg) in &p.markets {
            legs.push(build_leg_json(leg, is_sell));
        }

        // Resolution date — constraint store first, then position metadata, then date in name
        let cid = p.metadata.get("constraint_id")
            .and_then(|v| v.as_str()).unwrap_or("");
        let mut end_date_ts = s.constraints.get(cid)
            .map(|c| c.end_date_ts).unwrap_or(0.0);
        // Fallback 1: read end_date_ts stored in position metadata at entry time
        if end_date_ts <= 0.0 {
            end_date_ts = p.metadata.get("end_date_ts")
                .and_then(|v| v.as_f64()).unwrap_or(0.0);
        }
        // Fallback 2: extract YYYY-MM-DD from market names
        if end_date_ts <= 0.0 {
            for name in &full_names {
                if let Some(pos) = name.find("202") {
                    if name.len() >= pos + 10 {
                        let date_str = name.get(pos..pos+10).unwrap_or("");
                        if let Ok(dt) = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
                            end_date_ts = dt.and_hms_opt(23, 59, 59)
                                .map(|ndt| ndt.and_utc().timestamp() as f64)
                                .unwrap_or(0.0);
                            if end_date_ts > 0.0 { break; }
                        }
                    }
                }
            }
        }
        let end_date = if end_date_ts > 0.0 {
            chrono::DateTime::from_timestamp(end_date_ts as i64, 0)
                .map(|dt| dt.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "?".into())
        } else { "?".into() };

        let pp = p.metadata.get("postponement");
        let postponed = pp.map_or(false, |v| v.is_object() && v.get("effective_date").and_then(|d| d.as_str()).is_some());
        let pp_date = if postponed {
            pp.and_then(|v| v.get("effective_date")).and_then(|v| v.as_str()).unwrap_or("")
        } else { "" };

        // Resolution date formatting + hours remaining (with P95 delay)
        let status = if postponed { "postponed" } else { "monitoring" };
        let now_ts = chrono::Utc::now().timestamp() as f64;
        let (resolve_str, resolve_sort_secs) = resolve_with_delay(end_date_ts, now_ts, &full_names, &s.delay_table);
        let hours_remaining = resolve_sort_secs / 3600.0;

        // Score = profit_pct / hours_remaining
        let score_val = if hours_remaining > 0.01 {
            exp_pct / 100.0 / hours_remaining
        } else { 0.0 };

        // Postponement object (for JS)
        let postpone = if postponed {
            let pp_val = pp.unwrap();
            let pp_status = pp_val.get("status").and_then(|v| v.as_str()).unwrap_or("postponed");
            let pp_reason = pp_val.get("reason").and_then(|v| v.as_str()).unwrap_or("Postponed");
            Some(json!({ "status": pp_status, "reason": pp_reason, "date": pp_date }))
        } else { None };

        // B4.4: Unrealized P&L (mark-to-market) — value position at current best bids
        let unrealized_pnl = {
            let constraint = s.constraints.get(cid);
            let mut sale_proceeds = 0.0f64;
            for (mid, leg) in &p.markets {
                let shares = leg.shares;
                let live_bid = constraint.as_ref().and_then(|c| {
                    c.markets.iter().find(|mref| mref.market_id == *mid).and_then(|mref| {
                        let asset_id = if leg.outcome == "no" {
                            &mref.no_asset_id
                        } else {
                            &mref.yes_asset_id
                        };
                        let bid = s.book.get_best_bid(asset_id);
                        if bid > 0.0 { Some(bid) } else { None }
                    })
                });
                let bid = live_bid.unwrap_or(leg.entry_price);
                sale_proceeds += shares * bid;
            }
            sale_proceeds - total_cap
        };

        // Guaranteed payout: compute from legs (minimum scenario payout)
        let guaranteed = compute_guaranteed_payout(&legs, is_sell, total_cap + exp_profit);

        positions.push(json!({
            "idx": idx + 1, "short_name": short_name, "full_names": full_names,
            "strategy": strategy_fmt, "score": (score_val * 1000.0 * 100.0).round() / 100.0,
            "total_cap": (total_cap * 100.0).round() / 100.0,
            "exp_profit": (exp_profit * 100.0).round() / 100.0,
            "exp_pct": (exp_pct * 10.0).round() / 10.0,
            "resolve": resolve_str,
            "_resolve_secs": resolve_sort_secs,
            "end_date": end_date,
            "status": status, "postpone": postpone,
            "entry_ts": entry_ts_fmt, "legs": legs,
            "guaranteed": (guaranteed * 100.0).round() / 100.0,
            "unrealized_pnl": (unrealized_pnl * 100.0).round() / 100.0,
            "scenarios": [],  // TODO: compute sell-arb scenarios
        }));
    }
    // Sort open positions by score descending, then re-number
    positions.sort_by(|a, b| {
        let sb = b["score"].as_f64().unwrap_or(0.0);
        let sa = a["score"].as_f64().unwrap_or(0.0);
        sb.total_cmp(&sa)
    });
    for (i, pos) in positions.iter_mut().enumerate() {
        pos["idx"] = serde_json::Value::from(i + 1);
    }
    // Build aggregates: group all legs across positions by market name
    let mut agg_map: std::collections::HashMap<&str, (&str, f64, f64, f64, usize)> = std::collections::HashMap::new();
    for (pidx, pos) in positions.iter().enumerate() {
        if let Some(legs) = pos["legs"].as_array() {
            for leg in legs {
                let name = leg["name"].as_str().unwrap_or("?");
                let side = leg["side"].as_str().unwrap_or("?");
                let bet = leg["bet"].as_f64().unwrap_or(0.0);
                let price = leg["price"].as_f64().unwrap_or(0.0);
                let payout = leg["payout"].as_f64().unwrap_or(0.0);
                let entry = agg_map.entry(name).or_insert((side, 0.0, 0.0, 0.0, pidx + 1));
                entry.1 += bet;
                entry.2 = price; // last price (simplification)
                entry.3 += payout;
            }
        }
    }
    let mut aggregates: Vec<Value> = agg_map.iter().map(|(name, (side, bet, price, payout, pidx))| {
        json!({"name": name, "side": side, "total_bet": bet, "avg_price": price, "payout": payout, "pos_idx": pidx})
    }).collect();
    aggregates.sort_by(|a, b| a["pos_idx"].as_u64().unwrap_or(0)
        .cmp(&b["pos_idx"].as_u64().unwrap_or(0)));
    let agg_total: f64 = aggregates.iter().map(|a| a["total_bet"].as_f64().unwrap_or(0.0)).sum();

    json!({ "positions": positions, "pos_count": positions.len(),
            "aggregates": aggregates, "agg_market_count": aggregates.len(), "agg_total": agg_total })
}

fn build_opportunities(s: &DashboardState) -> Value {
    let opps = s.recent_opps.lock().clone();
    let held_cids = s.positions.lock().get_held_constraint_ids();
    let mut result = Vec::new();
    for (idx, opp) in opps.iter().enumerate() {
        let profit_pct = opp["expected_profit_pct"].as_f64().unwrap_or(0.0) * 100.0;
        // strategy and method live inside metadata (from evaluate_batch)
        let meta = &opp["metadata"];
        let method = meta["method"].as_str().unwrap_or("");
        let strategy = fmt_strategy("", method);
        let is_sell = meta["is_sell"].as_bool()
            .unwrap_or_else(|| method.to_lowercase().contains("sell"));

        let constraint_id = opp["constraint_id"].as_str().unwrap_or("");
        let is_held = held_cids.contains(constraint_id);

        let market_names: Vec<String> = opp["market_names"].as_array()
            .map(|a| a.iter().map(|v| v.as_str().unwrap_or("?").to_string()).collect())
            .unwrap_or_default();
        let market_ids: Vec<String> = opp["market_ids"].as_array()
            .map(|a| a.iter().map(|v| v.as_str().unwrap_or("").to_string()).collect())
            .unwrap_or_default();
        let short_name = market_names.first().map(|s| s.chars().take(60).collect::<String>())
            .unwrap_or_else(|| "?".into());

        // Build market_id → name lookup from parallel arrays
        let mut name_by_id: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        for (i, mid) in market_ids.iter().enumerate() {
            if let Some(name) = market_names.get(i) {
                name_by_id.insert(mid.as_str(), name.as_str());
            }
        }

        // Resolution hours — prefer constraint end_date_ts over opp field (with P95 delay)
        let end_date_ts = s.constraints.get(constraint_id)
            .map(|c| c.end_date_ts).unwrap_or(0.0);
        let now_ts = chrono::Utc::now().timestamp() as f64;
        let (resolves_str, _resolve_sort_secs) = resolve_with_delay(end_date_ts, now_ts, &market_names, &s.delay_table);
        let hours = _resolve_sort_secs / 3600.0;

        // Legs — use actual NO prices for sell arbs (not 1 - YES_ask)
        let mut legs = Vec::new();
        if let Some(bets) = opp["optimal_bets"].as_object() {
            for (mid, bet_val) in bets {
                let bet = bet_val.as_f64().unwrap_or(0.0);
                let name = name_by_id.get(mid.as_str()).copied().unwrap_or("?");
                let yes_price = opp["current_prices"].get(mid)
                    .and_then(|v| v.as_f64()).unwrap_or(0.0);
                let no_price = opp["current_no_prices"].get(mid)
                    .and_then(|v| v.as_f64())
                    .unwrap_or_else(|| (1.0 - yes_price).max(0.001));
                let side = if is_sell { "NO" } else { "YES" };
                let (sh, actual_price) = if is_sell {
                    let s = if no_price > 0.0 { bet / no_price } else { 0.0 };
                    (s, no_price)
                } else {
                    let s = if yes_price > 0.0 { bet / yes_price } else { 0.0 };
                    (s, yes_price)
                };
                legs.push(json!({
                    "name": name, "bet": (bet * 100.0).round() / 100.0,
                    "side": side, "price": (actual_price * 1000.0).round() / 1000.0,
                    "shares": format!("{:.2}", sh),
                    "payout": (sh * 100.0).round() / 100.0,
                }));
            }
        }

        let total_cap = opp["total_capital_required"].as_f64().unwrap_or(0.0);
        let net_profit = opp["net_profit"].as_f64()
            .or_else(|| opp["expected_profit"].as_f64()).unwrap_or(0.0);
        let fees = opp["fees_estimated"].as_f64().unwrap_or(0.0);

        // Guaranteed payout from legs (same logic as positions)
        let guaranteed_payout = compute_guaranteed_payout(&legs, is_sell, total_cap + net_profit);

        // Score = profit_pct / hours × 1000
        let score_val = if hours > 0.01 {
            (profit_pct / 100.0 / hours) * 1000.0
        } else { 0.0 };

        result.push(json!({
            "idx": idx + 1, "short_name": short_name, "full_names": market_names,
            "profit_pct": (profit_pct * 10.0).round() / 10.0,
            "resolve": resolves_str, "strategy": strategy,
            "score": (score_val * 100.0).round() / 100.0,
            "legs": legs, "scenarios": [],
            "guaranteed_payout": (guaranteed_payout * 100.0).round() / 100.0,
            "net_profit": net_profit,
            "fees": fees,
            "is_held": is_held,
        }));
    }
    json!({ "opportunities": result, "total_found": result.len() })
}

fn build_closed(s: &DashboardState) -> Value {
    // Clone closed positions under lock, then release
    let closed_snapshot: Vec<crate::position::Position> = {
        let pm = s.positions.lock();
        pm.closed_positions().to_vec()
    };
    let mut resolved = Vec::new();
    let mut proactive_exit = Vec::new();
    let mut replaced_profit = Vec::new();
    let mut replaced_loss = Vec::new();
    let mut replaced_even = Vec::new();

    for p in &closed_snapshot {
        let actual = p.actual_profit;
        let deployed = p.total_capital;
        let reason = p.metadata.get("close_reason")
            .and_then(|v| v.as_str()).unwrap_or("resolved");
        let strategy = p.metadata.get("strategy")
            .and_then(|v| v.as_str()).unwrap_or("?");
        let method = p.metadata.get("method")
            .and_then(|v| v.as_str()).unwrap_or("");
        let is_sell = p.metadata.get("is_sell")
            .and_then(|v| v.as_bool())
            .unwrap_or_else(|| method.to_lowercase().contains("sell"));

        let mkt_vals: Vec<&crate::position::MarketLeg> = p.markets.values().collect();
        let short_name = mkt_vals.first()
            .map(|m| m.name.chars().take(40).collect::<String>())
            .unwrap_or_else(|| "?".into());
        let full_names: Vec<String> = mkt_vals.iter()
            .map(|m| m.name.clone()).collect();

        // Parse entry timestamp
        let entry_epoch = parse_entry_ts_str(&p.entry_timestamp);

        let close_ts = p.close_timestamp.unwrap_or(0.0);
        let hold_secs = if entry_epoch > 0.0 && close_ts > 0.0 { close_ts - entry_epoch } else { -1.0 };
        let hold_str = if hold_secs >= 0.0 {
            if hold_secs < 60.0 { format!("{:.0}s", hold_secs) }
            else if hold_secs < 3600.0 { format!("{:.0}m", hold_secs / 60.0) }
            else if hold_secs < 86400.0 { format!("{:.1}h", hold_secs / 3600.0) }
            else { format!("{:.1}d", hold_secs / 86400.0) }
        } else { "?".into() };
        let close_dt_str = fmt_ts_sec(close_ts);

        // Build legs — read shares from position data (same as open positions)
        let mut legs = Vec::new();
        for (_mid, leg) in &p.markets {
            legs.push(build_leg_json(leg, is_sell));
        }

        let deployed_r = (deployed * 100.0).round() / 100.0;
        let pnl_r = (actual * 100.0).round() / 100.0;
        let pnl_pct = if deployed_r.abs() > 0.001 { (pnl_r / deployed_r * 1000.0).round() / 10.0 } else { 0.0 };
        let row = json!({
            "name": short_name, "deployed": deployed_r,
            "pnl": pnl_r, "pnl_pct": pnl_pct, "hold": hold_str,
            "closed_at": close_dt_str, "strategy": fmt_strategy(strategy, method),
            "legs": legs, "full_names": full_names,
            "_sort_ts": close_ts, "_hold_secs": hold_secs,
        });

        match reason {
            "resolved" | "expired" => resolved.push(row),
            "proactive_exit" => proactive_exit.push(row),
            "replaced" => {
                if actual > 0.01 { replaced_profit.push(row) }
                else if actual < -0.01 { replaced_loss.push(row) }
                else { replaced_even.push(row) }
            }
            _ => resolved.push(row),
        }
    }

    // Sort each category by close time descending
    for cat in [&mut resolved, &mut proactive_exit, &mut replaced_profit, &mut replaced_loss, &mut replaced_even] {
        cat.sort_by(|a, b| {
            let tb = b["_sort_ts"].as_f64().unwrap_or(0.0);
            let ta = a["_sort_ts"].as_f64().unwrap_or(0.0);
            tb.total_cmp(&ta)
        });
    }

    let total = resolved.len() + proactive_exit.len() + replaced_profit.len() + replaced_loss.len() + replaced_even.len();
    json!({
        "total_closed": total,
        "categories": {
            "resolved": { "label": "Resolved", "rows": resolved, "collapsed": resolved.len() > 10 },
            "proactive_exit": { "label": "Proactive Exit", "rows": proactive_exit, "collapsed": false },
            "replaced_profit": { "label": "Replaced (Profit)", "rows": replaced_profit, "collapsed": false },
            "replaced_loss": { "label": "Replaced (Loss)", "rows": replaced_loss, "collapsed": false },
            "replaced_even": { "label": "Closed Early", "rows": replaced_even, "collapsed": true },
        }
    })
}

fn build_system(s: &DashboardState) -> Value {
    let m = s.engine_metrics.lock().clone();
    let (q_urg, q_bg) = s.eval_queue.depths();
    let n_constraints = s.constraints.len();
    let n_markets = s.book.live_count();
    let pm = s.positions.lock();

    // Latency breakdown (only populated when instrumentation enabled)
    let lat_snap = s.latency.snapshot();
    let lat_enabled = s.latency.is_enabled();

    json!({
        "scanner": { "status": m.scanner_status, "ts": m.scanner_ts },
        "engine": {
            "status": m.engine_status, "ts": m.engine_ts,
            "capital": pm.current_capital(),
            "positions": pm.open_count(),
        },
        "metrics": {
            "has_rust": true,
            "constraints": n_constraints,
            "markets_total": n_markets,
            "iteration": m.iteration,
            "ws_subscribed": m.ws_subscribed,
            "ws_msgs": m.ws_total_msgs,
            "ws_live": m.ws_live_books,
            "queue_urgent": q_urg,
            "queue_background": q_bg,
            "lat_p50": m.lat_p50_us,
            "lat_p95": m.lat_p95_us,
            "lat_max": m.lat_max_us,
        },
        "latency_breakdown": {
            "enabled": lat_enabled,
            "ws_network":    { "p50": lat_snap.ws_network.p50 as u64,    "p95": lat_snap.ws_network.p95 as u64,    "max": lat_snap.ws_network.max as u64,    "n": lat_snap.ws_network.count },
            "ws_to_queue":   { "p50": lat_snap.ws_to_queue.p50 as u64,   "p95": lat_snap.ws_to_queue.p95 as u64,   "max": lat_snap.ws_to_queue.max as u64,   "n": lat_snap.ws_to_queue.count },
            "queue_wait":    { "p50": lat_snap.queue_wait.p50 as u64,    "p95": lat_snap.queue_wait.p95 as u64,    "max": lat_snap.queue_wait.max as u64,    "n": lat_snap.queue_wait.count },
            "eval_batch":    { "p50": lat_snap.eval_batch.p50 as u64,    "p95": lat_snap.eval_batch.p95 as u64,    "max": lat_snap.eval_batch.max as u64,    "n": lat_snap.eval_batch.count },
            "eval_to_entry": { "p50": lat_snap.eval_to_entry.p50 as u64, "p95": lat_snap.eval_to_entry.p95 as u64, "max": lat_snap.eval_to_entry.max as u64, "n": lat_snap.eval_to_entry.count },
            "e2e":           { "p50": lat_snap.e2e.p50 as u64,           "p95": lat_snap.e2e.p95 as u64,           "max": lat_snap.e2e.max as u64,           "n": lat_snap.e2e.count },
        },
    })
}

fn build_monitor(s: &DashboardState, full: bool) -> Value {
    let mut mon = s.monitor.lock();

    // Collect fresh metrics each time we build
    mon.collect_system_metrics();

    // App metrics from engine state (use EngineMetrics which includes tiered WS stats)
    let (q_urg, q_bg) = s.eval_queue.depths();
    let n_constraints = s.constraints.len();
    let n_markets = s.book.live_count();
    let m = s.engine_metrics.lock();
    mon.collect_app_metrics(
        m.ws_subscribed, m.ws_total_msgs, m.ws_live_books,
        n_constraints, n_markets,
        m.lat_p50_us, m.lat_p95_us,
        q_urg, q_bg,
    );
    drop(m);

    // Financial metrics
    let pm = s.positions.lock();
    let cap = pm.current_capital();
    let open_snapshot: Vec<crate::position::Position> = pm.open_positions().values().cloned().collect();
    let deployed: f64 = open_snapshot.iter().map(|p| p.total_capital).sum();
    let total_value = cap + deployed;
    let perf = pm.get_performance_metrics();
    let realized = perf.get("total_actual_profit").copied().unwrap_or(0.0);
    let closed_snapshot: Vec<crate::position::Position> = pm.closed_positions().to_vec();
    drop(pm);

    // Unrealized P&L: mark-to-market using live bids
    let mut unrealized = 0.0f64;
    for p in &open_snapshot {
        let cid = p.metadata.get("constraint_id").and_then(|v| v.as_str()).unwrap_or("");
        let constraint = s.constraints.get(cid);
        let mut sale_proceeds = 0.0f64;
        for (mid, leg) in &p.markets {
            let live_bid = constraint.as_ref().and_then(|c| {
                c.markets.iter().find(|mref| mref.market_id == *mid).and_then(|mref| {
                    let asset_id = if leg.outcome == "no" { &mref.no_asset_id } else { &mref.yes_asset_id };
                    let bid = s.book.get_best_bid(asset_id);
                    if bid > 0.0 { Some(bid) } else { None }
                })
            });
            sale_proceeds += leg.shares * live_bid.unwrap_or(leg.entry_price);
        }
        unrealized += sale_proceeds - p.total_capital;
    }

    mon.collect_financial_metrics(total_value, cap, deployed, realized, unrealized);

    // Build the JSON (pass separate log ring to avoid lock contention)
    let mut log_ring = s.log_ring.lock();
    let mut result = mon.build_json(full, &mut log_ring);
    drop(log_ring);

    // Add financial summary from closed positions
    let financial = mon.compute_financial_summary(&closed_snapshot);
    if let Value::Object(ref mut map) = result {
        map.insert("financial".to_string(), financial);
    }

    // Add POL gas balance (C1.1)
    let m = s.engine_metrics.lock();
    if let Some(bal) = m.pol_balance {
        if let Value::Object(ref mut map) = result {
            map.insert("pol_balance".to_string(), json!(bal));
        }
    }
    drop(m);

    // Add latency breakdown (migrated from system section)
    let lat_snap = s.latency.snapshot();
    let lat_enabled = s.latency.is_enabled();
    if let Value::Object(ref mut map) = result {
        map.insert("latency_breakdown".to_string(), json!({
            "enabled": lat_enabled,
            "ws_network":    { "p50": lat_snap.ws_network.p50 as u64,    "p95": lat_snap.ws_network.p95 as u64,    "max": lat_snap.ws_network.max as u64,    "n": lat_snap.ws_network.count },
            "ws_to_queue":   { "p50": lat_snap.ws_to_queue.p50 as u64,   "p95": lat_snap.ws_to_queue.p95 as u64,   "max": lat_snap.ws_to_queue.max as u64,   "n": lat_snap.ws_to_queue.count },
            "queue_wait":    { "p50": lat_snap.queue_wait.p50 as u64,    "p95": lat_snap.queue_wait.p95 as u64,    "max": lat_snap.queue_wait.max as u64,    "n": lat_snap.queue_wait.count },
            "eval_batch":    { "p50": lat_snap.eval_batch.p50 as u64,    "p95": lat_snap.eval_batch.p95 as u64,    "max": lat_snap.eval_batch.max as u64,    "n": lat_snap.eval_batch.count },
            "eval_to_entry": { "p50": lat_snap.eval_to_entry.p50 as u64, "p95": lat_snap.eval_to_entry.p95 as u64, "max": lat_snap.eval_to_entry.max as u64, "n": lat_snap.eval_to_entry.count },
            "e2e":           { "p50": lat_snap.e2e.p50 as u64,           "p95": lat_snap.e2e.p95 as u64,           "max": lat_snap.e2e.max as u64,           "n": lat_snap.e2e.count },
        }));
    }

    result
}
