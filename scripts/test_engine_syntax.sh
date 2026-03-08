#!/bin/bash
cd /home/andydoc/prediction-trader
source ../prediction-trader-env/bin/activate
echo "=== Syntax check ==="
python -c "
import ast
files = [
    'main.py', 'trading_engine.py', 'websocket_manager.py',
    'layer1_market_data/market_data.py',
    'layer3_arbitrage_math/arbitrage_engine.py',
    'paper_trading.py', 'live_trading.py',
]
for f in files:
    with open(f) as fh:
        try:
            ast.parse(fh.read())
            print(f'  OK: {f}')
        except SyntaxError as e:
            print(f'  FAIL: {f}: {e}')
            import sys; sys.exit(1)
"
echo "=== Import check ==="
python -c "
from trading_engine import TradingEngine
from websocket_manager import WebSocketManager
from layer1_market_data.market_data import MarketData
from layer3_arbitrage_math.arbitrage_engine import ArbitrageMathEngine
print('  All imports OK')
"
echo "=== Done ==="
