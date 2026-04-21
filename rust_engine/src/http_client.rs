/// Centralised HTTP client construction with explicit TLS verification.
///
/// All `reqwest::blocking::Client` instances in `rust_engine` should be built
/// via this module so security defaults stay consistent.
///
/// **TLS posture** (F-pre-2 / G2):
/// - rustls-tls backend (no system OpenSSL); selected via Cargo features in
///   `rust_engine/Cargo.toml`. rustls verifies the certificate chain against
///   the bundled webpki-roots by default.
/// - `min_tls_version(TLS_1_2)` — explicitly reject TLS 1.0/1.1 even though
///   modern servers refuse them anyway.
/// - `https_only(true)` — refuse any plaintext HTTP request, including
///   redirects from HTTPS → HTTP.
/// - `tls_built_in_root_certs(true)` — use webpki-roots, do not fall back
///   to system trust store (which may be modified on the VPS).
///
/// **First-connection cert log**: each helper logs the TLS posture once at
/// build time via `tracing::info!`, including target host hint and timeout.
/// Per-connection cert inspection is not done here (rustls handles
/// verification internally; logging certs would require a custom verifier
/// which we explicitly do not want — the whole point is to use rustls's
/// default secure verifier).
///
/// Created 2026-04-21 for v0.20.3 (F-pre-2 / G2).

use std::time::Duration;
use reqwest::blocking::{Client, ClientBuilder};
use reqwest::tls;

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const DEFAULT_USER_AGENT: &str =
    "prediction-trader/0.20.3 (+https://github.com/andydoc/Prediction-trading)";

/// Apply the standard secure TLS posture to a reqwest builder.
fn apply_secure_defaults(builder: ClientBuilder) -> ClientBuilder {
    builder
        .min_tls_version(tls::Version::TLS_1_2)
        .https_only(true)
        .tls_built_in_root_certs(true)
        .user_agent(DEFAULT_USER_AGENT)
}

/// Build a secure HTTP client with the default 30s timeout.
///
/// Use this when you don't have specific timeout requirements.
/// For custom timeouts, use [`secure_client_with_timeout`].
pub fn secure_client() -> Result<Client, String> {
    secure_client_with_timeout(DEFAULT_TIMEOUT_SECS)
}

/// Build a secure HTTP client with a custom request timeout (seconds).
///
/// Logs the TLS posture once per build via `tracing::info!`.
pub fn secure_client_with_timeout(timeout_secs: u64) -> Result<Client, String> {
    let builder = Client::builder().timeout(Duration::from_secs(timeout_secs));
    let client = apply_secure_defaults(builder)
        .build()
        .map_err(|e| format!("secure_client build failed: {}", e))?;
    tracing::info!(
        "[http_client] secure client built: timeout={}s, min_tls=TLS_1_2, https_only=true, rustls webpki-roots",
        timeout_secs
    );
    Ok(client)
}

/// Build a secure HTTP client with a custom UA suffix appended to the default UA.
///
/// Useful for distinguishing endpoints in server logs (e.g. "scanner", "executor").
pub fn secure_client_tagged(timeout_secs: u64, tag: &str) -> Result<Client, String> {
    let ua = format!("{} ({})", DEFAULT_USER_AGENT, tag);
    let builder = Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .user_agent(ua);
    let client = builder
        .min_tls_version(tls::Version::TLS_1_2)
        .https_only(true)
        .tls_built_in_root_certs(true)
        .build()
        .map_err(|e| format!("secure_client_tagged build failed: {}", e))?;
    tracing::info!(
        "[http_client] secure client built: tag={}, timeout={}s, min_tls=TLS_1_2, https_only=true",
        tag, timeout_secs
    );
    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_client_builds() {
        let c = secure_client();
        assert!(c.is_ok(), "default secure_client should build: {:?}", c.err());
    }

    #[test]
    fn timeout_client_builds() {
        let c = secure_client_with_timeout(5);
        assert!(c.is_ok(), "timeout secure_client should build");
    }

    #[test]
    fn tagged_client_builds() {
        let c = secure_client_tagged(10, "test");
        assert!(c.is_ok(), "tagged secure_client should build");
    }
}
