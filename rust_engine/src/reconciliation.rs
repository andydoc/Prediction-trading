/// B4.0/B4.1: Position reconciliation framework.
///
/// Compares internal position state against venue (CLOB API) state.
/// Based on NautilusTrader's three-report reconciliation model:
///   - Orders: open orders on venue vs tracked orders
///   - Positions: token balances on venue vs internal positions
///   - Fills: recent fills on venue vs tracked fills
///
/// B4.2: Cross-asset fill matching for negRisk markets.
/// When a YES fill executes on a negRisk market, Polymarket creates
/// corresponding NO positions on complementary outcomes. Detects and
/// reconciles these synthetic fills.

use std::collections::HashMap;

/// Result of a reconciliation run.
#[derive(Debug, Clone)]
pub struct ReconciliationReport {
    /// Timestamp of the reconciliation run.
    pub timestamp: f64,
    /// Whether reconciliation passed (no critical discrepancies).
    pub passed: bool,
    /// Individual discrepancies found.
    pub discrepancies: Vec<Discrepancy>,
    /// Number of positions checked.
    pub positions_checked: usize,
    /// Number of positions matched.
    pub positions_matched: usize,
    /// Whether this was a startup reconciliation.
    pub is_startup: bool,
}

/// A single discrepancy between internal and venue state.
#[derive(Debug, Clone)]
pub struct Discrepancy {
    pub kind: DiscrepancyKind,
    pub severity: Severity,
    pub description: String,
    /// Position ID (if applicable).
    pub position_id: Option<String>,
    /// Market ID (if applicable).
    pub market_id: Option<String>,
    /// Internal value.
    pub internal_value: Option<f64>,
    /// Venue value.
    pub venue_value: Option<f64>,
}

/// Types of discrepancies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscrepancyKind {
    /// Position exists internally but not on venue.
    PositionMissingOnVenue,
    /// Position exists on venue but not internally.
    PositionMissingInternal,
    /// Quantity mismatch between internal and venue.
    QuantityMismatch,
    /// Open order on venue not tracked internally.
    OrphanOrder,
    /// B4.2: Synthetic NO position from negRisk YES fill.
    SyntheticNegRiskPosition,
    /// B4.3: Overfill detected (tracked separately in executor).
    OverfillDetected,
    /// Market flagged as disputed (UMA).
    DisputedMarket,
}

/// Severity of a discrepancy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Informational — expected behavior (e.g., synthetic negRisk positions).
    Info,
    /// Warning — unexpected but not critical (e.g., small quantity drift).
    Warning,
    /// Critical — requires attention (e.g., missing position, large mismatch).
    Critical,
}

impl ReconciliationReport {
    pub fn new(is_startup: bool) -> Self {
        Self {
            timestamp: chrono::Utc::now().timestamp() as f64,
            passed: true,
            discrepancies: Vec::new(),
            positions_checked: 0,
            positions_matched: 0,
            is_startup,
        }
    }

    pub fn add(&mut self, d: Discrepancy) {
        if d.severity == Severity::Critical {
            self.passed = false;
        }
        self.discrepancies.push(d);
    }

    pub fn critical_count(&self) -> usize {
        self.discrepancies.iter().filter(|d| d.severity == Severity::Critical).count()
    }

    pub fn warning_count(&self) -> usize {
        self.discrepancies.iter().filter(|d| d.severity == Severity::Warning).count()
    }
}

// ---------------------------------------------------------------------------
// B4.0/B4.1: Compare internal positions against CLOB API response
// ---------------------------------------------------------------------------

/// Venue-reported position (from CLOB API `/positions` or `/balances`).
#[derive(Debug, Clone)]
pub struct VenuePosition {
    pub asset_id: String,
    pub market_id: String,
    pub size: f64,         // Token balance
    pub avg_price: f64,    // Average entry price (if available)
    pub side: String,      // "BUY" or "SELL"
    pub condition_id: String,
}

