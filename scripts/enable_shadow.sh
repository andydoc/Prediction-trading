#!/bin/bash
source /home/andydoc/prediction-trader-env/bin/activate
cd /home/andydoc/prediction-trader

python3 << 'PYEOF'
import yaml

with open('config/config.yaml') as f:
    config = yaml.safe_load(f)

# Enable shadow mode - validates everything but doesn't place orders
config['mode'] = 'dual'
config['live_trading']['enabled'] = True
config['live_trading']['shadow_only'] = True

with open('config/config.yaml', 'w') as f:
    yaml.dump(config, f, default_flow_style=False, sort_keys=True)

print(f"Mode: {config['mode']}")
print(f"Live enabled: {config['live_trading']['enabled']}")
print(f"Shadow only: {config['live_trading']['shadow_only']}")
print("System will validate opportunities against live orderbooks but NOT place orders")
PYEOF
