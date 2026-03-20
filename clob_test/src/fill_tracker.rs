/// Fill tracker: confirms order fills via WS User Channel and enters
/// positions in the engine's PositionManager.
///
/// Flow:
/// 1. Start WS User Channel subscription for the relevant markets
/// 2. Wait for CONFIRMED trade events matching our orders
/// 3. Call engine.enter_position() with fill data
/// 4. Return the engine-assigned position IDs

use std::collections::HashMap;
use std::time::Duration;

use rust_engine::TradingEngine;
use rust_engine::position::EntryResult;
use rust_engine::signing::ClobAuth;
use rust_engine::ws_user::{UserChannelClient, TradeEvent};

use crate::clob_client::{ClobClient, TestMarket};

/// Represents a leg that was submitted to the CLOB and needs fill confirmation.
pub struct SubmittedLeg {
    pub market: TestMarket,
    pub size_usd: f64,
}

/// Confirm fills via WS User Channel and enter positions in the engine.
///
/// Returns the engine position_id on success, or an error description.
pub fn confirm_and_enter(
    engine: &TradingEngine,
    _clob: &ClobClient,
    auth: &ClobAuth,
    position_id: &str,
    legs: &[SubmittedLeg],
    is_sell: bool,
    runtime: &tokio::runtime::Handle,
    _wallet_address: &str,
) -> Result<String, String> {
    let market_ids: Vec<String> = legs.iter().map(|l| l.market.market_id.clone()).collect();
    let asset_ids: Vec<String> = legs.iter().map(|l| {
        if is_sell { l.market.no_token_id.clone() } else { l.market.yes_token_id.clone() }
    }).collect();

    // Start WS user channel
    let user_ws = UserChannelClient::new();
    user_ws.start(auth, market_ids.clone(), runtime);

    // Wait for confirmed fills (60s timeout)
    tracing::info!("[FillTracker] Waiting for fills on {} assets (timeout=60s)...", asset_ids.len());
    let fills = user_ws.wait_for_confirmed_fills(&asset_ids, Duration::from_secs(60));
    user_ws.stop();

    if fills.is_empty() {
        // No fallback — WS User Channel is indispensable for production.
        // If it doesn't work, we need to fix it, not paper over it.
        return Err("WS User Channel: no confirmed fills within 60s timeout. \
            Check auth (raw secret vs HMAC), connection stability, and market subscriptions.".into());
    }

    tracing::info!("[FillTracker] Got {} confirmed fills", fills.len());

    // Build enter_position params from fills
    enter_position_from_fills(engine, position_id, legs, &fills, is_sell)
}

/// Enter a position in the engine from confirmed fill data.
fn enter_position_from_fills(
    engine: &TradingEngine,
    position_id: &str,
    legs: &[SubmittedLeg],
    fills: &[TradeEvent],
    is_sell: bool,
) -> Result<String, String> {
    let market_ids: Vec<String> = legs.iter().map(|l| l.market.market_id.clone()).collect();
    let market_names: Vec<String> = legs.iter().map(|l| l.market.question.clone()).collect();

    // Build price maps from fills or original order prices
    let mut current_prices = HashMap::new();
    let mut current_no_prices = HashMap::new();
    let mut optimal_bets = HashMap::new();

    for leg in legs {
        let mid = &leg.market.market_id;
        // Try to find matching fill for this leg
        let fill_price = fills.iter()
            .find(|f| f.market == *mid || f.asset_id == leg.market.yes_token_id || f.asset_id == leg.market.no_token_id)
            .map(|f| f.price)
            .filter(|p| *p > 0.0)
            .unwrap_or(leg.market.best_ask);

        current_prices.insert(mid.clone(), fill_price);
        current_no_prices.insert(mid.clone(), 1.0 - fill_price);
        optimal_bets.insert(mid.clone(), leg.size_usd);
    }

    let total_cost: f64 = optimal_bets.values().sum();
    let expected_profit = -total_cost * 0.01; // Conservative: assume small loss for test trades

    let result = engine.enter_position(
        position_id,
        "test_harness",       // constraint_id
        "test",               // strategy
        "forced_buy",         // method
        &market_ids,
        &market_names,
        &current_prices,
        &current_no_prices,
        &optimal_bets,
        expected_profit,
        expected_profit / total_cost.max(0.01),
        is_sell,
        None,  // no chain info
    );

    match result {
        EntryResult::Entered(pos) => {
            let pid = pos.position_id.clone();
            tracing::info!("[FillTracker] Position entered: {} ({} markets, ${:.2} capital)",
                pid, market_ids.len(), pos.total_capital);

            // Set token_ids on the position's MarketLegs for D6 helper to use
            {
                let mut pm = engine.positions.lock();
                if let Some(position) = pm.open_positions_mut().get_mut(&pid) {
                    for leg in legs {
                        if let Some(ml) = position.markets.get_mut(&leg.market.market_id) {
                            ml.token_id = if is_sell {
                                leg.market.no_token_id.clone()
                            } else {
                                leg.market.yes_token_id.clone()
                            };
                        }
                    }
                }
            }

            Ok(pid)
        }
        EntryResult::InsufficientCapital { available, required } => {
            Err(format!("Insufficient capital: need ${:.2}, have ${:.2}", required, available))
        }
    }
}
