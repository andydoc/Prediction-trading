#!/bin/bash
# kill.sh — Stop all trader processes
#
# Usage:
#   bash kill.sh                # graceful SIGTERM → wait → SIGKILL stragglers
#   bash kill.sh --quiet        # same but no output (used by restart.sh)
#   bash kill.sh --cancel       # also cancel open CLOB orders (live mode)

WORKSPACE="${TRADER_WORKSPACE:-/home/andydoc/prediction-trader}"
QUIET=false
CANCEL=false

for arg in "$@"; do
    case "$arg" in
        --quiet) QUIET=true ;;
        --cancel) CANCEL=true ;;
    esac
done

log() { [ "$QUIET" = false ] && echo "$@"; }

# --- Cancel open CLOB orders if requested ---
if [ "$CANCEL" = true ]; then
    log "[kill] CLOB order cancellation not yet implemented in Rust binary"
fi

# --- SIGTERM (graceful) ---
log "[kill] Sending SIGTERM to all trader processes..."
PIDS=$(ps aux | grep -E 'prediction-trader' | grep -v grep | awk '{print $2}')
if [ -n "$PIDS" ]; then
    log "  PIDs: $PIDS"
    echo "$PIDS" | xargs kill 2>/dev/null || true
    log "[kill] Waiting for graceful shutdown..."
    sleep 5
else
    log "  No trader processes found"
fi

# --- SIGKILL stragglers ---
REMAINING=$(ps aux | grep -E 'prediction-trader' | grep -v grep | awk '{print $2}')
if [ -n "$REMAINING" ]; then
    log "[kill] Force-killing stragglers: $REMAINING"
    echo "$REMAINING" | xargs kill -9 2>/dev/null || true
    sleep 1
fi

# --- Clean PID files ---
rm -f "$WORKSPACE"/*.pid

if [ "$QUIET" = false ]; then
    echo ""
    echo "[kill] Done."
fi
