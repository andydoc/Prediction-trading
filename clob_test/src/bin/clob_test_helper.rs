/// Milestone D Test Helper Binary.
///
/// Handles D6 cold-start restart:
/// 1. Waits for D6 ready flag
/// 2. SIGTERM main process
/// 3. Close one position via CLOB (real SELL order)
/// 4. Restart main with --resume-from, logging to file

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;

use rust_engine::executor::{Executor, ExecutorConfig, OrderType, OrderAggression};
use rust_engine::signing::{OrderSigner, ClobAuth, ClobApiCreds, Side};
use rust_engine::rate_limiter::RateLimiter;
use rust_engine::instrument::{Instrument, InstrumentStore, RoundingConfig};
use rust_engine::ws_user::{UserChannelClient, UserEvent};

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
    // Install rustls crypto provider before multi-thread runtime spawns WS on worker thread
    let _ = rustls::crypto::ring::default_provider().install_default();

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

    tracing::info!("[D6 Helper] Checkpoint has {} position IDs, {} serialized positions",
        checkpoint.open_position_ids.len(), checkpoint.open_positions_json.len());

    // 3. SIGTERM main process FIRST (before we touch CLOB)
    if let Some(pid) = ipc::read_pid(workspace) {
        tracing::info!("[D6 Helper] Sending SIGTERM to PID {}", pid);
        #[cfg(unix)]
        {
            use std::process::Command;
            let _ = Command::new("kill").arg("-TERM").arg(pid.to_string()).output();
        }
        #[cfg(not(unix))]
        {
            use std::process::Command;
            let _ = Command::new("taskkill").arg("/PID").arg(pid.to_string()).arg("/F").output();
        }
    } else {
        tracing::warn!("[D6 Helper] No PID file found, main may have already exited");
    }

    // 4. Wait for clean shutdown
    tracing::info!("[D6 Helper] Waiting 5s for main to shut down...");
    std::thread::sleep(std::time::Duration::from_secs(5));

    // 5. Close ONE position via real CLOB GTC SELL at best bid + WS confirmation
    let confirmed = close_one_position(workspace, &checkpoint);

    if !confirmed {
        tracing::error!("[D6 Helper] No venue state change confirmed — NOT restarting main");
        tracing::error!("[D6 Helper] Re-run the helper when the sell fills");
        std::process::exit(1);
    }

    // 6. Clean up D6 flag
    ipc::clear_d6_flag(workspace);

    // 7. Restart main
    restart_main(workspace);
}

/// Restart the main test binary with --resume-from checkpoint.
fn restart_main(workspace: &PathBuf) {
    let checkpoint_path = ipc::checkpoint_path(workspace);
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
        .env("RUST_LOG", "clob_test=info,rust_engine=info")
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn();

    match child {
        Ok(mut c) => {
            tracing::info!("[D6 Helper] Main restarted as PID {}. Output inherited to helper stderr.", c.id());
            match c.wait() {
                Ok(status) => tracing::info!("[D6 Helper] Main exited with: {}", status),
                Err(e) => tracing::error!("[D6 Helper] Failed to wait for main: {}", e),
            }
        }
        Err(e) => {
            tracing::error!("[D6 Helper] Failed to restart main: {}", e);
            std::process::exit(1);
        }
    }

    tracing::info!("[D6 Helper] Done. Exiting.");
}

