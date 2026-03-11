"""Initial Market Scanner — runs once at startup before Trading Engine.

Sequence:
  1. Check for postponed positions (AI web search) — so replacement scoring has fresh dates
  2. Fetch all active markets from Polymarket Gamma API → latest_markets.json
  3. Exit cleanly (Trading Engine handles everything via WebSocket after this)
"""
import json, logging, os, sys, time, yaml
from pathlib import Path
from datetime import datetime, timezone
from zoneinfo import ZoneInfo

sys.path.append(str(Path(__file__).parent.parent))  # project root

WORKSPACE = Path('/home/andydoc/prediction-trader')
CONFIG_PATH = WORKSPACE / 'config' / 'config.yaml'
SECRETS_PATH = WORKSPACE / 'config' / 'secrets.yaml'
OUTPUT_PATH = WORKSPACE / 'data' / 'latest_markets.json'
STATUS_PATH = WORKSPACE / 'data' / 'layer1_status.json'
EXEC_STATE_DB = WORKSPACE / 'data' / 'system_state' / 'execution_state.db'
EXEC_STATE_JSON = WORKSPACE / 'data' / 'system_state' / 'execution_state.json'

logging.basicConfig(level=logging.DEBUG,
    format='%(asctime)s - [SCANNER] %(levelname)s - %(message)s',
    handlers=[
        logging.FileHandler(str(
            WORKSPACE / 'logs' / f'scanner_{datetime.now().strftime("%Y%m%d")}.log'
        )),
        logging.StreamHandler(),
    ])
log = logging.getLogger('scanner')


def write_status(status, count=0, error=None):
    STATUS_PATH.parent.mkdir(parents=True, exist_ok=True)
    STATUS_PATH.write_text(json.dumps({
        'status': status, 'market_count': count, 'error': error,
        'timestamp': datetime.now(ZoneInfo('Europe/London')).isoformat()
    }))


# === Phase 1: Postponement Detection ===

def check_postponements():
    """Check all overdue open positions for postponed events.
    Runs synchronously — called before market scan."""
    with open(CONFIG_PATH) as f:
        config = yaml.safe_load(f)
    
    ai_config = config.get('ai', {})
    pp_cfg = ai_config.get('postponement', {})
    if not pp_cfg.get('enabled', True):
        log.info('Postponement detection disabled in config')
        return
    
    overdue_threshold_h = pp_cfg.get('overdue_threshold_hours', 24)
    now = datetime.now(timezone.utc)
    
    # Load execution state to find open positions
    state = None
    if EXEC_STATE_DB.exists():
        try:
            from utilities.state_db import read_state_from_disk
            state = read_state_from_disk(str(EXEC_STATE_DB))
        except Exception:
            pass
    if not state and EXEC_STATE_JSON.exists():
        try:
            state = json.loads(EXEC_STATE_JSON.read_text())
        except Exception:
            pass
    if not state:
        log.info('No execution state found — skipping postponement check')
        return
    
    open_pos = state.get('open_positions', [])
    if not open_pos:
        log.info('No open positions — skipping postponement check')
        return
    
    # Load market data for end_date lookup
    market_lookup = {}
    if OUTPUT_PATH.exists():
        try:
            mdata = json.loads(OUTPUT_PATH.read_text())
            for m in mdata.get('markets', []):
                market_lookup[str(m.get('market_id', ''))] = m
        except Exception:
            pass
    
    from utilities.postponement_detector import check_postponement
    checked = 0
    state_modified = False
    
    for p in open_pos:
        # Find expected resolution date
        expected_date_str = None
        for mid in p.get('markets', {}):
            md = market_lookup.get(str(mid), {})
            ed = md.get('end_date', md.get('endDate', ''))
            if ed:
                expected_date_str = str(ed)[:10]
                break
        
        if not expected_date_str:
            continue
        
        try:
            expected_dt = datetime.strptime(expected_date_str, '%Y-%m-%d').replace(
                tzinfo=timezone.utc)
            hours_overdue = (now - expected_dt).total_seconds() / 3600
        except Exception:
            continue
        
        if hours_overdue < overdue_threshold_h:
            continue
        
        pos_id = p.get('position_id', '')
        market_names = [m.get('name', '?') for m in p.get('markets', {}).values()]
        
        log.info(f'Position overdue ({hours_overdue:.0f}h): {market_names[0][:50]}...')
        result = check_postponement(
            position_id=pos_id,
            market_names=market_names,
            original_date=expected_date_str,
            ai_config=ai_config,
        )
        checked += 1
        
        if result and result.get('effective_resolution_date'):
            log.info(f'  → {result.get("status")}: new_date={result.get("new_date")} '
                     f'effective={result.get("effective_resolution_date")}')
            # Immediately apply to state so positions are rescored on next engine start
            p.setdefault('metadata', {})['postponement'] = {
                'status': result.get('status'),
                'new_date': result.get('new_date'),
                'effective_date': result.get('effective_resolution_date'),
                'confidence': result.get('date_confidence'),
                'reason': result.get('reason', ''),
                'checked_at': result.get('checked_at'),
            }
            state_modified = True
    
    # Write modified state back to disk
    if state_modified and state:
        try:
            if EXEC_STATE_JSON.exists():
                EXEC_STATE_JSON.write_text(json.dumps(state, indent=2))
                log.info(f'Wrote postponement metadata to execution_state.json')
        except Exception as e:
            log.error(f'Failed to write back state: {e}')
    
    if checked > 0:
        log.info(f'Postponement check complete: {checked} overdue positions scanned')
    else:
        log.info('No overdue positions found')


