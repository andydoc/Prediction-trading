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

    /// Dry-run mode: simulate orders without submitting to CLOB.
    #[arg(long)]
    dry_run: bool,

    /// Comma-separated test IDs to skip (e.g., "D2,D3,D4,D7").
    /// Skipped tests auto-PASS with a skip note.
    #[arg(long)]
    skip_tests: Option<String>,

    /// Cleanup mode: cancel all orders + sell all open positions, then exit.
    #[arg(long)]
    cleanup: bool,
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

    // Derive or load CLOB API credentials
    let clob_creds = {
        // Check if credentials are already in secrets.yaml
        let existing = secrets.get("polymarket")
            .and_then(|p| p.get("clob_api_key"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());

        if let Some(api_key) = existing {
            let secret = secrets.get("polymarket")
                .and_then(|p| p.get("clob_api_secret"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let passphrase = secrets.get("polymarket")
                .and_then(|p| p.get("clob_passphrase"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            tracing::info!("Using existing CLOB API credentials (key={}...)", &api_key[..8.min(api_key.len())]);
            rust_engine::signing::ClobApiCreds {
                api_key: api_key.to_string(),
                secret,
                passphrase,
            }
        } else {
            tracing::info!("No CLOB API credentials found, deriving from wallet...");
            let creds = signer.create_or_derive_api_key(&clob_host).unwrap_or_else(|e| {
                tracing::error!("Failed to derive CLOB API credentials: {}", e);
                std::process::exit(1);
            });
            // Save to secrets.yaml for future use
            save_clob_creds_to_secrets(&secrets_path, &creds);
            creds
        }
    };

    let clob_auth = rust_engine::signing::ClobAuth::new(&clob_creds, &wallet_address)
        .unwrap_or_else(|e| {
            tracing::error!("Failed to create CLOB auth: {}", e);
            std::process::exit(1);
        });

    // Create executor (live mode — real CLOB orders)
    let mut executor = Executor::new(
        ExecutorConfig {
            clob_host: clob_host.clone(),
            dry_run: cli.dry_run,
            order_type: OrderType::Gtc,  // GTC so D2 orders sit on book for cancel test
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

    // Set L2 auth on executor (clone for harness)
    let harness_auth = clob_auth.clone();
    executor.set_clob_auth(clob_auth);

    // Cleanup mode: cancel all orders + sell all positions, then exit
    if cli.cleanup {
        tracing::info!("=== CLEANUP MODE ===");
        close_all_positions(&executor, &engine, &wallet_address);
        tracing::info!("=== CLEANUP COMPLETE ===");
        std::process::exit(0);
    }

    // === PRE-RUN CLEANUP: close all existing positions for clean slate ===
    // Skip cleanup on resume — the checkpoint state is what we're verifying
    if cli.resume_from.is_none() {
        tracing::info!("=== PRE-RUN CLEANUP: closing all existing positions ===");
        close_all_positions(&executor, &engine, &wallet_address);
    } else {
        tracing::info!("=== SKIP PRE-RUN CLEANUP (resuming from checkpoint) ===");
    }
    // Verify clean slate
    let http_verify = reqwest::blocking::Client::new();
    let verify_url = format!(
        "https://data-api.polymarket.com/positions?user={}&sizeThreshold=0",
        wallet_address.to_lowercase()
    );
    let remaining: Vec<serde_json::Value> = http_verify.get(&verify_url)
        .send().and_then(|r| r.json()).unwrap_or_default();
    let remaining_with_size: Vec<_> = remaining.iter().filter(|p| {
        p.get("size").and_then(|v| v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0.0) > 0.0
    }).collect();
    tracing::info!("Pre-run cleanup complete: {} positions remaining on exchange", remaining_with_size.len());
    if !remaining_with_size.is_empty() {
        tracing::warn!("Some positions could not be closed — they may have no bids");
    }

    // Query actual USDC.e balance on-chain for true initial capital
    // On resume: use checkpoint's initial_usdc (the capital at checkpoint time).
    // D6 verify then reconciles this saved state against the changed exchange reality.
    let actual_capital = if let Some(ref resume_path) = cli.resume_from {
        let cp_capital = ipc::read_checkpoint(&std::path::PathBuf::from(resume_path))
            .map(|c| c.initial_usdc)
            .unwrap_or(test_config.initial_capital);
        tracing::info!("Resume mode — using checkpoint capital: ${:.2}", cp_capital);
        cp_capital
    } else {
        let usdc_contract = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";
        let call_data = format!("0x70a08231000000000000000000000000{}", &wallet_address[2..]);
        let rpc_body = serde_json::json!({
            "jsonrpc": "2.0", "method": "eth_call",
            "params": [{"to": usdc_contract, "data": call_data}, "latest"],
            "id": 1
        });
        let rpc_result = http_verify
            .post("https://polygon.drpc.org")
            .json(&rpc_body)
            .send()
            .and_then(|r| r.json::<serde_json::Value>());
        match rpc_result {
            Ok(v) => {
                let hex = v.get("result").and_then(|r| r.as_str()).unwrap_or("0x0");
                let raw = u64::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap_or(0);
                raw as f64 / 1e6
            }
            Err(e) => {
                tracing::warn!("Failed to query on-chain USDC.e balance: {}. Using config value.", e);
                test_config.initial_capital
            }
        }
    };
    tracing::info!("Initial capital: ${:.2}", actual_capital);

    // Re-initialize engine with actual capital (on resume, D6 verify will override from checkpoint)
    engine.init_positions(actual_capital, test_config.taker_fee_rate);

    // Create notifier
    let notify_cfg = build_notify_config(&workspace, &secrets);
    let notifier = Arc::new(Notifier::new(notify_cfg));

    // Parse skip-tests
    let skip_tests: Vec<String> = cli.skip_tests
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_uppercase())
        .filter(|s| !s.is_empty())
        .collect();
    if !skip_tests.is_empty() {
        tracing::info!("Skipping tests: {:?}", skip_tests);
    }

    // Create tokio runtime for async WS operations
    let runtime = tokio::runtime::Runtime::new().unwrap_or_else(|e| {
        tracing::error!("Failed to create tokio runtime: {}", e);
        std::process::exit(1);
    });
    let runtime_handle = runtime.handle().clone();

    // Build harness — override initial_capital with actual on-chain balance
    let mut test_config = test_config;
    test_config.initial_capital = actual_capital;

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
        harness_auth,
        skip_tests,
        runtime_handle,
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

    let hostname = "vps.madrid".to_string();

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

/// Close all open positions on the exchange. Cancels orders first, then sells at 0.01 to guarantee fills.
fn close_all_positions(
    executor: &Executor,
    engine: &TradingEngine,
    wallet_address: &str,
) {
    // 1. Cancel all open orders
    let (cancelled, err) = executor.cancel_all_orders();
    tracing::info!("[CLEANUP] Cancelled {} orders (err={:?})", cancelled, err);

    // 2. Query positions via Data API
    let http = reqwest::blocking::Client::new();
    let url = format!(
        "https://data-api.polymarket.com/positions?user={}&sizeThreshold=0",
        wallet_address.to_lowercase()
    );
    let positions: Vec<serde_json::Value> = http.get(&url)
        .send().and_then(|r| r.json()).unwrap_or_default();
    tracing::info!("[CLEANUP] Found {} positions from Data API", positions.len());

    // 3. Sell each position at 80% of best bid to guarantee fill
    let clob = clob_test::clob_client::ClobClient::new("https://clob.polymarket.com");
    for p in &positions {
        let asset = p.get("asset").and_then(|v| v.as_str()).unwrap_or("");
        let cond_id = p.get("conditionId").and_then(|v| v.as_str()).unwrap_or("");
        let size = p.get("size").and_then(|v| v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0.0);
        let title = p.get("title").and_then(|v| v.as_str()).unwrap_or("?");

        if size <= 0.0 || asset.is_empty() { continue; }

        // Get best bid and sell at 80% discount
        let best_bid = clob.get_best_bid(asset);
        let sell_price = if best_bid > 0.0 { (best_bid * 0.80 * 1000.0).floor() / 1000.0 } else { 0.001 };
        let sell_price = sell_price.max(0.001);

        // Register instrument so executor can find it
        let neg_risk = p.get("negRisk")
            .or_else(|| p.get("negativeRisk"))
            .and_then(|v| v.as_bool()).unwrap_or(true);
        engine.instruments.insert_instrument(rust_engine::instrument::Instrument {
            market_id: cond_id.to_string(),
            token_id: asset.to_string(),
            outcome: "yes".to_string(),
            condition_id: cond_id.to_string(),
            neg_risk,
            tick_size: 0.001,
            rounding: rust_engine::instrument::RoundingConfig::from_tick_size_f64(0.001),
            min_order_size: 1.0,
            max_order_size: 0.0,
            order_book_enabled: true,
            accepting_orders: true,
        });

        let sell_value = size * sell_price;
        tracing::info!("[CLEANUP] SELL {} shares of {} at {:.4} (best_bid={:.4}, value={:.4})",
            size, &title[..title.len().min(40)], sell_price, best_bid, sell_value);

        let legs = vec![(
            cond_id.to_string(),
            asset.to_string(),
            rust_engine::signing::Side::Sell,
            sell_price,
            sell_value,
        )];
        let result = executor.execute_arb(&format!("cleanup_{}", &cond_id[..10.min(cond_id.len())]), &legs);
        let accepted = result.legs.iter()
            .filter(|l| matches!(l, rust_engine::executor::OrderResult::Accepted(_)))
            .count();
        tracing::info!("[CLEANUP]   Result: {}/{} accepted", accepted, legs.len());
    }

    // Wait for settlement
    if !positions.is_empty() {
        tracing::info!("[CLEANUP] Waiting 10s for settlement...");
        std::thread::sleep(std::time::Duration::from_secs(10));
    }
}

/// Save derived CLOB API credentials to secrets.yaml for future use.
fn save_clob_creds_to_secrets(secrets_path: &std::path::Path, creds: &rust_engine::signing::ClobApiCreds) {
    // Read existing secrets.yaml
    let content = std::fs::read_to_string(secrets_path).unwrap_or_default();

    // Append CLOB credentials under polymarket section
    let new_lines = format!(
        "\n  # CLOB API credentials (auto-derived {})\n  clob_api_key: '{}'\n  clob_api_secret: '{}'\n  clob_passphrase: '{}'\n",
        chrono::Utc::now().format("%Y-%m-%d"),
        creds.api_key,
        creds.secret,
        creds.passphrase,
    );

    // Find the polymarket section and append
    if content.contains("polymarket:") {
        // Append to end of file (inside polymarket section)
        let updated = format!("{}{}", content.trim_end(), new_lines);
        if let Err(e) = std::fs::write(secrets_path, updated) {
            tracing::warn!("Failed to save CLOB credentials to secrets.yaml: {}", e);
        } else {
            tracing::info!("CLOB API credentials saved to {}", secrets_path.display());
        }
    } else {
        tracing::warn!("No 'polymarket:' section in secrets.yaml — credentials not saved");
    }
}
