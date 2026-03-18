//! Backend for the dashboard Monitor tab.
//!
//! Collects system, application, and financial metrics at regular intervals,
//! stores them in ring-buffer time series, and builds JSON payloads for the
//! dashboard (full snapshot or incremental delta).

use std::collections::{HashMap, VecDeque};
use serde_json::{json, Value};
use sysinfo::System;

// ---------------------------------------------------------------------------
// TimeSeries — ring buffer of (timestamp, value) pairs
// ---------------------------------------------------------------------------

const MAX_SAMPLES: usize = 60_480; // 7 days at 10 s intervals

pub struct TimeSeries {
    buf: VecDeque<(f64, f64)>,
}

impl TimeSeries {
    pub fn new() -> Self {
        Self {
            buf: VecDeque::with_capacity(1024),
        }
    }

    pub fn push(&mut self, ts: f64, value: f64) {
        if self.buf.len() >= MAX_SAMPLES {
            self.buf.pop_front();
        }
        self.buf.push_back((ts, value));
    }

    pub fn latest(&self) -> Option<f64> {
        self.buf.back().map(|&(_, v)| v)
    }

    /// Return entries with ts >= `since_ts` as `[ts, value]` pairs.
    pub fn as_json_array(&self, since_ts: f64) -> Vec<[f64; 2]> {
        self.buf
            .iter()
            .filter(|&&(ts, _)| ts >= since_ts)
            .map(|&(ts, v)| [ts, v])
            .collect()
    }
}

// ---------------------------------------------------------------------------
// LogEntry
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct LogEntry {
    pub ts: f64,
    pub level: String,
    pub target: String,
    pub message: String,
}

impl LogEntry {
    fn to_json(&self) -> Value {
        json!({
            "ts": self.ts,
            "level": self.level,
            "target": self.target,
            "message": self.message,
        })
    }
}

// ---------------------------------------------------------------------------
// MonitorState
// ---------------------------------------------------------------------------

const MAX_LOG_ENTRIES: usize = 2_000;

pub struct MonitorState {
    // System metrics
    pub cpu_pct: TimeSeries,
    pub mem_used_mb: TimeSeries,
    pub mem_total_mb: f64,
    pub disk_used_gb: TimeSeries,
    pub disk_total_gb: f64,

    // App metrics
    pub total_markets: TimeSeries,
    pub total_constraints: TimeSeries,
    pub ws_msg_rate: TimeSeries,
    pub ws_connections: TimeSeries,
    pub lat_p50: TimeSeries,
    pub lat_p95: TimeSeries,
    pub queue_depth: TimeSeries,

    // Financial metrics
    pub total_value: TimeSeries,
    pub capital_deployed_pct: TimeSeries,
    pub realized_pnl: TimeSeries,
    pub unrealized_pnl: TimeSeries,
    pub drawdown_pct: TimeSeries,

    // Log buffer
    pub log_buffer: VecDeque<LogEntry>,

    // Tracking state
    pub peak_value: f64,
    pub prev_ws_msgs: u64,
    pub prev_sample_ts: f64,

    // sysinfo handle (reused across samples)
    pub sys: System,
}

impl MonitorState {
    pub fn new() -> Self {
        Self {
            cpu_pct: TimeSeries::new(),
            mem_used_mb: TimeSeries::new(),
            mem_total_mb: 0.0,
            disk_used_gb: TimeSeries::new(),
            disk_total_gb: 0.0,

            total_markets: TimeSeries::new(),
            total_constraints: TimeSeries::new(),
            ws_msg_rate: TimeSeries::new(),
            ws_connections: TimeSeries::new(),
            lat_p50: TimeSeries::new(),
            lat_p95: TimeSeries::new(),
            queue_depth: TimeSeries::new(),

            total_value: TimeSeries::new(),
            capital_deployed_pct: TimeSeries::new(),
            realized_pnl: TimeSeries::new(),
            unrealized_pnl: TimeSeries::new(),
            drawdown_pct: TimeSeries::new(),

            log_buffer: VecDeque::with_capacity(MAX_LOG_ENTRIES),

            peak_value: 0.0,
            prev_ws_msgs: 0,
            prev_sample_ts: 0.0,

            sys: System::new(),
        }
    }

