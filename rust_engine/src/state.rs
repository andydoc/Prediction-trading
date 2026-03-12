/// Rust SQLite state persistence — replaces Python state_db.py
///
/// Same schema as Python version. Key win: `backup_to_disk()` runs without
/// holding the GIL, eliminating ~30-40ms of GIL contention every 30s.
///
/// Schema:
///   state(key TEXT PK, value TEXT)        — scalars: capital, metrics
///   positions(position_id TEXT PK, status TEXT, data JSON, opened_at, closed_at)
use rusqlite::{Connection, params};
use rusqlite::backup::Backup;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct StateDB {
    /// In-memory database (fast reads/writes during operation)
    mem: Mutex<Connection>,
    /// Path to disk mirror file
    disk_path: PathBuf,
    /// Count of dirty positions since last mirror
    dirty_count: Mutex<usize>,
}

impl StateDB {
    pub fn new(disk_path: &str) -> Result<Self, String> {
        let mem_conn = Connection::open_in_memory()
            .map_err(|e| format!("Failed to open in-memory SQLite: {}", e))?;

        mem_conn.execute_batch("
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=OFF;
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
        ").map_err(|e| format!("Failed to create tables: {}", e))?;

        Ok(Self {
            mem: Mutex::new(mem_conn),
            disk_path: PathBuf::from(disk_path),
            dirty_count: Mutex::new(0),
        })
    }

    // --- Scalar state (capital, counters) ---

    pub fn set_scalar(&self, key: &str, value: f64) {
        let db = self.mem.lock();
        let _ = db.execute(
            "INSERT OR REPLACE INTO scalars (key, value) VALUES (?1, ?2)",
            params![key, value],
        );
    }

    pub fn get_scalar(&self, key: &str) -> Option<f64> {
        let db = self.mem.lock();
        db.query_row(
            "SELECT value FROM scalars WHERE key = ?1",
            params![key],
            |row| row.get(0),
        ).ok()
    }

    pub fn set_scalars(&self, pairs: &[(String, f64)]) {
        let db = self.mem.lock();
        for (k, v) in pairs {
            let _ = db.execute(
                "INSERT OR REPLACE INTO scalars (key, value) VALUES (?1, ?2)",
                params![k, v],
            );
        }
    }

    pub fn get_all_scalars(&self) -> Vec<(String, f64)> {
        let db = self.mem.lock();
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
        let db = self.mem.lock();
        let _ = db.execute(
            "INSERT OR REPLACE INTO positions (position_id, status, data, opened_at, closed_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![position_id, status, data_json, opened_at, closed_at],
        );
        *self.dirty_count.lock() += 1;
    }

    pub fn save_positions_bulk(&self, rows: &[(String, String, String, Option<String>, Option<String>)]) {
        let db = self.mem.lock();
        for (pid, status, data, opened, closed) in rows {
            let _ = db.execute(
                "INSERT OR REPLACE INTO positions (position_id, status, data, opened_at, closed_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![pid, status, data, opened.as_deref(), closed.as_deref()],
            );
        }
        *self.dirty_count.lock() += rows.len();
    }

    pub fn delete_position(&self, position_id: &str) {
        let db = self.mem.lock();
        let _ = db.execute("DELETE FROM positions WHERE position_id = ?1", params![position_id]);
        *self.dirty_count.lock() += 1;
    }

    pub fn load_by_status(&self, status: &str) -> Vec<String> {
        let db = self.mem.lock();
        let mut stmt = db.prepare(
            "SELECT data FROM positions WHERE status = ?1"
        ).unwrap();
        stmt.query_map(params![status], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }

    pub fn load_open(&self) -> Vec<String> {
        let db = self.mem.lock();
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
        let db = self.mem.lock();
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
        let db = self.mem.lock();
        let mut stmt = db.prepare(
            "SELECT position_id FROM positions WHERE status IN ('open', 'monitoring')"
        ).unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }

    // --- Disk persistence (runs WITHOUT the GIL — main latency win) ---

    /// Atomic backup of in-memory DB to disk. Returns elapsed ms.
    pub fn mirror_to_disk(&self) -> f64 {
        let t0 = Instant::now();
        let db = self.mem.lock();

        // Ensure parent directory exists
        if let Some(parent) = self.disk_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        match Connection::open(&self.disk_path) {
            Ok(mut disk_db) => {
                match Backup::new(&*db, &mut disk_db) {
                    Ok(backup) => {
                        if let Err(e) = backup.run_to_completion(
                            100, Duration::ZERO,
                            None::<fn(rusqlite::backup::Progress)>,
                        ) {
                            tracing::warn!("Backup step failed: {}", e);
                        }
                    }
                    Err(e) => tracing::warn!("Backup init failed: {}", e),
                }
            }
            Err(e) => {
                tracing::warn!("Failed to open disk DB for backup: {}", e);
            }
        }

        *self.dirty_count.lock() = 0;
        t0.elapsed().as_secs_f64() * 1000.0
    }

    /// Load disk DB into memory (startup recovery). Returns elapsed ms.
    pub fn load_from_disk(&self) -> Result<f64, String> {
        if !self.disk_path.exists() {
            return Err("Disk DB file not found".into());
        }
        let t0 = Instant::now();
        let disk_db = Connection::open(&self.disk_path)
            .map_err(|e| format!("Failed to open disk DB: {}", e))?;

        let mut db = self.mem.lock();
        let backup = Backup::new(&disk_db, &mut *db)
            .map_err(|e| format!("Backup init failed: {}", e))?;
        backup.run_to_completion(
            100, Duration::ZERO,
            None::<fn(rusqlite::backup::Progress)>,
        ).map_err(|e| format!("Backup restore failed: {}", e))?;

        Ok(t0.elapsed().as_secs_f64() * 1000.0)
    }

    /// Get dirty count (positions changed since last mirror).
    pub fn dirty_count(&self) -> usize {
        *self.dirty_count.lock()
    }
}
