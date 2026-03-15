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
mod arb;
mod eval;
mod position;
mod ws;
mod dashboard;
mod resolution;
mod postponement;
mod scanner;
mod detect;

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use std::collections::HashMap;
use std::sync::Arc;

use book::BookMirror;
use queue::EvalQueue;
use eval::{ConstraintStore, EvalConfig, Constraint, MarketRef};
use types::EngineConfig;
use ws::WsManager;
use position::PositionManager;
use dashboard::{DashboardState, EngineMetrics};

/// The tokio runtime lives here — one per engine instance.
/// Python calls methods on this; all async work happens on the Rust runtime.
/// Owns ALL hot-path state: WS, books, evals, positions, dashboard.
#[pyclass]
struct RustWsEngine {
    book: Arc<BookMirror>,
    eval_queue: Arc<EvalQueue>,
    ws: Arc<WsManager>,
    constraints: Arc<ConstraintStore>,
    eval_config: EvalConfig,
    runtime: tokio::runtime::Runtime,
    // Position management (shared with dashboard via Arc<Mutex>)
    positions: Arc<parking_lot::Mutex<PositionManager>>,
    // Dashboard state
    engine_metrics: Arc<parking_lot::Mutex<EngineMetrics>>,
    recent_opps: Arc<parking_lot::Mutex<Vec<serde_json::Value>>>,
    mode: String,
    start_time: chrono::DateTime<chrono::Utc>,
    /// P95 delay table: category → p95_hours
    delay_table: Arc<parking_lot::Mutex<(std::collections::HashMap<String, f64>, f64)>>,
}

#[pymethods]
impl RustWsEngine {
    /// Create a new engine. Reads all config from config.yaml at the given workspace path.
    ///
    /// Args:
    ///   workspace: str — path to the project root (containing config/config.yaml)
    #[new]
    fn new(workspace: &str) -> PyResult<Self> {
        // Install rustls crypto provider (required by rustls 0.23+)
        let _ = rustls::crypto::ring::default_provider().install_default();

        // Initialize tracing (logs to stderr, picked up by Python logging)
        let _ = tracing_subscriber::fmt()
            .with_target(false)
            .with_env_filter("rust_engine=debug")
            .try_init();

        // Load all config from config.yaml
        let (cfg, eval_cfg, pos_cfg) = types::load_engine_config(workspace);

        let book = Arc::new(BookMirror::new(&cfg));
        let eval_queue = Arc::new(EvalQueue::new());
        let positions = Arc::new(parking_lot::Mutex::new(
            PositionManager::new(pos_cfg.initial_capital, pos_cfg.taker_fee)
        ));
        let ws = Arc::new(WsManager::new(
            cfg.clone(), Arc::clone(&book), Arc::clone(&eval_queue),
            Arc::clone(&positions),
        ));
        let constraints = Arc::new(ConstraintStore::new());

        let eval_config = EvalConfig {
            capital: cfg.trade_size_usd,
            fee_rate: eval_cfg.fee_rate,
            min_profit_threshold: eval_cfg.min_profit_threshold,
            max_profit_threshold: eval_cfg.max_profit_threshold,
            max_fw_iter: eval_cfg.max_fw_iter,
            max_hours: eval_cfg.max_hours,
        };

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("rust-ws")
            .build()
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(
                format!("Failed to create tokio runtime: {}", e)
            ))?;

        Ok(Self {
            book, eval_queue, ws, constraints, eval_config, runtime,
            positions,
            engine_metrics: Arc::new(parking_lot::Mutex::new(EngineMetrics::default())),
            recent_opps: Arc::new(parking_lot::Mutex::new(Vec::new())),
            mode: "shadow".to_string(),
            start_time: chrono::Utc::now(),
            delay_table: Arc::new(parking_lot::Mutex::new((std::collections::HashMap::new(), 33.5))),
        })
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

