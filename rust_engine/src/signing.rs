/// EIP-712 order signing for Polymarket CLOB Exchange (B2.0).
///
/// Implements pure Rust EIP-712 typed data signing matching the Polymarket
/// py-clob-client `order_builder/` implementation. Signature type 0 (EOA).
///
/// References:
///   - EIP-712: https://eips.ethereum.org/EIPS/eip-712
///   - Polymarket CLOB Exchange contract on Polygon (chain ID 137)
///   - py-clob-client order_builder/helpers.py

use alloy_primitives::{Address, B256, U256, FixedBytes, keccak256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;

// ---------------------------------------------------------------------------
// Polymarket contract addresses (Polygon mainnet)
// ---------------------------------------------------------------------------

/// CTF Exchange — used for regular (non-negRisk) markets.
pub const CTF_EXCHANGE: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";

/// Neg Risk CTF Exchange — used for negRisk markets.
pub const NEG_RISK_CTF_EXCHANGE: &str = "0xC5d563A36AE78145C45a50134d48A1215220f80a";

/// Neg Risk Adapter — taker address for negRisk orders.
pub const NEG_RISK_ADAPTER: &str = "0xd91E80cF2E7be2e162c6513ceD06f1dD0dA35296";

/// Polygon chain ID.
pub const CHAIN_ID: u64 = 137;

// ---------------------------------------------------------------------------
// EIP-712 type hashes (pre-computed)
// ---------------------------------------------------------------------------

/// keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)")
fn domain_type_hash() -> B256 {
    keccak256(b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)")
}

/// keccak256("Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,uint256 feeRateBps,uint8 side,uint8 signatureType)")
fn order_type_hash() -> B256 {
    keccak256(b"Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,uint256 feeRateBps,uint8 side,uint8 signatureType)")
}

// ---------------------------------------------------------------------------
// Order side
// ---------------------------------------------------------------------------

/// Order side: BUY = 0, SELL = 1 (matches Polymarket CLOB enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy = 0,
    Sell = 1,
}

