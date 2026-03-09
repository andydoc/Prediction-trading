#!/bin/bash
cd /home/andydoc/prediction-trader
source ../prediction-trader-env/bin/activate
python -c "
import ast
for f in ['live_trading.py', 'layer3_arbitrage_math/arbitrage_engine.py']:
    with open(f) as fh:
        ast.parse(fh.read())
        print(f'  OK: {f}')
print('ALL SYNTAX VALID')
"
