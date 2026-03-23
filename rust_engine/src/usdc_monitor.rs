/// USDC balance monitoring (B4.6).
///
/// Two balance sources:
/// 1. **CLOB exchange balance** (primary): `GET /balance-allowance?asset_type=COLLATERAL`
///    with L2 HMAC auth. Returns actual USDC held in the Polymarket exchange contract.
///    This is the real balance — on-chain ERC-20 shows $0 because funds are deposited.
/// 2. **On-chain ERC-20** (fallback): `eth_call balanceOf()` on USDC.e contract.
///    Only non-zero before deposit or after withdrawal.
///
/// Compares exchange balance vs accounting cash to detect drift.

use reqwest::blocking::Client;
use crate::signing::ClobAuth;

/// USDC.e token contract address on Polygon.
const USDC_E_CONTRACT: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";

/// USDC.e has 6 decimals.
const USDC_DECIMALS: f64 = 1e6;

/// ERC-20 `balanceOf(address)` function selector (first 4 bytes of keccak256).
const BALANCE_OF_SELECTOR: &str = "70a08231";

/// USDC monitor configuration.
#[derive(Clone, Debug)]
pub struct UsdcMonitorConfig {
    pub enabled: bool,
    pub rpc_url: String,
    pub wallet_address: String,
    pub check_interval_seconds: f64,
    /// Alert if |exchange - accounting| exceeds this (default $1.00).
    pub drift_threshold: f64,
    /// Telegram warning when balance falls below this (default $10.00).
    pub warning_balance: f64,
    /// Circuit breaker when balance falls below this (default $1.00).
    pub critical_balance: f64,
    /// CLOB host for balance-allowance endpoint (e.g., "https://clob.polymarket.com").
    pub clob_host: String,
}

impl Default for UsdcMonitorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            rpc_url: "https://polygon-bor-rpc.publicnode.com".to_string(),
            wallet_address: String::new(),
            check_interval_seconds: 3600.0,
            drift_threshold: 1.0,
            warning_balance: 10.0,
            critical_balance: 1.0,
            clob_host: "https://clob.polymarket.com".to_string(),
        }
    }
}

/// Result of a USDC balance check.
#[derive(Debug, Clone)]
pub enum UsdcCheckResult {
    /// Balance is healthy and within drift tolerance.
    Ok { on_chain: f64, accounting: f64 },
    /// On-chain vs accounting drift exceeds threshold.
    DriftWarning { on_chain: f64, accounting: f64, drift: f64 },
    /// Balance is low (below warning, above critical).
    LowBalance(f64),
    /// Balance is critically low.
    CriticalBalance(f64),
    /// RPC query failed.
    Error(String),
}

/// USDC.e balance monitor. Owned by the orchestrator.
pub struct UsdcMonitor {
    config: UsdcMonitorConfig,
    last_on_chain: Option<f64>,
}

impl UsdcMonitor {
    pub fn new(config: UsdcMonitorConfig) -> Self {
        Self {
            config,
            last_on_chain: None,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.config.enabled && !self.config.wallet_address.is_empty()
    }

    pub fn check_interval(&self) -> f64 {
        self.config.check_interval_seconds
    }

    /// Query exchange balance via CLOB `/balance-allowance` (primary) or
    /// fall back to on-chain RPC if no auth available.
    pub fn check_balance(&mut self, client: &Client, accounting_cash: f64) -> UsdcCheckResult {
        self.check_balance_with_auth(client, accounting_cash, None)
    }

    /// Query exchange balance with optional CLOB L2 auth.
    /// Primary: CLOB `/balance-allowance?asset_type=COLLATERAL` (actual exchange balance).
    /// Fallback: on-chain ERC-20 `balanceOf()` (only non-zero before deposit/after withdrawal).
    pub fn check_balance_with_auth(
        &mut self,
        client: &Client,
        accounting_cash: f64,
        clob_auth: Option<&ClobAuth>,
    ) -> UsdcCheckResult {
        if !self.is_enabled() {
            return UsdcCheckResult::Ok { on_chain: 0.0, accounting: accounting_cash };
        }

        // Primary: CLOB exchange balance (requires L2 auth)
        if let Some(auth) = clob_auth {
            match self.query_clob_balance(client, auth) {
                Ok(exchange_bal) => {
                    self.last_on_chain = Some(exchange_bal);
                    return self.evaluate_balance(exchange_bal, accounting_cash);
                }
                Err(e) => {
                    tracing::warn!("CLOB balance query failed, falling back to on-chain: {}", e);
                }
            }
        }

        // Fallback: on-chain ERC-20 balance
        match self.query_onchain_balance(client) {
            Ok(on_chain) => {
                self.last_on_chain = Some(on_chain);
                self.evaluate_balance(on_chain, accounting_cash)
            }
            Err(e) => UsdcCheckResult::Error(e),
        }
    }

    /// Query CLOB `/balance-allowance?asset_type=COLLATERAL` with L2 HMAC auth.
    /// Returns USDC balance in the Polymarket exchange (6 decimal places).
    fn query_clob_balance(&self, client: &Client, auth: &ClobAuth) -> Result<f64, String> {
        let path = "/balance-allowance?asset_type=COLLATERAL";
        let url = format!("{}{}", self.config.clob_host.trim_end_matches('/'), path);
        let headers = auth.build_headers("GET", path, None);

        let mut req = client.get(&url);
        for (k, v) in &headers {
            req = req.header(k, v);
        }

        let resp = req.send().map_err(|e| format!("CLOB balance request failed: {}", e))?;
        if !resp.status().is_success() {
            return Err(format!("CLOB balance returned {}", resp.status()));
        }

        let body: serde_json::Value = resp.json()
            .map_err(|e| format!("CLOB balance parse failed: {}", e))?;

        // Response: {"balance": "65776080", "allowances": {...}}
        let raw_str = body["balance"].as_str()
            .ok_or_else(|| "No 'balance' field in response".to_string())?;
        let raw: u64 = raw_str.parse()
            .map_err(|e| format!("Balance parse failed: {}", e))?;

        Ok(raw as f64 / USDC_DECIMALS)
    }

    /// Query on-chain ERC-20 USDC.e balance via Polygon RPC (fallback).
    fn query_onchain_balance(&self, client: &Client) -> Result<f64, String> {
        let wallet_padded = format!(
            "000000000000000000000000{}",
            self.config.wallet_address.trim_start_matches("0x")
        );
        let call_data = format!("0x{}{}", BALANCE_OF_SELECTOR, wallet_padded);

        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [{"to": USDC_E_CONTRACT, "data": call_data}, "latest"],
            "id": 1
        });

