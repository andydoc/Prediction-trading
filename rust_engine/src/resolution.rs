/// A2: Resolution Validator — Rust port of utilities/resolution_validator.py
///
/// Extracts true resolution dates from Polymarket market rules via Anthropic API.
/// Cache backed by in-memory SQLite with periodic disk backup (same pattern as StateDB).
///
/// Schema:
///   resolution_cache(group_id TEXT PK, data TEXT JSON, cached_at REAL)
use pyo3::prelude::*;
use pyo3::types::PyDict;
use rusqlite::{Connection, params};
use rusqlite::backup::Backup;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ValidationResult {
    pub latest_resolution_date: String,
    pub confidence: String,
    pub has_unrepresented_outcome: bool,
    #[serde(default)]
    pub unrepresented_outcome_reason: String,
    #[serde(default)]
    pub reasoning: String,
}

#[derive(Debug, Clone)]
struct ValidatorConfig {
    api_url: String,
    api_version: String,
    model: String,
    max_tokens: u32,
    cache_ttl_secs: f64,
}

// ---------------------------------------------------------------------------
// In-memory SQLite cache (mirrors StateDB pattern)
// ---------------------------------------------------------------------------

struct ResolutionCache {
    mem: Mutex<Connection>,
    disk_path: PathBuf,
    ttl_secs: f64,
}

impl ResolutionCache {
    fn new(disk_path: &str, ttl_secs: f64) -> Result<Self, String> {
        let mem_conn = Connection::open_in_memory()
            .map_err(|e| format!("Failed to open in-memory SQLite: {}", e))?;

        mem_conn.execute_batch("
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=OFF;
            CREATE TABLE IF NOT EXISTS resolution_cache (
                group_id TEXT PRIMARY KEY,
                data TEXT NOT NULL,
                cached_at REAL NOT NULL
            );
        ").map_err(|e| format!("Failed to create cache table: {}", e))?;

        let cache = Self {
            mem: Mutex::new(mem_conn),
            disk_path: PathBuf::from(disk_path),
            ttl_secs,
        };

        // Restore from disk if available
        if cache.disk_path.exists() {
            match cache.load_from_disk() {
                Ok(ms) => tracing::info!("Resolution cache loaded from disk in {:.1}ms", ms),
                Err(e) => tracing::warn!("Could not load resolution cache from disk: {}", e),
            }
        }

        Ok(cache)
    }

    fn load(&self, group_id: &str) -> Option<ValidationResult> {
        let db = self.mem.lock();
        let row: Option<(String, f64)> = db.query_row(
            "SELECT data, cached_at FROM resolution_cache WHERE group_id = ?1",
            params![group_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)),
        ).ok();

        let (data_json, cached_at) = row?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        if now - cached_at > self.ttl_secs {
            return None; // Expired
        }

        serde_json::from_str(&data_json).ok()
    }

    fn save(&self, group_id: &str, result: &ValidationResult) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        let data_json = match serde_json::to_string(result) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("Failed to serialize validation result: {}", e);
                return;
            }
        };

        let db = self.mem.lock();
        let _ = db.execute(
            "INSERT OR REPLACE INTO resolution_cache (group_id, data, cached_at) VALUES (?1, ?2, ?3)",
            params![group_id, data_json, now],
        );
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
                            tracing::warn!("Resolution cache backup failed: {}", e);
                        }
                    }
                    Err(e) => tracing::warn!("Resolution cache backup init failed: {}", e),
                }
            }
            Err(e) => tracing::warn!("Failed to open resolution cache disk DB: {}", e),
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
// HTTP helpers
// ---------------------------------------------------------------------------

fn fetch_market_description(
    client: &reqwest::blocking::Client,
    market_id: i64,
) -> Option<(String, String, String)> {
    let url = format!("https://gamma-api.polymarket.com/markets/{}", market_id);
    let resp = match client.get(&url).timeout(Duration::from_secs(10)).send() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("Polymarket API request failed for market {}: {}", market_id, e);
            return None;
        }
    };

    if !resp.status().is_success() {
        tracing::warn!("Polymarket API returned {} for market {}", resp.status(), market_id);
        return None;
    }

    let data: serde_json::Value = match resp.json() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Failed to parse Polymarket response for market {}: {}", market_id, e);
            return None;
        }
    };

    let question = data.get("question").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let description = data.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let end_date = data.get("endDate").and_then(|v| v.as_str()).unwrap_or("").to_string();

    if description.is_empty() {
        return None;
    }

    Some((question, description, end_date))
}