/// Compare internal open positions against venue-reported positions.
///
/// Returns a ReconciliationReport with any discrepancies.
/// Internal positions are (position_id, market_id, shares).
/// Venue positions are grouped by market_id.
pub fn compare_positions(
    internal: &[(String, String, f64)], // (position_id, market_id, shares)
    venue: &[VenuePosition],
    escalation_threshold: f64,
    is_startup: bool,
) -> ReconciliationReport {
    let mut report = ReconciliationReport::new(is_startup);

    // Build venue lookup: market_id → total size
    let mut venue_by_market: HashMap<String, f64> = HashMap::new();
    for vp in venue {
        *venue_by_market.entry(vp.market_id.clone()).or_default() += vp.size;
    }

    // Build internal lookup: market_id → (position_id, total shares)
    let mut internal_by_market: HashMap<String, (String, f64)> = HashMap::new();
    for (pid, mid, shares) in internal {
        let entry = internal_by_market.entry(mid.clone()).or_insert_with(|| (pid.clone(), 0.0));
        entry.1 += shares;
    }

    // Check internal positions against venue
    for (mid, (pid, internal_shares)) in &internal_by_market {
        report.positions_checked += 1;

        match venue_by_market.get(mid) {
            Some(&venue_shares) => {
                let diff = (internal_shares - venue_shares).abs();
                if diff < 0.01 {
                    report.positions_matched += 1;
                } else {
                    let severity = if diff > escalation_threshold {
                        Severity::Critical
                    } else {
                        Severity::Warning
                    };
                    report.add(Discrepancy {
                        kind: DiscrepancyKind::QuantityMismatch,
                        severity,
                        description: format!(
                            "Market {} qty mismatch: internal={:.4} venue={:.4} diff={:.4}",
                            mid, internal_shares, venue_shares, diff
                        ),
                        position_id: Some(pid.clone()),
                        market_id: Some(mid.clone()),
                        internal_value: Some(*internal_shares),
                        venue_value: Some(venue_shares),
                    });
                }
            }
            None => {
                // Position exists internally but not on venue
                report.add(Discrepancy {
                    kind: DiscrepancyKind::PositionMissingOnVenue,
                    severity: Severity::Critical,
                    description: format!(
                        "Market {} exists internally ({:.4} shares) but not on venue",
                        mid, internal_shares
                    ),
                    position_id: Some(pid.clone()),
                    market_id: Some(mid.clone()),
                    internal_value: Some(*internal_shares),
                    venue_value: None,
                });
            }
        }
    }

    // Check venue positions not tracked internally
    for (mid, &venue_shares) in &venue_by_market {
        if !internal_by_market.contains_key(mid) && venue_shares > 0.01 {
            report.add(Discrepancy {
                kind: DiscrepancyKind::PositionMissingInternal,
                severity: Severity::Warning,
                description: format!(
                    "Market {} exists on venue ({:.4} shares) but not tracked internally",
                    mid, venue_shares
                ),
                position_id: None,
                market_id: Some(mid.clone()),
                internal_value: None,
                venue_value: Some(venue_shares),
            });
        }
    }

    report
}

// ---------------------------------------------------------------------------
// B4.2: Cross-asset fill matching for negRisk markets
// ---------------------------------------------------------------------------

/// Detect synthetic NO positions created by negRisk YES fills.
///
/// When buying YES on outcome A in a negRisk multi-outcome market,
/// Polymarket's CTF adapter implicitly creates NO positions on all
/// other outcomes. This function detects those synthetic positions
/// in the venue state and reports them as informational discrepancies.
///
/// NT lesson (#3345/#3357): Never filter fills by single asset_id.
/// Cross-asset matches mean maker fills appear under the taker's asset.
pub fn detect_neg_risk_synthetics(
    venue_positions: &[VenuePosition],
    internal_market_ids: &[String],
    neg_risk_condition_ids: &[String],
) -> Vec<Discrepancy> {
    let mut synthetics = Vec::new();
    let internal_set: std::collections::HashSet<&String> = internal_market_ids.iter().collect();

    for vp in venue_positions {
        // Only check negRisk markets
        if !neg_risk_condition_ids.contains(&vp.condition_id) {
            continue;
        }
        // If venue has a position we don't track, and it's in a negRisk group, it's likely synthetic
        if !internal_set.contains(&vp.market_id) && vp.size > 0.01 {
            synthetics.push(Discrepancy {
                kind: DiscrepancyKind::SyntheticNegRiskPosition,
                severity: Severity::Info,
                description: format!(
                    "Synthetic negRisk position: market={} size={:.4} condition={}",
                    vp.market_id, vp.size, vp.condition_id
                ),
                position_id: None,
                market_id: Some(vp.market_id.clone()),
                internal_value: None,
                venue_value: Some(vp.size),
            });
        }
    }

    synthetics
}