    // -- System metrics -----------------------------------------------------

    pub fn collect_system_metrics(&mut self) {
        let ts = now_ts();

        self.sys.refresh_cpu_all();
        self.sys.refresh_memory();

        // CPU — average across all cores
        let cpus = self.sys.cpus();
        let cpu = if cpus.is_empty() {
            0.0
        } else {
            cpus.iter().map(|c| c.cpu_usage() as f64).sum::<f64>() / cpus.len() as f64
        };
        self.cpu_pct.push(ts, cpu);

        // Memory
        let mem_used = self.sys.used_memory() as f64 / (1024.0 * 1024.0);
        let mem_total = self.sys.total_memory() as f64 / (1024.0 * 1024.0);
        self.mem_used_mb.push(ts, mem_used);
        self.mem_total_mb = mem_total;

        // Disk — root mount (first disk or "/" on Linux, "C:" on Windows)
        let disks = sysinfo::Disks::new_with_refreshed_list();
        let root_disk = disks.list().iter().find(|d| {
            let mp = d.mount_point().to_string_lossy();
            mp == "/" || mp == "C:\\"
        });
        if let Some(disk) = root_disk {
            let total_gb = disk.total_space() as f64 / (1024.0 * 1024.0 * 1024.0);
            let avail_gb = disk.available_space() as f64 / (1024.0 * 1024.0 * 1024.0);
            let used_gb = total_gb - avail_gb;
            self.disk_used_gb.push(ts, used_gb);
            self.disk_total_gb = total_gb;
        }
    }

    // -- App metrics --------------------------------------------------------

    pub fn collect_app_metrics(
        &mut self,
        _ws_subs: u64,
        ws_msgs: u64,
        ws_live: u64,
        constraints: usize,
        markets: usize,
        lat_p50_val: u64,
        lat_p95_val: u64,
        q_urg: usize,
        q_bg: usize,
    ) {
        let ts = now_ts();

        self.total_markets.push(ts, markets as f64);
        self.total_constraints.push(ts, constraints as f64);
        self.ws_connections.push(ts, ws_live as f64);
        self.lat_p50.push(ts, lat_p50_val as f64);
        self.lat_p95.push(ts, lat_p95_val as f64);
        self.queue_depth.push(ts, (q_urg + q_bg) as f64);

        // Compute msg rate from delta
        let dt = ts - self.prev_sample_ts;
        let rate = if dt > 0.0 && self.prev_sample_ts > 0.0 {
            let delta = ws_msgs.saturating_sub(self.prev_ws_msgs);
            delta as f64 / dt
        } else {
            0.0
        };
        self.ws_msg_rate.push(ts, rate);
        self.prev_ws_msgs = ws_msgs;
        self.prev_sample_ts = ts;
    }

    // -- Financial metrics --------------------------------------------------

    pub fn collect_financial_metrics(
        &mut self,
        total_value: f64,
        capital: f64,
        deployed: f64,
        realized_pnl: f64,
        unrealized_pnl: f64,
    ) {
        let ts = now_ts();

        self.total_value.push(ts, total_value);

        let deployed_pct = if total_value > 0.0 {
            (deployed / total_value) * 100.0
        } else {
            0.0
        };
        self.capital_deployed_pct.push(ts, deployed_pct);
        self.realized_pnl.push(ts, realized_pnl);
        self.unrealized_pnl.push(ts, unrealized_pnl);

        // Track peak and drawdown
        if total_value > self.peak_value {
            self.peak_value = total_value;
        }
        let dd = if self.peak_value > 0.0 {
            ((self.peak_value - total_value) / self.peak_value) * 100.0
        } else {
            0.0
        };
        self.drawdown_pct.push(ts, dd);
    }

