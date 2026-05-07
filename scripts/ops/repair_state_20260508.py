#!/usr/bin/env python3
"""
Repair DB state after partial-fill incident (2026-05-08).

Changes:
1. Fix Crunchyroll arb position: populate token_ids for 3 filled legs,
   set actual_shares, set fill_status=accepted/cancelled appropriately.
2. Add position record for El Hodood Draw NO (trade-2, from journal).
3. Fix accounting checkpoint: cash=on-chain USDC ($80.16),
   deployed=sum of cost bases for tracked open positions,
   holdings populated with correct token_ids.
4. Fix scalars: current_capital=$97.37 (total portfolio value unchanged).
"""
import sqlite3, json, time

DB = '/home/ubuntu/prediction-trader/data/state_rust.db'
ON_CHAIN_USDC = 80.16

# ---------------------------------------------------------------------------
# Position data from Polymarket Data API / journal
# ---------------------------------------------------------------------------

CRUNCHYROLL_PID = 'paper_1778190634.257736_mutex_0xf7677b65d7bc3187034c89f4dc4a3b'
CRUNCHYROLL_LEG_FILLS = {
    # market_id: (token_id, actual_shares, fill_status)
    '1834533': ('82724943425600151029939281806822319359475765380323844283886329142061964392665', 4.1222, 'accepted'),
    '1834532': ('50044145130319219674999172614005136782999664814951249895410854425222125982484', 4.1222, 'accepted'),
    '1834530': ('82389438372188201312360823731909997627694063888783766735886884183469631603599', 4.1176, 'accepted'),
    '1834531': ('', 0.0, 'cancelled'),   # Suganuma -- CLOB accepted but 0 fills
    '1834529': ('', 0.0, 'cancelled'),   # Fudesaka  -- CLOB accepted but 0 fills
    '1834534': ('', 0.0, 'rejected'),    # Iino      -- rejected for min size (already correct)
}

CRUNCHYROLL_COSTS = {
    '1834533': 3.705189941867185,
    '1834532': 3.705189941867185,
    '1834530': 3.4993460562078957,
    '1834531': 3.581683610471612,
    '1834529': 4.034540158922045,
    '1834534': 0.9468818740327247,
}
CRUNCHYROLL_FILLED_COST = sum(CRUNCHYROLL_COSTS[mid] for mid in ['1834533','1834532','1834530'])

# El Hodood (trade 2) -- reconstructed from journal
HODOOD_PID       = 'paper_1778164484.3062873_mutex_0xce9f2b128e0b4d1d7b921e24a566cb'
HODOOD_TOKEN_ID  = '52318778167875974510372552758260515387718779455072251133050352396429657843717'
HODOOD_SHARES    = 7.2
HODOOD_BET       = 7.302603945000002   # from journal (leg 1 of trade 2)
HODOOD_MARKET    = 'hodood_draw_no'    # synthetic market key (no Gamma market_id available)
HODOOD_COND      = '0xdb2cae6b3169273e9358fccd78061b7274fe45f9fa28bf6104d3244730dc15eb'

conn = sqlite3.connect(DB)

# ---------------------------------------------------------------------------
# 1. Fix Crunchyroll position
# ---------------------------------------------------------------------------
row = conn.execute("SELECT data FROM positions WHERE position_id=?", (CRUNCHYROLL_PID,)).fetchone()
pos = json.loads(row[0])

for mid, (tok, shares, status) in CRUNCHYROLL_LEG_FILLS.items():
    if mid in pos['markets']:
        leg = pos['markets'][mid]
        if tok:
            leg['token_id'] = tok
        leg['actual_shares'] = shares
        leg['fill_status'] = status
        if shares > 0:
            leg['shares'] = shares

pos['status'] = 'open'

conn.execute("UPDATE positions SET data=?, status='open' WHERE position_id=?",
             (json.dumps(pos), CRUNCHYROLL_PID))
print(f'[1] Fixed Crunchyroll position: token_ids + actual_shares set on 3 filled legs')
print(f'    Suganuma/Fudesaka => cancelled, Iino => rejected')

