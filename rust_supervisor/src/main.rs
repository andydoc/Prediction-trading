//! Supervisor binary for the Prediction Market Trading System.
//!
//! Replaces Python `main.py`. Responsibilities:
//!   - PID file lock (prevents double-start)
//!   - Signal handling (SIGTERM, SIGINT → graceful shutdown)
//!   - Spawns trading_engine.py as supervised subprocess
//!   - Monitors subprocess health, restarts on crash
//!   - Log file cleanup on startup
//!   - Systemd compatible (runs in foreground, exits cleanly on signal)
//!
//! CLI overrides take precedence over config.yaml, which takes precedence over defaults.
//! Any config.yaml value can be overridden: `--set arbitrage.min_profit_threshold=0.05`

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use clap::Parser;
use tracing_subscriber::prelude::*;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Prediction Market Trading System — Rust supervisor
#[derive(Parser, Debug)]
#[command(name = "prediction-trader", version, about)]
struct Cli {
    /// Workspace root directory (contains config/, trading/, logs/)
    #[arg(short, long, env = "TRADER_WORKSPACE")]
    workspace: Option<String>,

    /// Path to Python interpreter (venv)
    #[arg(long, env = "TRADER_PYTHON")]
    python: Option<String>,

    /// Trading mode: dual, shadow, live, scanner-only, engine-only
    #[arg(short, long)]
    mode: Option<String>,

    /// Dashboard port (0 = disabled)
    #[arg(short, long)]
    port: Option<u16>,

    /// Log level: trace, debug, info, warn, error
    #[arg(short, long)]
    log_level: Option<String>,

    /// Seconds to wait before restarting crashed subprocess
    #[arg(long)]
    restart_delay: Option<u64>,

    /// Seconds between health checks
    #[arg(long)]
    health_interval: Option<u64>,

    /// Override any config.yaml value: --set key.path=value (repeatable)
    #[arg(short, long, value_name = "KEY=VALUE")]
    set: Vec<String>,

    /// Print resolved config and exit (dry run)
    #[arg(long)]
    dry_run: bool,
}

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
    mode: Option<String>,
    dashboard_port: Option<u16>,
    config_overrides: Vec<(String, String)>,
}

impl SupervisorConfig {
    /// Load config with priority: CLI args > env vars > config.yaml > defaults
    fn load(cli: &Cli) -> Self {
        let workspace = cli.workspace.as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/home/andydoc/prediction-trader"));

        let venv_python = cli.python.as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/home/andydoc/prediction-trader-env/bin/python"));

        // Read config.yaml for defaults
        let config_path = workspace.join("config").join("config.yaml");
        let yaml_val: serde_json::Value = fs::read_to_string(&config_path)
            .ok()
            .and_then(|s| serde_yaml::from_str(&s).ok())
            .unwrap_or_default();

        // Log level: CLI > config.yaml > "debug"
        let log_level = cli.log_level.clone().unwrap_or_else(|| {
            yaml_val.pointer("/monitoring/logging/level")
                .and_then(|v| v.as_str())
                .unwrap_or("debug")
                .to_lowercase()
        });

        let log_retention_days = yaml_val.pointer("/monitoring/logging/retention")
            .and_then(|v| v.as_u64())
            .unwrap_or(30) as u32;

        let restart_delay_secs = cli.restart_delay.unwrap_or(10);
        let health_check_interval_secs = cli.health_interval.unwrap_or(15);

        // Parse --set overrides
        let config_overrides: Vec<(String, String)> = cli.set.iter()
            .filter_map(|s| {
                let (k, v) = s.split_once('=')?;
                Some((k.trim().to_string(), v.trim().to_string()))
            })
            .collect();

        let log_dir = workspace.join("logs");
        let pid_file = workspace.join("supervisor.pid");

        Self {
            workspace,
            venv_python,
            log_dir,
            pid_file,
            log_level,
            log_retention_days,
            restart_delay_secs,
            health_check_interval_secs,
            mode: cli.mode.clone(),
            dashboard_port: cli.port,
            config_overrides,
        }
    }

