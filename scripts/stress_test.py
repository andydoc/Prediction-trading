#!/usr/bin/env python3
"""E2.5: Stress test harness for prediction-trader engine.

Runs the engine with one parameter varied at a time, collects metrics
via the /metrics endpoint, and writes results to data/stress_test.db.

Usage:
    python3 scripts/stress_test.py --param max_evals_per_batch --cycle 300
    python3 scripts/stress_test.py --param efp_drift_threshold --values 0.001,0.010,0.020 --cycle 300
"""

import argparse
import json
import os
import shutil
import signal
import sqlite3
import statistics
import subprocess
import sys
import time
import urllib.request
import urllib.error

# ---------------------------------------------------------------------------
# Parameter definitions: (min, max, type) matching PRODUCT_SPEC_v2 test ranges
# ---------------------------------------------------------------------------
PARAMS = {
    "max_evals_per_batch":                 (100,   2000,  int),
    "efp_drift_threshold":                 (0.001, 0.020, float),
    "efp_staleness_seconds":               (1.0,   30.0,  float),
    "constraint_rebuild_interval_seconds": (60,    1800,  int),
    "stale_sweep_interval_seconds":        (10,    300,   int),
    "stale_asset_threshold_seconds":       (5,     120,   int),
    "state_save_interval_seconds":         (5,     120,   int),
}

# Failure signal thresholds per parameter
# Failure signals: (metric, op, threshold, min_breach_pct, description)
# min_breach_pct = fraction of samples that must breach to flag failure (0.5 = 50%)
# This avoids false positives from startup transients or shutdown artifacts.
# Cumulative counters (ws_reconnects, evals_total, etc.) use "delta" mode —
# checked via the summary's max_val - min_val (growth during the run).
FAILURE_SIGNALS = {
    "max_evals_per_batch": [
        ("lat_p95", ">", 50000, 0.5, "lat_p95 > 50ms sustained"),
        # ws_pong_timeouts and ws_heartbeat_failures are cumulative — checked as delta
        ("ws_pong_timeouts", "delta>", 0, 0.0, "missed WS heartbeats (pong timeouts)"),
        ("ws_heartbeat_failures", "delta>", 0, 0.0, "WS heartbeat send failures"),
    ],
    "efp_drift_threshold": [
        ("queue_urgent", ">", 500, 0.5, "queue_urgent > 500 sustained (too sensitive)"),
        # opps_found checked as delta — low growth = missed price moves
    ],
    "efp_staleness_seconds": [
        ("stale_books_30s", ">", 20, 0.5, "stale-book false positives sustained"),
        ("stale_assets_swept", "delta>", 500, 0.0, "excessive stale asset churn"),
    ],
    "constraint_rebuild_interval_seconds": [
        # constraint count stagnation detected by low constraints delta
    ],
    "stale_sweep_interval_seconds": [
        ("stale_books_60s", ">", 10, 0.5, "stale books accumulating (too slow)"),
        ("cpu_pct", ">", 80, 0.5, "excessive CPU sustained (too fast)"),
        ("ws_reconnects", "delta>", 5, 0.0, "WS reconnects during test (instability)"),
    ],
    "stale_asset_threshold_seconds": [
        ("stale_books_60s", ">", 10, 0.5, "stale books in arb math (too high)"),
        ("ws_reconnects", "delta>", 5, 0.0, "churning re-subscribes causing reconnects"),
    ],
    "state_save_interval_seconds": [
        ("cpu_pct", ">", 80, 0.5, "excessive CPU from disk I/O (too frequent)"),
    ],
}

# Metrics to include in the summary table output
SUMMARY_METRICS = [
    "cpu_pct", "mem_mb", "lat_p50", "lat_p95", "lat_max",
    "lat_e2e_p50", "lat_e2e_p95", "lat_eval_p50", "lat_eval_p95",
    "queue_urgent", "queue_total",
    "ws_msg_rate", "ws_live", "ws_connections",
    "ws_reconnects", "ws_pong_timeouts", "ws_heartbeat_failures",
    "stale_books_30s", "stale_books_60s",
    "evals_total", "opps_found",
    "stale_sweeps", "stale_assets_swept",
    "constraints", "markets",
    "disk_used_gb",
]

