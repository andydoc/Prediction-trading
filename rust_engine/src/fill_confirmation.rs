/// Parallel trade fill confirmation (B4.5).
///
/// Races WebSocket User Channel + Data API polling to confirm trade fills.
/// WS is typically faster but unreliable (~20% CONFIRMED drop rate).
/// Data API is slower but reliable (polls position deltas).
///
/// Flow:
///   1. Both paths run concurrently via tokio::select!
///   2. WS MATCHED → enter suspense immediately (B3.2)
///   3. Data API position delta → confirms fill independently
///   4. First source to detect all expected fills terminates the other
///   5. Partial fills are handled per existing B3.6 logic (not changed here)

use std::collections::HashMap;
use std::time::Duration;

use crossbeam_channel::Receiver;

use crate::ws_user::{TradeEvent, UserEvent};
use crate::reconciliation::{query_clob_positions, VenuePosition};
use crate::signing::ClobAuth;

/// Source that detected a fill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FillSource {
    WebSocket,
    DataApi,
}

/// A confirmed fill from either source.
#[derive(Debug, Clone)]
pub struct ConfirmedFill {
    pub asset_id: String,
    pub shares: f64,
    pub price: f64,
    /// Trade UUID from WS (only if WS detected it).
    pub trade_id: Option<String>,
    pub source: FillSource,
}

/// Result of parallel fill confirmation.
#[derive(Debug)]
pub struct FillConfirmationResult {
    pub fills: Vec<ConfirmedFill>,
    pub ws_events: Vec<TradeEvent>,
    pub primary_source: FillSource,
}

/// Wait for fills via WS User Channel (returns on MATCHED for all assets).
///
/// This is a refactored version of UserChannelClient::wait_for_confirmed_fills
/// that returns immediately once all expected assets have MATCHED events.
/// CONFIRMED/MINED are still tracked for upgrade but don't block return.
pub fn wait_for_matched_fills(
    receiver: &Receiver<UserEvent>,
    asset_ids: &[String],
    timeout: Duration,
) -> Vec<TradeEvent> {
    let deadline = std::time::Instant::now() + timeout;
    let mut fills: Vec<TradeEvent> = Vec::new();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut pending: std::collections::HashSet<String> = asset_ids.iter().cloned().collect();

    while !pending.is_empty() {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            tracing::warn!("B4.5 WS: timeout after {:.0}s, {} fills, {} pending",
                timeout.as_secs_f64(), fills.len(), pending.len());
            break;
        }

        match receiver.recv_timeout(remaining.min(Duration::from_secs(2))) {
            Ok(UserEvent::Trade(trade)) => {
                let dominated = trade.status == "MATCHED"
                    || trade.status == "MINED"
                    || trade.status == "CONFIRMED";
                if !dominated { continue; }

                let is_our_asset = pending.contains(&trade.asset_id);
                if !is_our_asset { continue; }

                // Dedup by trade id
                if seen_ids.contains(&trade.id) { continue; }
                seen_ids.insert(trade.id.clone());

                tracing::info!("B4.5 WS: {} for asset {}... (id={}...)",
                    trade.status,
                    trade.asset_id.get(..12).unwrap_or(&trade.asset_id),
                    trade.id.get(..12).unwrap_or(&trade.id));

                // MATCHED is sufficient — remove from pending immediately
                pending.remove(&trade.asset_id);
                fills.retain(|f| f.asset_id != trade.asset_id);
                fills.push(trade);
            }
            Ok(UserEvent::Order(_)) => {}
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }

    fills
}

