#!/bin/bash
# mode.sh - Switch trading mode
# Usage: bash mode.sh paper|shadow|live|stop

source /home/andydoc/prediction-trader-env/bin/activate
cd /home/andydoc/prediction-trader

MODE="${1:-}"
if [ -z "$MODE" ]; then
    echo "Usage: $0 paper|shadow|live|stop"
    echo "  paper  - Paper trading only"
    echo "  shadow - Validate against live orderbooks, no real orders"
    echo "  live   - Real orders (with pre-flight checks + confirmation)"
    echo "  stop   - Emergency revert to paper + cancel open CLOB orders"
    exit 1
fi

echo "=========================================="
echo "  PREDICTION TRADER - MODE: $MODE"
echo "=========================================="

case "$MODE" in
  paper)
    echo "Switching to PAPER ONLY mode..."
    python3 -c "
import yaml
with open('config/config.yaml') as f:
    c = yaml.safe_load(f)
c['mode'] = 'paper_trading'
c['live_trading']['enabled'] = False
c['live_trading']['shadow_only'] = False
with open('config/config.yaml','w') as f:
    yaml.dump(c, f, default_flow_style=False, sort_keys=True)
print('Config updated: mode=paper_trading, live=disabled')
"
    ;;

  shadow)
    echo "Switching to SHADOW mode..."
    python3 -c "
import yaml
with open('config/config.yaml') as f:
    c = yaml.safe_load(f)
c['mode'] = 'dual'
c['live_trading']['enabled'] = True
c['live_trading']['shadow_only'] = True
with open('config/config.yaml','w') as f:
    yaml.dump(c, f, default_flow_style=False, sort_keys=True)
print('Config updated: mode=dual, shadow_only=true')
"
    ;;

  live)
    echo "Pre-flight checks for LIVE trading..."
    python3 << 'PYEOF'
import yaml, sys
from live_trading_engine import LiveTradingEngine
from py_clob_client.client import ClobClient
from py_clob_client.clob_types import BalanceAllowanceParams, AssetType

with open('config/config.yaml') as f:
    config = yaml.safe_load(f)

# Check connectivity + balance
try:
    engine = LiveTradingEngine(config, __import__('pathlib').Path('.'))
    health = engine.health_check()
    if not health['healthy']:
        print(f"❌ CLOB connection FAILED: {health.get('error')}")
        sys.exit(1)
    balance = health['balance_usd']
    print(f"✅ CLOB connected, server time: {health['server_time']}")
    print(f"✅ USDC balance: ${balance:.2f}")
    if balance < 20:
        print(f"⚠️  WARNING: Balance below $20 - live trading will pause")
except Exception as e:
    print(f"❌ Live engine init FAILED: {e}")
    sys.exit(1)
PYEOF
    if [ $? -ne 0 ]; then
        echo "❌ PRE-FLIGHT FAILED - config NOT changed"
        exit 1
    fi
    echo ""
    echo "⚠️  WARNING: This will place REAL orders with REAL money!"
    read -p "Type 'YES' to go live: " confirm
    if [ "$confirm" != "YES" ]; then
        echo "Aborted."
        exit 0
    fi
    python3 -c "
import yaml
with open('config/config.yaml') as f:
    c = yaml.safe_load(f)
c['mode'] = 'dual'
c['live_trading']['enabled'] = True
c['live_trading']['shadow_only'] = False
with open('config/config.yaml','w') as f:
    yaml.dump(c, f, default_flow_style=False, sort_keys=True)
print('Config updated: mode=dual, live=enabled, shadow_only=false')
"
    ;;

  stop)
    echo "EMERGENCY: Reverting to PAPER-ONLY mode..."
    python3 -c "
import yaml
with open('config/config.yaml') as f:
    c = yaml.safe_load(f)
c['mode'] = 'paper_trading'
c['live_trading']['enabled'] = False
c['live_trading']['shadow_only'] = False
with open('config/config.yaml','w') as f:
    yaml.dump(c, f, default_flow_style=False, sort_keys=True)
print('Config updated: mode=paper_trading, live disabled')
"
    echo "Cancelling open CLOB orders..."
    python3 -c "
import yaml
from live_trading_engine import LiveTradingEngine
from pathlib import Path
with open('config/config.yaml') as f:
    config = yaml.safe_load(f)
config['live_trading']['enabled'] = True
engine = LiveTradingEngine(config, Path('.'))
cancelled = engine.cancel_all_orders()
print(f'Cancelled {cancelled} orders')
" 2>/dev/null || echo "  (no orders to cancel)"
    ;;

  *)
    echo "Unknown mode: $MODE"
    echo "Usage: $0 paper|shadow|live|stop"
    exit 1
    ;;
esac

# Restart L4 + dashboard to pick up changes
echo ""
echo "Restarting L4 and dashboard..."
L4_PID=$(ps aux | grep 'layer4_runner' | grep -v grep | awk '{print $2}')
[ -n "$L4_PID" ] && kill $L4_PID 2>/dev/null && echo "  Killed L4 (pid $L4_PID)"
DASH_PID=$(ps aux | grep 'dashboard_server' | grep -v grep | awk '{print $2}')
[ -n "$DASH_PID" ] && kill $DASH_PID 2>/dev/null && echo "  Killed dashboard"
sleep 6

echo ""
echo "Verifying..."
tail -5 /home/andydoc/prediction-trader/logs/layer4_$(date +%Y%m%d).log 2>/dev/null
curl -s -o /dev/null -w "Dashboard: HTTP %{http_code}\n" http://localhost:5556/
echo "=========================================="
echo "  DONE"
echo "=========================================="
