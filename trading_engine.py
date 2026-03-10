"""
Trading Engine — Event-driven core combining constraint detection, arbitrage math, and execution.

Replaces the old poll-based L2 + L3 + L4 pipeline with a single async process
that reacts to WebSocket price changes in real time.

Architecture:
  Startup:
    1. Load markets from latest_markets.json (Market Scanner output)
    2. Run constraint detection (inline, no separate L2 process)
    3. Build asset_id → constraint_id index
    4. Start WS manager, subscribe all constraint group assets
    5. Load execution state, init paper/live engines

  Event loop:
    WS price_change → lookup affected constraints → re-run arb math → enter/replace
    WS market_resolved → find affected position → trigger immediate payout
    WS new_market → fetch metadata, re-run constraint detection, rebuild index
    Periodic (60s) → monitor positions, save state, log stats

Config: config.yaml (unchanged)
State:  data/system_state/execution_state.json (unchanged)
"""

import asyncio
import json
import logging
import math
import os
import signal
import sys
import time as _time
import yaml
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from datetime import datetime, timezone, timedelta
from zoneinfo import ZoneInfo
from typing import Dict, List, Set, Optional, Tuple

sys.path.append(str(Path(__file__).parent))
from paper_trading import PaperTradingEngine
from live_trading import LiveTradingEngine
from layer1_market_data.market_data import MarketData
from layer2_constraint_detection.constraint_detector import ConstraintDetector
from layer3_arbitrage_math.arbitrage_engine import ArbitrageMathEngine
from resolution_validator import (
    get_validated_resolution_date, get_full_validation,
    _load_cache as load_resolution_cache
)
from websocket_manager import (
    WebSocketManager, get_asset_ids_for_constraints,
    get_condition_ids_for_positions
)

try:
    import rust_arb
    HAS_RUST = True
except ImportError:
    HAS_RUST = False

# Load .env file for API keys
_env_file = Path(__file__).parent / '.env'
if _env_file.exists():
    for line in _env_file.read_text().splitlines():
        line = line.strip()
        if line and not line.startswith('#') and '=' in line:
            k, v = line.split('=', 1)
            os.environ.setdefault(k.strip(), v.strip())

# --- Paths ---
WORKSPACE    = Path('/home/andydoc/prediction-trader')
CONFIG_PATH  = WORKSPACE / 'config' / 'config.yaml'
SECRETS_PATH = WORKSPACE / 'config' / 'secrets.yaml'
MARKETS_PATH = WORKSPACE / 'data' / 'latest_markets.json'
CONSTRAINTS_PATH = WORKSPACE / 'layer2_constraint_detection' / 'data' / 'latest_constraints.json'
OPP_PATH     = WORKSPACE / 'layer3_arbitrage_math' / 'data' / 'latest_opportunities.json'
STATUS_PATH  = WORKSPACE / 'data' / 'trading_engine_status.json'
STATE_DIR    = WORKSPACE / 'data' / 'system_state'
EXEC_STATE   = STATE_DIR / 'execution_state.json'
P95_JSON_PATH = WORKSPACE / 'data' / 'resolution_delay_p95.json'

# --- Logging ---
logging.basicConfig(
    level=logging.DEBUG,
    format='%(asctime)s - [ENGINE] %(levelname)s - %(message)s',
    handlers=[
        logging.FileHandler(str(
            WORKSPACE / 'logs' / f'trading_engine_{datetime.now().strftime("%Y%m%d")}.log'
        )),
        logging.StreamHandler(),
    ])
log = logging.getLogger('trading_engine')

# Quiet noisy libraries
logging.getLogger('websockets').setLevel(logging.WARNING)
logging.getLogger('websockets.client').setLevel(logging.WARNING)

# =============================================================================
# Utility Functions (ported from layer4_runner.py)
# =============================================================================

# --- Resolution delay model ---
_FALLBACK_P95 = {
    'football': 14.8, 'us_sports': 33.6, 'esports': 20.0, 'tennis': 20.8,
    'mma_boxing': 50.3, 'cricket': 21.8, 'rugby': 23.3, 'politics': 350.2,
    'gov_policy': 44.3, 'crypto': 3.4, 'sports_props': 6.5, 'other': 33.5,
}
_FALLBACK_DEFAULT = 33.5
_delay_table_cache = {'p95_hours': None, 'default': None, 'loaded_at': None}


def load_delay_table():
    """Load P95 delay table from JSON. Falls back to hardcoded values."""
    if (_delay_table_cache['p95_hours'] is not None and _delay_table_cache['loaded_at']
            and (_time.time() - _delay_table_cache['loaded_at']) < 3600):
        return _delay_table_cache['p95_hours'], _delay_table_cache['default']
    try:
        if P95_JSON_PATH.exists():
            data = json.loads(P95_JSON_PATH.read_text())
            p95 = data.get('p95_hours', {})
            default = data.get('default_p95_hours', _FALLBACK_DEFAULT)
            if p95:
                _delay_table_cache.update(p95_hours=p95, default=default, loaded_at=_time.time())
                return p95, default
    except Exception as e:
        log.warning(f'Failed to load delay table: {e}')
    return _FALLBACK_P95, _FALLBACK_DEFAULT


