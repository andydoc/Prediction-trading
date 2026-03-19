/// Test report and exception report generation.

use serde::{Serialize, Deserialize};
use std::path::Path;

/// Overall test report written to data/clob_test_report.json.
#[derive(Debug, Clone, Serialize)]
pub struct TestReport {
    pub timestamp: String,
    pub duration_seconds: f64,
    pub wallet_address: String,
    pub initial_usdc: f64,
    pub initial_pol: f64,
    pub final_usdc: f64,
    pub final_pol: f64,
    pub overall: String,  // "PASS" or "FAIL"
    pub tests: Vec<TestResult>,
}

impl TestReport {
    pub fn new(wallet_address: &str, initial_usdc: f64, initial_pol: f64) -> Self {
        Self {
            timestamp: chrono::Utc::now().to_rfc3339(),
            duration_seconds: 0.0,
            wallet_address: wallet_address.to_string(),
            initial_usdc,
            initial_pol,
            final_usdc: 0.0,
            final_pol: 0.0,
            overall: "PENDING".to_string(),
            tests: Vec::new(),
        }
    }

    pub fn add_result(&mut self, result: TestResult) {
        self.tests.push(result);
    }

    pub fn finalize(&mut self, duration: f64, final_usdc: f64, final_pol: f64) {
        self.duration_seconds = duration;
        self.final_usdc = final_usdc;
        self.final_pol = final_pol;
        self.overall = if self.tests.iter().all(|t| t.result == "PASS") {
            "PASS".to_string()
        } else {
            "FAIL".to_string()
        };
    }

    pub fn write(&self, workspace: &Path) -> std::io::Result<()> {
        let path = workspace.join("data").join("clob_test_report.json");
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(&path, json)?;
        tracing::info!("Test report written to {}", path.display());
        Ok(())
    }
}

/// Individual test result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    pub id: String,
    pub name: String,
    pub result: String,  // "PASS", "FAIL", "SKIP"
    pub duration_ms: u64,
    pub details: serde_json::Value,
    pub errors: Vec<String>,
}

impl TestResult {
    pub fn pass(id: &str, name: &str, duration_ms: u64, details: serde_json::Value) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            result: "PASS".to_string(),
            duration_ms,
            details,
            errors: Vec::new(),
        }
    }

    pub fn fail(id: &str, name: &str, duration_ms: u64, errors: Vec<String>) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            result: "FAIL".to_string(),
            duration_ms,
            details: serde_json::json!({}),
            errors,
        }
    }
}

/// Exception report — written only if issues are found.
#[derive(Debug, Clone, Serialize)]
pub struct ExceptionReport {
    pub timestamp: String,
    pub exceptions: Vec<Exception>,
}

impl ExceptionReport {
    pub fn new() -> Self {
        Self {
            timestamp: chrono::Utc::now().to_rfc3339(),
            exceptions: Vec::new(),
        }
    }

    pub fn add(&mut self, exception: Exception) {
        self.exceptions.push(exception);
    }

    pub fn is_empty(&self) -> bool {
        self.exceptions.is_empty()
    }

    pub fn write(&self, workspace: &Path) -> std::io::Result<()> {
        if self.exceptions.is_empty() { return Ok(()); }
        let path = workspace.join("data").join("clob_test_exceptions.json");
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(&path, json)?;
        tracing::warn!("Exception report written to {}", path.display());
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Exception {
    pub severity: String,       // "CRITICAL", "WARNING", "INFO"
    pub test_id: String,
    pub component: String,
    pub description: String,
    pub expected: String,
    pub actual: String,
    pub recommendation: String,
}
