#!/bin/bash
cd /home/andydoc/prediction-trader
source ../prediction-trader-env/bin/activate
python -c "
import ast, sys
for f in ['layer3_runner.py', 'layer4_runner.py', 'websocket_manager.py']:
    with open(f) as fh:
        try:
            ast.parse(fh.read())
            print(f'OK: {f}')
        except SyntaxError as e:
            print(f'SYNTAX ERROR in {f}: {e}')
            sys.exit(1)
print('All files parse OK')
"
