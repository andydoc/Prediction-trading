/// WS User Channel Investigation
///
/// 1. Subscribe to WS User Channel
/// 2. Place 10 micro FAK buys on cheap markets
/// 3. Capture ALL raw WS messages (full JSON)
/// 4. Sell all 10 positions
/// 5. Capture ALL raw WS messages
/// 6. Dump to JSON for field analysis

use std::path::PathBuf;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::collections::HashMap;

use clap::Parser;
use tokio::sync::mpsc;

use rust_engine::signing::{OrderSigner, ClobAuth, ClobApiCreds, Side};
use rust_engine::executor::{Executor, ExecutorConfig, OrderType, OrderAggression};
use rust_engine::instrument::{Instrument, InstrumentStore, RoundingConfig};
use rust_engine::rate_limiter::RateLimiter;

#[derive(Parser)]
struct Cli {
    #[arg(long, default_value = ".")]
    workspace: String,
    /// Number of micro buys
    #[arg(long, default_value = "10")]
    count: usize,
}

fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();
    let workspace = PathBuf::from(&cli.workspace);

    tracing_subscriber::fmt()
        .with_env_filter("ws_investigate=info,rust_engine=info")
        .init();

    tracing::info!("=== WS USER CHANNEL INVESTIGATION ===");
    tracing::info!("Will place {} micro buys, then sell, capturing all WS messages", cli.count);

    // Load secrets
    let secrets_path = workspace.join("config").join("secrets.yaml");
    let secrets: serde_json::Value = std::fs::read_to_string(&secrets_path)
        .ok()
        .and_then(|s| serde_yaml_ng::from_str(&s).ok())
        .unwrap_or_default();

    let poly = secrets.get("polymarket").expect("polymarket config");
    let private_key = poly.get("private_key").and_then(|v| v.as_str()).unwrap();
    let clob_host = poly.get("host").and_then(|v| v.as_str()).unwrap_or("https://clob.polymarket.com");
    let api_key = poly.get("clob_api_key").and_then(|v| v.as_str()).unwrap().to_string();
    let api_secret = poly.get("clob_api_secret").and_then(|v| v.as_str()).unwrap().to_string();
    let passphrase = poly.get("clob_passphrase").and_then(|v| v.as_str()).unwrap().to_string();

    let signer = OrderSigner::new(private_key).expect("signer");
    let wallet = format!("{:?}", signer.address());
    tracing::info!("Wallet: {}", wallet);

    let creds = ClobApiCreds {
        api_key: api_key.clone(),
        secret: api_secret.clone(),
        passphrase: passphrase.clone(),
    };
    let clob_auth = ClobAuth::new(&creds, &wallet).expect("clob auth");

    let instruments = Arc::new(InstrumentStore::new());
    let rate_limiter = Arc::new(RateLimiter::new());

    // Find cheap market: use LeBron Democratic nom (we know it has asks at 0.01)
    let http = reqwest::blocking::Client::new();

    // Discover markets from Data API
    let condition_id = "0x8b203037c7c0e21b500314f8398d2a8ea294b7ce1f4f9185f426425a3505bc45";
    let token_url = format!("{}/markets/{}", clob_host, condition_id);
    let market_resp: serde_json::Value = http.get(&token_url)
        .send().and_then(|r| r.json())
        .unwrap_or_default();

    let tokens = market_resp.get("tokens")
        .and_then(|v| v.as_array())
        .expect("tokens array");

    let yes_token = tokens.iter()
        .find(|t| t.get("outcome").and_then(|v| v.as_str()) == Some("Yes"))
        .expect("Yes token");
    let token_id = yes_token.get("token_id").and_then(|v| v.as_str()).unwrap().to_string();

    tracing::info!("Market: {} token: {}...", condition_id, &token_id[..token_id.len().min(20)]);

    // Register instrument
    instruments.insert_instrument(Instrument {
        market_id: condition_id.to_string(),
        token_id: token_id.clone(),
        outcome: "yes".to_string(),
        condition_id: condition_id.to_string(),
        neg_risk: true,
        tick_size: 0.001,
        rounding: RoundingConfig::from_tick_size_f64(0.001),
        min_order_size: 1.0,
        max_order_size: 0.0,
        order_book_enabled: true,
        accepting_orders: true,
    });

    // Create FAK executor for buys
    let buy_signer = OrderSigner::new(private_key).expect("buy signer");
    let buy_auth = ClobAuth::new(&creds, &wallet).expect("buy auth");
    let mut buy_executor = Executor::new(
        ExecutorConfig {
            clob_host: clob_host.to_string(),
            dry_run: false,
            order_type: OrderType::Fak,
            aggression: OrderAggression::AtMarket,
            fee_rate_bps: 0,
            confirmation_timeout_secs: 120.0,
        },
        buy_signer,
        Arc::clone(&instruments),
        Arc::clone(&rate_limiter),
    ).expect("buy executor");
    buy_executor.set_clob_auth(buy_auth);

    // Create GTC executor for sells (at 80% bid — just place on book)
    let sell_signer = OrderSigner::new(private_key).expect("sell signer");
    let sell_auth = ClobAuth::new(&creds, &wallet).expect("sell auth");
    let mut sell_executor = Executor::new(
        ExecutorConfig {
            clob_host: clob_host.to_string(),
            dry_run: false,
            order_type: OrderType::Gtc,
            aggression: OrderAggression::AtMarket,
            fee_rate_bps: 0,
            confirmation_timeout_secs: 120.0,
        },
        sell_signer,
        Arc::clone(&instruments),
        Arc::clone(&rate_limiter),
    ).expect("sell executor");
    sell_executor.set_clob_auth(sell_auth);

    // === Start WS with raw message capture ===
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");

    let (raw_tx, mut raw_rx) = mpsc::channel::<serde_json::Value>(1000);
    let running = Arc::new(AtomicBool::new(true));
    let running2 = Arc::clone(&running);

    // Spawn WS listener that captures raw messages
    rt.spawn(async move {
        use tokio_tungstenite::connect_async;
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        let url = "wss://ws-subscriptions-clob.polymarket.com/ws/user";
        let (ws, _) = connect_async(url).await.expect("WS connect");
        let (mut sink, mut stream) = ws.split();

        // Subscribe
        let sub = serde_json::json!({
            "auth": { "apiKey": api_key, "secret": api_secret, "passphrase": passphrase },
            "type": "user",
        });
        sink.send(WsMessage::Text(sub.to_string())).await.expect("subscribe");
        tracing::info!("[WS] Subscribed (no market filter)");

        let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(10));

        loop {
            if !running2.load(Ordering::Relaxed) { break; }

            tokio::select! {
                biased;
                _ = heartbeat.tick() => {
                    let _ = sink.send(WsMessage::Text("PING".to_string())).await;
                }
                msg = stream.next() => {
                    match msg {
                        Some(Ok(WsMessage::Text(text))) => {
                            if text == "PONG" || text.contains("\"type\":\"pong\"") { continue; }
                            // Parse and send all raw messages
                            if text.starts_with('[') {
                                if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&text) {
                                    for v in arr { let _ = raw_tx.send(v).await; }
                                }
                            } else if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                                let _ = raw_tx.send(v).await;
                            }
                        }
                        Some(Ok(WsMessage::Close(_))) | None => break,
                        _ => {}
                    }
                }
            }
        }
    });

    // Give WS time to connect
    std::thread::sleep(std::time::Duration::from_millis(1000));

    // Collect all raw messages in background
    let messages: Arc<parking_lot::Mutex<Vec<serde_json::Value>>> = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let messages2 = Arc::clone(&messages);
    rt.spawn(async move {
        while let Some(msg) = raw_rx.recv().await {
            messages2.lock().push(msg);
        }
    });

    // === Phase 1: Place N micro buys ===
    tracing::info!("=== PHASE 1: {} MICRO BUYS ===", cli.count);

    let mut buy_order_ids: Vec<String> = Vec::new();

    // Get best ask
    let book_url = format!("{}/book?token_id={}", clob_host, token_id);
    let best_ask: f64 = http.get(&book_url).send().ok()
        .and_then(|r| r.json::<serde_json::Value>().ok())
        .and_then(|b| b.get("asks")?.as_array()?.iter()
            .filter_map(|o| o.get("price")?.as_str()?.parse::<f64>().ok())
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)))
        .unwrap_or(0.0);

    let buy_price = (best_ask * 2.0 * 1000.0).ceil() / 1000.0;
    tracing::info!("Best ask: {:.4}, buy limit: {:.4}", best_ask, buy_price);

    for i in 0..cli.count {
        let legs = vec![(
            condition_id.to_string(),
            token_id.clone(),
            Side::Buy,
            buy_price,
            1.0, // $1 each
        )];

        let result = buy_executor.execute_arb(&format!("ws_inv_buy_{}", i), &legs);
        for leg in &result.legs {
            match leg {
                rust_engine::executor::OrderResult::Accepted(t) => {
                    tracing::info!("[BUY {}] Accepted: order_id={}", i, t.order_id);
                    buy_order_ids.push(t.order_id.clone());
                }
                rust_engine::executor::OrderResult::Rejected(e) => {
                    tracing::warn!("[BUY {}] Rejected: {:?}", i, e);
                }
            }
        }
        // Small delay to let WS events arrive between orders
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    tracing::info!("Placed {} buys, {} accepted. Waiting 60s for all WS events...",
        cli.count, buy_order_ids.len());
    std::thread::sleep(std::time::Duration::from_secs(60));

    let buy_msg_count = messages.lock().len();
    tracing::info!("Captured {} WS messages after buys", buy_msg_count);

    // === Phase 2: Sell all positions ===
    tracing::info!("=== PHASE 2: SELL ALL ===");

    // Get current position size from Data API
    let positions_url = format!(
        "https://data-api.polymarket.com/positions?user={}&sizeThreshold=0",
        wallet.to_lowercase()
    );
    let data_positions: Vec<serde_json::Value> = http.get(&positions_url)
        .header("User-Agent", "Mozilla/5.0")
        .send().and_then(|r| r.json()).unwrap_or_default();

    let venue_shares: f64 = data_positions.iter()
        .filter(|p| p.get("asset").and_then(|v| v.as_str()) == Some(&token_id))
        .filter_map(|p| p.get("size").and_then(|v| v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))))
        .sum();

    tracing::info!("Current venue shares: {:.2}", venue_shares);

    // Get best bid for sell price
    let best_bid: f64 = http.get(&book_url).send().ok()
        .and_then(|r| r.json::<serde_json::Value>().ok())
        .and_then(|b| b.get("bids")?.as_array()?.iter()
            .filter_map(|o| o.get("price")?.as_str()?.parse::<f64>().ok())
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)))
        .unwrap_or(0.0);

    let sell_price = ((best_bid * 0.8) * 1000.0).floor() / 1000.0;
    let sell_value = venue_shares * sell_price;
    tracing::info!("Sell: {:.2} shares at {:.4} (80% of bid {:.4}), value=${:.2}",
        venue_shares, sell_price, best_bid, sell_value);

    if sell_price > 0.0 && venue_shares > 0.0 {
        let legs = vec![(
            condition_id.to_string(),
            token_id.clone(),
            Side::Sell,
            sell_price,
            sell_value,
        )];

        let result = sell_executor.execute_arb("ws_inv_sell", &legs);
        for leg in &result.legs {
            match leg {
                rust_engine::executor::OrderResult::Accepted(t) => {
                    tracing::info!("[SELL] Accepted: order_id={}", t.order_id);
                }
                rust_engine::executor::OrderResult::Rejected(e) => {
                    tracing::warn!("[SELL] Rejected: {:?}", e);
                }
            }
        }
    }

    tracing::info!("Waiting 60s for sell WS events...");
    std::thread::sleep(std::time::Duration::from_secs(60));

    // === Phase 3: Stop and dump ===
    running.store(false, Ordering::Relaxed);
    std::thread::sleep(std::time::Duration::from_secs(2));

    let all_messages = messages.lock().clone();
    tracing::info!("=== CAPTURED {} TOTAL WS MESSAGES ===", all_messages.len());

    // Annotate each message with our known order IDs
    let output = serde_json::json!({
        "investigation": "ws_user_channel_field_trace",
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "our_buy_order_ids": buy_order_ids,
        "market_id": condition_id,
        "token_id": token_id,
        "buy_count": cli.count,
        "total_ws_messages": all_messages.len(),
        "messages": all_messages,
    });

    let output_path = workspace.join("data").join("ws_investigation.json");
    std::fs::write(&output_path, serde_json::to_string_pretty(&output).unwrap())
        .expect("write output");
    tracing::info!("Results written to {}", output_path.display());

    // Quick field analysis
    tracing::info!("=== FIELD ANALYSIS ===");
    let mut field_values: HashMap<String, Vec<String>> = HashMap::new();
    for msg in &all_messages {
        if let Some(obj) = msg.as_object() {
            for (key, val) in obj {
                field_values.entry(key.clone()).or_default()
                    .push(val.to_string());
            }
        }
    }
    for (field, values) in &field_values {
        let unique: std::collections::HashSet<&String> = values.iter().collect();
        tracing::info!("  {}: {} occurrences, {} unique values", field, values.len(), unique.len());
    }

    tracing::info!("=== DONE ===");
}
