#!/bin/bash
# start.sh — Start the prediction trading system (single binary)
#
# Usage:
#   bash start.sh                         # default (shadow mode)
#   bash start.sh --mode live             # start in live mode
#   bash start.sh --set arbitrage.min_profit_threshold=0.05
#   bash start.sh --dry-run               # print config and exit
#
# On first run: builds the Rust binary if not present.

set -e
WORKSPACE="${TRADER_WORKSPACE:-/home/andydoc/prediction-trader}"
BINARY="$WORKSPACE/target/release/prediction-trader"

cd "$WORKSPACE"

# --- Build if needed ---
if [ ! -f "$BINARY" ]; then
    echo "[start] Building prediction-trader binary..."
    cargo build --release --manifest-path "$WORKSPACE/Cargo.toml"
fi

# --- Pull latest code (non-destructive) ---
echo "[start] Pulling latest code..."
git pull --ff-only origin main 2>&1 || echo "  WARNING: git pull failed — continuing with local code"

# --- Clean stale PID files ---
rm -f "$WORKSPACE"/*.pid

# --- Create log directory ---
mkdir -p "$WORKSPACE/logs"

# --- Start ---
echo "[start] Starting prediction-trader..."
nohup "$BINARY" --workspace "$WORKSPACE" "$@" >> "$WORKSPACE/logs/supervisor_start.log" 2>&1 &
PID=$!
disown $PID
echo "[start] PID: $PID"

# --- Wait for startup ---
echo "[start] Waiting for engine startup..."
sleep 15

# --- Verify ---
echo ""
echo "=== Process check ==="
ps aux | grep prediction-trader | grep -v grep | awk '{print $2, $NF}' || echo "  No processes found"

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
