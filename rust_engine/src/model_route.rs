//! Claude model routing shared by the resolution validator and the postponement detector.
//!
//! Owner rule (claude-cli-pipelines):
//!  C1 this implementation ("prediction-trading") has its own Anthropic key.
//!  C2 own key set -> Messages API first, claude-gateway (the Claude subscription) second;
//!     ANY API failure falls back to the gateway.
//!  C3 no key -> the gateway is the backend. A missing key never disables a feature.
//!  C4 `PT_MODEL_ROUTE=gateway` (or `cli`) forces the gateway whatever keys exist (dev switch).
//!  C5 the route used is logged on every call (target `model_route`).
//!  C6 failures are logged at error level, never swallowed silently.
//!
//! The gateway is an HTTP service (`POST {url}/v1/complete`), authenticated by this
//! implementation's own bearer secret. It lives on the owner's box and is reached through a
//! Cloudflare tunnel, so no `claude` CLI is needed on the trading host.

use parking_lot::Mutex;
use secrecy::{ExposeSecret, SecretString};
use std::time::Duration;

/// Name registered in the gateway's config.json.
pub const IMPLEMENTATION: &str = "prediction-trading";
pub const DEFAULT_GATEWAY_URL: &str = "https://claude-gateway.rate-edge.com";
/// Dev switch: `gateway` / `cli` / `subscription` forces the gateway route.
pub const ROUTE_ENV: &str = "PT_MODEL_ROUTE";

/// Neutral system prompt for the gateway route (the subscription CLI replaces its own system
/// prompt with this; an empty one is avoided). The API route keeps sending no system prompt.
const GATEWAY_SYSTEM: &str =
    "You are a careful research assistant. Follow the user's instructions exactly and reply in the format they ask for.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Api,
    Gateway,
}

impl Route {
    pub fn as_str(self) -> &'static str {
        match self {
            Route::Api => "api",
            Route::Gateway => "gateway",
        }
    }
}

/// True when the dev-switch value asks for the subscription route.
pub fn force_gateway_from(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("gateway") | Some("cli") | Some("subscription")
    )
}

/// The ordered routes to try. Empty means no route exists.
pub fn plan_routes(has_key: bool, has_gateway: bool, force_gateway: bool) -> Vec<Route> {
    let mut routes = Vec::new();
    if has_key && !force_gateway {
        routes.push(Route::Api);
    }
    if has_gateway {
        routes.push(Route::Gateway);
    }
    routes
}

pub struct ModelRequest<'a> {
    /// Feature label for logs, e.g. "resolution" or "postponement".
    pub feature: &'a str,
    pub model: &'a str,
    pub max_tokens: u32,
    pub user: &'a str,
    pub web_search: bool,
    pub api_url: &'a str,
    pub api_version: &'a str,
    pub api_timeout: Duration,
}

#[derive(Debug)]
pub struct Completion {
    /// Text blocks of the reply, in order (callers join them as they always did).
    pub text_parts: Vec<String>,
    /// Number of web_search_tool_result blocks (API route only; 0 via the gateway).
    pub search_count: u32,
    pub route: Route,
    /// True when an earlier route failed and this one answered.
    pub fell_back: bool,
}

pub struct ModelRouter {
    api_key: Mutex<SecretString>,
    gateway_url: String,
    gateway_secret: SecretString,
    force_gateway: bool,
}

impl ModelRouter {
    pub fn new(api_key: &str, gateway_url: &str, gateway_secret: &str, force_gateway: bool) -> Self {
        let url = gateway_url.trim();
        Self {
            api_key: Mutex::new(SecretString::from(api_key.to_string())),
            gateway_url: if url.is_empty() { DEFAULT_GATEWAY_URL.to_string() } else { url.to_string() },
            gateway_secret: SecretString::from(gateway_secret.to_string()),
            force_gateway,
        }
    }

    fn has_key(&self) -> bool {
        !self.api_key.lock().expose_secret().is_empty()
    }

