/// Formal instrument model for Polymarket conditional tokens (B2.3).
///
/// Encodes the py-clob-client `ROUNDING_CONFIG` rules that map tick_size to
/// decimal precision for price, size, and amount. Also tracks token IDs,
/// neg_risk flag, and condition IDs needed for order construction.
///
/// Instruments are loaded from MarketScanner data at startup and can be
/// updated dynamically when tick_size_change events arrive via Tier C WS.

use std::collections::HashMap;
use std::sync::Arc;
use parking_lot::RwLock;

// ---------------------------------------------------------------------------
// Rounding config (matches py-clob-client ROUNDING_CONFIG)
// ---------------------------------------------------------------------------

/// Decimal precision rules derived from tick_size.
///
/// py-clob-client ROUNDING_CONFIG:
///   "0.1"    → price: 1, size: 2, amount: 3
///   "0.01"   → price: 2, size: 2, amount: 4
///   "0.001"  → price: 3, size: 2, amount: 5
///   "0.0001" → price: 4, size: 2, amount: 6
#[derive(Debug, Clone, Copy)]
pub struct RoundingConfig {
    /// Decimal places for price (e.g., 2 for $0.01 precision).
    pub price_decimals: u32,
    /// Decimal places for size/quantity (always 2 in current config).
    pub size_decimals: u32,
    /// Decimal places for total amount (price × size).
    pub amount_decimals: u32,
}

impl RoundingConfig {
    /// Derive rounding config from tick_size string (e.g., "0.01").
    pub fn from_tick_size(tick_size: &str) -> Self {
        match tick_size {
            "0.1" => Self { price_decimals: 1, size_decimals: 2, amount_decimals: 3 },
            "0.01" => Self { price_decimals: 2, size_decimals: 2, amount_decimals: 4 },
            "0.001" => Self { price_decimals: 3, size_decimals: 2, amount_decimals: 5 },
            "0.0001" => Self { price_decimals: 4, size_decimals: 2, amount_decimals: 6 },
            _ => {
                tracing::warn!("Unknown tick_size '{}', defaulting to 0.01 rounding", tick_size);
                Self { price_decimals: 2, size_decimals: 2, amount_decimals: 4 }
            }
        }
    }

    /// Derive from tick_size as f64.
    pub fn from_tick_size_f64(tick_size: f64) -> Self {
        if (tick_size - 0.1).abs() < 1e-6 {
            Self::from_tick_size("0.1")
        } else if (tick_size - 0.001).abs() < 1e-6 {
            Self::from_tick_size("0.001")
        } else if (tick_size - 0.0001).abs() < 1e-6 {
            Self::from_tick_size("0.0001")
        } else {
            // Default: 0.01 (most common)
            Self::from_tick_size("0.01")
        }
    }

    /// Round a price to the configured precision.
    pub fn round_price(&self, price: f64) -> f64 {
        let scale = 10f64.powi(self.price_decimals as i32);
        (price * scale).round() / scale
    }

    /// Round a size/quantity to the configured precision.
    pub fn round_size(&self, size: f64) -> f64 {
        let scale = 10f64.powi(self.size_decimals as i32);
        (size * scale).round() / scale
    }

    /// Round an amount (price × size) to the configured precision.
    pub fn round_amount(&self, amount: f64) -> f64 {
        let scale = 10f64.powi(self.amount_decimals as i32);
        (amount * scale).round() / scale
    }
}

// ---------------------------------------------------------------------------
// Instrument
// ---------------------------------------------------------------------------

/// A tradeable instrument on Polymarket.
///
/// Each market has two instruments: YES token and NO token.
/// The instrument holds everything needed to construct and validate orders.
#[derive(Debug, Clone)]
pub struct Instrument {
    /// The market ID from Polymarket (condition_id-based).
    pub market_id: String,
    /// ERC1155 token ID for this outcome (YES or NO).
    pub token_id: String,
    /// "yes" or "no".
    pub outcome: String,
    /// Condition ID (used in CLOB order conditions).
    pub condition_id: String,
    /// Whether this is a negRisk market (affects exchange routing + taker address).
    pub neg_risk: bool,
    /// Current tick size (e.g., 0.01).
    pub tick_size: f64,
    /// Rounding/precision config derived from tick_size.
    pub rounding: RoundingConfig,
    /// Minimum order size in USDC (Polymarket default: $1).
    pub min_order_size: f64,
    /// Maximum order size in USDC (0 = no limit).
    pub max_order_size: f64,
    /// Whether the order book is enabled.
    pub order_book_enabled: bool,
    /// Whether the market is accepting orders.
    pub accepting_orders: bool,
}

