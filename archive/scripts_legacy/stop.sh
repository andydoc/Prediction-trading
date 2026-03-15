#!/bin/bash
# stop.sh - Kill trading processes
# Usage: bash stop.sh [--dash|--l4]
#   (no args) Kill ALL trader processes
#   --dash   Kill dashboard only (supervisor restarts it)
#   --l4     Kill L4 only (supervisor restarts it)

TARGET="${1:-all}"

case "$TARGET" in
  --dash)
    echo "Killing dashboard..."
    kill $(ps aux | grep dashboard_server | grep -v grep | awk '{print $2}') 2>/dev/null
    echo "Supervisor will restart in ~5s"
    sleep 6
    curl -s -o /dev/null -w "Dashboard: HTTP %{http_code}\n" http://localhost:5556/
    ;;
  --l4)
    echo "Killing Trading Engine..."
    kill $(ps aux | grep trading_engine | grep -v grep | awk '{print $2}') 2>/dev/null
    echo "Supervisor will restart in ~5s"
    ;;
  *)
    echo "Killing ALL trader processes..."
    # SIGTERM first (allows graceful shutdown + state save)
    kill $(ps aux | grep -E 'main\.py|trading_engine|initial_market_scanner' | grep -v grep | awk '{print $2}') 2>/dev/null
    echo "Waiting for graceful shutdown..."
    sleep 5
    # SIGKILL any stragglers
    kill -9 $(ps aux | grep -E 'main\.py|trading_engine|initial_market_scanner' | grep -v grep | awk '{print $2}') 2>/dev/null
    sleep 1
    rm -f /home/andydoc/prediction-trader/*.pid
    echo ""
    echo "=== State snapshot ==="
    python3 -c "
import sys; sys.path.insert(0, '/home/andydoc/prediction-trader')
from utilities.state_db import read_state_from_disk
d = read_state_from_disk('/home/andydoc/prediction-trader/data/system_state/execution_state.db')
if d:
    print(f'Capital: \${d[\"current_capital\"]:.2f}')
    print(f'Open: {len(d.get(\"open_positions\",[]))}')
    print(f'Closed: {len(d.get(\"closed_positions\",[]))}')
else:
    print('(could not read state)')
" 2>/dev/null || echo "(could not read state)"
    echo ""
    echo "=== Remaining python processes ==="
    ps aux | grep python | grep -v grep || echo "None"
    ;;
esac