    fn has_gateway(&self) -> bool {
        !self.gateway_secret.expose_secret().is_empty()
    }

    /// True when at least one route can serve a call. Gate for building the features.
    pub fn any_route(&self) -> bool {
        !self.routes().is_empty()
    }

    pub fn routes(&self) -> Vec<Route> {
        plan_routes(self.has_key(), self.has_gateway(), self.force_gateway)
    }

    /// Update the API key at runtime.
    pub fn set_api_key(&self, api_key: &str) {
        *self.api_key.lock() = SecretString::from(api_key.to_string());
    }

    /// One-line description for the startup log. Never contains a secret.
    pub fn describe(&self) -> String {
        let names: Vec<&str> = self.routes().iter().map(|r| r.as_str()).collect();
        format!(
            "routes=[{}] own_key={} gateway_secret={} forced_gateway={}",
            names.join(" -> "),
            self.has_key(),
            self.has_gateway(),
            self.force_gateway
        )
    }

    /// Complete one request. None means every route failed (already logged at error level).
    pub fn complete(
        &self,
        client: &reqwest::blocking::Client,
        req: &ModelRequest<'_>,
    ) -> Option<Completion> {
        let routes = self.routes();
        run_routes(&routes, req.feature, |route| match route {
            Route::Api => self.call_api(client, req),
            Route::Gateway => self.call_gateway(client, req),
        })
    }

    fn call_api(
        &self,
        client: &reqwest::blocking::Client,
        req: &ModelRequest<'_>,
    ) -> Result<(Vec<String>, u32), String> {
        let mut body = serde_json::json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "messages": [{"role": "user", "content": req.user}]
        });
        if req.web_search {
            body["tools"] = serde_json::json!([{"type": "web_search_20250305", "name": "web_search"}]);
        }
        // Build under the lock so the key is not copied onto the heap.
        let request = {
            let key = self.api_key.lock();
            client
                .post(req.api_url)
                .header("Content-Type", "application/json")
                .header("x-api-key", key.expose_secret())
                .header("anthropic-version", req.api_version)
                .timeout(req.api_timeout)
                .json(&body)
        };
        let resp = request.send().map_err(|e| format!("request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().unwrap_or_default();
            return Err(format!("returned {status}: {}", text.get(..300).unwrap_or(&text)));
        }
        let data: serde_json::Value = resp.json().map_err(|e| format!("response was not JSON: {e}"))?;
        Ok(parse_api_content(&data))
    }

    fn call_gateway(
        &self,
        client: &reqwest::blocking::Client,
        req: &ModelRequest<'_>,
    ) -> Result<(Vec<String>, u32), String> {
        let timeout_ms: u64 = if req.web_search { 240_000 } else { 120_000 };
        let endpoint = format!("{}/v1/complete", self.gateway_url.trim_end_matches('/'));
        let body = gateway_body(req, timeout_ms);
        let resp = client
            .post(&endpoint)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", self.gateway_secret.expose_secret()))
            .timeout(Duration::from_millis(timeout_ms + 15_000))
            .json(&body)
            .send()
            .map_err(|e| format!("gateway unreachable: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().unwrap_or_default();
            return Err(format!("gateway answered {status}: {}", text.get(..300).unwrap_or(&text)));
        }
        let data: serde_json::Value =
            resp.json().map_err(|e| format!("gateway reply was not JSON: {e}"))?;
        let text = parse_gateway_reply(&data)?;
        if let Some(billed) = data.get("billedTo").and_then(|v| v.as_str()) {
            let gw_route = data.get("route").and_then(|v| v.as_str()).unwrap_or("?");
            tracing::debug!(target: "model_route", "gateway served via {gw_route}, billedTo={billed}");
        }
        Ok((vec![text], 0))
    }
}

