/// A4: Market Scanner — Rust port of utilities/initial_market_scanner.py (Phase 2)
///
/// Fetches all active markets from Polymarket Gamma API with pagination,
/// converts to MarketData.to_dict() format, and caches in SQLite.
///
/// Schema:
///   markets(market_id TEXT PK, data TEXT JSON, updated_at REAL)
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use rusqlite::{Connection, params};
use rusqlite::backup::Backup;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// SQLite cache (in-memory + disk backup, same pattern as ResolutionCache)
// ---------------------------------------------------------------------------

struct ScannerDB {
    mem: Mutex<Connection>,
    disk_path: PathBuf,
}

impl ScannerDB {
    fn new(disk_path: &str) -> Result<Self, String> {
        let mem_conn = Connection::open_in_memory()
            .map_err(|e| format!("Failed to open in-memory SQLite: {}", e))?;

        mem_conn.execute_batch("
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=OFF;
            CREATE TABLE IF NOT EXISTS markets (
                market_id TEXT PRIMARY KEY,
                data TEXT NOT NULL,
                updated_at REAL NOT NULL
            );
        ").map_err(|e| format!("Failed to create markets table: {}", e))?;

        let db = Self {
            mem: Mutex::new(mem_conn),
            disk_path: PathBuf::from(disk_path),
        };

        // Restore from disk if available
        if db.disk_path.exists() {
            match db.load_from_disk() {
                Ok(ms) => eprintln!("[scanner] Loaded market cache from disk in {:.1}ms", ms),
                Err(e) => eprintln!("[scanner] Could not load market cache from disk: {}", e),
            }
        }

        Ok(db)
    }

    fn save_markets(&self, markets: &[(String, String)]) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        let db = self.mem.lock();
        // Clear and reload — full replacement each scan
        let _ = db.execute("DELETE FROM markets", []);
        let mut stmt = match db.prepare(
            "INSERT OR REPLACE INTO markets (market_id, data, updated_at) VALUES (?1, ?2, ?3)"
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[scanner] Failed to prepare insert: {}", e);
                return;
            }
        };
        for (mid, json) in markets {
            let _ = stmt.execute(params![mid, json, now]);
        }
    }

    fn load_all(&self) -> Vec<String> {
        let db = self.mem.lock();
        let mut result = Vec::new();
        if let Ok(mut stmt) = db.prepare("SELECT data FROM markets") {
            if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) {
                for row in rows.flatten() {
                    result.push(row);
                }
            }
        }
        result
    }

    fn count(&self) -> usize {
        let db = self.mem.lock();
        db.query_row("SELECT count(*) FROM markets", [], |row| row.get::<_, usize>(0))
            .unwrap_or(0)
    }

    fn mirror_to_disk(&self) -> f64 {
        let t0 = Instant::now();
        let db = self.mem.lock();

        if let Some(parent) = self.disk_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        match Connection::open(&self.disk_path) {
            Ok(mut disk_db) => {
                match Backup::new(&*db, &mut disk_db) {
                    Ok(backup) => {
                        if let Err(e) = backup.run_to_completion(
                            100, Duration::ZERO,
                            None::<fn(rusqlite::backup::Progress)>,
                        ) {
                            eprintln!("[scanner] Cache backup failed: {}", e);
                        }
                    }
                    Err(e) => eprintln!("[scanner] Cache backup init failed: {}", e),
                }
            }
            Err(e) => eprintln!("[scanner] Failed to open disk DB: {}", e),
        }

        t0.elapsed().as_secs_f64() * 1000.0
    }

    fn load_from_disk(&self) -> Result<f64, String> {
        if !self.disk_path.exists() {
            return Err("Disk DB file not found".into());
        }
        let t0 = Instant::now();
        let disk_db = Connection::open(&self.disk_path)
            .map_err(|e| format!("Failed to open disk DB: {}", e))?;

        let mut db = self.mem.lock();
        let backup = Backup::new(&disk_db, &mut *db)
            .map_err(|e| format!("Backup init failed: {}", e))?;
        backup.run_to_completion(
            100, Duration::ZERO,
            None::<fn(rusqlite::backup::Progress)>,
        ).map_err(|e| format!("Backup restore failed: {}", e))?;

        Ok(t0.elapsed().as_secs_f64() * 1000.0)
    }
}

