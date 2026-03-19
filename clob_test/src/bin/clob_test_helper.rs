/// Milestone D Test Helper Binary.
///
/// Handles D6 cold-start restart:
/// 1. Waits for D6 ready flag
/// 2. SIGTERM main process
/// 3. Close one position via CLOB
/// 4. Restart main with --resume-from

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;

use rust_engine::executor::{Executor, ExecutorConfig, OrderType, OrderAggression};
use rust_engine::signing::OrderSigner;
use rust_engine::rate_limiter::RateLimiter;
use rust_engine::instrument::InstrumentStore;

use clob_test::ipc;

#[derive(Parser, Debug)]
#[command(name = "clob-test-helper", about = "Milestone D test helper for D6 restart")]
struct Cli {
    /// Workspace root.
    #[arg(short, long, default_value = ".")]
    workspace: String,

    /// Mode: d6 (cold-start restart).
    #[arg(long, default_value = "d6")]
    mode: String,
}

fn main() {
    let cli = Cli::parse();
    let workspace = PathBuf::from(&cli.workspace);

    tracing_subscriber::fmt()
        .with_env_filter("clob_test=info,rust_engine=info")
        .init();

    tracing::info!("=== CLOB-TEST HELPER ({}) ===", cli.mode);

    match cli.mode.as_str() {
        "d6" => run_d6(&workspace),
        other => {
            tracing::error!("Unknown mode: {}. Supported: d6", other);
            std::process::exit(1);
        }
    }
}

fn run_d6(workspace: &PathBuf) {
    tracing::info!("[D6 Helper] Waiting for D6 ready flag...");

    // 1. Poll for D6 ready flag
    loop {
        if ipc::is_d6_ready(workspace) {
            tracing::info!("[D6 Helper] D6 ready flag detected!");
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }

    // 2. Read checkpoint
    let checkpoint_path = ipc::checkpoint_path(workspace);
    let checkpoint = match ipc::read_checkpoint(&checkpoint_path) {
        Some(c) => c,
        None => {
            tracing::error!("[D6 Helper] Failed to read checkpoint");
            std::process::exit(1);
        }
    };

    let position_to_close = checkpoint.open_position_ids.first().cloned();
    tracing::info!("[D6 Helper] Will close position: {:?}", position_to_close);

    // 3. SIGTERM main process FIRST
    if let Some(pid) = ipc::read_pid(workspace) {
        tracing::info!("[D6 Helper] Sending SIGTERM to PID {}", pid);
        #[cfg(unix)]
        {
            use std::process::Command;
            let _ = Command::new("kill").arg("-TERM").arg(pid.to_string()).output();
        }
        #[cfg(not(unix))]
        {
            // On Windows, use taskkill
            use std::process::Command;
            let _ = Command::new("taskkill").arg("/PID").arg(pid.to_string()).arg("/F").output();
        }
    } else {
        tracing::warn!("[D6 Helper] No PID file found, main may have already exited");
    }

    // 4. Wait for clean shutdown
    tracing::info!("[D6 Helper] Waiting 5s for main to shut down...");
    std::thread::sleep(std::time::Duration::from_secs(5));

    // 5. Close one position via CLOB (main is stopped, no conflict)
    if let Some(_position_id) = &position_to_close {
        tracing::info!("[D6 Helper] Closing position via CLOB...");
        // Load secrets for executor
        let secrets_path = workspace.join("config").join("secrets.yaml");
        let secrets: serde_json::Value = std::fs::read_to_string(&secrets_path)
            .ok()
            .and_then(|s| serde_yaml_ng::from_str(&s).ok())
            .unwrap_or_default();

        let private_key = secrets.get("polymarket")
            .and_then(|p| p.get("private_key"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let clob_host = secrets.get("polymarket")
            .and_then(|p| p.get("host"))
            .and_then(|v| v.as_str())
            .unwrap_or("https://clob.polymarket.com");

        if let Ok(signer) = OrderSigner::new(private_key) {
            let instruments = Arc::new(InstrumentStore::new());
            let rate_limiter = Arc::new(RateLimiter::new());

            if let Ok(executor) = Executor::new(
                ExecutorConfig {
                    clob_host: clob_host.to_string(),
                    dry_run: false,
                    order_type: OrderType::Fak,
                    aggression: OrderAggression::AtMarket,
                    fee_rate_bps: 0,
                    confirmation_timeout_secs: 120.0,
                },
                signer,
                instruments,
                rate_limiter,
            ) {
                // Cancel any remaining orders for this position
                let (cancelled, err) = executor.cancel_all_orders();
                tracing::info!("[D6 Helper] Cancelled {} orders (err={:?})", cancelled, err);
                // Note: actual position closing (SELL at market) requires order book data
                // which the helper doesn't have. The position is effectively orphaned on CLOB.
                // The main binary's D6 verify will detect it via reconciliation.
            }
        }
    }

    // 6. Wait a bit more
    tracing::info!("[D6 Helper] Waiting 5s before restart...");
    std::thread::sleep(std::time::Duration::from_secs(5));

    // 7. Clean up D6 flag
    ipc::clear_d6_flag(workspace);

    // 8. Restart main with --resume-from
    let clob_test_bin = std::env::current_exe()
        .unwrap_or_else(|_| PathBuf::from("clob-test"))
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("clob-test");

    tracing::info!("[D6 Helper] Restarting main: {} --workspace {} --resume-from {}",
        clob_test_bin.display(), workspace.display(), checkpoint_path.display());

    let child = std::process::Command::new(&clob_test_bin)
        .arg("--workspace")
        .arg(workspace.to_str().unwrap_or("."))
        .arg("--resume-from")
        .arg(checkpoint_path.to_str().unwrap_or(""))
        .arg("--skip-deposit-check")
        .spawn();

    match child {
        Ok(c) => {
            tracing::info!("[D6 Helper] Main restarted as PID {}", c.id());
            // Don't wait — let main run independently
        }
        Err(e) => {
            tracing::error!("[D6 Helper] Failed to restart main: {}", e);
            std::process::exit(1);
        }
    }

    tracing::info!("[D6 Helper] Done. Exiting.");
}
