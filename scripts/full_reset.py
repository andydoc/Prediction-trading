import json, shutil
from datetime import datetime
from zoneinfo import ZoneInfo

path = '/home/andydoc/prediction-trader/data/system_state/execution_state.json'
state = json.load(open(path))

# Backup
shutil.copy(path, path + '.pre_reset_backup')

# Show what we're clearing
print(f"Clearing: {len(state.get('open_positions',[]))} open, {len(state.get('closed_positions',[]))} closed")
print(f"Old capital: ${state.get('current_capital', 0):.2f}")

# Full reset
state['current_capital'] = 100.0
state['initial_capital'] = 100.0
state['open_positions'] = []
state['closed_positions'] = []
state['performance'] = {
    'total_pnl': 0.0,
    'win_count': 0,
    'loss_count': 0,
    'total_trades': 0,
    'reset_reason': 'L2 stale pyc bug - all prior positions invalid',
    'reset_time': datetime.now(ZoneInfo('Europe/London')).isoformat()
}

with open(path, 'w') as f:
    json.dump(state, f, indent=2)

print(f"RESET COMPLETE: $100.00, 0 positions")
