/// Double-entry accounting ledger for the trading engine.
///
/// Every cash movement creates balanced journal entries (debits = credits).
/// This is an independent, parallel record that can be reconciled against
/// PositionManager and exchange state at any time.
///
/// Chart of accounts:
///   Cash           — USDC.e available for trading
///   Position:{pid} — Capital deployed in a specific position
///   Fees           — Cumulative trading fees paid (expense)
///   RealizedPnL    — Cumulative realized profit/loss
///   Equity         — Opening equity (balancing entry)

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// A single journal entry in the double-entry ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub id: u64,
    pub timestamp: f64,
    pub account: String,
    pub debit: f64,
    pub credit: f64,
    pub position_id: Option<String>,
    pub description: String,
}

/// Holding of a specific asset (shares of a token).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetHolding {
    pub asset_id: String,
    pub market_id: String,
    pub shares: f64,
    pub cost_basis: f64, // Total USDC spent acquiring these shares
}

/// Result of reconciliation between accounting, engine, and exchange.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationResult {
    pub timestamp: String,
    pub accounting_cash: f64,
    pub engine_cash: f64,
    pub accounting_positions: usize,
    pub engine_positions: usize,
    pub exchange_positions: usize,
    pub accounting_nav: f64,
    pub exchange_value: f64,
    pub engine_value: f64,
    pub accounting_fees: f64,
    pub cash_match: bool,
    pub position_count_match: bool,
    pub value_match: bool,
    pub tolerance: f64,
    pub mismatches: Vec<String>,
    pub overall_pass: bool,
}

/// The double-entry accounting ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountingLedger {
    entries: Vec<JournalEntry>,
    next_id: u64,

    // Running balances (derived from journal entries, kept in sync)
    cash: f64,
    positions_deployed: f64,
    fees_total: f64,
    realized_pnl: f64,

    // Asset-level tracking: asset_id -> holding
    holdings: HashMap<String, AssetHolding>,

    // Dedup: trade IDs already accounted for (prevents double-counting
    // when a fill is recorded by both active process and WS event)
    recorded_trade_ids: HashSet<String>,

    // Gas tracking
    opening_pol: f64,
    closing_pol: f64,

    // Config
    taker_fee_rate: f64,
}

impl AccountingLedger {
    /// Create a new ledger with opening cash balance.
    pub fn new(opening_cash: f64, taker_fee_rate: f64) -> Self {
        let mut ledger = Self {
            entries: Vec::new(),
            next_id: 1,
            cash: 0.0,
            positions_deployed: 0.0,
            fees_total: 0.0,
            realized_pnl: 0.0,
            holdings: HashMap::new(),
            recorded_trade_ids: HashSet::new(),
            opening_pol: 0.0,
            closing_pol: 0.0,
            taker_fee_rate,
        };

        // Opening balance: debit Cash, credit Equity
        let now = now_secs();
        ledger.add_entry(now, "Cash", opening_cash, 0.0, None,
            &format!("Opening balance: ${:.2}", opening_cash));
        ledger.add_entry(now, "Equity", 0.0, opening_cash, None,
            &format!("Opening equity: ${:.2}", opening_cash));
        ledger.cash = opening_cash;

        ledger
    }

    /// Check if a trade ID has already been recorded (dedup guard).
    pub fn is_trade_recorded(&self, trade_id: &str) -> bool {
        self.recorded_trade_ids.contains(trade_id)
    }

    /// Mark a trade ID as recorded.
    pub fn mark_trade_recorded(&mut self, trade_id: &str) {
        self.recorded_trade_ids.insert(trade_id.to_string());
    }

    /// Record a BUY with dedup check. Returns false if already recorded.
    pub fn record_buy_dedup(
        &mut self,
        trade_id: &str,
        position_id: &str,
        capital: f64,
        fees: f64,
        asset_id: &str,
        market_id: &str,
        shares: f64,
        price: f64,
        description: &str,
    ) -> bool {
        if self.recorded_trade_ids.contains(trade_id) {
            tracing::debug!("[ACCT] Dedup: trade {} already recorded, skipping", trade_id);
            return false;
        }
        self.recorded_trade_ids.insert(trade_id.to_string());
        self.record_buy(position_id, capital, fees, asset_id, market_id, shares, price, description);
        true
    }

