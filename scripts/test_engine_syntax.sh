#!/bin/bash
cd /home/andydoc/prediction-trader
source ../prediction-trader-env/bin/activate
python -c "
import ast
with open('trading_engine.py') as f:
    ast.parse(f.read())
print('OK: trading_engine.py syntax valid')
"
