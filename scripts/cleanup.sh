#!/bin/bash
# Kill ALL python trader processes
for pid in $(ps aux | grep "prediction-trader" | grep -v grep | awk '{print $2}'); do
    kill -9 $pid 2>/dev/null
    echo "Killed $pid"
done
for pid in $(ps aux | grep "main.py" | grep -v grep | awk '{print $2}'); do
    kill -9 $pid 2>/dev/null
    echo "Killed main $pid"
done
sleep 2

# Delete opportunities file
rm -f /home/andydoc/prediction-trader/layer3_arbitrage_math/data/latest_opportunities.json
echo "Deleted opportunities"

# Check execution state format
python3 -c "
import json
s = json.load(open('/home/andydoc/prediction-trader/data/system_state/execution_state.json'))
print('Keys:', list(s.keys()))
print('State:', json.dumps(s, indent=2)[:500])
"

echo ""
echo "=== All python processes ==="
ps aux | grep python | grep -v grep
