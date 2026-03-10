"""
SQLite State Manager — replaces JSON execution_state.json with in-memory SQLite + WAL disk mirror.

Benefits over JSON:
  - Incremental updates: INSERT/UPDATE single position vs rewrite entire file
  - Atomic persistence: db.backup() is crash-safe
  - Fast queries: indexed by status, no full deserialization
  - Constant save time: O(1) per position change vs O(N) for all positions

Usage in paper_trading.py:
    from state_db import StateDB
    db = StateDB('/path/to/execution_state.db')
    db.save_position(position)       # incremental
    db.mirror_to_disk()              # periodic (every 30s)
    positions = db.load_open()       # fast indexed query
"""

import json
import sqlite3
import logging
import time
from pathlib import Path
from datetime import datetime, timezone
from typing import Dict, List, Optional, Any

log = logging.getLogger('state_db')


def read_state_from_disk(db_path: str) -> Optional[dict]:
    """Read-only access to the disk SQLite file (for dashboard / external tools).
    
    Returns a dict matching the old JSON format:
    {current_capital, initial_capital, open_positions, closed_positions, performance}
    Returns None if db file doesn't exist or can't be read.
    """
    p = Path(db_path)
    if not p.exists():
        return None
    try:
        db = sqlite3.connect(f'file:{db_path}?mode=ro', uri=True)
        db.execute('PRAGMA query_only=ON')
        
        # Scalars
        scalars = {}
        for row in db.execute('SELECT key, value FROM state').fetchall():
            try:
                scalars[row[0]] = json.loads(row[1])
            except:
                scalars[row[0]] = row[1]
        
        # Open positions
        open_pos = []
        for row in db.execute(
                "SELECT data FROM positions WHERE status IN ('open','monitoring')").fetchall():
            try:
                open_pos.append(json.loads(row[0]))
            except:
                pass
        
        # Closed positions
        closed_pos = []
        for row in db.execute(
                "SELECT data FROM positions WHERE status='closed'").fetchall():
            try:
                closed_pos.append(json.loads(row[0]))
            except:
                pass
        
        db.close()
        
        return {
            'current_capital': scalars.get('current_capital', 100.0),
            'initial_capital': scalars.get('initial_capital', 100.0),
            'open_positions': open_pos,
            'closed_positions': closed_pos,
            'performance': {
                'total_trades': scalars.get('total_trades', 0),
                'winning_trades': scalars.get('winning_trades', 0),
                'losing_trades': scalars.get('losing_trades', 0),
                'total_actual_profit': scalars.get('total_actual_profit', 0),
                'total_expected_profit': scalars.get('total_expected_profit', 0),
            }
        }
    except Exception as e:
        log.warning(f'read_state_from_disk failed: {e}')
        return None


