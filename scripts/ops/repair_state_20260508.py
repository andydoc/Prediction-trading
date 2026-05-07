#!/usr/bin/env python3
"""
Repair DB state after partial-fill incident (2026-05-08).

Post-unwind state (4 Crunchyroll legs + Spurs + Nagasaki + Takeshita + Yamashiro
all sold via manual_unwind_v2.py --auto --yes).

Changes:
1. Mark Crunchyroll arb position CLOSED (all legs unwound on-chain).
2. Add/update El Hodood position record (OPEN, held pending exit).
3. Fix accounting checkpoint: cash=post-unwind CLOB pUSD balance ($86.60),
   deployed=El Hodood cost basis only ($7.302), holdings={hodood_token_id}.
4. Fix scalars: current_capital=$97.37 (cash + deployed, total unchanged).

El Hodood: 7.2 shares of "Draw NO" token, cost $7.302 ($1.014/share).
True breakeven is impossible (max payout $1/share < cost); exit intent is
sell-at-best-bid when CLOB bids appear (see metadata.exit_intent).
"""
import sqlite3, json, time

DB = '/home/ubuntu/prediction-trader/data/state_rust.db'

# Post-unwind CLOB pUSD balance (from /balance-allowance, 2026-05-08)
CLOB_USDC = 86.599794

# ---------------------------------------------------------------------------
# Position identifiers
# ---------------------------------------------------------------------------

CRUNCHYROLL_PID = 'paper_1778190634.257736_mutex_0xf7677b65d7bc3187034c89f4dc4a3b'

HODOOD_PID      = 'paper_1778164484.3062873_mutex_0xce9f2b128e0b4d1d7b921e24a566cb'
HODOOD_TOKEN_ID = '52318778167875974510372552758260515387718779455072251133050352396429657843717'
HODOOD_SHARES   = 7.2
HODOOD_BET      = 7.302603945000002   # from journal (leg 1 of trade 2)
HODOOD_MARKET   = 'hodood_draw_no'
HODOOD_COND     = '0xdb2cae6b3169273e9358fccd78061b7274fe45f9fa28bf6104d3244730dc15eb'

conn = sqlite3.connect(DB)

# ---------------------------------------------------------------------------
# 1. Close Crunchyroll position (all legs unwound on-chain)
# ---------------------------------------------------------------------------
row = conn.execute("SELECT data FROM positions WHERE position_id=?", (CRUNCHYROLL_PID,)).fetchone()
if row:
    pos = json.loads(row[0])
    pos['status'] = 'closed'
    pos['close_timestamp'] = str(time.time())
    pos['metadata'] = pos.get('metadata', {})
    pos['metadata']['close_reason'] = 'manual_unwind_incident_20260508'
    conn.execute("UPDATE positions SET data=?, status='closed' WHERE position_id=?",
                 (json.dumps(pos), CRUNCHYROLL_PID))
    print('[1] Crunchyroll position marked CLOSED (unwound on-chain)')
else:
    print('[1] Crunchyroll position not found — skipping')

# ---------------------------------------------------------------------------
# 2. Add (or update) El Hodood position record — OPEN, pending exit
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
    'last_check': str(time.time()),
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
        'condition_id': HODOOD_COND,
        'note': 'Reconstructed 2026-05-08 after partial reconciliation incident',
        # Exit intent: sell at best available bid as soon as CLOB bids exist.
        # True breakeven ($1.014/share) exceeds max payout ($1.00/share).
        # Run: python3 scripts/ops/manual_unwind_v2.py --token-id <ID> --shares 7.2 --neg-risk
        'exit_intent': 'sell_at_best_bid',
        'exit_token_id': HODOOD_TOKEN_ID,
        'exit_shares': HODOOD_SHARES,
    },
    'chain_id': None,
    'chain_generation': 0,
    'parent_position_id': None,
}

existing = conn.execute("SELECT 1 FROM positions WHERE position_id=?", (HODOOD_PID,)).fetchone()
if existing:
    conn.execute("UPDATE positions SET data=?, status='open' WHERE position_id=?",
                 (json.dumps(hodood_pos), HODOOD_PID))
    print('[2] El Hodood position UPDATED (open, exit_intent=sell_at_best_bid)')
else:
    conn.execute("INSERT INTO positions (position_id, status, data, opened_at) VALUES (?,?,?,?)",
                 (HODOOD_PID, 'open', json.dumps(hodood_pos), '1778164484.306287'))
    print('[2] El Hodood position CREATED (open, exit_intent=sell_at_best_bid)')

# ---------------------------------------------------------------------------
# 3. Fix accounting checkpoint
# ---------------------------------------------------------------------------
row = conn.execute("SELECT data FROM checkpoints WHERE key='accounting_ledger'").fetchone()
ledger = json.loads(row[0])

print(f'\n[3] Accounting BEFORE:')
print(f'    cash={ledger.get("cash",0):.4f}  deployed={ledger.get("positions_deployed",0):.4f}')

ledger['cash'] = CLOB_USDC
ledger['positions_deployed'] = HODOOD_BET
ledger['holdings'] = {
    HODOOD_TOKEN_ID: {'quantity': HODOOD_SHARES, 'cost_basis': HODOOD_BET}
}

print(f'\n[3] Accounting AFTER:')
print(f'    cash={ledger["cash"]:.6f}  deployed={ledger["positions_deployed"]:.6f}')
print(f'    total={ledger["cash"]+ledger["positions_deployed"]:.4f}')
print(f'    holdings: {len(ledger["holdings"])} entry (El Hodood only)')

conn.execute("UPDATE checkpoints SET data=?, updated_at=? WHERE key='accounting_ledger'",
             (json.dumps(ledger), str(time.time())))

# ---------------------------------------------------------------------------
# 4. Fix scalars
# ---------------------------------------------------------------------------
total = CLOB_USDC + HODOOD_BET
conn.execute("UPDATE scalars SET value=? WHERE key='current_capital'", (round(total, 4),))
# initial_capital reflects original deposit — do NOT change
print(f'\n[4] scalars.current_capital={total:.4f} (cash {CLOB_USDC:.4f} + deployed {HODOOD_BET:.4f})')

conn.commit()
conn.close()
print('\n=== Repair complete ===')
print(f'El Hodood exit: run manual_unwind_v2.py --token-id {HODOOD_TOKEN_ID} --shares {HODOOD_SHARES} --neg-risk')
print('Then: sudo systemctl start prediction-trader')
