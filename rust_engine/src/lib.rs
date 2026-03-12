/// PyO3 module: exposes RustWsEngine to Python.
///
/// Usage from Python:
///   import rust_engine
///   engine = rust_engine.RustWsEngine(config_dict)
///   engine.set_asset_index({"asset_123": ["constraint_A", "constraint_B"], ...})
///   engine.start(["asset_123", "asset_456", ...])
///   # ... in eval loop:
///   evals = engine.drain_evals(500)  # [(constraint_id, urgent), ...]
///   efp = engine.get_efp("asset_123")
///   best_ask = engine.get_best_ask("asset_123")
///   # ... shutdown:
///   engine.stop()
mod types;
mod book;
mod queue;
mod state;
mod ws;

use pyo3::prelude::*;
use pyo3::types::PyDict;
use std::collections::HashMap;
use std::sync::Arc;

use book::BookMirror;
use queue::EvalQueue;
use types::EngineConfig;
use ws::WsManager;

/// The tokio runtime lives here — one per engine instance.
/// Python calls methods on this; all async work happens on the Rust runtime.
#[pyclass]
struct RustWsEngine {
    book: Arc<BookMirror>,
    eval_queue: Arc<EvalQueue>,
    ws: Arc<WsManager>,
    runtime: tokio::runtime::Runtime,
}

#[pymethods]
impl RustWsEngine {
    /// Create a new engine with config from Python dict.
    ///
    /// Config keys (all optional, sensible defaults):
    ///   ws_url: str (default: Polymarket market WS)
    ///   assets_per_shard: int (default: 2000)
    ///   heartbeat_interval_secs: int (default: 10)
    ///   efp_drift_threshold: float (default: 0.005)
    ///   efp_stale_secs: float (default: 5.0)
    ///   trade_size_usd: float (default: 10.0)
    #[new]
    fn new(config: &Bound<'_, PyDict>) -> PyResult<Self> {
        // Install rustls crypto provider (required by rustls 0.23+)
        let _ = rustls::crypto::ring::default_provider().install_default();

        // Initialize tracing (logs to stderr, picked up by Python logging)
        let _ = tracing_subscriber::fmt()
            .with_target(false)
            .with_env_filter("rust_engine=debug")
            .try_init();

        let cfg = EngineConfig {
            ws_url: get_str(config, "ws_url")
                .unwrap_or_else(|| EngineConfig::default().ws_url),
            assets_per_shard: get_int(config, "assets_per_shard")
                .unwrap_or(2000) as usize,
            heartbeat_interval_secs: get_int(config, "heartbeat_interval_secs")
                .unwrap_or(10) as u64,
            efp_drift_threshold: get_float(config, "efp_drift_threshold")
                .unwrap_or(0.005),
            efp_stale_secs: get_float(config, "efp_stale_secs")
                .unwrap_or(5.0),
            trade_size_usd: get_float(config, "trade_size_usd")
                .unwrap_or(10.0),
        };

        let book = Arc::new(BookMirror::new(&cfg));
        let eval_queue = Arc::new(EvalQueue::new());
        let ws = Arc::new(WsManager::new(cfg, Arc::clone(&book), Arc::clone(&eval_queue)));

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)   // WS shards + heartbeat
            .enable_all()
            .thread_name("rust-ws")
            .build()
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(
                format!("Failed to create tokio runtime: {}", e)
            ))?;

        Ok(Self { book, eval_queue, ws, runtime })
    }

    /// Set the asset_id → constraint_id index.
    /// Called after constraint detection, before start().
    /// index: dict of {asset_id: [constraint_id, ...]}
    fn set_asset_index(&self, index: &Bound<'_, PyDict>) -> PyResult<()> {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for (key, val) in index.iter() {
            let asset_id: String = key.extract()?;
            let cids: Vec<String> = val.extract()?;
            map.insert(asset_id, cids);
        }
        self.book.set_asset_index(map);
        Ok(())
    }

    /// Start WS connections for the given asset IDs.
    /// Non-blocking: spawns tokio tasks and returns immediately.
    fn start(&self, asset_ids: Vec<String>) -> PyResult<()> {
        let ws = Arc::clone(&self.ws);
        self.runtime.spawn(async move {
            ws.start(asset_ids).await;
        });
        Ok(())
    }

    /// Stop all WS connections.
    fn stop(&self) {
        self.ws.stop();
    }

    /// Drain eval queue: returns list of (constraint_id, urgent, queued_at) tuples.
    /// Called by Python's eval loop (replaces _process_dirty_assets).
    fn drain_evals(&self, max: usize) -> Vec<(String, bool, f64)> {
        self.eval_queue.drain(max)
            .into_iter()
            .map(|e| (e.constraint_id, e.urgent, e.queued_at))
            .collect()
    }

    /// Get EFP (effective fill price) for an asset.
    fn get_efp(&self, asset_id: &str) -> f64 {
        self.book.get_efp(asset_id)
    }

    /// Get best ask price for an asset.
    fn get_best_ask(&self, asset_id: &str) -> f64 {
        self.book.get_best_ask(asset_id)
    }

    /// Get best bid price for an asset.
    fn get_best_bid(&self, asset_id: &str) -> f64 {
        self.book.get_best_bid(asset_id)
    }

    /// Get ask book as (prices, sizes) for an asset.
    fn get_asks(&self, asset_id: &str) -> (Vec<f64>, Vec<f64>) {
        self.book.get_asks_vec(asset_id)
    }

    /// Get queue depths: (urgent, background).
    fn queue_depths(&self) -> (usize, usize) {
        self.eval_queue.depths()
    }

    /// Get stale assets (book older than given seconds).
    fn get_stale_assets(&self, max_age_secs: f64) -> Vec<String> {
        self.book.get_stale_assets(max_age_secs)
    }

    /// Drain resolved market events: list of (market_cid, asset_id) tuples.
    fn drain_resolved(&self) -> Vec<(String, String)> {
        self.ws.drain_resolved()
            .into_iter()
            .map(|e| (e.market_cid, e.asset_id))
            .collect()
    }

    /// Engine stats as a dict.
    fn stats(&self, py: Python<'_>) -> PyResult<PyObject> {
        let ws_stats = self.ws.stats();
        let (q_urg, q_bg) = self.eval_queue.depths();
        let dict = PyDict::new(py);
        dict.set_item("ws_msgs", ws_stats.total_msgs)?;
        dict.set_item("ws_subscribed", ws_stats.subscribed)?;
        dict.set_item("ws_live", ws_stats.live_books)?;
        dict.set_item("ws_running", ws_stats.running)?;
        dict.set_item("queue_urgent", q_urg)?;
        dict.set_item("queue_background", q_bg)?;
        dict.set_item("book_count", self.book.live_count())?;
        Ok(dict.into())
    }
}

