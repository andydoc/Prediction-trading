#!/bin/bash
python3 << 'PYEOF'
import json
from pathlib import Path

s = json.loads(Path('/home/andydoc/prediction-trader/data/system_state/execution_state.json').read_text())

# Top-level capital fields
print("=== TOP-LEVEL CAPITAL ===")
print(f"  initial_capital:  {s.get('initial_capital')}")
print(f"  current_capital:  {s.get('current_capital')}")

# Performance sub-object
perf = s.get('performance', {})
print("\n=== PERFORMANCE BLOCK ===")
for k, v in perf.items():
    if isinstance(v, float):
        print(f"  {k}: {v:.4f}")
    else:
        print(f"  {k}: {v}")

# Position summary
closed = s.get('closed_positions', [])
op = s.get('open_positions', [])
if isinstance(op, dict):
    op = list(op.values())
print(f"\n=== POSITIONS ===")
print(f"  Open:   {len(op)}")
print(f"  Closed: {len(closed)}")

# Capital deployed in open positions
deployed = sum(p.get('total_capital', 0) for p in op if isinstance(p, dict))
print(f"  Capital in open positions: ${deployed:.2f}")

cur = s.get('current_capital', 0)
ic  = s.get('initial_capital', 0)
print(f"\n  Cash on hand:  ${cur:.2f}")
print(f"  Total value (cash + deployed): ${cur + deployed:.2f}")
if ic:
    full_ret = ((cur + deployed) - ic) / ic * 100
    print(f"  Return if incl. open positions: {full_ret:.1f}%")

# What do the closed positions look like in total?
total_actual = sum(p.get('actual_profit', 0) for p in closed if isinstance(p, dict))
total_exp    = sum(p.get('expected_profit', 0) for p in closed if isinstance(p, dict))
print(f"\n=== CLOSED P&L ===")
print(f"  Total expected profit: ${total_exp:.2f}")
print(f"  Total actual profit:   ${total_actual:.2f}")

# All execution state top-level keys
print("\n=== ALL TOP-LEVEL KEYS ===")
for k in s.keys():
    v = s[k]
    if isinstance(v, list):
        print(f"  {k}: list({len(v)})")
    elif isinstance(v, dict):
        print(f"  {k}: dict({list(v.keys())[:5]}...)")
    else:
        print(f"  {k}: {v}")
PYEOF