    /// Record a SELL with dedup check. Returns false if already recorded.
    pub fn record_sell_dedup(
        &mut self,
        trade_id: &str,
        position_id: &str,
        proceeds: f64,
        cost_basis: f64,
        fees: f64,
        asset_id: &str,
        shares_sold: f64,
        price: f64,
        description: &str,
    ) -> bool {
        if self.recorded_trade_ids.contains(trade_id) {
            tracing::debug!("[ACCT] Dedup: trade {} already recorded, skipping", trade_id);
            return false;
        }
        self.recorded_trade_ids.insert(trade_id.to_string());
        self.record_sell(position_id, proceeds, cost_basis, fees, asset_id, shares_sold, price, description);
        true
    }

    /// Record a BUY: capital leaves cash, enters a position, fees deducted.
    pub fn record_buy(
        &mut self,
        position_id: &str,
        capital: f64,
        fees: f64,
        asset_id: &str,
        market_id: &str,
        shares: f64,
        price: f64,
        description: &str,
    ) {
        let now = now_secs();
        let total = capital + fees;
        let pid = Some(position_id.to_string());

        // Debit Position (capital deployed)
        self.add_entry(now, &format!("Position:{}", position_id),
            capital, 0.0, pid.clone(), description);

        // Debit Fees
        if fees > 0.0 {
            self.add_entry(now, "Fees", fees, 0.0, pid.clone(),
                &format!("Taker fee on {}", position_id));
        }

        // Credit Cash
        self.add_entry(now, "Cash", 0.0, total, pid, description);

        // Update running balances
        self.cash -= total;
        self.positions_deployed += capital;
        self.fees_total += fees;

        // Track asset holding
        let holding = self.holdings.entry(asset_id.to_string()).or_insert(AssetHolding {
            asset_id: asset_id.to_string(),
            market_id: market_id.to_string(),
            shares: 0.0,
            cost_basis: 0.0,
        });
        holding.shares += shares;
        holding.cost_basis += capital;

        tracing::info!("[ACCT] BUY {} shares @ {:.4} = ${:.4} + ${:.4} fee | cash=${:.2} deployed=${:.2}",
            shares, price, capital, fees, self.cash, self.positions_deployed);
    }

    /// Record a SELL: proceeds enter cash, position closes, PnL realized.
    ///
    /// Double-entry (all entries balance):
    ///   Debit Cash (gross proceeds)       — we receive this much
    ///   Credit Position:{pid} (cost_basis) — we give up the asset at book value
    ///   If loss: Debit RealizedPnL (cost_basis - proceeds) — loss is an expense
    ///   If gain: Credit RealizedPnL (proceeds - cost_basis) — gain is revenue
    ///   Debit Fees (fees) + Credit Cash (fees) — fee taken from proceeds
    pub fn record_sell(
        &mut self,
        position_id: &str,
        proceeds: f64,
        cost_basis: f64,
        fees: f64,
        asset_id: &str,
        shares_sold: f64,
        price: f64,
        description: &str,
    ) {
        let now = now_secs();
        let pid = Some(position_id.to_string());
        let trading_pnl = proceeds - cost_basis; // Gain/loss from the trade itself

        // 1. Debit Cash (gross proceeds received)
        self.add_entry(now, "Cash", proceeds, 0.0, pid.clone(), description);

        // 2. Credit Position (cost basis of asset we're selling)
        self.add_entry(now, &format!("Position:{}", position_id),
            0.0, cost_basis, pid.clone(), description);

        // 3. Balance the trade: PnL entry
        if trading_pnl < 0.0 {
            // Loss: we received less than cost basis → debit RealizedPnL (expense)
            self.add_entry(now, "RealizedPnL", trading_pnl.abs(), 0.0, pid.clone(), description);
        } else if trading_pnl > 0.0 {
            // Gain: we received more than cost basis → credit RealizedPnL (revenue)
            self.add_entry(now, "RealizedPnL", 0.0, trading_pnl, pid.clone(), description);
        }

        // 4. Fee: debit Fees (expense), credit Cash (cash leaves for fee)
        if fees > 0.0 {
            self.add_entry(now, "Fees", fees, 0.0, pid.clone(),
                &format!("Taker fee on sell {}", position_id));
            self.add_entry(now, "Cash", 0.0, fees, pid,
                &format!("Fee deducted from sell {}", position_id));
        }

        // Update running balances
        let net_proceeds = proceeds - fees;
        self.cash += net_proceeds;
        self.positions_deployed -= cost_basis;
        self.fees_total += fees;
        self.realized_pnl += trading_pnl; // Track trading PnL separate from fees

        // Update asset holding
        if let Some(holding) = self.holdings.get_mut(asset_id) {
            holding.shares -= shares_sold;
            holding.cost_basis -= cost_basis;
            if holding.shares <= 0.001 {
                self.holdings.remove(asset_id);
            }
        }

        tracing::info!("[ACCT] SELL {} shares @ {:.4} = ${:.4} - ${:.4} fee = ${:.4} PnL | cash=${:.2} deployed=${:.2}",
            shares_sold, price, proceeds, fees, trading_pnl, self.cash, self.positions_deployed);
    }

