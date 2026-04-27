/// SQLite state persistence — in-memory with periodic disk backup.
///
/// Atomic `mirror_to_disk()` runs on a background thread, never blocking
/// the orchestrator tick loop.
///
/// Schema:
///   state(key TEXT PK, value TEXT)        — scalars: capital, metrics
///   positions(position_id TEXT PK, status TEXT, data JSON, opened_at, closed_at)
use rusqlite::params;
use parking_lot::Mutex;
use crate::cached_db::CachedSqliteDB;

const STATE_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS scalars (
        key TEXT PRIMARY KEY,
        value REAL NOT NULL
    );
    CREATE TABLE IF NOT EXISTS positions (
        position_id TEXT PRIMARY KEY,
        status TEXT NOT NULL DEFAULT 'open',
        data TEXT NOT NULL,
        opened_at TEXT,
        closed_at TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_pos_status ON positions(status);
    CREATE TABLE IF NOT EXISTS delay_p95 (
        category TEXT PRIMARY KEY,
        p95_hours REAL NOT NULL,
        count INTEGER NOT NULL DEFAULT 0,
        median_hours REAL NOT NULL DEFAULT 0,
        p75_hours REAL NOT NULL DEFAULT 0,
        pct_over_24h REAL NOT NULL DEFAULT 0,
        updated_at TEXT
    );
    CREATE TABLE IF NOT EXISTS daily_reports (
        report_date TEXT PRIMARY KEY,
        timestamp REAL NOT NULL,
        entries INTEGER NOT NULL DEFAULT 0,
        exits INTEGER NOT NULL DEFAULT 0,
        fees REAL NOT NULL DEFAULT 0,
        net_pnl REAL NOT NULL DEFAULT 0,
        capital_util_pct REAL NOT NULL DEFAULT 0,
        drawdown_pct REAL NOT NULL DEFAULT 0,
        data TEXT
    );
    CREATE TABLE IF NOT EXISTS strategy_portfolios (
        name TEXT PRIMARY KEY,
        current_capital REAL NOT NULL,
        total_entered INTEGER NOT NULL DEFAULT 0,
        total_wins INTEGER NOT NULL DEFAULT 0,
        total_losses INTEGER NOT NULL DEFAULT 0,
        evals_seen INTEGER NOT NULL DEFAULT 0,
        evals_rejected INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE IF NOT EXISTS strategy_open_positions (
        strategy_name TEXT NOT NULL,
        constraint_id TEXT NOT NULL,
        data TEXT NOT NULL,
        PRIMARY KEY (strategy_name, constraint_id)
    );
    CREATE TABLE IF NOT EXISTS strategy_closed_positions (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        strategy_name TEXT NOT NULL,
        capital_deployed REAL NOT NULL,
        actual_profit REAL NOT NULL,
        actual_profit_pct REAL NOT NULL,
        entry_ts REAL NOT NULL,
        close_ts REAL NOT NULL,
        is_win INTEGER NOT NULL,
        short_name TEXT NOT NULL DEFAULT '',
        method TEXT NOT NULL DEFAULT '',
        is_sell INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX IF NOT EXISTS idx_strat_closed_ts ON strategy_closed_positions(close_ts);
    CREATE TABLE IF NOT EXISTS journal (
        id INTEGER PRIMARY KEY,
        timestamp REAL NOT NULL,
        account TEXT NOT NULL,
        debit REAL NOT NULL DEFAULT 0.0,
        credit REAL NOT NULL DEFAULT 0.0,
        position_id TEXT,
        description TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_journal_ts ON journal(timestamp);
    CREATE TABLE IF NOT EXISTS instruments (
        token_id TEXT PRIMARY KEY,
        market_id TEXT NOT NULL,
        outcome TEXT NOT NULL,
        condition_id TEXT NOT NULL,
        neg_risk INTEGER NOT NULL DEFAULT 0,
        tick_size REAL NOT NULL DEFAULT 0.01,
        min_order_size REAL NOT NULL DEFAULT 1.0,
        max_order_size REAL NOT NULL DEFAULT 0.0,
        order_book_enabled INTEGER NOT NULL DEFAULT 1,
        accepting_orders INTEGER NOT NULL DEFAULT 1
    );
    CREATE INDEX IF NOT EXISTS idx_instr_market ON instruments(market_id);
    CREATE TABLE IF NOT EXISTS checkpoints (
        key TEXT PRIMARY KEY,
        data TEXT NOT NULL,
        updated_at TEXT
    );
    CREATE TABLE IF NOT EXISTS evaluated_opportunities (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        ts REAL NOT NULL,
        constraint_id TEXT NOT NULL,
        method TEXT NOT NULL,
        num_legs INTEGER NOT NULL,
        expected_profit REAL NOT NULL,
        expected_profit_pct REAL NOT NULL,
        total_capital_required REAL NOT NULL,
        hours_to_resolve REAL NOT NULL,
        score REAL NOT NULL,
        entered INTEGER NOT NULL DEFAULT 0,
        rejected_reason TEXT,
        strategy_accepted TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_eo_ts ON evaluated_opportunities(ts);
    CREATE INDEX IF NOT EXISTS idx_eo_profit ON evaluated_opportunities(expected_profit_pct);
";

pub struct StateDB {
    /// Generic cached SQLite (in-memory + disk backup)
    db: CachedSqliteDB,
    /// Count of dirty positions since last mirror
    dirty_count: Mutex<usize>,
}

impl StateDB {
    pub fn new(disk_path: &str) -> Result<Self, String> {
        let db = CachedSqliteDB::new(disk_path, STATE_SCHEMA)?;

        // Migrations: add columns that may not exist in older DBs
        {
            let conn = db.conn();
            for (table, col, col_type) in &[
                ("strategy_closed_positions", "short_name", "TEXT NOT NULL DEFAULT ''"),
                ("strategy_closed_positions", "method", "TEXT NOT NULL DEFAULT ''"),
                ("strategy_closed_positions", "is_sell", "INTEGER NOT NULL DEFAULT 0"),
            ] {
                let _ = conn.execute(&format!("ALTER TABLE {} ADD COLUMN {} {}", table, col, col_type), []);
            }
        } // conn borrow dropped

        Ok(Self {
            db,
            dirty_count: Mutex::new(0),
        })
    }

    // --- Scalar state (capital, counters) ---

    pub fn set_scalar(&self, key: &str, value: f64) {
        let db = self.db.conn();
        let _ = db.execute(
            "INSERT OR REPLACE INTO scalars (key, value) VALUES (?1, ?2)",
            params![key, value],
        );
    }

    pub fn get_scalar(&self, key: &str) -> Option<f64> {
        let db = self.db.conn();
        db.query_row(
            "SELECT value FROM scalars WHERE key = ?1",
            params![key],
            |row| row.get(0),
        ).ok()
    }

    pub fn set_scalars(&self, pairs: &[(String, f64)]) {
        let db = self.db.conn();
        for (k, v) in pairs {
            let _ = db.execute(
                "INSERT OR REPLACE INTO scalars (key, value) VALUES (?1, ?2)",
                params![k, v],
            );
        }
    }

    pub fn get_all_scalars(&self) -> Vec<(String, f64)> {
        let db = self.db.conn();
        let mut stmt = match db.prepare("SELECT key, value FROM scalars") {
            Ok(s) => s,
            Err(e) => { tracing::warn!("get_all_scalars prepare failed: {}", e); return Vec::new(); }
        };
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        });
        match rows {
            Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
            Err(e) => { tracing::warn!("get_all_scalars query failed: {}", e); Vec::new() }
        }
    }

    // --- Position CRUD ---

    pub fn save_position(&self, position_id: &str, status: &str, data_json: &str,
                         opened_at: Option<&str>, closed_at: Option<&str>) {
        let db = self.db.conn();
        let _ = db.execute(
            "INSERT OR REPLACE INTO positions (position_id, status, data, opened_at, closed_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![position_id, status, data_json, opened_at, closed_at],
        );
        *self.dirty_count.lock() += 1;
    }

    pub fn save_positions_bulk(&self, rows: &[(String, String, String, Option<String>, Option<String>)]) {
        let db = self.db.conn();
        for (pid, status, data, opened, closed) in rows {
            let _ = db.execute(
                "INSERT OR REPLACE INTO positions (position_id, status, data, opened_at, closed_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![pid, status, data, opened.as_deref(), closed.as_deref()],
            );
        }
        *self.dirty_count.lock() += rows.len();
    }

    pub fn delete_position(&self, position_id: &str) {
        let db = self.db.conn();
        let _ = db.execute("DELETE FROM positions WHERE position_id = ?1", params![position_id]);
        *self.dirty_count.lock() += 1;
    }

    pub fn load_by_status(&self, status: &str) -> Vec<String> {
        let db = self.db.conn();
        let mut stmt = match db.prepare("SELECT data FROM positions WHERE status = ?1") {
            Ok(s) => s,
            Err(e) => { tracing::warn!("load_by_status prepare failed: {}", e); return Vec::new(); }
        };
        let rows = stmt.query_map(params![status], |row| row.get::<_, String>(0));
        match rows {
            Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
            Err(e) => { tracing::warn!("load_by_status query failed: {}", e); Vec::new() }
        }
    }

    pub fn load_open(&self) -> Vec<String> {
        let db = self.db.conn();
        let mut stmt = match db.prepare(
            "SELECT data FROM positions WHERE status IN ('open', 'monitoring')"
        ) {
            Ok(s) => s,
            Err(e) => { tracing::warn!("load_open prepare failed: {}", e); return Vec::new(); }
        };
        let rows = stmt.query_map([], |row| row.get::<_, String>(0));
        match rows {
            Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
            Err(e) => { tracing::warn!("load_open query failed: {}", e); Vec::new() }
        }
    }

    pub fn load_closed(&self) -> Vec<String> {
        self.load_by_status("closed")
    }

    pub fn count_by_status(&self) -> Vec<(String, i64)> {
        let db = self.db.conn();
        let mut stmt = match db.prepare(
            "SELECT status, COUNT(*) FROM positions GROUP BY status"
        ) {
            Ok(s) => s,
            Err(e) => { tracing::warn!("count_by_status prepare failed: {}", e); return Vec::new(); }
        };
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        });
        match rows {
            Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
            Err(e) => { tracing::warn!("count_by_status query failed: {}", e); Vec::new() }
        }
    }

    pub fn get_open_position_ids(&self) -> Vec<String> {
        let db = self.db.conn();
        let mut stmt = match db.prepare(
            "SELECT position_id FROM positions WHERE status IN ('open', 'monitoring')"
        ) {
            Ok(s) => s,
            Err(e) => { tracing::warn!("get_open_position_ids prepare failed: {}", e); return Vec::new(); }
        };
        let rows = stmt.query_map([], |row| row.get::<_, String>(0));
        match rows {
            Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
            Err(e) => { tracing::warn!("get_open_position_ids query failed: {}", e); Vec::new() }
        }
    }

    // --- Delay P95 table ---

    /// Replace the entire delay_p95 table with new data.
    /// rows: [(category, p95_hours, count, median, p75, pct_over_24h)]
    pub fn set_delay_table(&self, rows: &[(String, f64, i64, f64, f64, f64)], updated_at: &str) {
        let db = self.db.conn();
        let _ = db.execute("DELETE FROM delay_p95", []);
        for (cat, p95, count, median, p75, pct) in rows {
            let _ = db.execute(
                "INSERT INTO delay_p95 (category, p95_hours, count, median_hours, p75_hours, pct_over_24h, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![cat, p95, count, median, p75, pct, updated_at],
            );
        }
        *self.dirty_count.lock() += 1;
    }

    /// Get all delay P95 values as (category, p95_hours).
    pub fn get_delay_table(&self) -> Vec<(String, f64)> {
        let db = self.db.conn();
        let mut stmt = match db.prepare("SELECT category, p95_hours FROM delay_p95") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        });
        match rows {
            Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Get full delay table as JSON-ready rows.
    pub fn get_delay_table_full(&self) -> Vec<(String, f64, i64, f64, f64, f64, String)> {
        let db = self.db.conn();
        let mut stmt = match db.prepare(
            "SELECT category, p95_hours, count, median_hours, p75_hours, pct_over_24h, \
             COALESCE(updated_at, '') FROM delay_p95"
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, f64>(3)?,
                row.get::<_, f64>(4)?,
                row.get::<_, f64>(5)?,
                row.get::<_, String>(6)?,
            ))
        });
        match rows {
            Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
            Err(e) => { tracing::warn!("get_delay_table_full query failed: {}", e); Vec::new() }
        }
    }

    // --- Daily reports (C4) ---

    /// Save a daily report. Upserts by report_date (YYYY-MM-DD).
    pub fn save_daily_report(
        &self,
        report_date: &str,
        timestamp: f64,
        entries: u32,
        exits: u32,
        fees: f64,
        net_pnl: f64,
        capital_util_pct: f64,
        drawdown_pct: f64,
        data_json: Option<&str>,
    ) {
        let db = self.db.conn();
        let _ = db.execute(
            "INSERT OR REPLACE INTO daily_reports \
             (report_date, timestamp, entries, exits, fees, net_pnl, capital_util_pct, drawdown_pct, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![report_date, timestamp, entries, exits, fees, net_pnl, capital_util_pct, drawdown_pct, data_json],
        );
        *self.dirty_count.lock() += 1;
    }

    // --- Strategy virtual portfolios ---

    /// Save a strategy portfolio summary row (upsert).
    pub fn save_strategy_portfolio(&self, name: &str, capital: f64, entered: u64, wins: u64, losses: u64,
                                     evals_seen: u64, evals_rejected: u64) {
        let db = self.db.conn();
        let _ = db.execute(
            "INSERT OR REPLACE INTO strategy_portfolios \
             (name, current_capital, total_entered, total_wins, total_losses, evals_seen, evals_rejected) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![name, capital, entered as i64, wins as i64, losses as i64,
                    evals_seen as i64, evals_rejected as i64],
        );
    }

    /// Load all strategy portfolio summaries.
    pub fn load_strategy_portfolios(&self) -> Vec<(String, f64, i64, i64, i64, i64, i64)> {
        let db = self.db.conn();
        // Try with new columns first, fall back to old schema
        let mut stmt = match db.prepare(
            "SELECT name, current_capital, total_entered, total_wins, total_losses, \
             evals_seen, evals_rejected FROM strategy_portfolios"
        ) {
            Ok(s) => s,
            Err(_) => {
                // Old schema without evals columns — add them
                let _ = db.execute("ALTER TABLE strategy_portfolios ADD COLUMN evals_seen INTEGER NOT NULL DEFAULT 0", []);
                let _ = db.execute("ALTER TABLE strategy_portfolios ADD COLUMN evals_rejected INTEGER NOT NULL DEFAULT 0", []);
                match db.prepare(
                    "SELECT name, current_capital, total_entered, total_wins, total_losses, \
                     evals_seen, evals_rejected FROM strategy_portfolios"
                ) {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                }
            }
        };
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?,
                row.get::<_, i64>(2)?, row.get::<_, i64>(3)?, row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?, row.get::<_, i64>(6)?))
        });
        match rows {
            Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Replace all open positions for a strategy.
    pub fn save_strategy_open_positions(&self, strategy_name: &str, positions: &[(String, String)]) {
        let db = self.db.conn();
        let _ = db.execute(
            "DELETE FROM strategy_open_positions WHERE strategy_name = ?1",
            params![strategy_name],
        );
        for (cid, data_json) in positions {
            let _ = db.execute(
                "INSERT INTO strategy_open_positions (strategy_name, constraint_id, data) VALUES (?1, ?2, ?3)",
                params![strategy_name, cid, data_json],
            );
        }
    }

    /// Load all open positions for a strategy.
    pub fn load_strategy_open_positions(&self, strategy_name: &str) -> Vec<(String, String)> {
        let db = self.db.conn();
        let mut stmt = match db.prepare(
            "SELECT constraint_id, data FROM strategy_open_positions WHERE strategy_name = ?1"
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![strategy_name], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        });
        match rows {
            Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Insert a closed virtual position.
    pub fn save_strategy_closed_position(
        &self, strategy_name: &str, capital: f64, profit: f64, profit_pct: f64,
        entry_ts: f64, close_ts: f64, is_win: bool,
        short_name: &str, method: &str, is_sell: bool,
        close_reason: &str,
    ) {
        let db = self.db.conn();
        // ALTER TABLE migration: add close_reason column if missing
        let _ = db.execute_batch(
            "ALTER TABLE strategy_closed_positions ADD COLUMN close_reason TEXT NOT NULL DEFAULT 'resolved';"
        );
        let _ = db.execute(
            "INSERT INTO strategy_closed_positions \
             (strategy_name, capital_deployed, actual_profit, actual_profit_pct, entry_ts, close_ts, is_win, short_name, method, is_sell, close_reason) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![strategy_name, capital, profit, profit_pct, entry_ts, close_ts, is_win as i32,
                    short_name, method, is_sell as i32, close_reason],
        );
    }

    /// Load closed virtual positions for a strategy within a time window.
    pub fn load_strategy_closed_positions(&self, strategy_name: &str, since_ts: f64)
        -> Vec<(f64, f64, f64, f64, f64, bool, String, String, bool, String)>
    {
        let db = self.db.conn();
        // Ensure close_reason column exists
        let _ = db.execute_batch(
            "ALTER TABLE strategy_closed_positions ADD COLUMN close_reason TEXT NOT NULL DEFAULT 'resolved';"
        );
        let mut stmt = match db.prepare(
            "SELECT capital_deployed, actual_profit, actual_profit_pct, entry_ts, close_ts, is_win, \
             COALESCE(short_name, ''), COALESCE(method, ''), COALESCE(is_sell, 0), COALESCE(close_reason, 'resolved') \
             FROM strategy_closed_positions WHERE strategy_name = ?1 AND close_ts >= ?2 ORDER BY close_ts"
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![strategy_name, since_ts], |row| {
            Ok((row.get::<_, f64>(0)?, row.get::<_, f64>(1)?, row.get::<_, f64>(2)?,
                row.get::<_, f64>(3)?, row.get::<_, f64>(4)?, row.get::<_, i32>(5)? != 0,
                row.get::<_, String>(6)?, row.get::<_, String>(7)?, row.get::<_, i32>(8)? != 0,
                row.get::<_, String>(9)?))
        });
        match rows {
            Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Prune closed virtual positions older than cutoff_ts.
    pub fn prune_strategy_closed_positions(&self, cutoff_ts: f64) {
        let db = self.db.conn();
        let _ = db.execute(
            "DELETE FROM strategy_closed_positions WHERE close_ts < ?1",
            params![cutoff_ts],
        );
    }

    // --- Disk persistence ---

    /// Atomic backup of in-memory DB to disk. Returns elapsed ms.
    pub fn mirror_to_disk(&self) -> f64 {
        let ms = self.db.mirror_to_disk();
        *self.dirty_count.lock() = 0;
        ms
    }

    /// Load disk DB into memory (startup recovery). Returns elapsed ms.
    pub fn load_from_disk(&self) -> Result<f64, String> {
        let ms = self.db.load_from_disk()?;

        // Ensure new tables exist after restoring from older disk DB
        let db = self.db.conn();
        let _ = db.execute_batch("
            CREATE TABLE IF NOT EXISTS delay_p95 (
                category TEXT PRIMARY KEY,
                p95_hours REAL NOT NULL,
                count INTEGER NOT NULL DEFAULT 0,
                median_hours REAL NOT NULL DEFAULT 0,
                p75_hours REAL NOT NULL DEFAULT 0,
                pct_over_24h REAL NOT NULL DEFAULT 0,
                updated_at TEXT
            );
            CREATE TABLE IF NOT EXISTS daily_reports (
                report_date TEXT PRIMARY KEY,
                timestamp REAL NOT NULL,
                entries INTEGER NOT NULL DEFAULT 0,
                exits INTEGER NOT NULL DEFAULT 0,
                fees REAL NOT NULL DEFAULT 0,
                net_pnl REAL NOT NULL DEFAULT 0,
                capital_util_pct REAL NOT NULL DEFAULT 0,
                drawdown_pct REAL NOT NULL DEFAULT 0,
                data TEXT
            );
            CREATE TABLE IF NOT EXISTS strategy_portfolios (
                name TEXT PRIMARY KEY,
                current_capital REAL NOT NULL,
                total_entered INTEGER NOT NULL DEFAULT 0,
                total_wins INTEGER NOT NULL DEFAULT 0,
                total_losses INTEGER NOT NULL DEFAULT 0,
                evals_seen INTEGER NOT NULL DEFAULT 0,
                evals_rejected INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS strategy_open_positions (
                strategy_name TEXT NOT NULL,
                constraint_id TEXT NOT NULL,
                data TEXT NOT NULL,
                PRIMARY KEY (strategy_name, constraint_id)
            );
            CREATE TABLE IF NOT EXISTS strategy_closed_positions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                strategy_name TEXT NOT NULL,
                capital_deployed REAL NOT NULL,
                actual_profit REAL NOT NULL,
                actual_profit_pct REAL NOT NULL,
                entry_ts REAL NOT NULL,
                close_ts REAL NOT NULL,
                is_win INTEGER NOT NULL,
                short_name TEXT NOT NULL DEFAULT '',
                method TEXT NOT NULL DEFAULT '',
                is_sell INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_strat_closed_ts ON strategy_closed_positions(close_ts);
            CREATE TABLE IF NOT EXISTS instruments (
                token_id TEXT PRIMARY KEY,
                market_id TEXT NOT NULL,
                outcome TEXT NOT NULL,
                condition_id TEXT NOT NULL,
                neg_risk INTEGER NOT NULL DEFAULT 0,
                tick_size REAL NOT NULL DEFAULT 0.01,
                min_order_size REAL NOT NULL DEFAULT 1.0,
                max_order_size REAL NOT NULL DEFAULT 0.0,
                order_book_enabled INTEGER NOT NULL DEFAULT 1,
                accepting_orders INTEGER NOT NULL DEFAULT 1
            );
            CREATE INDEX IF NOT EXISTS idx_instr_market ON instruments(market_id);
            CREATE TABLE IF NOT EXISTS checkpoints (
                key TEXT PRIMARY KEY,
                data TEXT NOT NULL,
                updated_at TEXT
            );
        ");

        Ok(ms)
    }

    /// Get dirty count (positions changed since last mirror).
    pub fn dirty_count(&self) -> usize {
        *self.dirty_count.lock()
    }

    // --- Instrument persistence ---

    /// Bulk upsert instruments into SQLite.
    pub fn save_instruments_bulk(&self, rows: &[(String, String, String, String, bool, f64, f64, f64, bool, bool)]) {
        let db = self.db.conn();
        for (token_id, market_id, outcome, condition_id, neg_risk, tick_size,
             min_order_size, max_order_size, order_book_enabled, accepting_orders) in rows
        {
            let _ = db.execute(
                "INSERT OR REPLACE INTO instruments \
                 (token_id, market_id, outcome, condition_id, neg_risk, tick_size, \
                  min_order_size, max_order_size, order_book_enabled, accepting_orders) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![token_id, market_id, outcome, condition_id,
                        *neg_risk as i32, tick_size, min_order_size, max_order_size,
                        *order_book_enabled as i32, *accepting_orders as i32],
            );
        }
    }

    /// Load all instruments from SQLite.
    /// Returns (token_id, market_id, outcome, condition_id, neg_risk, tick_size,
    ///          min_order_size, max_order_size, order_book_enabled, accepting_orders).
    pub fn load_instruments(&self) -> Vec<(String, String, String, String, bool, f64, f64, f64, bool, bool)> {
        let db = self.db.conn();
        let mut stmt = match db.prepare(
            "SELECT token_id, market_id, outcome, condition_id, neg_risk, tick_size, \
             min_order_size, max_order_size, order_book_enabled, accepting_orders FROM instruments"
        ) {
            Ok(s) => s,
            Err(e) => { tracing::warn!("load_instruments prepare failed: {}", e); return Vec::new(); }
        };
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i32>(4)? != 0,
                row.get::<_, f64>(5)?,
                row.get::<_, f64>(6)?,
                row.get::<_, f64>(7)?,
                row.get::<_, i32>(8)? != 0,
                row.get::<_, i32>(9)? != 0,
            ))
        });
        match rows {
            Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
            Err(e) => { tracing::warn!("load_instruments query failed: {}", e); Vec::new() }
        }
    }

    // --- Checkpoint persistence ---

    /// Save a JSON checkpoint blob by key.
    pub fn save_checkpoint(&self, key: &str, data_json: &str) {
        let db = self.db.conn();
        let now = chrono::Utc::now().to_rfc3339();
        let _ = db.execute(
            "INSERT OR REPLACE INTO checkpoints (key, data, updated_at) VALUES (?1, ?2, ?3)",
            params![key, data_json, now],
        );
    }

    /// Load a JSON checkpoint blob by key.
    pub fn load_checkpoint(&self, key: &str) -> Option<String> {
        let db = self.db.conn();
        db.query_row(
            "SELECT data FROM checkpoints WHERE key = ?1",
            params![key],
            |row| row.get(0),
        ).ok()
    }

    // --- Opportunity logging ---

    /// Log an evaluated opportunity for post-run analysis.
    pub fn log_opportunity(
        &self, ts: f64, constraint_id: &str, method: &str, num_legs: usize,
        expected_profit: f64, expected_profit_pct: f64, total_capital_required: f64,
        hours_to_resolve: f64, score: f64, entered: bool,
        rejected_reason: Option<&str>, strategy_accepted: Option<&str>,
    ) {
        let db = self.db.conn();
        let _ = db.execute(
            "INSERT INTO evaluated_opportunities \
             (ts, constraint_id, method, num_legs, expected_profit, expected_profit_pct, \
              total_capital_required, hours_to_resolve, score, entered, rejected_reason, strategy_accepted) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![ts, constraint_id, method, num_legs as i64,
                    expected_profit, expected_profit_pct, total_capital_required,
                    hours_to_resolve, score, entered as i32,
                    rejected_reason, strategy_accepted],
        );
    }

    /// INC-019: Update the most-recently-logged row for `constraint_id` whose
    /// `rejected_reason` is still NULL, scoped to the current eval cycle
    /// (rows with `ts > now - max_age_secs`). Used by live-only silent-skip
    /// paths in the orchestrator (gamma_freshness, negRisk_cap_full,
    /// InsufficientCapital) to attach a reject reason after the initial INSERT.
    /// No-op if no matching row exists. Safe under concurrent INSERTs because
    /// constraint_id appears at most once per eval batch from the ranker.
    pub fn update_opportunity_reject_reason(
        &self, constraint_id: &str, reason: &str, max_age_secs: f64,
    ) {
        let db = self.db.conn();
        let cutoff_ts = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)) - max_age_secs;
        let _ = db.execute(
            "UPDATE evaluated_opportunities \
             SET rejected_reason = ?1 \
             WHERE id = ( \
               SELECT id FROM evaluated_opportunities \
               WHERE constraint_id = ?2 AND rejected_reason IS NULL AND ts > ?3 \
               ORDER BY id DESC LIMIT 1 \
             )",
            params![reason, constraint_id, cutoff_ts],
        );
    }

    // --- Journal persistence ---

    /// Bulk insert journal entries into SQLite.
    pub fn save_journal_entries(&self, entries: &[crate::accounting::JournalEntry]) {
        let db = self.db.conn();
        for e in entries {
            let _ = db.execute(
                "INSERT OR REPLACE INTO journal (id, timestamp, account, debit, credit, position_id, description) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![e.id as i64, e.timestamp, e.account, e.debit, e.credit,
                        e.position_id, e.description],
            );
        }
    }
}