    /// Start WS connections for the given asset IDs AND the dashboard server.
    /// Non-blocking: spawns tokio tasks and returns immediately.
    #[pyo3(signature = (asset_ids, dashboard_port=5556))]
    fn start(&self, asset_ids: Vec<String>, dashboard_port: u16) -> PyResult<()> {
        let ws = Arc::clone(&self.ws);
        self.runtime.spawn(async move {
            ws.start(asset_ids).await;
        });

        // Start dashboard on same tokio runtime (port=0 means skip, used on re-subscribe)
        if dashboard_port > 0 {
            let dash_state = DashboardState {
                positions: Arc::clone(&self.positions),
                book: Arc::clone(&self.book),
                eval_queue: Arc::clone(&self.eval_queue),
                ws: Arc::clone(&self.ws),
                constraints: Arc::clone(&self.constraints),
                engine_metrics: Arc::clone(&self.engine_metrics),
                recent_opps: Arc::clone(&self.recent_opps),
                mode: self.mode.clone(),
                start_time: self.start_time,
                delay_table: Arc::clone(&self.delay_table),
            };
            self.runtime.spawn(async move {
                dashboard::start(dash_state, dashboard_port).await;
            });
        }

        Ok(())
    }

    /// Stop all WS connections.
    fn stop(&self) {
        self.ws.stop();
    }

    /// Load constraint definitions from Python.
    /// constraints: list of dicts with keys:
    ///   constraint_id, constraint_type, is_neg_risk, implications,
    ///   markets: list of {market_id, yes_asset_id, no_asset_id, name}
    fn set_constraints(&self, constraints: &Bound<'_, PyList>) -> PyResult<()> {
        let mut rust_constraints = Vec::new();
        for item in constraints.iter() {
            let d: &Bound<'_, PyDict> = item.downcast()?;
            let cid: String = d.get_item("constraint_id")?.unwrap().extract()?;
            let ctype: String = d.get_item("constraint_type")?.unwrap().extract()?;
            let neg_risk: bool = d.get_item("is_neg_risk")?
                .map(|v| v.extract::<bool>().unwrap_or(false)).unwrap_or(false);
            let implications: Vec<(usize, usize)> = d.get_item("implications")?
                .map(|v| v.extract().unwrap_or_default()).unwrap_or_default();

            let markets_list = d.get_item("markets")?.unwrap();
            let markets_list: &Bound<'_, PyList> = markets_list.downcast()?;
            let mut markets = Vec::new();
            for mitem in markets_list.iter() {
                let md: &Bound<'_, PyDict> = mitem.downcast()?;
                markets.push(MarketRef {
                    market_id: md.get_item("market_id")?.unwrap().extract()?,
                    yes_asset_id: md.get_item("yes_asset_id")?.unwrap().extract()?,
                    no_asset_id: md.get_item("no_asset_id")?.unwrap().extract()?,
                    name: md.get_item("name")?.map(|v| v.extract().unwrap_or_default()).unwrap_or_default(),
                });
            }
            rust_constraints.push(Constraint {
                constraint_id: cid, constraint_type: ctype,
                markets, is_neg_risk: neg_risk, implications,
                end_date_ts: d.get_item("end_date_ts")?
                    .map(|v| v.extract::<f64>().unwrap_or(0.0)).unwrap_or(0.0),
            });
        }
        self.constraints.set_constraints(rust_constraints);
        tracing::info!("Loaded {} constraints into Rust evaluator", self.constraints.len());
        Ok(())
    }

