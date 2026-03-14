/// A3: Postponement Detector — Rust port of utilities/postponement_detector.py
///
/// Uses Anthropic API with web_search tool to detect postponed sporting events.
/// Two-attempt retry strategy: if attempt 1 finds postponement but no new date,
/// attempt 2 uses different search strategies with context injection.
///
/// Cache backed by in-memory SQLite with periodic disk backup (StateDB pattern).
///
/// Schema:
///   postponement_cache(position_id TEXT PK, data TEXT JSON, cached_at REAL)
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
struct PostponementResult {
    #[serde(default)]
    status: String,
    #[serde(default)]
    new_date: Option<String>,
    #[serde(default)]
    date_confidence: String,
    #[serde(default)]
    window_end: Option<String>,
    #[serde(default)]
    season_end: Option<String>,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    sources: Vec<String>,
    #[serde(default)]
    search_queries_used: Vec<String>,
    // Computed fields (not from API response)
    #[serde(default)]
    effective_resolution_date: Option<String>,
    #[serde(default)]
    checked_at: String,
    #[serde(default)]
    original_date: String,
    #[serde(default)]
    days_overdue: i64,
    #[serde(default)]
    _search_count: u32,
}

#[derive(Debug, Clone)]
struct PostponementConfig {
    api_url: String,
    api_version: String,
    model: String,
    max_tokens: u32,
    cache_ttl_secs: f64,
    rate_limit_secs: f64,
    max_attempts: u32,
    fallback_to_season_end: bool,
    date_buffer_hours: i64,
}

// ---------------------------------------------------------------------------
// In-memory SQLite cache (mirrors StateDB / ResolutionCache pattern)
// ---------------------------------------------------------------------------

struct PostponementCache {
    mem: Mutex<Connection>,
    disk_path: PathBuf,
    ttl_secs: f64,
}

impl PostponementCache {
    fn new(disk_path: &str, ttl_secs: f64) -> Result<Self, String> {
        let mem_conn = Connection::open_in_memory()
            .map_err(|e| format!("Failed to open in-memory SQLite: {}", e))?;

        mem_conn.execute_batch("
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=OFF;
            CREATE TABLE IF NOT EXISTS postponement_cache (
                position_id TEXT PRIMARY KEY,
                data TEXT NOT NULL,
                cached_at REAL NOT NULL
            );
        ").map_err(|e| format!("Failed to create cache table: {}", e))?;

        let cache = Self {
            mem: Mutex::new(mem_conn),
            disk_path: PathBuf::from(disk_path),
            ttl_secs,
        };

        if cache.disk_path.exists() {
            match cache.load_from_disk() {
                Ok(ms) => tracing::info!("Postponement cache loaded from disk in {:.1}ms", ms),
                Err(e) => tracing::warn!("Could not load postponement cache from disk: {}", e),
            }
        }

        Ok(cache)
    }

    fn load(&self, position_id: &str) -> Option<PostponementResult> {
        let db = self.mem.lock();
        let row: Option<(String, f64)> = db.query_row(
            "SELECT data, cached_at FROM postponement_cache WHERE position_id = ?1",
            params![position_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)),
        ).ok();

        let (data_json, cached_at) = row?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        if now - cached_at > self.ttl_secs {
            return None;
        }

        serde_json::from_str(&data_json).ok()
    }

    fn save(&self, position_id: &str, result: &PostponementResult) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        let data_json = match serde_json::to_string(result) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("Failed to serialize postponement result: {}", e);
                return;
            }
        };

        let db = self.mem.lock();
        let _ = db.execute(
            "INSERT OR REPLACE INTO postponement_cache (position_id, data, cached_at) VALUES (?1, ?2, ?3)",
            params![position_id, data_json, now],
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
                            tracing::warn!("Postponement cache backup failed: {}", e);
                        }
                    }
                    Err(e) => tracing::warn!("Postponement cache backup init failed: {}", e),
                }
            }
            Err(e) => tracing::warn!("Failed to open postponement cache disk DB: {}", e),
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
// HTTP helper: Anthropic API with web_search tool
// ---------------------------------------------------------------------------