/// Close one position via a real CLOB SELL order.
/// Reads the checkpoint's serialized positions to get market/token data.
fn close_one_position(workspace: &PathBuf, checkpoint: &ipc::Checkpoint) -> bool {
    // Collect ALL legs across ALL checkpoint positions
    let mut all_legs: Vec<(String, String, String)> = Vec::new(); // (market_id, token_id, outcome)
    for pos_json in &checkpoint.open_positions_json {
        let pos: serde_json::Value = match serde_json::from_str(pos_json) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("[D6 Helper] Failed to parse position JSON: {}", e);
                continue;
            }
        };
        let pos_id = pos.get("position_id").and_then(|v| v.as_str()).unwrap_or("?");
        if let Some(markets) = pos.get("markets").and_then(|v| v.as_object()) {
            for (mid, leg) in markets.iter() {
                let tid = leg.get("token_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let outcome = leg.get("outcome").and_then(|v| v.as_str()).unwrap_or("yes").to_string();
                if !tid.is_empty() {
                    tracing::info!("[D6 Helper] Found leg: pos={} market={} token={}...",
                        pos_id, &mid[..mid.len().min(20)], &tid[..tid.len().min(20)]);
                    all_legs.push((mid.clone(), tid, outcome));
                }
            }
        }
    }
    if all_legs.is_empty() {
        tracing::warn!("[D6 Helper] No legs found in any checkpoint position");
        return false;
    }

    // Load secrets
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

    let signer = match OrderSigner::new(private_key) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("[D6 Helper] Failed to create signer: {}", e);
            return false;
        }
    };

    let wallet_address = format!("{:?}", signer.address());

    // Load L2 auth credentials
    let clob_auth = {
        let api_key = secrets.get("polymarket").and_then(|p| p.get("clob_api_key"))
            .and_then(|v| v.as_str()).unwrap_or("").to_string();
        let secret = secrets.get("polymarket").and_then(|p| p.get("clob_api_secret"))
            .and_then(|v| v.as_str()).unwrap_or("").to_string();
        let passphrase = secrets.get("polymarket").and_then(|p| p.get("clob_passphrase"))
            .and_then(|v| v.as_str()).unwrap_or("").to_string();

        if api_key.is_empty() {
            tracing::error!("[D6 Helper] No CLOB API credentials in secrets.yaml");
            return false;
        }

        match ClobAuth::new(&ClobApiCreds { api_key, secret, passphrase }, &wallet_address) {
            Ok(a) => a,
            Err(e) => {
                tracing::error!("[D6 Helper] Failed to create ClobAuth: {}", e);
                return false;
            }
        }
    };

    let instruments = Arc::new(InstrumentStore::new());
    let rate_limiter = Arc::new(RateLimiter::new());

    let mut executor = match Executor::new(
        ExecutorConfig {
            clob_host: clob_host.to_string(),
            dry_run: false,
            order_type: OrderType::Fak,
            aggression: OrderAggression::AtMarket,
            fee_rate_bps: 0,
            confirmation_timeout_secs: 120.0,
        },
        signer,
        Arc::clone(&instruments),
        rate_limiter,
    ) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!("[D6 Helper] Failed to create executor: {}", e);
            return false;
        }
    };

    // Set L2 auth so cancel-all works
    let ws_auth = clob_auth.clone();
    executor.set_clob_auth(clob_auth);

    // Cancel any open orders first
    let (cancelled, err) = executor.cancel_all_orders();
    tracing::info!("[D6 Helper] Cancelled {} orders (err={:?})", cancelled, err);

    // No need to wait for on-chain confirmation — we're BUYING, not selling.
    // Scan ALL legs across ALL positions for cheapest external ask.

    let http_client = reqwest::blocking::Client::new();
    let neg_risk = true;
    let tick_size = 0.001;

    // Find the leg with the cheapest ask price (cheapest = most shares per $, guarantees fill)
    let mut best_leg: Option<(String, String, String, f64)> = None; // (market_id, token_id, outcome, best_ask)
    for (mid, tid, outcome) in &all_legs {
        let ask = {
            let book_url = format!("{}/book?token_id={}", clob_host, tid);
            http_client.get(&book_url).send().ok()
                .and_then(|r| r.json::<serde_json::Value>().ok())
                .and_then(|b| b.get("asks")?.as_array()?.iter()
                    .filter_map(|o| o.get("price")?.as_str()?.parse::<f64>().ok())
                    .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)))
                .unwrap_or(0.0)
        };
        if ask <= 0.0 { continue; }
        tracing::info!("[D6 Helper] Leg {} ask={:.4}", &mid[..mid.len().min(20)], ask);

        if best_leg.is_none() || ask < best_leg.as_ref().unwrap().3 {
            best_leg = Some((mid.clone(), tid.clone(), outcome.clone(), ask));
        }
    }

    let (market_id, token_id, outcome, best_ask) = match best_leg {
        Some(leg) => leg,
        None => {
            tracing::warn!("[D6 Helper] No legs with asks found");
            return false;
        }
    };

    instruments.insert_instrument(Instrument {
        market_id: market_id.clone(),
        token_id: token_id.clone(),
        outcome: outcome.to_string(),
        condition_id: market_id.clone(),
        neg_risk,
        tick_size,
        rounding: RoundingConfig::from_tick_size_f64(tick_size),
        min_order_size: 1.0,
        max_order_size: 0.0,
        order_book_enabled: true,
        accepting_orders: true,
    });

    // FAK at 2x best ask — fills at best available, overpaying ensures crossing
    let buy_price = (best_ask * 2.0 * 1000.0).ceil() / 1000.0; // round up to tick
    // Size in USDC: $2 to ensure $1 minimum is met at actual fill price
    let buy_size = 2.0_f64;
    tracing::info!("[D6 Helper] Micro BUY: market={} token={}... best_ask={:.4} limit={:.4} size=${:.2}",
        market_id, &token_id[..token_id.len().min(20)], best_ask, buy_price, buy_size);

    let buy_legs = vec![(market_id.clone(), token_id.clone(), Side::Buy, buy_price, buy_size)];
    let ws_market_ids = vec![market_id.clone()];

    // Start WS User Channel BEFORE placing the buy — so we don't miss events.
    // Must be multi_thread so the WS task actually runs while we block on recv_timeout.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let ws_client = UserChannelClient::new();
    ws_client.start(&ws_auth, ws_market_ids, rt.handle());
    std::thread::sleep(std::time::Duration::from_millis(500)); // let WS connect

    // Place FAK micro buy — immediate fill
    let result = executor.execute_arb("d6_helper_buy", &buy_legs);
    let accepted = result.legs.iter()
        .any(|l| matches!(l, rust_engine::executor::OrderResult::Accepted(_)));
    if !accepted {
        tracing::error!("[D6 Helper] No buys accepted — venue state unchanged");
        ws_client.stop();
        return false;
    }
    tracing::info!("[D6 Helper] BUY accepted, listening for WS confirmation...");

    // Trade lifecycle: MATCHED → MINED → CONFIRMED (success) or RETRYING / FAILED
    // Track taker_order_ids through lifecycle.
    // Fallback: if MATCHED but no CONFIRMED after 60s, poll Data API to verify settlement.
    let mut matched_taker_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut done = false;
    let ws_start = std::time::Instant::now();
    let data_api_fallback_secs = 60;

    tracing::info!("[D6 Helper] Waiting for trade lifecycle (Data API fallback after {}s)...", data_api_fallback_secs);

    while !done {
        // If we have a MATCHED but no CONFIRMED after 60s, fall back to Data API
        if !matched_taker_ids.is_empty() && ws_start.elapsed().as_secs() > data_api_fallback_secs {
            tracing::info!("[D6 Helper] MATCHED but no WS CONFIRMED after {}s — checking Data API...",
                ws_start.elapsed().as_secs());
            // Check if the asset's position size changed on venue
            let positions_url = format!(
                "https://data-api.polymarket.com/positions?user={}&sizeThreshold=0",
                wallet_address.to_lowercase()
            );
            if let Ok(resp) = http_client.get(&positions_url)
                .header("User-Agent", "Mozilla/5.0")
                .send()
                .and_then(|r| r.json::<Vec<serde_json::Value>>())
            {
                let venue_shares: f64 = resp.iter()
                    .filter(|p| p.get("asset").and_then(|v| v.as_str()) == Some(&token_id))
                    .filter_map(|p| p.get("size").and_then(|v| v.as_f64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))))
                    .sum();
                tracing::info!("[D6 Helper] Data API: {} shares for token {}...",
                    venue_shares, &token_id[..token_id.len().min(20)]);
                if venue_shares > 0.0 {
                    tracing::info!("[D6 Helper] Data API confirms position exists — venue state changed");
                    done = true;
                    break;
                }
            }
            // If Data API also fails, keep waiting for WS
        }

        match ws_client.receiver.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(UserEvent::Trade(trade)) => {
                let tracked = matched_taker_ids.contains(&trade.taker_order_id);
                tracing::info!("[D6 Helper] WS trade: status={} taker={}... tracked={} size={} price={}",
                    trade.status, &trade.taker_order_id[..trade.taker_order_id.len().min(20)],
                    tracked, trade.size, trade.price);

                match trade.status.as_str() {
                    "MATCHED" => {
                        if !trade.taker_order_id.is_empty() {
                            matched_taker_ids.insert(trade.taker_order_id.clone());
                            tracing::info!("[D6 Helper] MATCHED — tracking taker_order_id ({})", matched_taker_ids.len());
                        }
                    }
                    "MINED" => {
                        if tracked {
                            tracing::info!("[D6 Helper] MINED — tx on-chain");
                        }
                    }
                    "CONFIRMED" => {
                        if tracked {
                            tracing::info!("[D6 Helper] CONFIRMED — venue state change verified");
                            done = true;
                        }
                    }
                    "RETRYING" => {
                        if tracked {
                            tracing::warn!("[D6 Helper] RETRYING — tx failed, resubmitting");
                        }
                    }
                    "FAILED" => {
                        if tracked {
                            tracing::error!("[D6 Helper] FAILED — trade permanently failed");
                            matched_taker_ids.remove(&trade.taker_order_id);
                        }
                    }
                    other => {
                        tracing::info!("[D6 Helper] Unknown trade status: {}", other);
                    }
                }
            }
            Ok(UserEvent::Order(_)) => {}
            Err(_) => {
                if ws_start.elapsed().as_secs() % 30 == 0 {
                    tracing::info!("[D6 Helper] Heartbeat: {}s, {} matched",
                        ws_start.elapsed().as_secs(), matched_taker_ids.len());
                }
            }
        }
    }

    ws_client.stop();
    tracing::info!("[D6 Helper] Venue state change confirmed after {}s", ws_start.elapsed().as_secs());

    true
}
