/// Live order executor for Polymarket CLOB (B3.0, B3.1, B3.2, B3.5).
///
/// Handles order construction, signing, submission, and status tracking.
/// In dry-run mode: constructs and signs orders, logs full details, but
/// does not submit to CLOB.
///
/// Key components:
///   - Market BUY quantity guard (B3.0)
///   - Order construction with instrument model + signing (B3.1)
///   - Trade status pipeline types (B3.2)
///   - Error handling matrix (B3.5)

use std::collections::HashMap;
use std::sync::Arc;

use crate::instrument::{Instrument, InstrumentStore};
use crate::signing::{self, OrderSigner, OrderData, SignedOrder, Side};
use crate::rate_limiter::{RateLimiter, RateCategory};

// ---------------------------------------------------------------------------
// Order types
// ---------------------------------------------------------------------------

/// Order type for CLOB submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderType {
    /// Fill And Kill — limit order that cancels unfilled portion immediately.
    Fak,
    /// Good Till Cancel — limit order that stays on the book.
    Gtc,
    /// Fill Or Kill — must fill entirely or cancel.
    Fok,
}

impl OrderType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fak => "FAK",
            Self::Gtc => "GTC",
            Self::Fok => "FOK",
        }
    }
}

/// Order aggression level (B3.1 fast-market note).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderAggression {
    /// Place at best bid/ask (cheapest, highest non-fill risk).
    Passive,
    /// FAK at current best price (default).
    AtMarket,
    /// 1 tick into the book (best fill rate, slightly worse price).
    Aggressive,
}

impl OrderAggression {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "passive" => Self::Passive,
            "aggressive" => Self::Aggressive,
            _ => Self::AtMarket,
        }
    }
}

// ---------------------------------------------------------------------------
// B3.0: Market BUY quantity guard
// ---------------------------------------------------------------------------

/// Quantity semantics for Polymarket orders.
///
/// - Market BUY:  quantity = USDC notional (quote currency)
/// - Market SELL: quantity = token count (base currency)
/// - Limit/FAK:   quantity = token count (base currency)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantityType {
    /// USDC notional (for market BUY only).
    Quote,
    /// Token count (for everything else).
    Base,
}

/// INC-021: Classify a CLOB rejection by whether it looks like an API
/// contract / schema mismatch (a Polymarket-side change we need to react to)
/// vs. a routine business rejection (price out of range, insufficient depth,
/// not accepting orders, etc.).
///
/// Returns `Some(category_tag)` for contract-style errors and `None` for
/// routine ones. The orchestrator uses this to send a one-shot Telegram
/// alert on first sighting of each contract category per boot — so we know
/// to look at the API as soon as it changes, not days later.
///
/// Heuristic: match against known phrases in the message / code. We include
/// generic field/format/version/unsupported keywords because those almost
/// always indicate a payload-shape change rather than a per-order issue.
pub fn classify_clob_rejection(code: &str, message: &str) -> Option<&'static str> {
    let blob = format!("{} {}", code.to_lowercase(), message.to_lowercase());

    // Schema / order-version drift (the order_version_mismatch we just hit)
    if blob.contains("version_mismatch") || blob.contains("orderversion")
        || blob.contains("order_version") || blob.contains("schema_version")
        || blob.contains("api_version") || blob.contains("apiversion") {
        return Some("schema_version_mismatch");
    }
    // Unknown / unexpected / missing fields — sharp signal of payload drift
    if blob.contains("unknown_field") || blob.contains("unexpected_field")
        || blob.contains("missing_field") || blob.contains("required_field")
        || blob.contains("field_required") || blob.contains("invalid_field") {
        return Some("field_drift");
    }
    // Format / serialization complaints
    if blob.contains("invalid_format") || blob.contains("malformed")
        || blob.contains("invalid_json") || blob.contains("parse_error")
        || blob.contains("decode_error") {
        return Some("format_drift");
    }
    // Deprecation / unsupported markers
    if blob.contains("deprecated") || blob.contains("unsupported")
        || blob.contains("not_supported") || blob.contains("removed") {
        return Some("deprecation");
    }
    // Auth contract drift (signature scheme, header contract)
    if blob.contains("invalid_signature") || blob.contains("signature_invalid")
        || blob.contains("bad_hmac") || blob.contains("hmac_invalid")
        || blob.contains("signature_type") {
        return Some("auth_drift");
    }
    None
}

/// Validate and compute the correct quantity for an order.
///
/// Returns (quantity, quantity_type) or an error if the combination is invalid.
///
/// B3.0 rules:
///   (a) Market BUY  → quantity = USDC notional (quote)
///   (b) Market SELL → quantity = token count (base)
///   (c) Limit/FAK   → quantity = token count (base)
///   (d) Base-denominated market BUY → REJECTED
pub fn compute_order_quantity(
    side: Side,
    order_type: OrderType,
    size_usd: f64,
    price: f64,
    instrument: &Instrument,
) -> Result<(f64, QuantityType), String> {
    let shares = size_usd / price;

    match (order_type, side) {
        // (a) Market BUY → quote (USDC notional)
        // Note: we don't support market orders directly, but if we did,
        // the quantity would be in USDC terms.
        // For FAK BUY (our standard), quantity is in base (token count).

        // (c) FAK/GTC/FOK BUY → base (token count)
        (OrderType::Fak | OrderType::Gtc | OrderType::Fok, Side::Buy) => {
            let rounded = instrument.rounding.round_size(shares);
            if rounded < 0.01 {
                return Err(format!("Order size too small: {} shares (${:.2} at {:.4})",
                    rounded, size_usd, price));
            }
            Ok((rounded, QuantityType::Base))
        }

        // (b) Market SELL / (c) FAK/GTC/FOK SELL → base (token count)
        (_, Side::Sell) => {
            let rounded = instrument.rounding.round_size(shares);
            if rounded < 0.01 {
                return Err(format!("Order size too small: {} shares", rounded));
            }
            Ok((rounded, QuantityType::Base))
        }
    }
}

// ---------------------------------------------------------------------------
// B3.2: Trade status pipeline
// ---------------------------------------------------------------------------

/// Status of a submitted trade on the CLOB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeStatus {
    /// Order submitted, awaiting match.
    Submitted,
    /// Matched by the CLOB engine.
    Matched,
    /// Transaction mined on Polygon.
    Mined,
    /// Transaction confirmed (finality reached).
    Confirmed,
    /// Transaction failed, being retried.
    Retrying,
    /// Permanently failed.
    Failed,
    /// Order cancelled (FAK unfilled portion, or manual cancel).
    Cancelled,
}

impl TradeStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Submitted => "SUBMITTED",
            Self::Matched => "MATCHED",
            Self::Mined => "MINED",
            Self::Confirmed => "CONFIRMED",
            Self::Retrying => "RETRYING",
            Self::Failed => "FAILED",
            Self::Cancelled => "CANCELLED",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Confirmed | Self::Failed | Self::Cancelled)
    }

    /// Validate whether a state transition is allowed.
    /// Rejects impossible transitions (e.g., terminal→non-terminal, backwards flow).
    /// Based on NT lesson: strict state machine caught multiple production bugs (#3403).
    pub fn can_transition_to(&self, new: &TradeStatus) -> bool {
        use TradeStatus::*;
        match (self, new) {
            // Terminal states cannot transition to anything
            (Confirmed, _) | (Failed, _) | (Cancelled, _) => false,
            // Submitted → Matched, Failed, Cancelled
            (Submitted, Matched) | (Submitted, Failed) | (Submitted, Cancelled) => true,
            // Matched → Mined, Retrying, Failed, Confirmed (fast-path confirmation)
            (Matched, Mined) | (Matched, Retrying) | (Matched, Failed) | (Matched, Confirmed) => true,
            // Mined → Confirmed, Retrying, Failed
            (Mined, Confirmed) | (Mined, Retrying) | (Mined, Failed) => true,
            // Retrying → Mined, Confirmed, Failed
            (Retrying, Mined) | (Retrying, Confirmed) | (Retrying, Failed) => true,
            _ => false,
        }
    }
}