// ---------------------------------------------------------------------------
// CLOB API query helpers
// ---------------------------------------------------------------------------

/// Query the CLOB API for user positions.
///
/// Requires CLOB API credentials (API key, secret, passphrase).
/// Returns parsed venue positions or an error message.
///
/// Endpoint: GET /positions
/// Auth: POLY_API_KEY, POLY_TIMESTAMP, POLY_SIGNATURE headers
pub fn query_clob_positions(
    _http_client: &reqwest::blocking::Client,
    _clob_host: &str,
    _api_key: &str,
    _api_secret: &str,
    _passphrase: &str,
) -> Result<Vec<VenuePosition>, String> {
    // TODO: Implement when CLOB API credentials are available.
    // For shadow mode, this returns empty (no real positions to reconcile).
    //
    // Implementation notes from NT:
    //   1. Sign request: HMAC-SHA256(timestamp + "GET" + "/positions" + "")
    //   2. Headers: POLY_API_KEY, POLY_TIMESTAMP, POLY_SIGNATURE, POLY_PASSPHRASE
    //   3. Parse response: array of { asset_id, market, size, avg_price, side }
    //   4. Filter: only non-zero positions (size > 0)
    tracing::debug!("B4.0: CLOB position query skipped (no API credentials configured)");
    Ok(Vec::new())
}

/// Run startup reconciliation (B4.1).
///
/// Called once at engine startup after state is loaded from SQLite.
/// Compares loaded positions against CLOB API state.
pub fn reconcile_startup(
    open_positions: &[(String, String, f64)], // (position_id, market_id, shares)
    http_client: &reqwest::blocking::Client,
    clob_host: &str,
    escalation_threshold: f64,
) -> ReconciliationReport {
    let venue = match query_clob_positions(http_client, clob_host, "", "", "") {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("B4.1 startup reconciliation: CLOB query failed: {}", e);
            let mut report = ReconciliationReport::new(true);
            report.passed = true; // Don't block startup on query failure
            return report;
        }
    };

    // No credentials configured — skip comparison (shadow mode)
    if venue.is_empty() {
        tracing::info!(
            "B4.1 startup reconciliation: skipped (no venue credentials, {} internal positions)",
            open_positions.len()
        );
        let mut report = ReconciliationReport::new(true);
        report.positions_checked = open_positions.len();
        return report;
    }

    let report = compare_positions(open_positions, &venue, escalation_threshold, true);

    if report.passed {
        tracing::info!(
            "B4.1 startup reconciliation: PASSED ({}/{} positions matched, {} discrepancies)",
            report.positions_matched, report.positions_checked, report.discrepancies.len()
        );
    } else {
        tracing::warn!(
            "B4.1 startup reconciliation: FAILED ({} critical, {} warnings)",
            report.critical_count(), report.warning_count()
        );
        for d in &report.discrepancies {
            if d.severity == Severity::Critical {
                tracing::error!("  CRITICAL: {}", d.description);
            }
        }
    }

    report
}