/// Poll Data API for position changes indicating fills.
///
/// Compares current positions against baseline sizes. A fill is detected
/// when a position's size increases (buy) or appears for the first time.
pub fn poll_data_api_for_fills(
    http_client: &reqwest::blocking::Client,
    auth: &ClobAuth,
    asset_ids: &[String],
    baseline_sizes: &HashMap<String, f64>,
    poll_interval: Duration,
    timeout: Duration,
) -> Vec<ConfirmedFill> {
    let deadline = std::time::Instant::now() + timeout;
    let mut fills: Vec<ConfirmedFill> = Vec::new();
    let mut pending: std::collections::HashSet<String> = asset_ids.iter().cloned().collect();

    while !pending.is_empty() {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            tracing::warn!("B4.5 Data API: timeout after {:.0}s, {} fills, {} pending",
                timeout.as_secs_f64(), fills.len(), pending.len());
            break;
        }

        std::thread::sleep(poll_interval.min(remaining));

        match query_clob_positions(http_client, "https://clob.polymarket.com", Some(auth)) {
            Ok(positions) => {
                let current: HashMap<String, &VenuePosition> = positions.iter()
                    .map(|p| (p.asset_id.clone(), p))
                    .collect();

                for asset_id in pending.clone() {
                    let baseline = baseline_sizes.get(&asset_id).copied().unwrap_or(0.0);
                    if let Some(vp) = current.get(&asset_id) {
                        let delta = vp.size - baseline;
                        if delta > 0.001 {
                            // R2: Validate Data API fill price (matching BUG-2 WS validation)
                            if vp.avg_price <= 0.0 {
                                tracing::warn!("B4.5: Rejecting invalid Data API fill: asset={} avg_price={}",
                                    asset_id.get(..12).unwrap_or(&asset_id), vp.avg_price);
                                continue;
                            }
                            tracing::info!("B4.5 Data API: fill detected for {} delta={:.2} (baseline={:.2} → {:.2})",
                                asset_id.get(..12).unwrap_or(&asset_id), delta, baseline, vp.size);
                            pending.remove(&asset_id);
                            fills.push(ConfirmedFill {
                                asset_id,
                                shares: delta,
                                price: vp.avg_price,
                                trade_id: None,
                                source: FillSource::DataApi,
                            });
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("B4.5 Data API poll error: {}", e);
            }
        }
    }

    fills
}

/// Race WS and Data API for trade fill confirmation (B4.5).
///
/// Both paths run concurrently in separate threads. First source to detect
/// all expected fills causes the function to return. MATCHED events from WS
/// trigger suspense entry (B3.2) via the returned events — the caller is
/// responsible for calling `accounting.enter_suspense()`.
///
/// Partial fills are handled per existing B3.6 logic — if only some assets
/// fill within the timeout, the caller gets whatever fills were detected.
pub fn confirm_fills_parallel(
    ws_receiver: &Receiver<UserEvent>,
    http_client: &reqwest::blocking::Client,
    auth: &ClobAuth,
    asset_ids: &[String],
    baseline_sizes: &HashMap<String, f64>,
    ws_timeout: Duration,
    data_api_poll_interval: Duration,
    data_api_timeout: Duration,
) -> FillConfirmationResult {
    let asset_ids_ws = asset_ids.to_vec();
    let asset_ids_api = asset_ids.to_vec();
    let baseline_api = baseline_sizes.clone();

    // Clone what we need for the Data API thread
    let http_clone = http_client.clone();
    let auth_clone = auth.clone();

    // Shared completion flag
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done_ws = done.clone();
    let done_api = done.clone();

    // Clone the receiver for the WS thread (crossbeam receivers are Clone)
    let ws_rx = ws_receiver.clone();

    // Run both in parallel threads
    let ws_handle = std::thread::spawn(move || {
        let fills = wait_for_matched_fills(&ws_rx, &asset_ids_ws, ws_timeout);
        if !fills.is_empty() {
            done_ws.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        fills
    });

    let api_handle = std::thread::spawn(move || {
        // Small initial delay — give WS a head start since it's typically faster
        std::thread::sleep(Duration::from_secs(2));
        if done_api.load(std::sync::atomic::Ordering::SeqCst) {
            return Vec::new(); // WS already won
        }
        let fills = poll_data_api_for_fills(
            &http_clone, &auth_clone, &asset_ids_api,
            &baseline_api, data_api_poll_interval, data_api_timeout,
        );
        if !fills.is_empty() {
            done_api.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        fills
    });

    let ws_fills = ws_handle.join().unwrap_or_else(|_| {
        tracing::error!("B4.5: WS fill confirmation thread panicked");
        Vec::new()
    });
    let api_fills = api_handle.join().unwrap_or_else(|_| {
        tracing::error!("B4.5: Data API fill confirmation thread panicked");
        Vec::new()
    });

    // Merge results: WS fills take priority (they have trade_id for correlation)
    let mut fills: Vec<ConfirmedFill> = Vec::new();
    let mut ws_covered: std::collections::HashSet<String> = std::collections::HashSet::new();

    for trade in &ws_fills {
        // BUG-2: Validate price/size before accepting WS fills
        if trade.price <= 0.0 || trade.size <= 0.0 {
            tracing::warn!("B4.5: Rejecting invalid WS fill: asset={} price={} size={}",
                trade.asset_id, trade.price, trade.size);
            continue;
        }
        ws_covered.insert(trade.asset_id.clone());
        fills.push(ConfirmedFill {
            asset_id: trade.asset_id.clone(),
            shares: trade.size,
            price: trade.price,
            trade_id: Some(trade.id.clone()),
            source: FillSource::WebSocket,
        });
    }

    // Add Data API fills only for assets WS didn't catch
    for fill in api_fills {
        if !ws_covered.contains(&fill.asset_id) {
            fills.push(fill);
        }
    }

    let primary = if ws_fills.len() >= fills.len() {
        FillSource::WebSocket
    } else {
        FillSource::DataApi
    };

    tracing::info!("B4.5: {} fills confirmed ({} WS, {} Data API only)",
        fills.len(), ws_covered.len(), fills.len() - ws_covered.len());

    FillConfirmationResult {
        fills,
        ws_events: ws_fills,
        primary_source: primary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;

    #[test]
    fn test_ws_matched_returns_immediately() {
        let (tx, rx) = bounded::<UserEvent>(10);

        // Send a MATCHED event
        tx.send(UserEvent::Trade(TradeEvent {
            id: "trade-001".into(),
            taker_order_id: "order-001".into(),
            market: "mkt-1".into(),
            asset_id: "asset-A".into(),
            outcome: "YES".into(),
            side: "BUY".into(),
            size: 10.0,
            price: 0.50,
            status: "MATCHED".into(),
            timestamp: 1234567890.0,
        })).unwrap();

        let fills = wait_for_matched_fills(&rx, &["asset-A".into()], Duration::from_secs(5));
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].asset_id, "asset-A");
        assert_eq!(fills[0].status, "MATCHED");
    }

    #[test]
    fn test_ws_timeout_no_fills() {
        let (_tx, rx) = bounded::<UserEvent>(10);
        let fills = wait_for_matched_fills(&rx, &["asset-X".into()], Duration::from_millis(100));
        assert!(fills.is_empty());
    }

    #[test]
    fn test_ws_dedup_by_trade_id() {
        let (tx, rx) = bounded::<UserEvent>(10);

        let event = TradeEvent {
            id: "trade-001".into(),
            taker_order_id: "order-001".into(),
            market: "mkt-1".into(),
            asset_id: "asset-A".into(),
            outcome: "YES".into(),
            side: "BUY".into(),
            size: 10.0,
            price: 0.50,
            status: "MATCHED".into(),
            timestamp: 1234567890.0,
        };

        // Send same event twice
        tx.send(UserEvent::Trade(event.clone())).unwrap();
        tx.send(UserEvent::Trade(event)).unwrap();

        let fills = wait_for_matched_fills(&rx, &["asset-A".into()], Duration::from_secs(1));
        assert_eq!(fills.len(), 1); // Only one fill despite two events
    }

    #[test]
    fn test_multiple_assets_all_matched() {
        let (tx, rx) = bounded::<UserEvent>(10);

        for (i, asset) in ["asset-A", "asset-B", "asset-C"].iter().enumerate() {
            tx.send(UserEvent::Trade(TradeEvent {
                id: format!("trade-{}", i),
                taker_order_id: format!("order-{}", i),
                market: "mkt-1".into(),
                asset_id: asset.to_string(),
                outcome: "YES".into(),
                side: "BUY".into(),
                size: 10.0,
                price: 0.50,
                status: "MATCHED".into(),
                timestamp: 1234567890.0,
            })).unwrap();
        }

        let assets = vec!["asset-A".into(), "asset-B".into(), "asset-C".into()];
        let fills = wait_for_matched_fills(&rx, &assets, Duration::from_secs(5));
        assert_eq!(fills.len(), 3);
    }
}