    // --- Queries ---

    pub fn cash_balance(&self) -> f64 { self.cash }
    pub fn total_deployed(&self) -> f64 { self.positions_deployed }
    pub fn total_fees(&self) -> f64 { self.fees_total }
    pub fn total_realized_pnl(&self) -> f64 { self.realized_pnl }
    /// Cash + deployed capital (book value, not market value).
    pub fn total_value(&self) -> f64 { self.cash + self.positions_deployed }
    pub fn position_count(&self) -> usize {
        self.holdings.values().filter(|h| h.shares > 0.001).count()
    }
    pub fn distinct_assets(&self) -> usize { self.holdings.len() }
    pub fn holdings(&self) -> &HashMap<String, AssetHolding> { &self.holdings }
    pub fn entries(&self) -> &[JournalEntry] { &self.entries }

    /// Set opening POL balance.
    pub fn set_opening_pol(&mut self, pol: f64) {
        self.opening_pol = pol;
    }

    /// Set closing POL balance.
    pub fn set_closing_pol(&mut self, pol: f64) {
        self.closing_pol = pol;
    }

    /// POL spent as gas (opening - closing).
    pub fn gas_spent(&self) -> f64 {
        (self.opening_pol - self.closing_pol).max(0.0)
    }

    /// Verify double-entry balance: sum(debits) == sum(credits).
    pub fn verify_balance(&self) -> bool {
        let total_debits: f64 = self.entries.iter().map(|e| e.debit).sum();
        let total_credits: f64 = self.entries.iter().map(|e| e.credit).sum();
        (total_debits - total_credits).abs() < 0.001
    }

    /// Compute NAV by marking holdings to market (mid-price).
    /// `price_fn` takes an asset_id and returns (best_bid, best_ask).
    pub fn compute_nav<F>(&self, price_fn: F) -> f64
    where
        F: Fn(&str) -> (f64, f64),
    {
        let mut nav = self.cash;
        for holding in self.holdings.values() {
            if holding.shares > 0.001 {
                let (bid, ask) = price_fn(&holding.asset_id);
                let mid = if bid > 0.0 && ask > 0.0 {
                    (bid + ask) / 2.0
                } else if bid > 0.0 {
                    bid
                } else {
                    ask
                };
                nav += holding.shares * mid;
            }
        }
        nav
    }

    /// Log a summary of current accounting state.
    pub fn summary_log(&self, label: &str) {
        tracing::info!("[ACCT {}] cash=${:.2} deployed=${:.2} total=${:.2} fees=${:.4} pnl=${:.4} positions={} assets={} | balanced={}",
            label, self.cash, self.positions_deployed, self.total_value(),
            self.fees_total, self.realized_pnl,
            self.position_count(), self.distinct_assets(),
            self.verify_balance());
    }

