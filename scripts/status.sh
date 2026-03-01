#!/bin/bash
# Quick status check after restart
echo "=== State ==="
python3 -c "
import json
d = json.load(open('/home/andydoc/prediction-trader/data/system_state/execution_state.json'))
print(f'Cash: \${d[\"current_capital\"]:.2f}')
print(f'Open: {len(d[\"open_positions\"])} positions')
print(f'Closed: {len(d[\"closed_positions\"])} positions')
dep = sum(sum(m.get('bet_amount',0) for m in p.get('markets',{}).values()) for p in d['open_positions'])
print(f'Deployed: \${dep:.2f}')
print(f'Total: \${d[\"current_capital\"]+dep:.2f}')
"
echo "=== L4 log ==="
tail -5 /home/andydoc/prediction-trader/logs/layer4_$(date +%Y%m%d).log 2>/dev/null || echo "no log yet"
