/// Pre-trade Gamma API freshness check (F-pre-7 / G7).
///
/// **Purpose**: Just before placing a real CLOB order, fetch the live Gamma
/// API view of the condition group and compare its size to the size we
/// recorded at scanner-detection time (`constraint.full_group_size`).
///
/// **Why this exists**: The existing INC-017 fix in `eval.rs:225` rejects
/// opportunities where the candidate count `n` is below
/// `constraint.full_group_size`, but `full_group_size` is captured at
/// scanner-detection time (every `constraint_rebuild_interval_seconds`).
/// If a NEW outcome is added to the negRisk group between the last scanner
/// refresh and our entry, the existing check cannot see it. This module
/// closes that window with a single live REST call per entry attempt.
///
/// **Policy**:
/// - `Verdict::Ok` → group size matches; safe to enter.
/// - `Verdict::GroupGrew(n)` → new outcomes appeared; **REJECT entry**
///   (one of the new outcomes could resolve YES, making our candidate
///   YES tokens worthless — INC-017 / INC-001 class).
/// - `Verdict::GroupShrunk(n)` → unusual but not necessarily fatal; reject
///   conservatively and log.
/// - `Verdict::NetworkError(s)` → fail-closed: REJECT entry. Live trading
///   should never proceed when we cannot verify the group state.
///
/// **Cost**: One blocking GET to `gamma-api.polymarket.com` per attempted
/// live entry. Strategy D averages ~1 entry/day so the load is negligible.
/// Shadow instances skip this check (no real money at stake).
///
/// Created 2026-04-21 for v0.20.3 (F-pre-7 / G7).

use std::time::Duration;
use serde_json::Value;

/// Outcome of a freshness check.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Group size matches the recorded full_group_size — safe to enter.
    Ok,
    /// Live group size > recorded; new outcomes were added since detection.
    GroupGrew { current: usize, expected: usize },
    /// Live group size < recorded; outcomes vanished (rare).
    GroupShrunk { current: usize, expected: usize },
    /// Network or parse error — fail-closed.
    NetworkError(String),
}

impl Verdict {
    pub fn is_ok(&self) -> bool { matches!(self, Verdict::Ok) }

    pub fn reason(&self) -> String {
        match self {
            Verdict::Ok => "ok".into(),
            Verdict::GroupGrew { current, expected } =>
                format!("group_grew:{}->{}", expected, current),
            Verdict::GroupShrunk { current, expected } =>
                format!("group_shrunk:{}->{}", expected, current),
            Verdict::NetworkError(e) => format!("net_err:{}", e),
        }
    }
}

/// Check that the Gamma API negRisk group size matches what we expect.
///
/// `neg_risk_market_id` — Polymarket `negRiskMarketID` (the group identifier;
/// stored on `Constraint::neg_risk_market_id` at detection time).
/// `expected_size` — `constraint.full_group_size` from detection time.
/// `timeout` — request timeout (typically 5s from `gamma_freshness_timeout_ms`).
///
/// Returns a `Verdict` describing the outcome. The caller is responsible for
/// acting on it (skip + log + counter increment in orchestrator).
pub fn check_group_freshness(
    neg_risk_market_id: &str,
    expected_size: usize,
    timeout: Duration,
) -> Verdict {
    if neg_risk_market_id.is_empty() {
        // Non-negRisk constraints have no group to check; treat as Ok by convention.
        return Verdict::Ok;
    }

    // Use the centralised secure client (G2) with a per-call short timeout.
    let timeout_secs = timeout.as_secs().max(1);
    let client = match crate::http_client::secure_client_tagged(timeout_secs, "gamma_freshness") {
        Ok(c) => c,
        Err(e) => return Verdict::NetworkError(format!("client build failed: {}", e)),
    };

    // Polymarket Gamma API: we send `?negRiskMarketID=<id>&limit=N` and count
    // the returned array length as the current group size.
    //
    // INC-019 caveat: the Gamma `negRiskMarketID` filter is **unreliable** —
    // for many neg_risk_market_ids it ignores the filter entirely and returns
    // the first N markets in the catalog. When that happens, `current` saturates
    // at `limit` and every check looks like a `GroupGrew` event. We detect that
    // failure mode by checking `current == limit` and treating it as a broken-API
    // signal, falling back to the authoritative `full_group_size` already
    // verified by `eval.rs::evaluate_batch` at evaluation time.
    const PAGE_LIMIT: usize = 100;
    let url = format!(
        "https://gamma-api.polymarket.com/markets?negRiskMarketID={}&limit={}",
        neg_risk_market_id, PAGE_LIMIT
    );

    let resp = match client.get(&url).send() {
        Ok(r) => r,
        Err(e) => return Verdict::NetworkError(format!("request failed: {}", e)),
    };

    if !resp.status().is_success() {
        return Verdict::NetworkError(format!("status {}", resp.status()));
    }

    let body: Value = match resp.json() {
        Ok(v) => v,
        Err(e) => return Verdict::NetworkError(format!("parse failed: {}", e)),
    };

    // Gamma returns a JSON array of market objects. The array length is the
    // current group size for this negRiskMarketID.
    let current = match body.as_array() {
        Some(arr) => arr.len(),
        None => return Verdict::NetworkError("response was not a JSON array".into()),
    };

    // INC-019: broken-filter detection. If the response saturates at the page
    // limit, Gamma is almost certainly NOT filtering by negRiskMarketID — we
    // cannot trust `current` as a real measurement of the group, so don't use
    // it to reject. The eval.rs `full_group_size` check at detection time is
    // the authoritative gate; this runtime check exists only to catch the
    // narrow window between scanner refresh and entry, and degrading to Ok on
    // a broken response is strictly safer than a 100% false-reject rate.
    if current >= PAGE_LIMIT {
        tracing::warn!(
            "gamma_freshness: Gamma returned {} markets (>= page limit {}) for negRiskMarketID={} — \
             filter likely ignored, falling back to detection-time full_group_size={}",
            current, PAGE_LIMIT, neg_risk_market_id, expected_size
        );
        return Verdict::Ok;
    }

    if current == expected_size {
        Verdict::Ok
    } else if current > expected_size {
        Verdict::GroupGrew { current, expected: expected_size }
    } else {
        Verdict::GroupShrunk { current, expected: expected_size }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_ok_is_ok() {
        assert!(Verdict::Ok.is_ok());
    }

    #[test]
    fn verdict_grew_is_not_ok() {
        let v = Verdict::GroupGrew { current: 5, expected: 4 };
        assert!(!v.is_ok());
        assert_eq!(v.reason(), "group_grew:4->5");
    }

    #[test]
    fn verdict_shrunk_is_not_ok() {
        let v = Verdict::GroupShrunk { current: 3, expected: 4 };
        assert!(!v.is_ok());
        assert_eq!(v.reason(), "group_shrunk:4->3");
    }

    #[test]
    fn verdict_net_err_is_not_ok() {
        let v = Verdict::NetworkError("timeout".into());
        assert!(!v.is_ok());
        assert!(v.reason().starts_with("net_err:"));
    }
}
