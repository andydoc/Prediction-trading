import json
from datetime import datetime
from zoneinfo import ZoneInfo

state_path = '/home/andydoc/prediction-trader/data/system_state/execution_state.json'
state = json.load(open(state_path))

old_balance = state.get('balance', 0)
old_positions = state.get('open_positions', [])
old_deployed = sum(sum(m.get('bet_amount', 0) for m in p.get('markets', {}).values()) for p in old_positions)

print(f"Old state: balance=${old_balance}, positions={len(old_positions)}, deployed=${old_deployed}")

# Reset: return all deployed capital to balance
new_balance = old_balance + old_deployed
print(f"Resetting: ${old_deployed} returned to balance -> ${new_balance}")

state['balance'] = new_balance
state['open_positions'] = []
state['last_updated'] = datetime.now(ZoneInfo('Europe/London')).isoformat()

# Backup old state
import shutil
shutil.copy(state_path, state_path + '.broken_positions_backup')

with open(state_path, 'w') as f:
    json.dump(state, f, indent=2)

print(f"State reset. New balance: ${new_balance}, positions: 0")
