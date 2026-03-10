#!/bin/bash
# restart.sh - Start or restart the prediction trading system
# Usage: bash restart.sh [--clean]
#   --clean  Also purge stale L2/L3 data before starting

set -e
source /home/andydoc/prediction-trader-env/bin/activate
cd /home/andydoc/prediction-trader

echo "Restarting Prediction Trader..."

echo "Pulling latest code..."
git pull --ff-only origin main 2>&1 || echo "  WARNING: git pull failed — continuing with local code"

echo "Killing all trader processes..."
PIDS=$(ps aux | grep -E 'main\.py|layer[1-4]_runner|dashboard_server' | grep -v grep | tr -s ' ' | cut -d' ' -f2)
if [ -n "$PIDS" ]; then
    echo "  Killing PIDs: $PIDS"
    echo "$PIDS" | xargs kill 2>/dev/null || true
else
    echo "  No trader processes found"
fi
sleep 3

if [ "$1" = "--clean" ]; then
    echo "Cleaning stale data..."
    rm -f layer2_constraint_detection/data/latest_constraints.json
    rm -f layer3_arbitrage_math/data/latest_opportunities.json
    rm -f layer2_constraint_detection/__pycache__/*.pyc
    echo "  Cleaned L2/L3 cache"
fi

echo "Starting system..."
rm -f *.pid
mkdir -p logs
nohup python main.py >> logs/main.log 2>&1 &
MAIN_PID=$!
disown $MAIN_PID
echo "  Supervisor PID: $MAIN_PID"

echo "Waiting for layers to start..."
sleep 15

echo ""
echo "=== Process check ==="
ps aux | grep -E 'main\.py|layer[1-4]_runner|dashboard_server' | grep -v grep | tr -s ' ' | cut -d' ' -f2,11

echo ""
echo "=== L4 startup log (last 10 lines) ==="
LOGFILE="logs/layer4_$(date +%Y%m%d).log"
if [ -f "$LOGFILE" ]; then
    tail -10 "$LOGFILE"
else
    echo "  L4 log not yet created — try: tail -f $LOGFILE"
fi

echo ""
echo "=== Dashboard ==="
HTTP=$(curl -s -o /dev/null -w "%{http_code}" http://localhost:5556/ 2>/dev/null)
if [ "$HTTP" = "200" ]; then
    echo "  Dashboard OK (HTTP 200) — http://localhost:5556"
else
    echo "  Dashboard NOT responding (HTTP $HTTP)"
fi

echo ""
echo "=== Capital ==="
python3 -c "
import json
try:
    d = json.load(open('data/system_state/execution_state.json'))
    cap = d['current_capital']
    pos = d['open_positions']
    closed = d['closed_positions']
    deployed = sum(p.get('total_capital', 0) for p in pos)
    print(f'  Cash=\${cap:.2f}  Deployed=\${deployed:.2f}  Total=\${cap+deployed:.2f}  Open={len(pos)}  Closed={len(closed)}')
except Exception as e:
    print(f'  Could not read state: {e}')
" 2>/dev/null

echo ""
echo "Done."
