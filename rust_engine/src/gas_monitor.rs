/// POL gas balance monitoring (C1.1).
///
/// Periodically queries Polygon RPC for the wallet's native POL balance.
/// - Warning threshold: Telegram alert when balance < min_pol_balance
/// - Critical threshold: Trips circuit breaker when balance < critical_pol_balance
///
/// Uses a simple `eth_getBalance` JSON-RPC call — no web3 library needed.

use reqwest::blocking::Client;

/// Wei-to-POL conversion factor (18 decimals).
const WEI_PER_POL: f64 = 1e18;

/// Gas monitor configuration.
#[derive(Clone, Debug)]
pub struct GasMonitorConfig {
    pub enabled: bool,
    pub rpc_url: String,
    pub wallet_address: String,
    pub check_interval_seconds: f64,
    pub min_pol_balance: f64,
    pub critical_pol_balance: f64,
}

impl Default for GasMonitorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            rpc_url: "https://polygon-bor-rpc.publicnode.com".to_string(),
            wallet_address: String::new(),
            check_interval_seconds: 3600.0,
            min_pol_balance: 1.0,
            critical_pol_balance: 0.1,
        }
    }
}

/// Result of a gas balance check.
#[derive(Debug, Clone)]
pub enum GasCheckResult {
    /// Balance is healthy (above warning threshold).
    Ok(f64),
    /// Balance is low (below warning, above critical).
    Warning(f64),
    /// Balance is critically low (below critical threshold).
    Critical(f64),
    /// RPC query failed.
    Error(String),
}

/// Gas balance monitor. Owned by the orchestrator.
pub struct GasMonitor {
    config: GasMonitorConfig,
    last_balance: Option<f64>,
    last_warning_sent: bool,
}

impl GasMonitor {
    pub fn new(config: GasMonitorConfig) -> Self {
        Self {
            config,
            last_balance: None,
            last_warning_sent: false,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.config.enabled && !self.config.wallet_address.is_empty()
    }

    pub fn check_interval(&self) -> f64 {
        self.config.check_interval_seconds
    }

    /// Query the Polygon RPC for the wallet's native POL balance.
    pub fn check_balance(&mut self, client: &Client) -> GasCheckResult {
        if !self.is_enabled() {
            return GasCheckResult::Ok(0.0);
        }

        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getBalance",
            "params": [&self.config.wallet_address, "latest"],
            "id": 1
        });

        let resp = match client.post(&self.config.rpc_url)
            .json(&payload)
            .send()
        {
            Ok(r) => r,
            Err(e) => return GasCheckResult::Error(format!("RPC request failed: {}", e)),
        };

        let body: serde_json::Value = match resp.json() {
            Ok(v) => v,
            Err(e) => return GasCheckResult::Error(format!("RPC response parse failed: {}", e)),
        };

        // Parse hex balance from result field
        let hex_str = match body["result"].as_str() {
            Some(s) => s,
            None => {
                let err = body["error"].as_object()
                    .map(|e| format!("{}", serde_json::Value::Object(e.clone())))
                    .unwrap_or_else(|| "no result field".to_string());
                return GasCheckResult::Error(format!("RPC error: {}", err));
            }
        };

        let balance_wei = match u128::from_str_radix(hex_str.trim_start_matches("0x"), 16) {
            Ok(v) => v,
            Err(e) => return GasCheckResult::Error(format!("Balance parse failed: {}", e)),
        };

        let balance_pol = balance_wei as f64 / WEI_PER_POL;
        self.last_balance = Some(balance_pol);

        if balance_pol < self.config.critical_pol_balance {
            self.last_warning_sent = true;
            GasCheckResult::Critical(balance_pol)
        } else if balance_pol < self.config.min_pol_balance {
            self.last_warning_sent = true;
            GasCheckResult::Warning(balance_pol)
        } else {
            self.last_warning_sent = false;
            GasCheckResult::Ok(balance_pol)
        }
    }

    /// Get the last known balance (for dashboard display).
    pub fn last_balance(&self) -> Option<f64> {
        self.last_balance
    }

    pub fn wallet_address(&self) -> &str {
        &self.config.wallet_address
    }

    pub fn min_balance(&self) -> f64 {
        self.config.min_pol_balance
    }

    pub fn critical_balance(&self) -> f64 {
        self.config.critical_pol_balance
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_monitor_returns_ok() {
        let config = GasMonitorConfig::default();
        let mut monitor = GasMonitor::new(config);
        let client = Client::new();
        match monitor.check_balance(&client) {
            GasCheckResult::Ok(v) => assert_eq!(v, 0.0),
            _ => panic!("Expected Ok for disabled monitor"),
        }
    }

    #[test]
    fn hex_balance_parsing() {
        // 1 POL = 1e18 wei = 0xDE0B6B3A7640000
        let hex = "0xde0b6b3a7640000";
        let wei = u128::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap();
        let pol = wei as f64 / WEI_PER_POL;
        assert!((pol - 1.0).abs() < 0.001);
    }

    #[test]
    fn hex_balance_5_pol() {
        // 5 POL = 5e18 wei = 0x4563918244F40000
        let hex = "0x4563918244f40000";
        let wei = u128::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap();
        let pol = wei as f64 / WEI_PER_POL;
        assert!((pol - 5.0).abs() < 0.001);
    }
}