/// A tracked order with its current status.
#[derive(Debug, Clone)]
pub struct TrackedOrder {
    /// CLOB order ID (assigned after submission).
    pub order_id: String,
    /// Our internal trade ID for dedup.
    pub trade_id: String,
    /// Position ID this order belongs to.
    pub position_id: String,
    /// Market ID (leg of the arb).
    pub market_id: String,
    /// Token ID being traded.
    pub token_id: String,
    /// Side (BUY/SELL).
    pub side: Side,
    /// Submitted price.
    pub price: f64,
    /// Submitted quantity (in appropriate units per B3.0).
    pub quantity: f64,
    /// Current status.
    pub status: TradeStatus,
    /// Filled quantity so far.
    pub filled_quantity: f64,
    /// Average fill price.
    pub avg_fill_price: f64,
    /// Timestamp of submission.
    pub submitted_at: f64,
    /// Timestamp of last status update.
    pub last_update: f64,
    /// The signed order (for logging/debugging).
    pub signed_order: Option<SignedOrder>,
    /// Whether this is a negRisk market.
    pub neg_risk: bool,
    /// B4.3: Overfill quantity (excess fill beyond order.quantity). NT #3221.
    /// Clamped: filled_quantity stays <= quantity, overfill tracks the excess.
    pub overfill_quantity: f64,
}

// ---------------------------------------------------------------------------
// B3.5: Execution errors
// ---------------------------------------------------------------------------

/// Errors that can occur during order execution.
#[derive(Debug, Clone)]
pub enum ExecutionError {
    /// CLOB API returned an error response.
    ClobRejection { code: String, message: String },
    /// CLOB API timed out.
    Timeout { elapsed_secs: f64 },
    /// Network failure (connection refused, DNS, etc).
    NetworkFailure { message: String },
    /// Insufficient balance for the order.
    InsufficientBalance { available: f64, required: f64 },
    /// Rate limited by CLOB API (429).
    RateLimited { retry_after_secs: f64 },
    /// Instrument not found or not accepting orders.
    InstrumentError { message: String },
    /// Quantity guard rejected the order (B3.0).
    QuantityGuardRejection { message: String },
    /// Signing failed.
    SigningError { message: String },
}

impl std::fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClobRejection { code, message } => write!(f, "CLOB rejection [{}]: {}", code, message),
            Self::Timeout { elapsed_secs } => write!(f, "Timeout after {:.1}s", elapsed_secs),
            Self::NetworkFailure { message } => write!(f, "Network failure: {}", message),
            Self::InsufficientBalance { available, required } =>
                write!(f, "Insufficient balance: ${:.2} available, ${:.2} required", available, required),
            Self::RateLimited { retry_after_secs } =>
                write!(f, "Rate limited, retry after {:.1}s", retry_after_secs),
            Self::InstrumentError { message } => write!(f, "Instrument error: {}", message),
            Self::QuantityGuardRejection { message } => write!(f, "Quantity guard: {}", message),
            Self::SigningError { message } => write!(f, "Signing error: {}", message),
        }
    }
}

/// Result of executing a single order.
#[derive(Debug, Clone)]
pub enum OrderResult {
    /// Order accepted (dry-run or live submission).
    Accepted(TrackedOrder),
    /// Order rejected before or during submission.
    Rejected(ExecutionError),
}

/// Result of executing all legs of an arb.
#[derive(Debug)]
pub struct ArbExecutionResult {
    /// Per-leg results.
    pub legs: Vec<OrderResult>,
    /// True if all legs were accepted.
    pub all_accepted: bool,
    /// True if this was a dry-run (no actual CLOB submission).
    pub dry_run: bool,
}

// ---------------------------------------------------------------------------
// B3.6: Partial fill evaluation
// ---------------------------------------------------------------------------

/// Decision from evaluating partial fills after arb execution.
#[derive(Debug, Clone, PartialEq)]
pub enum PartialFillAction {
    /// All legs fully filled — accept the position.
    Accept,
    /// Partial fill is still profitable — accept as-is.
    AcceptPartial { filled_pct: f64, estimated_profit_pct: f64 },
    /// Partial fill is unprofitable — unwind filled legs.
    Unwind { filled_legs: Vec<String>, reason: String },
    /// No fills at all — nothing to do.
    NoFill,
}

/// Evaluate the fill status of an arb after execution.
///
/// B3.6 rules:
///   1. Check actual fills via tracked orders.
///   2. Compute arb score of the filled position.
///   3. If score >= min_profit_threshold: accept.
///   4. If score < threshold: mark for unwind.
///   5. If no fills: NoFill.
pub fn evaluate_partial_fills(
    tracked_orders: &[TrackedOrder],
    min_profit_threshold: f64,
) -> PartialFillAction {
    if tracked_orders.is_empty() {
        return PartialFillAction::NoFill;
    }

    let total_legs = tracked_orders.len();
    let filled_legs: Vec<&TrackedOrder> = tracked_orders.iter()
        .filter(|o| o.filled_quantity > 0.0)
        .collect();

    if filled_legs.is_empty() {
        return PartialFillAction::NoFill;
    }

    // All legs fully filled?
    let all_filled = filled_legs.len() == total_legs
        && tracked_orders.iter().all(|o| {
            (o.filled_quantity - o.quantity).abs() < 0.01
        });

    if all_filled {
        return PartialFillAction::Accept;
    }

    // Partial fill: estimate profit from filled legs.
    // For a 2-leg arb: BUY leg cost + SELL leg revenue.
    // Positive = profitable partial.
    let mut total_cost = 0.0;
    let mut total_revenue = 0.0;

    for order in &filled_legs {
        let notional = order.filled_quantity * order.avg_fill_price;
        match order.side {
            Side::Buy => total_cost += notional,
            Side::Sell => total_revenue += notional,
        }
    }

    let filled_pct = filled_legs.len() as f64 / total_legs as f64;

    // If only BUY legs filled (no revenue yet), we can't compute profit.
    // This is a one-sided fill — needs unwind unless the arb is still executable.
    if total_cost > 0.0 && total_revenue.abs() < 1e-9 {
        // One-sided fill: we bought but didn't sell. Need to unwind.
        let filled_ids: Vec<String> = filled_legs.iter()
            .map(|o| o.trade_id.clone())
            .collect();
        return PartialFillAction::Unwind {
            filled_legs: filled_ids,
            reason: format!("One-sided fill: {}/{} legs filled (cost ${:.2}, no revenue)",
                filled_legs.len(), total_legs, total_cost),
        };
    }

    if total_revenue > 0.0 && total_cost.abs() < 1e-9 {
        // Only SELL legs filled — unusual but possible. Revenue without cost = profit.
        return PartialFillAction::AcceptPartial {
            filled_pct,
            estimated_profit_pct: 1.0, // All revenue, no cost
        };
    }

    // Both sides partially filled — compute profit ratio
    let estimated_profit_pct = if total_cost > 0.0 {
        (total_revenue - total_cost) / total_cost
    } else {
        0.0
    };

    if estimated_profit_pct >= min_profit_threshold {
        tracing::info!(
            "Partial fill accepted: {}/{} legs, est. profit {:.2}% (threshold {:.2}%)",
            filled_legs.len(), total_legs,
            estimated_profit_pct * 100.0, min_profit_threshold * 100.0,
        );
        PartialFillAction::AcceptPartial { filled_pct, estimated_profit_pct }
    } else {
        let filled_ids: Vec<String> = filled_legs.iter()
            .map(|o| o.trade_id.clone())
            .collect();
        tracing::warn!(
            "Partial fill unprofitable: {}/{} legs, est. profit {:.2}% < threshold {:.2}%",
            filled_legs.len(), total_legs,
            estimated_profit_pct * 100.0, min_profit_threshold * 100.0,
        );
        PartialFillAction::Unwind {
            filled_legs: filled_ids,
            reason: format!("Unprofitable partial: {:.2}% < {:.2}% threshold",
                estimated_profit_pct * 100.0, min_profit_threshold * 100.0),
        }
    }
}