    // -- Log buffer ---------------------------------------------------------

    pub fn push_log(&mut self, level: &str, target: &str, message: &str) {
        if self.log_buffer.len() >= MAX_LOG_ENTRIES {
            self.log_buffer.pop_front();
        }
        self.log_buffer.push_back(LogEntry {
            ts: now_ts(),
            level: level.to_string(),
            target: target.to_string(),
            message: message.to_string(),
        });
    }

    // -- JSON builders ------------------------------------------------------

    /// Build the JSON payload for the dashboard.
    ///
    /// * `full = true` — full snapshot with all time series (last 24 h),
    ///   financial summary, and full log buffer.
    /// * `full = false` — delta with only current values and new logs.
    pub fn build_json(&self, full: bool, log_ring: &mut LogRing) -> Value {
        let current = self.build_current();
        let logs_json = if full { log_ring.to_json_full() } else { log_ring.to_json_delta() };

        if full {
            let since = now_ts() - 86_400.0; // last 24 h
            let series = json!({
                "cpu_pct": self.cpu_pct.as_json_array(since),
                "mem_used_mb": self.mem_used_mb.as_json_array(since),
                "disk_used_gb": self.disk_used_gb.as_json_array(since),
                "total_markets": self.total_markets.as_json_array(since),
                "total_constraints": self.total_constraints.as_json_array(since),
                "ws_msg_rate": self.ws_msg_rate.as_json_array(since),
                "lat_p50": self.lat_p50.as_json_array(since),
                "lat_p95": self.lat_p95.as_json_array(since),
                "total_value": self.total_value.as_json_array(since),
                "deployed_pct": self.capital_deployed_pct.as_json_array(since),
                "drawdown_pct": self.drawdown_pct.as_json_array(since),
                "realized_pnl": self.realized_pnl.as_json_array(since),
                "unrealized_pnl": self.unrealized_pnl.as_json_array(since),
            });

            json!({
                "full": true,
                "current": current,
                "series": series,
                "financial": {},
                "logs": logs_json,
            })
        } else {
            let ts = now_ts();

            json!({
                "full": false,
                "current": current,
                "ts": ts,
                "logs_delta": logs_json,
            })
        }
    }

    fn build_current(&self) -> Value {
        json!({
            "cpu_pct": self.cpu_pct.latest().unwrap_or(0.0),
            "mem_used_mb": self.mem_used_mb.latest().unwrap_or(0.0),
            "mem_total_mb": self.mem_total_mb,
            "disk_used_gb": self.disk_used_gb.latest().unwrap_or(0.0),
            "disk_total_gb": self.disk_total_gb,
            "total_markets": self.total_markets.latest().unwrap_or(0.0),
            "total_constraints": self.total_constraints.latest().unwrap_or(0.0),
            "ws_msg_rate": self.ws_msg_rate.latest().unwrap_or(0.0),
            "lat_p50": self.lat_p50.latest().unwrap_or(0.0),
            "lat_p95": self.lat_p95.latest().unwrap_or(0.0),
            "queue_depth": self.queue_depth.latest().unwrap_or(0.0),
            "total_value": self.total_value.latest().unwrap_or(0.0),
            "deployed_pct": self.capital_deployed_pct.latest().unwrap_or(0.0),
            "drawdown_pct": self.drawdown_pct.latest().unwrap_or(0.0),
            "realized_pnl": self.realized_pnl.latest().unwrap_or(0.0),
            "unrealized_pnl": self.unrealized_pnl.latest().unwrap_or(0.0),
        })
    }

    // -- Financial summary --------------------------------------------------

