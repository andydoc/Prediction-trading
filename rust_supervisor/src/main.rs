//! Prediction Market Trading System — Single Binary.
//!
//! A10: Everything runs in one process. No Python, no venv.
//!
//! Responsibilities:
//!   - PID file lock (prevents double-start)
//!   - Signal handling (SIGTERM, SIGINT → graceful shutdown)
//!   - Orchestrator: markets → constraints → WS → eval → positions
//!   - Dashboard (axum HTTP + SSE)
//!   - State persistence (SQLite)
//!   - Log rotation + cleanup
//!
//! CLI overrides take precedence over config.yaml.

mod orchestrator;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use clap::Parser;
use tracing_subscriber::prelude::*;

use orchestrator::{Orchestrator, OrchestratorConfig};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Prediction Market Trading System — single Rust binary
#[derive(Parser, Debug)]
#[command(name = "prediction-trader", version, about)]
struct Cli {
    /// Workspace root directory (contains config/, data/, logs/)
    #[arg(short, long, env = "TRADER_WORKSPACE")]
    workspace: Option<String>,

    /// Trading mode: shadow, live
    #[arg(short, long)]
    mode: Option<String>,

    /// Dashboard port (0 = disabled)
    #[arg(short, long)]
    port: Option<u16>,

    /// Log level: trace, debug, info, warn, error
    #[arg(short, long)]
    log_level: Option<String>,

    /// Override any config.yaml value: --set key.path=value (repeatable)
    #[arg(short, long, value_name = "KEY=VALUE")]
    set: Vec<String>,

    /// Print resolved config and exit (dry run)
    #[arg(long)]
    dry_run: bool,

    /// Skip PID lock check (for running alongside another instance)
    #[arg(long)]
    no_pid_lock: bool,
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

struct SupervisorConfig {
    workspace: PathBuf,
    log_dir: PathBuf,
    pid_file: PathBuf,
    log_level: String,
    log_retention_days: u32,
    mode: Option<String>,
    dashboard_port: Option<u16>,
    config_overrides: Vec<(String, String)>,
}

impl SupervisorConfig {
    fn load(cli: &Cli) -> Self {
        let workspace = cli.workspace.as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/home/andydoc/prediction-trader"));

        let config_path = workspace.join("config").join("config.yaml");
        let yaml_val: serde_json::Value = fs::read_to_string(&config_path)
            .ok()
            .and_then(|s| serde_yaml_ng::from_str(&s).ok())
            .unwrap_or_default();

        let log_level = cli.log_level.clone().unwrap_or_else(|| {
            yaml_val.pointer("/monitoring/logging/level")
                .and_then(|v| v.as_str())
                .unwrap_or("debug")
                .to_lowercase()
        });

        let log_retention_days = yaml_val.pointer("/monitoring/logging/retention")
            .and_then(|v| v.as_u64())
            .unwrap_or(30) as u32;

        let config_overrides: Vec<(String, String)> = cli.set.iter()
            .filter_map(|s| {
                let (k, v) = s.split_once('=')?;
                Some((k.trim().to_string(), v.trim().to_string()))
            })
            .collect();

        Self {
            log_dir: workspace.join("logs"),
            pid_file: workspace.join("prediction-trader.pid"),
            log_level,
            log_retention_days,
            mode: cli.mode.clone(),
            dashboard_port: cli.port,
            config_overrides,
            workspace,
        }
    }

    /// Apply --mode and --set overrides to config.yaml on disk.
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