    /// Apply --mode and --set overrides to config.yaml on disk.
    /// This lets the Python trading engine pick them up without its own CLI parsing.
    fn apply_overrides_to_config(&self) {
        if self.mode.is_none() && self.dashboard_port.is_none() && self.config_overrides.is_empty() {
            return;
        }

        let config_path = self.workspace.join("config").join("config.yaml");
        let contents = match fs::read_to_string(&config_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Cannot read config.yaml for overrides: {}", e);
                return;
            }
        };

        let mut val: serde_yaml::Value = match serde_yaml::from_str(&contents) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("Cannot parse config.yaml for overrides: {}", e);
                return;
            }
        };

        let mut changed = false;

        // Apply --mode
        if let Some(ref mode) = self.mode {
            set_yaml_value(&mut val, "mode", &serde_yaml::Value::String(mode.clone()));
            // Also set shadow_only based on mode
            match mode.as_str() {
                "shadow" => {
                    set_yaml_value(&mut val, "mode", &serde_yaml::Value::String("dual".into()));
                    set_yaml_path(&mut val, "live_trading.shadow_only", &serde_yaml::Value::Bool(true));
                    set_yaml_path(&mut val, "live_trading.enabled", &serde_yaml::Value::Bool(true));
                }
                "live" => {
                    set_yaml_value(&mut val, "mode", &serde_yaml::Value::String("dual".into()));
                    set_yaml_path(&mut val, "live_trading.shadow_only", &serde_yaml::Value::Bool(false));
                    set_yaml_path(&mut val, "live_trading.enabled", &serde_yaml::Value::Bool(true));
                }
                _ => {} // "dual", "scanner-only", "engine-only" set directly
            }
            changed = true;
        }

        // Apply --port
        if let Some(port) = self.dashboard_port {
            set_yaml_path(&mut val, "dashboard.port", &serde_yaml::Value::Number(serde_yaml::Number::from(port as u64)));
            changed = true;
        }

        // Apply --set key.path=value overrides
        for (key, value) in &self.config_overrides {
            let yaml_value = parse_yaml_value(value);
            set_yaml_path(&mut val, key, &yaml_value);
            tracing::info!("Config override: {} = {}", key, value);
            changed = true;
        }

        if changed {
            match serde_yaml::to_string(&val) {
                Ok(yaml_str) => {
                    if let Err(e) = fs::write(&config_path, &yaml_str) {
                        tracing::error!("Failed to write config overrides: {}", e);
                    } else {
                        tracing::info!("Config overrides applied to {}", config_path.display());
                    }
                }
                Err(e) => tracing::error!("Failed to serialize config: {}", e),
            }
        }
    }
}

/// Parse a string value into the most appropriate YAML type.
fn parse_yaml_value(s: &str) -> serde_yaml::Value {
    // Try bool
    match s {
        "true" | "True" | "TRUE" => return serde_yaml::Value::Bool(true),
        "false" | "False" | "FALSE" => return serde_yaml::Value::Bool(false),
        _ => {}
    }
    // Try integer
    if let Ok(n) = s.parse::<i64>() {
        return serde_yaml::Value::Number(serde_yaml::Number::from(n));
    }
    // Try float
    if let Ok(f) = s.parse::<f64>() {
        return serde_yaml::Value::Number(serde_yaml::Number::from(f));
    }
    // Default: string
    serde_yaml::Value::String(s.to_string())
}

/// Set a top-level YAML key.
fn set_yaml_value(root: &mut serde_yaml::Value, key: &str, val: &serde_yaml::Value) {
    if let serde_yaml::Value::Mapping(ref mut map) = root {
        map.insert(
            serde_yaml::Value::String(key.to_string()),
            val.clone(),
        );
    }
}

