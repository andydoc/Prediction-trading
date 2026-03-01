#!/bin/bash
echo "=== System health check ==="
echo "--- Processes ---"
ps aux | grep -E "main.py|layer[1-4]|dashboard" | grep -v grep | awk '{printf "%-6s %s\n", $2, $11}'

echo ""
echo "--- Log sizes today ---"
ls -lh /home/andydoc/prediction-trader/logs/*$(date +%Y%m%d)* 2>/dev/null

echo ""
echo "--- Execution state ---"
python3 -c "
import json
d = json.load(open('/home/andydoc/prediction-trader/data/system_state/execution_state.json'))
print(f'Capital: \${d[\"current_capital\"]:.2f}')
print(f'Open: {len(d[\"open_positions\"])}')
print(f'Closed: {len(d[\"closed_positions\"])}')
"

echo ""
echo "--- Disk usage ---"
du -sh /home/andydoc/prediction-trader/logs/
du -sh /home/andydoc/prediction-trader/data/

echo ""
echo "--- JSON file sizes ---"
ls -lh /home/andydoc/prediction-trader/data/latest_markets.json
ls -lh /home/andydoc/prediction-trader/layer2_constraint_detection/data/latest_constraints.json
ls -lh /home/andydoc/prediction-trader/layer3_arbitrage_math/data/latest_opportunities.json
ls -lh /home/andydoc/prediction-trader/data/system_state/execution_state.json

echo ""
echo "--- Scan timing ---"
grep "SCAN COMPLETE" /home/andydoc/prediction-trader/logs/layer3_$(date +%Y%m%d).log | tail -5

echo ""
echo "--- L4 timing ---"
grep "iter.*Capital" /home/andydoc/prediction-trader/logs/layer4_$(date +%Y%m%d).log | tail -3

echo ""
echo "--- Any errors in last hour? ---"
grep -c "ERROR" /home/andydoc/prediction-trader/logs/layer*$(date +%Y%m%d).log
