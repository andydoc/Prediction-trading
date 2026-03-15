#!/bin/bash
# status.sh - System status check
# Usage: bash status.sh [--full]
#   (no args) Quick capital/positions/L4 summary
#   --full   Full health check with disk, timing, errors, P&L breakdown

source /home/andydoc/prediction-trader-env/bin/activate 2>/dev/null
cd /home/andydoc/prediction-trader 2>/dev/null

echo "=== State ==="
python3 -c "
import json
d = json.load(open('data/system_state/execution_state.json'))
print(f'Cash: \${d[\"current_capital\"]:.2f}')
print(f'Open: {len(d[\"open_positions\"])} positions')
print(f'Closed: {len(d[\"closed_positions\"])} positions')
dep = sum(sum(m.get('bet_amount',0) for m in p.get('markets',{}).values()) for p in d['open_positions'])
print(f'Deployed: \${dep:.2f}')
print(f'Total: \${d[\"current_capital\"]+dep:.2f}')
ic = d.get('initial_capital', 100)
if ic: print(f'Return: {((d[\"current_capital\"]+dep)-ic)/ic*100:.1f}%')
"
echo ""
echo "=== Processes ==="
ps aux | grep -E "main\.py|trading_engine|dashboard_server|initial_market_scanner" | grep -v grep | awk '{printf "%-6s %s\n", $2, $NF}'
echo ""
echo "=== Trading Engine log (last 5) ==="
tail -5 logs/trading_engine_$(date +%Y%m%d).log 2>/dev/null || echo "no log yet"

if [ "$1" != "--full" ]; then
    exit 0
fi

echo ""
echo "=== Dashboard ==="
curl -s -o /dev/null -w "HTTP %{http_code}\n" http://localhost:5556/

echo ""
echo "=== Log sizes today ==="
ls -lh logs/*$(date +%Y%m%d)* 2>/dev/null || echo "no logs"

echo ""
echo "=== Disk usage ==="
du -sh logs/ data/ 2>/dev/null

echo ""
echo "=== JSON file sizes ==="
ls -lh layer1_market_data/data/polymarket/latest.json 2>/dev/null
ls -lh data/system_state/execution_state.json 2>/dev/null

echo ""
echo "=== Trading Engine timing ==="
grep -E "iter.*Capital|p50|latency" logs/trading_engine_$(date +%Y%m%d).log 2>/dev/null | tail -5

echo ""
echo "=== Errors today ==="
for f in logs/main_*.log logs/trading_engine_$(date +%Y%m%d).log logs/dashboard_$(date +%Y%m%d).log; do
    c=$(grep -c "ERROR" "$f" 2>/dev/null || echo 0)
    [ "$c" -gt 0 ] && echo "  $(basename $f): $c errors"
done

echo ""
echo "=== Closed P&L breakdown ==="
python3 -c "
import json
d = json.load(open('data/system_state/execution_state.json'))
closed = d.get('closed_positions', [])
total_actual = sum(p.get('actual_profit', 0) for p in closed)
total_exp = sum(p.get('expected_profit', 0) for p in closed)
perf = d.get('performance', {})
print(f'Total expected profit: \${total_exp:.2f}')
print(f'Total actual profit:   \${total_actual:.2f}')
print(f'Win/Loss: {perf.get(\"win_count\",\"?\")}/{perf.get(\"loss_count\",\"?\")}')
print(f'Total trades: {perf.get(\"total_trades\",\"?\")}')
ic = d.get('initial_capital', 100)
cc = d.get('current_capital', 0)
dep = sum(sum(m.get('bet_amount',0) for m in p.get('markets',{}).values()) for p in d['open_positions'])
print(f'')
print(f'Accounting: init(\${ic}) = cash(\${cc:.2f}) + deployed(\${dep:.2f}) + closed_pnl(\${total_actual:.2f})')
print(f'  Check: {cc:.2f} + {dep:.2f} - {total_actual:.2f} = {cc + dep - total_actual:.2f} (should be ~{ic})')
"
