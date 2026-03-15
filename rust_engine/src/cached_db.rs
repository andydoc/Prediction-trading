/// Generic in-memory SQLite database with automatic disk backup/restore.
///
/// Extracts the common pattern used by StateDB, ResolutionCache,
/// PostponementCache, and ScannerDB: an in-memory connection with
/// PRAGMA synchronous=OFF, schema DDL init, and Backup-based
/// mirror_to_disk / load_from_disk methods.
use std::path::PathBuf;
use std::time::{Duration, Instant};
use parking_lot::Mutex;
use rusqlite::{Connection, backup::Backup};

pub struct CachedSqliteDB {
    mem: Mutex<Connection>,
    disk_path: PathBuf,
}

impl CachedSqliteDB {
    /// Create a new in-memory DB and run the schema DDL.
    ///
    /// Does **not** auto-load from disk — callers decide whether and when
    /// to call `load_from_disk()`.
    pub fn new(disk_path: &str, schema_sql: &str) -> Result<Self, String> {
        let mem_conn = Connection::open_in_memory()
            .map_err(|e| format!("Failed to open in-memory SQLite: {}", e))?;
        mem_conn.execute_batch(&format!("PRAGMA synchronous=OFF;\n{}", schema_sql))
            .map_err(|e| format!("Schema init failed: {}", e))?;

        Ok(Self {
            mem: Mutex::new(mem_conn),
            disk_path: PathBuf::from(disk_path),
        })
    }

    /// Get a lock on the in-memory connection.
    pub fn conn(&self) -> parking_lot::MutexGuard<'_, Connection> {
        self.mem.lock()
    }

    /// Whether the disk file exists.
    pub fn disk_exists(&self) -> bool {
        self.disk_path.exists()
    }

    /// Backup in-memory DB to disk. Returns elapsed milliseconds.
    pub fn mirror_to_disk(&self) -> f64 {
        let t0 = Instant::now();
        let db = self.mem.lock();

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
            Err(e) => tracing::warn!("Failed to open disk DB for backup: {}", e),
        }

        t0.elapsed().as_secs_f64() * 1000.0
    }

    /// Restore in-memory DB from disk. Returns elapsed milliseconds.
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
        backup.run_to_completion(100, Duration::ZERO, None::<fn(rusqlite::backup::Progress)>)
            .map_err(|e| format!("Backup restore failed: {}", e))?;
        Ok(t0.elapsed().as_secs_f64() * 1000.0)
    }
}
