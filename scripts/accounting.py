import json

state = json.load(open('/home/andydoc/prediction-trader/data/system_state/execution_state.json'))
print(f"Current capital: ${state['current_capital']:.2f}")
print(f"Initial capital: ${state['initial_capital']:.2f}")

deployed = 0
for p in state.get('open_positions', []):
    for m in p.get('markets', {}).values():
        deployed += m.get('bet_amount', 0)

print(f"Deployed: ${deployed:.2f}")
print(f"Total value: ${state['current_capital'] + deployed:.2f}")
print(f"Missing: ${100 - state['current_capital'] - deployed:.2f}")

print(f"\nClosed positions: {len(state.get('closed_positions', []))}")
total_closed_pnl = 0
total_closed_deployed = 0
for p in state.get('closed_positions', []):
    actual = p.get('actual_profit', 0)
    dep = p.get('total_capital', 0)
    reason = p.get('metadata', {}).get('close_reason', '?')
    mkt_vals = list(p.get('markets', {}).values())
    name = mkt_vals[0].get('name', '?')[:50] if mkt_vals else '?'
    total_closed_pnl += actual
    total_closed_deployed += dep
    print(f"  {reason:12s} dep=${dep:.2f} pnl=${actual:+.2f}  {name}")

print(f"\nTotal closed P&L: ${total_closed_pnl:.2f}")
print(f"Total closed deployed: ${total_closed_deployed:.2f}")
print(f"\nAccounting: init({state['initial_capital']}) = cash({state['current_capital']:.2f}) + deployed({deployed:.2f}) + closed_pnl({total_closed_pnl:.2f})")
print(f"  Check: {state['current_capital']:.2f} + {deployed:.2f} - {total_closed_pnl:.2f} = {state['current_capital'] + deployed - total_closed_pnl:.2f} (should be ~{state['initial_capital']})")