# Sample columns that map directly from /metrics JSON
SAMPLE_COLUMNS = [
    "iteration", "cpu_pct", "mem_mb", "disk_used_gb",
    "queue_urgent", "queue_background", "queue_total",
    "lat_p50", "lat_p95", "lat_max",
    "lat_e2e_p50", "lat_e2e_p95",
    "lat_eval_p50", "lat_eval_p95",
    "ws_msgs", "ws_live", "ws_connections", "ws_msg_rate",
    "ws_reconnects", "ws_pong_timeouts", "ws_heartbeat_failures",
    "constraints", "markets",
    "stale_books_30s", "stale_books_60s",
    "evals_total", "opps_found",
    "stale_sweeps", "stale_assets_swept",
]


# Current production defaults from config.yaml
DEFAULTS = {
    "max_evals_per_batch":                 500,
    "efp_drift_threshold":                 0.005,
    "efp_staleness_seconds":               5.0,
    "constraint_rebuild_interval_seconds": 600,
    "stale_sweep_interval_seconds":        60,
    "stale_asset_threshold_seconds":       30,
    "state_save_interval_seconds":         30,
}

# Composite score weights: lower is better for all except evals (higher = better)
# The safe zone is defined as: no failure flags triggered for that value.
# The recommended value is the safe-zone value with the best composite score.
SCORE_WEIGHTS = {
    "cpu_pct":    1.0,   # lower CPU is better
    "lat_p95":    1.0,   # lower latency is better
    "queue_urgent": 0.5, # lower queue depth is better
}


def default_values(lo, hi, typ):
    """Generate 5 equally-spaced values across [lo, hi]."""
    step = (hi - lo) / 4
    vals = [lo + step * i for i in range(5)]
    return [typ(v) for v in vals]


def format_value(v, typ):
    """Format a parameter value for display."""
    if typ == float:
        return f"{v:.4f}" if v < 1 else f"{v:.1f}"
    return str(int(v))


# ---------------------------------------------------------------------------
# SQLite
# ---------------------------------------------------------------------------

def init_db(db_path):
    os.makedirs(os.path.dirname(db_path) or ".", exist_ok=True)
    conn = sqlite3.connect(db_path)
    conn.executescript("""
        CREATE TABLE IF NOT EXISTS runs (
            run_id       TEXT PRIMARY KEY,
            param_name   TEXT NOT NULL,
            param_value  REAL NOT NULL,
            start_ts     REAL NOT NULL,
            end_ts       REAL,
            cycle_seconds INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS samples (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id          TEXT NOT NULL REFERENCES runs(run_id),
            ts              REAL NOT NULL,
            iteration       INTEGER,
            cpu_pct         REAL,
            mem_mb          REAL,
            disk_used_gb    REAL,
            queue_urgent    INTEGER,
            queue_background INTEGER,
            queue_total     INTEGER,
            lat_p50         INTEGER,
            lat_p95         INTEGER,
            lat_max         INTEGER,
            lat_e2e_p50     INTEGER,
            lat_e2e_p95     INTEGER,
            lat_eval_p50    INTEGER,
            lat_eval_p95    INTEGER,
            ws_msgs         INTEGER,
            ws_live         INTEGER,
            ws_connections  INTEGER,
            ws_msg_rate     REAL,
            ws_reconnects   INTEGER,
            ws_pong_timeouts INTEGER,
            ws_heartbeat_failures INTEGER,
            constraints     INTEGER,
            markets         INTEGER,
            stale_books_30s INTEGER,
            stale_books_60s INTEGER,
            evals_total     INTEGER,
            opps_found      INTEGER,
            stale_sweeps    INTEGER,
            stale_assets_swept INTEGER
        );
        CREATE TABLE IF NOT EXISTS summary (
            run_id       TEXT NOT NULL,
            metric_name  TEXT NOT NULL,
            p50          REAL,
            p95          REAL,
            max_val      REAL,
            mean_val     REAL,
            min_val      REAL,
            sample_count INTEGER,
            failure_flag TEXT,
            PRIMARY KEY (run_id, metric_name)
        );
    """)
    conn.close()