// ---------------------------------------------------------------------------
// B3.1: LiveExecutor
// ---------------------------------------------------------------------------

/// Configuration for the executor.
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// CLOB API host (e.g., "https://clob.polymarket.com").
    pub clob_host: String,
    /// If true, sign and log orders but don't submit to CLOB.
    pub dry_run: bool,
    /// Default order type.
    pub order_type: OrderType,
    /// Order aggression (tick offset for FAK orders).
    pub aggression: OrderAggression,
    /// Fee rate in basis points (e.g., 0 for maker, 100 for 1% taker).
    pub fee_rate_bps: u64,
    /// Trade confirmation timeout in seconds (B3.2).
    pub confirmation_timeout_secs: f64,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            clob_host: "https://clob.polymarket.com".into(),
            dry_run: true,
            order_type: OrderType::Fak,
            aggression: OrderAggression::AtMarket,
            fee_rate_bps: 0,
            confirmation_timeout_secs: 120.0,
        }
    }
}

/// HTTP client timeout in seconds.
const HTTP_TIMEOUT_SECS: u64 = 30;

/// Format tick size for CLOB API (must be "0.1", "0.01", "0.001", or "0.0001").
fn format_tick_size(tick: f64) -> String {
    if (tick - 0.1).abs() < 1e-6 { "0.1".into() }
    else if (tick - 0.01).abs() < 1e-6 { "0.01".into() }
    else if (tick - 0.001).abs() < 1e-6 { "0.001".into() }
    else if (tick - 0.0001).abs() < 1e-6 { "0.0001".into() }
    else { "0.01".into() } // fallback
}

/// The live order executor. Constructs, signs, and submits orders to Polymarket CLOB.
pub struct Executor {
    config: ExecutorConfig,
    signer: OrderSigner,
    instruments: Arc<InstrumentStore>,
    rate_limiter: Arc<RateLimiter>,
    http_client: reqwest::blocking::Client,
    /// Active orders being tracked (trade_id → TrackedOrder).
    tracked: parking_lot::Mutex<HashMap<String, TrackedOrder>>,
    /// L2 HMAC auth for CLOB API (None = unauthenticated / shadow mode).
    clob_auth: Option<crate::signing::ClobAuth>,
}

impl Executor {
    /// Create a new executor.
    pub fn new(
        config: ExecutorConfig,
        signer: OrderSigner,
        instruments: Arc<InstrumentStore>,
        rate_limiter: Arc<RateLimiter>,
    ) -> Result<Self, String> {
        let http_client = crate::http_client::secure_client_tagged(HTTP_TIMEOUT_SECS, "executor")?;
        Ok(Self {
            config,
            signer,
            instruments,
            rate_limiter,
            http_client,
            tracked: parking_lot::Mutex::new(HashMap::new()),
            clob_auth: None,
        })
    }

    /// Set L2 CLOB API credentials for authenticated trading.
    pub fn set_clob_auth(&mut self, auth: crate::signing::ClobAuth) {
        self.clob_auth = Some(auth);
    }

    /// Execute all legs of an arb opportunity.
    ///
    /// Each leg is: (market_id, token_id, side, price, size_usd).
    /// In dry-run mode: constructs and signs all orders, logs them, returns Accepted.
    /// In live mode: submits to CLOB API after rate limit check.
    pub fn execute_arb(
        &self,
        position_id: &str,
        legs: &[(String, String, Side, f64, f64)],
    ) -> ArbExecutionResult {
        let now = chrono::Utc::now().timestamp() as f64;
        let mut results = Vec::with_capacity(legs.len());
        let mut all_accepted = true;

        for (market_id, token_id, side, price, size_usd) in legs {
            let result = self.execute_single_leg(
                position_id, market_id, token_id, *side, *price, *size_usd, now,
            );
            if matches!(&result, OrderResult::Rejected(_)) {
                all_accepted = false;
            }
            results.push(result);
        }

        // Log summary
        let accepted = results.iter().filter(|r| matches!(r, OrderResult::Accepted(_))).count();
        let rejected = results.len() - accepted;
        let mode = if self.config.dry_run { "SEQ DRY-RUN" } else { "SEQ LIVE" };

        if all_accepted {
            tracing::info!(
                "[{}] Arb {} submitted: {}/{} legs accepted",
                mode, position_id, accepted, results.len()
            );
        } else {
            tracing::warn!(
                "[{}] Arb {} partial: {}/{} accepted, {} rejected",
                mode, position_id, accepted, results.len(), rejected
            );
        }

        ArbExecutionResult {
            legs: results,
            all_accepted,
            dry_run: self.config.dry_run,
        }
    }

    /// Validate that an instrument exists and is accepting orders.
    fn validate_instrument(&self, token_id: &str, market_id: &str) -> Result<Instrument, OrderResult> {
        let inst = self.instruments.get(token_id)
            .ok_or_else(|| OrderResult::Rejected(ExecutionError::InstrumentError {
                message: format!("Unknown token_id: {}", token_id),
            }))?;
        if !inst.accepting_orders {
            return Err(OrderResult::Rejected(ExecutionError::InstrumentError {
                message: format!("Market {} not accepting orders", market_id),
            }));
        }
        Ok(inst)
    }

    /// Execute a single leg of an arb.
    fn execute_single_leg(
        &self,
        position_id: &str,
        market_id: &str,
        token_id: &str,
        side: Side,
        price: f64,
        size_usd: f64,
        now: f64,
    ) -> OrderResult {
        // 1. Look up and validate instrument
        let instrument = match self.validate_instrument(token_id, market_id) {
            Ok(inst) => inst,
            Err(result) => return result,
        };

        // 2. B3.0: Quantity guard
        let (quantity, _qty_type) = match compute_order_quantity(
            side, self.config.order_type, size_usd, price, &instrument,
        ) {
            Ok(q) => q,
            Err(msg) => return OrderResult::Rejected(ExecutionError::QuantityGuardRejection {
                message: msg,
            }),
        };

        // 3. Round price to instrument tick size
        let rounded_price = self.apply_aggression(price, side, &instrument);

        // 4. Build and sign order (use instrument's amount_decimals for precision)
        let is_market = matches!(self.config.order_type, OrderType::Fak);
        let order = match signing::build_order_with_precision_and_type(
            self.signer.address(),
            token_id,
            rounded_price,
            size_usd,
            side,
            instrument.neg_risk,
            self.config.fee_rate_bps,
            instrument.rounding.amount_decimals,
            is_market,
        ) {
            Ok(o) => o,
            Err(msg) => return OrderResult::Rejected(ExecutionError::SigningError {
                message: msg,
            }),
        };

        let signed = match self.signer.sign_order(&order, instrument.neg_risk) {
            Ok(s) => s,
            Err(msg) => return OrderResult::Rejected(ExecutionError::SigningError {
                message: msg,
            }),
        };

        // 5. Generate trade ID
        let trade_id = format!("{}_{}_{}",
            position_id, market_id,
            chrono::Utc::now().timestamp_millis()
        );

        let tracked = TrackedOrder {
            order_id: String::new(), // Set after CLOB submission
            trade_id: trade_id.clone(),
            position_id: position_id.to_string(),
            market_id: market_id.to_string(),
            token_id: token_id.to_string(),
            side,
            price: rounded_price,
            quantity,
            status: TradeStatus::Submitted,
            filled_quantity: 0.0,
            avg_fill_price: 0.0,
            submitted_at: now,
            last_update: now,
            signed_order: Some(signed.clone()),
            neg_risk: instrument.neg_risk,
            overfill_quantity: 0.0,
        };

        // 6. Log order details
        let side_str = if side == Side::Buy { "BUY" } else { "SELL" };
        let mode = if self.config.dry_run { "DRY-RUN" } else { "LIVE" };
        tracing::debug!(
            "[{}] {} {} {} shares @ {:.4} (${:.2}) token={} neg_risk={} sig={}...{}",
            mode, side_str, self.config.order_type.as_str(),
            quantity, rounded_price, size_usd,
            token_id.get(..8).unwrap_or(token_id),
            instrument.neg_risk,
            signed.signature.get(..10).unwrap_or(&signed.signature),
            signed.signature.get(signed.signature.len().saturating_sub(6)..).unwrap_or(&signed.signature),
        );

        // 7. Submit to CLOB (or skip in dry-run mode)
        if self.config.dry_run {
            // In dry-run: mark as confirmed immediately
            let mut order = tracked;
            order.status = TradeStatus::Confirmed;
            order.filled_quantity = quantity;
            order.avg_fill_price = rounded_price;

            // Track it
            self.tracked.lock().insert(trade_id, order.clone());

            return OrderResult::Accepted(order);
        }

        // Live mode: rate limit check, then submit
        if let Err(wait) = self.rate_limiter.check(RateCategory::Trading) {
            return OrderResult::Rejected(ExecutionError::RateLimited {
                retry_after_secs: wait,
            });
        }

        // Submit to CLOB API
        match self.submit_to_clob(&signed, &instrument, rounded_price, quantity, side) {
            Ok(order_id) => {
                let mut order = tracked;
                order.order_id = order_id;
                order.status = TradeStatus::Submitted;
                self.tracked.lock().insert(trade_id, order.clone());
                OrderResult::Accepted(order)
            }
            Err(e) => OrderResult::Rejected(e),
        }
    }

