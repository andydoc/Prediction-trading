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
use rusqlite::params;
use parking_lot::Mutex;
use secrecy::{ExposeSecret, SecretString};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use crate::cached_db::CachedSqliteDB;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PostponementResult {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub new_date: Option<String>,
    #[serde(default)]
    pub date_confidence: String,
    #[serde(default)]
    pub window_end: Option<String>,
    #[serde(default)]
    pub season_end: Option<String>,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub search_queries_used: Vec<String>,
    // Computed fields (not from API response)
    #[serde(default)]
    pub effective_resolution_date: Option<String>,
    #[serde(default)]
    pub checked_at: String,
    #[serde(default)]
    pub original_date: String,
    #[serde(default)]
    pub days_overdue: i64,
    #[serde(default)]
    pub _search_count: u32,
}

#[derive(Debug, Clone)]
pub struct PostponementConfig {
    pub api_url: String,
    pub api_version: String,
    pub model: String,
    pub max_tokens: u32,
    pub cache_ttl_secs: f64,
    pub rate_limit_secs: f64,
    pub max_attempts: u32,
    pub fallback_to_season_end: bool,
    pub date_buffer_hours: i64,
}

// ---------------------------------------------------------------------------
// In-memory SQLite cache (mirrors StateDB / ResolutionCache pattern)
// ---------------------------------------------------------------------------

const POSTPONEMENT_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS postponement_cache (
        position_id TEXT PRIMARY KEY,
        data TEXT NOT NULL,
        cached_at REAL NOT NULL
    );
";

struct PostponementCache {
    db: CachedSqliteDB,
    ttl_secs: f64,
}

impl PostponementCache {
    fn new(disk_path: &str, ttl_secs: f64) -> Result<Self, String> {
        let db = CachedSqliteDB::new(disk_path, POSTPONEMENT_SCHEMA)?;

        if db.disk_exists() {
            match db.load_from_disk() {
                Ok(ms) => tracing::info!("Postponement cache loaded from disk in {:.1}ms", ms),
                Err(e) => tracing::warn!("Could not load postponement cache from disk: {}", e),
            }
        }

        Ok(Self { db, ttl_secs })
    }

    fn load(&self, position_id: &str) -> Option<PostponementResult> {
        let db = self.db.conn();
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

        let db = self.db.conn();
        let _ = db.execute(
            "INSERT OR REPLACE INTO postponement_cache (position_id, data, cached_at) VALUES (?1, ?2, ?3)",
            params![position_id, data_json, now],
        );
    }

    fn mirror_to_disk(&self) -> f64 {
        self.db.mirror_to_disk()
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
            status, text.get(..200).unwrap_or(&text));
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

