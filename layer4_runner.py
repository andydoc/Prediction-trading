"""Layer 4 Runner - Execution / Paper Trading
   - Dynamic capital_per_trade = max(10, min(balance*0.1, 1000))
   - Ranks opportunities by profit_pct / time_to_resolution
   - Position replacement: worst open vs best available (max 1 every 5 min)
   - Dedup by market_id set (not just constraint_id)
"""
import asyncio, json, logging, sys, time as _time, yaml
from pathlib import Path
from datetime import datetime, timezone, timedelta
from zoneinfo import ZoneInfo

sys.path.append(str(Path(__file__).parent))
from paper_trading import PaperTradingEngine
from live_trading import LiveTradingEngine
from layer1_market_data.market_data import MarketData
from execution_control_client import ExecutionLock
from resolution_validator import get_validated_resolution_date, get_full_validation, _load_cache as load_resolution_cache
from websocket_manager import WebSocketManager, get_asset_ids_for_constraints, get_condition_ids_for_positions
import os

# Load .env file for API keys (bashrc interactive guard blocks non-interactive shells)
_env_file = Path(__file__).parent / '.env'
if _env_file.exists():
    for line in _env_file.read_text().splitlines():
        line = line.strip()
        if line and not line.startswith('#') and '=' in line:
            k, v = line.split('=', 1)
            os.environ.setdefault(k.strip(), v.strip())

WORKSPACE    = Path('/home/andydoc/prediction-trader')
CONFIG_PATH  = WORKSPACE / 'config' / 'config.yaml'
SECRETS_PATH = WORKSPACE / 'config' / 'secrets.yaml'
OPP_PATH     = WORKSPACE / 'layer3_arbitrage_math' / 'data' / 'latest_opportunities.json'
MARKETS_PATH = WORKSPACE / 'data' / 'latest_markets.json'
STATUS_PATH  = WORKSPACE / 'data' / 'layer4_status.json'
WS_PRICES_PATH = WORKSPACE / 'data' / 'ws_prices.json'   # Phase 6c: WS price bridge for L3
STATE_DIR    = WORKSPACE / 'data' / 'system_state'
EXEC_STATE   = STATE_DIR / 'execution_state.json'

logging.basicConfig(level=logging.DEBUG,
    format='%(asctime)s - [L4] %(levelname)s - %(message)s',
    handlers=[logging.FileHandler(str(WORKSPACE / 'logs' / f'layer4_{datetime.now().strftime("%Y%m%d")}.log')), logging.StreamHandler()])
log = logging.getLogger('layer4')

# Quiet noisy libraries at DEBUG level
logging.getLogger('websockets').setLevel(logging.WARNING)
logging.getLogger('websockets.client').setLevel(logging.WARNING)

# --- Resolution delay model (dynamically loaded, updated weekly) ---
# Fallback P95 values if JSON file not yet generated
_FALLBACK_P95 = {
    'football': 14.8, 'us_sports': 33.6, 'esports': 20.0, 'tennis': 20.8,
    'mma_boxing': 50.3, 'cricket': 21.8, 'rugby': 23.3, 'politics': 350.2,
    'gov_policy': 44.3, 'crypto': 3.4, 'sports_props': 6.5, 'other': 33.5,
}
_FALLBACK_DEFAULT = 33.5

P95_JSON_PATH = WORKSPACE / 'data' / 'resolution_delay_p95.json'
_delay_table_cache = {'p95_hours': None, 'default': None, 'loaded_at': None}


def load_delay_table():
    """Load P95 delay table from JSON. Returns (p95_dict, default_p95).
    Falls back to hardcoded values if file missing or corrupt."""
    # Cache for 1 hour to avoid repeated disk reads
    if (_delay_table_cache['p95_hours'] is not None and _delay_table_cache['loaded_at']
            and (_time.time() - _delay_table_cache['loaded_at']) < 3600):
        return _delay_table_cache['p95_hours'], _delay_table_cache['default']
    try:
        if P95_JSON_PATH.exists():
            data = json.loads(P95_JSON_PATH.read_text())
            p95 = data.get('p95_hours', {})
            default = data.get('default_p95_hours', _FALLBACK_DEFAULT)
            if p95:
                _delay_table_cache['p95_hours'] = p95
                _delay_table_cache['default'] = default
                _delay_table_cache['loaded_at'] = _time.time()
                log.debug(f'Loaded delay table: {len(p95)} categories, '
                          f'generated={data.get("generated_at","?")[:10]}, '
                          f'lookback={data.get("lookback_months","?")}mo')
                return p95, default
    except Exception as e:
        log.warning(f'Failed to load delay table: {e}')
    return _FALLBACK_P95, _FALLBACK_DEFAULT


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
                return  # Not due yet
        # Run update in subprocess to avoid blocking L4
        import subprocess
        script = WORKSPACE / 'scripts' / 'debug' / 'update_delay_table.py'
        if script.exists():
            log.info('Weekly delay table update triggered')
            subprocess.Popen(
                [sys.executable, str(script)],
                stdout=open(str(WORKSPACE / 'logs' / 'delay_update.log'), 'a'),
                stderr=subprocess.STDOUT,
                cwd=str(WORKSPACE)
            )
    except Exception as e:
        log.debug(f'Delay update check error: {e}')

import math