    /// A5: Detect constraints from market data, build indices, load into ConstraintStore.
    /// Replaces Python detect_constraints() + build_index() + _load_constraints_into_rust().
    ///
    /// markets: list of dicts with keys:
    ///   market_id, question, yes_asset_id, no_asset_id,
    ///   neg_risk (bool), neg_risk_market_id (str), yes_price (float), end_date_ts (float)
    /// config: dict with min_price_sum, max_price_sum, min_markets
    ///
    /// Returns dict: {n_constraints, all_asset_ids, asset_to_constraints, asset_to_market, stats}
    fn detect_and_load_constraints(
        &self,
        py: Python<'_>,
        markets: &Bound<'_, PyList>,
        config: &Bound<'_, PyDict>,
    ) -> PyResult<PyObject> {
        // 1. Parse market dicts
        let mut rust_markets = Vec::with_capacity(markets.len());
        for item in markets.iter() {
            let d: &Bound<'_, PyDict> = item.downcast()?;
            rust_markets.push(detect::DetectableMarket {
                market_id: d.get_item("market_id")?.unwrap().extract()?,
                question: d.get_item("question")?.unwrap().extract()?,
                yes_asset_id: d.get_item("yes_asset_id")?.unwrap().extract()?,
                no_asset_id: d.get_item("no_asset_id")?.unwrap().extract()?,
                neg_risk: d.get_item("neg_risk")?.unwrap().extract()?,
                neg_risk_market_id: d.get_item("neg_risk_market_id")?.unwrap().extract()?,
                yes_price: d.get_item("yes_price")?.unwrap().extract()?,
                end_date_ts: d.get_item("end_date_ts")?.unwrap().extract()?,
            });
        }

        // 2. Parse config
        let det_config = detect::DetectionConfig {
            min_price_sum: get_float(config, "min_price_sum").unwrap_or(0.85),
            max_price_sum: get_float(config, "max_price_sum").unwrap_or(1.15),
            min_markets: get_int(config, "min_markets").unwrap_or(2) as usize,
        };

        // 3. Run detection
        let result = detect::detect_constraints(&rust_markets, &det_config);

        let n_constraints = result.constraints.len();
        let n_assets = result.all_asset_ids.len();

        // 4. Load into ConstraintStore
        self.constraints.set_constraints(result.constraints);

        // 5. Set BookMirror asset→constraint index
        self.book.set_asset_index(result.asset_to_constraints.clone());

        // 6. Set PositionManager asset→market index (for WS resolution)
        //    Merge preserves entries for open position markets automatically.
        self.positions.lock().set_asset_index(result.asset_to_market.clone());

        // 6b. Ensure open position assets are in the WS subscription list.
        //     When markets close (dropped from scanner), their assets would be
        //     missing from all_asset_ids. Add them back so WS stays subscribed.
        let open_pos_assets = self.positions.lock().get_open_position_asset_ids();
        let mut all_asset_ids = result.all_asset_ids;
        if !open_pos_assets.is_empty() {
            let asset_set: std::collections::HashSet<String> =
                all_asset_ids.iter().cloned().collect();
            let mut extra = 0usize;
            for aid in &open_pos_assets {
                if !asset_set.contains(aid) {
                    all_asset_ids.push(aid.clone());
                    extra += 1;
                }
            }
            if extra > 0 {
                tracing::info!("Added {} assets from open positions to WS subscription", extra);
            }
        }

        tracing::info!(
            "Detected {} constraints from {} markets ({} assets, {} groups, {} incomplete, {} overpriced)",
            n_constraints, result.n_markets_input, all_asset_ids.len(),
            result.n_groups, result.n_skipped_incomplete, result.n_skipped_overpriced,
        );

        // 7. Build return dict for Python
        let ret = PyDict::new(py);
        ret.set_item("n_constraints", n_constraints)?;
        ret.set_item("all_asset_ids", &all_asset_ids)?;

        // asset_to_constraints: {str: [str]}
        let atc = PyDict::new(py);
        for (k, v) in &result.asset_to_constraints {
            atc.set_item(k, v)?;
        }
        ret.set_item("asset_to_constraints", atc)?;

        // asset_to_market: {str: (str, bool)}
        let atm = PyDict::new(py);
        for (k, (mid, is_yes)) in &result.asset_to_market {
            atm.set_item(k, (mid, *is_yes))?;
        }
        ret.set_item("asset_to_market", atm)?;

        // stats
        let stats = PyDict::new(py);
        stats.set_item("n_markets_input", result.n_markets_input)?;
        stats.set_item("n_groups", result.n_groups)?;
        stats.set_item("n_skipped_incomplete", result.n_skipped_incomplete)?;
        stats.set_item("n_skipped_overpriced", result.n_skipped_overpriced)?;
        ret.set_item("stats", stats)?;

        Ok(ret.into())
    }

    /// Load P95 delay table into engine (for dashboard display).
    /// delays: list of (category, p95_hours)
    fn set_delay_table(&self, delays: Vec<(String, f64)>) {
        let mut table = std::collections::HashMap::new();
        for (cat, p95) in &delays {
            table.insert(cat.clone(), *p95);
        }
        let default = table.get("other").copied().unwrap_or(33.5);
        *self.delay_table.lock() = (table, default);
    }