// ---------------------------------------------------------------------------
// Market conversion: Gamma API → MarketData.to_dict() format
// ---------------------------------------------------------------------------

/// Parse a JSON-encoded string-or-array field from the Gamma API.
/// The API sometimes returns `"[\"Yes\",\"No\"]"` (JSON string) or `["Yes","No"]` (array).
fn parse_json_string_or_array(val: &serde_json::Value) -> Vec<serde_json::Value> {
    match val {
        serde_json::Value::Array(arr) => arr.clone(),
        serde_json::Value::String(s) => {
            serde_json::from_str(s).unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

/// Convert a raw Gamma API market dict to the MarketData.to_dict() format.
/// Returns (market_id, converted_json_value) or None if malformed.
fn convert_market(raw: &serde_json::Value) -> Option<(String, serde_json::Value)> {
    let market_id = raw.get("id").and_then(|v| v.as_str())?.to_string();

    // Parse outcomes and outcomePrices → outcome_prices: {outcome: price}
    let outcomes = raw.get("outcomes")
        .map(|v| parse_json_string_or_array(v))
        .unwrap_or_default();
    let prices = raw.get("outcomePrices")
        .map(|v| parse_json_string_or_array(v))
        .unwrap_or_default();

    let mut outcome_prices = serde_json::Map::new();
    for (i, outcome) in outcomes.iter().enumerate() {
        if let (Some(name), Some(price_val)) = (outcome.as_str(), prices.get(i)) {
            let price: f64 = match price_val {
                serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0),
                serde_json::Value::String(s) => s.parse().unwrap_or(0.0),
                _ => 0.0,
            };
            outcome_prices.insert(
                name.to_string(),
                serde_json::Value::from(price),
            );
        }
    }

    // Parse endDate
    let end_date_raw = raw.get("endDate")
        .or_else(|| raw.get("end_date"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let end_date_str = if end_date_raw.is_empty() {
        chrono::Utc::now().to_rfc3339()
    } else {
        // Handle Z suffix → +00:00 for Python fromisoformat compat
        end_date_raw.replace("Z", "+00:00")
    };

    // Parse clobTokenIds → yes_asset_id, no_asset_id
    let clob_ids = raw.get("clobTokenIds")
        .map(|v| parse_json_string_or_array(v))
        .unwrap_or_default();
    let yes_asset_id = clob_ids.first()
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let no_asset_id = clob_ids.get(1)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // volume_24h: try volume_24h then volume24hr, handle string or number
    let volume_24h = extract_f64(raw, "volume_24h")
        .or_else(|| extract_f64(raw, "volume24hr"))
        .unwrap_or(0.0);

    let liquidity = extract_f64(raw, "liquidity").unwrap_or(0.0);

    let question = raw.get("question").and_then(|v| v.as_str()).unwrap_or("Unknown");

    let now_iso = chrono::Utc::now().to_rfc3339();

    // Build metadata dict (matches Python MarketData.from_api_response)
    let metadata = serde_json::json!({
        "conditionId": raw.get("conditionId").and_then(|v| v.as_str()).unwrap_or(""),
        "questionID": raw.get("questionID").and_then(|v| v.as_str()).unwrap_or(""),
        "negRisk": raw.get("negRisk").and_then(|v| v.as_bool()).unwrap_or(false),
        "negRiskMarketID": raw.get("negRiskMarketID").and_then(|v| v.as_str()).unwrap_or(""),
        "groupItemTitle": raw.get("groupItemTitle").and_then(|v| v.as_str()).unwrap_or(""),
        "slug": raw.get("slug").and_then(|v| v.as_str()).unwrap_or(""),
        "clobTokenIds": raw.get("clobTokenIds").cloned().unwrap_or(serde_json::Value::String(String::new())),
        "enableOrderBook": raw.get("enableOrderBook").and_then(|v| v.as_bool()).unwrap_or(false),
        "acceptingOrders": raw.get("acceptingOrders").and_then(|v| v.as_bool()).unwrap_or(false),
        "end_date": end_date_raw,
        "description": raw.get("description").and_then(|v| v.as_str()).unwrap_or(""),
    });

    // Build final dict matching MarketData.to_dict() output
    let converted = serde_json::json!({
        "market_id": market_id,
        "market_name": question,
        "question": question,
        "outcome_prices": outcome_prices,
        "volume_24h": volume_24h,
        "liquidity": liquidity,
        "end_date": end_date_str,
        "categories": raw.get("tags").cloned().unwrap_or(serde_json::Value::Array(Vec::new())),
        "metadata": metadata,
        "timestamp": now_iso,
        "source": "polymarket",
        "yes_asset_id": yes_asset_id,
        "no_asset_id": no_asset_id,
    });

    Some((market_id, converted))
}

/// Extract a f64 from a JSON value that may be a number or a string.
fn extract_f64(obj: &serde_json::Value, key: &str) -> Option<f64> {
    match obj.get(key)? {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// HTTP: paginated Gamma API fetch
// ---------------------------------------------------------------------------

fn fetch_all_markets(
    client: &reqwest::blocking::Client,
) -> (Vec<(String, serde_json::Value)>, usize) {
    let api_url = "https://gamma-api.polymarket.com/markets";
    let limit = 500usize;
    let mut offset = 0usize;
    let mut all_markets = Vec::new();
    let mut skipped = 0usize;

    loop {
        let resp = match client
            .get(api_url)
            .query(&[
                ("limit", limit.to_string()),
                ("offset", offset.to_string()),
                ("active", "true".to_string()),
                ("closed", "false".to_string()),
            ])
            .timeout(Duration::from_secs(30))
            .send()
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[scanner] Gamma API request failed at offset {}: {}", offset, e);
                break;
            }
        };

        if !resp.status().is_success() {
            eprintln!("[scanner] Gamma API returned {} at offset {}", resp.status(), offset);
            break;
        }

        let batch: Vec<serde_json::Value> = match resp.json() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[scanner] Failed to parse Gamma API response at offset {}: {}", offset, e);
                break;
            }
        };

        if batch.is_empty() {
            break;
        }

        let batch_len = batch.len();
        for raw in &batch {
            match convert_market(raw) {
                Some(pair) => all_markets.push(pair),
                None => skipped += 1,
            }
        }

        if batch_len < limit {
            break; // Last page
        }
        offset += limit;
    }

    (all_markets, skipped)
}

/// Write backward-compat latest_markets.json
fn write_json_file(path: &std::path::Path, markets: &[(String, serde_json::Value)]) {
    let market_list: Vec<&serde_json::Value> = markets.iter().map(|(_, v)| v).collect();
    let output = serde_json::json!({
        "markets": market_list,
        "count": markets.len(),
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    match std::fs::write(path, serde_json::to_string(&output).unwrap_or_default()) {
        Ok(_) => {}
        Err(e) => eprintln!("[scanner] Failed to write JSON file: {}", e),
    }
}

// ---------------------------------------------------------------------------
// Helper: serde_json::Value → PyObject
// ---------------------------------------------------------------------------

fn json_value_to_py(py: Python<'_>, val: &serde_json::Value) -> PyObject {
    match val {
        serde_json::Value::Null => py.None(),
        serde_json::Value::Bool(b) => {
            if *b { true.into_pyobject(py).unwrap().to_owned().into_any().unbind() }
            else { false.into_pyobject(py).unwrap().to_owned().into_any().unbind() }
        }
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into_pyobject(py).unwrap().into_any().unbind()
            } else if let Some(f) = n.as_f64() {
                f.into_pyobject(py).unwrap().into_any().unbind()
            } else {
                py.None()
            }
        }
        serde_json::Value::String(s) => s.into_pyobject(py).unwrap().into_any().unbind(),
        serde_json::Value::Array(arr) => {
            let list = PyList::empty(py);
            for item in arr {
                let _ = list.append(json_value_to_py(py, item));
            }
            list.into_any().unbind()
        }
        serde_json::Value::Object(map) => {
            let dict = PyDict::new(py);
            for (k, v) in map {
                let _ = dict.set_item(k, json_value_to_py(py, v));
            }
            dict.into_any().unbind()
        }
    }
}

// ---------------------------------------------------------------------------
// PyO3 class
// ---------------------------------------------------------------------------

#[pyclass]
pub struct RustMarketScanner {
    db: ScannerDB,
    client: reqwest::blocking::Client,
    json_output_path: Option<PathBuf>,
}

#[pymethods]
impl RustMarketScanner {
    #[new]
    #[pyo3(signature = (db_path, json_path=None))]
    fn new(db_path: String, json_path: Option<String>) -> PyResult<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(
                format!("Failed to create HTTP client: {}", e)
            ))?;

        let db = ScannerDB::new(&db_path)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))?;

        Ok(Self {
            db,
            client,
            json_output_path: json_path.map(PathBuf::from),
        })
    }

    /// Fetch all markets from Gamma API, convert, store in SQLite, return as list of dicts.
    fn scan(&self, py: Python<'_>) -> PyResult<PyObject> {
        let client = self.client.clone();
        let json_path = self.json_output_path.clone();

        // Release GIL during HTTP + conversion
        let (markets, skipped) = py.allow_threads(move || {
            let (markets, skipped) = fetch_all_markets(&client);

            if !markets.is_empty() {
                // Write to JSON file (backward compat)
                if let Some(ref path) = json_path {
                    write_json_file(path, &markets);
                }
            }

            (markets, skipped)
        });

        if markets.is_empty() {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "No markets fetched from Gamma API"
            ));
        }

        // Store in SQLite (needs lock, quick operation)
        let db_pairs: Vec<(String, String)> = markets.iter()
            .map(|(mid, val)| (mid.clone(), serde_json::to_string(val).unwrap_or_default()))
            .collect();
        self.db.save_markets(&db_pairs);
        let mirror_ms = self.db.mirror_to_disk();
        eprintln!("[scanner] {} markets stored, {} skipped, disk backup {:.1}ms",
                  markets.len(), skipped, mirror_ms);

        // Convert to Python dicts
        let market_list = PyList::empty(py);
        for (_, val) in &markets {
            let py_dict = json_value_to_py(py, val);
            let _ = market_list.append(py_dict);
        }

        let result = PyDict::new(py);
        result.set_item("markets", market_list)?;
        result.set_item("count", markets.len())?;
        result.set_item("skipped", skipped)?;

        Ok(result.into_any().unbind())
    }

    /// Load cached markets from SQLite (no API call). Returns same format as scan().
    fn load_cached(&self, py: Python<'_>) -> PyResult<PyObject> {
        // Try loading from disk if in-memory is empty
        if self.db.count() == 0 && self.db.disk_path.exists() {
            let _ = self.db.load_from_disk();
        }

        let json_strings = self.db.load_all();
        let market_list = PyList::empty(py);
        for json_str in &json_strings {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                let py_dict = json_value_to_py(py, &val);
                let _ = market_list.append(py_dict);
            }
        }

        let result = PyDict::new(py);
        result.set_item("markets", market_list)?;
        result.set_item("count", json_strings.len())?;
        result.set_item("skipped", 0)?;

        Ok(result.into_any().unbind())
    }

    /// Market count in DB.
    fn count(&self) -> usize {
        self.db.count()
    }

    /// Mirror in-memory SQLite to disk. Returns elapsed ms.
    fn mirror_to_disk(&self) -> f64 {
        self.db.mirror_to_disk()
    }

    /// Load disk DB into memory (startup recovery). Returns elapsed ms.
    fn load_from_disk(&self) -> PyResult<f64> {
        self.db.load_from_disk()
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))
    }
}