    /// Apply order aggression to adjust price by tick size.
    fn apply_aggression(&self, price: f64, side: Side, instrument: &Instrument) -> f64 {
        let tick = instrument.tick_size;
        let adjusted = match self.config.aggression {
            OrderAggression::Passive => price,
            OrderAggression::AtMarket => price,
            OrderAggression::Aggressive => match side {
                // BUY: increase price by 1 tick (more likely to fill)
                Side::Buy => price + tick,
                // SELL: decrease price by 1 tick
                Side::Sell => (price - tick).max(tick),
            },
        };
        instrument.rounding.round_price(adjusted)
    }

    /// Submit a signed order to the Polymarket CLOB API.
    ///
    /// Returns the CLOB order_id on success, or an ExecutionError.
    fn submit_to_clob(
        &self,
        signed: &SignedOrder,
        instrument: &Instrument,
        price: f64,
        size: f64,
        side: Side,
    ) -> Result<String, ExecutionError> {
        let url = format!("{}/order", self.config.clob_host);

        let order_payload = serde_json::json!({
            "order": {
                "salt": signed.order.salt.to::<u64>(),
                "maker": format!("{}", signed.order.maker),
                "signer": format!("{}", signed.order.signer),
                "taker": "0x0000000000000000000000000000000000000000",
                "tokenId": signed.order.token_id.to_string(),
                "makerAmount": signed.order.maker_amount.to_string(),
                "takerAmount": signed.order.taker_amount.to_string(),
                "expiration": signed.order.expiration.to_string(),
                "nonce": signed.order.nonce.to_string(),
                "feeRateBps": signed.order.fee_rate_bps.to_string(),
                "side": if side == Side::Buy { "BUY" } else { "SELL" },
                "signatureType": signed.order.signature_type,
                "signature": &signed.signature,
            },
            "owner": self.clob_auth.as_ref().map(|a| a.api_key()).unwrap_or_default(),
            "orderType": self.config.order_type.as_str(),
        });

        // Serialize once — same string for HMAC signing and HTTP body
        let body_str = serde_json::to_string(&order_payload).unwrap_or_default();
        tracing::warn!("[CLOB SUBMIT] POST {} payload: {}", url, &body_str);

        let mut req = self.http_client.post(&url)
            .header("Content-Type", "application/json")
            .body(body_str.clone());
        // Add L2 HMAC auth headers if configured
        if let Some(ref auth) = self.clob_auth {
            let headers = auth.build_headers("POST", "/order", Some(&body_str));
            for (k, v) in headers {
                req = req.header(&k, &v);
            }
        }
        let response = req.send()
            .map_err(|e| {
                if e.is_timeout() {
                    ExecutionError::Timeout {
                        elapsed_secs: HTTP_TIMEOUT_SECS as f64,
                    }
                } else {
                    ExecutionError::NetworkFailure { message: e.to_string() }
                }
            })?;

        let status = response.status();

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = response.headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(5.0);
            return Err(ExecutionError::RateLimited { retry_after_secs: retry_after });
        }