    /// Compute financial summary statistics from closed positions.
    pub fn compute_financial_summary(
        &self,
        closed_positions: &[crate::position::Position],
    ) -> Value {
        if closed_positions.is_empty() {
            return json!({
                "sharpe": 0.0,
                "sortino": 0.0,
                "max_drawdown_pct": 0.0,
                "recovery_ratio": 0.0,
                "avg_hold_hours": 0.0,
                "win_rate": 0.0,
                "profit_factor": 0.0,
                "by_category": {},
                "by_duration": {},
            });
        }

        // -- Daily PnL for Sharpe / Sortino --------------------------------
        let daily_pnl = compute_daily_pnl(closed_positions);
        let sharpe = compute_sharpe(&daily_pnl);
        let sortino = compute_sortino(&daily_pnl);

        // -- Max drawdown from total_value time series ---------------------
        let max_dd_pct = self.compute_max_drawdown_pct();

        // -- Recovery ratio ------------------------------------------------
        let total_return_pct = if let (Some(first), Some(last)) = (
            self.total_value.buf.front(),
            self.total_value.buf.back(),
        ) {
            if first.1 > 0.0 {
                ((last.1 - first.1) / first.1) * 100.0
            } else {
                0.0
            }
        } else {
            0.0
        };
        let recovery_ratio = if max_dd_pct > 0.0 {
            total_return_pct / max_dd_pct
        } else {
            0.0
        };

        // -- Hold duration -------------------------------------------------
        let hold_secs: Vec<f64> = closed_positions
            .iter()
            .filter_map(|p| {
                let close_ts = p.close_timestamp?;
                let entry_ts = parse_entry_ts(&p.entry_timestamp)?;
                let dur = close_ts - entry_ts;
                if dur >= 0.0 { Some(dur) } else { None }
            })
            .collect();

        let avg_hold_hours = if hold_secs.is_empty() {
            0.0
        } else {
            hold_secs.iter().sum::<f64>() / hold_secs.len() as f64 / 3600.0
        };

        // -- Win rate ------------------------------------------------------
        let wins = closed_positions
            .iter()
            .filter(|p| p.actual_profit > 0.0)
            .count();
        let win_rate = wins as f64 / closed_positions.len() as f64;

        // -- Profit factor -------------------------------------------------
        let gross_profit: f64 = closed_positions.iter()
            .filter(|p| p.actual_profit > 0.0)
            .map(|p| p.actual_profit)
            .sum();
        let gross_loss: f64 = closed_positions.iter()
            .filter(|p| p.actual_profit < 0.0)
            .map(|p| p.actual_profit.abs())
            .sum();
        let profit_factor = if gross_loss > 0.0 { gross_profit / gross_loss } else { 0.0 };

        // -- By category ---------------------------------------------------
        let by_category = compute_by_category(closed_positions);

        // -- By duration ---------------------------------------------------
        let by_duration = compute_by_duration(closed_positions);

        json!({
            "sharpe": round3(sharpe),
            "sortino": round3(sortino),
            "max_drawdown_pct": round3(max_dd_pct),
            "recovery_ratio": round3(recovery_ratio),
            "avg_hold_hours": round2(avg_hold_hours),
            "win_rate": round3(win_rate),
            "profit_factor": round2(profit_factor),
            "by_category": by_category,
            "by_duration": by_duration,
        })
    }

    fn compute_max_drawdown_pct(&self) -> f64 {
        let mut peak = 0.0_f64;
        let mut max_dd = 0.0_f64;
        for &(_, v) in &self.total_value.buf {
            if v > peak {
                peak = v;
            }
            if peak > 0.0 {
                let dd = (peak - v) / peak * 100.0;
                if dd > max_dd {
                    max_dd = dd;
                }
            }
        }
        max_dd
    }
}

// ===========================================================================
// Helper functions
// ===========================================================================

fn now_ts() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Parse entry_timestamp — supports ISO 8601 or bare Unix float.
fn parse_entry_ts(entry: &str) -> Option<f64> {
    // Try parsing as float first (fast path)
    if let Ok(ts) = entry.parse::<f64>() {
        return Some(ts);
    }
    // ISO 8601 via chrono
    chrono::DateTime::parse_from_rfc3339(entry)
        .ok()
        .map(|dt| dt.timestamp() as f64 + dt.timestamp_subsec_millis() as f64 / 1000.0)
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(entry, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|ndt| ndt.and_utc().timestamp() as f64)
        })
}

