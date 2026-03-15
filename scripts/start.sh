#!/bin/bash
# start.sh — Start the prediction trading system
#
# Usage:
#   bash start.sh                         # default (shadow mode)
#   bash start.sh --mode live             # start in live mode
#   bash start.sh --set arbitrage.min_profit_threshold=0.05
#   bash start.sh --dry-run               # print config and exit
#
# On first run: builds the Rust supervisor binary if not present.
# If P: drive is not mounted (WSL via Windows), mounts it.

set -e
WORKSPACE="${TRADER_WORKSPACE:-/home/andydoc/prediction-trader}"
BINARY="$WORKSPACE/rust_supervisor/target/release/prediction-trader"

cd "$WORKSPACE"

# --- Ensure P: drive is mounted (Windows/WSL only) ---
if [ -n "$WSL_DISTRO_NAME" ] && ! mountpoint -q /mnt/p 2>/dev/null; then
    # Check if subst P: exists on Windows side
    if ! ls /mnt/p/ >/dev/null 2>&1; then
        echo "[start] P: drive not mounted — mounting via subst..."
        cmd.exe /c "subst P: \\\\wsl.localhost\\Ubuntu\\home\\andydoc\\prediction-trader" 2>/dev/null || true
        sleep 1
    fi
fi

# --- Build if needed ---
if [ ! -f "$BINARY" ]; then
    echo "[start] Building supervisor binary..."
    cd "$WORKSPACE/rust_supervisor"
    cargo build --release
    cd "$WORKSPACE"
fi

# --- Build Rust engine if needed ---
RUST_ENGINE="$WORKSPACE/.venv/lib/python3.12/site-packages/rust_engine"
if [ ! -d "$RUST_ENGINE" ] && [ -f "$WORKSPACE/rust_engine/Cargo.toml" ]; then
    echo "[start] Building Rust engine (maturin)..."
    source "$WORKSPACE/.venv/bin/activate" 2>/dev/null || source "$WORKSPACE/../prediction-trader-env/bin/activate"
    cd "$WORKSPACE/rust_engine"
    maturin develop --release
    cd "$WORKSPACE"
fi

# --- Pull latest code (non-destructive) ---
echo "[start] Pulling latest code..."
git pull --ff-only origin main 2>&1 || echo "  WARNING: git pull failed — continuing with local code"

# --- Clean stale PID files ---
rm -f "$WORKSPACE"/*.pid

# --- Create log directory ---
mkdir -p "$WORKSPACE/logs"

# --- Start supervisor ---
echo "[start] Starting prediction-trader supervisor..."
nohup "$BINARY" "$@" >> "$WORKSPACE/logs/supervisor_start.log" 2>&1 &
SUPERVISOR_PID=$!
disown $SUPERVISOR_PID
echo "[start] Supervisor PID: $SUPERVISOR_PID"

# --- Wait for startup ---
echo "[start] Waiting for engine startup..."
sleep 15

# --- Verify ---
echo ""
echo "=== Process check ==="
ps aux | grep -E 'prediction-trader|trading_engine' | grep -v grep | awk '{print $2, $NF}' || echo "  No processes found"

echo ""
echo "=== Dashboard ==="
HTTP=$(curl -s -o /dev/null -w "%{http_code}" http://localhost:5556/ 2>/dev/null)
if [ "$HTTP" = "200" ]; then
    echo "  Dashboard OK (HTTP 200) — http://localhost:5556"
else
    echo "  Dashboard NOT responding (HTTP $HTTP) — may still be starting"
fi

echo ""
echo "[start] Done."