fn call_anthropic(
    client: &reqwest::blocking::Client,
    config: &ValidatorConfig,
    api_key: &str,
    prompt: &str,
) -> Option<ValidationResult> {
    let body = serde_json::json!({
        "model": config.model,
        "max_tokens": config.max_tokens,
        "messages": [{"role": "user", "content": prompt}]
    });

    let resp = match client
        .post(&config.api_url)
        .header("Content-Type", "application/json")
        .header("x-api-key", api_key)
        .header("anthropic-version", &config.api_version)
        .timeout(Duration::from_secs(30))
        .json(&body)
        .send()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[rust_rv] Anthropic API request failed: {}", e);
            return None;
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        eprintln!("[rust_rv] Anthropic API returned {}: {}", status, &text[..text.len().min(500)]);
        return None;
    }

    let data: serde_json::Value = match resp.json() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[rust_rv] Failed to parse Anthropic response: {}", e);
            return None;
        }
    };

    // Extract text from content blocks
    let mut text = String::new();
    if let Some(content) = data.get("content").and_then(|v| v.as_array()) {
        for block in content {
            if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    text.push_str(t);
                }
            }
        }
    }

    let mut text = text.trim().to_string();

    // Strip markdown code fences
    if text.starts_with("```") {
        if let Some(idx) = text.find('\n') {
            text = text[idx + 1..].to_string();
        } else {
            text = text[3..].to_string();
        }
        if text.ends_with("```") {
            text = text[..text.len() - 3].to_string();
        }
        text = text.trim().to_string();
    }

    match serde_json::from_str::<ValidationResult>(&text) {
        Ok(result) => Some(result),
        Err(e) => {
            eprintln!(
                "[rust_rv] JSON parse failed: {}, text={}",
                e, &text[..text.len().min(200)]
            );
            None
        }
    }
}

fn format_prompt(template: &str, question: &str, description: &str, api_end_date: &str) -> String {
    template
        .replace("{question}", question)
        .replace("{description}", description)
        .replace("{api_end_date}", api_end_date)
        // Prompt templates use {{ and }} for literal braces (Python f-string convention)
        .replace("{{", "{")
        .replace("}}", "}")
}

// ---------------------------------------------------------------------------
// YAML config loading
// ---------------------------------------------------------------------------

fn load_prompt_template(workspace: &str) -> String {
    let path = PathBuf::from(workspace).join("config").join("prompts.yaml");
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            match serde_yaml::from_str::<serde_json::Value>(&contents) {
                Ok(val) => {
                    if let Some(tpl) = val.get("resolution_validation").and_then(|v| v.as_str()) {
                        tracing::info!("Loaded resolution prompt from {}", path.display());
                        return tpl.to_string();
                    }
                    tracing::warn!("No 'resolution_validation' key in {}", path.display());
                }
                Err(e) => tracing::warn!("Failed to parse {}: {}", path.display(), e),
            }
        }
        Err(e) => tracing::warn!("Failed to read {}: {}", path.display(), e),
    }
    // Hardcoded fallback (should not be reached in normal operation)
    tracing::warn!("Using hardcoded resolution prompt fallback");
    String::new()
}

fn load_config(workspace: &str) -> ValidatorConfig {
    let path = PathBuf::from(workspace).join("config").join("config.yaml");

    let mut cfg = ValidatorConfig {
        api_url: "https://api.anthropic.com/v1/messages".to_string(),
        api_version: "2023-06-01".to_string(),
        model: "claude-sonnet-4-20250514".to_string(),
        max_tokens: 256,
        cache_ttl_secs: 168.0 * 3600.0, // 7 days
    };

    if let Ok(contents) = std::fs::read_to_string(&path) {
        if let Ok(val) = serde_yaml::from_str::<serde_json::Value>(&contents) {
            // ai.api_url
            if let Some(url) = val.pointer("/ai/api_url").and_then(|v| v.as_str()) {
                cfg.api_url = url.to_string();
            }
            // ai.api_version
            if let Some(ver) = val.pointer("/ai/api_version").and_then(|v| v.as_str()) {
                cfg.api_version = ver.to_string();
            }
            // ai.models.resolution_validation
            if let Some(model) = val.pointer("/ai/models/resolution_validation").and_then(|v| v.as_str()) {
                cfg.model = model.to_string();
            }
            // ai.max_tokens.resolution_validation
            if let Some(tokens) = val.pointer("/ai/max_tokens/resolution_validation").and_then(|v| v.as_u64()) {
                cfg.max_tokens = tokens as u32;
            }
            // arbitrage.resolution_validation.cache_ttl_hours
            if let Some(hours) = val.pointer("/arbitrage/resolution_validation/cache_ttl_hours").and_then(|v| v.as_f64()) {
                cfg.cache_ttl_secs = hours * 3600.0;
            }

            tracing::info!(
                "Resolution config: model={} max_tokens={} ttl={}h",
                cfg.model, cfg.max_tokens, cfg.cache_ttl_secs / 3600.0
            );
        }
    }

    cfg
}