        if !status.is_success() {
            let text = response.text().unwrap_or_default();
            tracing::warn!("[CLOB REJECT] HTTP {}: {}", status.as_u16(), &text[..text.len().min(500)]);
            let (code, message) = if let Ok(body) = serde_json::from_str::<serde_json::Value>(&text) {
                let code = body.get("code").and_then(|v| v.as_str()).unwrap_or("UNKNOWN").to_string();
                let msg = body.get("message")
                    .or_else(|| body.get("error"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown error")
                    .to_string();
                (code, msg)
            } else {
                ("UNKNOWN".to_string(), format!("HTTP {}: {}", status.as_u16(), text))
            };
            return Err(ExecutionError::ClobRejection { code, message });
        }

        let body: serde_json::Value = response.json()
            .map_err(|e| ExecutionError::NetworkFailure {
                message: format!("Failed to parse response: {}", e),
            })?;

        // Extract order ID from response
        let order_id_value = body.get("orderID").or_else(|| body.get("order_id"));
        let order_id = match order_id_value {
            Some(v) => match v.as_str() {
                Some(s) => s.to_string(),
                None => {
                    tracing::warn!("Malformed order response: orderID field is not a string: {:?}", v);
                    String::new()
                }
            },
            None => {
                tracing::warn!("Malformed order response: no orderID field in response body");
                String::new()
            }
        };

        if order_id.is_empty() {
            return Err(ExecutionError::ClobRejection {
                code: "EMPTY_ORDER_ID".to_string(),
                message: "CLOB returned empty/missing order ID".to_string(),
            });
        }

        tracing::info!("CLOB order accepted: order_id={}", order_id);

        Ok(order_id)
    }

    // -----------------------------------------------------------------------
    // B3.2: Trade status tracking
    // -----------------------------------------------------------------------

    /// Update the status of a tracked order (called from WS events).
    ///
    /// Returns true if the update was applied, false if rejected (invalid transition
    /// or unknown trade_id). Based on NT lessons:
    ///   - Strict state machine validation (#3403)
    ///   - Overfill clamping (#3221): filled_qty > order.quantity is clamped, excess tracked
    pub fn update_trade_status(&self, trade_id: &str, new_status: TradeStatus,
                                filled_qty: Option<f64>, avg_price: Option<f64>) -> bool {
        let mut tracked = self.tracked.lock();
        if let Some(order) = tracked.get_mut(trade_id) {
            // State transition validation
            if !order.status.can_transition_to(&new_status) {
                tracing::warn!("Trade {} invalid transition: {} → {}, ignoring",
                    trade_id, order.status.as_str(), new_status.as_str());
                return false;
            }

            let old = order.status.as_str();
            order.status = new_status;
            order.last_update = chrono::Utc::now().timestamp() as f64;

            if let Some(qty) = filled_qty {
                // Anomalous fill on zero-quantity order
                if order.quantity == 0.0 && qty > 0.0 {
                    tracing::error!("Anomalous fill: {} shares on zero-quantity order {}", qty, trade_id);
                }
                // B4.3: Overfill detection — clamp to order quantity, track excess
                if qty > order.quantity && order.quantity > 0.0 {
                    let excess = qty - order.quantity;
                    tracing::warn!("B4.3 overfill: trade={} order_qty={:.6} fill_qty={:.6} excess={:.6}",
                        trade_id, order.quantity, qty, excess);
                    order.filled_quantity = order.quantity;
                    order.overfill_quantity = excess;
                } else {
                    order.filled_quantity = qty;
                }
            }
            if let Some(price) = avg_price {
                order.avg_fill_price = price;
            }
            tracing::info!("Trade {} status: {} → {}", trade_id, old, new_status.as_str());
            true
        } else {
            tracing::warn!("Trade {} not found in tracked orders", trade_id);
            false
        }
    }

    /// Get all pending (non-terminal) tracked orders.
    pub fn pending_orders(&self) -> Vec<TrackedOrder> {
        self.tracked.lock().values()
            .filter(|o| !o.status.is_terminal())
            .cloned()
            .collect()
    }

    /// INC-021 bug 3: did this position get any real CLOB fills?
    ///
    /// Returns true iff at least one tracked order with this `position_id`
    /// reached terminal `Confirmed` status (matched + transaction confirmed
    /// on-chain) with a non-zero filled quantity. Used by the orchestrator
    /// at API resolution time to refuse paper-only payouts.
    ///
    /// Note: this is the strict criterion. A position with `Submitted` or
    /// even `Matched` status but no Confirmed tx hasn't yet had real shares
    /// settle to the wallet (within the platform's settlement window).
    pub fn position_has_confirmed_fills(&self, position_id: &str) -> bool {
        self.tracked.lock().values().any(|o| {
            o.position_id == position_id
                && o.status == TradeStatus::Confirmed
                && o.filled_quantity > 0.0
        })
    }

    /// INC-020: Snapshot all tracked SELL orders for a given position_id.
    /// Used by the live proactive-exit reconciliation path to find the actual
    /// filled quantity and price per leg after submission.
    ///
    /// Returns a Vec of (market_id, filled_quantity, avg_fill_price, terminal).
    /// `terminal` indicates the order has reached a final state (Confirmed,
    /// Failed, or Cancelled) — the caller should only reconcile when all legs
    /// are terminal, or after the FAK fill window has elapsed.
    pub fn sell_orders_for_position(
        &self, position_id: &str,
    ) -> Vec<(String, f64, f64, bool)> {
        self.tracked.lock().values()
            .filter(|o| o.position_id == position_id && o.side == Side::Sell)
            .map(|o| (o.market_id.clone(), o.filled_quantity, o.avg_fill_price, o.status.is_terminal()))
            .collect()
    }

    /// Return the set of all order IDs the executor is currently tracking.
    ///
    /// Used by the startup orphan-order sweep (`reconciliation::sweep_orphan_orders`)
    /// to distinguish "our" live orders from strays left behind by a prior
    /// process crash or a failed trade. Includes terminal orders too — not
    /// harmful to "protect" a filled/cancelled order ID from the sweep since
    /// the CLOB will never return it as LIVE. Empty IDs (pre-submission) are
    /// filtered out so they don't match unrelated orders.
    pub fn all_tracked_order_ids(&self) -> std::collections::HashSet<String> {
        self.tracked.lock().values()
            .map(|o| o.order_id.clone())
            .filter(|id| !id.is_empty())
            .collect()
    }

    /// Query the CLOB for live orders and cancel any the executor isn't tracking.
    /// Wrapper over `reconciliation::sweep_orphan_orders` that plugs in this
    /// executor's http client, auth, and tracked set. Returns (found, cancelled).
    ///
    /// Safe no-op in shadow mode: `clob_auth` is `None`, so the sweep skips
    /// the query entirely — Polymarket's `/data/orders` endpoint requires L2
    /// auth and would 401.
    pub fn sweep_orphan_orders(&self, clob_host: &str) -> (usize, usize) {
        let tracked = self.all_tracked_order_ids();
        crate::reconciliation::sweep_orphan_orders(
            &self.http_client,
            clob_host,
            self.clob_auth.as_ref(),
            &tracked,
        )
    }

    /// Get timed-out orders (submitted but no update within confirmation_timeout).
    pub fn timed_out_orders(&self) -> Vec<TrackedOrder> {
        let now = chrono::Utc::now().timestamp() as f64;
        let timeout = self.config.confirmation_timeout_secs;
        self.tracked.lock().values()
            .filter(|o| !o.status.is_terminal()
                && (now - o.submitted_at) >= timeout)
            .cloned()
            .collect()
    }

    /// Remove terminal orders older than the given age (seconds).
    pub fn cleanup_old_orders(&self, max_age_secs: f64) {
        let now = chrono::Utc::now().timestamp() as f64;
        let mut tracked = self.tracked.lock();
        tracked.retain(|_, o| {
            !o.status.is_terminal() || (now - o.last_update) < max_age_secs
        });
    }

    /// Is the executor in dry-run mode?
    pub fn is_dry_run(&self) -> bool {
        self.config.dry_run
    }

    /// Get the CLOB host URL.
    pub fn clob_host(&self) -> &str {
        &self.config.clob_host
    }

    // -----------------------------------------------------------------------
    // C2: Kill switch — cancel all open CLOB orders
    // -----------------------------------------------------------------------

    /// Cancel all non-terminal tracked orders.
    ///
    /// In dry-run mode: marks all pending orders as Cancelled locally.
    /// In live mode: calls the CLOB cancel-all endpoint, then marks locally.
    /// Returns (cancelled_count, error_message).
    pub fn cancel_all_orders(&self) -> (usize, Option<String>) {
        let pending: Vec<String> = {
            let tracked = self.tracked.lock();
            tracked.values()
                .filter(|o| !o.status.is_terminal())
                .map(|o| o.order_id.clone())
                .collect()
        };

        if pending.is_empty() {
            tracing::info!("[KILL] No pending orders to cancel");
            return (0, None);
        }

        tracing::warn!("[KILL] Cancelling {} pending orders", pending.len());

        // In live mode, call the CLOB cancel-all endpoint
        let api_error = if !self.config.dry_run {
            match self.cancel_all_on_clob() {
                Ok(()) => None,
                Err(e) => {
                    tracing::error!("[KILL] CLOB cancel-all failed: {}", e);
                    Some(e)
                }
            }
        } else {
            tracing::info!("[KILL] Dry-run mode — skipping CLOB cancel API call");
            None
        };

        // Mark all pending orders as cancelled locally
        let mut tracked = self.tracked.lock();
        let now = chrono::Utc::now().timestamp() as f64;
        let mut count = 0usize;
        for order in tracked.values_mut() {
            if !order.status.is_terminal() {
                order.status = TradeStatus::Cancelled;
                order.last_update = now;
                count += 1;
            }
        }

        tracing::warn!("[KILL] Marked {} tracked orders as cancelled", count);
        (count, api_error)
    }

    /// Call the Polymarket CLOB cancel-all endpoint.
    ///
    /// Requires L2 API credentials (API key + HMAC signature).
    /// These will be configured in Milestone D when live trading is enabled.
    fn cancel_all_on_clob(&self) -> Result<(), String> {
        // L2 auth headers (POLY_API_KEY, POLY_TIMESTAMP, POLY_SIGNATURE, POLY_PASSPHRASE)
        // are required for authenticated endpoints. Until Milestone D configures these,
        // this will return an error which is caught and logged by cancel_all_orders().
        //
        // Endpoint: DELETE {clob_host}/cancel-all
        // No body needed — cancels all open orders for the authenticated wallet.

        let url = format!("{}/cancel-all", self.config.clob_host);

        // API-3: L2 HMAC auth headers for cancel-all (safety-critical path for kill switch)
        let mut req = self.http_client.delete(&url);
        if let Some(ref auth) = self.clob_auth {
            let headers = auth.build_headers("DELETE", "/cancel-all", None);
            for (k, v) in headers {
                req = req.header(&k, &v);
            }
        }
        let resp = req.send()
            .map_err(|e| format!("cancel-all request failed: {}", e))?;

        let status = resp.status();
        if status.is_success() {
            tracing::info!("[KILL] CLOB cancel-all succeeded");
            Ok(())
        } else if status.as_u16() == 401 && self.config.dry_run {
            // 401 in dry-run / shadow mode is expected — no L2 credentials configured
            tracing::info!("[KILL] CLOB cancel-all returned 401 (no L2 credentials — expected in dry-run/shadow mode)");
            Ok(())
        } else {
            let body = resp.text().unwrap_or_default();
            Err(format!("cancel-all returned {}: {}", status, body))
        }
    }

    // -----------------------------------------------------------------------
    // B3.6: Evaluate partial fills after arb execution
    // -----------------------------------------------------------------------

    /// Evaluate the fill status of a previously executed arb.
    ///
    /// Retrieves tracked orders for the position and runs partial fill evaluation.
    pub fn evaluate_arb_fills(&self, position_id: &str, min_profit_threshold: f64) -> PartialFillAction {
        let tracked = self.tracked.lock();
        let orders: Vec<TrackedOrder> = tracked.values()
            .filter(|o| o.position_id == position_id)
            .cloned()
            .collect();
        evaluate_partial_fills(&orders, min_profit_threshold)
    }

    // -----------------------------------------------------------------------
    // B3.7: Batch order submission
    // -----------------------------------------------------------------------

    /// Execute all legs of an arb as a single batch request to the CLOB API.
    ///
    /// Reduces latency window for partial-fill exposure on multi-leg positions.
    /// Falls back to sequential submission if batch endpoint fails.
    pub fn execute_arb_batch(
        &self,
        position_id: &str,
        legs: &[(String, String, Side, f64, f64)],
    ) -> ArbExecutionResult {
        let now = chrono::Utc::now().timestamp() as f64;

        // Build and sign all orders first (before any submission)
        let mut prepared: Vec<(TrackedOrder, SignedOrder, Instrument)> = Vec::with_capacity(legs.len());
        let mut results: Vec<OrderResult> = Vec::new();

        for (market_id, token_id, side, price, size_usd) in legs {
            // Look up and validate instrument
            let instrument = match self.validate_instrument(token_id, market_id) {
                Ok(inst) => inst,
                Err(result) => {
                    results.push(result);
                    continue;
                }
            };

            // B3.0: Quantity guard
            let (quantity, _) = match compute_order_quantity(
                *side, self.config.order_type, *size_usd, *price, &instrument,
            ) {
                Ok(q) => q,
                Err(msg) => {
                    results.push(OrderResult::Rejected(ExecutionError::QuantityGuardRejection { message: msg }));
                    continue;
                }
            };

            let rounded_price = self.apply_aggression(*price, *side, &instrument);

            let is_market = matches!(self.config.order_type, OrderType::Fak);
            let order = match signing::build_order_with_precision_and_type(
                self.signer.address(),
                token_id,
                rounded_price,
                *size_usd,
                *side,
                instrument.neg_risk,
                self.config.fee_rate_bps,
                instrument.rounding.amount_decimals,
                is_market,
            ) {
                Ok(o) => o,
                Err(msg) => {
                    results.push(OrderResult::Rejected(ExecutionError::SigningError { message: msg }));
                    continue;
                }
            };

            let signed = match self.signer.sign_order(&order, instrument.neg_risk) {
                Ok(s) => s,
                Err(msg) => {
                    results.push(OrderResult::Rejected(ExecutionError::SigningError { message: msg }));
                    continue;
                }
            };

            let trade_id = format!("{}_{}_{}",
                position_id, market_id,
                chrono::Utc::now().timestamp_millis()
            );

            let tracked = TrackedOrder {
                order_id: String::new(),
                trade_id,
                position_id: position_id.to_string(),
                market_id: market_id.to_string(),
                token_id: token_id.to_string(),
                side: *side,
                price: rounded_price,
                quantity,
                status: TradeStatus::Submitted,
                filled_quantity: 0.0,
                avg_fill_price: 0.0,
                submitted_at: now,
                last_update: now,
                signed_order: Some(signed.clone()),
                neg_risk: instrument.neg_risk,
                overfill_quantity: 0.0,
            };

            prepared.push((tracked, signed, instrument));
        }

        // If any legs failed during preparation, don't submit any
        if !results.is_empty() {
            // Some legs couldn't be prepared — abort batch
            let accepted = 0;
            let rejected = results.len();
            tracing::warn!(
                "[BATCH] Arb {} preparation failed: {} rejected during signing/validation",
                position_id, rejected
            );
            return ArbExecutionResult {
                legs: results,
                all_accepted: false,
                dry_run: self.config.dry_run,
            };
        }

        if self.config.dry_run {
            // Dry run: mark all as confirmed
            for (mut tracked, _signed, _inst) in prepared {
                tracked.status = TradeStatus::Confirmed;
                tracked.filled_quantity = tracked.quantity;
                tracked.avg_fill_price = tracked.price;
                let side_str = if tracked.side == Side::Buy { "BUY" } else { "SELL" };
                tracing::info!(
                    "[DRY-RUN BATCH] {} {} shares @ {:.4} token={}",
                    side_str, tracked.quantity, tracked.price,
                    tracked.token_id.get(..8).unwrap_or(&tracked.token_id),
                );
                let tracking_copy = tracked.clone();
                results.push(OrderResult::Accepted(tracked));
                self.tracked.lock().insert(tracking_copy.trade_id.clone(), tracking_copy);
            }
            return ArbExecutionResult {
                legs: results,
                all_accepted: true,
                dry_run: true,
            };
        }

        // Live mode: rate limit check
        if let Err(wait) = self.rate_limiter.check(RateCategory::Trading) {
            return ArbExecutionResult {
                legs: vec![OrderResult::Rejected(ExecutionError::RateLimited {
                    retry_after_secs: wait,
                })],
                all_accepted: false,
                dry_run: false,
            };
        }

        // Build batch payload
        // Polymarket batch API caps at 15 orders
        if prepared.len() > 15 {
            tracing::warn!("[BATCH] {} legs exceeds Polymarket batch limit of 15, falling back to sequential",
                prepared.len());
            return self.execute_arb(position_id, legs);
        }

        let mut order_payloads = Vec::with_capacity(prepared.len());
        for (tracked, signed, instrument) in &prepared {
            let side_str = if tracked.side == Side::Buy { "BUY" } else { "SELL" };
            order_payloads.push(serde_json::json!({
                "order": {
                    "salt": signed.order.salt.to::<u64>(),
                    "maker": format!("{}", signed.order.maker),
                    "signer": format!("{}", signed.order.signer),
                    "taker": "0x0000000000000000000000000000000000000000",
                    "tokenId": signed.order.token_id.to_string(),
                    "makerAmount": signed.order.maker_amount.to_string(),
                    "takerAmount": signed.order.taker_amount.to_string(),
                    "expiration": signed.order.expiration.to_string(),
                    "nonce": signed.order.nonce.to_string(),
                    "feeRateBps": signed.order.fee_rate_bps.to_string(),
                    "side": side_str,
                    "signatureType": signed.order.signature_type,
                    "signature": &signed.signature,
                },
                "owner": self.clob_auth.as_ref().map(|a| a.api_key()).unwrap_or_default(),
                "orderType": self.config.order_type.as_str(),
            }));
        }

        // API-1: Serialize once for both HMAC signing and HTTP body (matches single-order path)
        let url = format!("{}/orders", self.config.clob_host);
        let body_str = serde_json::to_string(&order_payloads).unwrap_or_default();
        tracing::debug!("[BATCH SUBMIT] POST {} ({} orders)", url, order_payloads.len());

        let mut req = self.http_client.post(&url)
            .header("Content-Type", "application/json")
            .body(body_str.clone());
        if let Some(ref auth) = self.clob_auth {
            let headers = auth.build_headers("POST", "/orders", Some(&body_str));
            for (k, v) in headers {
                req = req.header(&k, &v);
            }
        }
        let response = req.send();

        match response {
            Ok(resp) => {
                let status = resp.status();
                if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    return ArbExecutionResult {
                        legs: vec![OrderResult::Rejected(ExecutionError::RateLimited {
                            retry_after_secs: 5.0,
                        })],
                        all_accepted: false,
                        dry_run: false,
                    };
                }

                // R1: Fail explicitly on malformed JSON instead of silently treating as empty
                let body: serde_json::Value = match resp.json() {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!("[BATCH] Failed to parse CLOB response: {}", e);
                        return ArbExecutionResult {
                            legs: prepared.into_iter().map(|(t, _, _)| {
                                OrderResult::Rejected(ExecutionError::NetworkFailure {
                                    message: format!("batch response parse error: {}", e),
                                })
                            }).collect(),
                            all_accepted: false,
                            dry_run: false,
                        };
                    }
                };

                if status.is_success() {
                    // Parse batch response: array of per-order results
                    // Each element has: orderID, success (bool), errorMsg (optional)
                    let response_arr = body.as_array();

                    let mut all_accepted = true;
                    for (i, (mut tracked, _signed, _inst)) in prepared.into_iter().enumerate() {
                        let entry = response_arr.and_then(|arr| arr.get(i));

                        // M3: Default to false — require explicit success from CLOB
                        let success = entry
                            .and_then(|v| v.get("success"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);

                        let order_id = entry
                            .and_then(|v| v.get("orderID").or_else(|| v.get("order_id")))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");

                        if success && !order_id.is_empty() {
                            tracked.order_id = order_id.to_string();
                            let tracking_copy = tracked.clone();
                            results.push(OrderResult::Accepted(tracked));
                            self.tracked.lock().insert(tracking_copy.trade_id.clone(), tracking_copy);
                        } else {
                            all_accepted = false;
                            let error_msg = entry
                                .and_then(|v| v.get("errorMsg").or_else(|| v.get("error_msg")))
                                .and_then(|v| v.as_str())
                                .unwrap_or("No order ID or success=false");
                            tracing::warn!("[BATCH] Leg {} rejected: {}", i, error_msg);
                            results.push(OrderResult::Rejected(ExecutionError::ClobRejection {
                                code: "BATCH_REJECTED".into(),
                                message: format!("Leg {}: {}", i, error_msg),
                            }));
                        }
                    }

                    tracing::info!("[BATCH] Arb {} submitted: {}/{} legs accepted",
                        position_id, results.len(), legs.len());

                    ArbExecutionResult { legs: results, all_accepted, dry_run: false }
                } else {
                    // Batch rejected — fall back to sequential
                    tracing::warn!(
                        "[BATCH] Batch endpoint returned {}, falling back to sequential",
                        status
                    );
                    drop(body);
                    // Re-prepare legs from scratch via sequential path
                    return self.execute_arb(position_id, legs);
                }
            }
            Err(e) => {
                // Network error — fall back to sequential
                tracing::warn!("[BATCH] Network error: {}, falling back to sequential", e);
                self.execute_arb(position_id, legs)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// B3.4: Timestamp normalisation
// ---------------------------------------------------------------------------

/// Parse a Polymarket timestamp from various formats into Unix seconds (f64).
///
/// Handles:
///   - ISO8601 with timezone ("2026-03-17T12:00:00Z", "2026-03-17T12:00:00+00:00")
///   - ISO8601 without timezone (assumes UTC)
///   - Unix seconds (integer or float, e.g., 1742212800)
///   - Unix milliseconds (>1e12, e.g., 1742212800000)
///   - null/empty → returns 0.0
pub fn parse_polymarket_timestamp(value: &serde_json::Value) -> f64 {
    match value {
        serde_json::Value::Null => 0.0,
        serde_json::Value::Number(n) => {
            let v = n.as_f64().unwrap_or(0.0);
            if v > 1e12 {
                // Milliseconds → seconds
                v / 1000.0
            } else {
                v
            }
        }
        serde_json::Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return 0.0;
            }
            // Try as numeric string first
            if let Ok(v) = s.parse::<f64>() {
                return if v > 1e12 { v / 1000.0 } else { v };
            }
            // Try ISO8601 with chrono
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                return dt.timestamp() as f64;
            }
            // Try ISO8601 without timezone (assume UTC)
            if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
                return dt.and_utc().timestamp() as f64;
            }
            if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
                return dt.and_utc().timestamp() as f64;
            }
            // Try Z suffix (common in Polymarket)
            let no_z = s.trim_end_matches('Z');
            if no_z != s {
                if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(no_z, "%Y-%m-%dT%H:%M:%S") {
                    return dt.and_utc().timestamp() as f64;
                }
                if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(no_z, "%Y-%m-%dT%H:%M:%S%.f") {
                    return dt.and_utc().timestamp() as f64;
                }
            }
            tracing::warn!("Failed to parse timestamp: '{}'", s);
            0.0
        }
        _ => 0.0,
    }
}

