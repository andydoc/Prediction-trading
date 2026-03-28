#!/usr/bin/env python3
"""Fix proactive exits in backup DB — insert into strategy tables."""
import sqlite3
import json
import sys

db_path = sys.argv[1] if len(sys.argv) > 1 else '/home/ubuntu/prediction-trader/data/state_rust.db.backup_20260327_pre_proactive_fix'
db = sqlite3.connect(db_path)

exits = [
    {
        'cid': 'mutex_0x35400da0a2072ad50810634bac7668',
        'profit_pct': 0.7876,
        'close_ts': 1774641378.71851,
        'name': 'Cordoba CF',
        'method': 'mutex_buy_all',
        'is_sell': 0,
    },
    {
        'cid': 'mutex_0xbc7528552d8226d98dc67176917e95',
        'profit_pct': 0.401832331606218,
        'close_ts': 1774656756.21808,
        'name': 'DHS shutdown',
        'method': 'mutex_buy_all',
        'is_sell': 0,
    },
]

for ex in exits:
    cid = ex['cid']
    rows = db.execute(
        'SELECT strategy_name, data FROM strategy_open_positions WHERE constraint_id=?',
        (cid,)
    ).fetchall()

    for strat, raw in rows:
        vp = json.loads(raw)
        capital = vp['capital_deployed']
        profit = capital * ex['profit_pct']
        entry_ts = vp['entry_ts']

        db.execute(
            'INSERT INTO strategy_closed_positions '
            '(strategy_name, capital_deployed, actual_profit, actual_profit_pct, '
            'entry_ts, close_ts, is_win, short_name, method, is_sell) '
            'VALUES (?,?,?,?,?,?,?,?,?,?)',
            (strat, capital, profit, ex['profit_pct'], entry_ts, ex['close_ts'],
             1, ex['name'], ex['method'], ex['is_sell'])
        )

        db.execute(
            'UPDATE strategy_portfolios SET current_capital = current_capital + ?, '
            'total_wins = total_wins + 1 WHERE name = ?',
            (capital + profit, strat)
        )
        print(f"{strat} {ex['name'][:20]}: deployed=${capital:.2f} profit=${profit:.2f} ({ex['profit_pct']*100:.1f}%)")

    deleted = db.execute(
        'DELETE FROM strategy_open_positions WHERE constraint_id=?', (cid,)
    ).rowcount
    print(f"  Deleted {deleted} open positions for {cid[:20]}")

db.commit()

# Verify
print('\n--- Verification ---')
total_cash = 0
total_deployed = 0
for row in db.execute('SELECT name, current_capital, total_entered, total_wins, total_losses FROM strategy_portfolios').fetchall():
    total_cash += row[1]
    print(f"{row[0]}: cash=${row[1]:.2f} entered={row[2]} wins={row[3]} losses={row[4]}")

for row in db.execute("SELECT strategy_name, json_extract(data, '$.capital_deployed') FROM strategy_open_positions").fetchall():
    total_deployed += row[1]

closed_profit = db.execute('SELECT SUM(actual_profit) FROM strategy_closed_positions').fetchone()[0] or 0
closed_count = db.execute('SELECT COUNT(*) FROM strategy_closed_positions').fetchone()[0]
open_count = db.execute('SELECT COUNT(*) FROM strategy_open_positions').fetchone()[0]

print(f"\nCash: ${total_cash:.2f}")
print(f"Deployed: ${total_deployed:.2f}")
print(f"Total value: ${total_cash + total_deployed:.2f}")
print(f"Closed positions: {closed_count} (total profit: ${closed_profit:.2f})")
print(f"Open positions: {open_count}")
print(f"Expected total: ${6000 + closed_profit:.2f} (6000 init + {closed_profit:.2f} realized)")

db.close()
