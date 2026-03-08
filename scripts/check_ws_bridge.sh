#!/bin/bash
sleep 60
echo "=== L4 WS status ==="
grep 'WS' /home/andydoc/prediction-trader/logs/layer4_20260308.log | grep -E 'msgs=|bridge' | tail -5
echo "=== L3 WS overlay ==="
grep 'WS prices' /home/andydoc/prediction-trader/logs/layer3_20260308.log | tail -5
echo "=== ws_prices.json ==="
cd /home/andydoc/prediction-trader
python3 -c "
import json, time
try:
    d = json.load(open('data/ws_prices.json'))
    age = time.time() - d.get('exported_at', 0)
    print(f'count={d.get(\"count\",0)} age={age:.1f}s')
    # Show a sample
    prices = d.get('prices', {})
    for mid, p in list(prices.items())[:3]:
        print(f'  market {mid}: Yes={p.get(\"Yes\",\"?\")} ts_age={time.time()-p.get(\"ts\",0):.1f}s')
except FileNotFoundError:
    print('ws_prices.json not found yet')
except Exception as e:
    print(f'Error: {e}')
"
