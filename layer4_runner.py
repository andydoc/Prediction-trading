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
from paper_trading_complete import CompletePaperTradingEngine
from live_trading_engine import LiveTradingEngine
from layer1_market_data.market_data import MarketData

WORKSPACE    = Path('/home/andydoc/prediction-trader')
CONFIG_PATH  = WORKSPACE / 'config' / 'config.yaml'
OPP_PATH     = WORKSPACE / 'layer3_arbitrage_math' / 'data' / 'latest_opportunities.json'
MARKETS_PATH = WORKSPACE / 'data' / 'latest_markets.json'
STATUS_PATH  = WORKSPACE / 'data' / 'layer4_status.json'
STATE_DIR    = WORKSPACE / 'data' / 'system_state'
EXEC_STATE   = STATE_DIR / 'execution_state.json'

logging.basicConfig(level=logging.DEBUG,
    format='%(asctime)s - [L4] %(levelname)s - %(message)s',
    handlers=[logging.FileHandler(str(WORKSPACE / 'logs' / f'layer4_{datetime.now().strftime("%Y%m%d")}.log')), logging.StreamHandler()])
log = logging.getLogger('layer4')

def write_status(status, capital=0, open_pos=0, error=None):
    STATUS_PATH.parent.mkdir(parents=True, exist_ok=True)
    STATUS_PATH.write_text(json.dumps({
        'status': status, 'capital': capital, 'open_positions': open_pos,
        'error': error, 'timestamp': datetime.now(ZoneInfo('Europe/London')).isoformat()
    }))

def dynamic_capital(current_balance: float) -> float:
    return max(10.0, min(current_balance * 0.1, 1000.0))

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

def rank_opportunities(opps: list, market_lookup: dict, min_resolution_secs: int = 300) -> list:
    scored = []
    for opp in opps:
        hours = get_resolution_hours(opp, market_lookup)
        if hours is None:
            continue
        if hours < 0:
            log.debug(f"  Filtered out {opp.get('opportunity_id','?')}: end_date in past")
            continue
        if hours * 3600 < min_resolution_secs:
            continue
        profit_pct = opp.get('expected_profit_pct', 0)
        score = profit_pct / max(hours, 0.01)
        scored.append((score, hours, opp))
    scored.sort(key=lambda x: x[0], reverse=True)
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

def opp_overlaps_held(opp_dict, held_market_ids) -> bool:
    """Check if opportunity shares any market_id with held positions."""
    for mid in opp_dict.get('market_ids', []):
        if str(mid) in held_market_ids:
            return True
    return False

async def main():
    with open(CONFIG_PATH) as f:
        config = yaml.safe_load(f)
    max_positions = config.get('arbitrage', {}).get('max_concurrent_positions', 20)
    check_interval = 30
    replacement_cooldown_secs = 60   # Min 60s between replacement rounds

    engine = CompletePaperTradingEngine(config, WORKSPACE)
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
    iteration = 0
    last_replacement_time = 0.0  # Unix timestamp of last replacement

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

            # Monitor open positions
            if engine.open_positions:
                log.debug(f'[iter {iteration}] Monitoring {len(engine.open_positions)} open positions')
                await engine.monitor_positions(markets)

            # --- POSITION REPLACEMENT (loop: replace ALL underperformers each round) ---
            now_ts = _time.time()
            if (engine.open_positions and OPP_PATH.exists()
                    and (now_ts - last_replacement_time) >= replacement_cooldown_secs):
                try:
                    opp_text = OPP_PATH.read_text()
                    if not opp_text.strip():
                        raise ValueError("Empty opportunities file (L3 writing)")
                    opps_for_replace = json.loads(opp_text).get('opportunities', [])
                    ranked_for_replace = rank_opportunities(opps_for_replace, market_lookup, min_resolution_secs=300)
                    now_utc = datetime.now(timezone.utc)
                    replacements_made = 0
                    max_replacements_per_round = 5

                    while replacements_made < max_replacements_per_round:
                        held_cids = get_held_constraint_ids(engine)
                        held_mids = get_held_market_ids(engine)

                        # Find best untraded opportunity
                        best_new = None
                        for score, hours, opp_d in ranked_for_replace:
                            cid = opp_d.get('constraint_id', '')
                            if cid in held_cids:
                                continue
                            if opp_overlaps_held(opp_d, held_mids):
                                continue
                            best_new = (score, hours, opp_d)
                            break

                        if not best_new:
                            break
                        best_score, best_hours, best_opp = best_new

                        # Score all open positions, find worst (skip <24h)
                        worst_pos = None
                        worst_remaining_score = float('inf')
                        for pid, pos in engine.open_positions.items():
                            pos_latest_end = None
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
                            hours_remaining = (pos_latest_end - now_utc).total_seconds() / 3600
                            if hours_remaining < 24:
                                continue
                            liq_value = 0.0
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
                                liq_value += shares * cur_p
                            unrealized_pnl = liq_value - pos.total_capital
                            remaining_upside = pos.expected_profit - unrealized_pnl
                            # Normalize to pct/hr to match opp scoring (profit_pct / hours)
                            remaining_score = (remaining_upside / max(pos.total_capital, 0.01)) / max(hours_remaining, 0.01)
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
                    cap = dynamic_capital(engine.current_capital)
                    ranked = rank_opportunities(opps, market_lookup, min_resolution_secs=300)
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
                        if opp_overlaps_held(opp_dict, held_mids):
                            log.debug(f'  Skip market overlap: {constraint_id}')
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
                log.info(f'[iter {iteration}] Capital=${capital:.2f} positions={len(engine.open_positions)} '
                         f'dynamic_cap=${dynamic_capital(capital):.2f}')
            else:
                log.debug(f'[iter {iteration}] Capital=${capital:.2f} positions={len(engine.open_positions)}')

        except Exception as e:
            log.error(f'[iter {iteration}] Error: {e}', exc_info=True)
            write_status('error', 0, 0, str(e))

        await asyncio.sleep(check_interval)

if __name__ == '__main__':
    asyncio.run(main())