// ---------------------------------------------------------------------------
// PyO3 class
// ---------------------------------------------------------------------------

#[pyclass]
pub struct RustResolutionValidator {
    cache: ResolutionCache,
    client: reqwest::blocking::Client,
    config: ValidatorConfig,
    api_key: Mutex<String>,
    prompt_template: String,
}

#[pymethods]
impl RustResolutionValidator {
    /// Create a new resolution validator.
    ///
    /// Reads config/prompts.yaml and config/config.yaml from workspace.
    /// Opens (or creates) SQLite cache at {workspace}/data/resolution_cache.db.
    /// Loads existing cache from disk on startup.
    #[new]
    fn new(workspace: String, api_key: String) -> PyResult<Self> {
        let prompt_template = load_prompt_template(&workspace);
        if prompt_template.is_empty() {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "Failed to load resolution_validation prompt template"
            ));
        }

        let config = load_config(&workspace);

        let cache_path = PathBuf::from(&workspace)
            .join("data")
            .join("resolution_cache.db");
        // Ensure parent dir exists
        if let Some(parent) = cache_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let cache_path_str = cache_path.to_string_lossy().to_string();

        let cache = ResolutionCache::new(&cache_path_str, config.cache_ttl_secs)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))?;

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(
                format!("Failed to build HTTP client: {}", e)
            ))?;

        tracing::info!(
            "RustResolutionValidator initialized: model={}, cache={}",
            config.model, cache_path_str
        );

        Ok(Self {
            cache,
            client,
            config,
            api_key: Mutex::new(api_key),
            prompt_template,
        })
    }

    /// Full validation: cache check → Polymarket fetch → Anthropic API → cache save.
    /// Returns Python dict with validation result, or None.
    /// Releases GIL during HTTP calls.
    fn validate(&self, py: Python<'_>, group_id: String, market_id: i64) -> PyResult<PyObject> {
        // 1. Check cache (fast, no GIL release needed)
        if let Some(cached) = self.cache.load(&group_id) {
            return validation_to_pydict(py, &cached);
        }

        // 2. HTTP calls — release GIL
        let api_key = self.api_key.lock().clone();
        let prompt_template = self.prompt_template.clone();
        let config = self.config.clone();
        let client = self.client.clone();

        let result: Result<ValidationResult, String> = py.allow_threads(move || {
            // Fetch market description from Polymarket
            let (question, description, end_date) = match fetch_market_description(&client, market_id) {
                Some(details) => details,
                None => return Err(format!("fetch_market_description failed for mid={}", market_id)),
            };

            // Format prompt
            let prompt = format_prompt(&prompt_template, &question, &description, &end_date);

            // Call Anthropic API
            match call_anthropic(&client, &config, &api_key, &prompt) {
                Some(vr) => Ok(vr),
                None => Err("call_anthropic returned None".into()),
            }
        });

        match result {
            Ok(vr) => {
                self.cache.save(&group_id, &vr);
                validation_to_pydict(py, &vr)
            }
            Err(e) => {
                eprintln!("[rust_rv] validation failed for {}: {}", group_id, e);
                Ok(py.None())
            }
        }
    }

    /// Cache-only read for position scoring. Returns dict or None.
    fn load_cache(&self, py: Python<'_>, group_id: String) -> PyResult<PyObject> {
        match self.cache.load(&group_id) {
            Some(vr) => validation_to_pydict(py, &vr),
            None => Ok(py.None()),
        }
    }

    /// Backup in-memory cache to disk. Returns elapsed ms.
    fn mirror_to_disk(&self) -> f64 {
        self.cache.mirror_to_disk()
    }

    /// Update API key at runtime.
    fn set_api_key(&self, api_key: String) {
        *self.api_key.lock() = api_key;
    }
}

// ---------------------------------------------------------------------------
// Helper: convert ValidationResult to Python dict
// ---------------------------------------------------------------------------

fn validation_to_pydict(py: Python<'_>, vr: &ValidationResult) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("latest_resolution_date", &vr.latest_resolution_date)?;
    dict.set_item("confidence", &vr.confidence)?;
    dict.set_item("has_unrepresented_outcome", vr.has_unrepresented_outcome)?;
    dict.set_item("unrepresented_outcome_reason", &vr.unrepresented_outcome_reason)?;
    dict.set_item("reasoning", &vr.reasoning)?;
    Ok(dict.into())
}
