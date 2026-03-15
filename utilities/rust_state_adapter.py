"""Adapter: makes RustStateDB look like Python StateStore for paper_trading.py.

Drop-in replacement: paper_trading.py calls the same methods (save_scalars, 
upsert_positions_bulk, backup_to_disk, etc.) and this adapter translates to
RustStateDB calls. The key win: backup_to_disk() releases the GIL.
"""
import json
import logging
from typing import Dict, List, Optional
from pathlib import Path

log = logging.getLogger('rust_state_adapter')

try:
    import rust_engine
    HAS_RUST_STATE = True
except ImportError:
    HAS_RUST_STATE = False


class RustStateAdapter:
    """Wraps rust_engine.RustStateDB with the same API as utilities.state_store.StateStore."""

    def __init__(self, disk_path: str):
        if not HAS_RUST_STATE:
            raise ImportError("rust_engine not available")
        self._db = rust_engine.RustStateDB(disk_path)
        self.disk_path = disk_path
        # Expose a fake .db attribute for the one direct SQL call in save_state
        self.db = _FakeDB(self)

    # --- Scalars ---

    def save_scalar(self, key: str, value):
        self._db.set_scalar(key, float(value))

    def save_scalars(self, kvs: Dict[str, float]):
        pairs = [(k, float(v)) for k, v in kvs.items()]
        self._db.set_scalars(pairs)

    def get_scalar(self, key: str, default: float = 0.0) -> float:
        v = self._db.get_scalar(key)
        return v if v is not None else default

    def get_all_scalars(self) -> Dict[str, float]:
        pairs = self._db.get_all_scalars()
        return {k: v for k, v in pairs}

    # --- Positions ---

    def upsert_position(self, pos_dict: dict, status: str = 'open'):
        pid = pos_dict.get('position_id', '')
        opened_at = pos_dict.get('entry_timestamp')
        closed_at = None
        if pos_dict.get('close_timestamp'):
            from datetime import datetime, timezone
            try:
                ts = float(pos_dict['close_timestamp'])
                closed_at = datetime.fromtimestamp(ts, tz=timezone.utc).isoformat()
            except (ValueError, TypeError):
                closed_at = str(pos_dict['close_timestamp'])
        self._db.save_position(pid, status, json.dumps(pos_dict), opened_at, closed_at)

    def upsert_positions_bulk(self, positions: List[dict], status: str):
        rows = []
        for p in positions:
            pid = p.get('position_id', '')
            opened_at = p.get('entry_timestamp')
            closed_at = None
            if p.get('close_timestamp'):
                from datetime import datetime, timezone
                try:
                    ts = float(p['close_timestamp'])
                    closed_at = datetime.fromtimestamp(ts, tz=timezone.utc).isoformat()
                except (ValueError, TypeError):
                    closed_at = str(p['close_timestamp'])
            rows.append((pid, status, json.dumps(p), opened_at, closed_at))
        self._db.save_positions_bulk(rows)

    def get_positions_by_status(self, status: str) -> List[dict]:
        """Query by DB column status (matches Python StateStore behavior)."""
        jsons = self._db.load_by_status(status)
        return [json.loads(j) for j in jsons]

    def delete_position(self, position_id: str):
        self._db.delete_position(position_id)

    def count_by_status(self) -> Dict[str, int]:
        pairs = self._db.count_by_status()
        return {status: int(count) for status, count in pairs}

    def get_open_position_ids(self) -> set:
        return set(self._db.get_open_position_ids())

    # --- Delay P95 table ---

    def get_delay_table(self):
        """Get delay P95 values as list of (category, p95_hours)."""
        return self._db.get_delay_table()

    def set_delay_table(self, rows, updated_at: str):
        """Replace delay P95 table. rows: [(cat, p95, count, median, p75, pct)]"""
        self._db.set_delay_table(rows, updated_at)

    # --- Disk persistence ---

    def backup_to_disk(self):
        """GIL-free disk mirror — the key latency win.
        Rust's mirror_to_disk() calls py.allow_threads() internally."""
        ms = self._db.mirror_to_disk()
        log.debug(f"State backed up to disk in {ms:.1f}ms (Rust)")

    def restore_from_disk(self) -> bool:
        try:
            ms = self._db.load_from_disk()
            log.info(f"Restored state from disk in {ms:.1f}ms (Rust)")
            return True
        except Exception as e:
            log.debug(f"No disk state to restore: {e}")
            return False

    def import_from_json(self, json_path: str) -> bool:
        """One-time migration from JSON execution state file."""
        try:
            state = json.loads(Path(json_path).read_text())
        except Exception as e:
            log.error(f"Failed to read JSON state: {e}")
            return False

        # Import scalars
        self.save_scalars({
            'current_capital': state.get('current_capital', 100),
            'initial_capital': state.get('initial_capital', 100),
        })
        perf = state.get('performance', {})
        for k in ('total_trades', 'winning_trades', 'losing_trades',
                  'total_actual_profit', 'total_expected_profit'):
            if k in perf:
                self.save_scalar(k, perf[k])

        # Import open positions
        for p in state.get('open_positions', []):
            if isinstance(p, dict):
                self.upsert_position(p, p.get('status', 'open'))

        # Import closed positions
        for p in state.get('closed_positions', []):
            if isinstance(p, dict):
                self.upsert_position(p, 'closed')

        log.info(f"Imported from JSON: {len(state.get('open_positions', []))} open, "
                f"{len(state.get('closed_positions', []))} closed")
        return True


class _FakeDB:
    """Minimal shim for the one `self._state_store.db.execute(...)` call in save_state."""

    def __init__(self, adapter: RustStateAdapter):
        self._adapter = adapter

    def execute(self, sql: str):
        """Only handles the specific query used in paper_trading.py save_state."""
        if 'position_id' in sql and 'status' in sql:
            # Return rows with (position_id,) tuples
            ids = self._adapter.get_open_position_ids()
            return _FakeResult([(pid,) for pid in ids])
        raise NotImplementedError(f"Direct SQL not supported via Rust adapter: {sql}")


class _FakeResult:
    """Minimal cursor result shim."""

    def __init__(self, rows):
        self._rows = rows

    def fetchall(self):
        return self._rows