/// Group closed positions by UTC day, return daily PnL sums.
fn compute_daily_pnl(positions: &[crate::position::Position]) -> Vec<f64> {
    use std::collections::BTreeMap;

    let mut by_day: BTreeMap<i64, f64> = BTreeMap::new();

    for p in positions {
        let day_key = if let Some(cts) = p.close_timestamp {
            (cts / 86_400.0).floor() as i64
        } else if let Some(ts) = parse_entry_ts(&p.entry_timestamp) {
            (ts / 86_400.0).floor() as i64
        } else {
            continue;
        };
        *by_day.entry(day_key).or_insert(0.0) += p.actual_profit;
    }

    by_day.into_values().collect()
}

/// Sharpe ratio: (mean_daily - rf_daily) / std_daily * sqrt(365)
fn compute_sharpe(daily_pnl: &[f64]) -> f64 {
    if daily_pnl.len() < 2 {
        return 0.0;
    }
    let rf_daily = 0.045 / 365.0;
    let n = daily_pnl.len() as f64;
    let mean = daily_pnl.iter().sum::<f64>() / n;
    let variance = daily_pnl.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
    let std = variance.sqrt();
    if std < 1e-12 {
        return 0.0;
    }
    (mean - rf_daily) / std * 365.0_f64.sqrt()
}

/// Sortino ratio: same but downside deviation only.
fn compute_sortino(daily_pnl: &[f64]) -> f64 {
    if daily_pnl.len() < 2 {
        return 0.0;
    }
    let rf_daily = 0.045 / 365.0;
    let n = daily_pnl.len() as f64;
    let mean = daily_pnl.iter().sum::<f64>() / n;
    let excess: Vec<f64> = daily_pnl.iter().map(|&x| x - rf_daily).collect();
    let downside_sq_sum: f64 = excess
        .iter()
        .filter(|&&x| x < 0.0)
        .map(|&x| x.powi(2))
        .sum();
    let downside_dev = (downside_sq_sum / n).sqrt();
    if downside_dev < 1e-12 {
        return 0.0;
    }
    (mean - rf_daily) / downside_dev * 365.0_f64.sqrt()
}

/// Profitability by category using `crate::types::classify_category`.
fn compute_by_category(positions: &[crate::position::Position]) -> Value {
    let mut buckets: HashMap<&'static str, Vec<&crate::position::Position>> = HashMap::new();

    for p in positions {
        let names: Vec<String> = p.markets.values().map(|leg| leg.name.clone()).collect();
        let cat = crate::types::classify_category(&names);
        buckets.entry(cat).or_default().push(p);
    }

    let mut out = serde_json::Map::new();
    for (cat, plist) in &buckets {
        out.insert((*cat).to_string(), bucket_stats(plist));
    }
    Value::Object(out)
}

/// Profitability by hold duration bucket.
fn compute_by_duration(positions: &[crate::position::Position]) -> Value {
    let buckets_def: &[(&str, f64, f64)] = &[
        ("<1h", 0.0, 3600.0),
        ("1-6h", 3600.0, 21_600.0),
        ("6-24h", 21_600.0, 86_400.0),
        ("1-3d", 86_400.0, 259_200.0),
        ("3-7d", 259_200.0, 604_800.0),
        ("7d+", 604_800.0, f64::MAX),
    ];

    let mut grouped: HashMap<&str, Vec<&crate::position::Position>> = HashMap::new();

    for p in positions {
        let hold = match (p.close_timestamp, parse_entry_ts(&p.entry_timestamp)) {
            (Some(close), Some(entry)) => close - entry,
            _ => continue,
        };

        for &(label, lo, hi) in buckets_def {
            if hold >= lo && hold < hi {
                grouped.entry(label).or_default().push(p);
                break;
            }
        }
    }

    let mut out = serde_json::Map::new();
    for &(label, _, _) in buckets_def {
        if let Some(plist) = grouped.get(label) {
            out.insert(label.to_string(), bucket_stats(plist));
        }
    }
    Value::Object(out)
}