impl Instrument {
    /// Update tick size and recalculate rounding config.
    /// Called when a tick_size_change event is received via WS.
    pub fn update_tick_size(&mut self, new_tick_size: f64) {
        tracing::info!(
            "Tick size change: {} ({}) {} → {}",
            self.market_id, self.outcome, self.tick_size, new_tick_size
        );
        self.tick_size = new_tick_size;
        self.rounding = RoundingConfig::from_tick_size_f64(new_tick_size);
    }

    /// Validate a price against this instrument's tick size.
    pub fn validate_price(&self, price: f64) -> bool {
        let rounded = self.rounding.round_price(price);
        (price - rounded).abs() < 1e-10
    }
}

// ---------------------------------------------------------------------------
// Instrument store
// ---------------------------------------------------------------------------

/// Thread-safe store of all known instruments, keyed by token_id.
///
/// Loaded from scanner data at startup. Updated on tick_size_change events.
pub struct InstrumentStore {
    /// token_id → Instrument
    instruments: Arc<RwLock<HashMap<String, Instrument>>>,
}

impl InstrumentStore {
    pub fn new() -> Self {
        Self {
            instruments: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Load instruments from scanner market data.
    ///
    /// Each market produces two instruments (YES and NO tokens).
    /// `markets` is the market_lookup from the orchestrator.
    pub fn load_from_markets(&self, markets: &HashMap<String, serde_json::Value>) {
        let mut store = self.instruments.write();
        let mut count = 0;

        for (_market_id, data) in markets {
            let market_id = data.get("market_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let metadata = data.get("metadata").cloned().unwrap_or_default();
            let condition_id = metadata.get("conditionId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let neg_risk = metadata.get("negRisk")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let order_book_enabled = metadata.get("enableOrderBook")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let accepting_orders = metadata.get("acceptingOrders")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            // Tick size: from metadata, default 0.01
            let tick_size = metadata.get("tick_size")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.01);

            let yes_token = data.get("yes_asset_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let no_token = data.get("no_asset_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            if !yes_token.is_empty() {
                store.insert(yes_token.clone(), Instrument {
                    market_id: market_id.clone(),
                    token_id: yes_token,
                    outcome: "yes".into(),
                    condition_id: condition_id.clone(),
                    neg_risk,
                    tick_size,
                    rounding: RoundingConfig::from_tick_size_f64(tick_size),
                    min_order_size: 1.0,
                    max_order_size: 0.0,
                    order_book_enabled,
                    accepting_orders,
                });
                count += 1;
            }

            if !no_token.is_empty() {
                store.insert(no_token.clone(), Instrument {
                    market_id,
                    token_id: no_token,
                    outcome: "no".into(),
                    condition_id,
                    neg_risk,
                    tick_size,
                    rounding: RoundingConfig::from_tick_size_f64(tick_size),
                    min_order_size: 1.0,
                    max_order_size: 0.0,
                    order_book_enabled,
                    accepting_orders,
                });
                count += 1;
            }
        }

        tracing::info!("InstrumentStore loaded {} instruments from {} markets", count, markets.len());
    }

    /// Insert a single instrument by token_id.
    pub fn insert_instrument(&self, inst: Instrument) {
        self.instruments.write().insert(inst.token_id.clone(), inst);
    }

    /// Get an instrument by token_id.
    pub fn get(&self, token_id: &str) -> Option<Instrument> {
        self.instruments.read().get(token_id).cloned()
    }

    /// Update tick size for a specific token_id (from WS tick_size_change event).
    /// Returns true if the instrument was found and updated.
    pub fn update_tick_size(&self, token_id: &str, new_tick_size: f64) -> bool {
        if let Some(inst) = self.instruments.write().get_mut(token_id) {
            inst.update_tick_size(new_tick_size);
            true
        } else {
            tracing::warn!("Tick size change for unknown token_id: {}", token_id);
            false
        }
    }

    /// Number of instruments in the store.
    pub fn len(&self) -> usize {
        self.instruments.read().len()
    }

    /// Get all instruments for a given market_id.
    pub fn by_market(&self, market_id: &str) -> Vec<Instrument> {
        self.instruments.read().values()
            .filter(|i| i.market_id == market_id)
            .cloned()
            .collect()
    }

    /// Persist all instruments to SQLite via StateDB.
    pub fn save_to_db(&self, db: &crate::state::StateDB) {
        let store = self.instruments.read();
        let rows: Vec<_> = store.values().map(|i| (
            i.token_id.clone(),
            i.market_id.clone(),
            i.outcome.clone(),
            i.condition_id.clone(),
            i.neg_risk,
            i.tick_size,
            i.min_order_size,
            i.max_order_size,
            i.order_book_enabled,
            i.accepting_orders,
        )).collect();
        db.save_instruments_bulk(&rows);
        tracing::info!("InstrumentStore persisted {} instruments to SQLite", rows.len());
    }

    /// Load instruments from SQLite via StateDB (startup recovery).
    /// Only loads if the store is currently empty (scanner hasn't run yet).
    pub fn load_from_db(&self, db: &crate::state::StateDB) {
        if self.len() > 0 {
            tracing::debug!("InstrumentStore already has {} instruments, skipping SQLite load", self.len());
            return;
        }
        let rows = db.load_instruments();
        if rows.is_empty() {
            return;
        }
        let mut store = self.instruments.write();
        for (token_id, market_id, outcome, condition_id, neg_risk, tick_size,
             min_order_size, max_order_size, order_book_enabled, accepting_orders) in rows
        {
            store.insert(token_id.clone(), Instrument {
                market_id,
                token_id,
                outcome,
                condition_id,
                neg_risk,
                tick_size,
                rounding: RoundingConfig::from_tick_size_f64(tick_size),
                min_order_size,
                max_order_size,
                order_book_enabled,
                accepting_orders,
            });
        }
        tracing::info!("InstrumentStore loaded {} instruments from SQLite", store.len());
    }
}

impl Default for InstrumentStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rounding_config_from_tick_size() {
        let r = RoundingConfig::from_tick_size("0.01");
        assert_eq!(r.price_decimals, 2);
        assert_eq!(r.size_decimals, 2);
        assert_eq!(r.amount_decimals, 4);

        let r = RoundingConfig::from_tick_size("0.001");
        assert_eq!(r.price_decimals, 3);
        assert_eq!(r.amount_decimals, 5);
    }

    #[test]
    fn test_round_price() {
        let r = RoundingConfig::from_tick_size("0.01");
        assert_eq!(r.round_price(0.505), 0.51);
        assert_eq!(r.round_price(0.504), 0.50);

        let r = RoundingConfig::from_tick_size("0.1");
        assert_eq!(r.round_price(0.55), 0.6);
        assert_eq!(r.round_price(0.54), 0.5);
    }

    #[test]
    fn test_validate_price() {
        let inst = Instrument {
            market_id: "test".into(),
            token_id: "123".into(),
            outcome: "yes".into(),
            condition_id: "cond".into(),
            neg_risk: false,
            tick_size: 0.01,
            rounding: RoundingConfig::from_tick_size("0.01"),
            min_order_size: 1.0,
            max_order_size: 0.0,
            order_book_enabled: true,
            accepting_orders: true,
        };
        assert!(inst.validate_price(0.50));
        assert!(inst.validate_price(0.01));
        assert!(!inst.validate_price(0.505));
    }

    #[test]
    fn test_instrument_store() {
        let store = InstrumentStore::new();
        let mut markets = HashMap::new();
        markets.insert("mkt1".to_string(), serde_json::json!({
            "market_id": "mkt1",
            "yes_asset_id": "token_yes_1",
            "no_asset_id": "token_no_1",
            "metadata": {
                "conditionId": "cond1",
                "negRisk": false,
                "enableOrderBook": true,
                "acceptingOrders": true,
                "tick_size": 0.01
            }
        }));

        store.load_from_markets(&markets);
        assert_eq!(store.len(), 2);

        let yes = store.get("token_yes_1").unwrap();
        assert_eq!(yes.outcome, "yes");
        assert_eq!(yes.condition_id, "cond1");
        assert!(!yes.neg_risk);
        assert_eq!(yes.rounding.price_decimals, 2);

        // Update tick size
        assert!(store.update_tick_size("token_yes_1", 0.001));
        let updated = store.get("token_yes_1").unwrap();
        assert_eq!(updated.rounding.price_decimals, 3);
    }
}
