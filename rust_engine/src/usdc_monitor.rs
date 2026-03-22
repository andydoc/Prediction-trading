/// USDC.e balance monitoring (B4.6).
///
/// Periodically queries Polygon RPC for the wallet's USDC.e token balance.
/// Compares on-chain balance vs accounting cash to detect drift.
///
/// - Drift alert: Telegram warning when |on-chain - accounting| > drift_threshold
/// - Warning threshold: Telegram alert when balance < warning_balance
/// - Critical threshold: Trips circuit breaker when balance < critical_balance
///
/// Uses `eth_call` to the USDC.e ERC-20 `balanceOf(address)` method — no web3 library needed.

use reqwest::blocking::Client;

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
    /// Alert if |on-chain - accounting| exceeds this (default $1.00).
    pub drift_threshold: f64,
    /// Telegram warning when balance falls below this (default $10.00).
    pub warning_balance: f64,
    /// Circuit breaker when balance falls below this (default $1.00).
    pub critical_balance: f64,
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

    /// Query the Polygon RPC for the wallet's USDC.e balance and compare
    /// against the accounting cash value.
    pub fn check_balance(&mut self, client: &Client, accounting_cash: f64) -> UsdcCheckResult {
        if !self.is_enabled() {
            return UsdcCheckResult::Ok { on_chain: 0.0, accounting: accounting_cash };
        }

        // Build eth_call payload for balanceOf(wallet_address)
        let wallet_padded = format!(
            "000000000000000000000000{}",
            self.config.wallet_address.trim_start_matches("0x")
        );
        let call_data = format!("0x{}{}", BALANCE_OF_SELECTOR, wallet_padded);

        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [{
                "to": USDC_E_CONTRACT,
                "data": call_data,
            }, "latest"],
            "id": 1
        });

        let resp = match client.post(&self.config.rpc_url)
            .json(&payload)
            .send()
        {
            Ok(r) => r,
            Err(e) => return UsdcCheckResult::Error(format!("RPC request failed: {}", e)),
        };

        let body: serde_json::Value = match resp.json() {
            Ok(v) => v,
            Err(e) => return UsdcCheckResult::Error(format!("RPC response parse failed: {}", e)),
        };

        // Parse hex balance from result field
        let hex_str = match body["result"].as_str() {
            Some(s) => s,
            None => {
                let err = body["error"].as_object()
                    .map(|e| format!("{}", serde_json::Value::Object(e.clone())))
                    .unwrap_or_else(|| "no result field".to_string());
                return UsdcCheckResult::Error(format!("RPC error: {}", err));
            }
        };

        let raw = match u128::from_str_radix(hex_str.trim_start_matches("0x"), 16) {
            Ok(v) => v,
            Err(e) => return UsdcCheckResult::Error(format!("Balance parse failed: {}", e)),
        };

        let on_chain = raw as f64 / USDC_DECIMALS;
        self.last_on_chain = Some(on_chain);

        // Check thresholds (critical takes priority)
        if on_chain < self.config.critical_balance {
            return UsdcCheckResult::CriticalBalance(on_chain);
        }
        if on_chain < self.config.warning_balance {
            return UsdcCheckResult::LowBalance(on_chain);
        }

        // Check drift against accounting
        let drift = (on_chain - accounting_cash).abs();
        if drift > self.config.drift_threshold {
            return UsdcCheckResult::DriftWarning {
                on_chain,
                accounting: accounting_cash,
                drift,
            };
        }

        UsdcCheckResult::Ok { on_chain, accounting: accounting_cash }
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
