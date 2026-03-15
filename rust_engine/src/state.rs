/// SQLite state persistence — in-memory with periodic disk backup.
///
/// Same schema as Python version. Key win: `backup_to_disk()` runs without
/// holding the GIL, eliminating ~30-40ms of GIL contention every 30s.
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
        let mut stmt = db.prepare("SELECT key, value FROM scalars").unwrap();
        stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        }).unwrap()
        .filter_map(|r| r.ok())
        .collect()
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
        let mut stmt = db.prepare(
            "SELECT data FROM positions WHERE status = ?1"
        ).unwrap();
        stmt.query_map(params![status], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }

    pub fn load_open(&self) -> Vec<String> {
        let db = self.db.conn();
        let mut stmt = db.prepare(
            "SELECT data FROM positions WHERE status IN ('open', 'monitoring')"
        ).unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }

    pub fn load_closed(&self) -> Vec<String> {
        self.load_by_status("closed")
    }

    pub fn count_by_status(&self) -> Vec<(String, i64)> {
        let db = self.db.conn();
        let mut stmt = db.prepare(
            "SELECT status, COUNT(*) FROM positions GROUP BY status"
        ).unwrap();
        stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        }).unwrap()
        .filter_map(|r| r.ok())
        .collect()
    }

    pub fn get_open_position_ids(&self) -> Vec<String> {
        let db = self.db.conn();
        let mut stmt = db.prepare(
            "SELECT position_id FROM positions WHERE status IN ('open', 'monitoring')"
        ).unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
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
        stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        }).unwrap()
        .filter_map(|r| r.ok())
        .collect()
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
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, f64>(3)?,
                row.get::<_, f64>(4)?,
                row.get::<_, f64>(5)?,
                row.get::<_, String>(6)?,
            ))
        }).unwrap()
        .filter_map(|r| r.ok())
        .collect()
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
        ");

        Ok(ms)
    }

    /// Get dirty count (positions changed since last mirror).
    pub fn dirty_count(&self) -> usize {
        *self.dirty_count.lock()
    }
}