def classify_opportunity_category(market_names: list, market_lookup: dict, market_ids: list) -> str:
    """Classify an opportunity into a resolution-delay category based on market names."""
    names_lower = ' '.join(n.lower() for n in market_names)
    # Try to get descriptions from market_lookup for better classification
    descs = ''
    for mid in market_ids:
        md = market_lookup.get(str(mid))
        if md and hasattr(md, 'metadata') and isinstance(md.metadata, dict):
            descs += ' ' + md.metadata.get('description', '').lower()

    # Football — most common in our arb universe
    football_q = any(p in names_lower for p in ['win on 20', 'end in a draw', 'halftime', 'leading at halftime'])
    football_d = any(p in descs for p in ['90 minutes', 'stoppage time', 'regular play'])
    if football_q and (football_d or not descs):
        return 'football'
    if any(p in names_lower for p in ['halftime', 'leading at halftime']):
        return 'football'

    # US Sports
    if any(p in names_lower for p in ['nba ', 'nfl ', 'nhl ', 'mlb ', 'wnba ',
                                       'touchdown', 'rushing yards', 'passing yards',
                                       'rebounds', 'three-pointer']):
        return 'us_sports'
    if any(p in names_lower for p in ['spread:', 'team total:', 'o/u ']):
        if any(p in descs for p in ['90 minutes', 'stoppage']):
            return 'football'
        return 'sports_props'

    # Esports
    if any(p in names_lower for p in ['counter-strike', 'cs2', 'dota', 'league of legends',
                                       'valorant', 'overwatch', 'dreamleague']):
        return 'esports'

    # Tennis
    if any(p in names_lower for p in ['atp ', 'wta ', 'tennis']):
        return 'tennis'

    # MMA/Boxing
    if any(p in names_lower for p in ['ufc ', 'mma ', 'boxing', 'pfl ', 'bellator']):
        return 'mma_boxing'

    # Cricket
    if any(p in names_lower for p in ['cricket', 'ipl ', 't20 ']):
        return 'cricket'

    # Rugby
    if any(p in names_lower for p in ['rugby', 'super rugby', 'waratahs']):
        return 'rugby'

    # Crypto
    if any(p in names_lower for p in ['bitcoin', 'ethereum', 'solana', 'btc ', 'eth ',
                                       'up or down']):
        return 'crypto'

    # Politics
    if any(p in names_lower for p in ['governor', 'congress', 'senate', 'primary',
                                       'democrat', 'republican', 'election', 'president']):
        return 'politics'

    # Government/Policy
    if any(p in names_lower for p in ['fed ', 'federal reserve', 'interest rate',
                                       'tariff', 'government shutdown']):
        return 'gov_policy'

    return 'other'


def get_volume_penalty_hours(min_volume: float) -> float:
    """Soft volume penalty: low-volume markets take longer to resolve.
    Adds ~6h for $100 volume, ~2h for $10K, ~0h for $100K+."""
    if min_volume <= 0:
        return 8.0  # No volume data = assume slow
    return max(0.0, (5.0 - math.log10(min_volume + 1)) * 2.0)


def get_min_volume(opp_dict: dict, market_lookup: dict) -> float:
    """Get minimum volume_24h across all markets in an opportunity.
    Resolution speed is limited by the least-liquid market."""
    volumes = []
    for mid in opp_dict.get('market_ids', []):
        md = market_lookup.get(str(mid))
        if md and hasattr(md, 'volume_24h'):
            volumes.append(md.volume_24h or 0)
    return min(volumes) if volumes else 0.0


def write_status(status, capital=0, open_pos=0, error=None):
    STATUS_PATH.parent.mkdir(parents=True, exist_ok=True)
    STATUS_PATH.write_text(json.dumps({
        'status': status, 'capital': capital, 'open_positions': open_pos,
        'error': error, 'timestamp': datetime.now(ZoneInfo('Europe/London')).isoformat()
    }))

def dynamic_capital(current_balance: float, pct: float = 0.10) -> float:
    """% of current capital, floor $10, cap $1000."""
    return max(10.0, min(current_balance * pct, 1000.0))

def get_resolution_hours(opp_dict: dict, market_lookup: dict) -> float:
    """Hours until ALL markets resolve (LATEST/MAX end_date).
    Returns float hours, -1 if all past, None if no dates."""
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

def rank_opportunities(opps: list, market_lookup: dict, min_resolution_secs: int = 300,
                       max_days_to_resolution: int = 60) -> list:
    scored = []
    max_hours = max_days_to_resolution * 24
    for opp in opps:
        hours = get_resolution_hours(opp, market_lookup)
        if hours is None:
            continue
        if hours < 0:
            log.debug(f"  Filtered out {opp.get('opportunity_id','?')}: end_date in past")
            continue
        if hours * 3600 < min_resolution_secs:
            continue
        if hours > max_hours:
            log.debug(f"  Filtered out {opp.get('opportunity_id','?')}: {hours/24:.0f}d > {max_days_to_resolution}d max")
            continue
        profit_pct = opp.get('expected_profit_pct', 0)
        # --- Resolution delay adjustment ---
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
    if scored:
        # Log delay model stats for top opportunity
        top_s, top_h, top_o = scored[0]
        top_cat = classify_opportunity_category(
            top_o.get('market_names', []), market_lookup, top_o.get('market_ids', []))
        top_vol = get_min_volume(top_o, market_lookup)
        p95_t, def_p95 = load_delay_table()
        log.debug(f'  Rank model: top score={top_s:.6f} cat={top_cat} '
                  f'raw_h={top_h:.1f} +p95={p95_t.get(top_cat, def_p95):.1f} '
                  f'+vol_pen={get_volume_penalty_hours(top_vol):.1f} (vol=${top_vol:.0f})')
    return scored