def insert_run(db_path, run_id, param_name, param_value, cycle_seconds):
    conn = sqlite3.connect(db_path)
    conn.execute(
        "INSERT OR REPLACE INTO runs VALUES (?, ?, ?, ?, NULL, ?)",
        (run_id, param_name, param_value, time.time(), cycle_seconds),
    )
    conn.commit()
    conn.close()


def update_run_end(db_path, run_id):
    conn = sqlite3.connect(db_path)
    conn.execute("UPDATE runs SET end_ts = ? WHERE run_id = ?", (time.time(), run_id))
    conn.commit()
    conn.close()


def insert_sample(db_path, run_id, metrics):
    conn = sqlite3.connect(db_path)
    cols = ["run_id", "ts"] + SAMPLE_COLUMNS
    vals = [run_id, time.time()] + [metrics.get(c) for c in SAMPLE_COLUMNS]
    placeholders = ",".join(["?"] * len(cols))
    conn.execute(f"INSERT INTO samples ({','.join(cols)}) VALUES ({placeholders})", vals)
    conn.commit()
    conn.close()


def compute_summary(db_path, run_id, param_name, param_value, samples):
    """Compute p50/p95/max/mean/min for each metric and check failure signals."""
    # Drop last sample — often a shutdown artifact (stale counts spike, metrics stale)
    if len(samples) > 2:
        samples = samples[:-1]
    conn = sqlite3.connect(db_path)
    signals = FAILURE_SIGNALS.get(param_name, [])

    for metric in SUMMARY_METRICS:
        values = [s.get(metric) for s in samples if s.get(metric) is not None]
        if not values:
            continue
        numeric = [float(v) for v in values]
        numeric.sort()
        n = len(numeric)
        p50 = numeric[n // 2]
        p95_idx = min(int(n * 0.95 + 0.5) - 1, n - 1)
        p95 = numeric[max(0, p95_idx)]
        max_val = numeric[-1]
        mean_val = statistics.mean(numeric)
        min_val = numeric[0]

        # Check failure signals — count breaching samples, require sustained breach
        failure = None
        for sig_metric, op, threshold, min_pct, msg in signals:
            if sig_metric != metric:
                continue
            if op == "delta>":
                # For cumulative counters: check growth during the run (max - min)
                delta = max_val - min_val
                if delta > threshold:
                    failure = f"{msg} (delta={int(delta)})"
            elif op == ">":
                breach_count = sum(1 for v in numeric if v > threshold)
                breach_pct = breach_count / n if n > 0 else 0
                if breach_pct >= min_pct:
                    failure = f"{msg} ({breach_count}/{n}={breach_pct:.0%})"
            elif op == "<":
                breach_count = sum(1 for v in numeric if v < threshold)
                breach_pct = breach_count / n if n > 0 else 0
                if breach_pct >= min_pct:
                    failure = f"{msg} ({breach_count}/{n}={breach_pct:.0%})"

        conn.execute(
            "INSERT OR REPLACE INTO summary VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (run_id, metric, p50, p95, max_val, mean_val, min_val, n, failure),
        )
    conn.commit()
    conn.close()


# ---------------------------------------------------------------------------
# Engine management
# ---------------------------------------------------------------------------

def start_engine(binary, workspace, port, mode, param, value):
    cmd = [
        binary,
        "--workspace", workspace,
        "--port", str(port),
        "--mode", mode,
        "--no-pid-lock",
        "--set", f"engine.{param}={value}",
    ]
    print(f"  Starting engine: --set engine.{param}={value}")
    proc = subprocess.Popen(
        cmd,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    return proc


def stop_engine(proc):
    if proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=15)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


def crash_engine(proc):
    """SIGKILL the engine to simulate a crash (no graceful shutdown)."""
    if proc.poll() is not None:
        return
    proc.kill()
    proc.wait(timeout=5)


def measure_state_loss(workspace, crash_time):
    """Measure state loss from a simulated crash.

    Returns the number of seconds since the last successful state save,
    based on the state_rust.db file modification time.
    """
    db_path = os.path.join(workspace, "data", "state_rust.db")
    if not os.path.exists(db_path):
        return None
    db_mtime = os.path.getmtime(db_path)
    return max(0, crash_time - db_mtime)


def run_crash_test(binary, workspace, port, mode, param, value, cycle, settle, db_path, run_id):
    """Special test for state_save_interval_seconds: measures actual state loss from crashes.

    Runs the engine, waits for it to stabilize, then performs 3 crash+restart cycles
    at random points in the save interval. Measures actual state loss each time.
    """
    import random

    save_interval = float(value)
    crash_results = []

    print(f"  Crash simulation: {3} cycles, save_interval={save_interval}s")

    for crash_num in range(3):
        # Start engine
        proc = start_engine(binary, workspace, port, mode, param, value)
        try:
            wait_for_dashboard(port)
        except RuntimeError as e:
            print(f"  ERROR: {e}")
            stop_engine(proc)
            continue

        # Wait for the engine to do at least 2 saves so we know it's actively saving
        # Poll db mtime until we see it update at least twice
        db_path_state = os.path.join(workspace, "data", "state_rust.db")
        initial_mtime = os.path.getmtime(db_path_state) if os.path.exists(db_path_state) else 0
        saves_seen = 0
        last_seen_mtime = initial_mtime
        wait_deadline = time.time() + save_interval * 4 + settle

        while time.time() < wait_deadline and saves_seen < 2:
            time.sleep(2)
            if os.path.exists(db_path_state):
                current_mtime = os.path.getmtime(db_path_state)
                if current_mtime > last_seen_mtime + 0.5:  # at least 0.5s newer
                    saves_seen += 1
                    last_seen_mtime = current_mtime

        if saves_seen < 1:
            print(f"    Crash {crash_num+1}/3: no saves detected, skipping crash test")
            stop_engine(proc)
            continue

        # Now wait a random fraction of save_interval before crashing
        extra_wait = random.random() * save_interval
        print(f"    Crash {crash_num+1}/3: {saves_seen} saves seen, waiting {extra_wait:.0f}s then SIGKILL...")
        time.sleep(extra_wait)

        # Record crash time and last known save mtime, then SIGKILL
        pre_crash_mtime = os.path.getmtime(db_path_state) if os.path.exists(db_path_state) else last_seen_mtime
        crash_time = time.time()
        crash_engine(proc)

        # State loss = time since last successful disk save
        loss_secs = crash_time - pre_crash_mtime
        crash_results.append(loss_secs)
        print(f"    State loss: {loss_secs:.1f}s (save_interval={save_interval}s)")

        time.sleep(2)  # Brief pause before next cycle

    # Record crash results as summary metrics
    if crash_results:
        conn = sqlite3.connect(db_path)
        n = len(crash_results)
        crash_results.sort()
        p50 = crash_results[n // 2]
        max_loss = crash_results[-1]
        mean_loss = statistics.mean(crash_results)
        min_loss = crash_results[0]

        # Failure: state loss exceeds the save interval (should be impossible but check)
        # or state loss exceeds a reasonable threshold
        failure = None
        if max_loss > save_interval * 1.5:
            failure = f"state loss {max_loss:.0f}s exceeds 1.5x interval {save_interval}s"

        conn.execute(
            "INSERT OR REPLACE INTO summary VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (run_id, "crash_state_loss_secs", p50, max_loss, max_loss, mean_loss, min_loss, n, failure),
        )
        conn.commit()
        conn.close()

    return crash_results


def wait_for_dashboard(port, timeout=60):
    url = f"http://127.0.0.1:{port}/metrics"
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            resp = urllib.request.urlopen(url, timeout=3)
            data = json.loads(resp.read())
            if "iteration" in data:
                return True
        except Exception:
            pass
        time.sleep(2)
    raise RuntimeError(f"Dashboard not ready on port {port} after {timeout}s")


def poll_metrics(port):
    try:
        url = f"http://127.0.0.1:{port}/metrics"
        resp = urllib.request.urlopen(url, timeout=5)
        return json.loads(resp.read())
    except Exception:
        return None


# ---------------------------------------------------------------------------
# Config backup/restore
# ---------------------------------------------------------------------------

def backup_config(workspace):
    src = os.path.join(workspace, "config", "config.yaml")
    dst = os.path.join(workspace, "config", "config.yaml.stress_backup")
    if os.path.exists(src):
        shutil.copy2(src, dst)
        print(f"  Config backed up: {dst}")
    return dst


def restore_config(workspace):
    src = os.path.join(workspace, "config", "config.yaml.stress_backup")
    dst = os.path.join(workspace, "config", "config.yaml")
    if os.path.exists(src):
        shutil.copy2(src, dst)
        os.remove(src)
        print(f"  Config restored from backup")


# ---------------------------------------------------------------------------
# Summary table
# ---------------------------------------------------------------------------

def print_summary_table(db_path, param_name, values, typ):
    conn = sqlite3.connect(db_path)
    cursor = conn.cursor()

    # Header
    print(f"\n{'='*120}")
    print(f"  Stress Test Results: engine.{param_name}")
    print(f"{'='*120}")
    crash_hdr = " | CrashS" if param_name == "state_save_interval_seconds" else ""
    hdr = (f"{'Value':>10} | {'N':>4} | {'CPU%':>5} | {'Latp95':>6} | {'Q_urg':>5} "
           f"| {'St30':>4} | {'St60':>4} | {'Recon':>5} | {'Pong':>4} | {'Evals':>6} | {'Opps':>5}{crash_hdr} | Failures")
    print(hdr)
    print("-" * len(hdr))

    for v in values:
        run_id = f"{param_name}_{v}"

        # Get sample count
        cursor.execute("SELECT COUNT(*) FROM samples WHERE run_id = ?", (run_id,))
        n = cursor.fetchone()[0]

        def get_stat(metric, stat_col):
            cursor.execute(
                f"SELECT {stat_col} FROM summary WHERE run_id = ? AND metric_name = ?",
                (run_id, metric),
            )
            row = cursor.fetchone()
            return row[0] if row else None

        def get_delta(metric):
            """For cumulative counters: max - min = growth during run."""
            mx = get_stat(metric, "max_val")
            mn = get_stat(metric, "min_val")
            if mx is not None and mn is not None:
                return int(mx - mn)
            return None

        cpu_p95 = get_stat("cpu_pct", "p95")
        lat_p95 = get_stat("lat_p95", "p95")
        q_urg = get_stat("queue_urgent", "p95")
        stale30 = get_stat("stale_books_30s", "p95")
        stale60 = get_stat("stale_books_60s", "p95")
        reconn = get_delta("ws_reconnects")
        pong = get_delta("ws_pong_timeouts")
        evals = get_delta("evals_total")
        opps = get_delta("opps_found")
        crash_loss = get_stat("crash_state_loss_secs", "max_val")

        # Collect all failures for this run
        cursor.execute(
            "SELECT failure_flag FROM summary WHERE run_id = ? AND failure_flag IS NOT NULL",
            (run_id,),
        )
        failures = [row[0] for row in cursor.fetchall()]
        fail_str = "; ".join(failures) if failures else "-"

        val_str = format_value(v, typ)
        def fmt(v, suffix=""):
            return f"{int(v)}{suffix}" if v is not None else "?"
        def fmtf(v, suffix="%"):
            return f"{v:.1f}{suffix}" if v is not None else "?"

        # Add crash loss column for state_save_interval_seconds
        crash_col = ""
        if param_name == "state_save_interval_seconds":
            crash_col = f" | {fmtf(crash_loss, 's'):>6}"

        print(f"{val_str:>10} | {n:>4} | {fmtf(cpu_p95):>5} | {fmt(lat_p95):>6} | {fmt(q_urg):>5} "
              f"| {fmt(stale30):>4} | {fmt(stale60):>4} | {fmt(reconn):>5} | {fmt(pong):>4} | {fmt(evals):>6} | {fmt(opps):>5}{crash_col} | {fail_str}")

    print()
    conn.close()


# ---------------------------------------------------------------------------
# Recommendation & config update
# ---------------------------------------------------------------------------

def print_recommendation(db_path, param_name, values, typ, workspace):
    """Analyse results, identify safe zone, recommend best value, update config if needed."""
    conn = sqlite3.connect(db_path)
    cursor = conn.cursor()

    current_default = DEFAULTS.get(param_name)

    # Identify safe zone: values with zero failure flags
    safe_values = []
    failed_values = []
    for v in values:
        run_id = f"{param_name}_{v}"
        cursor.execute(
            "SELECT COUNT(*) FROM summary WHERE run_id = ? AND failure_flag IS NOT NULL",
            (run_id,),
        )
        n_failures = cursor.fetchone()[0]
        if n_failures == 0:
            safe_values.append(v)
        else:
            failed_values.append(v)

    # Score safe values by composite metric (lower = better)
    best_value = None
    best_score = float("inf")
    value_scores = {}

    for v in safe_values:
        run_id = f"{param_name}_{v}"
        score = 0.0
        for metric, weight in SCORE_WEIGHTS.items():
            cursor.execute(
                "SELECT p95 FROM summary WHERE run_id = ? AND metric_name = ?",
                (run_id, metric),
            )
            row = cursor.fetchone()
            if row and row[0] is not None:
                score += row[0] * weight
        value_scores[v] = score
        if score < best_score:
            best_score = score
            best_value = v

    conn.close()

    # Print recommendation
    print(f"{'='*60}")
    print(f"  Recommendation: engine.{param_name}")
    print(f"{'='*60}")

    if safe_values:
        safe_str = ", ".join(format_value(v, typ) for v in safe_values)
        print(f"  Safe zone:    [{safe_str}]")
    else:
        print(f"  Safe zone:    NONE (all values triggered failures)")

    if failed_values:
        fail_str = ", ".join(format_value(v, typ) for v in failed_values)
        print(f"  Failed:       [{fail_str}]")

    if best_value is not None:
        print(f"  Recommended:  {format_value(best_value, typ)} (score={best_score:.1f})")
    else:
        print(f"  Recommended:  unable to determine (no safe values)")

    if current_default is not None:
        print(f"  Current default: {current_default}")
        if current_default in safe_values or not safe_values:
            print(f"  Status: DEFAULT IS IN SAFE ZONE — no change needed")
        else:
            print(f"  Status: DEFAULT IS OUTSIDE SAFE ZONE")
            if best_value is not None:
                update_config_default(workspace, param_name, best_value, typ)

    print()


def update_config_default(workspace, param_name, new_value, typ):
    """Update config.yaml with the recommended value."""
    config_path = os.path.join(workspace, "config", "config.yaml")
    if not os.path.exists(config_path):
        print(f"  WARNING: config.yaml not found at {config_path}")
        return

    with open(config_path, "r") as f:
        lines = f.readlines()

    key = f"{param_name}:"
    updated = False
    for i, line in enumerate(lines):
        stripped = line.lstrip()
        if stripped.startswith(key):
            indent = line[:len(line) - len(stripped)]
            if typ == float:
                lines[i] = f"{indent}{param_name}: {new_value}\n"
            else:
                lines[i] = f"{indent}{param_name}: {int(new_value)}\n"
            updated = True
            break

    if updated:
        with open(config_path, "w") as f:
            f.writelines(lines)
        val_str = format_value(new_value, typ)
        print(f"  CONFIG UPDATED: engine.{param_name} = {val_str} (was {DEFAULTS.get(param_name)})")
    else:
        print(f"  WARNING: could not find {param_name} in config.yaml")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description="E2.5: Stress test harness")
    parser.add_argument("--param", required=True, help="Parameter short name (e.g. max_evals_per_batch)")
    parser.add_argument("--values", help="Comma-separated values (default: 5 equally-spaced)")
    parser.add_argument("--cycle", type=int, default=3600, help="Soak seconds per value (default: 3600)")
    parser.add_argument("--workspace", default="/home/andydoc/prediction-trader", help="Workspace root")
    parser.add_argument("--port", type=int, default=5559, help="Dashboard port (default: 5559)")
    parser.add_argument("--poll", type=int, default=10, help="Poll interval seconds (default: 10)")
    parser.add_argument("--settle", type=int, default=30, help="Warm-up seconds (default: 30)")
    parser.add_argument("--db", default="data/stress_test.db", help="Output SQLite path")
    parser.add_argument("--binary", default="target/release/prediction-trader", help="Engine binary")
    parser.add_argument("--mode", default="shadow", help="Engine mode (default: shadow)")
    args = parser.parse_args()

    param = args.param
    if param not in PARAMS:
        print(f"Unknown parameter: {param}")
        print(f"Available: {', '.join(sorted(PARAMS.keys()))}")
        sys.exit(1)

    lo, hi, typ = PARAMS[param]

    if args.values:
        values = [typ(v.strip()) for v in args.values.split(",")]
    else:
        values = default_values(lo, hi, typ)

    db_path = os.path.join(args.workspace, args.db) if not os.path.isabs(args.db) else args.db

    print(f"E2.5 Stress Test: engine.{param}")
    print(f"  Values: {[format_value(v, typ) for v in values]}")
    print(f"  Cycle: {args.cycle}s  Settle: {args.settle}s  Poll: {args.poll}s")
    print(f"  DB: {db_path}")
    print()

    init_db(db_path)
    backup_config(args.workspace)

    # SIGINT handler — restore config and stop engine
    current_proc = [None]

    def sigint_handler(sig, frame):
        print("\n  SIGINT received — stopping engine and restoring config...")
        if current_proc[0]:
            stop_engine(current_proc[0])
        restore_config(args.workspace)
        sys.exit(1)

    signal.signal(signal.SIGINT, sigint_handler)

    try:
        for i, value in enumerate(values):
            run_id = f"{param}_{value}"
            print(f"[{i+1}/{len(values)}] engine.{param} = {format_value(value, typ)}")

            insert_run(db_path, run_id, param, float(value), args.cycle)

            proc = start_engine(args.binary, args.workspace, args.port, args.mode, param, value)
            current_proc[0] = proc

            try:
                wait_for_dashboard(args.port)
            except RuntimeError as e:
                print(f"  ERROR: {e}")
                stop_engine(proc)
                current_proc[0] = None
                continue

            print(f"  Settling for {args.settle}s...")
            time.sleep(args.settle)

            print(f"  Collecting metrics for {args.cycle}s (poll every {args.poll}s)...")
            start_time = time.time()
            samples = []
            poll_count = 0

            while time.time() - start_time < args.cycle:
                metrics = poll_metrics(args.port)
                if metrics:
                    insert_sample(db_path, run_id, metrics)
                    samples.append(metrics)
                    poll_count += 1
                    elapsed = int(time.time() - start_time)
                    remaining = args.cycle - elapsed
                    if poll_count % 6 == 0:  # progress every ~60s
                        print(f"    {elapsed}s elapsed, {remaining}s remaining, {poll_count} samples")
                time.sleep(args.poll)

            print(f"  Stopping engine ({poll_count} samples collected)...")
            stop_engine(proc)
            current_proc[0] = None

            update_run_end(db_path, run_id)
            compute_summary(db_path, run_id, param, value, samples)

            # Crash simulation for state_save_interval_seconds
            if param == "state_save_interval_seconds":
                run_crash_test(
                    args.binary, args.workspace, args.port, args.mode,
                    param, value, args.cycle, args.settle, db_path, run_id,
                )

            print(f"  Done.\n")

    finally:
        restore_config(args.workspace)

    print_summary_table(db_path, param, values, typ)
    print_recommendation(db_path, param, values, typ, args.workspace)


if __name__ == "__main__":
    main()
