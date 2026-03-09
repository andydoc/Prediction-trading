#!/bin/bash
cd /home/andydoc/prediction-trader
source ../prediction-trader-env/bin/activate
python -c "
import time
import rust_arb

# Test 1: Buy arb (sum=0.90)
r = rust_arb.check_mutex_arb(
    ['m1','m2','m3'], [0.30,0.30,0.30], [0.70,0.70,0.70],
    100.0, 0.0001, 0.03, 0.30, True)
assert r is not None and r['method'] == 'mutex_buy_all'
print(f'  Buy arb sum=0.90: {r[\"profit_pct\"]:.4f} OK')

# Test 2: Sell arb (sum=1.15, wider spread)
r2 = rust_arb.check_mutex_arb(
    ['m1','m2','m3'], [0.45,0.40,0.30], [0.52,0.57,0.67],
    100.0, 0.0001, 0.03, 0.30, False)
assert r2 is not None and r2['method'] == 'mutex_sell_all', f'Sell arb failed: {r2}'
print(f'  Sell arb sum=1.15: {r2[\"profit_pct\"]:.4f} OK')

# Test 3: negRisk sell arb (check cap efficiency)
r3 = rust_arb.check_mutex_arb(
    ['m1','m2','m3'], [0.45,0.40,0.30], [0.52,0.57,0.67],
    100.0, 0.0001, 0.03, 0.30, True)
assert r3 is not None
assert r3['neg_risk'] == True
assert r3['capital_efficiency'] > 1.0
print(f'  negRisk sell: cap_eff={r3[\"capital_efficiency\"]:.2f}x OK')

# Test 4: No arb
r4 = rust_arb.check_mutex_arb(
    ['m1','m2'], [0.50,0.50], [0.50,0.50],
    100.0, 0.0001, 0.03, 0.30, False)
assert r4 is None
print(f'  No arb: None OK')

# Test 5: EFP
efp = rust_arb.effective_fill_price([0.50,0.52,0.55], [100.0,200.0,50.0], 10.0)
assert abs(efp - 0.50) < 0.001
print(f'  EFP: {efp:.6f} OK')

# Benchmarks
N = 100000
t0 = time.perf_counter()
for _ in range(N):
    rust_arb.check_mutex_arb(
        ['m1','m2','m3','m4'], [0.25,0.25,0.25,0.24],
        [0.75,0.75,0.75,0.76], 100.0, 0.0001, 0.03, 0.30, True)
el = time.perf_counter() - t0
print(f'  check_mutex_arb: {N} calls in {el:.3f}s = {el/N*1e6:.1f}us/call')

t0 = time.perf_counter()
for _ in range(N):
    rust_arb.effective_fill_price([0.50,0.52,0.55,0.60,0.65], [100,200,50,30,10], 10.0)
el = time.perf_counter() - t0
print(f'  effective_fill_price: {N} calls in {el:.3f}s = {el/N*1e6:.1f}us/call')

print('ALL TESTS PASSED')
"