def get_held_market_ids(engine) -> set:
    """Get all market_ids currently held in open positions."""
    held = set()
    for pos in engine.open_positions.values():
        for mid in pos.markets.keys():
            held.add(str(mid))
    return held

def get_held_constraint_ids(engine) -> set:
    """Get all constraint_ids currently held."""
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
    """Estimate current liquidation value of a position at mid-prices."""
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
            cur_p = md.outcome_prices.get(outcome, entry_p)
        else:
            cur_p = entry_p
        liq += shares * cur_p
    return liq


def get_asset_ids_for_positions(engine, market_lookup: dict) -> list:
    """Extract all CLOB token_ids (YES+NO) for markets in open positions.
    Used to subscribe WS market channel for live depth + resolution."""
    asset_ids = set()
    for pos in engine.open_positions.values():
        for mid in pos.markets.keys():
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


def get_asset_ids_for_opportunity(opp_dict: dict, market_lookup: dict) -> list:
    """Extract CLOB token_ids for a single opportunity's markets."""
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


def build_token_to_market_map(market_lookup: dict) -> dict:
    """Build reverse map: CLOB token_id → (market_id, token_index).
    token_index 0 = YES token, 1 = NO token."""
    tmap = {}
    for mid, md in market_lookup.items():
        if not hasattr(md, 'metadata'):
            continue
        clob_raw = md.metadata.get('clobTokenIds', '[]')
        try:
            clob_ids = json.loads(clob_raw) if isinstance(clob_raw, str) else clob_raw
        except (json.JSONDecodeError, TypeError):
            continue
        for idx, tid in enumerate(clob_ids or []):
            if tid:
                tmap[tid] = (mid, idx)
    return tmap


def write_ws_price_bridge(ws_manager, market_lookup: dict):
    """Write WS live prices to data/ws_prices.json for L3 to consume.
    Maps asset_id prices back to market_id Yes/No prices."""
    try:
        price_cache = ws_manager.export_price_cache()
        if not price_cache:
            return
        token_map = build_token_to_market_map(market_lookup)
        market_prices = {}
        for asset_id, pdata in price_cache.items():
            mapping = token_map.get(asset_id)
            if not mapping:
                continue
            mid, token_idx = mapping
            if mid not in market_prices:
                market_prices[mid] = {'ts': pdata['ts']}
            price = pdata['mid'] or pdata['best_ask'] or pdata['best_bid'] or 0
            if token_idx == 0:
                market_prices[mid]['Yes'] = round(price, 6)
            elif token_idx == 1:
                market_prices[mid]['No'] = round(price, 6)
            if pdata['ts'] > market_prices[mid].get('ts', 0):
                market_prices[mid]['ts'] = pdata['ts']
        if market_prices:
            WS_PRICES_PATH.write_text(json.dumps({
                'prices': market_prices,
                'count': len(market_prices),
                'exported_at': _time.time(),
            }))
    except Exception as e:
        log.debug(f'WS price bridge write error: {e}')