    // Find last JSON object in text using naive brace-matching.
    // NOTE: This doesn't handle escaped braces inside JSON string values.
    // Acceptable because LLM responses rarely contain escaped braces in practice.
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
            match serde_yaml_ng::from_str::<serde_json::Value>(&contents) {
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
        if let Ok(val) = serde_yaml_ng::from_str::<serde_json::Value>(&contents) {
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
// PostponementDetector
// ---------------------------------------------------------------------------

pub struct PostponementDetector {
    cache: PostponementCache,
    client: reqwest::blocking::Client,
    config: PostponementConfig,
    api_key: Mutex<SecretString>,
    prompt_template: String,
    retry_prompt_template: String,
    last_api_call: Mutex<Instant>,
}

impl PostponementDetector {
    /// Create a new postponement detector.
    ///
    /// Reads config/prompts.yaml and config/config.yaml from workspace.
    /// Opens (or creates) SQLite cache at {workspace}/data/postponement_cache.db.
    pub fn new(workspace: &str, api_key: &str) -> Result<Self, String> {
        let (prompt_template, retry_prompt_template) = load_prompts(workspace);
        if prompt_template.is_empty() {
            return Err(
                "Failed to load postponement_detection prompt template".to_string()
            );
        }

        let config = load_config(workspace);

        let cache_path = PathBuf::from(workspace)
            .join("data")
            .join("postponement_cache.db");
        if let Some(parent) = cache_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let cache_path_str = cache_path.to_string_lossy().to_string();

        let cache = PostponementCache::new(&cache_path_str, config.cache_ttl_secs)?;

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(90))
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

        tracing::info!(
            "PostponementDetector initialized: model={}, cache={}",
            config.model, cache_path_str
        );

        Ok(Self {
            cache,
            client,
            config,
            api_key: Mutex::new(SecretString::from(api_key.to_string())),
            prompt_template,
            retry_prompt_template,
            last_api_call: Mutex::new(Instant::now() - Duration::from_secs(120)),
        })
    }

    /// Full postponement check: cache -> attempt 1 -> optional retry -> date buffer -> cache save.
    /// Returns PostponementResult on success, or None.
    pub fn check(
        &self,
        position_id: &str,
        market_names: &[String],
        original_date: &str,
    ) -> Option<PostponementResult> {
        // 1. Check cache
        if let Some(cached) = self.cache.load(position_id) {

            return Some(cached);
        }

        // 2. Compute today and days_overdue
        let now = chrono::Utc::now();
        let today = now.format("%Y-%m-%d").to_string();
        let days_overdue = if let Ok(orig) = chrono::NaiveDate::parse_from_str(original_date, "%Y-%m-%d") {
            let today_date = now.date_naive();
            (today_date - orig).num_days().max(0)
        } else {
            0
        };

        tracing::info!("Postponement check: {}... (overdue {}d)",
            market_names.first().map(|n| &n[..n.len().min(50)]).unwrap_or("?"),
            days_overdue);

        // 3. Attempt 1
        // S1: expose_secret() returns &str; .to_string() creates a heap copy because
        // the MutexGuard lifetime prevents passing &str directly across the lock boundary.
        let api_key = self.api_key.lock().expose_secret().to_string();
        let prompt = format_prompt(
            &self.prompt_template, market_names, original_date, &today, days_overdue,
        );
        let mut result = call_anthropic_with_search(
            &self.client, &self.config, &api_key, &prompt, &self.last_api_call,
        )?;

        // Attempt 2: if postponed but no date found
        if result.status == "postponed"
            && result.new_date.is_none()
            && self.config.max_attempts >= 2
            && !self.retry_prompt_template.is_empty()
        {
            tracing::info!("  Attempt 1 found postponement but no date — retrying with context injection");
            let retry_prompt = format_retry_prompt(
                &self.retry_prompt_template, &result, market_names, original_date, &today,
            );
            if let Some(result2) = call_anthropic_with_search(
                &self.client, &self.config, &api_key, &retry_prompt, &self.last_api_call,
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
            apply_date_buffer(d, self.config.date_buffer_hours)
        } else if self.config.fallback_to_season_end {
            if let Some(se) = season_end {
                let eff = apply_date_buffer(se, self.config.date_buffer_hours);
                if eff.is_some() {
                    result.date_confidence = "season_end".to_string();
                    result.new_date = Some(se.to_string());
                    tracing::info!("  No date found — falling back to season end: {} (+{}h = {})",
                        se, self.config.date_buffer_hours, eff.as_deref().unwrap_or("?"));
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
        result.original_date = original_date.to_string();
        result.days_overdue = days_overdue;

        tracing::info!("  Result: status={} date={} confidence={} effective={}",
            result.status,
            result.new_date.as_deref().unwrap_or("null"),
            result.date_confidence,
            result.effective_resolution_date.as_deref().unwrap_or("null"));

        // Save to cache on success
        self.cache.save(position_id, &result);

        Some(result)
    }

    /// Cache-only read for replacement scoring. Returns result or None.
    pub fn load_cache(&self, position_id: &str) -> Option<PostponementResult> {
        self.cache.load(position_id)
    }

    /// Backup in-memory cache to disk. Returns elapsed ms.
    pub fn mirror_to_disk(&self) -> f64 {
        self.cache.mirror_to_disk()
    }

    /// Update API key at runtime.
    pub fn set_api_key(&self, api_key: &str) {
        *self.api_key.lock() = SecretString::from(api_key.to_string());
    }
}