def classify_opportunity_category(market_names: list, market_lookup: dict, market_ids: list) -> str:
    """Classify an opportunity into a resolution-delay category."""
    names_lower = ' '.join(n.lower() for n in market_names)
    descs = ''
    for mid in market_ids:
        md = market_lookup.get(str(mid))
        if md and hasattr(md, 'metadata') and isinstance(md.metadata, dict):
            descs += ' ' + md.metadata.get('description', '').lower()
    football_q = any(p in names_lower for p in ['win on 20', 'end in a draw', 'halftime', 'leading at halftime'])
    football_d = any(p in descs for p in ['90 minutes', 'stoppage time', 'regular play'])
    if football_q and (football_d or not descs):
        return 'football'
    if any(p in names_lower for p in ['halftime', 'leading at halftime']):
        return 'football'
    if any(p in names_lower for p in ['nba ', 'nfl ', 'nhl ', 'mlb ', 'wnba ',
                                       'touchdown', 'rushing yards', 'passing yards',
                                       'rebounds', 'three-pointer']):
        return 'us_sports'
    if any(p in names_lower for p in ['spread:', 'team total:', 'o/u ']):
        if any(p in descs for p in ['90 minutes', 'stoppage']):
            return 'football'
        return 'sports_props'
    if any(p in names_lower for p in ['counter-strike', 'cs2', 'dota', 'league of legends',
                                       'valorant', 'overwatch', 'dreamleague']):
        return 'esports'
    if any(p in names_lower for p in ['atp ', 'wta ', 'tennis']):
        return 'tennis'
    if any(p in names_lower for p in ['ufc ', 'mma ', 'boxing', 'pfl ', 'bellator']):
        return 'mma_boxing'
    if any(p in names_lower for p in ['cricket', 'ipl ', 't20 ']):
        return 'cricket'
    if any(p in names_lower for p in ['rugby', 'super rugby', 'waratahs']):
        return 'rugby'
    if any(p in names_lower for p in ['bitcoin', 'ethereum', 'solana', 'btc ', 'eth ',
                                       'up or down']):
        return 'crypto'
    if any(p in names_lower for p in ['governor', 'congress', 'senate', 'primary',
                                       'democrat', 'republican', 'election', 'president']):
        return 'politics'
    if any(p in names_lower for p in ['fed ', 'federal reserve', 'interest rate',
                                       'tariff', 'government shutdown']):
        return 'gov_policy'
    return 'other'


def get_volume_penalty_hours(min_volume: float) -> float:
    """Soft volume penalty for low-volume markets."""
    if min_volume <= 0:
        return 8.0
    return max(0.0, (5.0 - math.log10(min_volume + 1)) * 2.0)


def get_min_volume(opp_dict: dict, market_lookup: dict) -> float:
    """Minimum volume_24h across all markets in an opportunity."""
    volumes = []
    for mid in opp_dict.get('market_ids', []):
        md = market_lookup.get(str(mid))
        if md and hasattr(md, 'volume_24h'):
            volumes.append(md.volume_24h or 0)
    return min(volumes) if volumes else 0.0


def dynamic_capital(current_balance: float, pct: float = 0.10) -> float:
    """% of current capital, floor $10, cap $1000."""
    return max(10.0, min(current_balance * pct, 1000.0))


def get_resolution_hours(opp_dict: dict, market_lookup: dict) -> Optional[float]:
    """Hours until ALL markets resolve (latest end_date). Returns None if no dates."""
    now = datetime.now(timezone.utc)
    dates = []
    for mid in opp_dict.get('market_ids', []):
        mid_str = str(mid)
        if mid_str in market_lookup:
            ed = market_lookup[mid_str].end_date
            if ed.tzinfo is None:
                ed = ed.replace(tzinfo=timezone.utc)
            dates.append((ed - now).total_seconds())
    if not dates:
        return None
    max_delta = max(dates)
    if max_delta <= 0:
        return -1
    return max_delta / 3600


def rank_opportunities(opps: list, market_lookup: dict,
                       min_resolution_secs: int = 300,
                       max_days_to_resolution: int = 60) -> list:
    """Score and rank opportunities by profit_pct / effective_hours."""
    scored = []
    max_hours = max_days_to_resolution * 24
    for opp in opps:
        hours = get_resolution_hours(opp, market_lookup)
        if hours is None or hours < 0:
            continue
        if hours * 3600 < min_resolution_secs:
            continue
        if hours > max_hours:
            continue
        profit_pct = opp.get('expected_profit_pct', 0)
        p95_table, default_p95 = load_delay_table()
        category = classify_opportunity_category(
            opp.get('market_names', []), market_lookup, opp.get('market_ids', []))
        p95_delay = p95_table.get(category, default_p95)
        min_vol = get_min_volume(opp, market_lookup)
        vol_penalty = get_volume_penalty_hours(min_vol)
        effective_hours = hours + p95_delay + vol_penalty
        score = profit_pct / max(effective_hours, 0.01)
        scored.append((score, hours, opp))
    scored.sort(key=lambda x: x[0], reverse=True)
    return scored


def write_status(status, capital=0, open_pos=0, error=None, engine_metrics=None):
    STATUS_PATH.parent.mkdir(parents=True, exist_ok=True)
    data = {
        'status': status, 'capital': capital, 'open_positions': open_pos,
        'error': error, 'timestamp': datetime.now(ZoneInfo('Europe/London')).isoformat()
    }
    if engine_metrics:
        data['metrics'] = engine_metrics
    STATUS_PATH.write_text(json.dumps(data))


def get_held_market_ids(engine) -> set:
    """All market_ids currently held in open positions."""
    held = set()
    for pos in engine.open_positions.values():
        for mid in pos.markets.keys():
            held.add(str(mid))
    return held


def get_held_constraint_ids(engine) -> set:
    """All constraint_ids currently held."""
    held = set()
    for pos in engine.open_positions.values():
        cid = pos.metadata.get('constraint_id', '')
        if not cid:
            parts = pos.opportunity_id.split('_')
            if len(parts) >= 4:
                cid = '_'.join(parts[2:-1])
        if cid:
            held.add(cid)
    return held


def opportunity_overlaps_held(opp_dict, held_market_ids) -> bool:
    """Check if opportunity shares any market_id with held positions."""
    for mid in opp_dict.get('market_ids', []):
        if str(mid) in held_market_ids:
            return True
    return False


def calc_position_liq_value(pos, market_lookup) -> float:
    """Estimate current liquidation value at bid prices (what we'd get to exit)."""
    liq = 0.0
    for mid_str, mkt_info in pos.markets.items():
        entry_p = mkt_info.get('entry_price', 0)
        bet_amt = mkt_info.get('bet_amount', 0)
        if entry_p <= 0:
            continue
        shares = bet_amt / entry_p
        md = market_lookup.get(str(mid_str))
        if md:
            outcome = mkt_info.get('outcome', 'Yes')
            cur_p = md.get_exit_price(outcome)  # bid price if live, else midpoint
        else:
            cur_p = entry_p
        liq += shares * cur_p
    return liq


def get_asset_ids_for_opportunity(opp_dict: dict, market_lookup: dict) -> list:
    """Extract CLOB token_ids for one opportunity's markets."""
    asset_ids = set()
    for mid in opp_dict.get('market_ids', []):
        md = market_lookup.get(str(mid))
        if not md:
            continue
        clob_raw = md.metadata.get('clobTokenIds', '[]') if hasattr(md, 'metadata') else '[]'
        try:
            clob_ids = json.loads(clob_raw) if isinstance(clob_raw, str) else clob_raw
        except (json.JSONDecodeError, TypeError):
            continue
        for tid in (clob_ids or []):
            if tid:
                asset_ids.add(tid)
    return list(asset_ids)