// === RustStateDB: SQLite state persistence (Phase 8 P4b) ===

#[pyclass]
struct RustStateDB {
    inner: state::StateDB,
}

#[pymethods]
impl RustStateDB {
    #[new]
    fn new(disk_path: &str) -> PyResult<Self> {
        let inner = state::StateDB::new(disk_path)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))?;
        Ok(Self { inner })
    }

    fn set_scalar(&self, key: &str, value: &str) {
        self.inner.set_scalar(key, value);
    }

    fn get_scalar(&self, key: &str) -> Option<String> {
        self.inner.get_scalar(key)
    }

    fn set_scalars(&self, pairs: Vec<(String, String)>) {
        self.inner.set_scalars(&pairs);
    }

    fn save_position(&self, position_id: &str, status: &str, data_json: &str,
                     opened_at: Option<&str>, closed_at: Option<&str>) {
        self.inner.save_position(position_id, status, data_json, opened_at, closed_at);
    }

    fn save_positions_bulk(&self, rows: Vec<(String, String, String, Option<String>, Option<String>)>) {
        self.inner.save_positions_bulk(&rows);
    }

    fn delete_position(&self, position_id: &str) {
        self.inner.delete_position(position_id);
    }

    fn load_open(&self) -> Vec<String> {
        self.inner.load_open()
    }

    fn load_closed(&self) -> Vec<String> {
        self.inner.load_closed()
    }

    fn count_by_status(&self) -> Vec<(String, i64)> {
        self.inner.count_by_status()
    }

    fn get_open_position_ids(&self) -> Vec<String> {
        self.inner.get_open_position_ids()
    }

    /// Mirror in-memory DB to disk. Returns elapsed ms.
    /// This is the key win: runs WITHOUT the GIL.
    fn mirror_to_disk(&self, py: Python<'_>) -> f64 {
        py.allow_threads(|| self.inner.mirror_to_disk())
    }

    /// Load disk DB into memory. Returns elapsed ms.
    fn load_from_disk(&self) -> PyResult<f64> {
        self.inner.load_from_disk()
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))
    }

    fn dirty_count(&self) -> usize {
        self.inner.dirty_count()
    }
}

// --- Helper functions for extracting config from PyDict ---
fn get_str(d: &Bound<'_, PyDict>, key: &str) -> Option<String> {
    d.get_item(key).ok().flatten().and_then(|v| v.extract::<String>().ok())
}
fn get_int(d: &Bound<'_, PyDict>, key: &str) -> Option<i64> {
    d.get_item(key).ok().flatten().and_then(|v| v.extract::<i64>().ok())
}
fn get_float(d: &Bound<'_, PyDict>, key: &str) -> Option<f64> {
    d.get_item(key).ok().flatten().and_then(|v| v.extract::<f64>().ok())
}

/// Python module registration.
#[pymodule]
fn rust_engine(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<RustWsEngine>()?;
    m.add_class::<RustStateDB>()?;
    Ok(())
}
