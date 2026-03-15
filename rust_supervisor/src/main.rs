//! Supervisor binary for the Prediction Market Trading System.
//!
//! Replaces Python `main.py`. Responsibilities:
//!   - PID file lock (prevents double-start)
//!   - Signal handling (SIGTERM, SIGINT → graceful shutdown)
//!   - Spawns trading_engine.py as supervised subprocess
//!   - Monitors subprocess health, restarts on crash
//!   - Log file cleanup on startup
//!   - Systemd compatible (runs in foreground, exits cleanly on signal)

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tracing_subscriber::prelude::*;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

struct SupervisorConfig {
    workspace: PathBuf,
    venv_python: PathBuf,
    log_dir: PathBuf,
    pid_file: PathBuf,
    log_level: String,
    log_retention_days: u32,
    restart_delay_secs: u64,
    health_check_interval_secs: u64,
}

impl SupervisorConfig {
    fn load() -> Self {
        // Workspace from env or default
        let workspace = std::env::var("TRADER_WORKSPACE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/home/andydoc/prediction-trader"));

        let venv_python = std::env::var("TRADER_PYTHON")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/home/andydoc/prediction-trader-env/bin/python"));

        // Read config.yaml for logging settings
        let config_path = workspace.join("config").join("config.yaml");
        let (log_level, log_retention_days) = match fs::read_to_string(&config_path) {
            Ok(contents) => {
                let val: serde_json::Value = serde_yaml::from_str(&contents).unwrap_or_default();
                let level = val.pointer("/monitoring/logging/level")
                    .and_then(|v| v.as_str())
                    .unwrap_or("DEBUG")
                    .to_lowercase();
                let retention = val.pointer("/monitoring/logging/retention")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30) as u32;
                (level, retention)
            }
            Err(_) => ("debug".into(), 30),
        };

        let log_dir = workspace.join("logs");
        let pid_file = workspace.join("supervisor.pid");

        Self {
            workspace,
            venv_python,
            log_dir,
            pid_file,
            log_level,
            log_retention_days,
            restart_delay_secs: 10,
            health_check_interval_secs: 15,
        }
    }
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

fn init_logging(cfg: &SupervisorConfig) {
    let _ = fs::create_dir_all(&cfg.log_dir);

    let file_appender = tracing_appender::rolling::daily(&cfg.log_dir, "supervisor");

    let filter_str = format!("prediction_trader={}", cfg.log_level);
    let env_filter = tracing_subscriber::EnvFilter::try_new(&filter_str)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("prediction_trader=debug"));

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_appender)
        .with_target(false)
        .with_ansi(false);

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false);

    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .with(stderr_layer)
        .try_init();
}

fn cleanup_old_logs(log_dir: &Path, retention_days: u32) {
    if retention_days == 0 {
        return;
    }
    let cutoff = SystemTime::now() - Duration::from_secs(retention_days as u64 * 86400);
    let mut removed = 0u32;

    if let Ok(entries) = fs::read_dir(log_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.ends_with(".log") && !name_str.starts_with("supervisor") && !name_str.starts_with("rust_engine") {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if let Ok(modified) = meta.modified() {
                    if modified < cutoff {
                        if fs::remove_file(entry.path()).is_ok() {
                            removed += 1;
                        }
                    }
                }
            }
        }
    }
    if removed > 0 {
        tracing::info!("Cleaned {} old log files (>{} days)", removed, retention_days);
    }
}

// ---------------------------------------------------------------------------
// PID lock
// ---------------------------------------------------------------------------

fn check_pid_lock(pid_file: &Path) {
    if pid_file.exists() {
        if let Ok(contents) = fs::read_to_string(pid_file) {
            if let Ok(old_pid) = contents.trim().parse::<i32>() {
                // Check if process is still running
                use nix::sys::signal;
                use nix::unistd::Pid;
                match signal::kill(Pid::from_raw(old_pid), None) {
                    Ok(_) => {
                        tracing::error!("Supervisor already running (PID {}). Exiting.", old_pid);
                        std::process::exit(1);
                    }
                    Err(_) => {
                        tracing::warn!("Stale PID file (PID {} not running). Removing.", old_pid);
                        let _ = fs::remove_file(pid_file);
                    }
                }
            }
        }
    }
    // Write our PID
    if let Ok(mut f) = fs::File::create(pid_file) {
        let _ = write!(f, "{}", std::process::id());
    }
}

fn remove_pid_file(pid_file: &Path) {
    let _ = fs::remove_file(pid_file);
}