fn call_anthropic_with_search(
    client: &reqwest::blocking::Client,
    config: &PostponementConfig,
    api_key: &str,
    prompt: &str,
    last_api_call: &Mutex<Instant>,
) -> Option<PostponementResult> {
    // Rate limiting
    {
        let last = last_api_call.lock();
        let elapsed = last.elapsed().as_secs_f64();
        if elapsed < config.rate_limit_secs {
            let wait = config.rate_limit_secs - elapsed;
            tracing::debug!("Rate limit: waiting {:.0}s before postponement API call", wait);
            std::thread::sleep(Duration::from_secs_f64(wait));
        }
    }

    let body = serde_json::json!({
        "model": config.model,
        "max_tokens": config.max_tokens,
        "tools": [{"type": "web_search_20250305", "name": "web_search"}],
        "messages": [{"role": "user", "content": prompt}]
    });

    // Update last call time before sending
    *last_api_call.lock() = Instant::now();

    let resp = match client
        .post(&config.api_url)
        .header("Content-Type", "application/json")
        .header("x-api-key", api_key)
        .header("anthropic-version", &config.api_version)
        .timeout(Duration::from_secs(90))
        .json(&body)
        .send()
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("Postponement API request failed: {}", e);
            return None;
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        tracing::warn!("Postponement API returned {}: {}",
            status, &text[..text.len().min(200)]);
        return None;
    }

    let data: serde_json::Value = match resp.json() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Failed to parse postponement API response: {}", e);
            return None;
        }
    };

    // Extract text blocks and count web_search_tool_result blocks
    let mut text_parts = Vec::new();
    let mut search_count: u32 = 0;
    if let Some(content) = data.get("content").and_then(|v| v.as_array()) {
        for block in content {
            match block.get("type").and_then(|v| v.as_str()) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        text_parts.push(t.to_string());
                    }
                }
                Some("web_search_tool_result") => {
                    search_count += 1;
                }
                _ => {}
            }
        }
    }

    let full_text = text_parts.join("\n");
    tracing::info!("Postponement API: {} web searches, {} chars response",
        search_count, full_text.len());

    // Extract JSON from response
    let mut result = extract_json(&full_text)?;
    result._search_count = search_count;
    Some(result)
}

