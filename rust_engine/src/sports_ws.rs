/// Sports WebSocket manager — persistent connection to Polymarket's Sports WS.
///
/// Receives real-time game status updates (scores, periods, postponements, cancellations)
/// for all active sports events. Used to pre-screen postponement detection before
/// expensive AI calls.
///
/// Architecture:
///   - Single connection to wss://sports-api.polymarket.com/ws
///   - No auth, no subscription — connect and start receiving
///   - Server sends ping every 5s, client must respond with pong within 10s
///   - Reconnect with exponential backoff (1s → 60s)
///   - Maintains HashMap<game_id, GameState> updated on each message
///   - Matching: cross-reference game team names against market question text
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use crate::cached_db::CachedSqliteDB;

/// Statuses that indicate a game is postponed, cancelled, or otherwise disrupted.
const POSTPONEMENT_STATUSES: &[&str] = &[
    "Postponed", "postponed",
    "Canceled", "canceled", "cancelled",
    "Forfeit",
    "Suspended",
    "Delayed",
    "NotNecessary",
];

/// Statuses that indicate a game is actively in progress (not postponed).
const ACTIVE_STATUSES: &[&str] = &[
    "InProgress", "inprogress", "running",
    "Final", "F/OT", "finished",
    "Break", "PenaltyShootout",
];

/// A tracked game from the Sports WebSocket.
#[derive(Debug, Clone)]
pub struct GameState {
    pub game_id: String,
    pub league: String,
    pub home_team: String,
    pub away_team: String,
    pub status: String,
    pub live: bool,
    pub ended: bool,
    pub last_updated: f64,
    pub finished_timestamp: Option<String>,
}

/// Result of checking a position against the sports feed.
#[derive(Debug, Clone)]
pub enum SportsCheckResult {
    /// Game found and status indicates postponement/cancellation.
    Postponed {
        game_id: String,
        status: String,
        league: String,
    },
    /// Game found and is actively in progress or finished — not postponed.
    Active {
        game_id: String,
        status: String,
    },
    /// No matching game found in sports feed — fall through to AI.
    NoMatch,
}

/// Configuration for the Sports WebSocket.
#[derive(Debug, Clone)]
pub struct SportsWsConfig {
    pub enabled: bool,
    pub url: String,
    pub reconnect_base_delay: f64,
    pub reconnect_max_delay: f64,
}

impl Default for SportsWsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: "wss://sports-api.polymarket.com/ws".into(),
            reconnect_base_delay: 1.0,
            reconnect_max_delay: 60.0,
        }
    }
}

/// Manages a persistent connection to the Polymarket Sports WebSocket.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sports_games (
    game_id TEXT PRIMARY KEY,
    league TEXT NOT NULL,
    home_team TEXT NOT NULL,
    away_team TEXT NOT NULL,
    status TEXT NOT NULL,
    live INTEGER NOT NULL DEFAULT 0,
    ended INTEGER NOT NULL DEFAULT 0,
    last_updated REAL NOT NULL,
    finished_timestamp TEXT
);
";

pub struct SportsWsManager {
    config: SportsWsConfig,
    /// game_id → latest GameState
    games: Arc<Mutex<HashMap<String, GameState>>>,
    /// SQLite persistence (in-memory + disk backup)
    db: Arc<CachedSqliteDB>,
    /// Total messages received (for dashboard)
    total_msgs: Arc<AtomicU64>,
    /// Whether the WS is currently connected
    connected: Arc<AtomicBool>,
    /// Shutdown signal
    running: Arc<AtomicBool>,
}

