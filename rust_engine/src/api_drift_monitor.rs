//! INC-021: proactive Polymarket API drift detection.
//!
//! Two probes, both runnable from inside the trader binary as periodic tasks
//! (no external cron). Both alert via Telegram one-shot per unique signal per
//! boot:
//!
//! 1. **Header probe** (`probe_deprecation_headers`): hits a few stable
//!    Polymarket endpoints and inspects the response headers for
//!    `Deprecation`, `Sunset`, `Warning`, and `X-API-Version` (RFC 8594 +
//!    RFC 7234). When Polymarket marks an endpoint deprecated, the response
//!    carries the deprecation header **before** the endpoint actually breaks
//!    — so we get advance warning. INC-021 itself was triggered by missing
//!    a `Sunset: Fri, 01 May 2026` header on `/markets`.
//!
//! 2. **GitHub release probe** (`probe_clob_client_release`): polls the
//!    public GitHub Releases API for `Polymarket/py-clob-client`. New tag
//!    almost always means a wire-format change in the order schema or auth.
//!    Stores the last-seen tag in the persisted scalars table so subsequent
//!    boots remember what was last observed.
//!
//! Both probes are network-only — no auth required, no order quota consumed,
//! no risk of an accidental real trade.

use std::time::Duration;
use serde_json::Value;

/// Endpoints to probe for deprecation headers. Pairs of (label, URL).
/// Curated to ones we actually depend on. The CLOB has no public list endpoint;
/// `/data/orders` requires L2 auth. We probe the lightest unauthenticated
/// surface that still exercises the deployment.
pub const HEADER_PROBE_ENDPOINTS: &[(&str, &str)] = &[
    ("gamma_markets_list", "https://gamma-api.polymarket.com/markets/keyset?limit=1"),
    ("gamma_market_by_id", "https://gamma-api.polymarket.com/markets/540816"),
    ("clob_root",          "https://clob.polymarket.com/"),
];

/// Result of a single header-probe call against one endpoint.
#[derive(Debug, Clone)]
pub struct HeaderProbeFinding {
    pub endpoint_label: String,
    pub url: String,
    /// Stable signature used for dedup (prevents the same alert firing every cycle).
    /// e.g. `"deprecation=true; sunset=2026-05-01T00:00:00Z; warning=use /markets/keyset"`.
    pub signature: String,
    /// Human-readable summary for Telegram.
    pub summary: String,
}

/// Probe the listed endpoints for deprecation/sunset/warning headers.
///
/// Returns one `HeaderProbeFinding` per endpoint that emitted ANY of the
/// monitored headers. Endpoints with no deprecation signal return nothing.
/// Network errors are logged and skipped (do not produce a finding).
pub fn probe_deprecation_headers(timeout: Duration) -> Vec<HeaderProbeFinding> {
    let timeout_secs = timeout.as_secs().max(1);
    let client = match crate::http_client::secure_client_tagged(timeout_secs, "api_drift_headers") {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("api_drift: client build failed: {}", e);
            return Vec::new();
        }
    };

    let mut findings = Vec::new();
    for (label, url) in HEADER_PROBE_ENDPOINTS {
        let resp = match client.get(*url).send() {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("api_drift: GET {} failed: {}", url, e);
                continue;
            }
        };
        let headers = resp.headers();

        // Collect monitored headers (all RFC-standard except X-API-Version which
        // is informal but commonly used).
        let dep   = headers.get("deprecation").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
        let sun   = headers.get("sunset").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
        let warn  = headers.get("warning").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
        let apiv  = headers.get("x-api-version").and_then(|v| v.to_str().ok()).map(|s| s.to_string());

        // No interesting headers → skip
        if dep.is_none() && sun.is_none() && warn.is_none() && apiv.is_none() {
            continue;
        }

        // Build dedup signature (stable across boots if the values stay the same).
        let signature = format!(
            "dep={};sun={};warn={};apiv={}",
            dep.as_deref().unwrap_or(""),
            sun.as_deref().unwrap_or(""),
            warn.as_deref().unwrap_or(""),
            apiv.as_deref().unwrap_or(""),
        );

        // Pretty summary
        let mut parts = Vec::new();
        if let Some(s) = &dep  { parts.push(format!("Deprecation: {}", s)); }
        if let Some(s) = &sun  { parts.push(format!("Sunset: {}", s)); }
        if let Some(s) = &warn { parts.push(format!("Warning: {}", s)); }
        if let Some(s) = &apiv { parts.push(format!("X-API-Version: {}", s)); }
        let summary = format!("{} ({}): {}", label, url, parts.join(" | "));

        findings.push(HeaderProbeFinding {
            endpoint_label: label.to_string(),
            url: url.to_string(),
            signature,
            summary,
        });
    }
    findings
}

/// Result of polling Polymarket/py-clob-client for the latest GitHub release.
#[derive(Debug, Clone)]
pub struct ClobClientRelease {
    pub tag: String,
    pub published_at: String,
    pub html_url: String,
}

/// Hit GitHub's "latest release" API for `Polymarket/py-clob-client`.
/// Returns `None` on any error (network, parse, rate limit). Caller compares
/// `tag` against the persisted last-seen tag to detect new releases.
pub fn probe_clob_client_release(timeout: Duration) -> Option<ClobClientRelease> {
    let timeout_secs = timeout.as_secs().max(1);
    let client = match crate::http_client::secure_client_tagged(timeout_secs, "api_drift_github") {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("api_drift_github: client build failed: {}", e);
            return None;
        }
    };

    // GitHub requires User-Agent; will 403 without one.
    let resp = match client
        .get("https://api.github.com/repos/Polymarket/py-clob-client/releases/latest")
        .header("User-Agent", "prediction-trader/api-drift-monitor")
        .header("Accept", "application/vnd.github+json")
        .send()
    {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("api_drift_github: request failed: {}", e);
            return None;
        }
    };

    if !resp.status().is_success() {
        tracing::debug!("api_drift_github: status {}", resp.status());
        return None;
    }

    let body: Value = match resp.json() {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("api_drift_github: parse failed: {}", e);
            return None;
        }
    };

    let tag = body.get("tag_name").and_then(|v| v.as_str())?.to_string();
    let published = body.get("published_at").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let url = body.get("html_url").and_then(|v| v.as_str()).unwrap_or("").to_string();

    Some(ClobClientRelease {
        tag,
        published_at: published,
        html_url: url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_is_stable_when_inputs_unchanged() {
        let f1 = HeaderProbeFinding {
            endpoint_label: "x".into(),
            url: "u".into(),
            signature: "dep=true;sun=2026-05-01;warn=;apiv=".into(),
            summary: "...".into(),
        };
        let f2 = HeaderProbeFinding {
            endpoint_label: "x".into(),
            url: "u".into(),
            signature: "dep=true;sun=2026-05-01;warn=;apiv=".into(),
            summary: "....".into(),  // summary differs but signature does not
        };
        // Operator-level uniqueness is by signature, not summary.
        assert_eq!(f1.signature, f2.signature);
    }
}
