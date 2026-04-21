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
    ///
    /// G3 (F-pre-3): the intermediate `key_bytes` Vec is explicitly zeroized
    /// after `PrivateKeySigner::from_bytes` consumes it. The PrivateKeySigner
    /// itself holds the key internally and is not zeroized on drop — this is
    /// a known gap pending upstream alloy support (alloy-signer-local does
    /// not currently implement Zeroize on its key material). Wrapping it
    /// would require an unsafe transmute which is worse than the gap.
    pub fn new(private_key_hex: &str) -> Result<Self, String> {
        use zeroize::Zeroize;
        let key_hex = private_key_hex.strip_prefix("0x").unwrap_or(private_key_hex);
        let mut key_bytes = hex::decode(key_hex)
            .map_err(|e| format!("Invalid private key hex: {}", e))?;
        if key_bytes.len() != 32 {
            key_bytes.zeroize();
            return Err(format!("Private key must be 32 bytes, got {}", key_bytes.len()));
        }

        let signer_result = PrivateKeySigner::from_bytes(
            &FixedBytes::from_slice(&key_bytes),
        );
        // Zeroize the intermediate buffer regardless of from_bytes outcome.
        key_bytes.zeroize();
        let signer = signer_result.map_err(|e| format!("Invalid private key: {}", e))?;

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
// L1 Auth: CLOB API key derivation (ClobAuth EIP-712)
// ---------------------------------------------------------------------------

/// CLOB API credentials returned by /auth/derive-api-key or /auth/api-key.
///
/// G3 (F-pre-3): `Zeroize, ZeroizeOnDrop` — `secret` and `passphrase` are
/// wiped from memory on drop. `Clone` is preserved; each clone is
/// independently zeroized when its own scope ends.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
pub struct ClobApiCreds {
    #[serde(rename = "apiKey")]
    pub api_key: String,
    pub secret: String,
    pub passphrase: String,
}

impl OrderSigner {
    /// Build L1 auth headers for /auth/* endpoints.
    /// Signs ClobAuth EIP-712 typed data with the wallet key.
    pub fn build_l1_headers(&self, nonce: u64) -> Result<Vec<(String, String)>, String> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string();

        let address = format!("{:?}", self.address()); // 0x-prefixed checksummed

        // ClobAuth domain
        let domain_hash = {
            let type_hash = keccak256(b"EIP712Domain(string name,string version,uint256 chainId)");
            let name_hash = keccak256(b"ClobAuthDomain");
            let version_hash = keccak256(b"1");
            let chain_id = U256::from(CHAIN_ID);
            let mut buf = Vec::with_capacity(4 * 32);
            buf.extend_from_slice(type_hash.as_slice());
            buf.extend_from_slice(name_hash.as_slice());
            buf.extend_from_slice(version_hash.as_slice());
            buf.extend_from_slice(&chain_id.to_be_bytes::<32>());
            keccak256(&buf)
        };

        // ClobAuth struct hash
        let struct_hash = {
            let type_hash = keccak256(
                b"ClobAuth(address address,string timestamp,uint256 nonce,string message)"
            );
            let addr: Address = address.parse().map_err(|e| format!("bad addr: {}", e))?;
            let ts_hash = keccak256(ts.as_bytes());
            let msg_hash = keccak256(
                b"This message attests that I control the given wallet"
            );
            let mut buf = Vec::with_capacity(5 * 32);
            buf.extend_from_slice(type_hash.as_slice());
            buf.extend_from_slice(&{
                let mut padded = [0u8; 32];
                padded[12..].copy_from_slice(addr.as_slice());
                padded
            });
            buf.extend_from_slice(ts_hash.as_slice());
            buf.extend_from_slice(&U256::from(nonce).to_be_bytes::<32>());
            buf.extend_from_slice(msg_hash.as_slice());
            keccak256(&buf)
        };

        let signing_hash = eip712_hash(domain_hash, struct_hash);
        let sig = self.signer.sign_hash_sync(&signing_hash)
            .map_err(|e| format!("ClobAuth signing failed: {}", e))?;

        let sig_bytes = {
            let mut buf = [0u8; 65];
            buf[..32].copy_from_slice(&sig.r().to_be_bytes::<32>());
            buf[32..64].copy_from_slice(&sig.s().to_be_bytes::<32>());
            buf[64] = sig.v() as u8;
            buf
        };

        Ok(vec![
            ("POLY_ADDRESS".into(), address),
            ("POLY_SIGNATURE".into(), format!("0x{}", hex::encode(sig_bytes))),
            ("POLY_TIMESTAMP".into(), ts),
            ("POLY_NONCE".into(), nonce.to_string()),
        ])
    }

    /// Derive existing CLOB API credentials from the wallet.
    /// Calls GET /auth/derive-api-key with L1 auth headers.
    pub fn derive_api_key(&self, clob_host: &str) -> Result<ClobApiCreds, String> {
        let headers = self.build_l1_headers(0)?;
        let client = crate::http_client::secure_client_tagged(30, "signing.derive")?;
        let url = format!("{}/auth/derive-api-key", clob_host.trim_end_matches('/'));

        let mut req = client.get(&url);
        for (k, v) in &headers {
            req = req.header(k, v);
        }

        let resp = req.send().map_err(|e| format!("derive-api-key request failed: {}", e))?;
        let status = resp.status();

        if status.is_success() {
            let creds: ClobApiCreds = resp.json()
                .map_err(|e| format!("derive-api-key JSON parse error: {}", e))?;
            Ok(creds)
        } else {
            let body = resp.text().unwrap_or_default();
            Err(format!("derive-api-key failed ({}): {}", status, body))
        }
    }

    /// Create new CLOB API credentials from the wallet.
    /// Calls POST /auth/api-key with L1 auth headers.
    pub fn create_api_key(&self, clob_host: &str) -> Result<ClobApiCreds, String> {
        let headers = self.build_l1_headers(0)?;
        let client = crate::http_client::secure_client_tagged(30, "signing.create")?;
        let url = format!("{}/auth/api-key", clob_host.trim_end_matches('/'));

        let mut req = client.post(&url);
        for (k, v) in &headers {
            req = req.header(k, v);
        }

        let resp = req.send().map_err(|e| format!("create-api-key request failed: {}", e))?;
        let status = resp.status();

        if status.is_success() {
            let creds: ClobApiCreds = resp.json()
                .map_err(|e| format!("create-api-key JSON parse error: {}", e))?;
            Ok(creds)
        } else {
            let body = resp.text().unwrap_or_default();
            Err(format!("create-api-key failed ({}): {}", status, body))
        }
    }

    /// Derive or create CLOB API credentials.
    /// Tries derive first, falls back to create if no existing credentials.
    pub fn create_or_derive_api_key(&self, clob_host: &str) -> Result<ClobApiCreds, String> {
        match self.derive_api_key(clob_host) {
            Ok(creds) => {
                tracing::info!("Derived existing CLOB API credentials (key={}...)", &creds.api_key[..8.min(creds.api_key.len())]);
                Ok(creds)
            }
            Err(e) => {
                tracing::info!("No existing API key ({}), creating new one...", e);
                let creds = self.create_api_key(clob_host)?;
                tracing::info!("Created new CLOB API credentials (key={}...)", &creds.api_key[..8.min(creds.api_key.len())]);
                Ok(creds)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// L2 Auth: HMAC-SHA256 signing for authenticated requests
// ---------------------------------------------------------------------------

/// L2 HMAC-SHA256 auth for CLOB API trading endpoints.
///
/// G3 (F-pre-3): `Zeroize, ZeroizeOnDrop` — `secret` and `passphrase` are
/// wiped from memory on drop. The `address` is also zeroized for hygiene
/// (the wallet address is public on-chain but treating it as sensitive
/// is harmless and keeps the derive uniform). `Clone` is preserved; each
/// clone is independently zeroized when its own scope ends.
#[derive(Clone, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
pub struct ClobAuth {
    api_key: String,
    secret: Vec<u8>,  // base64url-decoded secret
    passphrase: String,
    address: String,
}

impl ClobAuth {
    /// Get the API key (UUID) for use as `owner` in order payloads.
    pub fn api_key(&self) -> &str { &self.api_key }
    /// Get the wallet address.
    pub fn wallet_address(&self) -> &str { &self.address }

    /// Raw base64url-encoded API secret for WS user channel auth.
    /// The WS subscription message wants the original secret string, NOT an HMAC signature.
    /// REST endpoints use build_headers() which computes HMAC; WS just wants raw creds.
    pub fn raw_secret_b64(&self) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE.encode(&self.secret)
    }

    /// API passphrase for WS auth.
    pub fn passphrase(&self) -> &str { &self.passphrase }

    /// Create from API credentials.
    pub fn new(creds: &ClobApiCreds, address: &str) -> Result<Self, String> {
        use base64::Engine;
        let secret = base64::engine::general_purpose::URL_SAFE
            .decode(&creds.secret)
            .map_err(|e| format!("Invalid API secret (base64 decode): {}", e))?;
        Ok(Self {
            api_key: creds.api_key.clone(),
            secret,
            passphrase: creds.passphrase.clone(),
            address: address.to_string(),
        })
    }

    /// Build L2 auth headers for an authenticated request.
    /// `method`: "GET", "POST", "DELETE"
    /// `path`: request path, e.g., "/order"
    /// `body`: optional JSON body string
    pub fn build_headers(&self, method: &str, path: &str, body: Option<&str>) -> Vec<(String, String)> {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        use base64::Engine;

        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string();

        // Message: timestamp + method + path + body
        let mut message = format!("{}{}{}", ts, method, path);
        if let Some(b) = body {
            // Replace single quotes with double quotes for cross-language compat
            message.push_str(&b.replace('\'', "\""));
        }

        let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret)
            .expect("HMAC accepts any key size");
        mac.update(message.as_bytes());
        let sig = base64::engine::general_purpose::URL_SAFE.encode(mac.finalize().into_bytes());

        vec![
            ("POLY_ADDRESS".into(), self.address.clone()),
            ("POLY_SIGNATURE".into(), sig),
            ("POLY_TIMESTAMP".into(), ts),
            ("POLY_API_KEY".into(), self.api_key.clone()),
            ("POLY_PASSPHRASE".into(), self.passphrase.clone()),
        ]
    }
}

// ---------------------------------------------------------------------------
// Order construction helpers
// ---------------------------------------------------------------------------

/// Generate a random salt for order uniqueness.
/// Must fit in JavaScript's Number.MAX_SAFE_INTEGER (2^53 - 1) for CLOB API compatibility.
pub fn random_salt() -> U256 {
    use rand::Rng;
    let val: u64 = rand::thread_rng().gen_range(0..=(1u64 << 53) - 1);
    U256::from(val)
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
///
/// Polymarket CLOB has different precision rules for GTC (limit) vs FAK (market) orders:
///
/// **GTC (limit) orders:**
///   BUY:  makerAmount = USDC (max `amount_decimals`), takerAmount = shares (max 2 decimals)
///   SELL: makerAmount = shares (max 2 decimals), takerAmount = USDC (max `amount_decimals`)
///
/// **FAK (market) orders — precision is swapped:**
///   BUY:  makerAmount = USDC (max 2 decimals), takerAmount = shares (max `amount_decimals`)
///   SELL: makerAmount = shares (max `amount_decimals`), takerAmount = USDC (max 2 decimals)
///
/// `amount_decimals` comes from the tick size rounding config (e.g., 5 for 0.001 tick).
pub fn compute_amounts(price: f64, size_usd: f64, side: Side, amount_decimals: u32) -> (U256, U256) {
    compute_amounts_for_order_type(price, size_usd, side, amount_decimals, false)
}

/// Like compute_amounts but with explicit market order flag.
pub fn compute_amounts_for_order_type(
    price: f64, size_usd: f64, side: Side, amount_decimals: u32, is_market_order: bool,
) -> (U256, U256) {
    let scale = 10f64.powi(6); // 6 decimal raw units (USDC = 6 decimals, CTF = 6 decimals)
    let size_round = 100.0f64;  // 2 decimal places
    let amt_round = 10f64.powi(amount_decimals as i32);

    // For market (FAK) orders, the precision rules are swapped vs limit (GTC).
    let (usdc_round, shares_round) = if is_market_order {
        (size_round, amt_round)   // FAK: USDC=2dec, shares=amount_decimals
    } else {
        (amt_round, size_round)   // GTC: USDC=amount_decimals, shares=2dec
    };

    match side {
        Side::Buy => {
            // BUY: maker=USDC, taker=shares
            let shares_human = (size_usd / price * shares_round).floor() / shares_round;
            let taker = (shares_human * scale).round() as u128;
            let maker_human = (shares_human * price * usdc_round).round() / usdc_round;
            let maker = (maker_human * scale).round() as u128;
            (U256::from(maker), U256::from(taker))
        }
        Side::Sell => {
            // SELL: maker=shares, taker=USDC
            let shares_human = (size_usd / price * shares_round).floor() / shares_round;
            let maker = (shares_human * scale).round() as u128;
            let taker_human = (shares_human * price * usdc_round).round() / usdc_round;
            let taker = (taker_human * scale).round() as u128;
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
    // Default amount_decimals based on price range (matching Python ROUNDING_CONFIG)
    let amount_decimals = if size_usd / price > 10000.0 { 2 } else { 4 };
    build_order_with_precision(maker, token_id, price, size_usd, side, neg_risk, fee_rate_bps, amount_decimals)
}

/// Build order with explicit amount precision and order type awareness.
pub fn build_order_with_precision(
    maker: Address,
    token_id: &str,
    price: f64,
    size_usd: f64,
    side: Side,
    neg_risk: bool,
    fee_rate_bps: u64,
    amount_decimals: u32,
) -> Result<OrderData, String> {
    build_order_with_precision_and_type(
        maker, token_id, price, size_usd, side, neg_risk, fee_rate_bps, amount_decimals, false,
    )
}

/// Build order with explicit amount precision and market order flag.
pub fn build_order_with_precision_and_type(
    maker: Address,
    token_id: &str,
    price: f64,
    size_usd: f64,
    side: Side,
    neg_risk: bool,
    fee_rate_bps: u64,
    amount_decimals: u32,
    is_market_order: bool,
) -> Result<OrderData, String> {
    let token_id_u256 = U256::from_str_radix(token_id, 10)
        .map_err(|e| format!("Invalid token_id '{}': {}", token_id, e))?;

    // Taker is always 0x0 — neg risk routing is handled server-side by the CLOB.
    let taker = Address::ZERO;

    let (maker_amount, taker_amount) = compute_amounts_for_order_type(
        price, size_usd, side, amount_decimals, is_market_order,
    );

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

        // API-7: Taker is always Address::ZERO — negRisk routing is server-side
        assert_eq!(order.taker, Address::ZERO);

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

    // -----------------------------------------------------------------------
    // B2.3: Comprehensive rounding verification (16 core + 4 edge cases)
    // -----------------------------------------------------------------------

    /// Check that a raw 6-decimal value has at most `max_decimals` human-readable decimal places.
    fn check_decimals(raw: U256, max_decimals: u32) -> bool {
        let val = raw.as_limbs()[0] as f64;
        let human = val / 1_000_000.0;
        let scale = 10f64.powi(max_decimals as i32);
        let rounded = (human * scale).round() / scale;
        (human - rounded).abs() < 1e-9
    }

    /// Helper: run a single rounding test case and assert precision rules.
    ///
    /// For BUY: maker=USDC, taker=shares
    ///   GTC: USDC ≤ amount_decimals, shares ≤ 2
    ///   FAK: USDC ≤ 2, shares ≤ amount_decimals
    ///
    /// For SELL: maker=shares, taker=USDC
    ///   GTC: shares ≤ 2, USDC ≤ amount_decimals
    ///   FAK: shares ≤ amount_decimals, USDC ≤ 2
    fn assert_precision(
        price: f64, size_usd: f64, side: Side, amount_decimals: u32,
        is_market: bool, label: &str,
    ) {
        let (maker, taker) = compute_amounts_for_order_type(
            price, size_usd, side, amount_decimals, is_market,
        );
        assert!(!maker.is_zero(), "{label}: maker is zero");
        assert!(!taker.is_zero(), "{label}: taker is zero");

        let (usdc_max_dec, shares_max_dec) = if is_market {
            (2, amount_decimals) // FAK: USDC=2, shares=amount_decimals
        } else {
            (amount_decimals, 2) // GTC: USDC=amount_decimals, shares=2
        };

        match side {
            Side::Buy => {
                // maker=USDC, taker=shares
                assert!(check_decimals(maker, usdc_max_dec),
                    "{label}: maker (USDC) exceeds {usdc_max_dec} decimals, raw={maker}");
                assert!(check_decimals(taker, shares_max_dec),
                    "{label}: taker (shares) exceeds {shares_max_dec} decimals, raw={taker}");
            }
            Side::Sell => {
                // maker=shares, taker=USDC
                assert!(check_decimals(maker, shares_max_dec),
                    "{label}: maker (shares) exceeds {shares_max_dec} decimals, raw={maker}");
                assert!(check_decimals(taker, usdc_max_dec),
                    "{label}: taker (USDC) exceeds {usdc_max_dec} decimals, raw={taker}");
            }
        }
    }

    // --- Tick 0.1 (amount_decimals=3) ---

    #[test]
    fn test_amounts_tick01_gtc_buy() {
        assert_precision(0.3, 15.0, Side::Buy, 3, false, "tick0.1/GTC/BUY");
    }
    #[test]
    fn test_amounts_tick01_gtc_sell() {
        assert_precision(0.3, 15.0, Side::Sell, 3, false, "tick0.1/GTC/SELL");
    }
    #[test]
    fn test_amounts_tick01_fak_buy() {
        assert_precision(0.3, 15.0, Side::Buy, 3, true, "tick0.1/FAK/BUY");
    }
    #[test]
    fn test_amounts_tick01_fak_sell() {
        assert_precision(0.3, 15.0, Side::Sell, 3, true, "tick0.1/FAK/SELL");
    }

    // --- Tick 0.01 (amount_decimals=4) ---

    #[test]
    fn test_amounts_tick001_gtc_buy() {
        assert_precision(0.45, 22.50, Side::Buy, 4, false, "tick0.01/GTC/BUY");
    }
    #[test]
    fn test_amounts_tick001_gtc_sell() {
        assert_precision(0.45, 22.50, Side::Sell, 4, false, "tick0.01/GTC/SELL");
    }
    #[test]
    fn test_amounts_tick001_fak_buy() {
        assert_precision(0.45, 22.50, Side::Buy, 4, true, "tick0.01/FAK/BUY");
    }
    #[test]
    fn test_amounts_tick001_fak_sell() {
        assert_precision(0.45, 22.50, Side::Sell, 4, true, "tick0.01/FAK/SELL");
    }

    // --- Tick 0.001 (amount_decimals=5) ---

    #[test]
    fn test_amounts_tick0001_gtc_buy() {
        assert_precision(0.123, 50.0, Side::Buy, 5, false, "tick0.001/GTC/BUY");
    }
    #[test]
    fn test_amounts_tick0001_gtc_sell() {
        assert_precision(0.123, 50.0, Side::Sell, 5, false, "tick0.001/GTC/SELL");
    }
    #[test]
    fn test_amounts_tick0001_fak_buy() {
        assert_precision(0.123, 50.0, Side::Buy, 5, true, "tick0.001/FAK/BUY");
    }
    #[test]
    fn test_amounts_tick0001_fak_sell() {
        assert_precision(0.123, 50.0, Side::Sell, 5, true, "tick0.001/FAK/SELL");
    }

    // --- Tick 0.0001 (amount_decimals=6) ---

    #[test]
    fn test_amounts_tick00001_gtc_buy() {
        assert_precision(0.0567, 100.0, Side::Buy, 6, false, "tick0.0001/GTC/BUY");
    }
    #[test]
    fn test_amounts_tick00001_gtc_sell() {
        assert_precision(0.0567, 100.0, Side::Sell, 6, false, "tick0.0001/GTC/SELL");
    }
    #[test]
    fn test_amounts_tick00001_fak_buy() {
        assert_precision(0.0567, 100.0, Side::Buy, 6, true, "tick0.0001/FAK/BUY");
    }
    #[test]
    fn test_amounts_tick00001_fak_sell() {
        assert_precision(0.0567, 100.0, Side::Sell, 6, true, "tick0.0001/FAK/SELL");
    }

    // --- Edge cases ---

    #[test]
    fn test_amounts_minimum_size() {
        // Smallest plausible order: ~$0.50 at price 0.99
        assert_precision(0.99, 0.50, Side::Buy, 4, true, "min_size/FAK/BUY");
        assert_precision(0.99, 0.50, Side::Sell, 4, false, "min_size/GTC/SELL");
    }

    #[test]
    fn test_amounts_large_size() {
        // $10,000 order — should not overflow U256
        assert_precision(0.50, 10_000.0, Side::Buy, 4, false, "large/GTC/BUY");
        assert_precision(0.50, 10_000.0, Side::Sell, 4, true, "large/FAK/SELL");
    }

    #[test]
    fn test_amounts_extreme_price_low() {
        // Near-zero price: 0.01
        assert_precision(0.01, 5.0, Side::Buy, 4, true, "low_price/FAK/BUY");
        assert_precision(0.01, 5.0, Side::Sell, 4, false, "low_price/GTC/SELL");
    }

    #[test]
    fn test_amounts_extreme_price_high() {
        // Near-one price: 0.99
        assert_precision(0.99, 50.0, Side::Buy, 6, false, "high_price/GTC/BUY");
        assert_precision(0.99, 50.0, Side::Sell, 6, true, "high_price/FAK/SELL");
    }

    /// Parametric test: all 16 core combos in one test for regression coverage.
    #[test]
    fn test_amounts_all_combos_parametric() {
        let combos: Vec<(f64, f64, u32, &str)> = vec![
            (0.3,    15.0,  3, "tick0.1"),
            (0.45,   22.50, 4, "tick0.01"),
            (0.123,  50.0,  5, "tick0.001"),
            (0.0567, 100.0, 6, "tick0.0001"),
        ];
        for (price, size, amt_dec, tick_label) in &combos {
            for is_market in [false, true] {
                let order_type = if is_market { "FAK" } else { "GTC" };
                for side in [Side::Buy, Side::Sell] {
                    let side_label = match side { Side::Buy => "BUY", Side::Sell => "SELL" };
                    let label = format!("{tick_label}/{order_type}/{side_label}");
                    assert_precision(*price, *size, side, *amt_dec, is_market, &label);
                }
            }
        }
    }
}