    /// Reconcile accounting state against engine and exchange.
    pub fn reconcile(
        &self,
        engine_cash: f64,
        engine_open_count: usize,
        engine_value: f64,
        exchange_positions: usize,
        exchange_value: f64,
        tolerance: f64,
    ) -> ReconciliationResult {
        let mut mismatches = Vec::new();

        let cash_match = (self.cash - engine_cash).abs() < tolerance;
        if !cash_match {
            mismatches.push(format!("Cash: accounting=${:.4} vs engine=${:.4} (delta=${:.4})",
                self.cash, engine_cash, self.cash - engine_cash));
        }

        let position_count_match = self.position_count() == engine_open_count;
        if !position_count_match {
            mismatches.push(format!("Positions: accounting={} vs engine={}",
                self.position_count(), engine_open_count));
        }

        // NAV comparison uses total_value (book) since we may not have live prices here
        let acct_value = self.total_value();
        let value_match = (acct_value - exchange_value).abs() < tolerance
            && (engine_value - exchange_value).abs() < tolerance;
        if !value_match {
            mismatches.push(format!("Value: accounting=${:.4} vs engine=${:.4} vs exchange=${:.4}",
                acct_value, engine_value, exchange_value));
        }

        if !self.verify_balance() {
            mismatches.push("CRITICAL: Double-entry balance violated (debits != credits)".into());
        }

        let overall_pass = cash_match && position_count_match && value_match && self.verify_balance();

        ReconciliationResult {
            timestamp: chrono::Utc::now().to_rfc3339(),
            accounting_cash: self.cash,
            engine_cash,
            accounting_positions: self.position_count(),
            engine_positions: engine_open_count,
            exchange_positions,
            accounting_nav: acct_value,
            exchange_value,
            engine_value,
            accounting_fees: self.fees_total,
            cash_match,
            position_count_match,
            value_match,
            tolerance,
            mismatches,
            overall_pass,
        }
    }