async def main():
    with open(CONFIG_PATH) as f:
        config = yaml.safe_load(f)
    # Load secrets (Polymarket keys, Anthropic key, etc.)
    secrets = {}
    if SECRETS_PATH.exists():
        with open(SECRETS_PATH) as f:
            secrets = yaml.safe_load(f) or {}
    max_positions = config.get('arbitrage', {}).get('max_concurrent_positions', 20)
    max_days_to_resolution = config.get('arbitrage', {}).get('max_days_to_resolution', 60)
    max_days_to_replacement = config.get('arbitrage', {}).get('max_days_to_replacement', 30)
    replace_on_postponement = config.get('arbitrage', {}).get('replace_on_postponement', True)
    postponement_breakeven_only = config.get('arbitrage', {}).get('postponement_replace_breakeven_only', True)
    capital_pct = config.get('arbitrage', {}).get('capital_per_trade_pct', 0.10)
    res_val_cfg = config.get('arbitrage', {}).get('resolution_validation', {})
    # Anthropic key: secrets.yaml > config.yaml > env var
    anthropic_api_key = (
        secrets.get('resolution_validation', {}).get('anthropic_api_key', '')
        or os.path.expandvars(res_val_cfg.get('anthropic_api_key', ''))
        or os.environ.get('ANTHROPIC_API_KEY', '')
    )
    resolution_validation_enabled = res_val_cfg.get('enabled', True)
    check_interval = 30
    replacement_cooldown_secs = 60   # Min 60s between replacement rounds

    engine = PaperTradingEngine(config, WORKSPACE)
    if EXEC_STATE.exists():
        try:
            engine.load_state(EXEC_STATE)
            log.info(f'Loaded state: ${engine.current_capital:.2f} capital, {len(engine.open_positions)} positions')
        except Exception as e:
            log.warning(f'Could not load state: {e}')

        # Detect trading mode
    trading_mode = config.get('mode', 'paper_trading')
    live_cfg = config.get('live_trading', {})
    live_enabled = live_cfg.get('enabled', False)
    shadow_only = live_cfg.get('shadow_only', False)
    
    live_engine = None
    if trading_mode in ('live_trading', 'dual') or live_enabled:
        try:
            live_engine = LiveTradingEngine(config, WORKSPACE)
            health = live_engine.health_check()
            if health['healthy']:
                log.info(f'Live engine initialized: balance=${health["balance_usd"]:.2f}')
            else:
                log.error(f'Live engine unhealthy: {health.get("error")}')
                live_engine = None
        except Exception as e:
            log.error(f'Failed to init live engine: {e}')
            live_engine = None
    
    mode_str = trading_mode
    if live_engine and shadow_only:
        mode_str = 'shadow'
    elif live_engine:
        mode_str = 'live' if trading_mode == 'live_trading' else 'dual'
    
    log.info(f'Layer 4 started [{mode_str.upper()}] (dynamic capital + resolution ranking + replacement)')
    write_status('starting')

    # Check for weekly delay table update at startup
    trigger_weekly_delay_update()

    # Execution lock — only the leader machine may trade
    exec_lock_url = config.get('execution_control', {}).get('url', 'http://localhost:5557')
    exec_lock_enabled = config.get('execution_control', {}).get('enabled', True)
    exec_lock = ExecutionLock(server_url=exec_lock_url, ttl=300)
    if not exec_lock_enabled:
        exec_lock.disable()
        log.info('Execution lock DISABLED (single-machine mode)')
    else:
        log.info(f'Execution lock enabled, server={exec_lock_url}')

    iteration = 0
    last_replacement_time = 0.0  # Unix timestamp of last replacement

    # --- WebSocket Manager (Phase 6b) ---
    ws_manager = None
    ws_resolved_markets = set()  # condition_ids resolved via WS (consumed by monitor_positions)
    try:
        ws_manager = WebSocketManager(config=config, secrets=secrets)

        # Wire user channel auth from live engine's derived CLOB API creds
        if live_engine and hasattr(live_engine, 'client'):
            try:
                creds = live_engine.client.creds
                if creds and hasattr(creds, 'api_key') and creds.api_key:
                    ws_manager._user_auth = {
                        'apiKey': creds.api_key,
                        'secret': creds.api_secret,
                        'passphrase': creds.api_passphrase,
                    }
                    log.info(f'WS user channel: auth from live engine (key={creds.api_key[:8]}...)')
            except Exception as ae:
                log.debug(f'WS user auth from live engine failed: {ae}')

        # Callback: market_resolved → flag for immediate resolution check
        def _on_ws_resolved(market_cid, asset_id):
            ws_resolved_markets.add(market_cid)
            log.info(f'WS: market_resolved {market_cid[:20]}... — queued for resolution check')

        ws_manager.on_market_resolved(_on_ws_resolved)

        # Callback: trade confirm (log for now, wire to fill tracking later)
        def _on_ws_trade(trade_data):
            status = trade_data.get('status', '?')
            side = trade_data.get('side', '?')
            size = trade_data.get('size', '?')
            price = trade_data.get('price', '?')
            log.info(f'WS trade: status={status} side={side} size={size} price={price}')

        ws_manager.on_trade_confirm(_on_ws_trade)

        await ws_manager.start()
        log.info('WebSocket manager started')
    except ImportError:
        log.warning('WebSocket manager: websockets package not installed, running without WS')
    except Exception as e:
        log.warning(f'WebSocket manager init failed: {e} — running without WS')

    # Track last WS subscription refresh (don't refresh every iteration)
    ws_last_sub_refresh = 0.0
    WS_SUB_REFRESH_INTERVAL = 120  # seconds between full subscription refreshes

    while True:
        iteration += 1
        try:
            # Load markets
            if not MARKETS_PATH.exists():
                log.warning(f'[iter {iteration}] No markets, waiting...')
                write_status('waiting_for_markets')
                await asyncio.sleep(check_interval)
                continue
            markets = [MarketData.from_dict(m) for m in json.loads(MARKETS_PATH.read_text()).get('markets', [])]
            market_lookup = {str(m.market_id): m for m in markets}

            # --- WS: refresh subscriptions periodically ---
            if ws_manager and ws_manager._running:
                now_ws = _time.time()
                if (now_ws - ws_last_sub_refresh) >= WS_SUB_REFRESH_INTERVAL:
                    try:
                        # Subscribe assets for all open positions
                        pos_assets = get_asset_ids_for_positions(engine, market_lookup)
                        if pos_assets:
                            await ws_manager.subscribe_assets(pos_assets)
                        # Subscribe assets from L3 opportunities
                        if OPP_PATH.exists():
                            try:
                                opp_text = OPP_PATH.read_text()
                                if opp_text.strip():
                                    l3_opps = json.loads(opp_text).get('opportunities', [])
                                    opp_constraints = [{'market_ids': o.get('market_ids', [])} for o in l3_opps[:50]]
                                    opp_assets = get_asset_ids_for_constraints(opp_constraints, market_lookup)
                                    if opp_assets:
                                        await ws_manager.subscribe_assets(opp_assets)
                            except Exception:
                                pass
                        ws_last_sub_refresh = now_ws
                        if iteration % 20 == 1:
                            stats = ws_manager.get_stats()
                            log.debug(f'WS: subs={len(ws_manager._subscribed_assets)} '
                                      f'market_msgs={stats["market_msgs"]} user_msgs={stats["user_msgs"]}')
                    except Exception as wse:
                        log.debug(f'WS subscription refresh error: {wse}')

                # Write WS price bridge for L3 (every iteration, cheap)
                write_ws_price_bridge(ws_manager, market_lookup)

            # --- WS: check for resolved markets (instant resolution trigger) ---
            if ws_resolved_markets:
                resolved_cids = list(ws_resolved_markets)
                ws_resolved_markets.clear()
                log.info(f'[iter {iteration}] WS triggered resolution check for {len(resolved_cids)} markets')
                # Force a monitor pass — the next monitor_positions call will detect price→1.0

            # Monitor open positions
            if engine.open_positions:
                log.debug(f'[iter {iteration}] Monitoring {len(engine.open_positions)} open positions')
                await engine.monitor_positions(markets)

            # --- POSTPONEMENT REPLACEMENT ---
            # Triggered by monitor_positions flagging position.metadata['postponed']=True.
            # Controlled by: replace_on_postponement (bool) and postponement_breakeven_only (bool).
            # breakeven_only=True means we only replace if liq_value >= capital_deployed (no loss on exit).
            if replace_on_postponement and engine.open_positions and OPP_PATH.exists():
                for ppid, ppos in list(engine.open_positions.items()):
                    if not ppos.metadata.get('postponed'):
                        continue
                    pname = list(ppos.markets.values())[0].get('name', '?')[:40] if ppos.markets else '?'
                    liq_val = calc_position_liq_value(ppos, market_lookup)
                    unrealized = liq_val - ppos.total_capital
                    log.info(f'[iter {iteration}] POSTPONED position: "{pname}" '
                             f'capital=${ppos.total_capital:.2f} liq=${liq_val:.2f} P&L=${unrealized:+.2f}')
                    if postponement_breakeven_only and unrealized < 0:
                        log.info(f'  Skipping replacement — P&L ${unrealized:+.2f} < 0 '
                                 f'(postponement_replace_breakeven_only=True)')
                        continue
                    # Find best replacement candidate (reuse same ranked list if available)
                    try:
                        opp_text = OPP_PATH.read_text()
                        if not opp_text.strip():
                            continue
                        post_opps = json.loads(opp_text).get('opportunities', [])
                        post_ranked = rank_opportunities(post_opps, market_lookup,
                                                         min_resolution_secs=300,
                                                         max_days_to_resolution=max_days_to_replacement)
                        held_cids = get_held_constraint_ids(engine)
                        held_mids = get_held_market_ids(engine)
                        best_post = None
                        for pscore, phours, popp in post_ranked:
                            pcid = popp.get('constraint_id', '')
                            if pcid in held_cids:
                                continue
                            if opportunity_overlaps_held(popp, held_mids):
                                continue
                            if resolution_validation_enabled and anthropic_api_key:
                                try:
                                    pmids = popp.get('market_ids', [])
                                    pval = get_full_validation(pmids, market_lookup, anthropic_api_key, pcid)
                                    if pval:
                                        if pval.get('has_unrepresented_outcome', False):
                                            continue
                                        try:
                                            pvd = datetime.strptime(pval['latest_resolution_date'], '%Y-%m-%d').replace(
                                                hour=23, minute=59, second=59, tzinfo=timezone.utc)
                                            if (pvd - datetime.now(timezone.utc)).days > max_days_to_replacement:
                                                continue
                                        except (ValueError, KeyError):
                                            pass
                                except Exception:
                                    pass
                            best_post = (pscore, phours, popp)
                            break
                        if best_post:
                            bpscore, bphours, bpopp = best_post
                            bpname = bpopp.get('market_names', ['?'])[0][:40]
                            log.info(f'  Replacing with: "{bpname}" '
                                     f'(score={bpscore:.6f}/hr, {bphours:.0f}h, '
                                     f'{bpopp.get("expected_profit_pct",0)*100:.1f}%)')
                            presult = await engine.liquidate_position(ppid, market_lookup)
                            if presult.get('success'):
                                log.info(f'  Liquidated postponed: freed ${presult["freed_capital"]:.2f}, '
                                         f'realized ${presult["actual_profit"]:+.2f}')
                                if live_engine and trading_mode in ('live_trading', 'dual'):
                                    plive_meta = ppos.metadata.get('live', {})
                                    if plive_meta.get('token_map'):
                                        try:
                                            live_engine.liquidate_live_position(
                                                {mid: {'token_id': info.get('token_id', ''), 'shares': info.get('shares', 0)}
                                                 for mid, info in plive_meta.get('position_markets', {}).items()},
                                                plive_meta['token_map'])
                                        except Exception as ple:
                                            log.error(f'  LIVE postponed liquidation error: {ple}')
                            else:
                                log.warning(f'  Postponed liquidation failed: {presult.get("reason")}')
                        else:
                            log.info(f'  No valid replacement found for postponed position — holding')
                    except Exception as pe:
                        log.error(f'[iter {iteration}] Postponement replacement error: {pe}', exc_info=True)


            if not exec_lock.can_execute():
                leader = (exec_lock.last_status or {}).get('leader', '?')
                if iteration % 10 == 1:
                    log.info(f'[iter {iteration}] Execution locked by {leader} — monitoring only')
                # Still save state and monitor, just don't trade
                STATE_DIR.mkdir(parents=True, exist_ok=True)
                engine.save_state(EXEC_STATE)
                metrics = engine.get_performance_metrics() if hasattr(engine, 'get_performance_metrics') else {}
                write_status('locked', metrics.get('current_capital', 0), len(engine.open_positions))
                await asyncio.sleep(check_interval)
                continue

            # Send heartbeat if we're the leader
            if exec_lock._enabled:
                exec_lock.heartbeat()

            # --- POSITION REPLACEMENT (loop: replace ALL underperformers each round) ---
            now_ts = _time.time()
            if (engine.open_positions and OPP_PATH.exists()
                    and (now_ts - last_replacement_time) >= replacement_cooldown_secs):
                try:
                    opp_text = OPP_PATH.read_text()
                    if not opp_text.strip():
                        raise ValueError("Empty opportunities file (L3 writing)")
                    opps_for_replace = json.loads(opp_text).get('opportunities', [])
                    ranked_for_replace = rank_opportunities(opps_for_replace, market_lookup, min_resolution_secs=300, max_days_to_resolution=max_days_to_replacement)
                    now_utc = datetime.now(timezone.utc)
                    replacements_made = 0
                    max_replacements_per_round = 5
                    used_opp_cids = set()  # Prevent same opp liquidating multiple positions

                    while replacements_made < max_replacements_per_round:
                        held_cids = get_held_constraint_ids(engine)
                        held_mids = get_held_market_ids(engine)

                        # Find best untraded opportunity
                        best_new = None
                        for score, hours, opp_d in ranked_for_replace:
                            cid = opp_d.get('constraint_id', '')
                            if cid in held_cids:
                                continue
                            if cid in used_opp_cids:
                                continue  # Already used this opp to replace something this round
                            if opportunity_overlaps_held(opp_d, held_mids):
                                continue
                            # AI resolution date + outcome check for replacement candidates
                            if resolution_validation_enabled and anthropic_api_key:
                                try:
                                    mids = opp_d.get('market_ids', [])
                                    val = get_full_validation(mids, market_lookup, anthropic_api_key, cid)
                                    if val:
                                        if val.get('has_unrepresented_outcome', False):
                                            continue  # Skip — broken mutual exclusivity
                                        try:
                                            vd = datetime.strptime(val['latest_resolution_date'], '%Y-%m-%d').replace(
                                                hour=23, minute=59, second=59, tzinfo=timezone.utc)
                                            if (vd - datetime.now(timezone.utc)).days > max_days_to_replacement:
                                                continue
                                        except (ValueError, KeyError):
                                            pass
                                except Exception:
                                    pass  # Fail open
                            best_new = (score, hours, opp_d)
                            break

                        if not best_new:
                            break
                        best_score, best_hours, best_opp = best_new

                        # Score all open positions, find worst
                        # Positions whose validated resolution date is <24h away are PROTECTED
                        worst_pos = None
                        worst_remaining_score = float('inf')
                        for pid, pos in engine.open_positions.items():
                            # 1. Try validated resolution date from AI cache
                            pos_latest_end = None
                            cid = pos.metadata.get('constraint_id', '')
                            if cid and resolution_validation_enabled:
                                cached = load_resolution_cache(cid)
                                if cached and 'latest_resolution_date' in cached:
                                    try:
                                        pos_latest_end = datetime.strptime(cached['latest_resolution_date'], '%Y-%m-%d').replace(tzinfo=timezone.utc)
                                    except (ValueError, TypeError):
                                        pass

                            # 2. Fallback to API end_date if no validated date
                            if pos_latest_end is None:
                                for mid_str in pos.markets.keys():
                                    md = market_lookup.get(str(mid_str))
                                    if md:
                                        ed = md.end_date
                                        if ed.tzinfo is None:
                                            ed = ed.replace(tzinfo=timezone.utc)
                                        if pos_latest_end is None or ed > pos_latest_end:
                                            pos_latest_end = ed
                            if pos_latest_end is None:
                                continue
                            # Denominator adjustment: if event is postponed or end_date has
                            # already passed without resolution, push denominator to now+14d.
                            # Prevents artificially high remaining_score for stuck positions.
                            # This is a config-tunable horizon (postponement_rescore_days).
                            _postponement_rescore_days = config.get('arbitrage', {}).get('postponement_rescore_days', 14)
                            _rescore_floor = now_utc + timedelta(days=_postponement_rescore_days)
                            if pos.metadata.get('postponed') or pos_latest_end < now_utc:
                                # Use entry_timestamp (dataclass field) as reference baseline.
                                # Push denominator to max(original_end, now + 14d).
                                pos_latest_end = max(pos_latest_end, _rescore_floor)
                                log.debug(f'  {pid[:30]} postponed/past — denominator extended to {pos_latest_end.date()}')
                                # Entry time as reference; score over (now+14d - entry_time)
                                entry_time = pos.metadata.get('entry_time')
                                if entry_time:
                                    try:
                                        entry_dt = datetime.fromisoformat(entry_time).replace(tzinfo=timezone.utc) \
                                            if isinstance(entry_time, str) else entry_time
                                        if entry_dt.tzinfo is None:
                                            entry_dt = entry_dt.replace(tzinfo=timezone.utc)
                                        pos_latest_end = max(pos_latest_end, _rescore_floor)
                                        log.debug(f'  {pid[:30]} postponed/past — denominator extended to {pos_latest_end.date()}')
                                    except Exception:
                                        pos_latest_end = _rescore_floor
                                else:
                                    pos_latest_end = _rescore_floor
