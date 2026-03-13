"""Supervisor - Prediction Market Trading System

Startup sequence:
  1. Initial Market Scanner (initial_market_scanner.py) — runs ONCE:
     a. Postponement detection: checks overdue positions via AI web search
     b. Market scan: fetches all markets from Polymarket Gamma API (synchronous REST)
  2. Trading Engine (trading_engine.py) — event-driven: constraints, arb math, execution via WS
  3. Dashboard (Rust axum, port 5556) — starts automatically inside trading_engine

The scanner populates latest_markets.json which the trading engine needs at startup.
After that, WS handles all ongoing market discovery and price monitoring.
"""
import json, logging, os, signal, subprocess, sys, time
from pathlib import Path
from datetime import datetime

WORKSPACE   = Path('/home/andydoc/prediction-trader')
VENV_PYTHON = '/home/andydoc/prediction-trader-env/bin/python'
LOG_DIR     = WORKSPACE / 'logs'
STATUS_DIR  = WORKSPACE / 'data'
PID_FILE    = WORKSPACE / 'supervisor.pid'
LOG_DIR.mkdir(parents=True, exist_ok=True)

# --- Startup log cleanup: keep only last 3 days of logs ---
def cleanup_old_logs(max_days=3):
    """Remove log files older than max_days to prevent disk bloat."""
    import glob
    now = time.time()
    cutoff = now - (max_days * 86400)
    removed = 0
    for f in LOG_DIR.glob('*.log'):
        try:
            if f.stat().st_mtime < cutoff:
                f.unlink()
                removed += 1
        except Exception:
            pass
    if removed:
        print(f'[STARTUP] Cleaned {removed} old log files (>{max_days} days)')

cleanup_old_logs()

logging.basicConfig(level=logging.DEBUG,
    format='%(asctime)s - [SUPERVISOR] %(levelname)s - %(message)s',
    handlers=[logging.FileHandler(str(LOG_DIR / f'supervisor_{datetime.now().strftime("%Y%m%d")}.log')), logging.StreamHandler()])
log = logging.getLogger('supervisor')

LAYERS = [
    {'name': 'trading_engine', 'script': WORKSPACE / 'trading' / 'trading_engine.py', 'restart_delay': 10},
    # Dashboard now runs inside trading_engine (Rust axum server, same process)
]
processes = {}
running = True

def check_pid_lock():
    """Prevent double-start of supervisor"""
    if PID_FILE.exists():
        old_pid = int(PID_FILE.read_text().strip())
        try:
            os.kill(old_pid, 0)  # Check if process exists
            log.error(f'Supervisor already running (PID {old_pid}). Exiting.')
            sys.exit(1)
        except OSError:
            log.warning(f'Stale PID file (PID {old_pid} not running). Removing.')
            PID_FILE.unlink()
    PID_FILE.write_text(str(os.getpid()))

def start_layer(layer):
    name = layer['name']
    # Layers have their own log files via logging framework - discard stdout to prevent duplication
    devnull = open(os.devnull, 'w')
    proc = subprocess.Popen([VENV_PYTHON, str(layer['script'])], cwd=str(WORKSPACE), stdout=devnull, stderr=subprocess.STDOUT)
    log.info(f'Started {name} pid={proc.pid}')
    return proc

def shutdown(signum, frame):
    global running
    log.info(f'Signal {signum} - shutting down')
    running = False
    for name, proc in processes.items():
        if proc and proc.poll() is None:
            proc.terminate()
    PID_FILE.unlink(missing_ok=True)
    sys.exit(0)

def main():
    check_pid_lock()
    signal.signal(signal.SIGTERM, shutdown)
    signal.signal(signal.SIGINT, shutdown)
    log.info('=' * 60)
    log.info('PREDICTION MARKET TRADING SYSTEM - SUPERVISOR')
    log.info(f'PID: {os.getpid()}')
    log.info('=' * 60)

    # --- Run initial scanner: postponement check + market fetch (synchronous, blocking) ---
    scanner_script = WORKSPACE / 'utilities' / 'initial_market_scanner.py'
    log.info('Running initial scanner (postponement check + market fetch)...')
    try:
        scanner_proc = subprocess.run(
            [VENV_PYTHON, str(scanner_script)],
            cwd=str(WORKSPACE),
            timeout=300,  # 5 min: allows for postponement AI calls (60s rate limit each)
        )
        if scanner_proc.returncode != 0:
            log.error(f'Scanner failed (exit code {scanner_proc.returncode}). Starting engine anyway (may use stale data).')
        else:
            log.info('Scanner complete — latest_markets.json ready')
    except subprocess.TimeoutExpired:
        log.warning('Scanner timed out after 300s — starting engine with whatever data is available')

    # --- Start supervised processes ---
    for layer in LAYERS:
        processes[layer['name']] = start_layer(layer)
        time.sleep(2)
    log.info('All layers + dashboard started. Monitoring...')
    while running:
        for layer in LAYERS:
            name = layer['name']
            proc = processes.get(name)
            if proc is None or proc.poll() is not None:
                exit_code = proc.poll() if proc else 'never started'
                log.warning(f'{name} exited (code={exit_code}), restarting in {layer["restart_delay"]}s')
                time.sleep(layer['restart_delay'])
                processes[name] = start_layer(layer)
            else:
                log.debug(f'{name} healthy pid={proc.pid}')
        time.sleep(15)

if __name__ == '__main__':
    main()