    /// Serialize the entire ledger to JSON string (for checkpoint persistence).
    pub fn serialize_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Deserialize from JSON string (for checkpoint restore).
    pub fn deserialize_json(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| format!("Ledger deserialize failed: {}", e))
    }

    /// Convert to JSON value for report output.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "cash": self.cash,
            "positions_deployed": self.positions_deployed,
            "total_value": self.total_value(),
            "fees_total": self.fees_total,
            "realized_pnl": self.realized_pnl,
            "position_count": self.position_count(),
            "asset_count": self.distinct_assets(),
            "opening_pol": self.opening_pol,
            "closing_pol": self.closing_pol,
            "gas_spent_pol": self.gas_spent(),
            "balanced": self.verify_balance(),
            "entry_count": self.entries.len(),
            "holdings": self.holdings.values().map(|h| {
                serde_json::json!({
                    "asset_id": h.asset_id,
                    "market_id": h.market_id,
                    "shares": h.shares,
                    "cost_basis": h.cost_basis,
                })
            }).collect::<Vec<_>>(),
            "journal": self.entries,
        })
    }

    // --- Internal ---

    fn add_entry(
        &mut self,
        timestamp: f64,
        account: &str,
        debit: f64,
        credit: f64,
        position_id: Option<String>,
        description: &str,
    ) {
        self.entries.push(JournalEntry {
            id: self.next_id,
            timestamp,
            account: account.to_string(),
            debit,
            credit,
            position_id,
            description: description.to_string(),
        });
        self.next_id += 1;
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_balance_is_balanced() {
        let ledger = AccountingLedger::new(100.0, 0.02);
        assert!(ledger.verify_balance());
        assert_eq!(ledger.cash_balance(), 100.0);
        assert_eq!(ledger.total_deployed(), 0.0);
        assert_eq!(ledger.entries().len(), 2); // Cash debit + Equity credit
    }

    #[test]
    fn buy_then_sell_round_trip() {
        let mut ledger = AccountingLedger::new(50.0, 0.02);

        // BUY: $2.50 capital + $0.05 fee = $2.55 total
        ledger.record_buy("pos1", 2.50, 0.05, "token_abc", "market_1", 13.58, 0.184, "D3 BUY");

        assert!((ledger.cash_balance() - 47.45).abs() < 0.001);
        assert!((ledger.total_deployed() - 2.50).abs() < 0.001);
        assert!((ledger.total_fees() - 0.05).abs() < 0.001);
        assert_eq!(ledger.position_count(), 1);
        assert!(ledger.verify_balance());

        // SELL: $2.40 gross proceeds, $2.50 cost basis, $0.048 fee
        // net_proceeds = 2.40 - 0.048 = 2.352
        // pnl = 2.352 - 2.50 = -0.148
        ledger.record_sell("pos1", 2.40, 2.50, 0.048, "token_abc", 13.58, 0.177, "D8 SELL");

        // cash = 47.45 + (2.40 - 0.048) = 49.802
        assert!((ledger.cash_balance() - 49.802).abs() < 0.01);
        assert!((ledger.total_deployed() - 0.0).abs() < 0.001);
        assert_eq!(ledger.position_count(), 0);
        assert!(ledger.verify_balance());

        // Trading PnL = proceeds - cost_basis = 2.40 - 2.50 = -0.10 (fee tracked separately)
        assert!((ledger.total_realized_pnl() - (-0.10)).abs() < 0.001);
        // Total fees = 0.05 (buy) + 0.048 (sell) = 0.098
        assert!((ledger.total_fees() - 0.098).abs() < 0.001);
    }

    #[test]
    fn multiple_buys_same_asset() {
        let mut ledger = AccountingLedger::new(100.0, 0.0);

        ledger.record_buy("pos1", 5.0, 0.0, "token_x", "mkt_1", 50.0, 0.10, "buy 1");
        ledger.record_buy("pos2", 5.0, 0.0, "token_x", "mkt_1", 25.0, 0.20, "buy 2");

        assert_eq!(ledger.position_count(), 1); // same asset
        let h = ledger.holdings().get("token_x").unwrap();
        assert!((h.shares - 75.0).abs() < 0.001);
        assert!((h.cost_basis - 10.0).abs() < 0.001);
        assert!(ledger.verify_balance());
    }

    #[test]
    fn nav_computation() {
        let mut ledger = AccountingLedger::new(50.0, 0.0);
        ledger.record_buy("pos1", 5.0, 0.0, "token_a", "mkt_1", 100.0, 0.05, "buy");

        // NAV with mid price = 0.06
        let nav = ledger.compute_nav(|_| (0.05, 0.07));
        // 45.0 cash + 100 shares * 0.06 mid = 45.0 + 6.0 = 51.0
        assert!((nav - 51.0).abs() < 0.001);
    }

    #[test]
    fn dedup_prevents_double_counting() {
        let mut ledger = AccountingLedger::new(50.0, 0.02);

        // First record succeeds
        let ok = ledger.record_buy_dedup("trade_001", "pos1", 2.50, 0.05, "token_a", "mkt_1", 10.0, 0.25, "buy 1");
        assert!(ok);
        assert!((ledger.cash_balance() - 47.45).abs() < 0.001);

        // Duplicate is rejected
        let dup = ledger.record_buy_dedup("trade_001", "pos1", 2.50, 0.05, "token_a", "mkt_1", 10.0, 0.25, "buy 1 dup");
        assert!(!dup);
        assert!((ledger.cash_balance() - 47.45).abs() < 0.001); // unchanged

        // Different trade ID works
        let ok2 = ledger.record_buy_dedup("trade_002", "pos2", 2.50, 0.05, "token_b", "mkt_2", 5.0, 0.50, "buy 2");
        assert!(ok2);
        assert!((ledger.cash_balance() - 44.90).abs() < 0.001);

        assert!(ledger.verify_balance());
    }

    #[test]
    fn serialize_deserialize_round_trip() {
        let mut ledger = AccountingLedger::new(50.0, 0.02);
        ledger.record_buy("pos1", 2.50, 0.05, "token_1", "mkt_1", 10.0, 0.25, "test buy");

        let json = ledger.serialize_json();
        let restored = AccountingLedger::deserialize_json(&json).unwrap();

        assert!((restored.cash_balance() - ledger.cash_balance()).abs() < 0.001);
        assert!((restored.total_deployed() - ledger.total_deployed()).abs() < 0.001);
        assert_eq!(restored.position_count(), ledger.position_count());
        assert!(restored.verify_balance());
    }
}
