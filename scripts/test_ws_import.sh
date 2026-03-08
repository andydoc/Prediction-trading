#!/bin/bash
cd /home/andydoc/prediction-trader
source ../prediction-trader-env/bin/activate
python -c "
import ast, sys
with open('layer4_runner.py') as f:
    try:
        ast.parse(f.read())
        print('OK: layer4_runner.py parses successfully')
    except SyntaxError as e:
        print(f'SYNTAX ERROR: {e}')
        sys.exit(1)
with open('websocket_manager.py') as f:
    try:
        ast.parse(f.read())
        print('OK: websocket_manager.py parses successfully')
    except SyntaxError as e:
        print(f'SYNTAX ERROR: {e}')
        sys.exit(1)
"