impl SportsWsManager {
    pub fn new(config: SportsWsConfig, db_path: &str) -> Self {
        let db = CachedSqliteDB::new(db_path, SCHEMA)
            .expect("Failed to create sports_ws SQLite");

        let games = Arc::new(Mutex::new(HashMap::new()));

        // Load from disk if available
        if db.disk_exists() {
            match db.load_from_disk() {
                Ok(ms) => {
                    tracing::info!("Sports WS: loaded state from disk [{ms:.0}ms]");
                    // Populate HashMap from SQLite
                    let conn = db.conn();
                    let mut stmt = conn.prepare(
                        "SELECT game_id, league, home_team, away_team, status, live, ended, last_updated, finished_timestamp FROM sports_games"
                    ).expect("prepare select");
                    let rows = stmt.query_map([], |row| {
                        Ok(GameState {
                            game_id: row.get(0)?,
                            league: row.get(1)?,
                            home_team: row.get(2)?,
                            away_team: row.get(3)?,
                            status: row.get(4)?,
                            live: row.get::<_, i32>(5)? != 0,
                            ended: row.get::<_, i32>(6)? != 0,
                            last_updated: row.get(7)?,
                            finished_timestamp: row.get(8)?,
                        })
                    }).expect("query sports_games");
                    let mut map = games.lock();
                    for row in rows.flatten() {
                        map.insert(row.game_id.clone(), row);
                    }
                    tracing::info!("Sports WS: restored {} games from disk", map.len());
                }
                Err(e) => tracing::warn!("Sports WS: failed to load from disk: {}", e),
            }
        }

        Self {
            config,
            games,
            db: Arc::new(db),
            total_msgs: Arc::new(AtomicU64::new(0)),
            connected: Arc::new(AtomicBool::new(false)),
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Start the Sports WS connection loop in a background tokio task.
    /// Must be called from within a tokio runtime.
    pub fn start(&self, runtime: &tokio::runtime::Handle) {
        if !self.config.enabled {
            tracing::info!("Sports WS disabled");
            return;
        }
        self.running.store(true, Ordering::SeqCst);

        let url = self.config.url.clone();
        let base_delay = self.config.reconnect_base_delay;
        let max_delay = self.config.reconnect_max_delay;
        let games = Arc::clone(&self.games);
        let total_msgs = Arc::clone(&self.total_msgs);
        let connected = Arc::clone(&self.connected);
        let running = Arc::clone(&self.running);

        runtime.spawn(async move {
            let mut delay = base_delay;
            loop {
                if !running.load(Ordering::SeqCst) { break; }

                match run_sports_ws(&url, &games, &total_msgs, &connected, &running).await {
                    Ok(()) => {
                        tracing::info!("Sports WS: clean disconnect");
                        delay = base_delay; // reset backoff
                    }
                    Err(e) => {
                        tracing::warn!("Sports WS: connection error: {}", e);
                    }
                }

                connected.store(false, Ordering::SeqCst);
                if !running.load(Ordering::SeqCst) { break; }

                tracing::info!("Sports WS: reconnecting in {:.1}s", delay);
                tokio::time::sleep(Duration::from_secs_f64(delay)).await;
                delay = (delay * 2.0).min(max_delay);
            }
        });
    }

    /// Check if a position's markets match any game in the sports feed.
    /// Returns the game status if matched.
    pub fn check_postponement(&self, market_names: &[String]) -> SportsCheckResult {
        let games = self.games.lock();
        if games.is_empty() {
            return SportsCheckResult::NoMatch;
        }

        // Normalize market names to lowercase for matching
        let names_lower: Vec<String> = market_names.iter()
            .map(|n| n.to_lowercase())
            .collect();

        for game in games.values() {
            let home_lower = game.home_team.to_lowercase();
            let away_lower = game.away_team.to_lowercase();

            // Skip games with empty team names
            if home_lower.is_empty() || away_lower.is_empty() { continue; }
            // Skip very short team names (< 3 chars) to avoid false matches
            if home_lower.len() < 3 || away_lower.len() < 3 { continue; }

            // Check if both team names appear in any market question
            let matched = names_lower.iter().any(|name| {
                name.contains(&home_lower) && name.contains(&away_lower)
            });

            if matched {
                // Check postponement status
                let status_str = game.status.as_str();
                if POSTPONEMENT_STATUSES.iter().any(|&s| s == status_str) {
                    return SportsCheckResult::Postponed {
                        game_id: game.game_id.clone(),
                        status: game.status.clone(),
                        league: game.league.clone(),
                    };
                }
                if ACTIVE_STATUSES.iter().any(|&s| s == status_str) || game.live || game.ended {
                    return SportsCheckResult::Active {
                        game_id: game.game_id.clone(),
                        status: game.status.clone(),
                    };
                }
            }
        }

        SportsCheckResult::NoMatch
    }

    /// Number of tracked games.
    pub fn game_count(&self) -> usize {
        self.games.lock().len()
    }

    /// Number of games with postponement status.
    pub fn postponed_count(&self) -> usize {
        self.games.lock().values()
            .filter(|g| POSTPONEMENT_STATUSES.iter().any(|&s| s == g.status.as_str()))
            .count()
    }

    /// Whether the Sports WS is currently connected.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Total messages received.
    pub fn total_messages(&self) -> u64 {
        self.total_msgs.load(Ordering::SeqCst)
    }

    /// Prune games that are ended and older than the given age (seconds).
    /// Returns count of pruned entries.
    pub fn prune_ended(&self, max_age_secs: f64) -> usize {
        let now = now_secs();
        let mut games = self.games.lock();
        let before = games.len();
        games.retain(|_, g| {
            // Keep if not ended, or if ended recently
            !g.ended || (now - g.last_updated) < max_age_secs
        });
        before - games.len()
    }

    /// Sync in-memory HashMap to the SQLite in-memory DB.
    fn sync_to_db(&self) {
        let games = self.games.lock();
        let conn = self.db.conn();
        let _ = conn.execute("DELETE FROM sports_games", []);
        let mut stmt = conn.prepare_cached(
            "INSERT INTO sports_games (game_id, league, home_team, away_team, status, live, ended, last_updated, finished_timestamp) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
        ).expect("prepare insert");
        for g in games.values() {
            let _ = stmt.execute(rusqlite::params![
                g.game_id, g.league, g.home_team, g.away_team, g.status,
                g.live as i32, g.ended as i32, g.last_updated,
                g.finished_timestamp.as_deref(),
            ]);
        }
    }

    /// Persist to disk. Called by the orchestrator's periodic state save.
    pub fn mirror_to_disk(&self) {
        self.sync_to_db();
        self.db.mirror_to_disk();
    }

    /// Stop the Sports WS connection.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

/// Run the Sports WS connection loop (single connection attempt).
async fn run_sports_ws(
    url: &str,
    games: &Arc<Mutex<HashMap<String, GameState>>>,
    total_msgs: &Arc<AtomicU64>,
    connected: &Arc<AtomicBool>,
    running: &Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (ws_stream, _) = tokio_tungstenite::connect_async(url).await?;
    let (mut sink, mut stream) = ws_stream.split();

    connected.store(true, Ordering::SeqCst);
    tracing::info!("Sports WS: connected to {}", url);

    // Sports WS: server sends ping every 5s, we must respond with pong within 10s
    // tokio-tungstenite auto-responds to protocol-level pings, but Polymarket
    // may use text-based PING/PONG like the market channel
    let mut last_activity = std::time::Instant::now();
    let activity_timeout = Duration::from_secs(30); // no message for 30s = dead

    loop {
        if !running.load(Ordering::SeqCst) { break; }

        let msg = tokio::time::timeout(Duration::from_secs(10), stream.next()).await;

        match msg {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                last_activity = std::time::Instant::now();

                // Handle text-based PING (Polymarket pattern)
                let trimmed = text.trim();
                if trimmed.eq_ignore_ascii_case("ping") {
                    let _ = sink.send(WsMessage::Text("PONG".into())).await;
                    continue;
                }

                // Parse game state update
                if let Ok(data) = serde_json::from_str::<Value>(&text) {
                    total_msgs.fetch_add(1, Ordering::Relaxed);
                    if let Some(state) = parse_game_state(&data) {
                        games.lock().insert(state.game_id.clone(), state);
                    }
                }
            }
            Ok(Some(Ok(WsMessage::Ping(payload)))) => {
                last_activity = std::time::Instant::now();
                let _ = sink.send(WsMessage::Pong(payload)).await;
            }
            Ok(Some(Ok(WsMessage::Pong(_)))) => {
                last_activity = std::time::Instant::now();
            }
            Ok(Some(Ok(WsMessage::Close(_)))) => {
                tracing::info!("Sports WS: server sent close frame");
                break;
            }
            Ok(Some(Err(e))) => {
                tracing::warn!("Sports WS: message error: {}", e);
                break;
            }
            Ok(None) => {
                tracing::info!("Sports WS: stream ended");
                break;
            }
            Err(_) => {
                // Timeout — check if connection is still alive
                if last_activity.elapsed() > activity_timeout {
                    tracing::warn!("Sports WS: no activity for {}s, reconnecting",
                        last_activity.elapsed().as_secs());
                    break;
                }
            }
            _ => {}
        }
    }

    connected.store(false, Ordering::SeqCst);
    Ok(())
}

/// Parse a Sports WS JSON message into a GameState.
fn parse_game_state(data: &Value) -> Option<GameState> {
    // Two formats observed:
    // 1. Full: { gameId: 123, leagueAbbreviation, homeTeam, awayTeam, status, ... }
    // 2. Metadata: { metadataGameId: "id...", score, period, live, ended }
    let game_id = data.get("gameId")
        .and_then(|v| match v {
            Value::Number(n) => Some(n.to_string()),
            Value::String(s) => Some(s.clone()),
            _ => None,
        })
        .or_else(|| data.get("metadataGameId").and_then(|v| v.as_str()).map(|s| s.to_string()))?;

    let league = data.get("leagueAbbreviation")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let home_team = data.get("homeTeam")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let away_team = data.get("awayTeam")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let status = data.get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let live = data.get("live")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let ended = data.get("ended")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let finished_timestamp = data.get("finished_timestamp")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Some(GameState {
        game_id,
        league,
        home_team,
        away_team,
        status,
        live,
        ended,
        last_updated: now_secs(),
        finished_timestamp,
    })
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_game_state() {
        let data = serde_json::json!({
            "gameId": 5265223,
            "leagueAbbreviation": "nfl",
            "homeTeam": "Buffalo Bills",
            "awayTeam": "Kansas City Chiefs",
            "status": "InProgress",
            "score": "14-7",
            "period": "Q2",
            "live": true,
            "ended": false,
        });
        let state = parse_game_state(&data).unwrap();
        assert_eq!(state.game_id, "5265223");
        assert_eq!(state.league, "nfl");
        assert_eq!(state.home_team, "Buffalo Bills");
        assert_eq!(state.away_team, "Kansas City Chiefs");
        assert_eq!(state.status, "InProgress");
        assert!(state.live);
        assert!(!state.ended);
    }

    #[test]
    fn parse_metadata_game_state() {
        let data = serde_json::json!({
            "metadataGameId": "id2705070470147650",
            "leagueAbbreviation": "",
            "score": "0-0",
            "period": "Live",
            "live": true,
            "ended": false,
        });
        let state = parse_game_state(&data).unwrap();
        assert_eq!(state.game_id, "id2705070470147650");
        assert!(state.home_team.is_empty());
    }

    #[test]
    fn check_postponement_match() {
        let mgr = SportsWsManager::new(SportsWsConfig::default(), "/tmp/sports_ws_test.db");
        {
            let mut games = mgr.games.lock();
            games.insert("123".into(), GameState {
                game_id: "123".into(),
                league: "nfl".into(),
                home_team: "Buffalo Bills".into(),
                away_team: "Kansas City Chiefs".into(),
                status: "Postponed".into(),
                live: false,
                ended: false,
                last_updated: now_secs(),
                finished_timestamp: None,
            });
        }
        let result = mgr.check_postponement(&[
            "Will the Buffalo Bills beat the Kansas City Chiefs?".into(),
        ]);
        match result {
            SportsCheckResult::Postponed { status, .. } => {
                assert_eq!(status, "Postponed");
            }
            _ => panic!("Expected Postponed, got {:?}", result),
        }
    }

    #[test]
    fn check_postponement_active() {
        let mgr = SportsWsManager::new(SportsWsConfig::default(), "/tmp/sports_ws_test.db");
        {
            let mut games = mgr.games.lock();
            games.insert("456".into(), GameState {
                game_id: "456".into(),
                league: "nba".into(),
                home_team: "Los Angeles Lakers".into(),
                away_team: "Boston Celtics".into(),
                status: "InProgress".into(),
                live: true,
                ended: false,
                last_updated: now_secs(),
                finished_timestamp: None,
            });
        }
        let result = mgr.check_postponement(&[
            "Will the Los Angeles Lakers beat the Boston Celtics?".into(),
        ]);
        matches!(result, SportsCheckResult::Active { .. });
    }

    #[test]
    fn check_postponement_no_match() {
        let mgr = SportsWsManager::new(SportsWsConfig::default(), "/tmp/sports_ws_test.db");
        let result = mgr.check_postponement(&[
            "Will Bitcoin hit $100k?".into(),
        ]);
        matches!(result, SportsCheckResult::NoMatch);
    }

    #[test]
    fn prune_ended_games() {
        let mgr = SportsWsManager::new(SportsWsConfig::default(), "/tmp/sports_ws_test.db");
        {
            let mut games = mgr.games.lock();
            games.insert("old".into(), GameState {
                game_id: "old".into(),
                league: "nfl".into(),
                home_team: "Team A".into(),
                away_team: "Team B".into(),
                status: "Final".into(),
                live: false,
                ended: true,
                last_updated: now_secs() - 100000.0, // old
                finished_timestamp: None,
            });
            games.insert("recent".into(), GameState {
                game_id: "recent".into(),
                league: "nba".into(),
                home_team: "Team C".into(),
                away_team: "Team D".into(),
                status: "InProgress".into(),
                live: true,
                ended: false,
                last_updated: now_secs(),
                finished_timestamp: None,
            });
        }
        let pruned = mgr.prune_ended(86400.0); // 24h
        assert_eq!(pruned, 1);
        assert_eq!(mgr.game_count(), 1);
    }
}