    /// Update eval config (called when capital changes).
    fn set_eval_config(&mut self, capital: f64, fee_rate: f64, min_profit: f64, max_profit: f64) {
        self.eval_config.capital = capital;
        self.eval_config.fee_rate = fee_rate;
        self.eval_config.min_profit_threshold = min_profit;
        self.eval_config.max_profit_threshold = max_profit;
        // Keep BookMirror's trade size in sync for EFP calculations
        self.book.set_trade_size(capital);
    }

    /// THE KEY FUNCTION: drain queue + read books + arb math → pre-ranked opportunities.
    /// held_cids/held_mids filter out already-held positions in Rust (zero-cost).
    /// Returns {opportunities, n_urgent, n_background, n_evaluated, n_skipped_held}.
    #[pyo3(signature = (max_evals, held_cids=None, held_mids=None, top_n=20))]
    fn evaluate_batch(&self, py: Python<'_>, max_evals: usize,
                      held_cids: Option<std::collections::HashSet<String>>,
                      held_mids: Option<std::collections::HashSet<String>>,
                      top_n: usize) -> PyResult<PyObject> {
        let empty_set = std::collections::HashSet::new();
        let hc = held_cids.as_ref().unwrap_or(&empty_set);
        let hm = held_mids.as_ref().unwrap_or(&empty_set);
        let (opps, n_urg, n_bg, n_eval, n_held) = eval::evaluate_batch(
            &self.eval_queue, &self.book, &self.constraints, &self.eval_config,
            max_evals, hc, hm, top_n,
        );

        let result = PyDict::new(py);
        result.set_item("n_urgent", n_urg)?;
        result.set_item("n_background", n_bg)?;
        result.set_item("n_evaluated", n_eval)?;
        result.set_item("n_skipped_held", n_held)?;

        let opp_list = PyList::empty(py);
        for opp in &opps {
            let d = PyDict::new(py);
            d.set_item("constraint_id", &opp.constraint_id)?;
            d.set_item("market_ids", &opp.market_ids)?;
            d.set_item("market_names", &opp.market_names)?;
            d.set_item("expected_profit_pct", opp.expected_profit_pct)?;
            d.set_item("expected_profit", opp.expected_profit)?;
            d.set_item("fees_estimated", opp.fees_estimated)?;
            d.set_item("total_capital_required", opp.total_capital_required)?;
            d.set_item("current_prices", &opp.current_prices)?;
            d.set_item("current_no_prices", &opp.current_no_prices)?;
            d.set_item("optimal_bets", &opp.optimal_bets)?;
            d.set_item("hours_to_resolve", opp.hours_to_resolve)?;
            d.set_item("score", opp.score)?;
            let meta = PyDict::new(py);
            meta.set_item("method", &opp.method)?;
            meta.set_item("neg_risk", opp.neg_risk)?;
            if let Some(ns) = opp.n_scenarios { meta.set_item("n_scenarios", ns)?; }
            d.set_item("metadata", meta)?;
            d.set_item("net_profit", opp.expected_profit)?;
            opp_list.append(d)?;
        }
        result.set_item("opportunities", opp_list)?;
        Ok(result.into())
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

    // =================================================================
    // Position management (merged from RustPositionManager)
    // =================================================================

    /// Initialize position manager with correct capital (called after state load).
    fn init_positions(&self, initial_capital: f64, taker_fee: f64) {
        let mut pm = self.positions.lock();
        *pm = PositionManager::new(initial_capital, taker_fee);
    }

    /// Update the trade size used in EFP and arb math calculations.
    /// Called by Python when capital changes (dynamic sizing).
    fn set_trade_size(&mut self, trade_size_usd: f64) {
        self.eval_config.capital = trade_size_usd;
        // BookMirror also needs this for EFP calculation
        self.book.set_trade_size(trade_size_usd);
    }

    fn current_capital(&self) -> f64 { self.positions.lock().current_capital() }
    fn total_value(&self) -> f64 { self.positions.lock().total_value() }
    fn initial_capital(&self) -> f64 { self.positions.lock().initial_capital() }
    fn pm_open_count(&self) -> usize { self.positions.lock().open_count() }
    fn pm_closed_count(&self) -> usize { self.positions.lock().closed_count() }

    fn enter_position(
        &self, py: Python<'_>,
        opportunity_id: &str, constraint_id: &str,
        strategy: &str, method: &str,
        market_ids: Vec<String>, market_names: Vec<String>,
        current_prices: HashMap<String, f64>,
        current_no_prices: HashMap<String, f64>,
        optimal_bets: HashMap<String, f64>,
        expected_profit: f64, expected_profit_pct: f64,
        is_sell: bool,
    ) -> PyResult<PyObject> {
        // Look up end_date_ts from constraint store before entering
        let end_date_ts = self.constraints.get(constraint_id)
            .map(|c| c.end_date_ts).unwrap_or(0.0);

        let result = self.positions.lock().enter_position(
            opportunity_id, constraint_id, strategy, method,
            &market_ids, &market_names, &current_prices, &current_no_prices,
            &optimal_bets, expected_profit, expected_profit_pct, is_sell,
            end_date_ts,
        );
        let d = PyDict::new(py);
        match result {
            position::EntryResult::Entered(pos) => {
                d.set_item("ok", true)?;
                d.set_item("position_id", &pos.position_id)?;
                d.set_item("total_capital", pos.total_capital)?;
                d.set_item("fees_paid", pos.fees_paid)?;
                d.set_item("data", serde_json::to_string(&pos).unwrap_or_default())?;
            }
            position::EntryResult::InsufficientCapital { available, required } => {
                d.set_item("ok", false)?;
                d.set_item("reason", "insufficient_capital")?;
                d.set_item("available", available)?;
                d.set_item("required", required)?;
            }
        }
        Ok(d.into())
    }

    fn close_on_resolution(&self, position_id: &str, winning_market_id: &str) -> Option<(f64, f64)> {
        self.positions.lock().close_on_resolution(position_id, winning_market_id)
            .map(|e| (e.payout, e.profit))
    }

    /// Check Polymarket API for positions whose markets already resolved.
    /// Catches missed WS events (system downtime, WS reconnect gaps).
    /// Only for shadow/paper mode — live mode relies on account movements.
    /// Requires BOTH closed=true AND prices at 1/0 to confirm resolution.
    /// Returns list of (position_id, winning_market_id, payout, profit).
    fn check_api_resolutions(&self, py: Python<'_>) -> PyResult<PyObject> {
        let positions_json = self.positions.lock().get_open_positions_json();
        if positions_json.is_empty() {
            return Ok(PyList::empty(py).into());
        }

        tracing::info!("Checking API resolution for {} open positions...", positions_json.len());

        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .user_agent("Mozilla/5.0 (prediction-trader)")
            .build()
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("HTTP client error: {}", e)))?;

