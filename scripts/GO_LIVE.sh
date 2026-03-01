#!/bin/bash
# GO_LIVE.sh - Run this when USDC is deposited and you're ready for real trades
# Usage: wsl bash /mnt/c/Users/andyd/ai-workspace/GO_LIVE.sh

set -e
source /home/andydoc/prediction-trader-env/bin/activate
cd /home/andydoc/prediction-trader

echo "============================================"
echo "  POLYMARKET LIVE TRADING ACTIVATION"
echo "============================================"
echo ""

# Step 1: Check USDC balance
echo "=== Step 1: Checking USDC Balance ==="
BALANCE=$(python3 -c "
import yaml
from py_clob_client.client import ClobClient
from py_clob_client.clob_types import BalanceAllowanceParams, AssetType
with open('config/secrets.yaml') as f:
    s = yaml.safe_load(f)['polymarket']
c = ClobClient(s['host'], key=s['private_key'], chain_id=s['chain_id'],
               signature_type=s['signature_type'], funder=s['funder_address'])
creds = c.create_or_derive_api_creds(); c.set_api_creds(creds)
b = c.get_balance_allowance(BalanceAllowanceParams(asset_type=AssetType.COLLATERAL))
print(f'{float(b[\"balance\"])/1e6:.2f}')
")
echo "  USDC Balance: \$$BALANCE"

# Check minimum
MIN_BALANCE=20
if (( $(echo "$BALANCE < $MIN_BALANCE" | bc -l) )); then
    echo ""
    echo "  ❌ INSUFFICIENT BALANCE (\$$BALANCE < \$$MIN_BALANCE minimum)"
    echo "  Deposit USDC to your funder address before going live."
    echo "  Aborting."
    exit 1
fi
echo "  ✅ Balance sufficient"
echo ""

# Step 2: Health check
echo "=== Step 2: CLOB API Health Check ==="
python3 -c "
import yaml
from live_trading_engine import LiveTradingEngine
from pathlib import Path
with open('config/config.yaml') as f:
    config = yaml.safe_load(f)
engine = LiveTradingEngine(config, Path('.'))
h = engine.health_check()
if h['healthy']:
    print('  ✅ API healthy, server_time=' + str(h['server_time']))
else:
    print('  ❌ API unhealthy: ' + str(h.get('error')))
    exit(1)
"
echo ""

# Step 3: Confirm
echo "=== Step 3: Configuration ==="
echo "  Mode will be set to: DUAL (paper + live)"
echo "  Shadow only: FALSE (REAL orders will be placed)"
echo "  Max capital: \$100"
echo "  Capital per trade: \$10"
echo "  Max positions: 11"
echo ""
echo "  ⚠️  WARNING: This will place REAL orders with REAL money!"
echo ""
read -p "  Type 'YES' to go live: " confirm
if [ "$confirm" != "YES" ]; then
    echo "  Aborted."
    exit 0
fi

# Step 4: Update config
echo ""
echo "=== Step 4: Updating Config ==="
python3 << 'PYEOF'
import yaml
with open('config/config.yaml') as f:
    config = yaml.safe_load(f)
config['mode'] = 'dual'
config['live_trading']['enabled'] = True
config['live_trading']['shadow_only'] = False
with open('config/config.yaml', 'w') as f:
    yaml.dump(config, f, default_flow_style=False, sort_keys=True)
print("  ✅ Config updated: mode=dual, live_trading.enabled=True, shadow_only=False")
PYEOF

# Step 5: Restart system
echo ""
echo "=== Step 5: Restarting System ==="
kill $(ps aux | grep 'main.py\|layer[1-4]_runner\|dashboard_server' | grep -v grep | awk '{print $2}') 2>/dev/null
sleep 3
rm -f *.pid
nohup python main.py > logs/main.log 2>&1 &
echo "  Started PID: $!"
sleep 15

# Step 6: Verify
echo ""
echo "=== Step 6: Verification ==="
L4_MODE=$(grep 'Layer 4 started' logs/layer4_$(date +%Y%m%d).log | tail -1)
echo "  $L4_MODE"
ps aux | grep layer4_runner | grep -v grep | awk '{print "  L4 running as PID: " $2}'
curl -s -o /dev/null -w "  Dashboard: HTTP %{http_code}\n" http://localhost:5556/

echo ""
echo "============================================"
echo "  ✅ LIVE TRADING ACTIVATED"
echo "  Dashboard: http://localhost:5556"
echo "  Monitor L4 log: tail -f logs/layer4_\$(date +%Y%m%d).log"
echo "============================================"
