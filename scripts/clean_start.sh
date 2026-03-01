#!/bin/bash
# Clean stale data
rm -f /home/andydoc/prediction-trader/layer2_constraint_detection/data/latest_constraints.json
rm -f /home/andydoc/prediction-trader/layer3_arbitrage_math/data/latest_opportunities.json
rm -f /home/andydoc/prediction-trader/layer2_constraint_detection/__pycache__/*.pyc
echo "Cleaned stale files"

# Start fresh
cd /home/andydoc/prediction-trader
nohup python3 main.py > /tmp/trader_start.log 2>&1 &
MAIN_PID=$!
echo "Started main.py PID=$MAIN_PID"

# Wait for L2 to complete first cycle
echo "Waiting 30s for first L2 cycle..."
sleep 30

# Check what L2 produced
echo ""
echo "=== L2 status ==="
cat /home/andydoc/prediction-trader/data/layer2_status.json 2>/dev/null

echo ""
echo "=== Constraints file ==="
if [ -f /home/andydoc/prediction-trader/layer2_constraint_detection/data/latest_constraints.json ]; then
    python3 -c "
import json
c = json.load(open('/home/andydoc/prediction-trader/layer2_constraint_detection/data/latest_constraints.json'))
constraints = c.get('constraints', [])
methods = {}
for x in constraints:
    m = x.get('metadata', {}).get('detection_method', '?')
    methods[m] = methods.get(m, 0) + 1
print(f'Total: {len(constraints)} constraints')
print(f'Methods: {methods}')
# Show first 2
for x in constraints[:2]:
    names = x.get('market_names', [])
    print(f'  Group ({len(names)} mkts): {names[0][:60]}...')
"
else
    echo "No constraints file yet"
fi