        let results = PyList::empty(py);

        for pj in &positions_json {
            let p: serde_json::Value = match serde_json::from_str(pj) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let pid = p["position_id"].as_str().unwrap_or("");
            let markets = match p["markets"].as_object() {
                Some(m) => m,
                None => continue,
            };

            // Query each market's resolution status
            let mut resolved_outcomes: HashMap<String, String> = HashMap::new();
            let mut all_resolved = true;

            for mid in markets.keys() {
                let url = format!("https://gamma-api.polymarket.com/markets/{}", mid);
                match client.get(&url).send() {
                    Ok(resp) => {
                        if let Ok(mdata) = resp.json::<serde_json::Value>() {
                            // Must be closed=true (market actually closed, not just extreme prices)
                            let is_closed = mdata["closed"].as_bool().unwrap_or(false);
                            if !is_closed {
                                all_resolved = false;
                                continue;
                            }

                            let prices_raw = &mdata["outcomePrices"];
                            let prices: Vec<f64> = if let Some(s) = prices_raw.as_str() {
                                serde_json::from_str(s).unwrap_or_default()
                            } else if let Some(arr) = prices_raw.as_array() {
                                arr.iter().filter_map(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).collect()
                            } else {
                                Vec::new()
                            };

                            // Require definitive resolution: exactly one outcome at $1
                            if prices.len() >= 2 {
                                if prices[0] >= 0.99 && prices[1] <= 0.01 {
                                    resolved_outcomes.insert(mid.clone(), "yes".into());
                                } else if prices[1] >= 0.99 && prices[0] <= 0.01 {
                                    resolved_outcomes.insert(mid.clone(), "no".into());
                                } else {
                                    all_resolved = false;
                                }
                            } else {
                                all_resolved = false;
                            }
                        } else {
                            all_resolved = false;
                        }
                    }
                    Err(e) => {
                        tracing::debug!("API check failed for market {}: {}", mid, e);
                        all_resolved = false;
                    }
                }
            }

            if !all_resolved || resolved_outcomes.len() != markets.len() {
                continue;
            }

            // Find winning market (YES outcome)
            let winning_mid = match resolved_outcomes.iter().find(|(_, v)| v.as_str() == "yes") {
                Some((mid, _)) => mid.clone(),
                None => {
                    tracing::warn!("Position {}: all markets resolved but no YES winner", pid);
                    continue;
                }
            };

            // Resolve the position
            if let Some((payout, profit)) = self.positions.lock()
                .close_on_resolution(pid, &winning_mid)
                .map(|e| (e.payout, e.profit))
            {
                tracing::info!(
                    "API resolution: {} → winner={}, payout={:.2}, profit={:.4}",
                    pid, winning_mid, payout, profit
                );
                let d = PyDict::new(py);
                let _ = d.set_item("position_id", pid);
                let _ = d.set_item("winning_market_id", &winning_mid);
                let _ = d.set_item("payout", payout);
                let _ = d.set_item("profit", profit);
                let _ = results.append(d);
            }
        }

