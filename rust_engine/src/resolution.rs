/// A2: Resolution Validator — Rust port of utilities/resolution_validator.py
///
/// Extracts true resolution dates from Polymarket market rules via Anthropic API.
/// Cache backed by in-memory SQLite with periodic disk backup (same pattern as StateDB).
///
/// Schema:
///   resolution_cache(group_id TEXT PK, data TEXT JSON, cached_at REAL)
use rusqlite::params;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::time::Duration;
use crate::cached_db::CachedSqliteDB;

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
pub struct ValidatorConfig {
    pub api_url: String,
    pub api_version: String,
    pub model: String,
    pub max_tokens: u32,
    pub cache_ttl_secs: f64,
}

// ---------------------------------------------------------------------------
// In-memory SQLite cache (mirrors StateDB pattern)
// ---------------------------------------------------------------------------

const RESOLUTION_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS resolution_cache (
        group_id TEXT PRIMARY KEY,
        data TEXT NOT NULL,
        cached_at REAL NOT NULL
    );
";

struct ResolutionCache {
    db: CachedSqliteDB,
    ttl_secs: f64,
}

impl ResolutionCache {
    fn new(disk_path: &str, ttl_secs: f64) -> Result<Self, String> {
        let db = CachedSqliteDB::new(disk_path, RESOLUTION_SCHEMA)?;

        // Restore from disk if available
        if db.disk_exists() {
            match db.load_from_disk() {
                Ok(ms) => tracing::info!("Resolution cache loaded from disk in {:.1}ms", ms),
                Err(e) => tracing::warn!("Could not load resolution cache from disk: {}", e),
            }
        }

        Ok(Self { db, ttl_secs })
    }

    fn load(&self, group_id: &str) -> Option<ValidationResult> {
        let db = self.db.conn();
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

        let db = self.db.conn();
        let _ = db.execute(
            "INSERT OR REPLACE INTO resolution_cache (group_id, data, cached_at) VALUES (?1, ?2, ?3)",
            params![group_id, data_json, now],
        );
    }

    fn mirror_to_disk(&self) -> f64 {
        self.db.mirror_to_disk()
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
            tracing::error!("[rust_rv] Anthropic API request failed: {}", e);
            return None;
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        tracing::error!("[rust_rv] Anthropic API returned {}: {}", status, &text[..text.len().min(500)]);
        return None;
    }

    let data: serde_json::Value = match resp.json() {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("[rust_rv] Failed to parse Anthropic response: {}", e);
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
            tracing::error!(
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
            match serde_yaml_ng::from_str::<serde_json::Value>(&contents) {
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
        if let Ok(val) = serde_yaml_ng::from_str::<serde_json::Value>(&contents) {
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
// ResolutionValidator
// ---------------------------------------------------------------------------

pub struct ResolutionValidator {
    cache: ResolutionCache,
    client: reqwest::blocking::Client,
    config: ValidatorConfig,
    api_key: Mutex<String>,
    prompt_template: String,
}

impl ResolutionValidator {
    /// Create a new resolution validator.
    ///
    /// Reads config/prompts.yaml and config/config.yaml from workspace.
    /// Opens (or creates) SQLite cache at {workspace}/data/resolution_cache.db.
    /// Loads existing cache from disk on startup.
    pub fn new(workspace: &str, api_key: &str) -> Result<Self, String> {
        let prompt_template = load_prompt_template(workspace);
        if prompt_template.is_empty() {
            return Err("Failed to load resolution_validation prompt template".into());
        }

        let config = load_config(workspace);

        let cache_path = PathBuf::from(workspace)
            .join("data")
            .join("resolution_cache.db");
        // Ensure parent dir exists
        if let Some(parent) = cache_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let cache_path_str = cache_path.to_string_lossy().to_string();

        let cache = ResolutionCache::new(&cache_path_str, config.cache_ttl_secs)?;

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

        tracing::info!(
            "ResolutionValidator initialized: model={}, cache={}",
            config.model, cache_path_str
        );

        Ok(Self {
            cache,
            client,
            config,
            api_key: Mutex::new(api_key.to_string()),
            prompt_template,
        })
    }

    /// Full validation: cache check -> Polymarket fetch -> Anthropic API -> cache save.
    /// Returns Some(ValidationResult) on success, or None on failure.
    pub fn validate(&self, group_id: &str, market_id: i64) -> Option<ValidationResult> {
        // 1. Check cache
        if let Some(cached) = self.cache.load(group_id) {
            return Some(cached);
        }

        // 2. Fetch market description from Polymarket
        let (question, description, end_date) = match fetch_market_description(&self.client, market_id) {
            Some(details) => details,
            None => {
                tracing::error!("[rust_rv] fetch_market_description failed for mid={}", market_id);
                return None;
            }
        };

        // 3. Format prompt
        let api_key = self.api_key.lock().clone();
        let prompt = format_prompt(&self.prompt_template, &question, &description, &end_date);

        // 4. Call Anthropic API
        match call_anthropic(&self.client, &self.config, &api_key, &prompt) {
            Some(vr) => {
                self.cache.save(group_id, &vr);
                Some(vr)
            }
            None => {
                tracing::error!("[rust_rv] validation failed for {}: call_anthropic returned None", group_id);
                None
            }
        }
    }

    /// Cache-only read for position scoring. Returns Some(result) or None.
    pub fn load_cache(&self, group_id: &str) -> Option<ValidationResult> {
        self.cache.load(group_id)
    }

    /// Backup in-memory cache to disk. Returns elapsed ms.
    pub fn mirror_to_disk(&self) -> f64 {
        self.cache.mirror_to_disk()
    }

    /// Update API key at runtime.
    pub fn set_api_key(&self, api_key: &str) {
        *self.api_key.lock() = api_key.to_string();
    }
}