# === Phase 2: Market Scan (synchronous REST, no asyncio) ===

def scan_markets():
    """Fetch all markets from Polymarket Gamma API using simple REST calls.
    No asyncio — just requests.get() in a pagination loop."""
    import requests as req
    
    log.info('Market scan starting...')
    write_status('scanning')
    
    api_url = 'https://gamma-api.polymarket.com/markets'
    all_markets = []
    offset = 0
    limit = 500  # Gamma API max per page
    
    while True:
        try:
            resp = req.get(api_url, params={
                'limit': limit,
                'offset': offset,
                'active': 'true',
                'closed': 'false',
            }, timeout=30)
            
            if not resp.ok:
                log.warning(f'Gamma API returned {resp.status_code} at offset {offset}')
                break
            
            batch = resp.json()
            if not batch:
                break
            
            all_markets.extend(batch)
            log.debug(f'  fetched {len(batch)} markets (total: {len(all_markets)}, offset: {offset})')
            
            if len(batch) < limit:
                break  # Last page
            offset += limit
            
        except Exception as e:
            log.error(f'Gamma API error at offset {offset}: {e}')
            break
    
    if not all_markets:
        log.error('No markets fetched — check Gamma API connectivity')
        write_status('error', 0, 'No markets fetched')
        return False
    
    # Convert raw API data to MarketData objects
    from market_data.market_data import MarketData
    market_objects = []
    skipped = 0
    for m in all_markets:
        try:
            md = MarketData.from_api_response(m)
            market_objects.append(md)
        except Exception as e:
            skipped += 1
            if skipped <= 3:
                log.debug(f'  Skip market {m.get("id","?")}: {e}')
    
    if skipped:
        log.info(f'  Skipped {skipped} unparseable markets')
    
    OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    market_dicts = [m.to_dict() if hasattr(m, 'to_dict') else m for m in market_objects]
    OUTPUT_PATH.write_text(json.dumps({
        'markets': market_dicts,
        'count': len(market_dicts),
        'timestamp': datetime.now(ZoneInfo('Europe/London')).isoformat(),
    }))
    
    log.info(f'Scan complete: {len(market_dicts)} markets written to {OUTPUT_PATH.name}')
    write_status('done', len(market_dicts))
    return True


# === Entry point ===

def main():
    log.info('=' * 50)
    log.info('INITIAL MARKET SCANNER — startup sequence')
    log.info('=' * 50)
    
    # Phase 1: Postponement detection (before market scan)
    try:
        check_postponements()
    except Exception as e:
        log.error(f'Postponement check failed: {e}', exc_info=True)
        # Non-fatal — continue to market scan
    
    # Phase 2: Market scan
    try:
        success = scan_markets()
        if not success:
            log.warning('Market scan failed — engine will start with stale data')
            sys.exit(1)
    except Exception as e:
        log.error(f'Market scan failed: {e}', exc_info=True)
        sys.exit(1)
    
    log.info('Scanner complete — exiting cleanly')


if __name__ == '__main__':
    main()
