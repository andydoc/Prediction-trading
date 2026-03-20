/// CLOB REST API client for the test harness.
///
/// Queries the Gamma API and CLOB API directly, without requiring
/// WebSocket connections or the full engine pipeline.

use reqwest::blocking::Client;
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::time::Duration;

/// Gamma API returns clobTokenIds as a stringified JSON array.
fn deserialize_string_array<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where D: Deserializer<'de> {
    let s: Option<String> = Option::deserialize(deserializer)?;
    match s {
        Some(ref val) if val.starts_with('[') => {
            serde_json::from_str(val).map_err(serde::de::Error::custom)
        }
        _ => Ok(Vec::new()),
    }
}

/// A market suitable for testing, discovered via REST API.
#[derive(Debug, Clone)]
pub struct TestMarket {
    pub market_id: String,         // condition_id
    pub question: String,
    pub yes_token_id: String,
    pub no_token_id: String,
    pub best_ask: f64,
    pub best_bid: f64,
    pub neg_risk: bool,
    pub active: bool,
    pub volume: f64,
    pub tick_size: f64,
}

#[derive(Debug, Deserialize)]
struct GammaMarket {
    #[serde(rename = "conditionId", default)]
    condition_id: String,
    question: Option<String>,
    /// Token IDs as stringified JSON array: "[\"yes_id\", \"no_id\"]"
    #[serde(rename = "clobTokenIds", default, deserialize_with = "deserialize_string_array")]
    clob_token_ids: Vec<String>,
    #[serde(rename = "enableOrderBook", default)]
    enable_order_book: bool,
    #[serde(rename = "negRisk", default)]
    neg_risk: bool,
    #[serde(default)]
    active: bool,
    #[serde(rename = "volumeNum", default)]
    volume_num: f64,
    #[serde(rename = "bestAsk", default)]
    best_ask: f64,
    #[serde(rename = "bestBid", default)]
    best_bid: f64,
    #[serde(rename = "orderPriceMinTickSize", default = "default_tick")]
    tick_size: f64,
}

fn default_tick() -> f64 { 0.01 }

#[derive(Debug, Deserialize)]
struct ClobBook {
    asks: Option<Vec<ClobOrder>>,
    bids: Option<Vec<ClobOrder>>,
}

#[derive(Debug, Clone, Deserialize)]
struct ClobOrder {
    price: Option<String>,
    size: Option<String>,
}

pub struct ClobClient {
    http: Client,
    clob_host: String,
}

impl ClobClient {
    pub fn new(clob_host: &str) -> Self {
        Self {
            http: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap(),
            clob_host: clob_host.trim_end_matches('/').to_string(),
        }
    }

    /// Find a liquid non-negRisk market suitable for D2/D3.
    pub fn find_liquid_market(&self) -> Option<TestMarket> {
        self.find_market(false)
    }

    /// Find a liquid negRisk market suitable for D4.
    pub fn find_neg_risk_market(&self) -> Option<TestMarket> {
        self.find_market(true)
    }

    /// Find a 2-outcome market for D5 forced arb (buy YES on both sides).
    pub fn find_two_outcome_market(&self) -> Option<(TestMarket, TestMarket)> {
        // Search Gamma for events with exactly 2 markets
        let url = "https://gamma-api.polymarket.com/markets?limit=200&active=true&enableOrderBook=true&closed=false";
        let resp = self.http.get(url).send().ok()?;
        let markets: Vec<GammaMarket> = resp.json().ok()?;

        // Group by condition_id prefix to find mutex pairs
        // Actually, negRisk events with exactly 2 outcomes work best
        // Let's find two markets from the same event
        let mut by_group: HashMap<String, Vec<&GammaMarket>> = HashMap::new();
        for m in &markets {
            if m.condition_id.is_empty() || !m.enable_order_book || !m.active { continue; }
            if m.clob_token_ids.len() < 2 { continue; }
            if m.neg_risk {
                let key = "neg_risk_pool".to_string();
                by_group.entry(key).or_default().push(m);
            }
        }

        // Take first two negRisk markets with books
        if let Some(pool) = by_group.get("neg_risk_pool") {
            let mut found = Vec::new();
            for m in pool.iter().take(20) {
                if let Some(tm) = self.enrich_market(m) {
                    if tm.best_ask > 0.10 && tm.best_ask < 0.90 {
                        found.push(tm);
                        if found.len() == 2 { break; }
                    }
                }
            }
            if found.len() == 2 {
                return Some((found.remove(0), found.remove(0)));
            }
        }

        // Fallback: any two markets with active books
        let mut found = Vec::new();
        for m in &markets {
            if m.condition_id.is_empty() || !m.enable_order_book || !m.active || m.clob_token_ids.len() < 2 { continue; }
            if let Some(tm) = self.enrich_market(m) {
                if tm.best_ask > 0.10 && tm.best_ask < 0.90 {
                    found.push(tm);
                    if found.len() == 2 { break; }
                }
            }
        }
        if found.len() == 2 {
            Some((found.remove(0), found.remove(0)))
        } else {
            None
        }
    }

