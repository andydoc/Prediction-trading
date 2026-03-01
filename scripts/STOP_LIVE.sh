#!/bin/bash
# STOP_LIVE.sh - Emergency: revert to paper-only trading
# Does NOT close existing live positions - just prevents NEW live orders
source /home/andydoc/prediction-trader-env/bin/activate
cd /home/andydoc/prediction-trader

echo "=== Reverting to PAPER-ONLY mode ==="
python3 << 'PYEOF'
import yaml
with open('config/config.yaml') as f:
    config = yaml.safe_load(f)
config['mode'] = 'paper_trading'
config['live_trading']['enabled'] = False
config['live_trading']['shadow_only'] = False
with open('config/config.yaml', 'w') as f:
    yaml.dump(config, f, default_flow_style=False, sort_keys=True)
print("Config updated: mode=paper_trading, live disabled")
PYEOF

# Cancel any open orders on CLOB
echo "Cancelling open CLOB orders..."
python3 -c "
import yaml
from live_trading_engine import LiveTradingEngine
from pathlib import Path
with open('config/config.yaml') as f:
    config = yaml.safe_load(f)
config['live_trading']['enabled'] = True  # Temp enable to init client
engine = LiveTradingEngine(config, Path('.'))
cancelled = engine.cancel_all_orders()
print(f'Cancelled {cancelled} orders')
" 2>/dev/null || echo "  (no orders to cancel)"

# Restart
echo "Restarting in paper mode..."
kill $(ps aux | grep 'main.py\|layer[1-4]_runner\|dashboard_server' | grep -v grep | awk '{print $2}') 2>/dev/null
sleep 3
rm -f *.pid
nohup python main.py > logs/main.log 2>&1 &
sleep 12
echo "=== Restarted in PAPER mode ==="
grep 'Layer 4 started' logs/layer4_$(date +%Y%m%d).log | tail -1