/// Body for `POST /v1/complete`. `route: "cli"` because this client already tried its own key.
pub fn gateway_body(req: &ModelRequest<'_>, timeout_ms: u64) -> serde_json::Value {
    serde_json::json!({
        "implementation": IMPLEMENTATION,
        "site": "prediction-trading-production",
        "model": req.model,
        "system": GATEWAY_SYSTEM,
        "user": req.user,
        "webSearch": req.web_search,
        "route": "cli",
        "timeoutMs": timeout_ms
    })
}

pub fn parse_gateway_reply(data: &serde_json::Value) -> Result<String, String> {
    match data.get("text").and_then(|v| v.as_str()) {
        Some(t) => Ok(t.trim().to_string()),
        None => Err("gateway reply had no text".to_string()),
    }
}

/// Text blocks plus the number of web_search_tool_result blocks of a Messages API reply.
pub fn parse_api_content(data: &serde_json::Value) -> (Vec<String>, u32) {
    let mut parts = Vec::new();
    let mut searches = 0u32;
    if let Some(content) = data.get("content").and_then(|v| v.as_array()) {
        for block in content {
            match block.get("type").and_then(|v| v.as_str()) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        parts.push(t.to_string());
                    }
                }
                Some("web_search_tool_result") => searches += 1,
                _ => {}
            }
        }
    }
    (parts, searches)
}

