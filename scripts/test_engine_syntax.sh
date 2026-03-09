#!/bin/bash
cd /home/andydoc/prediction-trader
source ../prediction-trader-env/bin/activate
python -c "
import ast
for f in ['trading_engine.py', 'layer3_arbitrage_math/arbitrage_engine.py']:
    with open(f) as fh:
        ast.parse(fh.read())
        print(f'  syntax OK: {f}')
# Test import chain
from trading_engine import TradingEngine
print('  import OK: TradingEngine')
import rust_arb
print(f'  import OK: rust_arb (HAS_RUST=True)')
print('ALL CHECKS PASSED')
"