        let mut val: serde_yaml_ng::Value = match serde_yaml_ng::from_str(&contents) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("Cannot parse config.yaml for overrides: {}", e);
                return;
            }
        };

        let mut changed = false;

        if let Some(ref mode) = self.mode {
            match mode.as_str() {
                "shadow" => {
                    set_yaml_value(&mut val, "mode", &serde_yaml_ng::Value::String("dual".into()));
                    set_yaml_path(&mut val, "live_trading.shadow_only", &serde_yaml_ng::Value::Bool(true));
                    set_yaml_path(&mut val, "live_trading.enabled", &serde_yaml_ng::Value::Bool(true));
                }
                "live" => {
                    set_yaml_value(&mut val, "mode", &serde_yaml_ng::Value::String("dual".into()));
                    set_yaml_path(&mut val, "live_trading.shadow_only", &serde_yaml_ng::Value::Bool(false));
                    set_yaml_path(&mut val, "live_trading.enabled", &serde_yaml_ng::Value::Bool(true));
                }
                _ => {
                    set_yaml_value(&mut val, "mode", &serde_yaml_ng::Value::String(mode.clone()));
                }
            }
            changed = true;
        }

        if let Some(port) = self.dashboard_port {
            set_yaml_path(&mut val, "dashboard.port",
                &serde_yaml_ng::Value::Number(serde_yaml_ng::Number::from(port as u64)));
            changed = true;
        }

        const ALLOWED_SET_KEYS: &[&str] = &[
            "dashboard.port", "mode", "state.db_path",
            "live_trading.shadow_only", "live_trading.enabled",
            "arbitrage.max_concurrent_positions", "arbitrage.capital_per_trade_pct",
            "arbitrage.min_trade_size", "arbitrage.max_days_to_resolution",
            "engine.state_save_interval_seconds", "engine.monitor_interval_seconds",
            "engine.constraint_rebuild_interval_seconds",
            "monitoring.logging.level",
        ];

        for (key, value) in &self.config_overrides {
            if !ALLOWED_SET_KEYS.contains(&key.as_str()) {
                tracing::warn!("Ignoring unknown --set key: {} (not in allowlist)", key);
                continue;
            }
            let yaml_value = parse_yaml_value(value);
            set_yaml_path(&mut val, key, &yaml_value);
            tracing::info!("Config override: {} = {}", key, value);
            changed = true;
        }

        if changed {
            match serde_yaml_ng::to_string(&val) {
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

fn parse_yaml_value(s: &str) -> serde_yaml_ng::Value {
    match s {
        "true" | "True" | "TRUE" => return serde_yaml_ng::Value::Bool(true),
        "false" | "False" | "FALSE" => return serde_yaml_ng::Value::Bool(false),
        _ => {}
    }
    if let Ok(n) = s.parse::<i64>() {
        return serde_yaml_ng::Value::Number(serde_yaml_ng::Number::from(n));
    }
    if let Ok(f) = s.parse::<f64>() {
        return serde_yaml_ng::Value::Number(serde_yaml_ng::Number::from(f));
    }
    serde_yaml_ng::Value::String(s.to_string())
}

fn set_yaml_value(root: &mut serde_yaml_ng::Value, key: &str, val: &serde_yaml_ng::Value) {
    if let serde_yaml_ng::Value::Mapping(ref mut map) = root {
        map.insert(serde_yaml_ng::Value::String(key.to_string()), val.clone());
    }
}

fn set_yaml_path(root: &mut serde_yaml_ng::Value, path: &str, val: &serde_yaml_ng::Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = root;
    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            if let serde_yaml_ng::Value::Mapping(ref mut map) = current {
                map.insert(serde_yaml_ng::Value::String(part.to_string()), val.clone());
            }
        } else {
            let key = serde_yaml_ng::Value::String(part.to_string());
            if let serde_yaml_ng::Value::Mapping(ref mut map) = current {
                if !map.contains_key(&key) {
                    map.insert(key.clone(), serde_yaml_ng::Value::Mapping(serde_yaml_ng::Mapping::new()));
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

    let filter_str = format!(
        "prediction_trader={},rust_engine={}", cfg.log_level, cfg.log_level
    );
    let env_filter = tracing_subscriber::EnvFilter::try_new(&filter_str)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(
            "prediction_trader=debug,rust_engine=debug"
        ));

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
    if retention_days == 0 { return; }
    let cutoff = SystemTime::now() - Duration::from_secs(retention_days as u64 * 86400);
    let mut removed = 0u32;

    if let Ok(entries) = fs::read_dir(log_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
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
            if let Ok(old_pid) = contents.trim().parse::<u32>() {
                // Platform-agnostic: try to detect if process exists
                #[cfg(unix)]
                {
                    use nix::sys::signal;
                    use nix::unistd::Pid;
                    match signal::kill(Pid::from_raw(old_pid as i32), None) {
                        Ok(_) => {
                            tracing::error!("Already running (PID {}). Exiting.", old_pid);
                            std::process::exit(1);
                        }
                        Err(_) => {
                            tracing::warn!("Stale PID file (PID {} not running). Removing.", old_pid);
                            let _ = fs::remove_file(pid_file);
                        }
                    }
                }
                #[cfg(not(unix))]
                {
                    tracing::warn!("PID file exists (PID {}). Removing (Windows).", old_pid);
                    let _ = fs::remove_file(pid_file);
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
// Signal handling
// ---------------------------------------------------------------------------

fn setup_signal_handler(running: &Arc<AtomicBool>) {
    #[cfg(unix)]
    {
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
    #[cfg(not(unix))]
    {
        let r = Arc::clone(running);
        ctrlc::set_handler(move || {
            if r.load(Ordering::Relaxed) {
                tracing::info!("Ctrl+C — shutting down gracefully");
                r.store(false, Ordering::Relaxed);
            } else {
                tracing::warn!("Second Ctrl+C — forcing exit");
                std::process::exit(1);
            }
        }).ok();
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse();
    let cfg = SupervisorConfig::load(&cli);

    init_logging(&cfg);
    cleanup_old_logs(&cfg.log_dir, cfg.log_retention_days);

    if cli.dry_run {
        println!("workspace:      {}", cfg.workspace.display());
        println!("mode:           {}", cfg.mode.as_deref().unwrap_or("(from config)"));
        println!("dashboard_port: {}", cfg.dashboard_port.map_or("(from config)".into(), |p| p.to_string()));
        println!("log_level:      {}", cfg.log_level);
        if !cfg.config_overrides.is_empty() {
            println!("overrides:");
            for (k, v) in &cfg.config_overrides {
                println!("  {} = {}", k, v);
            }
        }
        return;
    }

    if !cli.no_pid_lock {
        check_pid_lock(&cfg.pid_file);
    }
    cfg.apply_overrides_to_config();

    let running = Arc::new(AtomicBool::new(true));
    setup_signal_handler(&running);

    tracing::info!("{}", "=".repeat(60));
    tracing::info!("PREDICTION MARKET TRADING SYSTEM v{}", env!("CARGO_PKG_VERSION"));
    tracing::info!("PID: {} | Single binary (A10)", std::process::id());
    tracing::info!("Workspace: {}", cfg.workspace.display());
    if let Some(ref mode) = cfg.mode {
        tracing::info!("Mode: {}", mode);
    }
    tracing::info!("{}", "=".repeat(60));

    // Load orchestrator config
    let orch_cfg = OrchestratorConfig::load(&cfg.workspace);

    // Create and run orchestrator
    match Orchestrator::new(orch_cfg) {
        Ok(mut orchestrator) => {
            orchestrator.run(running.clone());
        }
        Err(e) => {
            tracing::error!("Failed to create orchestrator: {}", e);
            std::process::exit(1);
        }
    }

    if !cli.no_pid_lock {
        remove_pid_file(&cfg.pid_file);
    }
    tracing::info!("System stopped.");
}