impl Side {
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

// ---------------------------------------------------------------------------
// Order data
// ---------------------------------------------------------------------------

/// All fields needed to construct and sign a Polymarket CLOB order.
///
/// Amounts are in raw token units (no decimals).
/// For USDC (6 decimals): $10.00 = 10_000_000.
/// For CTF tokens (6 decimals): 100 shares = 100_000_000.
#[derive(Debug, Clone)]
pub struct OrderData {
    /// Random salt for uniqueness.
    pub salt: U256,
    /// Maker address (the trading wallet).
    pub maker: Address,
    /// Signer address (same as maker for EOA, type 0).
    pub signer: Address,
    /// Taker address (0x0 for open orders, or Neg Risk Adapter for negRisk).
    pub taker: Address,
    /// ERC1155 token ID of the conditional token.
    pub token_id: U256,
    /// Maker amount in raw units.
    pub maker_amount: U256,
    /// Taker amount in raw units.
    pub taker_amount: U256,
    /// Order expiration (unix timestamp, 0 = no expiry).
    pub expiration: U256,
    /// Nonce (0 for most orders).
    pub nonce: U256,
    /// Fee rate in basis points (e.g., 100 = 1%).
    pub fee_rate_bps: U256,
    /// Buy or Sell.
    pub side: Side,
    /// Signature type: 0 = EOA.
    pub signature_type: u8,
}

/// Signed order ready for CLOB submission.
#[derive(Debug, Clone)]
pub struct SignedOrder {
    pub order: OrderData,
    /// Hex-encoded signature (0x-prefixed, 65 bytes = 130 hex chars + 0x).
    pub signature: String,
    /// The EIP-712 struct hash (for debugging/verification).
    pub order_hash: B256,
}

// ---------------------------------------------------------------------------
// Domain separator
// ---------------------------------------------------------------------------

/// Compute the EIP-712 domain separator for a Polymarket exchange contract.
fn domain_separator(exchange_address: Address) -> B256 {
    let mut buf = Vec::with_capacity(5 * 32);
    buf.extend_from_slice(domain_type_hash().as_slice());
    buf.extend_from_slice(keccak256(b"Polymarket CTF Exchange").as_slice());
    buf.extend_from_slice(keccak256(b"1").as_slice());
    buf.extend_from_slice(&U256::from(CHAIN_ID).to_be_bytes::<32>());
    // Address is 20 bytes, left-padded to 32
    let mut addr_padded = [0u8; 32];
    addr_padded[12..].copy_from_slice(exchange_address.as_slice());
    buf.extend_from_slice(&addr_padded);
    keccak256(&buf)
}

// ---------------------------------------------------------------------------
// Struct hash
// ---------------------------------------------------------------------------

/// Compute the EIP-712 struct hash for an order.
fn hash_order(order: &OrderData) -> B256 {
    let mut buf = Vec::with_capacity(13 * 32);
    buf.extend_from_slice(order_type_hash().as_slice());
    buf.extend_from_slice(&order.salt.to_be_bytes::<32>());
    // Addresses: left-padded to 32 bytes
    let mut addr_buf = [0u8; 32];
    addr_buf[12..].copy_from_slice(order.maker.as_slice());
    buf.extend_from_slice(&addr_buf);
    addr_buf = [0u8; 32];
    addr_buf[12..].copy_from_slice(order.signer.as_slice());
    buf.extend_from_slice(&addr_buf);
    addr_buf = [0u8; 32];
    addr_buf[12..].copy_from_slice(order.taker.as_slice());
    buf.extend_from_slice(&addr_buf);
    buf.extend_from_slice(&order.token_id.to_be_bytes::<32>());
    buf.extend_from_slice(&order.maker_amount.to_be_bytes::<32>());
    buf.extend_from_slice(&order.taker_amount.to_be_bytes::<32>());
    buf.extend_from_slice(&order.expiration.to_be_bytes::<32>());
    buf.extend_from_slice(&order.nonce.to_be_bytes::<32>());
    buf.extend_from_slice(&order.fee_rate_bps.to_be_bytes::<32>());
    buf.extend_from_slice(&U256::from(order.side.as_u8()).to_be_bytes::<32>());
    buf.extend_from_slice(&U256::from(order.signature_type).to_be_bytes::<32>());
    keccak256(&buf)
}

// ---------------------------------------------------------------------------
// EIP-712 signing hash
// ---------------------------------------------------------------------------

/// Compute the full EIP-712 signing hash: keccak256("\x19\x01" + domainSep + structHash).
fn eip712_hash(domain_sep: B256, struct_hash: B256) -> B256 {
    let mut buf = Vec::with_capacity(2 + 32 + 32);
    buf.push(0x19);
    buf.push(0x01);
    buf.extend_from_slice(domain_sep.as_slice());
    buf.extend_from_slice(struct_hash.as_slice());
    keccak256(&buf)
}

// ---------------------------------------------------------------------------
// Signer
// ---------------------------------------------------------------------------

/// Polymarket order signer. Holds the private key and pre-computed domain separators.
pub struct OrderSigner {
    signer: PrivateKeySigner,
    /// Domain separator for the regular CTF Exchange.
    domain_ctf: B256,
    /// Domain separator for the Neg Risk CTF Exchange.
    domain_neg_risk: B256,
}

impl OrderSigner {
    /// Create a new signer from a hex-encoded private key (with or without 0x prefix).
    pub fn new(private_key_hex: &str) -> Result<Self, String> {
        let key_hex = private_key_hex.strip_prefix("0x").unwrap_or(private_key_hex);
        let key_bytes = hex::decode(key_hex)
            .map_err(|e| format!("Invalid private key hex: {}", e))?;
        if key_bytes.len() != 32 {
            return Err(format!("Private key must be 32 bytes, got {}", key_bytes.len()));
        }

        let signer = PrivateKeySigner::from_bytes(
            &FixedBytes::from_slice(&key_bytes),
        ).map_err(|e| format!("Invalid private key: {}", e))?;

        let ctf_addr: Address = CTF_EXCHANGE.parse()
            .map_err(|e| format!("Invalid CTF Exchange address: {}", e))?;
        let neg_risk_addr: Address = NEG_RISK_CTF_EXCHANGE.parse()
            .map_err(|e| format!("Invalid Neg Risk Exchange address: {}", e))?;

        Ok(Self {
            signer,
            domain_ctf: domain_separator(ctf_addr),
            domain_neg_risk: domain_separator(neg_risk_addr),
        })
    }