class StateDB:
    """In-memory SQLite state with periodic disk mirror."""

    def __init__(self, disk_path: str = None):
        self.disk_path = Path(disk_path) if disk_path else None
        self.db = sqlite3.connect(':memory:')
        self.db.execute('PRAGMA journal_mode=WAL')
        self.db.execute('PRAGMA synchronous=OFF')  # in-memory, no disk sync needed
        self._create_tables()
        self._dirty_positions: set = set()  # position_ids changed since last mirror
        self._last_mirror = 0.0
        log.info(f'StateDB initialized (in-memory, disk={self.disk_path})')

    def _create_tables(self):
        self.db.executescript('''
            CREATE TABLE IF NOT EXISTS state (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS positions (
                position_id TEXT PRIMARY KEY,
                status TEXT NOT NULL DEFAULT 'open',
                data JSON NOT NULL,
                opened_at TEXT,
                closed_at TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_pos_status ON positions(status);
        ''')
        self.db.commit()

    # --- Scalar state (capital, counters) ---

    def set_scalar(self, key: str, value: Any):
        self.db.execute(
            'INSERT OR REPLACE INTO state (key, value) VALUES (?, ?)',
            (key, json.dumps(value))
        )

    def get_scalar(self, key: str, default: Any = None) -> Any:
        row = self.db.execute(
            'SELECT value FROM state WHERE key = ?', (key,)
        ).fetchone()
        return json.loads(row[0]) if row else default

    def set_capital(self, current: float, initial: float = None):
        self.set_scalar('current_capital', current)
        if initial is not None:
            self.set_scalar('initial_capital', initial)

    def get_capital(self) -> tuple:
        return (
            self.get_scalar('current_capital', 100.0),
            self.get_scalar('initial_capital', 100.0)
        )

    def set_metrics(self, total_trades: int, winning: int, losing: int,
                    total_actual_profit: float, total_expected_profit: float):
        self.set_scalar('total_trades', total_trades)
        self.set_scalar('winning_trades', winning)
        self.set_scalar('losing_trades', losing)
        self.set_scalar('total_actual_profit', total_actual_profit)
        self.set_scalar('total_expected_profit', total_expected_profit)

    # --- Position CRUD ---

    def save_position(self, pos_dict: dict):
        """Upsert a single position. pos_dict is PaperPosition.to_dict() output."""
        pid = pos_dict.get('position_id', '')
        status = pos_dict.get('status', 'open')
        opened_at = pos_dict.get('entry_timestamp')
        closed_at = pos_dict.get('close_timestamp')
        if isinstance(closed_at, (int, float)):
            closed_at = datetime.fromtimestamp(closed_at, tz=timezone.utc).isoformat()

        self.db.execute(
            '''INSERT OR REPLACE INTO positions
               (position_id, status, data, opened_at, closed_at)
               VALUES (?, ?, ?, ?, ?)''',
            (pid, status, json.dumps(pos_dict, separators=(',', ':')),
             opened_at, closed_at)
        )
        self._dirty_positions.add(pid)

    def save_positions_bulk(self, positions: list):
        """Bulk upsert positions (for initial load from JSON)."""
        rows = []
        for p in positions:
            pid = p.get('position_id', '')
            status = p.get('status', 'open')
            opened_at = p.get('entry_timestamp')
            closed_at = p.get('close_timestamp')
            if isinstance(closed_at, (int, float)):
                closed_at = datetime.fromtimestamp(closed_at, tz=timezone.utc).isoformat()
            rows.append((pid, status, json.dumps(p, separators=(',', ':')),
                         opened_at, closed_at))
        self.db.executemany(
            '''INSERT OR REPLACE INTO positions
               (position_id, status, data, opened_at, closed_at)
               VALUES (?, ?, ?, ?, ?)''', rows
        )
        self.db.commit()

    def load_open(self) -> list:
        """Load all open/monitoring positions."""
        rows = self.db.execute(
            "SELECT data FROM positions WHERE status IN ('open', 'monitoring')"
        ).fetchall()
        return [json.loads(r[0]) for r in rows]

    def load_closed(self) -> list:
        """Load all closed positions."""
        rows = self.db.execute(
            "SELECT data FROM positions WHERE status = 'closed'"
        ).fetchall()
        return [json.loads(r[0]) for r in rows]

    def count_by_status(self) -> dict:
        """Quick counts without deserializing."""
        rows = self.db.execute(
            'SELECT status, COUNT(*) FROM positions GROUP BY status'
        ).fetchall()
        return {r[0]: r[1] for r in rows}

    def delete_position(self, position_id: str):
        self.db.execute('DELETE FROM positions WHERE position_id = ?', (position_id,))
        self._dirty_positions.add(position_id)

    # --- Disk persistence ---

    def mirror_to_disk(self):
        """Atomic backup of in-memory DB to disk file. ~1ms for typical state."""
        if not self.disk_path:
            return
        self.db.commit()  # flush any pending transactions
        t0 = time.time()
        self.disk_path.parent.mkdir(parents=True, exist_ok=True)
        disk_db = sqlite3.connect(str(self.disk_path))
        self.db.backup(disk_db)
        disk_db.close()
        elapsed_ms = (time.time() - t0) * 1000
        n_dirty = len(self._dirty_positions)
        self._dirty_positions.clear()
        self._last_mirror = time.time()
        if elapsed_ms > 50:
            log.warning(f'Slow mirror: {elapsed_ms:.0f}ms ({n_dirty} dirty)')

    def load_from_disk(self):
        """Load disk DB into memory (startup recovery)."""
        if not self.disk_path or not self.disk_path.exists():
            return False
        t0 = time.time()
        disk_db = sqlite3.connect(str(self.disk_path))
        disk_db.backup(self.db)
        disk_db.close()
        elapsed_ms = (time.time() - t0) * 1000
        counts = self.count_by_status()
        log.info(f'Loaded from disk in {elapsed_ms:.0f}ms: {counts}')
        return True

    # --- JSON migration (backward compatibility) ---

    def import_from_json(self, json_path: str):
        """One-time migration: load execution_state.json into SQLite."""
        t0 = time.time()
        with open(json_path) as f:
            state = json.load(f)

        self.set_capital(
            state.get('current_capital', 100.0),
            state.get('initial_capital', 100.0)
        )

        perf = state.get('performance', {})
        self.set_metrics(
            total_trades=perf.get('total_trades', 0),
            winning=perf.get('winning_trades', 0),
            losing=perf.get('losing_trades', 0),
            total_actual_profit=perf.get('total_actual_profit', 0),
            total_expected_profit=perf.get('total_expected_profit', 0),
        )

        all_positions = []
        for p in state.get('open_positions', []):
            if isinstance(p, dict):
                all_positions.append(p)
        for p in state.get('closed_positions', []):
            if isinstance(p, dict):
                all_positions.append(p)

        self.save_positions_bulk(all_positions)
        elapsed_ms = (time.time() - t0) * 1000
        counts = self.count_by_status()
        log.info(f'Imported JSON in {elapsed_ms:.0f}ms: {counts}')

    def export_to_json(self, json_path: str):
        """Export state back to JSON format (backward compat for dashboard)."""
        t0 = time.time()
        current, initial = self.get_capital()
        open_positions = self.load_open()
        closed_positions = self.load_closed()

        state = {
            'current_capital': current,
            'initial_capital': initial,
            'open_positions': open_positions,
            'closed_positions': closed_positions,
            'performance': {
                'total_trades': self.get_scalar('total_trades', 0),
                'winning_trades': self.get_scalar('winning_trades', 0),
                'losing_trades': self.get_scalar('losing_trades', 0),
                'total_actual_profit': self.get_scalar('total_actual_profit', 0),
                'total_expected_profit': self.get_scalar('total_expected_profit', 0),
            }
        }

        Path(json_path).parent.mkdir(parents=True, exist_ok=True)
        with open(json_path, 'w') as f:
            json.dump(state, f, separators=(',', ':'))
        elapsed_ms = (time.time() - t0) * 1000
        if elapsed_ms > 100:
            log.warning(f'Slow JSON export: {elapsed_ms:.0f}ms')

    # --- Aliases (paper_trading.py compatibility) ---

    def save_scalar(self, key: str, value: Any):
        """Alias for set_scalar (paper_trading.py calls this)."""
        self.set_scalar(key, value)

    def save_scalars(self, kv: dict):
        """Batch set_scalar from dict."""
        for k, v in kv.items():
            self.set_scalar(k, v)

    def upsert_positions_bulk(self, positions: list, status: str = None):
        """Alias for save_positions_bulk with optional status override."""
        if status:
            for p in positions:
                p['status'] = status
        self.save_positions_bulk(positions)

    def backup_to_disk(self):
        """Alias for mirror_to_disk."""
        self.mirror_to_disk()

    def save_json_compat(self, json_path: str):
        """Alias for export_to_json."""
        self.export_to_json(json_path)

    def close(self):
        """Final mirror + close."""
        self.mirror_to_disk()
        self.db.close()
