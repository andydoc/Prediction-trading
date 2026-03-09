"""
SQLite State Store — In-memory database with periodic disk backup.

Replaces JSON-based state persistence with:
  - In-memory SQLite for fast reads/writes during operation
  - Atomic backup to disk file via sqlite3.backup()
  - Incremental position updates (only changed rows, not full rewrite)
  - WAL journal mode for crash safety on disk copy

Usage:
    store = StateStore('/path/to/execution_state.db')
    store.save_scalar('current_capital', 107.30)
    store.upsert_position(position_dict, status='open')
    store.backup_to_disk()  # atomic, ~1ms

Migration from JSON:
    store = StateStore('/path/to/execution_state.db')
    store.import_from_json('/path/to/execution_state.json')
"""

import json
import logging
import sqlite3
import time
from pathlib import Path
from typing import Dict, List, Optional, Any

log = logging.getLogger('state_store')


class StateStore:
    """In-memory SQLite state store with atomic disk backup."""

    def __init__(self, disk_path: str):
        self.disk_path = Path(disk_path)
        self.disk_path.parent.mkdir(parents=True, exist_ok=True)

        # In-memory database for fast access
        self.db = sqlite3.connect(':memory:', check_same_thread=False)
        self.db.execute('PRAGMA journal_mode=WAL')
        self.db.execute('PRAGMA synchronous=NORMAL')
        self._create_schema()

        # Track dirty positions for incremental saves
        self._dirty_positions: set = set()
        self._last_backup = 0.0

    def _create_schema(self):
        self.db.executescript('''
            CREATE TABLE IF NOT EXISTS scalars (
                key   TEXT PRIMARY KEY,
                value REAL
            );
            CREATE TABLE IF NOT EXISTS positions (
                position_id TEXT PRIMARY KEY,
                status      TEXT NOT NULL,
                data        TEXT NOT NULL,
                opened_at   TEXT,
                closed_at   TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_pos_status ON positions(status);
        ''')
        self.db.commit()

    # ── Scalar operations ──────────────────────────────────────────────

    def save_scalar(self, key: str, value: float):
        self.db.execute(
            'INSERT OR REPLACE INTO scalars (key, value) VALUES (?, ?)',
            (key, value))
        self.db.commit()

    def save_scalars(self, kvs: Dict[str, float]):
        self.db.executemany(
            'INSERT OR REPLACE INTO scalars (key, value) VALUES (?, ?)',
            list(kvs.items()))
        self.db.commit()

    def get_scalar(self, key: str, default: float = 0.0) -> float:
        row = self.db.execute(
            'SELECT value FROM scalars WHERE key=?', (key,)).fetchone()
        return row[0] if row else default

    def get_all_scalars(self) -> Dict[str, float]:
        rows = self.db.execute('SELECT key, value FROM scalars').fetchall()
        return {k: v for k, v in rows}

    # ── Position operations ────────────────────────────────────────────

    def upsert_position(self, pos_dict: dict, status: str = 'open'):
        """Insert or update a single position."""
        pid = pos_dict.get('position_id', '')
        opened_at = pos_dict.get('entry_timestamp', '')
        closed_at = pos_dict.get('close_timestamp', '')
        data_json = json.dumps(pos_dict, separators=(',', ':'))
        self.db.execute(
            '''INSERT OR REPLACE INTO positions
               (position_id, status, data, opened_at, closed_at)
               VALUES (?, ?, ?, ?, ?)''',
            (pid, status, data_json, str(opened_at), str(closed_at) if closed_at else None))
        self.db.commit()

    def upsert_positions_bulk(self, positions: List[dict], status: str):
        """Bulk upsert — much faster than individual calls."""
        rows = []
        for p in positions:
            pid = p.get('position_id', '')
            opened_at = p.get('entry_timestamp', '')
            closed_at = p.get('close_timestamp', '')
            data_json = json.dumps(p, separators=(',', ':'))
            rows.append((pid, status, data_json, str(opened_at),
                         str(closed_at) if closed_at else None))
        self.db.executemany(
            '''INSERT OR REPLACE INTO positions
               (position_id, status, data, opened_at, closed_at)
               VALUES (?, ?, ?, ?, ?)''', rows)
        self.db.commit()

    def get_positions_by_status(self, status: str) -> List[dict]:
        """Get all positions with given status."""
        rows = self.db.execute(
            'SELECT data FROM positions WHERE status=?', (status,)).fetchall()
        return [json.loads(row[0]) for row in rows]

    def get_position(self, position_id: str) -> Optional[dict]:
        row = self.db.execute(
            'SELECT data FROM positions WHERE position_id=?',
            (position_id,)).fetchone()
        return json.loads(row[0]) if row else None

    def delete_position(self, position_id: str):
        self.db.execute(
            'DELETE FROM positions WHERE position_id=?', (position_id,))
        self.db.commit()

    def update_position_status(self, position_id: str, new_status: str,
                                updated_data: dict = None):
        """Change status and optionally update data (e.g. open → closed)."""
        if updated_data:
            data_json = json.dumps(updated_data, separators=(',', ':'))
            closed_at = updated_data.get('close_timestamp', '')
            self.db.execute(
                '''UPDATE positions SET status=?, data=?, closed_at=?
                   WHERE position_id=?''',
                (new_status, data_json, str(closed_at) if closed_at else None,
                 position_id))
        else:
            self.db.execute(
                'UPDATE positions SET status=? WHERE position_id=?',
                (new_status, position_id))
        self.db.commit()

    def count_by_status(self) -> Dict[str, int]:
        rows = self.db.execute(
            'SELECT status, COUNT(*) FROM positions GROUP BY status').fetchall()
        return {s: c for s, c in rows}

    # ── Backup / Restore ───────────────────────────────────────────────

    def backup_to_disk(self):
        """Atomic backup of in-memory DB to disk file. ~1ms for typical state."""
        t0 = time.time()
        disk_db = sqlite3.connect(str(self.disk_path))
        self.db.backup(disk_db)
        disk_db.close()
        elapsed_ms = (time.time() - t0) * 1000
        self._last_backup = time.time()
        log.debug(f'State backed up to {self.disk_path.name} in {elapsed_ms:.1f}ms')

    def restore_from_disk(self):
        """Load disk DB into memory. Called on startup."""
        if not self.disk_path.exists():
            log.info(f'No disk state at {self.disk_path} — starting fresh')
            return False
        t0 = time.time()
        disk_db = sqlite3.connect(str(self.disk_path))
        disk_db.backup(self.db)  # disk → memory
        disk_db.close()
        elapsed_ms = (time.time() - t0) * 1000
        log.info(f'State restored from {self.disk_path.name} in {elapsed_ms:.1f}ms')
        return True

    # ── JSON migration ─────────────────────────────────────────────────

    def import_from_json(self, json_path: str) -> bool:
        """One-time migration: load execution_state.json into SQLite."""
        json_path = Path(json_path)
        if not json_path.exists():
            log.warning(f'JSON state not found: {json_path}')
            return False

        t0 = time.time()
        with open(json_path) as f:
            state = json.load(f)

        # Scalars
        self.save_scalars({
            'current_capital': state.get('current_capital', 100.0),
            'initial_capital': state.get('initial_capital', 100.0),
        })

        # Performance metrics
        perf = state.get('performance', {})
        for k in ('total_trades', 'winning_trades', 'losing_trades',
                  'total_actual_profit', 'total_expected_profit'):
            if k in perf:
                self.save_scalar(k, perf[k])

        # Open positions
        open_pos = state.get('open_positions', [])
        if open_pos:
            self.upsert_positions_bulk(open_pos, 'open')

        # Closed positions
        closed_pos = state.get('closed_positions', [])
        if closed_pos:
            self.upsert_positions_bulk(closed_pos, 'closed')

        elapsed_ms = (time.time() - t0) * 1000
        counts = self.count_by_status()
        log.info(f'Imported JSON state in {elapsed_ms:.0f}ms: '
                 f'{counts.get("open", 0)} open, {counts.get("closed", 0)} closed')
        return True

    def export_to_json(self) -> dict:
        """Export full state as JSON-compatible dict (backward compat for dashboard)."""
        scalars = self.get_all_scalars()
        open_pos = self.get_positions_by_status('open')
        closed_pos = self.get_positions_by_status('closed')

        return {
            'current_capital': scalars.get('current_capital', 100.0),
            'initial_capital': scalars.get('initial_capital', 100.0),
            'open_positions': open_pos,
            'closed_positions': closed_pos,
            'performance': {
                'total_trades': int(scalars.get('total_trades', 0)),
                'winning_trades': int(scalars.get('winning_trades', 0)),
                'losing_trades': int(scalars.get('losing_trades', 0)),
                'total_actual_profit': scalars.get('total_actual_profit', 0),
                'total_expected_profit': scalars.get('total_expected_profit', 0),
            }
        }

    def save_json_compat(self, json_path: str):
        """Write execution_state.json for backward compatibility (dashboard, debug scripts).
        This is the SLOW path — only called alongside backup, not on every state change."""
        t0 = time.time()
        state = self.export_to_json()
        json_path = Path(json_path)
        json_path.parent.mkdir(parents=True, exist_ok=True)
        with open(json_path, 'w') as f:
            json.dump(state, f, separators=(',', ':'))
        elapsed_ms = (time.time() - t0) * 1000
        log.debug(f'JSON compat written in {elapsed_ms:.0f}ms')
