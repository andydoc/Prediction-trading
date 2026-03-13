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
use std::collections::HashMap;
use std::time::Duration;
use axum::{Router, Json, response::{Html, IntoResponse, Sse, sse}};
use axum::extract::State;
use tokio_stream::StreamExt;
use serde_json::{json, Value};
use parking_lot::Mutex;

use crate::position::PositionManager;
use crate::book::BookMirror;
use crate::queue::EvalQueue;
use crate::eval::ConstraintStore;
use crate::ws::WsManager;

/// Shared state passed to all axum handlers via Arc.
#[derive(Clone)]
pub struct DashboardState {
    pub positions: Arc<Mutex<PositionManager>>,
    pub book: Arc<BookMirror>,
    pub eval_queue: Arc<EvalQueue>,
    pub ws: Arc<WsManager>,
    pub constraints: Arc<ConstraintStore>,
    /// Engine metrics updated by Python each stats cycle
    pub engine_metrics: Arc<Mutex<EngineMetrics>>,
    /// Recent opportunities (last batch from evaluate_batch)
    pub recent_opps: Arc<Mutex<Vec<Value>>>,
    /// Mode: "shadow" or "live"
    pub mode: String,
    /// Start time (for uptime display)
    pub start_time: chrono::DateTime<chrono::Utc>,
}

/// Metrics updated by the Python engine loop each stats cycle.
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
}

/// Static HTML — loaded at compile time from the extracted template.
const DASHBOARD_HTML: &str = include_str!("../static/dashboard.html");

/// Start the dashboard server on the given port (default 5556).
/// Must be called from within a tokio runtime (spawned as async task).
pub async fn start(state: DashboardState, port: u16) {
    let app = Router::new()
        .route("/", axum::routing::get(handle_html))
        .route("/stream", axum::routing::get(handle_sse))
        .route("/state", axum::routing::get(handle_state))
        .with_state(Arc::new(state));

    let listener = match tokio::net::TcpListener::bind(
        format!("0.0.0.0:{}", port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("Dashboard failed to bind port {}: {}", port, e);
            return;
        }
    };
    tracing::info!("Dashboard server started on port {}", port);
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
        .replace("{{MODE_CLASS}}", &format!("mode-{}", s.mode));
    Html(html)
}

async fn handle_state(State(s): State<Arc<DashboardState>>) -> Json<Value> {
    Json(build_state_snapshot(&s))
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
            }

            // Opportunities: every 15s (every 3rd tick)
            if tick % 3 == 0 {
                yield Ok(sse::Event::default().event("opportunities").data(
                    serde_json::to_string(&build_opportunities(&s)).unwrap_or_default()));
            }

            // Closed: every 60s (every 12th tick)
            if tick % 12 == 0 {
                yield Ok(sse::Event::default().event("closed").data(
                    serde_json::to_string(&build_closed(&s)).unwrap_or_default()));
            }
        }
    };

    Sse::new(stream).keep_alive(
        sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping")
    )
}

// =====================================================================
// Data builders — read directly from in-memory Rust structs
// =====================================================================

fn build_state_snapshot(s: &DashboardState) -> Value {
    let pm = s.positions.lock();
    let open = pm.get_open_positions_json();
    let closed = pm.get_closed_positions_json();
    let perf = pm.get_performance_metrics();
    json!({
        "current_capital": pm.current_capital(),
        "initial_capital": pm.initial_capital(),
        "open_positions": open.iter().map(|j| serde_json::from_str::<Value>(j).unwrap_or_default()).collect::<Vec<_>>(),
        "closed_positions": closed.iter().map(|j| serde_json::from_str::<Value>(j).unwrap_or_default()).collect::<Vec<_>>(),
        "performance": perf,
    })
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

/// Parse entry_timestamp which can be ISO string, Unix float string, or raw f64.
fn parse_entry_ts(val: &Value) -> f64 {
    if let Some(s) = val.as_str() {
        // Try ISO first
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
            return dt.timestamp() as f64;
        }
        // Try Unix float string like "1773397067.088904"
        if let Ok(f) = s.parse::<f64>() {
            return f;
        }
        0.0
    } else {
        val.as_f64().unwrap_or(0.0)
    }
}

/// Format strategy for display
fn fmt_strategy(strategy: &str, method: &str) -> String {
    if method.contains("sell") { "Mutex Sell All".into() }
    else if method.contains("buy") { "Mutex Buy All".into() }
    else if strategy == "arb_sell" { "Mutex Sell All".into() }
    else if strategy == "arb_buy" { "Mutex Buy All".into() }
    else { strategy.to_string() }
}

