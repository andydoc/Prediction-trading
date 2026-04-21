/// Fill quality log writer (F-pre-4 / G4 infra).
///
/// **Purpose**: capture intent-vs-fill data for every LIVE entry/exit so we
/// can post-hoc validate that real fills hit at the prices the engine
/// expected. The validation step (95%+ fills above `min_profit_ratio`) is
/// run separately and AFTER real fills arrive — F-pre-4 explicitly defers
/// that to post-live. This module just installs the infrastructure.
///
/// **Format**: JSON Lines (newline-delimited JSON), one record per write,
/// flushed immediately. File: `data/fill_quality.log` (rotated externally).
///
/// **Two record types**:
///   - `intent`: written at entry time, captures what the engine wanted
///   - `actual`: written when the fill is confirmed (G1 fill_tracker)
///
/// Both share an `opp_id` (unique per attempted entry) so the post-hoc
/// validator can join them.
///
/// **Thread safety**: a parking_lot::Mutex guards the file handle; writes
/// are serialised. The lock is held only for the duration of one write
/// (microseconds in normal operation).
///
/// Created 2026-04-21 for v0.20.3 (F-pre-4 / G4 infra).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use parking_lot::Mutex;
use serde::Serialize;

/// One record describing what the engine intended to enter (or exit).
#[derive(Debug, Clone, Serialize)]
pub struct IntentRecord {
    /// "intent"
    pub kind: &'static str,
    /// Unix seconds (f64).
    pub ts: f64,
    /// Unique id for this entry attempt (use position_id when available).
    pub opp_id: String,
    /// Constraint id from the opportunity.
    pub constraint_id: String,
    /// "arb_buy" | "arb_sell".
    pub strategy: String,
    /// Method tag (e.g. "mutex_buy_all", "mutex_sell_all", "polytope").
    pub method: String,
    /// Per-market intended bet sizes (usd).
    pub intended_bets_usd: serde_json::Value,
    /// Per-market intended fill prices (engine-side EFP at decision time).
    pub intended_prices: serde_json::Value,
    /// Per-market NO-side prices recorded at decision time.
    pub intended_no_prices: serde_json::Value,
    /// Expected profit (usd) and pct, scaled to capital deployed.
    pub expected_profit_usd: f64,
    pub expected_profit_pct: f64,
    /// True for sell-all arbs.
    pub is_sell: bool,
}

/// One record describing what actually filled.
#[derive(Debug, Clone, Serialize)]
pub struct ActualRecord {
    /// "actual"
    pub kind: &'static str,
    pub ts: f64,
    pub opp_id: String,
    /// Per-market actual fill prices (averaged across partial fills).
    pub actual_prices: serde_json::Value,
    /// Per-market filled quantity (shares).
    pub actual_filled_shares: serde_json::Value,
    /// Per-market filled USDC notional.
    pub actual_filled_usd: serde_json::Value,
    /// "complete" | "partial_accepted" | "partial_unwound" | "failed".
    pub outcome: String,
}

/// Append-only writer. Cheap to clone (Arc<Mutex<File>>).
#[derive(Clone)]
pub struct FillQualityLog {
    inner: Arc<Mutex<Option<File>>>,
    path: PathBuf,
}

impl FillQualityLog {
    /// Open (or create) the log file at the given path. The parent dir must
    /// exist. Errors are logged but do not propagate — the engine continues
    /// without fill quality logging if the file cannot be opened.
    pub fn open(path: PathBuf) -> Self {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| {
                tracing::warn!(
                    "[fill_quality] failed to open {}: {} — logging disabled",
                    path.display(), e
                );
                e
            })
            .ok();
        if file.is_some() {
            tracing::info!("[fill_quality] log opened at {}", path.display());
        }
        Self { inner: Arc::new(Mutex::new(file)), path }
    }

    /// Write an intent record. Best-effort; errors are logged at debug level
    /// and do not propagate.
    pub fn record_intent(&self, rec: &IntentRecord) {
        self.write_one(rec);
    }

    /// Write an actual-fill record. Best-effort; errors are logged at debug
    /// level and do not propagate.
    pub fn record_actual(&self, rec: &ActualRecord) {
        self.write_one(rec);
    }

    fn write_one<T: Serialize>(&self, rec: &T) {
        let mut g = self.inner.lock();
        if let Some(f) = g.as_mut() {
            match serde_json::to_string(rec) {
                Ok(line) => {
                    if let Err(e) = writeln!(f, "{}", line) {
                        tracing::debug!("[fill_quality] write failed: {}", e);
                    }
                }
                Err(e) => tracing::debug!("[fill_quality] serialize failed: {}", e),
            }
        }
    }

    pub fn path(&self) -> &PathBuf { &self.path }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn writes_intent_and_actual() {
        let dir = std::env::temp_dir().join("pt_fillq_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("fillq_{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let log = FillQualityLog::open(path.clone());
        let intent = IntentRecord {
            kind: "intent",
            ts: 1700000000.0,
            opp_id: "pos-1".into(),
            constraint_id: "mutex_abc".into(),
            strategy: "arb_buy".into(),
            method: "mutex_buy_all".into(),
            intended_bets_usd: serde_json::json!({"m1": 10.0, "m2": 20.0}),
            intended_prices: serde_json::json!({"m1": 0.4, "m2": 0.55}),
            intended_no_prices: serde_json::json!({"m1": 0.6, "m2": 0.45}),
            expected_profit_usd: 1.5,
            expected_profit_pct: 0.05,
            is_sell: false,
        };
        log.record_intent(&intent);

        let actual = ActualRecord {
            kind: "actual",
            ts: 1700000005.0,
            opp_id: "pos-1".into(),
            actual_prices: serde_json::json!({"m1": 0.41, "m2": 0.555}),
            actual_filled_shares: serde_json::json!({"m1": 24.39, "m2": 36.04}),
            actual_filled_usd: serde_json::json!({"m1": 10.0, "m2": 20.0}),
            outcome: "complete".into(),
        };
        log.record_actual(&actual);

        // Drop to release the file handle on Windows.
        drop(log);

        let mut buf = String::new();
        File::open(&path).unwrap().read_to_string(&mut buf).unwrap();
        let lines: Vec<&str> = buf.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"kind\":\"intent\""));
        assert!(lines[1].contains("\"kind\":\"actual\""));
        assert!(lines[0].contains("\"opp_id\":\"pos-1\""));
        assert!(lines[1].contains("\"outcome\":\"complete\""));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_with_bad_dir_does_not_panic() {
        let path = PathBuf::from("/nonexistent/path/no_dir/fillq.log");
        let log = FillQualityLog::open(path);
        // Writes are best-effort; should silently no-op.
        log.record_intent(&IntentRecord {
            kind: "intent",
            ts: 0.0,
            opp_id: "x".into(),
            constraint_id: "x".into(),
            strategy: "x".into(),
            method: "x".into(),
            intended_bets_usd: serde_json::json!({}),
            intended_prices: serde_json::json!({}),
            intended_no_prices: serde_json::json!({}),
            expected_profit_usd: 0.0,
            expected_profit_pct: 0.0,
            is_sell: false,
        });
    }
}