/// Compute stats for a bucket of positions: count, win_rate, total_pnl, avg_pnl.
fn bucket_stats(positions: &[&crate::position::Position]) -> Value {
    let count = positions.len();
    let wins = positions.iter().filter(|p| p.actual_profit > 0.0).count();
    let total_pnl: f64 = positions.iter().map(|p| p.actual_profit).sum();
    let avg_pnl = if count > 0 { total_pnl / count as f64 } else { 0.0 };

    json!({
        "count": count,
        "win_rate": round3(if count > 0 { wins as f64 / count as f64 } else { 0.0 }),
        "total_pnl": round4(total_pnl),
        "avg_pnl": round4(avg_pnl),
    })
}

fn round2(v: f64) -> f64 { (v * 100.0).round() / 100.0 }
fn round3(v: f64) -> f64 { (v * 1000.0).round() / 1000.0 }
fn round4(v: f64) -> f64 { (v * 10000.0).round() / 10000.0 }

// ---------------------------------------------------------------------------
// MonitorLayer — tracing Layer that captures logs into a separate ring buffer
// ---------------------------------------------------------------------------

use std::sync::Arc;
use parking_lot::Mutex;

/// Separate log ring buffer — never shares lock with MonitorState to avoid
/// deadlock / contention when tracing events fire while monitor is locked.
pub struct LogRing {
    buf: VecDeque<LogEntry>,
    /// Number of entries sent last time (for delta calculation)
    sent_cursor: usize,
}

impl LogRing {
    pub fn new() -> Self {
        Self { buf: VecDeque::new(), sent_cursor: 0 }
    }

    pub fn push(&mut self, level: &str, target: &str, message: &str) {
        if self.buf.len() >= MAX_LOG_ENTRIES {
            self.buf.pop_front();
            // Adjust cursor if we dropped an entry that was already sent
            if self.sent_cursor > 0 { self.sent_cursor -= 1; }
        }
        self.buf.push_back(LogEntry {
            ts: now_ts(),
            level: level.to_string(),
            target: target.to_string(),
            message: message.to_string(),
        });
    }

    /// Return all log entries as JSON (for full snapshot).
    pub fn to_json_full(&mut self) -> Vec<Value> {
        self.sent_cursor = self.buf.len();
        self.buf.iter().map(|e| e.to_json()).collect()
    }

    /// Return only new entries since last call (for delta).
    pub fn to_json_delta(&mut self) -> Vec<Value> {
        let new_entries: Vec<Value> = self.buf.iter()
            .skip(self.sent_cursor)
            .map(|e| e.to_json())
            .collect();
        self.sent_cursor = self.buf.len();
        new_entries
    }

    pub fn len(&self) -> usize { self.buf.len() }
}

pub struct MonitorLayer {
    pub log_ring: Arc<Mutex<LogRing>>,
}

impl<S> tracing_subscriber::Layer<S> for MonitorLayer
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let meta = event.metadata();
        let level = meta.level().as_str();
        let target = meta.target();

        let mut visitor = MessageVisitor(String::new());
        event.record(&mut visitor);
        let message = visitor.0;

        self.log_ring.lock().push(level, target, &message);
    }
}

struct MessageVisitor(String);

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{:?}", value);
        } else if self.0.is_empty() {
            self.0 = format!("{}: {:?}", field.name(), value);
        } else {
            self.0.push_str(&format!(" {}: {:?}", field.name(), value));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0 = value.to_string();
        } else if self.0.is_empty() {
            self.0 = format!("{}: {}", field.name(), value);
        } else {
            self.0.push_str(&format!(" {}: {}", field.name(), value));
        }
    }
}