def trigger_weekly_delay_update():
    """Check if weekly delay table update is due, run in background if so."""
    try:
        update_state_path = WORKSPACE / 'data' / 'delay_update_state.json'
        if update_state_path.exists():
            state = json.loads(update_state_path.read_text())
            last = datetime.fromisoformat(state.get('last_update', '2000-01-01'))
            if last.tzinfo is None:
                last = last.replace(tzinfo=timezone.utc)
            if (datetime.now(timezone.utc) - last).days < 7:
                return
        import subprocess
        script = WORKSPACE / 'scripts' / 'debug' / 'update_delay_table.py'
        if script.exists():
            log.info('Weekly delay table update triggered')
            subprocess.Popen(
                [sys.executable, str(script)],
                stdout=open(str(WORKSPACE / 'logs' / 'delay_update.log'), 'a'),
                stderr=subprocess.STDOUT, cwd=str(WORKSPACE)
            )
    except Exception as e:
        log.debug(f'Delay update check error: {e}')


# =============================================================================
# TradingEngine — Event-driven core
# =============================================================================

class TradingEngine:
    """
    Event-driven trading engine combining constraint detection,
    arbitrage math, and execution in a single async process.

    Reacts to WebSocket price changes to re-evaluate specific
    constraint groups instantly rather than polling all constraints.
    """

    def __init__(self, config: dict, secrets: dict):
        self.config = config
        self.secrets = secrets

        # Sub-engines
        self.arb_engine = ArbitrageMathEngine(config, WORKSPACE)
        self.detector = ConstraintDetector(config, WORKSPACE)
        self.paper_engine = PaperTradingEngine(config, WORKSPACE)
        self.live_engine: Optional[LiveTradingEngine] = None
        self.ws_manager: Optional[WebSocketManager] = None

        # Config
        arb_cfg = config.get('arbitrage', {})
        self.max_positions = arb_cfg.get('max_concurrent_positions', 20)
        self.max_days_entry = arb_cfg.get('max_days_to_resolution', 60)
        self.max_days_replace = arb_cfg.get('max_days_to_replacement', 30)
        self.capital_pct = arb_cfg.get('capital_per_trade_pct', 0.10)
        self.replace_on_postponement = arb_cfg.get('replace_on_postponement', True)
        self.postponement_breakeven = arb_cfg.get('postponement_replace_breakeven_only', True)
        self.postponement_rescore_days = arb_cfg.get('postponement_rescore_days', 14)

        res_val_cfg = arb_cfg.get('resolution_validation', {})
        self.resolution_validation_enabled = res_val_cfg.get('enabled', True)
        self.anthropic_api_key = (
            secrets.get('resolution_validation', {}).get('anthropic_api_key', '')
            or os.path.expandvars(res_val_cfg.get('anthropic_api_key', ''))
            or os.environ.get('ANTHROPIC_API_KEY', '')
        )

        live_cfg = config.get('live_trading', {})
        self.live_enabled = live_cfg.get('enabled', False)
        self.shadow_only = live_cfg.get('shadow_only', False)

        # --- Core index: which constraints does each asset affect? ---
        self.market_lookup: Dict[str, MarketData] = {}
        self.constraints: list = []
        self.constraint_by_id: Dict[str, object] = {}   # constraint_id → constraint obj
        self.asset_to_constraints: Dict[str, Set[str]] = {}  # asset_id → {constraint_ids}
        self.constraint_to_assets: Dict[str, Set[str]] = {}  # constraint_id → {asset_ids}
        self.market_to_constraint: Dict[str, Set[str]] = {}  # market_id → {constraint_ids}
        self.asset_to_market: Dict[str, Tuple[str, int]] = {}  # asset_id → (market_id, token_index)
                                                                # token_index: 0=YES, 1=NO

        # --- State ---
        self._running = False
        self._ws_resolved_markets: Set[str] = set()        # condition_ids resolved via WS
        self._last_state_save = 0.0
        self._last_monitor = 0.0
        self._last_replacement = 0.0
        self._iteration = 0

        # Priority queues: urgent (effective fill price drift) processed before background (stale)
        self._urgent_evals: Set[str] = set()
        self._background_evals: Set[str] = set()
        self._last_eval_efp: Dict[str, float] = {}         # asset_id → effective fill price at last eval
        self._constraint_last_eval: Dict[str, float] = {}   # constraint_id → unix time of last eval
        self._constraint_queue_time: Dict[str, float] = {}  # constraint_id → unix time when queued
        self._recent_latencies: list = []                    # last 200 latency_ms values
        self.EFP_DRIFT_THRESHOLD = 0.005                     # $0.005 effective fill price drift → urgent

        # Wake event: WS callbacks set this to instantly wake the eval loop
        # Created in run() because it needs the asyncio event loop
        self._eval_wake: Optional[asyncio.Event] = None

        # Thread pool for CPU-bound work (arb math, constraint detection)
        # Keeps asyncio event loop free for WS heartbeats
        self._executor = ThreadPoolExecutor(max_workers=2, thread_name_prefix='arb')

        # --- Timing config ---
        self.STATE_SAVE_INTERVAL = 30       # seconds
        self.MONITOR_INTERVAL = 30          # seconds
        self.REPLACEMENT_COOLDOWN = 60      # seconds
        self.CONSTRAINT_REBUILD_INTERVAL = 600  # seconds (on new_market batch)
        self._last_constraint_rebuild = 0.0
        self._new_market_buffer: list = []  # buffer new_market events for batch rebuild
        self.MAX_EVALS_PER_BATCH = 500      # Rust arb math = 4µs each; 500 evals = 2ms CPU

    # --- Index Building ---

    def load_markets(self):
        """Load markets from Market Scanner output."""
        if not MARKETS_PATH.exists():
            log.warning('No markets file yet')
            return
        data = json.loads(MARKETS_PATH.read_text())
        markets = [MarketData.from_dict(m) for m in data.get('markets', [])]
        self.market_lookup = {str(m.market_id): m for m in markets}
        log.info(f'Loaded {len(self.market_lookup)} markets')

    def load_and_detect_constraints(self):
        """Run constraint detection inline (replaces old L2 process)."""
        if not self.market_lookup:
            log.warning('Cannot detect constraints: no markets loaded')
            return
        markets = list(self.market_lookup.values())
        self.constraints = self.detector.detect_constraints(markets)
        self.constraint_by_id = {}
        for c in self.constraints:
            self.constraint_by_id[c.constraint_id] = c
        log.info(f'Detected {len(self.constraints)} constraints')
        # Also write to file so dashboard/debug tools can read
        self.detector.save_constraints(CONSTRAINTS_PATH)

    def build_index(self):
        """Build asset_id → constraint_id and asset_id → market_id mappings."""
        self.asset_to_constraints.clear()
        self.constraint_to_assets.clear()
        self.market_to_constraint.clear()
        self.asset_to_market.clear()

        for c in self.constraints:
            cid = c.constraint_id
            asset_ids_for_constraint = set()

            for mid in c.market_ids:
                # Map market_id → constraint_ids
                self.market_to_constraint.setdefault(str(mid), set()).add(cid)

                # Resolve CLOB token_ids for this market
                md = self.market_lookup.get(str(mid))
                if not md:
                    continue
                clob_raw = md.metadata.get('clobTokenIds', '[]') if hasattr(md, 'metadata') else '[]'
                try:
                    clob_ids = json.loads(clob_raw) if isinstance(clob_raw, str) else clob_raw
                except (json.JSONDecodeError, TypeError):
                    continue
                for tid in (clob_ids or []):
                    if tid:
                        self.asset_to_constraints.setdefault(tid, set()).add(cid)
                        asset_ids_for_constraint.add(tid)
                # Reverse lookup: asset_id → (market_id, token_index)
                for idx, tid in enumerate(clob_ids or []):
                    if tid and tid not in self.asset_to_market:
                        self.asset_to_market[tid] = (str(mid), idx)  # 0=YES, 1=NO

            self.constraint_to_assets[cid] = asset_ids_for_constraint

        total_assets = len(self.asset_to_constraints)
        log.info(f'Index built: {len(self.constraints)} constraints, '
                 f'{total_assets} asset_ids mapped, '
                 f'{len(self.market_to_constraint)} market_ids mapped, '
                 f'{len(self.asset_to_market)} asset→market reverse lookups')

    # --- WS Callback Registration ---

    def _register_ws_callbacks(self):
        """Register event handlers on the WebSocket manager."""
        if not self.ws_manager:
            return

        def on_price_change(asset_id: str, best_bid: float, best_ask: float, ts: float):
            """Price level changed — update MarketData, check drift for queueing."""
            self._update_market_price_from_ws(asset_id, best_bid, best_ask)
            self._queue_on_price_change(asset_id, best_bid, best_ask)

        def on_book_update(asset_id: str, local_book):
            """Full book snapshot (triggered by trade) — always urgent, depth changed."""
            self._update_market_price_from_ws(asset_id, local_book.best_bid, local_book.best_ask)
            self._queue_on_book_update(asset_id, local_book.best_bid, local_book.best_ask)

        def on_market_resolved(market_cid: str, asset_id: str):
            """A market resolved — trigger immediate payout check."""
            self._ws_resolved_markets.add(market_cid)
            log.info(f'WS: market_resolved {market_cid[:20]}... — queued')

        def on_new_market(data: dict):
            """A new market was created — buffer for batch constraint rebuild."""
            market_id = data.get('id', '')
            question = data.get('question', '?')[:60]
            log.info(f'WS: new_market id={market_id} "{question}"')
            self._new_market_buffer.append(data)

        def on_trade_confirm(trade_data: dict):
            """Trade confirmed — log for now, wire to fill tracking in live mode."""
            status = trade_data.get('status', '?')
            side = trade_data.get('side', '?')
            size = trade_data.get('size', '?')
            price = trade_data.get('price', '?')
            log.info(f'WS trade: status={status} side={side} size={size} price={price}')

        self.ws_manager.on_price_change(on_price_change)
        self.ws_manager.on_book_update(on_book_update)
        self.ws_manager.on_market_resolved(on_market_resolved)
        self.ws_manager.on_trade_confirm(on_trade_confirm)
        self.ws_manager.on_new_market(on_new_market)

    # --- WS → MarketData price bridge ---

    def _update_market_price_from_ws(self, asset_id: str, best_bid: float, best_ask: float):
        """
        Update MarketData.outcome_bids/outcome_asks in-place from a WS price event.

        Uses asset_to_market reverse lookup:
          asset_id → (market_id, token_index)
          token_index 0 = YES token → update bids/asks for 'Yes'
          token_index 1 = NO token  → update bids/asks for 'No'
        """
        mapping = self.asset_to_market.get(asset_id)
        if not mapping:
            return
        market_id, token_index = mapping
        md = self.market_lookup.get(market_id)
        if not md:
            return

        outcome = 'Yes' if token_index == 0 else 'No'

        # Initialise dicts on first WS update
        if md.outcome_bids is None:
            md.outcome_bids = {}
        if md.outcome_asks is None:
            md.outcome_asks = {}

        if best_bid > 0:
            md.outcome_bids[outcome] = round(best_bid, 6)
        if best_ask > 0:
            md.outcome_asks[outcome] = round(best_ask, 6)

        # Also update legacy outcome_prices with midpoint for backward compat
        if best_bid > 0 and best_ask > 0:
            md.outcome_prices[outcome] = round((best_bid + best_ask) / 2.0, 6)

    def _get_efp(self, asset_id: str) -> float:
        """Get current effective fill price for an asset at our trade size.
        Uses Rust implementation when available (4.5μs vs ~50μs Python).
        Returns 0.0 if no book data or insufficient depth."""
        if not self.ws_manager:
            return 0.0
        book = self.ws_manager.get_book(asset_id)
        if not book or not book.asks:
            return 0.0
        trade_size = dynamic_capital(self.paper_engine.current_capital, self.capital_pct)
        if HAS_RUST:
            ask_prices = [level.price for level in book.asks]
            ask_sizes = [level.size for level in book.asks]
            return rust_arb.effective_fill_price(ask_prices, ask_sizes, trade_size)
        return book.effective_fill_price(trade_size)

    def _queue_on_book_update(self, asset_id: str, best_bid: float, best_ask: float):
        """
        Book event (trade happened, depth changed).
        Uses effective fill price drift to decide urgency — same logic as price_change.
        Book events always carry depth information so EFP is always computable.
        """
        self._queue_by_efp(asset_id)

    def _queue_on_price_change(self, asset_id: str, best_bid: float, best_ask: float):
        """Price level changed — check EFP drift for queueing."""
        self._queue_by_efp(asset_id)

    def _queue_by_efp(self, asset_id: str):
        """
        Unified queue logic for both book and price_change events.
        Computes effective fill price (VWAP at trade size) and compares
        to EFP at last evaluation. This captures both price AND depth
        changes in a single metric.

        Triggers:
          1. EFP drift > threshold from last eval → urgent
          2. >5s since last eval + any new data → background
        OR condition — either triggers.
        """
        affected = self.asset_to_constraints.get(asset_id)
        if not affected:
            return

        current_efp = self._get_efp(asset_id)
        if current_efp <= 0:
            return  # No usable book data

        # Cumulative drift from last evaluation
        last_efp = self._last_eval_efp.get(asset_id, 0.0)
        drift = abs(current_efp - last_efp) if last_efp > 0 else 999.0
        significant = drift >= self.EFP_DRIFT_THRESHOLD

        now = _time.time()
        for cid in affected:
            if significant:
                # Update EFP baseline only when drift triggers queue
                self._last_eval_efp[asset_id] = current_efp
                if cid not in self._constraint_queue_time:
                    self._constraint_queue_time[cid] = now
                self._urgent_evals.add(cid)
                self._background_evals.discard(cid)
                # Wake the eval loop immediately (Phase 8a)
                if self._eval_wake is not None:
                    self._eval_wake.set()
            else:
                last_eval_time = self._constraint_last_eval.get(cid, 0.0)
                if (now - last_eval_time) >= 5.0:
                    if cid not in self._urgent_evals:
                        if cid not in self._constraint_queue_time:
                            self._constraint_queue_time[cid] = now
                        self._background_evals.add(cid)

    # =================================================================
    # Event Processing — the heart of the event-driven engine
    # =================================================================

    def _all_markets_live(self, market_ids: list) -> bool:
        """Check that ALL markets in a group have live WS bid/ask data."""
        for mid in market_ids:
            md = self.market_lookup.get(str(mid))
            if not md or not md.has_live_prices():
                return False
        return True

    def _evaluate_constraint(self, constraint_id: str) -> Optional[dict]:
        """
        Run arb math on a single constraint group.
        Returns opportunity dict if profitable, else None.

        Only evaluates if all markets have live WS prices.
        """
        constraint = self.constraint_by_id.get(constraint_id)
        if not constraint:
            self._constraint_queue_time.pop(constraint_id, None)
            return None

        market_ids = constraint.market_ids
        if not self._all_markets_live(market_ids):
            return None  # Will be re-queued when book events arrive

        # Measure queue→eval latency
        queue_time = self._constraint_queue_time.pop(constraint_id, None)
        if queue_time:
            latency_ms = (_time.time() - queue_time) * 1000
            self._recent_latencies.append(latency_ms)
            if len(self._recent_latencies) > 200:
                self._recent_latencies = self._recent_latencies[-200:]

        # Record eval time (EFP baselines updated at queue time, not here)
        self._constraint_last_eval[constraint_id] = _time.time()

        # Build MarketData list for the arb engine
        markets = []
        for mid in market_ids:
            md = self.market_lookup.get(str(mid))
            if not md:
                return None
            markets.append(md)

        # Run arb math (same engine, now fed live ask prices)
        try:
            opp = self.arb_engine._check_constraint_for_arbitrage(constraint, markets)
            if opp:
                return opp.to_dict()
        except Exception as e:
            log.debug(f'Arb eval error for {constraint_id[:30]}: {e}')

        return None

    def _process_pending_evals(self) -> List[dict]:
        """
        Process constraints from priority queues:
          1. ALL urgent evals first (price drift / book depth change)
          2. Background evals fill remaining batch capacity (stale re-checks)
        Returns list of opportunity dicts.
        """
        if not self._urgent_evals and not self._background_evals:
            return []

        # Build batch: urgent first, background fills remaining
        batch = []
        # Take all urgent (they should never wait)
        urgent_list = list(self._urgent_evals)
        self._urgent_evals.clear()
        batch.extend(urgent_list)

        # Fill remaining capacity with background
        remaining_cap = max(0, self.MAX_EVALS_PER_BATCH - len(batch))
        if remaining_cap > 0 and self._background_evals:
            bg_list = list(self._background_evals)
            bg_batch = bg_list[:remaining_cap]
            bg_remaining = bg_list[remaining_cap:]
            self._background_evals = set(bg_remaining)
            # Don't add if already in urgent batch
            for cid in bg_batch:
                if cid not in set(urgent_list):
                    batch.append(cid)

        if batch:
            n_urgent = len(urgent_list)
            n_bg = len(batch) - n_urgent
            n_bg_remaining = len(self._background_evals)
            log.debug(f'Eval batch: {n_urgent} urgent + {n_bg} background '
                      f'(bg_queue={n_bg_remaining})')

        opportunities = []
        for cid in batch:
            opp = self._evaluate_constraint(cid)
            if opp:
                opportunities.append(opp)

        return opportunities

    def _validate_opportunity(self, opp_dict: dict,
                               max_days: int = None) -> bool:
        """
        Validate an opportunity before entry/replacement:
        1. Not already held (constraint or market overlap)
        2. AI resolution date within max_days
        3. No unrepresented outcomes
        Returns True if valid.
        """
        if max_days is None:
            max_days = self.max_days_entry

        constraint_id = opp_dict.get('constraint_id', '')
        held_cids = get_held_constraint_ids(self.paper_engine)
        held_mids = get_held_market_ids(self.paper_engine)

        # Already held?
        if constraint_id in held_cids:
            return False
        if opportunity_overlaps_held(opp_dict, held_mids):
            return False

        # AI resolution validation
        if self.resolution_validation_enabled and self.anthropic_api_key:
            try:
                market_ids = opp_dict.get('market_ids', [])
                validation = get_full_validation(
                    market_ids=market_ids,
                    market_lookup=self.market_lookup,
                    api_key=self.anthropic_api_key,
                    group_id=constraint_id
                )
                if validation:
                    if validation.get('has_unrepresented_outcome', False):
                        reason = validation.get('unrepresented_outcome_reason', '')[:100]
                        log.info(f'  SKIP (unrepresented outcome): {constraint_id[:30]} — {reason}')
                        return False
                    try:
                        vd = datetime.strptime(
                            validation['latest_resolution_date'], '%Y-%m-%d'
                        ).replace(hour=23, minute=59, second=59, tzinfo=timezone.utc)
                        days_until = (vd - datetime.now(timezone.utc)).days
                        if days_until > max_days:
                            log.debug(f'  SKIP (AI date): {constraint_id[:30]} '
                                      f'resolves in {days_until}d > {max_days}d')
                            return False
                    except (ValueError, KeyError):
                        pass
            except Exception as ve:
                log.warning(f'  Resolution validation error: {ve}')
                # Fail open

        return True

    async def _try_enter_or_replace(self, opportunities: List[dict]):
        """
        Given a list of new opportunities (already arb-verified with live prices):
        1. Rank them by score
        2. If open slots → enter best valid opportunities
        3. If full → compare best new vs worst held, replace if 20% better
        """
        if not opportunities:
            return

        # Rank by score
        ranked = rank_opportunities(
            opportunities, self.market_lookup,
            min_resolution_secs=300,
            max_days_to_resolution=self.max_days_entry
        )
        if not ranked:
            return

        slots = self.max_positions - len(self.paper_engine.open_positions)
        cap = dynamic_capital(self.paper_engine.current_capital, self.capital_pct)
        markets = list(self.market_lookup.values())

        # --- ENTER NEW POSITIONS ---
        entered = 0
        for score, hours, opp_dict in ranked:
            if entered >= max(slots, 0):
                break
            if slots <= 0:
                break
            if not self._validate_opportunity(opp_dict, self.max_days_entry):
                continue

            # Scale to dynamic capital
            opp_dict['total_capital_required'] = cap
            old_cap = sum(opp_dict.get('optimal_bets', {}).values())
            if old_cap > 0:
                scale = cap / old_cap
                opp_dict['optimal_bets'] = {k: v * scale for k, v in opp_dict['optimal_bets'].items()}
                opp_dict['expected_profit'] = opp_dict.get('expected_profit', 0) * scale
                opp_dict['net_profit'] = opp_dict.get('net_profit', 0) * scale
                opp_dict['fees_estimated'] = opp_dict.get('fees_estimated', 0) * scale

            try:
                result = await self.paper_engine.execute_opportunity(opp_dict, markets)
                if result and result.get('success'):
                    cid = opp_dict.get('constraint_id', '')
                    log.info(f'ENTER: {cid[:30]} | ${cap:.2f} | '
                             f'exp ${opp_dict.get("expected_profit",0):.2f} | '
                             f'{hours:.1f}h | score={score:.6f}')
                    entered += 1
                    slots -= 1
                    # Subscribe new assets to WS
                    if self.ws_manager and self.ws_manager._running:
                        new_assets = get_asset_ids_for_opportunity(opp_dict, self.market_lookup)
                        if new_assets:
                            await self.ws_manager.subscribe_assets(new_assets)
            except Exception as e:
                log.error(f'Entry exec error: {e}', exc_info=True)

        # --- REPLACEMENT: if full, compare best new vs worst held ---
        if slots <= 0 and (_time.time() - self._last_replacement) >= self.REPLACEMENT_COOLDOWN:
            # Filter ranked to replacement-eligible (stricter max_days)
            replace_ranked = rank_opportunities(
                opportunities, self.market_lookup,
                min_resolution_secs=300,
                max_days_to_resolution=self.max_days_replace
            )
            if not replace_ranked:
                return

            used_opp_cids = set()
            replacements_made = 0
            max_replacements = 5
            now_utc = datetime.now(timezone.utc)

            while replacements_made < max_replacements:
                held_cids = get_held_constraint_ids(self.paper_engine)
                held_mids = get_held_market_ids(self.paper_engine)

                # Find best untraded opportunity
                best_new = None
                for sc, hr, od in replace_ranked:
                    cid = od.get('constraint_id', '')
                    if cid in held_cids or cid in used_opp_cids:
                        continue
                    if opportunity_overlaps_held(od, held_mids):
                        continue
                    if not self._validate_opportunity(od, self.max_days_replace):
                        continue
                    best_new = (sc, hr, od)
                    break

                if not best_new:
                    break
                best_score, best_hours, best_opp = best_new

                # Score all open positions, find worst
                worst_pos = None
                worst_score = float('inf')
                for pid, pos in self.paper_engine.open_positions.items():
                    # Get validated resolution date
                    pos_end = None
                    cid = pos.metadata.get('constraint_id', '')
                    if cid and self.resolution_validation_enabled:
                        cached = load_resolution_cache(cid)
                        if cached and 'latest_resolution_date' in cached:
                            try:
                                pos_end = datetime.strptime(
                                    cached['latest_resolution_date'], '%Y-%m-%d'
                                ).replace(tzinfo=timezone.utc)
                            except (ValueError, TypeError):
                                pass
                    # Fallback to API end_date
                    if pos_end is None:
                        for mid_str in pos.markets.keys():
                            md = self.market_lookup.get(str(mid_str))
                            if md:
                                ed = md.end_date
                                if ed.tzinfo is None:
                                    ed = ed.replace(tzinfo=timezone.utc)
                                if pos_end is None or ed > pos_end:
                                    pos_end = ed
                    if pos_end is None:
                        continue
                    # Postponement/overdue: extend denominator
                    if pos.metadata.get('postponed') or pos_end < now_utc:
                        rescore_floor = now_utc + timedelta(days=self.postponement_rescore_days)
                        pos_end = max(pos_end, rescore_floor)

                    hours_remaining = (pos_end - now_utc).total_seconds() / 3600
                    # 24h protection
                    if hours_remaining < 24:
                        continue

                    liq_value = calc_position_liq_value(pos, self.market_lookup)
                    unrealized = liq_value - pos.total_capital
                    remaining_upside = pos.expected_profit - unrealized

                    # Apply delay model (same as rank_opportunities)
                    pos_names = [m.get('name', '') for m in pos.markets.values()]
                    pos_mids = list(pos.markets.keys())
                    pos_cat = classify_opportunity_category(pos_names, self.market_lookup, pos_mids)
                    p95_t, def_p95 = load_delay_table()
                    pos_p95 = p95_t.get(pos_cat, def_p95)
                    pos_min_vol = 0.0
                    for mid_str in pos.markets.keys():
                        md = self.market_lookup.get(str(mid_str))
                        if md and hasattr(md, 'volume_24h'):
                            v = md.volume_24h or 0
                            pos_min_vol = v if pos_min_vol == 0 else min(pos_min_vol, v)
                    eff_remaining = hours_remaining + pos_p95 + get_volume_penalty_hours(pos_min_vol)
                    rem_score = (remaining_upside / max(pos.total_capital, 0.01)) / max(eff_remaining, 0.01)
                    if rem_score < worst_score:
                        worst_score = rem_score
                        worst_pos = (pid, pos, rem_score, hours_remaining, unrealized)

                # Replace if best new is 20% better than worst held
                if worst_pos and best_score > worst_score * 1.2:
                    wpid, wpos, wscore, whours, wpnl = worst_pos
                    wname = list(wpos.markets.values())[0].get('name', '?')[:40] if wpos.markets else '?'
                    bname = best_opp.get('market_names', ['?'])[0][:40]
                    log.info(f'REPLACE: "{wname}" (score={wscore:.6f}, {whours:.0f}h)')
                    log.info(f'  WITH: "{bname}" (score={best_score:.6f}, {best_hours:.0f}h)')

                    result = await self.paper_engine.liquidate_position(wpid, self.market_lookup)
                    if result.get('success'):
                        log.info(f'  Liquidated: freed ${result["freed_capital"]:.2f}, '
                                 f'realized ${result["actual_profit"]:+.2f}')
                        replacements_made += 1
                        used_opp_cids.add(best_opp.get('constraint_id', ''))
                    else:
                        log.warning(f'  Liquidation failed: {result.get("reason")}')
                        break
                else:
                    break  # No more profitable swaps

            if replacements_made > 0:
                self._last_replacement = _time.time()
                log.info(f'Replacement round: {replacements_made} swaps')

    # =================================================================
    # Main Run Loop
    # =================================================================

    async def run(self):
        """
        Main entry point. Startup sequence then event-driven loop.

        Startup:
          1. Load execution state
          2. Load markets from Market Scanner output
          3. Run constraint detection (inline)
          4. Build asset→constraint index
          5. Init live engine (if configured)
          6. Start WS manager + subscribe all constraint assets
          7. Enter event loop

        Event loop runs until stopped.
        """
        log.info('Trading Engine starting...')
        self._running = True
        write_status('starting')

        # 1. Load execution state
        if EXEC_STATE.exists():
            try:
                self.paper_engine.load_state(EXEC_STATE)
                log.info(f'State loaded: ${self.paper_engine.current_capital:.2f} capital, '
                         f'{len(self.paper_engine.open_positions)} positions')
            except Exception as e:
                log.warning(f'Could not load state: {e}')

        # 2. Wait for markets (Market Scanner must have run at least once)
        log.info('Waiting for Market Scanner output...')
        while self._running and not MARKETS_PATH.exists():
            await asyncio.sleep(5)
        if not self._running:
            return
        self.load_markets()

        # 3. Constraint detection (inline, replaces old L2) — runs in thread pool
        log.info('Running constraint detection (threaded)...')
        loop = asyncio.get_event_loop()
        await loop.run_in_executor(self._executor, self.load_and_detect_constraints)

        # 4. Build index (threaded)
        await loop.run_in_executor(self._executor, self.build_index)

        # 5. Init live engine
        if self.live_enabled:
            try:
                self.live_engine = LiveTradingEngine(self.config, WORKSPACE)
                health = self.live_engine.health_check()
                if health['healthy']:
                    log.info(f'Live engine: balance=${health["balance_usd"]:.2f}')
                else:
                    log.error(f'Live engine unhealthy: {health.get("error")}')
                    self.live_engine = None
            except Exception as e:
                log.error(f'Live engine init failed: {e}')
                self.live_engine = None

        # 6. Start WS manager + subscribe
        try:
            self.ws_manager = WebSocketManager(config=self.config, secrets=self.secrets)

            # Wire user channel auth from live engine creds
            if self.live_engine and hasattr(self.live_engine, 'client'):
                try:
                    creds = self.live_engine.client.get_api_creds()
                    if creds and creds.api_key:
                        self.ws_manager._user_auth = {
                            'apiKey': creds.api_key,
                            'secret': creds.api_secret,
                            'passphrase': creds.api_passphrase,
                        }
                        log.info(f'WS user auth from live engine (key={creds.api_key[:8]}...)')
                except Exception:
                    pass

            self._register_ws_callbacks()

            # Subscribe all constraint group assets
            all_assets = list(self.asset_to_constraints.keys())
            if all_assets:
                await self.ws_manager.subscribe_assets(all_assets)
                log.info(f'WS: pre-subscribed {len(all_assets)} assets from index')

            await self.ws_manager.start()
            log.info('WebSocket manager started')
        except Exception as e:
            log.warning(f'WS manager failed: {e} — running without live prices')
            self.ws_manager = None

        # Weekly delay table update
        trigger_weekly_delay_update()

        mode_str = 'shadow' if self.shadow_only else ('live' if self.live_engine else 'paper')
        log.info(f'Trading Engine ready [{mode_str.upper()}] — entering event loop')
        write_status('running', self.paper_engine.current_capital,
                     len(self.paper_engine.open_positions))

        # Phase 8a: Event-based eval wake (replaces asyncio.sleep(1.0))
        self._eval_wake = asyncio.Event()

        # =============================================================
        # EVENT LOOP — process WS-triggered evaluations + periodic tasks
        # =============================================================
        while self._running:
            self._iteration += 1
            now = _time.time()

            try:
                # --- Process WS-triggered constraint evaluations (threaded) ---
                opportunities = await loop.run_in_executor(
                    self._executor, self._process_pending_evals)
                if opportunities:
                    # Write opportunities to file (dashboard + debug)
                    from layer3_arbitrage_math.arbitrage_engine import ArbitrageOpportunity
                    OPP_PATH.parent.mkdir(parents=True, exist_ok=True)
                    OPP_PATH.write_text(json.dumps({
                        'opportunities': opportunities,
                        'count': len(opportunities),
                        'timestamp': datetime.now(ZoneInfo('Europe/London')).isoformat()
                    }, indent=2))

                    await self._try_enter_or_replace(opportunities)

                # --- Monitor open positions (periodic) ---
                if (now - self._last_monitor) >= self.MONITOR_INTERVAL:
                    if self.paper_engine.open_positions:
                        markets_list = list(self.market_lookup.values())
                        await self.paper_engine.monitor_positions(markets_list)
                    self._last_monitor = now

                # --- Handle new_market buffer (batch constraint rebuild) ---
                if (self._new_market_buffer
                        and (now - self._last_constraint_rebuild) >= self.CONSTRAINT_REBUILD_INTERVAL):
                    count = len(self._new_market_buffer)
                    self._new_market_buffer.clear()
                    log.info(f'Rebuilding constraints ({count} new markets buffered, threaded)')
                    # Reload markets + detect constraints in thread pool
                    def _rebuild():
                        self.load_markets()
                        self.load_and_detect_constraints()
                        self.build_index()
                    await loop.run_in_executor(self._executor, _rebuild)
                    # Re-subscribe all assets
                    if self.ws_manager and self.ws_manager._running:
                        all_assets = list(self.asset_to_constraints.keys())
                        if all_assets:
                            await self.ws_manager.subscribe_assets(all_assets)
                    self._last_constraint_rebuild = now

                # --- WS resolved markets → force monitor pass ---
                if self._ws_resolved_markets:
                    resolved = list(self._ws_resolved_markets)
                    self._ws_resolved_markets.clear()
                    log.info(f'WS resolution trigger: {len(resolved)} markets')
                    if self.paper_engine.open_positions:
                        markets_list = list(self.market_lookup.values())
                        await self.paper_engine.monitor_positions(markets_list)

                # --- Save state (periodic) ---
                if (now - self._last_state_save) >= self.STATE_SAVE_INTERVAL:
                    STATE_DIR.mkdir(parents=True, exist_ok=True)
                    self.paper_engine.save_state(EXEC_STATE)
                    self._last_state_save = now

                    cap = self.paper_engine.current_capital
                    npos = len(self.paper_engine.open_positions)
                    write_status('running', cap, npos)

                # --- Stats logging (every ~30s) ---
                if (now - getattr(self, '_last_stats_log', 0)) >= 30.0:
                    ws_info = ''
                    engine_metrics = {
                        'iteration': self._iteration,
                        'has_rust': HAS_RUST,
                        'constraints': len(self.constraints) if self.constraints else 0,
                        'markets_total': len(self.market_lookup),
                    }
                    if self.ws_manager and self.ws_manager._running:
                        ws_stats = self.ws_manager.get_stats()
                        n_urgent = len(self._urgent_evals)
                        n_bg = len(self._background_evals)
                        live_count = sum(1 for m in self.market_lookup.values() if m.has_live_prices())
                        # Latency percentiles from recent evals
                        lat_info = ''
                        lat_p50 = lat_p95 = lat_max = 0
                        if self._recent_latencies:
                            lats = sorted(self._recent_latencies)
                            lat_p50 = lats[len(lats) // 2]
                            lat_p95 = lats[int(len(lats) * 0.95)]
                            lat_max = lats[-1]
                            lat_info = f' lat_ms p50={lat_p50:.0f} p95={lat_p95:.0f} max={lat_max:.0f}'
                        ws_info = (f' | WS: subs={len(self.ws_manager._subscribed_assets)} '
                                   f'msgs={ws_stats["market_msgs"]} '
                                   f'live={live_count}/{len(self.market_lookup)} '
                                   f'urgent={n_urgent} bg={n_bg}{lat_info}')
                        engine_metrics.update({
                            'ws_subscribed': len(self.ws_manager._subscribed_assets),
                            'ws_msgs': ws_stats.get('market_msgs', 0),
                            'ws_live': live_count,
                            'queue_urgent': n_urgent,
                            'queue_background': n_bg,
                            'lat_p50_ms': round(lat_p50),
                            'lat_p95_ms': round(lat_p95),
                            'lat_max_ms': round(lat_max),
                        })
                    cap = self.paper_engine.current_capital
                    npos = len(self.paper_engine.open_positions)
                    log.info(f'[iter {self._iteration}] Capital=${cap:.2f} '
                             f'positions={npos}{ws_info}')
                    write_status('running', cap, npos, engine_metrics=engine_metrics)
                    self._last_stats_log = now

                # --- Weekly delay update ---
                if (now - getattr(self, '_last_delay_update', 0)) >= 86400.0:
                    trigger_weekly_delay_update()
                    self._last_delay_update = now

            except Exception as e:
                log.error(f'[iter {self._iteration}] Error: {e}', exc_info=True)
                write_status('error', 0, 0, str(e))

            # Phase 8a: Event-based wake — instant processing of urgent evals
            # WS callbacks set _eval_wake when urgent drift detected.
            # Falls back to 50ms timeout for background processing.
            self._eval_wake.clear()
            try:
                await asyncio.wait_for(self._eval_wake.wait(), timeout=0.05)
            except asyncio.TimeoutError:
                pass  # Normal: no urgent evals, process background on next iter

        # --- Shutdown ---
        log.info('Trading Engine shutting down...')
        if self.ws_manager:
            await self.ws_manager.stop()
        STATE_DIR.mkdir(parents=True, exist_ok=True)
        self.paper_engine.save_state(EXEC_STATE)
        write_status('stopped')
        log.info('Trading Engine stopped')

    def stop(self):
        """Signal the engine to stop."""
        self._running = False


# =============================================================================
# Entry point
# =============================================================================

async def main():
    with open(CONFIG_PATH) as f:
        config = yaml.safe_load(f)
    secrets = {}
    if SECRETS_PATH.exists():
        with open(SECRETS_PATH) as f:
            secrets = yaml.safe_load(f) or {}

    engine = TradingEngine(config, secrets)

    # Handle signals
    def handle_signal(signum, frame):
        log.info(f'Signal {signum} received, stopping...')
        engine.stop()

    signal.signal(signal.SIGTERM, handle_signal)
    signal.signal(signal.SIGINT, handle_signal)

    await engine.run()


if __name__ == '__main__':
    asyncio.run(main())