    fn find_market(&self, want_neg_risk: bool) -> Option<TestMarket> {
        // Query Gamma API for active markets with order books
        let url = format!(
            "https://gamma-api.polymarket.com/markets?limit=100&active=true&enableOrderBook=true&closed=false&negRisk={}",
            want_neg_risk
        );
        let resp = match self.http.get(&url).send() {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Gamma API request failed: {}", e);
                return None;
            }
        };
        let body = match resp.text() {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("Gamma API response body error: {}", e);
                return None;
            }
        };
        let markets: Vec<GammaMarket> = match serde_json::from_str(&body) {
            Ok(m) => m,
            Err(e) => {
                tracing::error!("Gamma API JSON parse error: {} — first 500 chars: {}", e, &body[..body.len().min(500)]);
                return None;
            }
        };

        tracing::info!("Gamma API returned {} markets (neg_risk={})", markets.len(), want_neg_risk);

        // Sort by volume descending for liquidity
        let mut sorted: Vec<&GammaMarket> = markets.iter()
            .filter(|m| !m.condition_id.is_empty() && m.clob_token_ids.len() >= 2)
            .collect();
        sorted.sort_by(|a, b| b.volume_num.partial_cmp(&a.volume_num).unwrap_or(std::cmp::Ordering::Equal));

        // Try top markets by volume, check for actual book depth
        for m in sorted.iter().take(30) {
            if let Some(tm) = self.enrich_market(m) {
                if tm.best_ask > 0.05 && tm.best_ask < 0.95 && tm.best_bid > 0.0 {
                    tracing::info!("Found market: {} ask={:.4} bid={:.4} vol={:.0}",
                        tm.question, tm.best_ask, tm.best_bid, tm.volume);
                    return Some(tm);
                }
            }
        }

        None
    }

    /// Convert a Gamma market to TestMarket using top-level bestAsk/bestBid.
    /// Falls back to CLOB /book query if Gamma prices are missing.
    fn enrich_market(&self, m: &GammaMarket) -> Option<TestMarket> {
        if m.clob_token_ids.len() < 2 { return None; }
        let yes_token = m.clob_token_ids[0].clone();
        let no_token = m.clob_token_ids[1].clone();

        // Use Gamma's top-level bestAsk/bestBid if available
        let (best_ask, best_bid) = if m.best_ask > 0.0 && m.best_bid > 0.0 {
            (m.best_ask, m.best_bid)
        } else {
            // Fallback: query CLOB for order book
            let book_url = format!("{}/book?token_id={}", self.clob_host, yes_token);
            let resp = self.http.get(&book_url).send().ok()?;
            if !resp.status().is_success() { return None; }
            let book: ClobBook = resp.json().ok()?;

            let ask = book.asks.as_ref()
                .and_then(|a| a.first())
                .and_then(|o| o.price.as_ref())
                .and_then(|p| p.parse::<f64>().ok())
                .unwrap_or(0.0);
            let bid = book.bids.as_ref()
                .and_then(|b| b.first())
                .and_then(|o| o.price.as_ref())
                .and_then(|p| p.parse::<f64>().ok())
                .unwrap_or(0.0);
            (ask, bid)
        };

        Some(TestMarket {
            market_id: m.condition_id.clone(),
            question: m.question.clone().unwrap_or_default(),
            yes_token_id: yes_token,
            no_token_id: no_token,
            best_ask,
            best_bid,
            neg_risk: m.neg_risk,
            active: m.active,
            volume: m.volume_num,
            tick_size: if m.tick_size > 0.0 { m.tick_size } else { 0.01 },
        })
    }

    /// Get current best ask for a token.
    pub fn get_best_ask(&self, token_id: &str) -> f64 {
        let url = format!("{}/book?token_id={}", self.clob_host, token_id);
        self.http.get(&url).send().ok()
            .and_then(|r| r.json::<ClobBook>().ok())
            .and_then(|b| b.asks)
            .and_then(|a| a.first().cloned())
            .and_then(|o| o.price)
            .and_then(|p| p.parse::<f64>().ok())
            .unwrap_or(0.0)
    }

    /// Register a TestMarket's instruments in the engine's InstrumentStore.
    /// Must be called before executing orders on this market.
    pub fn register_instrument(&self, market: &TestMarket, engine: &rust_engine::TradingEngine) {
        use rust_engine::instrument::{Instrument, RoundingConfig};

        let yes_inst = Instrument {
            market_id: market.market_id.clone(),
            token_id: market.yes_token_id.clone(),
            outcome: "yes".to_string(),
            condition_id: market.market_id.clone(),
            neg_risk: market.neg_risk,
            tick_size: market.tick_size,
            rounding: RoundingConfig::from_tick_size_f64(market.tick_size),
            min_order_size: 1.0,
            max_order_size: 0.0,
            order_book_enabled: true,
            accepting_orders: true,
        };
        engine.instruments.insert_instrument(yes_inst);

        if !market.no_token_id.is_empty() {
            let no_inst = Instrument {
                market_id: market.market_id.clone(),
                token_id: market.no_token_id.clone(),
                outcome: "no".to_string(),
                condition_id: market.market_id.clone(),
                neg_risk: market.neg_risk,
                tick_size: 0.01,
                rounding: RoundingConfig::from_tick_size_f64(0.01),
                min_order_size: 1.0,
                max_order_size: 0.0,
                order_book_enabled: true,
                accepting_orders: true,
            };
            engine.instruments.insert_instrument(no_inst);
        }

        tracing::info!("Registered instrument: {} (neg_risk={})", market.question, market.neg_risk);
    }

    /// Get current best bid for a token.
    pub fn get_best_bid(&self, token_id: &str) -> f64 {
        let url = format!("{}/book?token_id={}", self.clob_host, token_id);
        self.http.get(&url).send().ok()
            .and_then(|r| r.json::<ClobBook>().ok())
            .and_then(|b| b.bids)
            .and_then(|b| b.first().cloned())
            .and_then(|o| o.price)
            .and_then(|p| p.parse::<f64>().ok())
            .unwrap_or(0.0)
    }
}
