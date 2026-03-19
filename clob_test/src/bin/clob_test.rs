/// Milestone D CLOB Integration Test Harness.
///
/// Runs all 8 Milestone D acceptance tests against the real Polymarket CLOB.
/// Uses merged Shadow A-F parameters for maximum arb detection speed.
///
/// Usage:
///   clob-test --workspace /path/to/prediction-trader
///   clob-test --workspace /path --skip-deposit-check
///   clob-test --workspace /path --resume-from data/clob_test_checkpoint.json

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use rust_engine::TradingEngine;
use rust_engine::executor::{Executor, ExecutorConfig, OrderType, OrderAggression};
use rust_engine::signing::OrderSigner;
use rust_engine::notify::{Notifier, NotifyConfig};
use rust_engine::rate_limiter::RateLimiter;

use clob_test::config::MergedTestConfig;
use clob_test::ipc;
use clob_test::orchestrate::TestHarness;

#[derive(Parser, Debug)]
#[command(name = "clob-test", about = "Milestone D CLOB Integration Test Harness")]
struct Cli {
    /// Workspace root (contains config/, data/, logs/).
    #[arg(short, long, default_value = ".")]
    workspace: String,

    /// Dashboard port (0 = disabled).
    #[arg(long, default_value = "5570")]
    dashboard_port: u16,

    /// Resume from a D6 checkpoint file.
    #[arg(long)]
    resume_from: Option<String>,

    /// Skip D1 deposit check (use engine capital from config).
    #[arg(long)]
    skip_deposit_check: bool,

    /// Timeout in minutes (default: 720 = 12 hours).
    #[arg(long, default_value = "720")]
    timeout_minutes: u64,
}

fn main() {
    let cli = Cli::parse();
    let workspace = PathBuf::from(&cli.workspace);

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter("clob_test=info,rust_engine=info")
        .init();

    tracing::info!("=== CLOB-TEST HARNESS v0.1.0 ===");
    tracing::info!("Workspace: {}", workspace.display());

    // Load secrets
    let secrets_path = workspace.join("config").join("secrets.yaml");
    let secrets: serde_json::Value = std::fs::read_to_string(&secrets_path)
        .ok()
        .and_then(|s| serde_yaml_ng::from_str(&s).ok())
        .unwrap_or_else(|| {
            tracing::error!("Failed to load secrets from {}", secrets_path.display());
            std::process::exit(1);
        });

    // Extract credentials
    let private_key = secrets.get("polymarket")
        .and_then(|p| p.get("private_key"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            tracing::error!("No polymarket.private_key in secrets.yaml");
            std::process::exit(1);
        });

    let clob_host = secrets.get("polymarket")
        .and_then(|p| p.get("host"))
        .and_then(|v| v.as_str())
        .unwrap_or("https://clob.polymarket.com")
        .to_string();

    let _funder_address = secrets.get("polymarket")
        .and_then(|p| p.get("funder_address"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Create order signer
    let signer = OrderSigner::new(private_key).unwrap_or_else(|e| {
        tracing::error!("Failed to create order signer: {}", e);
        std::process::exit(1);
    });

    let wallet_address = format!("{:?}", signer.address());
    tracing::info!("Wallet: {}", wallet_address);

    // Load merged test config
    let test_config = MergedTestConfig::from_workspace(&workspace);
    tracing::info!("Test config: {:?}", test_config);

    // Create trading engine
    let engine = TradingEngine::new(&cli.workspace).unwrap_or_else(|e| {
        tracing::error!("Failed to create engine: {}", e);
        std::process::exit(1);
    });

    // Start dashboard
    if cli.dashboard_port > 0 {
        engine.start_dashboard(cli.dashboard_port, "0.0.0.0");
        tracing::info!("Dashboard: http://localhost:{}", cli.dashboard_port);
    }

    // Initialize positions with test capital
    engine.init_positions(test_config.initial_capital, test_config.taker_fee_rate);

    // Create executor (live mode — real CLOB orders)
    let executor = Executor::new(
        ExecutorConfig {
            clob_host: clob_host.clone(),
            dry_run: false,  // Real trades!
            order_type: OrderType::Fak,
            aggression: OrderAggression::AtMarket,
            fee_rate_bps: 0,
            confirmation_timeout_secs: 120.0,
        },
        signer,
        Arc::clone(&engine.instruments),
        Arc::new(RateLimiter::new()),
    ).unwrap_or_else(|e| {
        tracing::error!("Failed to create executor: {}", e);
        std::process::exit(1);
    });

    // Create notifier
    let notify_cfg = build_notify_config(&workspace, &secrets);
    let notifier = Arc::new(Notifier::new(notify_cfg));

    // Build harness
    let mut harness = TestHarness::new(
        engine,
        executor,
        Arc::clone(&notifier),
        test_config,
        workspace.clone(),
        wallet_address,
        clob_host,
        cli.timeout_minutes,
        cli.skip_deposit_check,
    );

    // Check for D6 resume
    if let Some(resume_path) = &cli.resume_from {
        let path = PathBuf::from(resume_path);
        match ipc::read_checkpoint(&path) {
            Some(checkpoint) => {
                tracing::info!("Resuming from checkpoint: {}", resume_path);
                harness = harness.resume_from_checkpoint(checkpoint);
            }
            None => {
                tracing::error!("Failed to read checkpoint from {}", resume_path);
                std::process::exit(1);
            }
        }
    }

    // Run!
    harness.run();

    tracing::info!("=== CLOB-TEST COMPLETE ===");
}

/// Build notification config from workspace config + secrets.
fn build_notify_config(workspace: &PathBuf, secrets: &serde_json::Value) -> NotifyConfig {
    // Read config.yaml for notification settings
    let config_path = workspace.join("config").join("config.yaml");
    let config: serde_json::Value = std::fs::read_to_string(&config_path)
        .ok()
        .and_then(|s| serde_yaml_ng::from_str(&s).ok())
        .unwrap_or_default();

    let n = config.get("notifications").cloned().unwrap_or_default();

    // Build Telegram webhook URL
    let cfg_url = n.get("webhook_url").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let webhook_url = secrets.get("telegram_bot_token")
        .and_then(|v| v.as_str())
        .filter(|t| !t.is_empty())
        .map(|token| format!("https://api.telegram.org/bot{}/sendMessage", token))
        .unwrap_or(cfg_url);

    let hostname = std::fs::read_to_string("/etc/hostname")
        .unwrap_or_default().trim().to_string();

    NotifyConfig {
        enabled: true,  // Always enabled for test harness
        webhook_url,
        api_key: String::new(),
        phone_number: n.get("phone_number").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        on_entry: true,
        on_resolution: true,
        on_error: true,
        on_circuit_breaker: true,
        on_daily_summary: false,
        rate_limit_seconds: 5.0,  // Faster rate for testing
        hostname,
        instance: "clob-test".to_string(),
    }
}