/// Extract JSON from model response that may have text/markdown around it.
fn extract_json(text: &str) -> Option<PostponementResult> {
    let text = text.trim();

    // Try direct parse
    if let Ok(r) = serde_json::from_str::<PostponementResult>(text) {
        return Some(r);
    }

    // Try markdown code block
    if let Some(start) = text.find("```") {
        let after_fence = &text[start + 3..];
        // Skip language tag (e.g., ```json)
        let content_start = after_fence.find('\n').map(|i| i + 1).unwrap_or(0);
        let content = &after_fence[content_start..];
        if let Some(end) = content.find("```") {
            let json_str = content[..end].trim();
            if let Ok(r) = serde_json::from_str::<PostponementResult>(json_str) {
                return Some(r);
            }
        }
    }

    // Find last JSON object in text
    let mut depth = 0i32;
    let mut last_start = None;
    let mut last_json = None;
    for (i, ch) in text.char_indices() {
        match ch {
            '{' => {
                if depth == 0 {
                    last_start = Some(i);
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(start) = last_start {
                        let candidate = &text[start..=i];
                        if let Ok(r) = serde_json::from_str::<PostponementResult>(candidate) {
                            last_json = Some(r);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    last_json
}

// ---------------------------------------------------------------------------
// Date buffer: add hours and round to next midnight UTC
// ---------------------------------------------------------------------------

fn apply_date_buffer(date_str: &str, buffer_hours: i64) -> Option<String> {
    use chrono::{NaiveDate, Duration as CDuration};
    let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()?;
    let dt = date.and_hms_opt(0, 0, 0)?;
    let buffered = dt + CDuration::hours(buffer_hours);
    // Round up to next midnight if not already at midnight
    let result_date = if buffered.time() != chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap() {
        buffered.date() + CDuration::days(1)
    } else {
        buffered.date()
    };
    Some(result_date.format("%Y-%m-%d").to_string())
}

// ---------------------------------------------------------------------------
// Prompt formatting
// ---------------------------------------------------------------------------

fn format_prompt(
    template: &str,
    market_names: &[String],
    original_date: &str,
    today: &str,
    days_overdue: i64,
) -> String {
    let names_json = serde_json::to_string(market_names).unwrap_or_else(|_| "[]".into());
    template
        .replace("{market_names}", &names_json)
        .replace("{original_date}", original_date)
        .replace("{today}", today)
        .replace("{days_overdue}", &days_overdue.to_string())
}

fn format_retry_prompt(
    template: &str,
    prev: &PostponementResult,
    market_names: &[String],
    original_date: &str,
    today: &str,
) -> String {
    let names_json = serde_json::to_string(market_names).unwrap_or_else(|_| "[]".into());
    let sources_json = serde_json::to_string(&prev.sources).unwrap_or_else(|_| "[]".into());
    let queries_json = serde_json::to_string(&prev.search_queries_used).unwrap_or_else(|_| "[]".into());
    template
        .replace("{prev_status}", &prev.status)
        .replace("{prev_reason}", &prev.reason)
        .replace("{prev_sources}", &sources_json)
        .replace("{prev_queries}", &queries_json)
        .replace("{prev_season_end}", prev.season_end.as_deref().unwrap_or(""))
        .replace("{market_names}", &names_json)
        .replace("{original_date}", original_date)
        .replace("{today}", today)
}

// ---------------------------------------------------------------------------
// YAML config loading
// ---------------------------------------------------------------------------

fn load_prompts(workspace: &str) -> (String, String) {
    let path = PathBuf::from(workspace).join("config").join("prompts.yaml");
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            match serde_yaml::from_str::<serde_json::Value>(&contents) {
                Ok(val) => {
                    let detection = val.get("postponement_detection")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let retry = val.get("postponement_retry")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if !detection.is_empty() {
                        tracing::info!("Loaded postponement prompts from {}", path.display());
                    }
                    (detection, retry)
                }
                Err(e) => {
                    tracing::warn!("Failed to parse {}: {}", path.display(), e);
                    (String::new(), String::new())
                }
            }
        }
        Err(e) => {
            tracing::warn!("Failed to read {}: {}", path.display(), e);
            (String::new(), String::new())
        }
    }
}

fn load_config(workspace: &str) -> PostponementConfig {
    let path = PathBuf::from(workspace).join("config").join("config.yaml");

    let mut cfg = PostponementConfig {
        api_url: "https://api.anthropic.com/v1/messages".to_string(),
        api_version: "2023-06-01".to_string(),
        model: "claude-sonnet-4-20250514".to_string(),
        max_tokens: 1024,
        cache_ttl_secs: 24.0 * 3600.0,
        rate_limit_secs: 60.0,
        max_attempts: 2,
        fallback_to_season_end: true,
        date_buffer_hours: 24,
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
            // ai.models.postponement_detection
            if let Some(model) = val.pointer("/ai/models/postponement_detection").and_then(|v| v.as_str()) {
                cfg.model = model.to_string();
            }
            // ai.max_tokens.postponement_detection
            if let Some(tokens) = val.pointer("/ai/max_tokens/postponement_detection").and_then(|v| v.as_u64()) {
                cfg.max_tokens = tokens as u32;
            }
            // ai.postponement.*
            if let Some(pp) = val.pointer("/ai/postponement") {
                if let Some(ttl) = pp.get("cache_ttl_hours").and_then(|v| v.as_f64()) {
                    cfg.cache_ttl_secs = ttl * 3600.0;
                }
                if let Some(rl) = pp.get("rate_limit_seconds").and_then(|v| v.as_f64()) {
                    cfg.rate_limit_secs = rl;
                }
                if let Some(ma) = pp.get("max_attempts").and_then(|v| v.as_u64()) {
                    cfg.max_attempts = ma as u32;
                }
                if let Some(fb) = pp.get("fallback_to_season_end").and_then(|v| v.as_bool()) {
                    cfg.fallback_to_season_end = fb;
                }
                if let Some(bh) = pp.get("date_buffer_hours").and_then(|v| v.as_i64()) {
                    cfg.date_buffer_hours = bh;
                }
            }

            tracing::info!(
                "Postponement config: model={} max_tokens={} ttl={}h rate_limit={}s max_attempts={} buffer={}h",
                cfg.model, cfg.max_tokens, cfg.cache_ttl_secs / 3600.0,
                cfg.rate_limit_secs, cfg.max_attempts, cfg.date_buffer_hours
            );
        }
    }

    cfg
}

// ---------------------------------------------------------------------------
// PyO3 class
// ---------------------------------------------------------------------------

#[pyclass]
pub struct RustPostponementDetector {
    cache: PostponementCache,
    client: reqwest::blocking::Client,
    config: PostponementConfig,
    api_key: Mutex<String>,
    prompt_template: String,
    retry_prompt_template: String,
    last_api_call: Mutex<Instant>,
}

#[pymethods]
impl RustPostponementDetector {
    /// Create a new postponement detector.
    ///
    /// Reads config/prompts.yaml and config/config.yaml from workspace.
    /// Opens (or creates) SQLite cache at {workspace}/data/postponement_cache.db.
    #[new]
    fn new(workspace: String, api_key: String) -> PyResult<Self> {
        let (prompt_template, retry_prompt_template) = load_prompts(&workspace);
        if prompt_template.is_empty() {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "Failed to load postponement_detection prompt template"
            ));
        }

        let config = load_config(&workspace);

        let cache_path = PathBuf::from(&workspace)
            .join("data")
            .join("postponement_cache.db");
        if let Some(parent) = cache_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let cache_path_str = cache_path.to_string_lossy().to_string();

        let cache = PostponementCache::new(&cache_path_str, config.cache_ttl_secs)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))?;

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(90))
            .build()
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(
                format!("Failed to build HTTP client: {}", e)
            ))?;

        tracing::info!(
            "RustPostponementDetector initialized: model={}, cache={}",
            config.model, cache_path_str
        );

        Ok(Self {
            cache,
            client,
            config,
            api_key: Mutex::new(api_key),
            prompt_template,
            retry_prompt_template,
            last_api_call: Mutex::new(Instant::now() - Duration::from_secs(120)),
        })
    }

    /// Full postponement check: cache → attempt 1 → optional retry → date buffer → cache save.
    /// Returns Python dict with result, or None.
    /// Releases GIL during HTTP calls.
    fn check(
        &self,
        py: Python<'_>,
        position_id: String,
        market_names: Vec<String>,
        original_date: String,
    ) -> PyResult<PyObject> {
        // 1. Check cache
        if let Some(cached) = self.cache.load(&position_id) {
            tracing::debug!("Postponement cache hit for {}", &position_id[..position_id.len().min(30)]);
            return result_to_pydict(py, &cached);
        }

        // 2. Compute today and days_overdue
        let now = chrono::Utc::now();
        let today = now.format("%Y-%m-%d").to_string();
        let days_overdue = if let Ok(orig) = chrono::NaiveDate::parse_from_str(&original_date, "%Y-%m-%d") {
            let today_date = now.date_naive();
            (today_date - orig).num_days().max(0)
        } else {
            0
        };

        tracing::info!("Postponement check: {}... (overdue {}d)",
            market_names.first().map(|n| &n[..n.len().min(50)]).unwrap_or("?"),
            days_overdue);

        // 3. Release GIL for HTTP calls
        let api_key = self.api_key.lock().clone();
        let prompt_template = self.prompt_template.clone();
        let retry_template = self.retry_prompt_template.clone();
        let config = self.config.clone();
        let client = self.client.clone();
        let last_api_call = &self.last_api_call;

        let result: Option<PostponementResult> = py.allow_threads(|| {
            // Attempt 1
            let prompt = format_prompt(
                &prompt_template, &market_names, &original_date, &today, days_overdue,
            );
            let mut result = call_anthropic_with_search(
                &client, &config, &api_key, &prompt, last_api_call,
            )?;

            // Attempt 2: if postponed but no date found
            if result.status == "postponed"
                && result.new_date.is_none()
                && config.max_attempts >= 2
                && !retry_template.is_empty()
            {
                tracing::info!("  Attempt 1 found postponement but no date — retrying with context injection");
                let retry_prompt = format_retry_prompt(
                    &retry_template, &result, &market_names, &original_date, &today,
                );
                if let Some(result2) = call_anthropic_with_search(
                    &client, &config, &api_key, &retry_prompt, last_api_call,
                ) {
                    if result2.new_date.is_some() {
                        tracing::info!("  Attempt 2 found date: {}",
                            result2.new_date.as_deref().unwrap_or("?"));
                        result = result2;
                    } else {
                        // Merge: inherit season_end, merge sources
                        if result.season_end.is_none() {
                            result.season_end = result2.season_end;
                        }
                        let mut all_sources = result.sources.clone();
                        for s in &result2.sources {
                            if !all_sources.contains(s) {
                                all_sources.push(s.clone());
                            }
                        }
                        result.sources = all_sources;
                    }
                }
            }

            // Compute effective_resolution_date
            let raw_date = result.new_date.as_deref();
            let season_end = result.season_end.as_deref();

            let effective = if let Some(d) = raw_date {
                apply_date_buffer(d, config.date_buffer_hours)
            } else if config.fallback_to_season_end {
                if let Some(se) = season_end {
                    let eff = apply_date_buffer(se, config.date_buffer_hours);
                    if eff.is_some() {
                        result.date_confidence = "season_end".to_string();
                        result.new_date = Some(se.to_string());
                        tracing::info!("  No date found — falling back to season end: {} (+{}h = {})",
                            se, config.date_buffer_hours, eff.as_deref().unwrap_or("?"));
                    }
                    eff
                } else {
                    None
                }
            } else {
                None
            };

            result.effective_resolution_date = effective;
            result.checked_at = now.to_rfc3339();
            result.original_date = original_date;
            result.days_overdue = days_overdue;

            tracing::info!("  Result: status={} date={} confidence={} effective={}",
                result.status,
                result.new_date.as_deref().unwrap_or("null"),
                result.date_confidence,
                result.effective_resolution_date.as_deref().unwrap_or("null"));

            Some(result)
        });

        match result {
            Some(pr) => {
                self.cache.save(&position_id, &pr);
                result_to_pydict(py, &pr)
            }
            None => {
                tracing::warn!("Postponement check failed for {}",
                    &position_id[..position_id.len().min(30)]);
                Ok(py.None())
            }
        }
    }

    /// Cache-only read for replacement scoring. Returns dict or None.
    fn load_cache(&self, py: Python<'_>, position_id: String) -> PyResult<PyObject> {
        match self.cache.load(&position_id) {
            Some(pr) => result_to_pydict(py, &pr),
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
// Helper: convert PostponementResult to Python dict
// ---------------------------------------------------------------------------

fn result_to_pydict(py: Python<'_>, pr: &PostponementResult) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("status", &pr.status)?;
    dict.set_item("new_date", &pr.new_date)?;
    dict.set_item("date_confidence", &pr.date_confidence)?;
    dict.set_item("window_end", &pr.window_end)?;
    dict.set_item("season_end", &pr.season_end)?;
    dict.set_item("reason", &pr.reason)?;
    dict.set_item("sources", &pr.sources)?;
    dict.set_item("search_queries_used", &pr.search_queries_used)?;
    dict.set_item("effective_resolution_date", &pr.effective_resolution_date)?;
    dict.set_item("checked_at", &pr.checked_at)?;
    dict.set_item("original_date", &pr.original_date)?;
    dict.set_item("days_overdue", pr.days_overdue)?;
    Ok(dict.into())
}