fn build_stats(s: &DashboardState) -> Value {
    let pm = s.positions.lock();
    let cap = pm.current_capital();
    let init_cap = pm.initial_capital();
    let perf = pm.get_performance_metrics();
    let open_count = pm.open_count();
    let closed_count = pm.closed_count();

    // Calculate deployed capital and fees from open positions
    let mut deployed = 0.0f64;
    let mut total_fees = 0.0f64;
    let mut first_entry_ts = f64::MAX;
    for pos_json in pm.get_open_positions_json() {
        if let Ok(p) = serde_json::from_str::<Value>(&pos_json) {
            deployed += p["total_capital"].as_f64().unwrap_or(0.0);
            total_fees += p["fees_paid"].as_f64().unwrap_or(0.0);
            // Track first entry
            let ts = parse_entry_ts(&p["entry_timestamp"]);
            if ts > 0.0 && ts < first_entry_ts { first_entry_ts = ts; }
        }
    }
    // Also check closed for first trade + fees
    let closed_jsons = pm.get_closed_positions_json();
    let total_realized = perf.get("total_actual_profit").copied().unwrap_or(0.0);
    drop(pm); // Release lock — no more position reads needed
    for cj in &closed_jsons {
        if let Ok(p) = serde_json::from_str::<Value>(cj) {
            total_fees += p["fees_paid"].as_f64().unwrap_or(0.0);
            let ts = parse_entry_ts(&p["entry_timestamp"]);
            if ts > 0.0 && ts < first_entry_ts { first_entry_ts = ts; }
        }
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
    let annualized_ret = if days_running > 1.0 {
        ((total_value / init_cap).powf(365.0 / days_running) - 1.0) * 100.0
    } else { 0.0 };
    let annualized_str = if days_running > 1.0 {
        format!("{:+.0}%", annualized_ret)
    } else { "N/A".into() };
    let now_str = chrono::Utc::now().format("%d/%m/%Y %H:%M:%S").to_string();
    let start_str = s.start_time.format("%d/%m/%Y %H:%M").to_string();

    json!({
        "cash": cap, "deployed": deployed, "total_value": total_value,
        "init_cap": init_cap, "fees": total_fees, "ret_pct": ret_pct,
        "trades": trades, "open_count": open_count, "closed_count": closed_count,
        "realized": total_realized,
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

fn build_positions(s: &DashboardState) -> Value {
    let mut positions = Vec::new();
    let open_jsons = s.positions.lock().get_open_positions_json();
    for (idx, pos_json) in open_jsons.iter().enumerate() {
        let p: Value = match serde_json::from_str(pos_json) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let markets = p["markets"].as_object();
        let mkt_vals: Vec<&Value> = markets.map(|m| m.values().collect()).unwrap_or_default();
        let short_name = mkt_vals.first()
            .and_then(|m| m["name"].as_str())
            .unwrap_or("?")
            .chars().take(40).collect::<String>();
        let full_names: Vec<String> = mkt_vals.iter()
            .map(|m| m["name"].as_str().unwrap_or("?").to_string())
            .collect();

        let strategy = p["metadata"]["strategy"].as_str().unwrap_or("?");
        let method = p["metadata"]["method"].as_str().unwrap_or("");
        let is_sell = method.to_lowercase().contains("sell");
        let total_cap = p["total_capital"].as_f64().unwrap_or(0.0);
        let exp_profit = p["expected_profit"].as_f64().unwrap_or(0.0);
        let exp_pct = if total_cap > 0.0 { exp_profit / total_cap * 100.0 } else { 0.0 };

        let strategy = p["metadata"]["strategy"].as_str().unwrap_or("?");
        let method = p["metadata"]["method"].as_str().unwrap_or("");
        let strategy_fmt = fmt_strategy(strategy, method);
        let is_sell = method.to_lowercase().contains("sell");

        // Entry timestamp
        let entry_epoch = parse_entry_ts(&p["entry_timestamp"]);
        let entry_ts_fmt = fmt_ts(entry_epoch);

        // Build legs
        let mut legs = Vec::new();
        if let Some(mkts) = markets {
            for (_mid, mdata) in mkts {
                let name = mdata["name"].as_str().unwrap_or("?");
                let ep = mdata["entry_price"].as_f64().unwrap_or(0.0);
                let bet = mdata["bet_amount"].as_f64().unwrap_or(0.0);
                let (side, shares, payout) = if is_sell {
                    let no_price = 1.0 - ep;
                    let sh = if no_price > 0.0 { bet / no_price } else { 0.0 };
                    ("NO", sh, sh)
                } else {
                    let sh = if ep > 0.0 { bet / ep } else { 0.0 };
                    ("YES", sh, sh)
                };
                legs.push(json!({
                    "name": name, "bet": (bet * 100.0).round() / 100.0,
                    "side": side, "price": (ep * 1000.0).round() / 1000.0,
                    "shares": format!("{:.2}", shares),
                    "payout": (payout * 100.0).round() / 100.0,
                }));
            }
        }

        // Resolution date — look up from constraint store (positions don't store end_date)
        let cid = p["metadata"]["constraint_id"].as_str().unwrap_or("");
        let end_date_ts = s.constraints.get(cid)
            .map(|c| c.end_date_ts).unwrap_or(0.0);
        let end_date = if end_date_ts > 0.0 {
            chrono::DateTime::from_timestamp(end_date_ts as i64, 0)
                .map(|dt| dt.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "?".into())
        } else { "?".into() };

        let pp = &p["metadata"]["postponement"];
        let postponed = pp.is_object() && pp["effective_date"].is_string();
        let pp_date = if postponed {
            pp["effective_date"].as_str().unwrap_or("")
        } else { "" };

        // Resolution date formatting + hours remaining
        let status = if postponed { "postponed" } else { "monitoring" };
        let now_ts = chrono::Utc::now().timestamp() as f64;
        let hours_remaining = if end_date_ts > 0.0 { (end_date_ts - now_ts) / 3600.0 } else { 0.0 };
        let resolve_str = if end_date_ts > 0.0 {
            if hours_remaining < 24.0 {
                format!("{:.1}h", hours_remaining.max(0.0))
            } else {
                format!("{:.1}d", hours_remaining / 24.0)
            }
        } else { "?".into() };

        // Score = profit_pct / hours_remaining
        let score_val = if hours_remaining > 0.01 {
            exp_pct / 100.0 / hours_remaining
        } else { 0.0 };

        // Postponement object (for JS)
        let postpone = if postponed {
            let pp_status = pp.get("status").and_then(|v| v.as_str()).unwrap_or("postponed");
            let pp_reason = pp.get("reason").and_then(|v| v.as_str()).unwrap_or("Postponed");
            Some(json!({ "status": pp_status, "reason": pp_reason, "date": pp_date }))
        } else { None };

        // Guaranteed payout: compute from legs (minimum scenario payout)
        let guaranteed = if is_sell && legs.len() > 1 {
            // Sell-all: when outcome i wins YES, NO_i loses, all others pay $1/share
            let share_vals: Vec<f64> = legs.iter()
                .map(|l| l["payout"].as_f64().unwrap_or(0.0))
                .collect();
            let total_shares: f64 = share_vals.iter().sum();
            let min_scenario = share_vals.iter()
                .map(|s| total_shares - s)  // lose this leg, keep all others
                .fold(f64::MAX, f64::min);
            min_scenario
        } else if !is_sell && legs.len() > 0 {
            // Buy-all: each leg pays shares if it wins, guaranteed = min(shares)
            legs.iter()
                .map(|l| l["payout"].as_f64().unwrap_or(0.0))
                .fold(f64::MAX, f64::min)
        } else {
            total_cap + exp_profit
        };

        positions.push(json!({
            "idx": idx + 1, "short_name": short_name, "full_names": full_names,
            "strategy": strategy_fmt, "score": (score_val * 1000.0 * 100.0).round() / 100.0,
            "total_cap": (total_cap * 100.0).round() / 100.0,
            "exp_profit": (exp_profit * 100.0).round() / 100.0,
            "exp_pct": (exp_pct * 10.0).round() / 10.0,
            "resolve": resolve_str, "end_date": end_date,
            "status": status, "postpone": postpone,
            "entry_ts": entry_ts_fmt, "legs": legs,
            "guaranteed": (guaranteed * 100.0).round() / 100.0,
            "scenarios": [],  // TODO: compute sell-arb scenarios
        }));
    }
    // Build aggregates: group all legs across positions by market name
    let mut agg_map: std::collections::HashMap<String, (String, f64, f64, f64, usize)> = std::collections::HashMap::new();
    for (pidx, pos) in positions.iter().enumerate() {
        if let Some(legs) = pos["legs"].as_array() {
            for leg in legs {
                let name = leg["name"].as_str().unwrap_or("?").to_string();
                let side = leg["side"].as_str().unwrap_or("?").to_string();
                let bet = leg["bet"].as_f64().unwrap_or(0.0);
                let price = leg["price"].as_f64().unwrap_or(0.0);
                let payout = leg["payout"].as_f64().unwrap_or(0.0);
                let entry = agg_map.entry(name.clone()).or_insert((side, 0.0, 0.0, 0.0, pidx + 1));
                entry.1 += bet;
                entry.2 = price; // last price (simplification)
                entry.3 += payout;
            }
        }
    }
    let mut aggregates: Vec<Value> = agg_map.iter().map(|(name, (side, bet, price, payout, pidx))| {
        json!({"name": name, "side": side, "total_bet": bet, "avg_price": price, "payout": payout, "pos_idx": pidx})
    }).collect();
    aggregates.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
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
        let is_sell = method.to_lowercase().contains("sell");

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

        // Resolution hours — prefer constraint end_date_ts over opp field
        let end_date_ts = s.constraints.get(constraint_id)
            .map(|c| c.end_date_ts).unwrap_or(0.0);
        let now_ts = chrono::Utc::now().timestamp() as f64;
        let hours = if end_date_ts > 0.0 {
            (end_date_ts - now_ts) / 3600.0
        } else {
            opp["hours_to_resolve"].as_f64().unwrap_or(0.0)
        };
        let resolves_str = if hours <= 0.0 { "past".into() }
            else if hours < 24.0 { format!("{:.1}h", hours) }
            else { format!("{:.1}d", hours / 24.0) };

        // Legs — prices are in "current_prices" (not "prices")
        let mut legs = Vec::new();
        if let Some(bets) = opp["optimal_bets"].as_object() {
            for (mid, bet_val) in bets {
                let bet = bet_val.as_f64().unwrap_or(0.0);
                let name = name_by_id.get(mid.as_str()).copied().unwrap_or("?");
                let price = opp["current_prices"].get(mid)
                    .and_then(|v| v.as_f64()).unwrap_or(0.0);
                let side = if is_sell { "NO" } else { "YES" };
                let sh = if is_sell {
                    if (1.0 - price) > 0.0 { bet / (1.0 - price) } else { 0.0 }
                } else {
                    if price > 0.0 { bet / price } else { 0.0 }
                };
                legs.push(json!({
                    "name": name, "bet": (bet * 100.0).round() / 100.0,
                    "side": side, "price": (price * 1000.0).round() / 1000.0,
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
        let guaranteed_payout = if is_sell && legs.len() > 1 {
            let share_vals: Vec<f64> = legs.iter()
                .map(|l| l["payout"].as_f64().unwrap_or(0.0)).collect();
            let total_shares: f64 = share_vals.iter().sum();
            share_vals.iter().map(|s| total_shares - s).fold(f64::MAX, f64::min)
        } else if !is_sell && legs.len() > 0 {
            legs.iter().map(|l| l["payout"].as_f64().unwrap_or(0.0)).fold(f64::MAX, f64::min)
        } else {
            total_cap + net_profit
        };

        // Score = profit_pct / hours × 1000
        let score_val = if hours > 0.01 {
            (profit_pct / 100.0 / hours) * 1000.0
        } else { 0.0 };

        result.push(json!({
            "idx": idx + 1, "short_name": short_name, "full_names": market_names,
            "profit_pct": (profit_pct * 10.0).round() / 10.0,
            "resolves": resolves_str, "strategy": strategy,
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
    let closed_jsons = s.positions.lock().get_closed_positions_json();
    let mut resolved = Vec::new();
    let mut replaced_profit = Vec::new();
    let mut replaced_loss = Vec::new();
    let mut replaced_even = Vec::new();

    for cj in &closed_jsons {
        let p: Value = match serde_json::from_str(cj) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let actual = p["actual_profit"].as_f64().unwrap_or(0.0);
        let deployed = p["total_capital"].as_f64().unwrap_or(0.0);
        let reason = p["metadata"]["close_reason"].as_str().unwrap_or("resolved");
        let strategy = p["metadata"]["strategy"].as_str().unwrap_or("?");
        let method = p["metadata"]["method"].as_str().unwrap_or("");
        let is_sell = method.to_lowercase().contains("sell");

        let markets = p["markets"].as_object();
        let mkt_vals: Vec<&Value> = markets.map(|m| m.values().collect()).unwrap_or_default();
        let short_name = mkt_vals.first()
            .and_then(|m| m["name"].as_str())
            .unwrap_or("?").chars().take(40).collect::<String>();
        let full_names: Vec<String> = mkt_vals.iter()
            .map(|m| m["name"].as_str().unwrap_or("?").to_string()).collect();

        // Parse entry timestamp
        let entry_ts = &p["entry_timestamp"];
        let entry_epoch = if let Some(s) = entry_ts.as_str() {
            chrono::DateTime::parse_from_rfc3339(s)
                .map(|dt| dt.timestamp() as f64)
                .unwrap_or_else(|_| s.parse::<f64>().unwrap_or(0.0))
        } else { entry_ts.as_f64().unwrap_or(0.0) };

        let close_ts = p["close_timestamp"].as_f64().unwrap_or(0.0);
        let hold_str = if entry_epoch > 0.0 && close_ts > 0.0 {
            let secs = close_ts - entry_epoch;
            if secs < 3600.0 { format!("{:.0}m", secs / 60.0) }
            else if secs < 86400.0 { format!("{:.1}h", secs / 3600.0) }
            else { format!("{:.1}d", secs / 86400.0) }
        } else { "?".into() };
        let close_dt_str = fmt_ts_sec(close_ts);

        // Build legs
        let mut legs = Vec::new();
        if let Some(mkts) = markets {
            for (_mid, mdata) in mkts {
                let name = mdata["name"].as_str().unwrap_or("?");
                let ep = mdata["entry_price"].as_f64().unwrap_or(0.0);
                let bet = mdata["bet_amount"].as_f64().unwrap_or(0.0);
                let (side, shares, payout) = if is_sell {
                    let np = 1.0 - ep;
                    let sh = if np > 0.0 { bet / np } else { 0.0 };
                    ("NO", sh, sh)
                } else {
                    let sh = if ep > 0.0 { bet / ep } else { 0.0 };
                    ("YES", sh, sh)
                };
                legs.push(json!({
                    "name": name, "bet": (bet * 100.0).round() / 100.0,
                    "side": side, "price": (ep * 1000.0).round() / 1000.0,
                    "shares": format!("{:.2}", shares),
                    "payout": (payout * 100.0).round() / 100.0,
                }));
            }
        }

        let row = json!({
            "name": short_name, "deployed": (deployed * 100.0).round() / 100.0,
            "pnl": (actual * 100.0).round() / 100.0, "hold": hold_str,
            "closed_at": close_dt_str, "strategy": strategy,
            "legs": legs, "full_names": full_names,
            "_sort_ts": close_ts,
        });

        match reason {
            "resolved" | "expired" => resolved.push(row),
            "replaced" | "proactive_exit" => {
                if actual > 0.01 { replaced_profit.push(row) }
                else if actual < -0.01 { replaced_loss.push(row) }
                else { replaced_even.push(row) }
            }
            _ => resolved.push(row),
        }
    }

    // Sort each category by close time descending
    for cat in [&mut resolved, &mut replaced_profit, &mut replaced_loss, &mut replaced_even] {
        cat.sort_by(|a, b| b["_sort_ts"].as_f64().unwrap_or(0.0)
            .partial_cmp(&a["_sort_ts"].as_f64().unwrap_or(0.0)).unwrap());
    }

    let total = resolved.len() + replaced_profit.len() + replaced_loss.len() + replaced_even.len();
    json!({
        "total_closed": total,
        "categories": {
            "resolved": { "label": "Resolved", "rows": resolved, "collapsed": resolved.len() > 10 },
            "replaced_profit": { "label": "Replaced (Profit)", "rows": replaced_profit, "collapsed": false },
            "replaced_loss": { "label": "Replaced (Loss)", "rows": replaced_loss, "collapsed": false },
            "replaced_even": { "label": "Closed Early", "rows": replaced_even, "collapsed": true },
        }
    })
}

fn build_system(s: &DashboardState) -> Value {
    let m = s.engine_metrics.lock().clone();
    let ws = s.ws.stats();
    let (q_urg, q_bg) = s.eval_queue.depths();
    let n_constraints = s.constraints.len();
    let n_markets = s.book.len();
    let pm = s.positions.lock();

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
            "ws_subscribed": ws.subscribed,
            "ws_msgs": ws.total_msgs,
            "ws_live": ws.live_books,
            "queue_urgent": q_urg,
            "queue_background": q_bg,
            "lat_p50": m.lat_p50_us,
            "lat_p95": m.lat_p95_us,
            "lat_max": m.lat_max_us,
        },
    })
}
