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
    log "[kill] Cancelling open CLOB orders..."
    python3 -c "
import yaml
from live_trading_engine import LiveTradingEngine
from pathlib import Path
with open('$WORKSPACE/config/config.yaml') as f:
    config = yaml.safe_load(f)
config['live_trading']['enabled'] = True
engine = LiveTradingEngine(config, Path('$WORKSPACE'))
cancelled = engine.cancel_all_orders()
print(f'Cancelled {cancelled} orders')
" 2>/dev/null || log "  (no orders to cancel or engine unavailable)"
fi

# --- SIGTERM (graceful) ---
log "[kill] Sending SIGTERM to all trader processes..."
PIDS=$(ps aux | grep -E 'prediction-trader|main\.py|trading_engine|initial_market_scanner' | grep -v grep | awk '{print $2}')
if [ -n "$PIDS" ]; then
    log "  PIDs: $PIDS"
    echo "$PIDS" | xargs kill 2>/dev/null || true
    log "[kill] Waiting for graceful shutdown..."
    sleep 5
else
    log "  No trader processes found"
fi

# --- SIGKILL stragglers ---
REMAINING=$(ps aux | grep -E 'prediction-trader|main\.py|trading_engine|initial_market_scanner' | grep -v grep | awk '{print $2}')
if [ -n "$REMAINING" ]; then
    log "[kill] Force-killing stragglers: $REMAINING"
    echo "$REMAINING" | xargs kill -9 2>/dev/null || true
    sleep 1
fi

# --- Clean PID files ---
rm -f "$WORKSPACE"/*.pid

# --- State snapshot ---
if [ "$QUIET" = false ]; then
    echo ""
    echo "=== State snapshot ==="
    python3 -c "
import sys; sys.path.insert(0, '$WORKSPACE')
from utilities.state_db import read_state_from_disk
d = read_state_from_disk('$WORKSPACE/data/system_state/execution_state.db')
if d:
    cap = d['current_capital']
    pos = d.get('open_positions', [])
    closed = d.get('closed_positions', [])
    deployed = sum(p.get('total_capital', 0) for p in pos)
    print(f'  Cash=\${cap:.2f}  Deployed=\${deployed:.2f}  Total=\${cap+deployed:.2f}  Open={len(pos)}  Closed={len(closed)}')
else:
    print('  (could not read state)')
" 2>/dev/null || echo "  (could not read state)"

    echo ""
    echo "=== Remaining python processes ==="
    ps aux | grep python | grep -v grep || echo "  None"
    echo ""
    echo "[kill] Done."
fi