        let count = results.len();
        if count > 0 {
            tracing::info!("API resolution check: resolved {} positions", count);
        } else {
            tracing::info!("API resolution check: all positions still open");
        }

        Ok(results.into())
    }

    fn calculate_liquidation_value(&self, py: Python<'_>,
                                    position_id: &str,
                                    current_bids: HashMap<String, f64>) -> PyResult<PyObject> {
        match self.positions.lock().calculate_liquidation_value(position_id, &current_bids) {
            Some(liq) => {
                let d = PyDict::new(py);
                d.set_item("position_id", &liq.position_id)?;
                d.set_item("sale_proceeds", liq.sale_proceeds)?;
                d.set_item("fees", liq.fees)?;
                d.set_item("net_proceeds", liq.net_proceeds)?;
                d.set_item("profit", liq.profit)?;
                d.set_item("resolution_payout", liq.resolution_payout)?;
                Ok(d.into())
            }
            None => Ok(py.None())
        }
    }

    fn evaluate_replacement(&self, py: Python<'_>,
                             position_id: &str,
                             current_bids: HashMap<String, f64>,
                             replacement_profit: f64) -> PyResult<PyObject> {
        match self.positions.lock().evaluate_replacement(position_id, &current_bids, replacement_profit) {
            Some(eval) => {
                let d = PyDict::new(py);
                d.set_item("position_id", &eval.position_id)?;
                d.set_item("net_gain", eval.net_gain)?;
                d.set_item("worth_replacing", eval.worth_replacing)?;
                d.set_item("replacement_profit", eval.replacement_profit)?;
                d.set_item("liquidation_profit", eval.liquidation.profit)?;
                d.set_item("liquidation_net_proceeds", eval.liquidation.net_proceeds)?;
                d.set_item("resolution_payout", eval.liquidation.resolution_payout)?;
                Ok(d.into())
            }
            None => Ok(py.None())
        }
    }

    fn check_proactive_exits(&self, py: Python<'_>,
                              current_bids: HashMap<String, f64>,
                              exit_multiplier: f64) -> PyResult<PyObject> {
        let exits = self.positions.lock().check_proactive_exits(&current_bids, exit_multiplier);
        let list = PyList::empty(py);
        for exit in &exits {
            let d = PyDict::new(py);
            d.set_item("position_id", &exit.position_id)?;
            d.set_item("ratio", exit.ratio)?;
            d.set_item("net_proceeds", exit.liquidation.net_proceeds)?;
            d.set_item("resolution_payout", exit.liquidation.resolution_payout)?;
            d.set_item("profit", exit.liquidation.profit)?;
            list.append(d)?;
        }
        Ok(list.into())
    }

    fn liquidate_position(&self, position_id: &str, reason: &str,
                           current_bids: HashMap<String, f64>) -> Option<(f64, f64)> {
        self.positions.lock().liquidate_position(position_id, reason, &current_bids)
    }

    /// Set asset_id → (market_id, is_yes) index for WS resolution mapping.
    /// Called after build_index() in Python.
    fn set_resolution_index(&self, index: HashMap<String, (String, bool)>) {
        self.positions.lock().set_asset_index(index);
    }

    /// Resolve positions using raw WS events: [(condition_id, asset_id), ...]
    fn resolve_by_ws_events(&self, events: Vec<(String, String)>) -> Vec<(String, String)> {
        self.positions.lock().resolve_by_ws_events(&events)
    }

    fn get_held_constraint_ids(&self) -> std::collections::HashSet<String> {
        self.positions.lock().get_held_constraint_ids()
    }

    fn get_held_market_ids(&self) -> std::collections::HashSet<String> {
        self.positions.lock().get_held_market_ids()
    }

    /// Asset IDs from the index that map to markets in open positions.
    /// Used to ensure WS stays subscribed even after constraint rebuild.
    fn get_open_position_asset_ids(&self) -> Vec<String> {
        self.positions.lock().get_open_position_asset_ids()
    }

    fn get_open_positions_json(&self) -> Vec<String> {
        self.positions.lock().get_open_positions_json()
    }

    fn get_closed_positions_json(&self) -> Vec<String> {
        self.positions.lock().get_closed_positions_json()
    }

    fn get_open_position_ids(&self) -> Vec<String> {
        self.positions.lock().get_open_position_ids()
    }

    fn get_performance_metrics(&self) -> HashMap<String, f64> {
        self.positions.lock().get_performance_metrics()
    }

    fn import_positions(
        &self, open_json: Vec<String>, closed_json: Vec<String>,
        capital: f64, initial_capital: f64,
    ) {
        self.positions.lock().import_positions_json(&open_json, &closed_json, capital, initial_capital);
    }

    // =================================================================
    // Dashboard helpers (Python pushes metrics/opps to Rust for SSE)
    // =================================================================

    /// Update engine metrics (Python calls this each stats cycle).
    fn update_dashboard_metrics(&self, iteration: u64,
                                 lat_p50: u64, lat_p95: u64, lat_max: u64,
                                 scanner_status: &str, scanner_ts: &str,
                                 engine_status: &str, engine_ts: &str) {
        let mut m = self.engine_metrics.lock();
        m.iteration = iteration;
        m.lat_p50_us = lat_p50;
        m.lat_p95_us = lat_p95;
        m.lat_max_us = lat_max;
        m.scanner_status = scanner_status.to_string();
        m.scanner_ts = scanner_ts.to_string();
        m.engine_status = engine_status.to_string();
        m.engine_ts = engine_ts.to_string();
    }

    /// Push latest opportunities for dashboard display.
    fn set_recent_opps(&self, opps_json: Vec<String>) {
        let mut opps = self.recent_opps.lock();
        *opps = opps_json.iter()
            .filter_map(|s| serde_json::from_str(s).ok())
            .collect();
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

    fn set_scalar(&self, key: &str, value: f64) {
        self.inner.set_scalar(key, value);
    }

    fn get_scalar(&self, key: &str) -> Option<f64> {
        self.inner.get_scalar(key)
    }

    fn set_scalars(&self, pairs: Vec<(String, f64)>) {
        self.inner.set_scalars(&pairs);
    }

    fn get_all_scalars(&self) -> Vec<(String, f64)> {
        self.inner.get_all_scalars()
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

    fn load_by_status(&self, status: &str) -> Vec<String> {
        self.inner.load_by_status(status)
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

    /// Replace the delay P95 table.
    /// rows: list of (category, p95_hours, count, median, p75, pct_over_24h)
    fn set_delay_table(&self, rows: Vec<(String, f64, i64, f64, f64, f64)>, updated_at: &str) {
        self.inner.set_delay_table(&rows, updated_at);
    }

    /// Get delay P95 values as list of (category, p95_hours).
    fn get_delay_table(&self) -> Vec<(String, f64)> {
        self.inner.get_delay_table()
    }

    /// Get full delay table with all stats.
    fn get_delay_table_full(&self) -> Vec<(String, f64, i64, f64, f64, f64, String)> {
        self.inner.get_delay_table_full()
    }
}

// RustPositionManager removed — position management merged into RustWsEngine

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
    m.add_class::<resolution::RustResolutionValidator>()?;
    m.add_class::<postponement::RustPostponementDetector>()?;
    m.add_class::<scanner::RustMarketScanner>()?;
    Ok(())
}