// ---------------------------------------------------------------------------
// Process management
// ---------------------------------------------------------------------------

fn start_trading_engine(cfg: &SupervisorConfig) -> Option<Child> {
    let script = cfg.workspace.join("trading").join("trading_engine.py");
    match Command::new(&cfg.venv_python)
        .arg(&script)
        .current_dir(&cfg.workspace)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
    {
        Ok(child) => {
            tracing::info!("Started trading_engine pid={}", child.id());
            Some(child)
        }
        Err(e) => {
            tracing::error!("Failed to start trading_engine: {}", e);
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let cfg = SupervisorConfig::load();
    init_logging(&cfg);
    cleanup_old_logs(&cfg.log_dir, cfg.log_retention_days);
    check_pid_lock(&cfg.pid_file);

    // Signal handling
    let running = Arc::new(AtomicBool::new(true));
    let r = Arc::clone(&running);
    ctrlc_handler(&r);

    tracing::info!("{}", "=".repeat(60));
    tracing::info!("PREDICTION MARKET TRADING SYSTEM - SUPERVISOR (Rust)");
    tracing::info!("PID: {}", std::process::id());
    tracing::info!("Workspace: {}", cfg.workspace.display());
    tracing::info!("{}", "=".repeat(60));

    // Start trading engine
    let mut engine_proc = start_trading_engine(&cfg);

    // Monitor loop
    while running.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(cfg.health_check_interval_secs));

        if !running.load(Ordering::Relaxed) {
            break;
        }

        match engine_proc.as_mut() {
            Some(proc) => {
                match proc.try_wait() {
                    Ok(Some(status)) => {
                        tracing::warn!(
                            "trading_engine exited (code={}), restarting in {}s",
                            status.code().map_or("signal".to_string(), |c| c.to_string()),
                            cfg.restart_delay_secs
                        );
                        std::thread::sleep(Duration::from_secs(cfg.restart_delay_secs));
                        if running.load(Ordering::Relaxed) {
                            engine_proc = start_trading_engine(&cfg);
                        }
                    }
                    Ok(None) => {
                        tracing::debug!("trading_engine healthy pid={}", proc.id());
                    }
                    Err(e) => {
                        tracing::error!("Failed to check trading_engine status: {}", e);
                    }
                }
            }
            None => {
                tracing::warn!("trading_engine not running, restarting in {}s", cfg.restart_delay_secs);
                std::thread::sleep(Duration::from_secs(cfg.restart_delay_secs));
                if running.load(Ordering::Relaxed) {
                    engine_proc = start_trading_engine(&cfg);
                }
            }
        }
    }

    // Graceful shutdown
    tracing::info!("Shutting down...");
    if let Some(mut proc) = engine_proc {
        // Send SIGTERM to child
        use nix::sys::signal;
        use nix::unistd::Pid;
        let pid = proc.id() as i32;
        let _ = signal::kill(Pid::from_raw(pid), signal::Signal::SIGTERM);
        // Wait up to 10 seconds for graceful exit
        for _ in 0..20 {
            if proc.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        // Force kill if still running
        let _ = proc.kill();
        let _ = proc.wait();
    }

    remove_pid_file(&cfg.pid_file);
    tracing::info!("Supervisor stopped.");
}

/// Register SIGTERM and SIGINT handlers using nix.
fn ctrlc_handler(running: &Arc<AtomicBool>) {
    use nix::sys::signal;

    // Set up signal mask for SIGTERM and SIGINT
    let r1 = Arc::clone(running);
    let r2 = Arc::clone(running);

    // Use a simple thread-based approach: block signals on a dedicated thread
    let mask = signal::SigSet::from_iter([signal::Signal::SIGTERM, signal::Signal::SIGINT]);
    // Block these signals on the main thread so sigwait catches them
    let _ = mask.thread_block();

    std::thread::spawn(move || {
        loop {
            match mask.wait() {
                Ok(sig) => {
                    // First signal: set running=false for graceful shutdown
                    if r1.load(Ordering::Relaxed) {
                        tracing::info!("Signal {:?} received — shutting down gracefully", sig);
                        r1.store(false, Ordering::Relaxed);
                    } else {
                        // Second signal: force exit
                        tracing::warn!("Second signal received — forcing exit");
                        std::process::exit(1);
                    }
                }
                Err(_) => break,
            }
        }
    });
    // r2 not needed here but kept to satisfy borrow
    let _ = r2;
}
