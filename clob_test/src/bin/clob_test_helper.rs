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
    // Parse the first serialized position to get market data
    let pos_json = match checkpoint.open_positions_json.first() {
        Some(j) => j,
        None => {
            tracing::warn!("[D6 Helper] No serialized positions in checkpoint, skipping close");
            return false;
        }
    };

    let pos: serde_json::Value = match serde_json::from_str(pos_json) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("[D6 Helper] Failed to parse position JSON: {}", e);
            return false;
        }
    };

    tracing::info!("[D6 Helper] Position to close: {}", pos.get("position_id")
        .and_then(|v| v.as_str()).unwrap_or("unknown"));

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
            order_type: OrderType::Gtc,
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

    // Wait for on-chain confirmation: poll Data API until positions show confirmed shares.
    // The main process bought shares via GTC orders — they need to settle on-chain
    // before the helper can sell them.
    let http = reqwest::blocking::Client::new();
    let positions_url = format!(
        "https://data-api.polymarket.com/positions?user={}&sizeThreshold=0",
        wallet_address.to_lowercase()
    );

    // Extract the token_id of the position we want to sell
    let target_token_id = {
        let markets = pos.get("markets").and_then(|v| v.as_object());
        markets.and_then(|m| m.values().next())
            .and_then(|leg| leg.get("token_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };

    tracing::info!("[D6 Helper] Waiting for on-chain confirmation of token {}...", &target_token_id[..target_token_id.len().min(20)]);

    let mut confirmed_shares = 0.0f64;
    for attempt in 1..=12 {  // Max 60s (12 × 5s)
        let data_api_positions: Vec<serde_json::Value> = http.get(&positions_url)
            .send().and_then(|r| r.json()).unwrap_or_default();

        confirmed_shares = data_api_positions.iter()
            .filter(|dp| dp.get("asset").and_then(|v| v.as_str()) == Some(&target_token_id))
            .filter_map(|dp| dp.get("size").and_then(|v| v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))))
            .sum();

        if confirmed_shares > 0.0 {
            tracing::info!("[D6 Helper] Confirmed: {:.2} shares on-chain (attempt {})", confirmed_shares, attempt);
            break;
        }
        tracing::info!("[D6 Helper] Waiting for confirmed shares... (attempt {}/12)", attempt);
        std::thread::sleep(std::time::Duration::from_secs(5));
    }

    if confirmed_shares <= 0.0 {
        tracing::error!("[D6 Helper] No confirmed shares after 60s — cannot sell. Proceeding to restart.");
        // Clear D6 flag and restart main without selling
        ipc::clear_d6_flag(workspace);
        return false;
    }

    // Log all confirmed positions
    let data_api_positions: Vec<serde_json::Value> = http.get(&positions_url)
        .send().and_then(|r| r.json()).unwrap_or_default();
    tracing::info!("[D6 Helper] Data API shows {} positions:", data_api_positions.len());
    for dp in &data_api_positions {
        let title = dp.get("title").and_then(|v| v.as_str()).unwrap_or("?");
        let size = dp.get("size").and_then(|v| v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0.0);
        let asset = dp.get("asset").and_then(|v| v.as_str()).unwrap_or("");
        tracing::info!("[D6 Helper]   {} size={:.2} asset={}...", &title[..title.len().min(40)], size, &asset[..asset.len().min(20)]);
    }

    // Extract market legs from the position JSON
    let markets = pos.get("markets").and_then(|v| v.as_object());
    let markets = match markets {
        Some(m) => m,
        None => {
            tracing::warn!("[D6 Helper] No markets in position, cannot sell");
            return false;
        }
    };

    // Sell each leg of the first position
    let mut sell_legs = Vec::new();
    for (market_id, leg) in markets {
        let outcome = leg.get("outcome").and_then(|v| v.as_str()).unwrap_or("yes");
        let checkpoint_shares = leg.get("shares").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if checkpoint_shares <= 0.0 { continue; }

        // Read token_id from serialized MarketLeg (populated by fill_tracker)
        let token_id = leg.get("token_id").and_then(|v| v.as_str()).unwrap_or("").to_string();

        // Sell minimum volume at market (FAK at best bid) to guarantee immediate fill.
        // We only need to prove venue state changed — smallest possible trade.
        let confirmed_shares = data_api_positions.iter()
            .find(|dp| dp.get("asset").and_then(|v| v.as_str()) == Some(&token_id))
            .and_then(|dp| dp.get("size").and_then(|v| v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))))
            .unwrap_or(checkpoint_shares);
        // Minimum sell: enough shares to make $1 at best bid (Polymarket minimum)
        // We'll calculate this after fetching the bid price below
        tracing::info!("[D6 Helper] Shares: checkpoint={:.2}, confirmed={:.2}",
            checkpoint_shares, confirmed_shares);
        if token_id.is_empty() {
            tracing::warn!("[D6 Helper] No token_id for market {} in checkpoint. \
                Was fill_tracker used to enter this position?", market_id);
            continue;
        }
        let neg_risk = true; // All test markets are negRisk
        let tick_size = 0.001;

        // Register instrument
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

        // Sell at best bid (FAK) — minimum volume to guarantee immediate fill.
        // Only need to prove venue state changed during D6 shutdown.
        let best_bid = {
            let book_url = format!("{}/book?token_id={}", clob_host, token_id);
            reqwest::blocking::Client::new().get(&book_url).send().ok()
                .and_then(|r| r.json::<serde_json::Value>().ok())
                .and_then(|b| b.get("bids")?.as_array()?.iter()
                    .filter_map(|o| o.get("price")?.as_str()?.parse::<f64>().ok())
                    .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)))
                .unwrap_or(0.0)
        };
        if best_bid <= 0.0 {
            tracing::warn!("[D6 Helper] No bids for market {}, skipping", market_id);
            continue;
        }
        // Minimum shares to meet $1 minimum order size at best bid
        let min_shares = (1.0 / best_bid).ceil();
        let sell_shares = min_shares.min(confirmed_shares); // Don't sell more than we have
        let sell_price = best_bid; // At market — guarantees immediate fill
        let sell_value = sell_shares * sell_price;
        tracing::info!("[D6 Helper] SELL leg: market={} shares={:.2} (min for $1) best_bid={:.4} price={:.4} value={:.4}",
            market_id, sell_shares, best_bid, sell_price, sell_value);

        sell_legs.push((market_id.clone(), token_id.to_string(), Side::Sell, sell_price, sell_value));
    }

    if sell_legs.is_empty() {
        tracing::warn!("[D6 Helper] No legs to sell");
        return false;
    }

    // Collect market_ids for WS subscription
    let ws_market_ids: Vec<String> = sell_legs.iter().map(|(mid, _, _, _, _)| mid.clone()).collect();

    // Start WS User Channel BEFORE placing the sell — so we don't miss events
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let ws_client = UserChannelClient::new();
    ws_client.start(&ws_auth, ws_market_ids, rt.handle());
    std::thread::sleep(std::time::Duration::from_millis(500)); // let WS connect

    // Place FAK sell
    let result = executor.execute_arb("d6_helper_close", &sell_legs);
    let accepted = result.legs.iter()
        .filter(|l| matches!(l, rust_engine::executor::OrderResult::Accepted(_)))
        .count();
    tracing::info!("[D6 Helper] SELL result: {}/{} legs accepted", accepted, sell_legs.len());

    if accepted == 0 {
        tracing::error!("[D6 Helper] No sells accepted — venue state unchanged");
        ws_client.stop();
        return false;
    }

    // Wait for WS confirmation: MATCHED then CONFIRMED on any trade.
    // Patient wait — sell is GTC at best bid, may take hours/days to fill on illiquid markets.
    let wait_hours = 48;
    tracing::info!("[D6 Helper] Waiting for WS User Channel trade confirmation ({}h timeout)...", wait_hours);
    let mut saw_matched = false;
    let mut saw_confirmed = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait_hours * 3600);

    while !saw_confirmed {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            tracing::warn!("[D6 Helper] Timeout waiting for trade confirmation (matched={}, confirmed={})",
                saw_matched, saw_confirmed);
            break;
        }

        match ws_client.receiver.recv_timeout(remaining) {
            Ok(UserEvent::Trade(trade)) => {
                tracing::info!("[D6 Helper] WS trade: asset={}... status={} size={} price={}",
                    &trade.asset_id[..trade.asset_id.len().min(20)], trade.status, trade.size, trade.price);
                if trade.status == "MATCHED" {
                    saw_matched = true;
                    tracing::info!("[D6 Helper] MATCHED — venue state change initiated");
                }
                if trade.status == "CONFIRMED" && saw_matched {
                    saw_confirmed = true;
                    tracing::info!("[D6 Helper] CONFIRMED — venue state change verified");
                }
            }
            Ok(UserEvent::Order(_)) => {}
            Err(_) => break,
        }
    }

    ws_client.stop();

    if saw_confirmed {
        tracing::info!("[D6 Helper] Venue state change confirmed via WS (MATCHED → CONFIRMED)");
    } else if saw_matched {
        tracing::warn!("[D6 Helper] Got MATCHED but not CONFIRMED");
    } else {
        tracing::error!("[D6 Helper] No trade signals received after {}h", wait_hours);
    }

    saw_confirmed
}