/// Set a dotted path in YAML (e.g., "live_trading.shadow_only").
fn set_yaml_path(root: &mut serde_yaml::Value, path: &str, val: &serde_yaml::Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = root;
    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            // Set the leaf
            if let serde_yaml::Value::Mapping(ref mut map) = current {
                map.insert(
                    serde_yaml::Value::String(part.to_string()),
                    val.clone(),
                );
            }
        } else {
            // Navigate/create intermediate mapping
            let key = serde_yaml::Value::String(part.to_string());
            if let serde_yaml::Value::Mapping(ref mut map) = current {
                if !map.contains_key(&key) {
                    map.insert(key.clone(), serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
                }
                current = map.get_mut(&key).unwrap();
            }
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
            // Clean log files and tracing-appender dated files
            if !name_str.ends_with(".log")
                && !name_str.starts_with("supervisor")
                && !name_str.starts_with("rust_engine")
                && !name_str.starts_with("trading_engine")
            {
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
// Signal handling
// ---------------------------------------------------------------------------

fn setup_signal_handler(running: &Arc<AtomicBool>) {
    use nix::sys::signal;

    let r = Arc::clone(running);
    let mask = signal::SigSet::from_iter([signal::Signal::SIGTERM, signal::Signal::SIGINT]);
    let _ = mask.thread_block();

    std::thread::spawn(move || {
        loop {
            match mask.wait() {
                Ok(sig) => {
                    if r.load(Ordering::Relaxed) {
                        tracing::info!("Signal {:?} — shutting down gracefully", sig);
                        r.store(false, Ordering::Relaxed);
                    } else {
                        tracing::warn!("Second signal — forcing exit");
                        std::process::exit(1);
                    }
                }
                Err(_) => break,
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse();
    let cfg = SupervisorConfig::load(&cli);

    init_logging(&cfg);
    cleanup_old_logs(&cfg.log_dir, cfg.log_retention_days);

    // Dry run: print config and exit
    if cli.dry_run {
        println!("workspace:      {}", cfg.workspace.display());
        println!("python:         {}", cfg.venv_python.display());
        println!("mode:           {}", cfg.mode.as_deref().unwrap_or("(from config)"));
        println!("dashboard_port: {}", cfg.dashboard_port.map_or("(from config)".into(), |p| p.to_string()));
        println!("log_level:      {}", cfg.log_level);
        println!("restart_delay:  {}s", cfg.restart_delay_secs);
        println!("health_check:   {}s", cfg.health_check_interval_secs);
        if !cfg.config_overrides.is_empty() {
            println!("overrides:");
            for (k, v) in &cfg.config_overrides {
                println!("  {} = {}", k, v);
            }
        }
        return;
    }

    check_pid_lock(&cfg.pid_file);

    // Apply CLI overrides to config.yaml before starting engine
    cfg.apply_overrides_to_config();

    let running = Arc::new(AtomicBool::new(true));
    setup_signal_handler(&running);

    tracing::info!("{}", "=".repeat(60));
    tracing::info!("PREDICTION MARKET TRADING SYSTEM — SUPERVISOR (Rust)");
    tracing::info!("PID: {}", std::process::id());
    tracing::info!("Workspace: {}", cfg.workspace.display());
    if let Some(ref mode) = cfg.mode {
        tracing::info!("Mode: {}", mode);
    }
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
                            status.code().map_or("signal".into(), |c| c.to_string()),
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
        use nix::sys::signal;
        use nix::unistd::Pid;
        let pid = proc.id() as i32;
        let _ = signal::kill(Pid::from_raw(pid), signal::Signal::SIGTERM);
        for _ in 0..20 {
            if proc.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let _ = proc.kill();
        let _ = proc.wait();
    }

    remove_pid_file(&cfg.pid_file);
    tracing::info!("Supervisor stopped.");
}
