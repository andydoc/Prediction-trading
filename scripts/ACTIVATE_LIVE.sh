#!/bin/bash
# ACTIVATE_LIVE.sh - Switch from paper to dual (paper+live) trading
# Run this when USDC is deposited and you're ready to go live
#
# Usage: wsl bash /mnt/c/Users/andyd/ai-workspace/ACTIVATE_LIVE.sh
#   or:  wsl bash /mnt/c/Users/andyd/ai-workspace/ACTIVATE_LIVE.sh shadow
#   or:  wsl bash /mnt/c/Users/andyd/ai-workspace/ACTIVATE_LIVE.sh off

source /home/andydoc/prediction-trader-env/bin/activate
cd /home/andydoc/prediction-trader

MODE="${1:-dual}"

echo "=========================================="
echo "  PREDICTION TRADER - MODE SWITCH"
echo "=========================================="

if [ "$MODE" = "off" ] || [ "$MODE" = "paper" ]; then
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
elif [ "$MODE" = "shadow" ]; then
    echo "Switching to SHADOW mode (validate but don't execute)..."
    python3 -c "
import yaml
with open('config/config.yaml') as f:
    c = yaml.safe_load(f)
c['mode'] = 'dual'
c['live_trading']['enabled'] = True
c['live_trading']['shadow_only'] = True
with open('config/config.yaml','w') as f:
    yaml.dump(c, f, default_flow_style=False, sort_keys=True)
print('Config updated: mode=dual, shadow=true')
"
elif [ "$MODE" = "dual" ] || [ "$MODE" = "live" ]; then
    echo "Switching to DUAL mode (paper + LIVE execution)..."
    echo ""
    # Pre-flight check
    echo "Running pre-flight checks..."
    python3 << 'PYEOF'
import yaml, sys
from live_trading_engine import LiveTradingEngine

with open('config/config.yaml') as f:
    config = yaml.safe_load(f)

# Test connectivity
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
        print(f"⚠️  WARNING: Balance is below $20 pause threshold")
        print(f"   Live trading will pause until more USDC is deposited")
    else:
        print(f"✅ Balance sufficient for trading")
except Exception as e:
    print(f"❌ Live engine init FAILED: {e}")
    sys.exit(1)

# Update config
config['mode'] = 'dual'
config['live_trading']['enabled'] = True
config['live_trading']['shadow_only'] = False
with open('config/config.yaml', 'w') as f:
    yaml.dump(config, f, default_flow_style=False, sort_keys=True)
print(f"\n✅ Config updated: mode=dual, live=enabled")
PYEOF
    if [ $? -ne 0 ]; then
        echo ""
        echo "❌ PRE-FLIGHT FAILED - config NOT changed"
        exit 1
    fi
else
    echo "Usage: $0 [dual|shadow|off|paper]"
    exit 1
fi

echo ""
echo "Restarting Layer 4 to pick up changes..."
# Kill L4, supervisor will restart it
L4_PID=$(ps aux | grep 'layer4_runner' | grep -v grep | awk '{print $2}')
if [ -n "$L4_PID" ]; then
    kill $L4_PID 2>/dev/null
    echo "Killed L4 (pid $L4_PID), supervisor will restart in ~5s"
    sleep 6
fi

# Also restart dashboard for badge update
DASH_PID=$(ps aux | grep 'dashboard_server' | grep -v grep | awk '{print $2}')
if [ -n "$DASH_PID" ]; then
    kill $DASH_PID 2>/dev/null
    echo "Killed dashboard (pid $DASH_PID), supervisor will restart"
    sleep 4
fi

# Verify
echo ""
echo "Verifying..."
tail -5 /home/andydoc/prediction-trader/logs/layer4_$(date +%Y%m%d).log
echo ""
HTTP=$(curl -s -o /dev/null -w "%{http_code}" http://localhost:5556/)
echo "Dashboard: HTTP $HTTP"

echo ""
echo "=========================================="
echo "  DONE"
echo "=========================================="
