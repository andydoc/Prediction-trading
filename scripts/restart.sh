#!/bin/bash
# restart.sh — Stop, pull, rebuild, and restart the trading system
#
# Usage:
#   bash restart.sh                       # default restart
#   bash restart.sh --mode shadow         # restart in shadow mode
#   bash restart.sh --clean               # also purge stale cache data
#   bash restart.sh --set key=value       # pass overrides to supervisor

set -e
WORKSPACE="${TRADER_WORKSPACE:-/home/andydoc/prediction-trader}"
cd "$WORKSPACE"

# Separate --clean from supervisor args
CLEAN=false
SUPERVISOR_ARGS=()
for arg in "$@"; do
    if [ "$arg" = "--clean" ]; then
        CLEAN=true
    else
        SUPERVISOR_ARGS+=("$arg")
    fi
done

echo "[restart] Stopping all trader processes..."
bash "$WORKSPACE/scripts/kill.sh" --quiet 2>/dev/null || true
sleep 3

if [ "$CLEAN" = true ]; then
    echo "[restart] Cleaning stale cache data..."
    rm -f constraint_detection/data/latest_constraints.json
    rm -f arbitrage_math/data/latest_opportunities.json
    echo "  Cleaned"
fi

# Pull + rebuild
echo "[restart] Pulling latest code..."
git pull --ff-only origin main 2>&1 || echo "  WARNING: git pull failed — continuing with local code"

# Rebuild Rust supervisor if source changed
echo "[restart] Rebuilding supervisor..."
cd "$WORKSPACE/rust_supervisor"
cargo build --release 2>&1 | tail -3
cd "$WORKSPACE"

# Rebuild Rust engine
echo "[restart] Rebuilding Rust engine..."
source "$WORKSPACE/.venv/bin/activate" 2>/dev/null || source "$WORKSPACE/../prediction-trader-env/bin/activate"
cd "$WORKSPACE/rust_engine"
maturin develop --release 2>&1 | tail -3
cd "$WORKSPACE"

# Start via start.sh (passes through supervisor args)
echo ""
bash "$WORKSPACE/scripts/start.sh" "${SUPERVISOR_ARGS[@]}"