/// Try the routes in order; any failure moves on to the next; log the route used on every call.
pub fn run_routes<F>(routes: &[Route], feature: &str, mut call: F) -> Option<Completion>
where
    F: FnMut(Route) -> Result<(Vec<String>, u32), String>,
{
    if routes.is_empty() {
        tracing::error!(
            target: "model_route",
            "[{feature}] no route: no API key and no claude_gateway secret (or {ROUTE_ENV} forces the gateway without one)"
        );
        return None;
    }
    let mut failed = false;
    for (i, route) in routes.iter().copied().enumerate() {
        match call(route) {
            Ok((text_parts, search_count)) => {
                tracing::info!(
                    target: "model_route",
                    "[{feature}] route={} ok fell_back={failed}",
                    route.as_str()
                );
                return Some(Completion { text_parts, search_count, route, fell_back: failed });
            }
            Err(e) => {
                failed = true;
                if i + 1 < routes.len() {
                    tracing::warn!(
                        target: "model_route",
                        "[{feature}] route={} failed ({e}); falling back to {}",
                        route.as_str(),
                        routes[i + 1].as_str()
                    );
                } else {
                    tracing::error!(
                        target: "model_route",
                        "[{feature}] route={} failed ({e}); no route left",
                        route.as_str()
                    );
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_switch_values() {
        assert!(force_gateway_from(Some("gateway")));
        assert!(force_gateway_from(Some(" CLI ")));
        assert!(force_gateway_from(Some("subscription")));
        assert!(!force_gateway_from(Some("api")));
        assert!(!force_gateway_from(Some("")));
        assert!(!force_gateway_from(None));
    }

    #[test]
    fn own_key_goes_api_first_then_gateway() {
        assert_eq!(plan_routes(true, true, false), vec![Route::Api, Route::Gateway]);
    }

    #[test]
    fn key_without_gateway_is_api_only() {
        assert_eq!(plan_routes(true, false, false), vec![Route::Api]);
    }

    #[test]
    fn no_key_uses_gateway() {
        assert_eq!(plan_routes(false, true, false), vec![Route::Gateway]);
    }

    #[test]
    fn dev_switch_forces_gateway_even_with_key() {
        assert_eq!(plan_routes(true, true, true), vec![Route::Gateway]);
    }

    #[test]
    fn forced_gateway_without_secret_has_no_route_and_never_spends_the_key() {
        assert!(plan_routes(true, false, true).is_empty());
    }

    #[test]
    fn nothing_configured_has_no_route() {
        assert!(plan_routes(false, false, false).is_empty());
    }

    #[test]
    fn router_any_route_follows_config() {
        assert!(!ModelRouter::new("", "", "", false).any_route());
        assert!(ModelRouter::new("k", "", "", false).any_route());
        assert!(ModelRouter::new("", "", "s", false).any_route());
        assert!(!ModelRouter::new("k", "", "", true).any_route());
        assert_eq!(
            ModelRouter::new("k", "", "s", false).routes(),
            vec![Route::Api, Route::Gateway]
        );
    }

    #[test]
    fn describe_never_leaks_secrets() {
        let d = ModelRouter::new("sk-ant-SECRETKEY", "", "GWSECRET", false).describe();
        assert!(!d.contains("SECRETKEY") && !d.contains("GWSECRET"));
        assert!(d.contains("api -> gateway"));
    }

    #[test]
    fn api_failure_falls_back_to_gateway() {
        let mut seen = Vec::new();
        let out = run_routes(&[Route::Api, Route::Gateway], "t", |r| {
            seen.push(r);
            match r {
                Route::Api => Err("529 overloaded".to_string()),
                Route::Gateway => Ok((vec!["hi".to_string()], 0)),
            }
        })
        .expect("gateway answers");
        assert_eq!(seen, vec![Route::Api, Route::Gateway]);
        assert_eq!(out.route, Route::Gateway);
        assert!(out.fell_back);
    }

    #[test]
    fn api_success_does_not_touch_gateway() {
        let mut seen = Vec::new();
        let out = run_routes(&[Route::Api, Route::Gateway], "t", |r| {
            seen.push(r);
            Ok((vec!["x".to_string()], 2))
        })
        .unwrap();
        assert_eq!(seen, vec![Route::Api]);
        assert_eq!(out.route, Route::Api);
        assert_eq!(out.search_count, 2);
        assert!(!out.fell_back);
    }

    #[test]
    fn all_routes_failing_returns_none() {
        let out = run_routes(&[Route::Api, Route::Gateway], "t", |_| Err("boom".to_string()));
        assert!(out.is_none());
    }

    #[test]
    fn empty_plan_returns_none_without_calling() {
        let mut called = false;
        let out = run_routes(&[], "t", |_| {
            called = true;
            Ok((vec![], 0))
        });
        assert!(out.is_none() && !called);
    }

    #[test]
    fn gateway_body_carries_identity_websearch_and_forces_subscription() {
        let req = ModelRequest {
            feature: "postponement",
            model: "claude-sonnet-4-20250514",
            max_tokens: 1000,
            user: "prompt",
            web_search: true,
            api_url: "u",
            api_version: "v",
            api_timeout: Duration::from_secs(1),
        };
        let b = gateway_body(&req, 240_000);
        assert_eq!(b["implementation"], "prediction-trading");
        assert_eq!(b["webSearch"], true);
        assert_eq!(b["route"], "cli");
        assert_eq!(b["model"], "claude-sonnet-4-20250514");
        assert_eq!(b["user"], "prompt");
        assert!(b["system"].as_str().map(|s| !s.is_empty()).unwrap_or(false));
        assert!(b.get("apiKey").is_none());
    }

    #[test]
    fn parses_gateway_reply_and_rejects_missing_text() {
        let ok = serde_json::json!({"text": "  {\"a\":1}\n", "route": "cli", "billedTo": "subscription"});
        assert_eq!(parse_gateway_reply(&ok).unwrap(), "{\"a\":1}");
        assert!(parse_gateway_reply(&serde_json::json!({"route": "cli"})).is_err());
    }

    #[test]
    fn parses_api_content_text_and_search_blocks() {
        let data = serde_json::json!({"content": [
            {"type": "text", "text": "one"},
            {"type": "web_search_tool_result", "content": []},
            {"type": "server_tool_use"},
            {"type": "text", "text": "two"}
        ]});
        let (parts, n) = parse_api_content(&data);
        assert_eq!(parts, vec!["one".to_string(), "two".to_string()]);
        assert_eq!(n, 1);
    }
}