// ---------------------------------------------------------------------------
// B3.2: Fill lifecycle event processing (suspense accounting integration)
// ---------------------------------------------------------------------------

/// Action taken after processing a fill lifecycle event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FillAction {
    /// Trade entered suspense (MATCHED).
    EnteredSuspense,
    /// Trade confirmed — promoted to real position (CONFIRMED).
    Confirmed,
    /// Trade retrying on-chain — held in suspense (RETRYING).
    Retrying,
    /// Trade failed — capital reversed, opposing legs may need selling (FAILED).
    Failed { needs_opposing_leg_sell: bool },
    /// Trade already processed (dedup).
    AlreadyProcessed,
}

/// Process a fill lifecycle event and update accounting suspense.
///
/// Routes MATCHED/CONFIRMED/RETRYING/FAILED to the appropriate
/// `AccountingLedger` method and returns the action taken.
pub fn process_fill_event(
    accounting: &parking_lot::Mutex<crate::accounting::AccountingLedger>,
    trade_id: &str,
    status: TradeStatus,
    order: &TrackedOrder,
    has_opposing_legs_filled: bool,
) -> FillAction {
    let mut ledger = accounting.lock();

    match status {
        TradeStatus::Matched => {
            let capital = order.price * order.filled_quantity;
            let fees = capital * 0.02; // taker fee (from config ideally, but ledger has it)
            let ok = ledger.enter_suspense(
                trade_id,
                &order.position_id,
                &order.token_id,
                &order.market_id,
                order.filled_quantity,
                order.avg_fill_price,
                capital,
                fees,
            );
            if ok { FillAction::EnteredSuspense } else { FillAction::AlreadyProcessed }
        }
        TradeStatus::Confirmed => {
            match ledger.confirm_from_suspense(trade_id) {
                Some(_) => FillAction::Confirmed,
                None => {
                    // Not in suspense — may have been recorded directly via record_buy_dedup
                    FillAction::AlreadyProcessed
                }
            }
        }
        TradeStatus::Retrying => {
            ledger.mark_suspense_retrying(trade_id);
            FillAction::Retrying
        }
        TradeStatus::Failed => {
            match ledger.reverse_suspense(trade_id) {
                Some(_) => FillAction::Failed {
                    needs_opposing_leg_sell: has_opposing_legs_filled,
                },
                None => FillAction::AlreadyProcessed,
            }
        }
        _ => FillAction::AlreadyProcessed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::RoundingConfig;

    fn test_instrument() -> Instrument {
        Instrument {
            market_id: "test_market".into(),
            token_id: "123456789".into(),
            outcome: "yes".into(),
            condition_id: "cond_1".into(),
            neg_risk: false,
            tick_size: 0.01,
            rounding: RoundingConfig::from_tick_size("0.01"),
            min_order_size: 1.0,
            max_order_size: 0.0,
            order_book_enabled: true,
            accepting_orders: true,
        }
    }

    // B3.0 tests
    #[test]
    fn test_quantity_guard_fak_buy() {
        let inst = test_instrument();
        let (qty, qt) = compute_order_quantity(Side::Buy, OrderType::Fak, 50.0, 0.50, &inst).unwrap();
        assert_eq!(qt, QuantityType::Base);
        assert!((qty - 100.0).abs() < 0.01); // $50 / $0.50 = 100 shares
    }

    #[test]
    fn test_quantity_guard_fak_sell() {
        let inst = test_instrument();
        let (qty, qt) = compute_order_quantity(Side::Sell, OrderType::Fak, 50.0, 0.50, &inst).unwrap();
        assert_eq!(qt, QuantityType::Base);
        assert!((qty - 100.0).abs() < 0.01);
    }

    #[test]
    fn test_quantity_guard_too_small() {
        let inst = test_instrument();
        let result = compute_order_quantity(Side::Buy, OrderType::Fak, 0.001, 0.50, &inst);
        assert!(result.is_err());
    }

    #[test]
    fn test_quantity_guard_gtc_buy() {
        let inst = test_instrument();
        let (qty, qt) = compute_order_quantity(Side::Buy, OrderType::Gtc, 10.0, 0.25, &inst).unwrap();
        assert_eq!(qt, QuantityType::Base);
        assert!((qty - 40.0).abs() < 0.01); // $10 / $0.25 = 40 shares
    }

    // B3.4 timestamp tests
    #[test]
    fn test_parse_timestamp_iso8601() {
        let v = serde_json::json!("2026-03-17T12:00:00Z");
        let ts = parse_polymarket_timestamp(&v);
        assert!(ts > 1.7e9); // Reasonable Unix timestamp
    }

    #[test]
    fn test_parse_timestamp_unix_seconds() {
        let v = serde_json::json!(1742212800);
        let ts = parse_polymarket_timestamp(&v);
        assert!((ts - 1742212800.0).abs() < 0.01);
    }

    #[test]
    fn test_parse_timestamp_unix_millis() {
        let v = serde_json::json!(1742212800000u64);
        let ts = parse_polymarket_timestamp(&v);
        assert!((ts - 1742212800.0).abs() < 0.01);
    }

    #[test]
    fn test_parse_timestamp_null() {
        let v = serde_json::json!(null);
        assert_eq!(parse_polymarket_timestamp(&v), 0.0);
    }

    #[test]
    fn test_parse_timestamp_empty_string() {
        let v = serde_json::json!("");
        assert_eq!(parse_polymarket_timestamp(&v), 0.0);
    }

    #[test]
    fn test_parse_timestamp_numeric_string() {
        let v = serde_json::json!("1742212800");
        let ts = parse_polymarket_timestamp(&v);
        assert!((ts - 1742212800.0).abs() < 0.01);
    }

    #[test]
    fn test_parse_timestamp_iso_no_tz() {
        let v = serde_json::json!("2026-03-17T12:00:00");
        let ts = parse_polymarket_timestamp(&v);
        assert!(ts > 1.7e9);
    }

    // Trade status tests
    #[test]
    fn test_trade_status_terminal() {
        assert!(TradeStatus::Confirmed.is_terminal());
        assert!(TradeStatus::Failed.is_terminal());
        assert!(TradeStatus::Cancelled.is_terminal());
        assert!(!TradeStatus::Submitted.is_terminal());
        assert!(!TradeStatus::Matched.is_terminal());
        assert!(!TradeStatus::Retrying.is_terminal());
    }

    // B3.6: Partial fill tests
    fn make_tracked(side: Side, qty: f64, price: f64, filled_qty: f64, avg_price: f64) -> TrackedOrder {
        TrackedOrder {
            order_id: "ord1".into(),
            trade_id: "t1".into(),
            position_id: "pos1".into(),
            market_id: "mkt1".into(),
            token_id: "tok1".into(),
            side,
            price,
            quantity: qty,
            status: if filled_qty > 0.0 { TradeStatus::Confirmed } else { TradeStatus::Cancelled },
            filled_quantity: filled_qty,
            avg_fill_price: avg_price,
            submitted_at: 0.0,
            last_update: 0.0,
            signed_order: None,
            neg_risk: false,
            overfill_quantity: 0.0,
        }
    }

    #[test]
    fn test_partial_fill_all_filled() {
        let orders = vec![
            make_tracked(Side::Buy, 100.0, 0.40, 100.0, 0.40),
            make_tracked(Side::Sell, 100.0, 0.60, 100.0, 0.60),
        ];
        assert_eq!(evaluate_partial_fills(&orders, 0.03), PartialFillAction::Accept);
    }

    #[test]
    fn test_partial_fill_no_fills() {
        let orders = vec![
            make_tracked(Side::Buy, 100.0, 0.40, 0.0, 0.0),
            make_tracked(Side::Sell, 100.0, 0.60, 0.0, 0.0),
        ];
        assert_eq!(evaluate_partial_fills(&orders, 0.03), PartialFillAction::NoFill);
    }

    #[test]
    fn test_partial_fill_one_sided_buy() {
        let orders = vec![
            make_tracked(Side::Buy, 100.0, 0.40, 100.0, 0.40),
            make_tracked(Side::Sell, 100.0, 0.60, 0.0, 0.0),
        ];
        match evaluate_partial_fills(&orders, 0.03) {
            PartialFillAction::Unwind { reason, .. } => {
                assert!(reason.contains("One-sided"));
            }
            other => panic!("Expected Unwind, got {:?}", other),
        }
    }

    #[test]
    fn test_partial_fill_profitable() {
        // BUY 50 @ 0.40 = $20 cost, SELL 50 @ 0.65 = $32.50 revenue
        // Profit = $12.50 / $20 = 62.5%
        let orders = vec![
            make_tracked(Side::Buy, 100.0, 0.40, 50.0, 0.40),
            make_tracked(Side::Sell, 100.0, 0.60, 50.0, 0.65),
        ];
        match evaluate_partial_fills(&orders, 0.03) {
            PartialFillAction::AcceptPartial { estimated_profit_pct, .. } => {
                assert!(estimated_profit_pct > 0.5); // >50% profit
            }
            other => panic!("Expected AcceptPartial, got {:?}", other),
        }
    }

    #[test]
    fn test_partial_fill_unprofitable() {
        // BUY 50 @ 0.50 = $25 cost, SELL 50 @ 0.50 = $25 revenue
        // Profit = 0%
        let orders = vec![
            make_tracked(Side::Buy, 100.0, 0.50, 50.0, 0.50),
            make_tracked(Side::Sell, 100.0, 0.50, 50.0, 0.50),
        ];
        match evaluate_partial_fills(&orders, 0.03) {
            PartialFillAction::Unwind { reason, .. } => {
                assert!(reason.contains("Unprofitable"));
            }
            other => panic!("Expected Unwind, got {:?}", other),
        }
    }

    #[test]
    fn test_partial_fill_empty() {
        assert_eq!(evaluate_partial_fills(&[], 0.03), PartialFillAction::NoFill);
    }

    // State transition validation tests
    #[test]
    fn test_valid_transitions() {
        use TradeStatus::*;
        // Happy path: Submitted → Matched → Mined → Confirmed
        assert!(Submitted.can_transition_to(&Matched));
        assert!(Matched.can_transition_to(&Mined));
        assert!(Mined.can_transition_to(&Confirmed));
        // Retry path
        assert!(Matched.can_transition_to(&Retrying));
        assert!(Retrying.can_transition_to(&Mined));
        assert!(Retrying.can_transition_to(&Confirmed));
        // Failure from any non-terminal
        assert!(Submitted.can_transition_to(&Failed));
        assert!(Matched.can_transition_to(&Failed));
        assert!(Mined.can_transition_to(&Failed));
        assert!(Retrying.can_transition_to(&Failed));
        // Cancel from Submitted
        assert!(Submitted.can_transition_to(&Cancelled));
        // Fast-path: Matched → Confirmed (skip Mined)
        assert!(Matched.can_transition_to(&Confirmed));
    }

    #[test]
    fn test_invalid_transitions() {
        use TradeStatus::*;
        // Terminal states cannot transition
        assert!(!Confirmed.can_transition_to(&Failed));
        assert!(!Failed.can_transition_to(&Matched));
        assert!(!Cancelled.can_transition_to(&Submitted));
        // Backwards flow
        assert!(!Matched.can_transition_to(&Submitted));
        assert!(!Mined.can_transition_to(&Submitted));
        assert!(!Confirmed.can_transition_to(&Mined));
        // Self-transition
        assert!(!Submitted.can_transition_to(&Submitted));
    }

    #[test]
    fn test_overfill_clamp() {
        // Simulate what update_trade_status does: clamp fill to order qty
        let order_qty: f64 = 100.0;
        let fill_qty: f64 = 105.5;
        let excess = fill_qty - order_qty;
        assert!((excess - 5.5).abs() < 1e-10);
        // The clamped value should be order_qty
        let clamped = if fill_qty > order_qty { order_qty } else { fill_qty };
        assert!((clamped - 100.0).abs() < 1e-10);
    }
}
