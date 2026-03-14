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

sys.path.append(str(Path(__file__).parent.parent))  # project root
from trading.paper_trading import PaperTradingEngine
from trading.live_trading import LiveTradingEngine
from market_data.market_data import MarketData
from constraint_detection.constraint_detector import ConstraintDetector
from arbitrage_math.arbitrage_engine import ArbitrageMathEngine
from utilities.resolution_validator import (
    get_validated_resolution_date, get_full_validation,
    _load_cache as load_resolution_cache
)
from utilities.postponement_detector import check_postponement
from trading.websocket_manager import (
    WebSocketManager, get_asset_ids_for_constraints,
    get_condition_ids_for_positions
)

try:
    import rust_arb
    HAS_RUST = True
except ImportError:
    HAS_RUST = False

try:
    import rust_engine
    HAS_RUST_WS = True  # Phase 8 P4a: Rust WS engine with ABBA-safe single-lock queue
except ImportError:
    HAS_RUST_WS = False

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
CONSTRAINTS_PATH = WORKSPACE / 'constraint_detection' / 'data' / 'latest_constraints.json'
OPP_PATH     = WORKSPACE / 'arbitrage_math' / 'data' / 'latest_opportunities.json'
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
        if md.yes_asset_id:
            asset_ids.add(md.yes_asset_id)
        if md.no_asset_id:
            asset_ids.add(md.no_asset_id)
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
        self.rust_ws = None  # rust_engine.RustWsEngine (Phase 8 P4a)
        self.rust_pm = None  # rust_engine.RustPositionManager (Phase 8q-2)

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
        self.live_enabled = True  # Always enabled — paper mode retired, shadow is minimum
        self.shadow_only = live_cfg.get('shadow_only', True)  # Default shadow if not specified

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
        self._recent_latencies: list = []                    # last 200 latency_μs values (batch eval time)
        self.EFP_DRIFT_THRESHOLD = 0.005                     # $0.005 effective fill price drift → urgent

        # Phase 8j: buffer dirty assets from WS callbacks, batch-process in eval loop
        # Reduces per-event Python overhead from ~10 ops to 1 (set.add)
        self._dirty_assets: Set[str] = set()
        self._last_stale_sweep = 0.0  # Phase 8l: periodic stale re-subscribe

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

        # --- AI config (for resolution validator + postponement detector) ---
        self.ai_config = config.get('ai', {})
        self._last_postponement_check = 0.0
        self.POSTPONEMENT_CHECK_INTERVAL = 3600  # seconds (1 hour — actual per-position throttle is in detector cache)

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
                for tid in (md.yes_asset_id, md.no_asset_id):
                    if tid:
                        self.asset_to_constraints.setdefault(tid, set()).add(cid)
                        asset_ids_for_constraint.add(tid)
                # Reverse lookup: asset_id → (market_id, token_index)
                if md.yes_asset_id and md.yes_asset_id not in self.asset_to_market:
                    self.asset_to_market[md.yes_asset_id] = (str(mid), 0)  # 0=YES
                if md.no_asset_id and md.no_asset_id not in self.asset_to_market:
                    self.asset_to_market[md.no_asset_id] = (str(mid), 1)  # 1=NO

            self.constraint_to_assets[cid] = asset_ids_for_constraint

        total_assets = len(self.asset_to_constraints)
        log.info(f'Index built: {len(self.constraints)} constraints, '
                 f'{total_assets} asset_ids mapped, '
                 f'{len(self.market_to_constraint)} market_ids mapped, '
                 f'{len(self.asset_to_market)} asset→market reverse lookups')

    def _load_constraints_into_rust(self):
        """Build constraint list with full market refs and load into Rust evaluator."""
        if not self.rust_ws:
            return
        rust_constraints = []
        for c in self.constraints:
            markets = []
            for mid in c.market_ids:
                md = self.market_lookup.get(str(mid))
                if not md:
                    continue
                name = md.question[:80] if hasattr(md, 'question') else str(mid)
                if md.yes_asset_id:
                    markets.append({
                        'market_id': str(mid),
                        'yes_asset_id': md.yes_asset_id,
                        'no_asset_id': md.no_asset_id,
                        'name': name,
                    })
            if len(markets) >= 2:
                neg_risk_id = c.metadata.get('negRiskMarketID', '') if hasattr(c, 'metadata') else ''
                # Find earliest end_date across markets in this constraint
                end_ts = 0.0
                for mid in c.market_ids:
                    md = self.market_lookup.get(str(mid))
                    if md and hasattr(md, 'end_date') and md.end_date:
                        try:
                            edt = datetime.fromisoformat(str(md.end_date))
                            if edt.tzinfo is None:
                                edt = edt.replace(tzinfo=timezone.utc)
                            ts = edt.timestamp()
                            if end_ts == 0 or ts < end_ts:
                                end_ts = ts
                        except (ValueError, TypeError):
                            pass
                rust_constraints.append({
                    'constraint_id': c.constraint_id,
                    'constraint_type': c.relationship_type.value,
                    'is_neg_risk': bool(neg_risk_id),
                    'implications': [],
                    'markets': markets,
                    'end_date_ts': end_ts,
                })
        self.rust_ws.set_constraints(rust_constraints)
        # Also set eval config with current capital
        cap = dynamic_capital(self._pm_capital(), self.capital_pct)
        arb_cfg = self.config.get('arbitrage', {})
        self.rust_ws.set_eval_config(
            cap,
            arb_cfg.get('fees', {}).get('polymarket_taker_fee', 0.0001),
            arb_cfg.get('min_profit_threshold', 0.03),
            arb_cfg.get('max_profit_threshold', 0.30),
        )
        log.info(f'Loaded {len(rust_constraints)} constraints into Rust evaluator (capital=${cap:.2f})')

    # --- WS Callback Registration ---

    # --- Rust PM helpers (Phase 8q-2: fall back to paper_engine if Rust unavailable) ---

    def _pm_capital(self) -> float:
        if self.rust_pm:
            return self.rust_pm.current_capital()
        return self.paper_engine.current_capital

    def _pm_open_count(self) -> int:
        if self.rust_pm:
            return self.rust_pm.open_count()
        return len(self.paper_engine.open_positions)

    def _pm_held_cids(self) -> set:
        if self.rust_pm:
            return self.rust_pm.get_held_constraint_ids()
        return get_held_constraint_ids(self.paper_engine)

    def _pm_held_mids(self) -> set:
        if self.rust_pm:
            return self.rust_pm.get_held_market_ids()
        return get_held_market_ids(self.paper_engine)

    def _pm_enter(self, opp_dict: dict) -> dict:
        """Enter position via Rust PM. Returns {'ok': bool, 'position_id': str, ...}."""
        if not self.rust_pm:
            return {'success': False, 'reason': 'no_rust_pm'}
        meta = opp_dict.get('metadata', {})
        method = meta.get('method', opp_dict.get('method', 'unknown'))
        strategy = meta.get('strategy', 'arb_sell' if 'sell' in method else 'arb_buy')
        is_sell = 'sell' in method.lower()
        result = self.rust_pm.enter_position(
            opp_dict.get('opportunity_id', opp_dict.get('constraint_id', '')),
            opp_dict.get('constraint_id', ''),
            strategy, method,
            opp_dict.get('market_ids', []),
            opp_dict.get('market_names', []),
            opp_dict.get('current_prices', {}),
            opp_dict.get('current_no_prices', {}),
            opp_dict.get('optimal_bets', {}),
            opp_dict.get('expected_profit', 0),
            opp_dict.get('expected_profit_pct', 0),
            is_sell,
        )
        if result.get('ok'):
            return {'success': True, 'position_id': result['position_id']}
        return {'success': False, 'reason': result.get('reason', 'unknown')}

    def _pm_get_current_bids(self, position_json: str) -> dict:
        """Get current bid prices for a position's markets from the Rust book mirror."""
        pos = json.loads(position_json) if isinstance(position_json, str) else position_json
        markets = pos.get('markets', {})
        method = pos.get('metadata', {}).get('method', '')
        is_sell = 'sell' in method.lower()
        bids = {}
        for mid, leg in markets.items():
            md = self.market_lookup.get(str(mid))
            if md:
                if is_sell:
                    # Sell arb holds NO shares — need NO bid
                    bids[mid] = md.get_exit_price('No') if hasattr(md, 'get_exit_price') else (1.0 - md.outcome_prices.get('Yes', 0.5))
                else:
                    # Buy arb holds YES shares — need YES bid
                    bids[mid] = md.get_exit_price('Yes') if hasattr(md, 'get_exit_price') else md.outcome_prices.get('Yes', 0.5)
            else:
                bids[mid] = leg.get('entry_price', 0.5)
        return bids

    def _build_market_prices(self) -> dict:
        """Build market_id → {outcome → price} dict for Rust check_resolutions.
        Reads current prices from market_lookup (API snapshot)."""
        market_prices = {}
        for mid, md in self.market_lookup.items():
            prices = {}
            if hasattr(md, 'outcome_prices') and md.outcome_prices:
                for outcome, price in md.outcome_prices.items():
                    prices[outcome] = float(price)
            elif hasattr(md, 'outcomes') and md.outcomes:
                for outcome in md.outcomes:
                    p = getattr(md, f'{outcome.lower()}_price', 0.0)
                    if p:
                        prices[outcome] = float(p)
            if prices:
                market_prices[str(mid)] = prices
        return market_prices

    def _pm_liquidate(self, position_id: str, reason: str) -> dict:
        """Liquidate via Rust PM (sells shares at current bids)."""
        if self.rust_pm:
            # Get position data to build bids
            open_jsons = self.rust_pm.get_open_positions_json()
            pos_data = None
            for j in open_jsons:
                p = json.loads(j)
                if p.get('position_id') == position_id:
                    pos_data = p
                    break
            if not pos_data:
                return {'success': False, 'reason': 'position_not_found'}
            bids = self._pm_get_current_bids(pos_data)
            result = self.rust_pm.liquidate_position(position_id, reason, bids)
            if result:
                net_proceeds, profit = result
                return {'success': True, 'freed_capital': net_proceeds, 'actual_profit': profit}
            return {'success': False, 'reason': 'liquidation_failed'}
        return {'success': False, 'reason': 'no_rust_pm'}

    def _save_state(self):
        """Save state from rust_pm or paper_engine. SQLite is the sole source of truth."""
        import time as _t
        t0 = _t.time()
        if self.rust_pm and self.paper_engine._state_store:
            store = self.paper_engine._state_store
            cap = self.rust_pm.current_capital()
            init_cap = self.rust_pm.initial_capital()
            perf = self.rust_pm.get_performance_metrics()
            # Sync scalars
            store.save_scalars({
                'current_capital': cap,
                'initial_capital': init_cap,
            })
            for k in ('total_trades', 'winning_trades', 'losing_trades',
                      'total_actual_profit', 'total_expected_profit'):
                if k in perf:
                    store.save_scalar(k, perf[k])
            # Sync open positions — full replace
            open_jsons = self.rust_pm.get_open_positions_json()
            open_dicts = [json.loads(j) for j in open_jsons]
            db_open_ids = store.get_open_position_ids() if hasattr(store, 'get_open_position_ids') else set()
            live_ids = {d.get('position_id', '') for d in open_dicts}
            for stale_id in (db_open_ids - live_ids):
                store.delete_position(stale_id)
            if open_dicts:
                store.upsert_positions_bulk(open_dicts, 'open')
            # Sync closed — incremental
            counts = store.count_by_status()
            db_closed = counts.get('closed', 0)
            all_closed_jsons = self.rust_pm.get_closed_positions_json()
            all_closed_dicts = [json.loads(j) for j in all_closed_jsons]
            if len(all_closed_dicts) > db_closed:
                store.upsert_positions_bulk(all_closed_dicts[db_closed:], 'closed')
            store.backup_to_disk()

            # Keep paper_engine in sync (monitoring + dashboard helpers still read it)
            self.paper_engine.current_capital = cap
            self.paper_engine.initial_capital = init_cap

            ms = (_t.time() - t0) * 1000
            log.info(f'Saved state (Rust PM → SQLite): {len(open_dicts)} open, '
                     f'{len(all_closed_dicts)} closed [{ms:.0f}ms]')
        else:
            self.paper_engine.save_state(EXEC_STATE)

    def _check_proactive_exits(self):
        """Check all open positions for proactive exit (sell now ≥ 1.2× resolution payout)."""
        if not self.rust_pm or self.rust_pm.open_count() == 0:
            return
        # Build bids dict for all open position markets
        all_bids = {}
        for pos_json in self.rust_pm.get_open_positions_json():
            bids = self._pm_get_current_bids(pos_json)
            all_bids.update(bids)
        exits = self.rust_pm.check_proactive_exits(all_bids, 1.2)
        for exit_info in exits:
            pid = exit_info['position_id']
            ratio = exit_info['ratio']
            net = exit_info['net_proceeds']
            res_pay = exit_info['resolution_payout']
            log.info(f'PROACTIVE EXIT: {pid[:40]}... '
                     f'ratio={ratio:.3f} (sell=${net:.2f} vs resolve=${res_pay:.2f})')
            result = self._pm_liquidate(pid, 'proactive_exit')
            if result.get('success'):
                log.info(f'  Sold: freed ${result["freed_capital"]:.2f}, '
                         f'profit=${result["actual_profit"]:+.2f}')
            else:
                log.warning(f'  Proactive exit failed: {result.get("reason")}')

    def _register_ws_callbacks(self):
        """Register event handlers on the WebSocket manager."""
        if not self.ws_manager:
            return

        def on_price_change(asset_id: str, best_bid: float, best_ask: float, ts: float):
            """Price level changed — update MarketData, buffer for batch queue processing."""
            self._update_market_price_from_ws(asset_id, best_bid, best_ask)
            self._dirty_assets.add(asset_id)  # Phase 8j: 1 Python op instead of ~10

        def on_book_update(asset_id: str, local_book):
            """Full book snapshot — update MarketData, buffer for batch queue processing."""
            self._update_market_price_from_ws(asset_id, local_book.best_bid, local_book.best_ask)
            self._dirty_assets.add(asset_id)  # Phase 8j: 1 Python op instead of ~10

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
        trade_size = dynamic_capital(self._pm_capital(), self.capital_pct)
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

    def _process_dirty_assets(self):
        """
        Phase 8j: Batch-process all buffered dirty assets from WS callbacks.
        Called once per eval loop iteration instead of per-WS-event.
        
        1. Snapshot and clear dirty set (atomic-ish)
        2. Collect book data for all dirty assets
        3. Batch compute EFPs in one Rust call
        4. Run queue decision logic
        """
        if not self._dirty_assets:
            return

        # Snapshot + clear
        dirty = list(self._dirty_assets)
        self._dirty_assets.clear()

        # Collect book data and filter to assets with constraint mappings
        asset_ids_with_books = []
        all_ask_prices = []
        all_ask_sizes = []
        trade_size = dynamic_capital(self._pm_capital(), self.capital_pct)

        for asset_id in dirty:
            if asset_id not in self.asset_to_constraints:
                continue
            if not self.ws_manager:
                continue
            book = self.ws_manager.get_book(asset_id)
            if not book or not book.asks:
                continue
            asset_ids_with_books.append(asset_id)
            all_ask_prices.append([level.price for level in book.asks])
            all_ask_sizes.append([level.size for level in book.asks])

        if not asset_ids_with_books:
            return

        # Batch EFP via Rust (1 PyO3 crossing for all assets)
        if HAS_RUST:
            efps = rust_arb.batch_effective_fill_prices(
                all_ask_prices, all_ask_sizes, trade_size)
        else:
            efps = []
            for i in range(len(asset_ids_with_books)):
                book = self.ws_manager.get_book(asset_ids_with_books[i])
                efps.append(book.effective_fill_price(trade_size) if book else 0.0)

        # Queue decisions (same logic as _queue_by_efp but batched)
        now = _time.time()
        wake_needed = False
        for i, asset_id in enumerate(asset_ids_with_books):
            current_efp = efps[i]
            if current_efp <= 0:
                continue

            last_efp = self._last_eval_efp.get(asset_id, 0.0)
            drift = abs(current_efp - last_efp) if last_efp > 0 else 999.0
            significant = drift >= self.EFP_DRIFT_THRESHOLD

            for cid in self.asset_to_constraints.get(asset_id, ()):
                if significant:
                    self._last_eval_efp[asset_id] = current_efp
                    if cid not in self._constraint_queue_time:
                        self._constraint_queue_time[cid] = now
                    self._urgent_evals.add(cid)
                    self._background_evals.discard(cid)
                    wake_needed = True
                else:
                    last_eval_time = self._constraint_last_eval.get(cid, 0.0)
                    if (now - last_eval_time) >= 5.0:
                        if cid not in self._urgent_evals:
                            if cid not in self._constraint_queue_time:
                                self._constraint_queue_time[cid] = now
                            self._background_evals.add(cid)

        if wake_needed and self._eval_wake is not None:
            self._eval_wake.set()

    # =================================================================
    # Event Processing — the heart of the event-driven engine
    # =================================================================

    def _sync_rust_prices_for_batch(self, constraint_ids: list):
        """
        Sync Rust book mirror prices → Python MarketData objects for a batch of constraints.
        Called before arb math evaluation when using Rust WS engine.
        
        For each constraint's markets, reads best_ask/best_bid from Rust and updates
        the MarketData outcome_asks/outcome_bids in-place — same effect as
        _update_market_price_from_ws() but pulling from Rust instead of Python WS callbacks.
        """
        if not self.rust_ws:
            return
        seen_assets = set()
        for cid in constraint_ids:
            for asset_id in self.constraint_to_assets.get(cid, ()):
                if asset_id in seen_assets:
                    continue
                seen_assets.add(asset_id)
                mapping = self.asset_to_market.get(asset_id)
                if not mapping:
                    continue
                market_id, token_index = mapping
                md = self.market_lookup.get(market_id)
                if not md:
                    continue
                best_bid = self.rust_ws.get_best_bid(asset_id)
                best_ask = self.rust_ws.get_best_ask(asset_id)
                if best_ask <= 0 and best_bid <= 0:
                    continue
                if token_index == 0:  # YES token
                    if best_ask > 0:
                        md.outcome_asks = md.outcome_asks or {}
                        md.outcome_asks['Yes'] = best_ask
                    if best_bid > 0:
                        md.outcome_bids = md.outcome_bids or {}
                        md.outcome_bids['Yes'] = best_bid
                    md._has_live_prices = True
                elif token_index == 1:  # NO token
                    if best_ask > 0:
                        md.outcome_asks = md.outcome_asks or {}
                        md.outcome_asks['No'] = best_ask
                    if best_bid > 0:
                        md.outcome_bids = md.outcome_bids or {}
                        md.outcome_bids['No'] = best_bid
                    md._has_live_prices = True

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
            self._recent_latencies.append(latency_ms * 1000)  # ms → μs
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
          - Rust WS mode: drain evals from Rust queue, sync prices, then evaluate
          - Python WS mode: batch-process dirty assets, then drain Python queues
        Returns list of opportunity dicts.
        """
        # === Rust WS path: full eval pipeline in Rust (Phase 8 P4c) ===
        if self.rust_ws:
            # Update capital for Rust evaluator (changes as positions open/close)
            cap = dynamic_capital(self._pm_capital(), self.capital_pct)
            arb_cfg = self.config.get('arbitrage', {})
            self.rust_ws.set_eval_config(
                cap,
                arb_cfg.get('fees', {}).get('polymarket_taker_fee', 0.0001),
                arb_cfg.get('min_profit_threshold', 0.03),
                arb_cfg.get('max_profit_threshold', 0.30),
            )

            # Pass held positions so Rust skips them (zero-cost vs Python post-filter)
            held_cids = self._pm_held_cids()
            held_mids = self._pm_held_mids()

            t0 = _time.time()
            result = self.rust_ws.evaluate_batch(
                self.MAX_EVALS_PER_BATCH,
                held_cids=held_cids,
                held_mids=held_mids,
                top_n=20,
            )
            batch_ms = (_time.time() - t0) * 1000
            n_eval = result['n_evaluated']
            if n_eval == 0:
                return []

            # Record batch latency in μs for dashboard stats
            self._recent_latencies.append(batch_ms * 1000)  # ms → μs
            if len(self._recent_latencies) > 200:
                self._recent_latencies = self._recent_latencies[-200:]

            n_urgent = result['n_urgent']
            n_bg = result['n_background']
            n_held = result.get('n_skipped_held', 0)
            n_opps = len(result.get('opportunities', []))
            log.debug(f'Rust eval batch: {n_urgent} urgent + {n_bg} background (held={n_held}) opps={n_opps}')

            # Opportunities are pre-ranked by Rust (score = profit_pct / hours)
            opportunities = list(result['opportunities'])
            return opportunities

        # === Python WS path (fallback) ===
        # Phase 8j: batch-process all WS-buffered dirty assets into queue
        self._process_dirty_assets()

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
        held_cids = self._pm_held_cids()
        held_mids = self._pm_held_mids()

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

    def _check_postponements(self):
        """Check all open positions for overdue events and search for rescheduled dates.
        
        Runs in thread pool (blocking HTTP calls). Per-position caching in
        postponement_detector prevents redundant API calls.
        """
        pp_cfg = self.ai_config.get('postponement', {})
        overdue_threshold_h = pp_cfg.get('overdue_threshold_hours', 24)
        now = datetime.now(timezone.utc)
        checked = 0

        # A1: Iterate Rust PM positions (JSON dicts) instead of paper_engine
        if self.rust_pm:
            open_positions = [json.loads(j) for j in self.rust_pm.get_open_positions_json()]
        else:
            open_positions = [pos.to_dict() if hasattr(pos, 'to_dict') else pos
                              for pos in self.paper_engine.open_positions.values()]
        for pos_dict in open_positions:
            # Get expected resolution date from position metadata or market end_date
            expected_date_str = None
            meta = pos_dict.get('metadata', {})

            # Try AI-validated date first, then raw end_date
            for mid in pos_dict.get('markets', {}):
                md = self.market_lookup.get(str(mid))
                if md and hasattr(md, 'metadata') and md.metadata:
                    ed = md.metadata.get('end_date', '')
                    if ed:
                        expected_date_str = ed[:10]  # YYYY-MM-DD
                        break

            if not expected_date_str:
                continue

            # Check if overdue
            try:
                expected_dt = datetime.strptime(expected_date_str, '%Y-%m-%d').replace(
                    tzinfo=timezone.utc)
                hours_overdue = (now - expected_dt).total_seconds() / 3600
            except Exception:
                continue

            if hours_overdue < overdue_threshold_h:
                continue

            # Position is overdue — check for postponement
            pos_id = pos_dict.get('position_id', '')
            market_names = [m.get('name', '?') for m in pos_dict.get('markets', {}).values()]

            result = check_postponement(
                position_id=pos_id,
                market_names=market_names,
                original_date=expected_date_str,
                ai_config=self.ai_config,
            )
            checked += 1

            if result and result.get('effective_resolution_date'):
                # Store in position metadata for replacement scoring
                if hasattr(pos, 'metadata') and isinstance(pos.metadata, dict):
                    pos.metadata['postponement'] = {
                        'status': result.get('status'),
                        'new_date': result.get('new_date'),
                        'effective_date': result.get('effective_resolution_date'),
                        'confidence': result.get('date_confidence'),
                        'reason': result.get('reason', ''),
                        'checked_at': result.get('checked_at'),
                    }
                    log.info(f'Postponement detected: {market_names[0][:40]}... '
                             f'→ {result.get("effective_resolution_date")} '
                             f'({result.get("date_confidence")})')

        if checked > 0:
            log.info(f'Postponement check: scanned {checked} overdue positions')

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

        slots = self.max_positions - self._pm_open_count()
        cap = dynamic_capital(self._pm_capital(), self.capital_pct)
        min_trade = self.config.get('live_trading', self.config.get('paper_trading', {})).get('min_trade_size', 10.0)
        if cap < min_trade:
            slots = 0  # Can't afford a new position — only replacement is possible
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
                if self.rust_pm:
                    result = self._pm_enter(opp_dict)
                else:
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
            excluded_pids = set()  # Positions already rejected for replacement (can't liquidate profitably)
            replacements_made = 0
            max_replacements = 5
            now_utc = datetime.now(timezone.utc)

            while replacements_made < max_replacements:
                held_cids = self._pm_held_cids()
                held_mids = self._pm_held_mids()

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

                # Score all open positions, find worst (skip excluded positions)
                worst_pos = None
                worst_score = float('inf')
                
                if self.rust_pm:
                    # Rust PM path: iterate position JSON data
                    for pos_json in self.rust_pm.get_open_positions_json():
                        pos = json.loads(pos_json)
                        pid = pos.get('position_id', '')
                        if pid in excluded_pids:
                            continue
                        pos_meta = pos.get('metadata', {})
                        cid = pos_meta.get('constraint_id', '')
                        
                        # Get validated resolution date
                        pos_end = None
                        if cid and self.resolution_validation_enabled:
                            cached = load_resolution_cache(cid)
                            if cached and 'latest_resolution_date' in cached:
                                try:
                                    pos_end = datetime.strptime(
                                        cached['latest_resolution_date'], '%Y-%m-%d'
                                    ).replace(tzinfo=timezone.utc)
                                except (ValueError, TypeError):
                                    pass
                        if pos_end is None:
                            for mid_str in pos.get('markets', {}).keys():
                                md = self.market_lookup.get(str(mid_str))
                                if md:
                                    ed = md.end_date
                                    if ed.tzinfo is None:
                                        ed = ed.replace(tzinfo=timezone.utc)
                                    if pos_end is None or ed > pos_end:
                                        pos_end = ed
                        if pos_end is None:
                            continue
                        # Postponement override
                        pp_meta = pos_meta.get('postponement', {})
                        if pp_meta and pp_meta.get('effective_date'):
                            try:
                                pp_end = datetime.strptime(
                                    pp_meta['effective_date'], '%Y-%m-%d'
                                ).replace(hour=23, minute=59, second=59, tzinfo=timezone.utc)
                                pos_end = pp_end
                            except (ValueError, TypeError):
                                pass
                        elif pos_meta.get('postponed') or pos_end < now_utc:
                            rescore_floor = now_utc + timedelta(days=self.postponement_rescore_days)
                            pos_end = max(pos_end, rescore_floor)

                        hours_remaining = (pos_end - now_utc).total_seconds() / 3600
                        if hours_remaining < 24:
                            continue
                        
                        # Use Rust evaluate_replacement for accurate liquidation value
                        bids = self._pm_get_current_bids(pos)
                        repl_profit = best_opp.get('expected_profit', 0)
                        eval_result = self.rust_pm.evaluate_replacement(pid, bids, repl_profit)
                        if not eval_result:
                            continue
                        
                        total_cap = pos.get('total_capital', 0)
                        remaining_upside = pos.get('expected_profit', 0) - eval_result['liquidation_profit']
                        eff_remaining = hours_remaining
                        rem_score = (remaining_upside / max(total_cap, 0.01)) / max(eff_remaining, 0.01)
                        
                        if rem_score < worst_score:
                            worst_score = rem_score
                            pos_name = list(pos.get('markets', {}).values())[0].get('name', '?')[:40] if pos.get('markets') else '?'
                            worst_pos = (pid, pos_name, rem_score, hours_remaining, eval_result)
                else:
                  for pid, pos in self.paper_engine.open_positions.items():
                    if pid in excluded_pids:
                        continue
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
                    # Postponement detection: use AI-detected date if available
                    pp_meta = pos.metadata.get('postponement', {})
                    if pp_meta and pp_meta.get('effective_date'):
                        try:
                            pp_end = datetime.strptime(
                                pp_meta['effective_date'], '%Y-%m-%d'
                            ).replace(hour=23, minute=59, second=59, tzinfo=timezone.utc)
                            pos_end = pp_end
                        except (ValueError, TypeError):
                            pass
                    # Fallback: if still overdue and no postponement data, extend denominator
                    elif pos.metadata.get('postponed') or pos_end < now_utc:
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
                # Iterate through held positions worst→best until we find one worth replacing
                if worst_pos and best_score > worst_score * 1.2:
                    if self.rust_pm:
                        # Rust PM path: worst_pos = (pid, name, score, hours, eval_result)
                        wpid, wname, wscore, whours, eval_r = worst_pos
                        if not eval_r.get('worth_replacing', False):
                            # This position can't be profitably liquidated — exclude it and retry
                            log.debug(f'  Skip replace "{wname}": net_gain=${eval_r.get("net_gain",0):+.2f} (not worth it)')
                            excluded_pids.add(wpid)
                            continue  # Retry while loop — will find next worst (skipping excluded)
                        bname = best_opp.get('market_names', ['?'])[0][:40]
                        log.info(f'REPLACE: "{wname}" (score={wscore:.6f}, {whours:.0f}h, '
                                 f'liq_profit=${eval_r["liquidation_profit"]:+.2f})')
                        log.info(f'  WITH: "{bname}" (score={best_score:.6f}, {best_hours:.0f}h, '
                                 f'net_gain=${eval_r["net_gain"]:+.2f})')
                        result = self._pm_liquidate(wpid, 'replaced')
                        if result.get('success'):
                            log.info(f'  Liquidated: freed ${result["freed_capital"]:.2f}, '
                                     f'realized ${result["actual_profit"]:+.2f}')
                            # Enter the replacement
                            cap = dynamic_capital(self._pm_capital(), self.capital_pct)
                            best_opp['total_capital_required'] = cap
                            old_cap = sum(best_opp.get('optimal_bets', {}).values())
                            if old_cap > 0:
                                scale = cap / old_cap
                                best_opp['optimal_bets'] = {k: v * scale for k, v in best_opp['optimal_bets'].items()}
                                best_opp['expected_profit'] = best_opp.get('expected_profit', 0) * scale
                            entry_r = self._pm_enter(best_opp)
                            if entry_r.get('success'):
                                replacements_made += 1
                                used_opp_cids.add(best_opp.get('constraint_id', ''))
                            else:
                                log.warning(f'  Replacement entry failed: {entry_r.get("reason")}')
                        else:
                            log.warning(f'  Liquidation failed: {result.get("reason")}')
                            break
                    else:
                        # Python fallback path: worst_pos = (pid, pos_obj, score, hours, unrealized)
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

        # 1. Load execution state (SQLite is the source of truth)
        if EXEC_STATE.exists():
            try:
                self.paper_engine.load_state(EXEC_STATE)
                log.info(f'State loaded: ${self.paper_engine.current_capital:.2f} capital, '
                         f'{len(self.paper_engine.open_positions)} positions')
            except Exception as e:
                log.warning(f'Could not load state: {e}')

        # 1b. Initialize positions in Rust WS engine (merged — no separate RustPositionManager)
        #     Wire self.rust_pm = self.rust_ws so ALL position ops go through Rust (A1)
        if self.rust_ws:
            try:
                fee_rate = self.config.get('arbitrage', {}).get('fees', {}).get('polymarket_taker_fee', 0.0001)
                self.rust_ws.init_positions(self.paper_engine.initial_capital, fee_rate)
                # Import existing positions from paper engine
                open_jsons = [json.dumps(p.to_dict()) for p in self.paper_engine.open_positions.values()]
                closed_jsons = [json.dumps(p.to_dict()) for p in self.paper_engine.closed_positions]
                self.rust_ws.import_positions(open_jsons, closed_jsons,
                                             self.paper_engine.current_capital,
                                             self.paper_engine.initial_capital)
                # A1: Wire rust_pm so all _pm_* helpers use Rust directly
                self.rust_pm = self.rust_ws
                log.info(f'Rust PM wired (A1): ${self.rust_ws.current_capital():.2f} capital, '
                         f'{self.rust_ws.pm_open_count()} open, {self.rust_ws.pm_closed_count()} closed')
            except Exception as e:
                log.warning(f'Rust PM init failed: {e}')

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
        all_assets = list(self.asset_to_constraints.keys())

        # 6a. Try Rust WS engine first (Phase 8 P4a — no GIL, full async)
        if HAS_RUST_WS and all_assets:
            try:
                ws_cfg = self.config.get('websocket', {})
                rust_cfg = {
                    'ws_url': ws_cfg.get('market_url',
                        'wss://ws-subscriptions-clob.polymarket.com/ws/market'),
                    'assets_per_shard': ws_cfg.get('assets_per_shard', 2000),
                    'heartbeat_interval_secs': ws_cfg.get('heartbeat_interval', 10),
                    'efp_drift_threshold': self.EFP_DRIFT_THRESHOLD,
                    'efp_stale_secs': 5.0,
                    'trade_size_usd': dynamic_capital(
                        self.paper_engine.current_capital, self.capital_pct),
                }
                self.rust_ws = rust_engine.RustWsEngine(rust_cfg)

                # Build asset→constraint index for Rust (converts sets to lists)
                rust_index = {aid: list(cids) for aid, cids
                              in self.asset_to_constraints.items()}
                self.rust_ws.set_asset_index(rust_index)

                # Load constraint definitions into Rust evaluator (Phase 8 P4c)
                self._load_constraints_into_rust()

                # Start WS connections + dashboard (non-blocking, spawns tokio tasks)
                self.rust_ws.start(all_assets, dashboard_port=5556)
                log.info(f'Rust WS engine + dashboard started: {len(all_assets)} assets')
            except Exception as e:
                log.warning(f'Rust WS engine failed: {e} — falling back to Python WS')
                self.rust_ws = None

        # 6b. Python WS fallback (if Rust WS not available or failed)
        if not self.rust_ws:
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

                if all_assets:
                    await self.ws_manager.subscribe_assets(all_assets)
                    log.info(f'WS: pre-subscribed {len(all_assets)} assets from index')

                await self.ws_manager.start()
                log.info('Python WebSocket manager started')
            except Exception as e:
                log.warning(f'WS manager failed: {e} — running without live prices')
                self.ws_manager = None

        # Weekly delay table update
        trigger_weekly_delay_update()

        mode_str = 'shadow' if self.shadow_only else 'live'
        log.info(f'Trading Engine ready [{mode_str.upper()}] — entering event loop')
        write_status('running', self._pm_capital(), self._pm_open_count())

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
                    from arbitrage_math.arbitrage_engine import ArbitrageOpportunity
                    OPP_PATH.parent.mkdir(parents=True, exist_ok=True)
                    OPP_PATH.write_text(json.dumps({
                        'opportunities': opportunities,
                        'count': len(opportunities),
                        'timestamp': datetime.now(ZoneInfo('Europe/London')).isoformat()
                    }, indent=2))

                    await self._try_enter_or_replace(opportunities)

                # --- Monitor open positions (periodic) ---
                if (now - self._last_monitor) >= self.MONITOR_INTERVAL:
                    if self._pm_open_count() > 0:
                        if self.rust_pm:
                            # A1: Use Rust PM check_resolutions directly
                            market_prices = self._build_market_prices()
                            resolved = self.rust_pm.check_resolutions(market_prices)
                            for pid, winning_mid in resolved:
                                result = self.rust_pm.close_on_resolution(pid, winning_mid)
                                if result:
                                    payout, profit = result
                                    log.info(f'RESOLVED {pid[:40]}: payout=${payout:.2f} profit=${profit:.2f}')
                        else:
                            markets_list = list(self.market_lookup.values())
                            await self.paper_engine.monitor_positions(markets_list)
                    # Proactive exit: DISABLED — needs validation before production use
                    # self._check_proactive_exits()
                    self._last_monitor = now

                # --- Check for postponed events (periodic, threaded) ---
                if (now - self._last_postponement_check) >= self.POSTPONEMENT_CHECK_INTERVAL:
                    if self._pm_open_count() > 0 and self.ai_config.get('postponement', {}).get('enabled', False):
                        await loop.run_in_executor(
                            self._executor,
                            self._check_postponements
                        )
                    self._last_postponement_check = now

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
                    # Re-subscribe / re-index all assets
                    all_assets = list(self.asset_to_constraints.keys())
                    if self.rust_ws and all_assets:
                        rust_index = {aid: list(cids) for aid, cids
                                      in self.asset_to_constraints.items()}
                        self.rust_ws.set_asset_index(rust_index)
                        self._load_constraints_into_rust()
                        self.rust_ws.start(all_assets, dashboard_port=0)  # Re-subscribe only, no dashboard restart
                        log.info(f'Rust WS re-indexed: {len(all_assets)} assets')
                    elif self.ws_manager and self.ws_manager._running:
                        if all_assets:
                            await self.ws_manager.subscribe_assets(all_assets)
                    self._last_constraint_rebuild = now

                # --- WS resolved markets → force monitor pass ---
                # Drain Rust WS resolved events into the same set
                if self.rust_ws:
                    for cid, _aid in self.rust_ws.drain_resolved():
                        if cid:
                            self._ws_resolved_markets.add(cid)
                if self._ws_resolved_markets:
                    resolved = list(self._ws_resolved_markets)
                    self._ws_resolved_markets.clear()
                    log.info(f'WS resolution trigger: {len(resolved)} markets')
                    if self._pm_open_count() > 0:
                        if self.rust_pm:
                            market_prices = self._build_market_prices()
                            resolved_positions = self.rust_pm.check_resolutions(market_prices)
                            for pid, winning_mid in resolved_positions:
                                result = self.rust_pm.close_on_resolution(pid, winning_mid)
                                if result:
                                    payout, profit = result
                                    log.info(f'RESOLVED {pid[:40]}: payout=${payout:.2f} profit=${profit:.2f}')
                        else:
                            markets_list = list(self.market_lookup.values())
                            await self.paper_engine.monitor_positions(markets_list)

                # --- Save state (periodic) ---
                if (now - self._last_state_save) >= self.STATE_SAVE_INTERVAL:
                    STATE_DIR.mkdir(parents=True, exist_ok=True)
                    self._save_state()
                    self._last_state_save = now

                    cap = self._pm_capital()
                    npos = self._pm_open_count()
                    write_status('running', cap, npos)

                # --- Phase 8l: Stale-asset re-subscribe sweep (every 60s) ---
                if (now - self._last_stale_sweep) >= 60.0:
                    if self.rust_ws:
                        # Rust tracks staleness internally
                        stale_assets = self.rust_ws.get_stale_assets(30.0)
                        if stale_assets:
                            log.info(f'Rust stale assets: {len(stale_assets)} >30s old (re-subscribe not yet implemented)')
                            # TODO P4a: Add per-shard mpsc channels for dynamic re-subscribe
                            # For now, stale books are expected for low-volume markets
                        self._last_stale_sweep = now
                    elif self.ws_manager:
                        stale_threshold = 30.0  # seconds
                        stale_assets = []
                        for asset_id in self.asset_to_constraints:
                            book = self.ws_manager.get_book(asset_id)
                            if book and book.last_update > 0:
                                age = now - book.last_update
                                if age > stale_threshold:
                                    stale_assets.append(asset_id)
                        if stale_assets:
                            log.info(f'Stale re-subscribe: {len(stale_assets)} assets >30s old')
                            await self.ws_manager.subscribe_assets(stale_assets)
                        self._last_stale_sweep = now

                # --- Stats logging (every ~30s) ---
                if (now - getattr(self, '_last_stats_log', 0)) >= 30.0:
                    ws_info = ''
                    engine_metrics = {
                        'iteration': self._iteration,
                        'has_rust': HAS_RUST,
                        'constraints': len(self.constraints) if self.constraints else 0,
                        'markets_total': len(self.market_lookup),
                    }
                    if self.rust_ws:
                        rs = self.rust_ws.stats()
                        q_urg, q_bg = self.rust_ws.queue_depths()
                        live_count = rs.get('ws_live', 0)
                        lat_info = ''
                        lat_p50 = lat_p95 = lat_max = 0
                        if self._recent_latencies:
                            lats = sorted(self._recent_latencies)
                            lat_p50 = lats[len(lats) // 2]
                            lat_p95 = lats[int(len(lats) * 0.95)]
                            lat_max = lats[-1]
                            lat_info = f' lat_\u03bcs p50={lat_p50:.0f} p95={lat_p95:.0f} max={lat_max:.0f}'
                        ws_info = (f' | RustWS: subs={rs.get("ws_subscribed",0)} '
                                   f'msgs={rs.get("ws_msgs",0)} '
                                   f'live={live_count}/{len(self.market_lookup)} '
                                   f'urgent={q_urg} bg={q_bg}{lat_info}')
                        engine_metrics.update({
                            'ws_subscribed': rs.get('ws_subscribed', 0),
                            'ws_msgs': rs.get('ws_msgs', 0),
                            'ws_live': live_count,
                            'queue_urgent': q_urg,
                            'queue_background': q_bg,
                            'lat_p50_us': round(lat_p50),
                            'lat_p95_us': round(lat_p95),
                            'lat_max_us': round(lat_max),
                        })
                    elif self.ws_manager and self.ws_manager._running:
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
                            lat_info = f' lat_\u03bcs p50={lat_p50:.0f} p95={lat_p95:.0f} max={lat_max:.0f}'
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
                            'lat_p50_us': round(lat_p50),
                            'lat_p95_us': round(lat_p95),
                            'lat_max_us': round(lat_max),
                        })
                    cap = self._pm_capital()
                    npos = self._pm_open_count()
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
        if self.rust_ws:
            self.rust_ws.stop()
            log.info('Rust WS engine stopped')
        if self.ws_manager:
            await self.ws_manager.stop()
        STATE_DIR.mkdir(parents=True, exist_ok=True)
        self._save_state()
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