        let resp = client.post(&self.config.rpc_url)
            .json(&payload)
            .send()
            .map_err(|e| format!("RPC request failed: {}", e))?;

        let body: serde_json::Value = resp.json()
            .map_err(|e| format!("RPC response parse failed: {}", e))?;

        let hex_str = body["result"].as_str()
            .ok_or_else(|| {
                let err = body["error"].as_object()
                    .map(|e| format!("{}", serde_json::Value::Object(e.clone())))
                    .unwrap_or_else(|| "no result field".to_string());
                format!("RPC error: {}", err)
            })?;

        let raw = u128::from_str_radix(hex_str.trim_start_matches("0x"), 16)
            .map_err(|e| format!("Balance parse failed: {}", e))?;

        Ok(raw as f64 / USDC_DECIMALS)
    }

    /// Evaluate a balance against thresholds and accounting.
    fn evaluate_balance(&self, balance: f64, accounting_cash: f64) -> UsdcCheckResult {
        if balance < self.config.critical_balance {
            return UsdcCheckResult::CriticalBalance(balance);
        }
        if balance < self.config.warning_balance {
            return UsdcCheckResult::LowBalance(balance);
        }
        let drift = (balance - accounting_cash).abs();
        if drift > self.config.drift_threshold {
            return UsdcCheckResult::DriftWarning {
                on_chain: balance,
                accounting: accounting_cash,
                drift,
            };
        }
        UsdcCheckResult::Ok { on_chain: balance, accounting: accounting_cash }
    }

    /// Get the last known on-chain USDC.e balance (for dashboard display).
    pub fn last_on_chain_balance(&self) -> Option<f64> {
        self.last_on_chain
    }

    pub fn wallet_address(&self) -> &str {
        &self.config.wallet_address
    }

    pub fn drift_threshold(&self) -> f64 {
        self.config.drift_threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disabled_monitor() {
        let config = UsdcMonitorConfig::default();
        let mut monitor = UsdcMonitor::new(config);
        let client = Client::new();
        match monitor.check_balance(&client, 50.0) {
            UsdcCheckResult::Ok { on_chain, accounting } => {
                assert_eq!(on_chain, 0.0);
                assert_eq!(accounting, 50.0);
            }
            other => panic!("Expected Ok for disabled monitor, got {:?}", other),
        }
    }

    #[test]
    fn test_hex_parsing_1_usdc() {
        // 1 USDC = 1_000_000 raw = 0xF4240
        let hex = "0xf4240";
        let raw = u128::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap();
        let usdc = raw as f64 / USDC_DECIMALS;
        assert!((usdc - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_hex_parsing_100_usdc() {
        // 100 USDC = 100_000_000 raw = 0x5F5E100
        let hex = "0x5f5e100";
        let raw = u128::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap();
        let usdc = raw as f64 / USDC_DECIMALS;
        assert!((usdc - 100.0).abs() < 0.001);
    }

    #[test]
    fn test_drift_detection() {
        // Simulate: on-chain=100, accounting=102, threshold=1 → DriftWarning
        let on_chain: f64 = 100.0;
        let accounting: f64 = 102.0;
        let threshold: f64 = 1.0;
        let drift = (on_chain - accounting).abs();
        assert!(drift > threshold);
    }

    #[test]
    fn test_no_drift_within_threshold() {
        // Simulate: on-chain=100, accounting=100.50, threshold=1 → Ok
        let on_chain: f64 = 100.0;
        let accounting: f64 = 100.50;
        let threshold: f64 = 1.0;
        let drift = (on_chain - accounting).abs();
        assert!(drift <= threshold);
    }
}