# ---------------------------------------------------------------------------
# 2. Add (or update) El Hodood position record
# ---------------------------------------------------------------------------
hodood_pos = {
    'position_id': HODOOD_PID,
    'opportunity_id': 'mutex_0xce9f2b128e0b4d1d7b921e24a566cb',
    'markets': {
        HODOOD_MARKET: {
            'name': 'Will Haras El Hodood SC vs. ZED FC end in a draw?',
            'entry_price': 0.14,
            'bet_amount': HODOOD_BET,
            'outcome': 'no',
            'shares': HODOOD_SHARES,
            'token_id': HODOOD_TOKEN_ID,
            'intended_shares': HODOOD_SHARES,
            'actual_shares': HODOOD_SHARES,
            'fill_status': 'accepted',
        }
    },
    'total_capital': HODOOD_BET,
    'expected_profit': 0.0,
    'expected_profit_pct': 0.0,
    'fees_paid': round(HODOOD_BET * 0.0001, 8),
    'entry_timestamp': '1778164484.306287',
    'entry_prices': {HODOOD_MARKET: 0.14},
    'status': 'open',
    'last_check': '1778164484.306287',
    'price_drift': {},
    'resolved_at': None,
    'close_timestamp': None,
    'winning_market': None,
    'actual_payout': 0.0,
    'actual_profit': 0.0,
    'actual_profit_pct': 0.0,
    'profit_delta': 0.0,
    'profit_accuracy': 0.0,
    'metadata': {
        'constraint_id': 'mutex_0xce9f2b128e0b4d1d7b921e24a566cb',
        'strategy': 'arb_sell',
        'is_sell': True,
        'end_date_ts': 0.0,
        'note': 'Reconstructed 2026-05-08 after partial reconciliation incident',
        'condition_id': HODOOD_COND,
    },
    'chain_id': None,
    'chain_generation': 0,
    'parent_position_id': None,
}

existing = conn.execute("SELECT 1 FROM positions WHERE position_id=?", (HODOOD_PID,)).fetchone()
if existing:
    conn.execute("UPDATE positions SET data=?, status='open' WHERE position_id=?",
                 (json.dumps(hodood_pos), HODOOD_PID))
    print(f'[2] Updated El Hodood position (already existed)')
else:
    conn.execute("INSERT INTO positions (position_id, status, data, opened_at) VALUES (?,?,?,?)",
                 (HODOOD_PID, 'open', json.dumps(hodood_pos), '1778164484.306287'))
    print(f'[2] Created El Hodood position record')

# ---------------------------------------------------------------------------
# 3. Fix accounting checkpoint
# ---------------------------------------------------------------------------
row = conn.execute("SELECT data FROM checkpoints WHERE key='accounting_ledger'").fetchone()
ledger = json.loads(row[0])

print(f'\n[3] Accounting BEFORE:')
print(f'    cash={ledger["cash"]:.4f}, deployed={ledger["positions_deployed"]:.4f}, fees={ledger["fees_total"]:.6f}')

ledger['cash'] = ON_CHAIN_USDC
total_deployed = CRUNCHYROLL_FILLED_COST + HODOOD_BET
ledger['positions_deployed'] = total_deployed

# Holdings keyed by token_id
h = {}
h[HODOOD_TOKEN_ID] = {'quantity': HODOOD_SHARES, 'cost_basis': HODOOD_BET}
h['82724943425600151029939281806822319359475765380323844283886329142061964392665'] = \
    {'quantity': 4.1222, 'cost_basis': CRUNCHYROLL_COSTS['1834533']}
h['50044145130319219674999172614005136782999664814951249895410854425222125982484'] = \
    {'quantity': 4.1222, 'cost_basis': CRUNCHYROLL_COSTS['1834532']}
h['82389438372188201312360823731909997627694063888783766735886884183469631603599'] = \
    {'quantity': 4.1176, 'cost_basis': CRUNCHYROLL_COSTS['1834530']}
ledger['holdings'] = h

print(f'\n[3] Accounting AFTER:')
print(f'    cash={ledger["cash"]:.4f}, deployed={ledger["positions_deployed"]:.4f}')
print(f'    cash+deployed={ledger["cash"]+ledger["positions_deployed"]:.4f}  (initial=$97.37, delta explains Spurs legacy position)')
print(f'    holdings: {len(h)} entries')

conn.execute("UPDATE checkpoints SET data=?, updated_at=? WHERE key='accounting_ledger'",
             (json.dumps(ledger), str(time.time())))

# ---------------------------------------------------------------------------
# 4. Fix scalars
# ---------------------------------------------------------------------------
conn.execute("UPDATE scalars SET value=97.37 WHERE key='current_capital'")
conn.execute("UPDATE scalars SET value=97.37 WHERE key='initial_capital'")
print(f'\n[4] scalars.current_capital=$97.37 (total portfolio value)')

conn.commit()
conn.close()
print('\n=== Repair complete. Run: sudo systemctl start prediction-trader ===')