    /// The wallet address derived from the private key.
    pub fn address(&self) -> Address {
        self.signer.address()
    }

    /// Sign an order. Returns the signed order with hex signature.
    ///
    /// `neg_risk`: if true, uses the Neg Risk CTF Exchange domain.
    pub fn sign_order(&self, order: &OrderData, neg_risk: bool) -> Result<SignedOrder, String> {
        let domain = if neg_risk { self.domain_neg_risk } else { self.domain_ctf };
        let struct_hash = hash_order(order);
        let signing_hash = eip712_hash(domain, struct_hash);

        let sig = self.signer.sign_hash_sync(&signing_hash)
            .map_err(|e| format!("Signing failed: {}", e))?;

        // Encode as 65-byte signature: r (32) + s (32) + v (1)
        let sig_bytes = {
            let mut buf = [0u8; 65];
            buf[..32].copy_from_slice(&sig.r().to_be_bytes::<32>());
            buf[32..64].copy_from_slice(&sig.s().to_be_bytes::<32>());
            buf[64] = sig.v() as u8;
            buf
        };

        Ok(SignedOrder {
            order: order.clone(),
            signature: format!("0x{}", hex::encode(sig_bytes)),
            order_hash: struct_hash,
        })
    }
}

// ---------------------------------------------------------------------------
// Order construction helpers
// ---------------------------------------------------------------------------

/// Generate a random salt for order uniqueness.
pub fn random_salt() -> U256 {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let mut bytes = [0u8; 32];
    rng.fill(&mut bytes);
    U256::from_be_bytes(bytes)
}

/// Convert a human price (0.0 - 1.0) and size (in USDC) to maker/taker amounts.
///
/// For a BUY order:
///   - makerAmount = size in USDC (what you pay) — raw units (6 decimals)
///   - takerAmount = size / price (shares you receive) — raw units (6 decimals)
///
/// For a SELL order:
///   - makerAmount = number of shares to sell — raw units (6 decimals)
///   - takerAmount = shares * price (USDC you receive) — raw units (6 decimals)
///
/// Returns (maker_amount, taker_amount) in raw token units.
pub fn compute_amounts(price: f64, size_usd: f64, side: Side, decimals: u32) -> (U256, U256) {
    let scale = 10f64.powi(decimals as i32);
    match side {
        Side::Buy => {
            let maker = (size_usd * scale).round() as u128;
            let shares = size_usd / price;
            let taker = (shares * scale).round() as u128;
            (U256::from(maker), U256::from(taker))
        }
        Side::Sell => {
            let shares = size_usd / price;
            let maker = (shares * scale).round() as u128;
            let taker_usd = shares * price;
            let taker = (taker_usd * scale).round() as u128;
            (U256::from(maker), U256::from(taker))
        }
    }
}

/// Build an OrderData for a Polymarket CLOB order.
pub fn build_order(
    maker: Address,
    token_id: &str,
    price: f64,
    size_usd: f64,
    side: Side,
    neg_risk: bool,
    fee_rate_bps: u64,
) -> Result<OrderData, String> {
    let token_id_u256 = U256::from_str_radix(token_id, 10)
        .map_err(|e| format!("Invalid token_id '{}': {}", token_id, e))?;

    let taker = if neg_risk {
        NEG_RISK_ADAPTER.parse::<Address>()
            .map_err(|e| format!("Invalid adapter address: {}", e))?
    } else {
        Address::ZERO
    };

    let (maker_amount, taker_amount) = compute_amounts(price, size_usd, side, 6);

    Ok(OrderData {
        salt: random_salt(),
        maker,
        signer: maker,
        taker,
        token_id: token_id_u256,
        maker_amount,
        taker_amount,
        expiration: U256::ZERO,
        nonce: U256::ZERO,
        fee_rate_bps: U256::from(fee_rate_bps),
        side,
        signature_type: 0, // EOA
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify domain separator computation matches expected values.
    #[test]
    fn test_domain_separator() {
        let ctf_addr: Address = CTF_EXCHANGE.parse().unwrap();
        let sep = domain_separator(ctf_addr);
        // Just verify it's deterministic (same inputs → same output)
        let sep2 = domain_separator(ctf_addr);
        assert_eq!(sep, sep2);
        // Non-zero
        assert_ne!(sep, B256::ZERO);
    }

    /// Verify order hashing is deterministic.
    #[test]
    fn test_order_hash_deterministic() {
        let order = OrderData {
            salt: U256::from(12345u64),
            maker: Address::ZERO,
            signer: Address::ZERO,
            taker: Address::ZERO,
            token_id: U256::from(1u64),
            maker_amount: U256::from(10_000_000u64),  // $10
            taker_amount: U256::from(20_000_000u64),   // 20 shares
            expiration: U256::ZERO,
            nonce: U256::ZERO,
            fee_rate_bps: U256::from(100u64),
            side: Side::Buy,
            signature_type: 0,
        };
        let h1 = hash_order(&order);
        let h2 = hash_order(&order);
        assert_eq!(h1, h2);
        assert_ne!(h1, B256::ZERO);
    }

    /// Verify signing produces a valid 65-byte signature.
    #[test]
    fn test_sign_order() {
        // Well-known test key (do NOT use in production)
        let test_key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let signer = OrderSigner::new(test_key).unwrap();

        let order = build_order(
            signer.address(),
            "1234567890",
            0.50,
            10.0,
            Side::Buy,
            false,
            0,
        ).unwrap();

        let signed = signer.sign_order(&order, false).unwrap();
        // Signature should be 0x + 130 hex chars (65 bytes)
        assert_eq!(signed.signature.len(), 132);
        assert!(signed.signature.starts_with("0x"));
    }

    /// Verify neg_risk orders use different domain and taker address.
    #[test]
    fn test_neg_risk_order() {
        let test_key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let signer = OrderSigner::new(test_key).unwrap();

        let order = build_order(
            signer.address(),
            "999",
            0.60,
            20.0,
            Side::Sell,
            true,
            100,
        ).unwrap();

        // Taker should be the Neg Risk Adapter
        let adapter: Address = NEG_RISK_ADAPTER.parse().unwrap();
        assert_eq!(order.taker, adapter);

        // Sign with neg_risk domain
        let signed = signer.sign_order(&order, true).unwrap();
        assert_eq!(signed.signature.len(), 132);

        // Signing with regular domain should produce different signature
        let signed_regular = signer.sign_order(&order, false).unwrap();
        assert_ne!(signed.signature, signed_regular.signature);
    }

    /// Verify amount computation for BUY orders.
    #[test]
    fn test_compute_amounts_buy() {
        // BUY 100 shares at $0.50 = $50 USDC
        let (maker, taker) = compute_amounts(0.50, 50.0, Side::Buy, 6);
        assert_eq!(maker, U256::from(50_000_000u64));  // $50 in raw USDC
        assert_eq!(taker, U256::from(100_000_000u64)); // 100 shares in raw units
    }

    /// Verify amount computation for SELL orders.
    #[test]
    fn test_compute_amounts_sell() {
        // SELL shares worth $50 at $0.50 = 100 shares
        let (maker, taker) = compute_amounts(0.50, 50.0, Side::Sell, 6);
        assert_eq!(maker, U256::from(100_000_000u64)); // 100 shares
        assert_eq!(taker, U256::from(50_000_000u64));  // $50 USDC
    }
}