#>>>>>>> Stashed changes
                            hours_remaining = (pos_latest_end - now_utc).total_seconds() / 3600
                            if hours_remaining < 24:
                                log.debug(f'  Position {pid[:30]} protected: resolves in {hours_remaining:.1f}h')
                                continue
                            liq_value = calc_position_liq_value(pos, market_lookup)
                            unrealized_pnl = liq_value - pos.total_capital
                            remaining_upside = pos.expected_profit - unrealized_pnl
                            # Normalize to pct/hr to match opp scoring (profit_pct / effective_hours)
                            # Apply same delay model as rank_opportunities
                            pos_names = [m.get('name', '') for m in pos.markets.values()]
                            pos_mids = list(pos.markets.keys())
                            pos_category = classify_opportunity_category(pos_names, market_lookup, pos_mids)
                            pos_p95_table, pos_def_p95 = load_delay_table()
                            pos_p95 = pos_p95_table.get(pos_category, pos_def_p95)
                            pos_min_vol = 0.0
                            for mid_str in pos.markets.keys():
                                md = market_lookup.get(str(mid_str))
                                if md and hasattr(md, 'volume_24h'):
                                    v = md.volume_24h or 0
                                    pos_min_vol = v if pos_min_vol == 0 else min(pos_min_vol, v)
                            pos_vol_penalty = get_volume_penalty_hours(pos_min_vol)
                            effective_remaining = hours_remaining + pos_p95 + pos_vol_penalty
                            remaining_score = (remaining_upside / max(pos.total_capital, 0.01)) / max(effective_remaining, 0.01)
                            if remaining_score < worst_remaining_score:
                                worst_remaining_score = remaining_score
                                worst_pos = (pid, pos, remaining_score, hours_remaining, unrealized_pnl)

                        # Replace if best new is 20% better than worst held
                        if worst_pos:
                            wname_dbg = list(worst_pos[1].markets.values())[0].get('name', '?')[:30] if worst_pos[1].markets else '?'
                            bname_dbg = best_opp.get('market_names', ['?'])[0][:30]
                            log.debug(f'  Replacement check: best_opp={best_score:.6f}/hr "{bname_dbg}" vs worst_pos={worst_remaining_score:.6f}/hr "{wname_dbg}" (threshold={worst_remaining_score*1.2:.6f})')
                        if worst_pos and best_score > worst_remaining_score * 1.2:
                            wpid, wpos, wscore, whours, wpnl = worst_pos
                            wname = list(wpos.markets.values())[0].get('name', '?')[:40] if wpos.markets else '?'
                            bname = best_opp.get('market_names', ['?'])[0][:40]
                            log.info(f'[iter {iteration}] REPLACING: "{wname}" (score={wscore:.6f}/hr, {whours:.0f}h left)')
                            log.info(f'  WITH: "{bname}" (score={best_score:.6f}/hr, {best_hours:.0f}h, {best_opp.get("expected_profit_pct",0)*100:.1f}%)')
                            result = await engine.liquidate_position(wpid, market_lookup)
                            if result.get('success'):
                                log.info(f'  Liquidated: freed ${result["freed_capital"]:.2f}, realized ${result["actual_profit"]:+.2f}')
                                # Also liquidate live position if running
                                if live_engine and trading_mode in ('live_trading', 'dual'):
                                    live_meta = wpos.metadata.get('live', {})
                                    if live_meta.get('token_map'):
                                        try:
                                            live_liq = live_engine.liquidate_live_position(
                                                {mid: {'token_id': info.get('token_id',''), 'shares': info.get('shares',0)}
                                                 for mid, info in live_meta.get('position_markets', {}).items()},
                                                live_meta['token_map']
                                            )
                                            if live_liq.get('success'):
                                                log.info(f'  LIVE liquidated: ${live_liq["proceeds"]:.2f}')
                                            else:
                                                log.warning(f'  LIVE liquidation failed')
                                        except Exception as le:
                                            log.error(f'  LIVE liquidation error: {le}')
                                replacements_made += 1
                                used_opp_cids.add(best_opp.get('constraint_id', ''))  # Don't reuse same opp
                            else:
                                log.warning(f'  Liquidation failed: {result.get("reason")}')
                                break
                        else:
                            break  # No more profitable swaps

                    if replacements_made > 0:
                        last_replacement_time = now_ts
                        log.info(f'[iter {iteration}] Replacement round complete: {replacements_made} swaps')

                except Exception as e:
                    log.error(f'[iter {iteration}] Replacement check error: {e}', exc_info=True)

            # --- ENTER NEW POSITIONS ---
            slots = max_positions - len(engine.open_positions)
            if OPP_PATH.exists() and slots > 0:
                opps = json.loads(OPP_PATH.read_text()).get('opportunities', [])
                if opps:
                    cap = dynamic_capital(engine.current_capital, capital_pct)
                    ranked = rank_opportunities(opps, market_lookup, min_resolution_secs=300, max_days_to_resolution=max_days_to_resolution)
                    log.info(f'[iter {iteration}] {len(opps)} raw opps -> {len(ranked)} ranked, cap=${cap:.2f}')

                    if ranked:
                        for score, hours, _ in ranked[:3]:
                            log.debug(f'  Top: score={score:.4f}/hr, resolves {hours:.1f}h')

                    held_cids = get_held_constraint_ids(engine)
                    held_mids = get_held_market_ids(engine)

                    entered = 0
                    for score, hours, opp_dict in ranked:
                        if entered >= slots:
                            break
                        # Skip duplicate constraint
                        constraint_id = opp_dict.get('constraint_id', '')
                        if constraint_id in held_cids:
                            continue
                        # Skip if any market_id already held
                        if opportunity_overlaps_held(opp_dict, held_mids):
                            log.debug(f'  Skip market overlap: {constraint_id}')
                            continue

                        # --- AI Resolution Date + Outcome Validation ---
                        if resolution_validation_enabled and anthropic_api_key:
                            try:
                                market_ids = opp_dict.get('market_ids', [])
                                validation = get_full_validation(
                                    market_ids=market_ids,
                                    market_lookup=market_lookup,
                                    api_key=anthropic_api_key,
                                    group_id=constraint_id
                                )
                                if validation:
                                    # Check for unrepresented outcomes (e.g. "Other")
                                    if validation.get('has_unrepresented_outcome', False):
                                        reason = validation.get('unrepresented_outcome_reason', '')[:100]
                                        log.info(f'  SKIP (unrepresented outcome): {constraint_id} — {reason}')
                                        continue

                                    # Check resolution date
                                    try:
                                        validated_date = datetime.strptime(
                                            validation['latest_resolution_date'], '%Y-%m-%d'
                                        ).replace(hour=23, minute=59, second=59, tzinfo=timezone.utc)
                                        days_until = (validated_date - datetime.now(timezone.utc)).days
                                        if days_until > max_days_to_resolution:
                                            log.info(f'  SKIP (AI date): {constraint_id} '
                                                     f'resolves in {days_until}d > {max_days_to_resolution}d max')
                                            continue
                                        elif days_until != int(hours / 24):
                                            log.debug(f'  AI date: {validated_date.date()} '
                                                      f'({days_until}d vs API {hours/24:.0f}d)')
                                    except (ValueError, KeyError):
                                        pass  # Date parse failed, continue without
                            except Exception as ve:
                                log.warning(f'  Resolution validation error: {ve}')
                                # Fail open — proceed without validation

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
                            result = await engine.execute_opportunity(opp_dict, markets)
                            if result and result.get('success'):
                                log.info(f'[iter {iteration}] ✓ TRADE: {constraint_id} | ${cap:.2f} | '
                                         f'exp ${opp_dict.get("expected_profit",0):.2f} | '
                                         f'{hours:.1f}h | score={score:.4f}')
                                held_cids.add(constraint_id)
                                # Update held_mids with new position's markets
                                for mid in opp_dict.get('market_ids', []):
                                    held_mids.add(str(mid))
                                entered += 1
                                # WS: subscribe new position's assets for live depth + resolution
                                if ws_manager and ws_manager._running:
                                    try:
                                        new_assets = get_asset_ids_for_opportunity(opp_dict, market_lookup)
                                        if new_assets:
                                            await ws_manager.subscribe_assets(new_assets)
                                    except Exception:
                                        pass
                                # Shadow/Live execution (if enabled)
                                if live_engine and result and result.get('success'):
                                    if shadow_only:
                                        shadow_result = live_engine.shadow_trade(opp_dict)
                                        log.debug(f'  Shadow: {shadow_result.get("validation", {}).get("reason", "valid")}')
                                    elif trading_mode in ('live_trading', 'dual'):
                                        live_result = live_engine.execute_live_trade(opp_dict)
                                        if live_result.get('success'):
                                            log.info(f'  LIVE: ${live_result["total_cost"]:.2f} deployed, fees ${live_result["total_fees"]:.4f}')
                                            # Store live metadata in position for later liquidation/tracking
                                            pid = result.get('position_id', '')
                                            if pid and pid in engine.open_positions:
                                                engine.open_positions[pid].metadata['live'] = {
                                                    'token_map': live_result.get('token_map', {}),
                                                    'position_markets': live_result.get('position_markets', {}),
                                                    'total_cost': live_result['total_cost'],
                                                    'total_fees': live_result['total_fees'],
                                                    'entry_time': datetime.now(timezone.utc).isoformat()
                                                }
                                        else:
                                            log.warning(f'  LIVE failed: {live_result.get("reason", live_result.get("stage", "?"))}')
                            elif result:
                                reason = result.get('reason', '?')
                                if reason != 'insufficient_capital':
                                    log.debug(f'  Rejected: {reason}')
                        except Exception as e:
                            log.error(f'[iter {iteration}] Exec error: {e}', exc_info=True)

            # Save state
            STATE_DIR.mkdir(parents=True, exist_ok=True)
            engine.save_state(EXEC_STATE)
            metrics = engine.get_performance_metrics() if hasattr(engine, 'get_performance_metrics') else {}
            capital = metrics.get('current_capital', 0)
            write_status('running', capital, len(engine.open_positions))
            if iteration % 10 == 1:
                ws_info = ''
                if ws_manager and ws_manager._running:
                    ws_stats = ws_manager.get_stats()
                    ws_info = (f' | WS: subs={len(ws_manager._subscribed_assets)} '
                               f'msgs={ws_stats["market_msgs"]}')
                log.info(f'[iter {iteration}] Capital=${capital:.2f} positions={len(engine.open_positions)} '
                         f'dynamic_cap=${dynamic_capital(capital):.2f}{ws_info}')
            else:
                log.debug(f'[iter {iteration}] Capital=${capital:.2f} positions={len(engine.open_positions)}')

            # Check for weekly delay table update (~daily check, cheap)
            if iteration % 2880 == 0:  # ~24h at 30s intervals
                trigger_weekly_delay_update()

        except Exception as e:
            log.error(f'[iter {iteration}] Error: {e}', exc_info=True)
            write_status('error', 0, 0, str(e))

        await asyncio.sleep(check_interval)

    # Cleanup (if loop ever exits)
    if ws_manager:
        await ws_manager.stop()

if __name__ == '__main__':
    asyncio.run(main())
