#!/bin/bash
cd /home/andydoc/prediction-trader
source ../prediction-trader-env/bin/activate
python -c "
import ast
for f in ['trading_engine.py', 'websocket_manager.py']:
    with open(f) as fh:
        ast.parse(fh.read())
        print(f'  OK: {f}')
from websocket_manager import WebSocketManager, LocalOrderBook, OrderBookLevel
# Test EFP calculation
book = LocalOrderBook(asset_id='test')
book.asks = [
    OrderBookLevel(0.50, 100),  # $50 available
    OrderBookLevel(0.52, 200),  # $104 available
    OrderBookLevel(0.55, 50),   # $27.50 available
]
# Walk $10 → all filled at 0.50 → EFP = 0.50
efp10 = book.effective_fill_price(10.0)
assert abs(efp10 - 0.50) < 0.001, f'EFP $10 should be 0.50, got {efp10}'

# Walk $60 → $50 at 0.50 + $10 at 0.52 → VWAP = 60/(100+19.23) ≈ 0.503
efp60 = book.effective_fill_price(60.0)
assert 0.50 < efp60 < 0.52, f'EFP $60 should be ~0.503, got {efp60}'

# Walk $200 → insufficient ($181.50 total) → 0.0
efp200 = book.effective_fill_price(200.0)
assert efp200 == 0.0, f'EFP $200 should be 0.0 (insufficient), got {efp200}'

print('  EFP tests passed')
print('ALL CHECKS PASSED')
"
