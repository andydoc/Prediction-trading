#!/bin/bash
cd /home/andydoc/prediction-trader
source ../prediction-trader-env/bin/activate
python -c "
import ast
for f in ['main.py', 'websocket_manager.py', 'trading_engine.py']:
    with open(f) as fh:
        ast.parse(fh.read())
        print(f'  syntax OK: {f}')

# Test imports
from websocket_manager import WebSocketManager
from trading_engine import TradingEngine
print('  import OK: all modules')
print('ALL CHECKS PASSED')
"
