#!/bin/bash
cd /home/andydoc/prediction-trader
source ../prediction-trader-env/bin/activate
python -c "
import ast
for f in ['websocket_manager.py', 'trading_engine.py', 'layer1_market_data/market_data.py', 'layer3_arbitrage_math/arbitrage_engine.py']:
    with open(f) as fh:
        ast.parse(fh.read())
        print(f'  syntax OK: {f}')
from trading_engine import TradingEngine
print('  import OK: TradingEngine')
print('ALL CHECKS PASSED')
"
