#!/bin/bash
echo "=== EMERGENCY: Kill ALL L4 instances ==="
pids=$(ps aux | grep layer4_runner | grep -v grep | awk '{print $2}')
for p in $pids; do
    kill -9 $p 2>/dev/null && echo "Killed L4 PID $p"
done

echo ""
echo "=== Kill old dashboard on 5555 ==="
pids5555=$(ps aux | grep "python.*main.py\|5555" | grep -v grep | awk '{print $2}')
for p in $pids5555; do
    kill -9 $p 2>/dev/null && echo "Killed PID $p (5555/main)"
done

echo ""
echo "=== Kill any dashboard_server ==="
pids_dash=$(ps aux | grep dashboard_server | grep -v grep | awk '{print $2}')
for p in $pids_dash; do
    kill $p 2>/dev/null && echo "Killed dashboard PID $p"
done

echo ""
echo "=== Current state snapshot ==="
python3 -c "
import json
d = json.load(open('/home/andydoc/prediction-trader/data/system_state/execution_state.json'))
print(f'Capital: \${d[\"current_capital\"]:.2f}')
print(f'Open: {len(d.get(\"open_positions\",[]))}')
print(f'Closed: {len(d.get(\"closed_positions\",[]))}')
print()
print('OPEN:')
for p in d.get('open_positions',[]):
    m = list(p.get('markets',{}).values())
    name = m[0]['name'][:55] if m else '?'
    exp = p.get('expected_profit',0)
    print(f'  \${p[\"total_capital\"]:.2f} | exp \${exp:.2f} | {name}')
print()
print('CLOSED:')
for p in d.get('closed_positions',[]):
    m = list(p.get('markets',{}).values())
    name = m[0]['name'][:55] if m else '?'
    pnl = p.get('actual_profit',0)
    reason = p.get('metadata',{}).get('close_reason','?')
    print(f'  P&L \${pnl:+.2f} | {reason} | {name}')
"

echo ""
echo "=== All python processes ==="
ps aux | grep python | grep -v grep
