#!/bin/bash
cd /home/andydoc/prediction-trader
source ../prediction-trader-env/bin/activate
python -c "
import time, json, rust_arb
from market_data.market_data import MarketData
from datetime import datetime, timezone

# Simulate realistic constraint eval overhead
markets_data = json.loads(open('data/latest_markets.json').read())
markets = [MarketData.from_dict(m) for m in markets_data.get('markets', [])[:100]]
market_lookup = {str(m.market_id): m for m in markets}

# Simulate what _evaluate_constraint does
def fake_eval(market_ids):
    # has_live_prices check
    for mid in market_ids:
        md = market_lookup.get(str(mid))
        if not md:
            return None
    # build markets list
    mlist = [market_lookup[str(mid)] for mid in market_ids if str(mid) in market_lookup]
    if len(mlist) < 2:
        return None
    # build price arrays
    yes_p = [m.get_entry_price('Yes') for m in mlist]
    no_p = [m.get_entry_price('No') for m in mlist]
    mids = [str(m.market_id) for m in mlist]
    # rust call
    result = rust_arb.check_mutex_arb(mids, yes_p, no_p, 10.0, 0.0001, 0.03, 0.30, True)
    return result

# Get some real market_ids
all_mids = list(market_lookup.keys())
groups = [all_mids[i:i+3] for i in range(0, min(90, len(all_mids)), 3)]

N = 10000
t0 = time.perf_counter()
for _ in range(N):
    for g in groups[:30]:  # 30 groups per iteration
        fake_eval(g)
elapsed = time.perf_counter() - t0
total_evals = N * 30
per_eval_us = elapsed / total_evals * 1e6
print(f'{total_evals} evals in {elapsed:.3f}s = {per_eval_us:.1f}us/eval ({per_eval_us/1000:.2f}ms/eval)')
print(f'100 evals would take: {per_eval_us * 100 / 1000:.1f}ms')
"