/// Run periodic reconciliation (B4.0).
///
/// Called on interval from the orchestrator tick loop.
/// Same as startup but with additional disputed market detection.
pub fn reconcile_periodic(
    open_positions: &[(String, String, f64)],
    http_client: &reqwest::blocking::Client,
    clob_host: &str,
    escalation_threshold: f64,
) -> ReconciliationReport {
    let venue = match query_clob_positions(http_client, clob_host, "", "", "") {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("B4.0 periodic reconciliation: CLOB query failed: {}", e);
            let mut report = ReconciliationReport::new(false);
            report.passed = true;
            return report;
        }
    };

    // No credentials configured — skip comparison (shadow mode)
    if venue.is_empty() {
        tracing::debug!("B4.0 periodic reconciliation: skipped (no venue credentials)");
        let mut report = ReconciliationReport::new(false);
        report.positions_checked = open_positions.len();
        return report;
    }

    let report = compare_positions(open_positions, &venue, escalation_threshold, false);

    let level = if report.passed { "PASSED" } else { "ATTENTION" };
    tracing::info!(
        "B4.0 reconciliation: {} ({}/{} matched, {} critical, {} warnings)",
        level, report.positions_matched, report.positions_checked,
        report.critical_count(), report.warning_count()
    );

    report
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compare_positions_all_match() {
        let internal = vec![
            ("pos1".into(), "mkt1".into(), 100.0),
            ("pos2".into(), "mkt2".into(), 50.0),
        ];
        let venue = vec![
            VenuePosition { asset_id: "a1".into(), market_id: "mkt1".into(), size: 100.0, avg_price: 0.5, side: "BUY".into(), condition_id: "c1".into() },
            VenuePosition { asset_id: "a2".into(), market_id: "mkt2".into(), size: 50.0, avg_price: 0.3, side: "BUY".into(), condition_id: "c2".into() },
        ];
        let report = compare_positions(&internal, &venue, 1.0, false);
        assert!(report.passed);
        assert_eq!(report.positions_matched, 2);
        assert_eq!(report.discrepancies.len(), 0);
    }

    #[test]
    fn test_compare_positions_quantity_mismatch() {
        let internal = vec![("pos1".into(), "mkt1".into(), 100.0)];
        let venue = vec![
            VenuePosition { asset_id: "a1".into(), market_id: "mkt1".into(), size: 95.0, avg_price: 0.5, side: "BUY".into(), condition_id: "c1".into() },
        ];
        let report = compare_positions(&internal, &venue, 10.0, false);
        assert_eq!(report.discrepancies.len(), 1);
        assert_eq!(report.discrepancies[0].kind, DiscrepancyKind::QuantityMismatch);
    }

    #[test]
    fn test_compare_positions_missing_on_venue() {
        let internal = vec![("pos1".into(), "mkt1".into(), 100.0)];
        let venue: Vec<VenuePosition> = vec![];
        let report = compare_positions(&internal, &venue, 1.0, false);
        assert!(!report.passed);
        assert_eq!(report.discrepancies[0].kind, DiscrepancyKind::PositionMissingOnVenue);
    }

    #[test]
    fn test_compare_positions_missing_internal() {
        let internal: Vec<(String, String, f64)> = vec![];
        let venue = vec![
            VenuePosition { asset_id: "a1".into(), market_id: "mkt1".into(), size: 50.0, avg_price: 0.5, side: "BUY".into(), condition_id: "c1".into() },
        ];
        let report = compare_positions(&internal, &venue, 1.0, false);
        assert!(report.passed); // Warning, not critical
        assert_eq!(report.discrepancies[0].kind, DiscrepancyKind::PositionMissingInternal);
    }

    #[test]
    fn test_neg_risk_synthetic_detection() {
        let venue = vec![
            VenuePosition { asset_id: "a1".into(), market_id: "mkt1".into(), size: 100.0, avg_price: 0.5, side: "BUY".into(), condition_id: "neg1".into() },
            VenuePosition { asset_id: "a2".into(), market_id: "mkt2".into(), size: 100.0, avg_price: 0.5, side: "BUY".into(), condition_id: "neg1".into() },
        ];
        let internal_markets = vec!["mkt1".to_string()];
        let neg_risk_ids = vec!["neg1".to_string()];

        let synthetics = detect_neg_risk_synthetics(&venue, &internal_markets, &neg_risk_ids);
        assert_eq!(synthetics.len(), 1);
        assert_eq!(synthetics[0].kind, DiscrepancyKind::SyntheticNegRiskPosition);
        assert_eq!(synthetics[0].market_id.as_deref(), Some("mkt2"));
    }

    #[test]
    fn test_empty_reconciliation() {
        let report = compare_positions(&[], &[], 1.0, true);
        assert!(report.passed);
        assert_eq!(report.positions_checked, 0);
    }
}
