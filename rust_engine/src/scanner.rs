/// A4: Market Scanner — Rust port of utilities/initial_market_scanner.py (Phase 2)
///
/// Fetches all active markets from Polymarket Gamma API with pagination,
/// converts to MarketData.to_dict() format, and caches in SQLite.
///
/// Schema:
///   markets(market_id TEXT PK, data TEXT JSON, updated_at REAL)
use rusqlite::params;
use std::time::Duration;
use crate::cached_db::CachedSqliteDB;

// ---------------------------------------------------------------------------
// SQLite cache (in-memory + disk backup, same pattern as ResolutionCache)
// ---------------------------------------------------------------------------

const SCANNER_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS markets (
        market_id TEXT PRIMARY KEY,
        data TEXT NOT NULL,
        updated_at REAL NOT NULL
    );
";

struct ScannerDB {
    db: CachedSqliteDB,
}

impl ScannerDB {
    fn new(disk_path: &str) -> Result<Self, String> {
        let db = CachedSqliteDB::new(disk_path, SCANNER_SCHEMA)?;

        // Restore from disk if available
        if db.disk_exists() {
            match db.load_from_disk() {
                Ok(ms) => tracing::info!("[scanner] Loaded market cache from disk in {:.1}ms", ms),
                Err(e) => tracing::warn!("[scanner] Could not load market cache from disk: {}", e),
            }
        }

        Ok(Self { db })
    }

    fn save_markets(&self, markets: &[(String, String)]) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        let conn = self.db.conn();
        // Clear and reload — full replacement each scan
        let _ = conn.execute("DELETE FROM markets", []);
        let mut stmt = match conn.prepare(
            "INSERT OR REPLACE INTO markets (market_id, data, updated_at) VALUES (?1, ?2, ?3)"
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("[scanner] Failed to prepare insert: {}", e);
                return;
            }
        };
        for (mid, json) in markets {
            let _ = stmt.execute(params![mid, json, now]);
        }
    }

    fn load_all(&self) -> Vec<String> {
        let conn = self.db.conn();
        let mut result = Vec::new();
        if let Ok(mut stmt) = conn.prepare("SELECT data FROM markets") {
            if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) {
                for row in rows.flatten() {
                    result.push(row);
                }
            }
        }
        result
    }

    fn count(&self) -> usize {
        let conn = self.db.conn();
        conn.query_row("SELECT count(*) FROM markets", [], |row| row.get::<_, i64>(0))
            .unwrap_or(0) as usize
    }

    fn mirror_to_disk(&self) -> f64 {
        self.db.mirror_to_disk()
    }

    fn load_from_disk(&self) -> Result<f64, String> {
        self.db.load_from_disk()
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
                tracing::error!("[scanner] Gamma API request failed at offset {}: {}", offset, e);
                break;
            }
        };

        if !resp.status().is_success() {
            tracing::error!("[scanner] Gamma API returned {} at offset {}", resp.status(), offset);
            break;
        }

        let batch: Vec<serde_json::Value> = match resp.json() {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("[scanner] Failed to parse Gamma API response at offset {}: {}", offset, e);
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

// ---------------------------------------------------------------------------
// ScanResult
// ---------------------------------------------------------------------------

#[must_use]
pub struct ScanResult {
    pub markets: Vec<serde_json::Value>,
    pub count: usize,
    pub skipped: usize,
}

// ---------------------------------------------------------------------------
// MarketScanner
// ---------------------------------------------------------------------------

pub struct MarketScanner {
    db: ScannerDB,
    client: reqwest::blocking::Client,
}

impl MarketScanner {
    pub fn new(db_path: &str) -> Result<Self, String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

        let db = ScannerDB::new(db_path)?;

        Ok(Self { db, client })
    }

    /// Fetch all markets from Gamma API, convert, store in SQLite, return as ScanResult.
    pub fn scan(&self) -> Result<ScanResult, String> {
        let (markets, skipped) = fetch_all_markets(&self.client);

        if markets.is_empty() {
            return Err("No markets fetched from Gamma API".into());
        }

        // Store in SQLite (needs lock, quick operation)
        let db_pairs: Vec<(String, String)> = markets.iter()
            .map(|(mid, val)| (mid.clone(), serde_json::to_string(val).unwrap_or_default()))
            .collect();
        self.db.save_markets(&db_pairs);
        let mirror_ms = self.db.mirror_to_disk();
        tracing::info!("[scanner] {} markets stored, {} skipped, disk backup {:.1}ms",
                  markets.len(), skipped, mirror_ms);

        let market_values: Vec<serde_json::Value> = markets.into_iter()
            .map(|(_, val)| val)
            .collect();
        let count = market_values.len();

        Ok(ScanResult {
            markets: market_values,
            count,
            skipped,
        })
    }

    /// Load cached markets from SQLite (no API call). Returns same format as scan().
    pub fn load_cached(&self) -> ScanResult {
        // Try loading from disk if in-memory is empty
        if self.db.count() == 0 && self.db.db.disk_exists() {
            let _ = self.db.load_from_disk();
        }

        let json_strings = self.db.load_all();
        let markets: Vec<serde_json::Value> = json_strings.iter()
            .filter_map(|json_str| serde_json::from_str::<serde_json::Value>(json_str).ok())
            .collect();
        let count = markets.len();

        ScanResult {
            markets,
            count,
            skipped: 0,
        }
    }

    /// Market count in DB.
    pub fn count(&self) -> usize {
        self.db.count()
    }

    /// Mirror in-memory SQLite to disk. Returns elapsed ms.
    pub fn mirror_to_disk(&self) -> f64 {
        self.db.mirror_to_disk()
    }

    /// Load disk DB into memory (startup recovery). Returns elapsed ms.
    pub fn load_from_disk(&self) -> Result<f64, String> {
        self.db.load_from_disk()
    }
}
