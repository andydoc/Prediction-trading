#!/usr/bin/env python3
"""reset.py - Reset trading state
Usage: python3 reset.py [--soft|--hard]
  --soft  Return deployed capital to cash, clear open positions (default)
  --hard  Full wipe: $100 capital, zero positions, zero history
"""
import json, shutil, sys
from datetime import datetime
from zoneinfo import ZoneInfo

path = '/home/andydoc/prediction-trader/data/system_state/execution_state.json'
state = json.load(open(path))
mode = sys.argv[1] if len(sys.argv) > 1 else '--soft'

# Always backup first
backup = path + f'.backup_{datetime.now().strftime("%Y%m%d_%H%M%S")}'
shutil.copy(path, backup)
print(f"Backed up to: {backup}")

if mode == '--hard':
    print(f"HARD RESET: Clearing {len(state.get('open_positions',[]))} open, "
          f"{len(state.get('closed_positions',[]))} closed")
    print(f"Old capital: ${state.get('current_capital', 0):.2f}")
    state['current_capital'] = 100.0
    state['initial_capital'] = 100.0
    state['open_positions'] = []
    state['closed_positions'] = []
    state['performance'] = {
        'total_pnl': 0.0, 'win_count': 0, 'loss_count': 0,
        'total_trades': 0,
        'reset_time': datetime.now(ZoneInfo('Europe/London')).isoformat()
    }
    print("RESET COMPLETE: $100.00, 0 positions")

else:  # --soft
    open_pos = state.get('open_positions', [])
    deployed = sum(
        sum(m.get('bet_amount', 0) for m in p.get('markets', {}).values())
        for p in open_pos
    )
    old_capital = state.get('current_capital', 0)
    new_capital = old_capital + deployed
    print(f"SOFT RESET: Returning ${deployed:.2f} deployed -> cash")
    print(f"  Capital: ${old_capital:.2f} -> ${new_capital:.2f}")
    print(f"  Clearing {len(open_pos)} open positions")
    state['current_capital'] = new_capital
    state['open_positions'] = []

with open(path, 'w') as f:
    json.dump(state, f, indent=2)
print("State saved.")
