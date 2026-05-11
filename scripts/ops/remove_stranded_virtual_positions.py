#!/usr/bin/env python3
"""Surgically remove specific constraint_ids from strategy_tracker virtual portfolios.

Used to clean up phantom open positions that the live executor partial-filled and
then auto-rolled back, but which remained "open" in shadow portfolios because the
old code didn't propagate the rollback to the strategy tracker.

For each affected portfolio:
  1. DELETE row from strategy_open_positions where constraint_id matches
  2. Add the position's capital_deployed back to strategy_portfolios.current_capital
  3. Decrement total_entered by 1 (the entry never really happened)

Backs up state_rust.db before mutating. Idempotent: skips constraints/portfolios
that no longer have the row.
"""
import json
import shutil
import sqlite3
import sys
import time
from pathlib import Path

DB = Path('/home/ubuntu/prediction-trader/data/state_rust.db')

# INC-D9 stranded shadow positions (live executor partial-filled and reconciled
# to MissingOnVenue; strategy tracker was never notified).
STRANDED_CONSTRAINT_IDS = [
    'mutex_0xf7677b65d7bc3187034c89f4dc4a3b',  # Crunchyroll Best Director (2026-05-07)
    'mutex_0xbd4bcfcb256afd1809f4b9c7e72427',  # SAKAMOTO DAYS Arabic Voice  (2026-05-09)
]


def main():
    if not DB.exists():
        print(f'ERROR: {DB} not found', file=sys.stderr)
        return 1

    backup = DB.with_suffix(f'.db.pre_remove_stranded_{int(time.time())}')
    shutil.copy(DB, backup)
    print(f'[backup] {backup.name}')

    conn = sqlite3.connect(str(DB))
    conn.row_factory = sqlite3.Row

    total_removed = 0
    total_capital_refunded = 0.0

    for cid in STRANDED_CONSTRAINT_IDS:
        rows = conn.execute(
            'SELECT strategy_name, data FROM strategy_open_positions WHERE constraint_id = ?',
            (cid,),
        ).fetchall()

        if not rows:
            print(f'[{cid[:24]}…] no rows — already clean')
            continue

        for row in rows:
            strategy = row['strategy_name']
            data = json.loads(row['data'])
            deployed = float(data.get('capital_deployed', 0.0))

            conn.execute(
                'DELETE FROM strategy_open_positions WHERE strategy_name = ? AND constraint_id = ?',
                (strategy, cid),
            )

            conn.execute(
                'UPDATE strategy_portfolios '
                'SET current_capital = current_capital + ?, '
                '    total_entered  = MAX(total_entered - 1, 0) '
                'WHERE name = ?',
                (deployed, strategy),
            )

            total_removed += 1
            total_capital_refunded += deployed
            print(f'  [{strategy}] removed {cid[:24]}… ${deployed:.2f} refunded')

    conn.commit()

    print()
    print(f'[done] removed {total_removed} virtual position(s), '
          f'${total_capital_refunded:.2f} capital refunded across portfolios')
    print(f'[done] backup at {backup}')

    print()
    print('=== verification ===')
    for strategy in ['Shadow-A', 'Shadow-B', 'Shadow-C', 'Shadow-D', 'Shadow-E', 'Shadow-F']:
        cnt = conn.execute(
            'SELECT COUNT(*) FROM strategy_open_positions WHERE strategy_name = ?',
            (strategy,),
        ).fetchone()[0]
        cap = conn.execute(
            'SELECT current_capital FROM strategy_portfolios WHERE name = ?',
            (strategy,),
        ).fetchone()
        cap_v = cap[0] if cap else float('nan')
        print(f'  {strategy}: open={cnt}, capital=${cap_v:.2f}')

    conn.close()
    return 0


if __name__ == '__main__':
    sys.exit(main())
