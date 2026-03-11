"""Dashboard — Fully dynamic SSE dashboard.
All data served as JSON via typed SSE events. HTML is a static shell with JS rendering.
No page reloads needed — positions, opportunities, aggregates, closed positions all update live.
Run: python -m utilities.dashboard_server
Serves on http://localhost:5556
"""
import json, os, sys, html as html_mod, time, threading, hashlib, glob
from pathlib import Path
from datetime import datetime, timezone
from http.server import HTTPServer, BaseHTTPRequestHandler
from socketserver import ThreadingMixIn
from collections import defaultdict

WORKSPACE = Path('/home/andydoc/prediction-trader')
CONFIG_PATH = WORKSPACE / 'config' / 'config.yaml'
DATA = WORKSPACE / 'data'
OPP_PATH = WORKSPACE / 'arbitrage_math' / 'data' / 'latest_opportunities.json'
EXEC_STATE = DATA / 'system_state' / 'execution_state.json'
EXEC_STATE_DB = DATA / 'system_state' / 'execution_state.db'
ENGINE_STATUS = DATA / 'trading_engine_status.json'
START_TIME = datetime.now(timezone.utc)

def load_config():
    try:
        import yaml
        return yaml.safe_load(CONFIG_PATH.read_text())
    except:
        return {}

def load_json(p):
    try:
        return json.loads(Path(p).read_text())
    except:
        return {}

def load_execution_state():
    """Load execution state: SQLite primary, JSON fallback."""
    if EXEC_STATE_DB.exists():
        try:
            from utilities.state_db import read_state_from_disk
            result = read_state_from_disk(str(EXEC_STATE_DB))
            if result:
                return result
        except Exception:
            pass
    return load_json(EXEC_STATE)

def _load_markets_dict():
    """Load market data as {market_id_str: market_dict}."""
    market_data = load_json(DATA / 'latest_markets.json')
    return {str(m.get('market_id', m.get('id', ''))): m
            for m in market_data.get('markets', [])
            if m.get('market_id') or m.get('id')}

def _load_resolution_cache():
    """Load AI resolution validation cache."""
    cache = {}
    cache_dir = DATA / 'resolution_cache'
    if cache_dir.exists():
        for cf in cache_dir.glob('*.json'):
            try:
                cd = json.loads(cf.read_text())
                if 'latest_resolution_date' in cd:
                    cache[cf.stem] = cd
            except:
                pass
    return cache

def _get_mode_info():
    """Return (mode_label, mode_class) from config."""
    cfg = load_config()
    live_cfg = cfg.get('live_trading', {})
    shadow_only = live_cfg.get('shadow_only', True)
    if shadow_only:
        return 'SHADOW', 'mode-shadow'
    return 'LIVE', 'mode-live'

def _get_first_trade_dt(state):
    """Find earliest entry_timestamp across all positions."""
    first = None
    op = state.get('open_positions', [])
    all_pos = state.get('closed_positions', []) + (list(op.values()) if isinstance(op, dict) else op)
    for p in all_pos:
        ts = p.get('entry_timestamp', '') if isinstance(p, dict) else ''
        if ts:
            try:
                dt = datetime.fromisoformat(str(ts)[:19])
                if dt.tzinfo is None:
                    dt = dt.replace(tzinfo=timezone.utc)
                if first is None or dt < first:
                    first = dt
            except:
                pass
    return first or START_TIME

def get_stats_json():
    """Top-level metrics for the header stats bar."""
    state = load_execution_state()
    now = datetime.now(timezone.utc)
    cap = state.get('current_capital', 0)
    init_cap = state.get('initial_capital', 100)
    open_pos = state.get('open_positions', [])
    closed_pos = state.get('closed_positions', [])
    perf = state.get('performance', {})

    deployed = sum(sum(m.get('bet_amount', 0) for m in p.get('markets', {}).values()) for p in open_pos)
    total_fees = sum(p.get('fees_paid', 0) for p in open_pos) + sum(p.get('fees_paid', 0) for p in closed_pos)
    total_value = cap + deployed
    ret_pct = ((total_value - init_cap) / init_cap * 100) if init_cap else 0
    trades = perf.get('total_trades', 0)
    total_realized = sum(p.get('actual_profit', 0) for p in closed_pos if p.get('actual_profit') is not None)
    repl_count = sum(1 for p in closed_pos if p.get('metadata', {}).get('close_reason') == 'replaced')
    repl_cost = sum(p.get('fees_paid', 0) for p in closed_pos if p.get('metadata', {}).get('close_reason') == 'replaced')
    repl_cost += sum(abs(p.get('actual_profit', 0)) for p in closed_pos if p.get('metadata', {}).get('close_reason') == 'replaced' and p.get('actual_profit', 0) < 0)

    # Annualized return
    first_trade_dt = _get_first_trade_dt(state)
    elapsed_days = (now - first_trade_dt).total_seconds() / 86400
    annualized_ret = 0.0
    annualized_str = 'N/A'
    if elapsed_days > 0.01 and init_cap > 0:
        annualized_ret = ((total_value - init_cap) / init_cap) * (365.0 / elapsed_days) * 100
        annualized_str = f'{annualized_ret:+.0f}%'

    # Engine metrics
    engine_status = load_json(ENGINE_STATUS)
    em = engine_status.get('metrics', {})
    ws_live = em.get('ws_live', 0)
    ws_total = em.get('markets_total', 1)
    ws_pct = int(ws_live / ws_total * 100) if ws_total else 0

    mode_label, mode_class = _get_mode_info()
    # Live USDC balance (cached, refreshed in background)
    live_balance = _get_live_balance()

    return {
        'cash': cap, 'deployed': deployed, 'total_value': total_value,
        'init_cap': init_cap, 'fees': total_fees, 'ret_pct': ret_pct,
        'trades': trades, 'open_count': len(open_pos),
        'closed_count': len(closed_pos),
        'realized': total_realized, 'annualized': annualized_str,
        'annualized_ret': annualized_ret,
        'repl_count': repl_count, 'repl_cost': repl_cost,
        'mode_label': mode_label, 'mode_class': mode_class,
        'first_trade': first_trade_dt.strftime('%d/%m/%Y %H:%M'),
        'start_time': START_TIME.strftime('%d/%m/%Y %H:%M'),
        'live_balance': live_balance,
        'timestamp': now.strftime('%d/%m/%Y %H:%M:%S'),
        # Engine/WS metrics
        'ws_subs': em.get('ws_subscribed', 0),
        'ws_msgs': em.get('ws_msgs', 0),
        'ws_live': ws_live, 'ws_total': ws_total, 'ws_pct': ws_pct,
        'q_bg': em.get('queue_background', 0),
        'q_urg': em.get('queue_urgent', 0),
        'lat_p50': em.get('lat_p50_ms', 0),
        'lat_p95': em.get('lat_p95_ms', 0),
        'lat_max': em.get('lat_max_ms', 0),
        'has_rust': em.get('has_rust', False),
        'constraints': em.get('constraints', 0),
        'iteration': em.get('iteration', 0),
    }

_live_balance_cache = {'value': None, 'ts': 0}

def _get_live_balance():
    """Get USDC balance, cached for 60s."""
    now = time.time()
    if now - _live_balance_cache['ts'] < 60:
        return _live_balance_cache['value']
    try:
        import yaml as _yaml
        from py_clob_client.client import ClobClient
        from py_clob_client.clob_types import BalanceAllowanceParams, AssetType
        secrets_path = WORKSPACE / 'config' / 'secrets.yaml'
        with open(secrets_path) as _f:
            _secrets = _yaml.safe_load(_f)['polymarket']
        _client = ClobClient(
            _secrets['host'], key=_secrets['private_key'],
            chain_id=_secrets['chain_id'],
            signature_type=_secrets['signature_type'],
            funder=_secrets['funder_address']
        )
        _creds = _client.create_or_derive_api_creds()
        _client.set_api_creds(_creds)
        _params = BalanceAllowanceParams(asset_type=AssetType.COLLATERAL)
        _bal = _client.get_balance_allowance(_params)
        val = float(_bal.get('balance', 0)) / 1e6
        _live_balance_cache['value'] = val
        _live_balance_cache['ts'] = now
        return val
    except Exception:
        _live_balance_cache['ts'] = now
        return _live_balance_cache['value']

def get_positions_json():
    """Open positions + aggregate holdings as JSON for SSE."""
    now = datetime.now(timezone.utc)
    state = load_execution_state()
    open_pos = state.get('open_positions', [])
    markets = _load_markets_dict()
    res_cache = _load_resolution_cache()

    positions = []
    agg_markets = defaultdict(lambda: {'name': '', 'total_bet': 0.0, 'pos_idx': 0,
                                        'side': 'YES', 'entry_prices': []})

    for idx, p in enumerate(open_pos, 1):
        total_cap = p.get('total_capital', 0)
        exp_profit = p.get('expected_profit', 0)
        exp_pct = p.get('expected_profit_pct', 0) * 100
        entry_ts = p.get('entry_timestamp', '')
        status = p.get('status', '?')
        opp_id = p.get('opportunity_id', '')
        is_sell = 'sell' in opp_id
        pos_meta = p.get('metadata', {})
        method = pos_meta.get('method', '')

        # Strategy label
        if method:
            strategy = method.replace('_', ' ').title()
        elif 'buy_mutex' in opp_id:
            strategy = 'Mutex Buy All'
        elif 'sell_mutex' in opp_id:
            strategy = 'Mutex Sell All'
        else:
            strategy = opp_id.split('_')[1].title() if '_' in opp_id else '?'

        # Resolution date
        pos_resolve_dt = None
        for mid_str in p.get('markets', {}).keys():
            mkt = markets.get(str(mid_str))
            if mkt:
                try:
                    ed = datetime.fromisoformat(mkt['end_date'])
                    if ed.tzinfo is None:
                        ed = ed.replace(tzinfo=timezone.utc)
                    if pos_resolve_dt is None or ed > pos_resolve_dt:
                        pos_resolve_dt = ed
                except:
                    pass

        # Override with AI-detected postponement date
        pp = pos_meta.get('postponement', {}) or {}
        postpone_info = None
        eff_str = pp.get('effective_date', '')
        if eff_str:
            try:
                eff_dt = datetime.fromisoformat(eff_str)
                if eff_dt.tzinfo is None:
                    eff_dt = eff_dt.replace(tzinfo=timezone.utc)
                pos_resolve_dt = eff_dt
                postpone_info = {'reason': pp.get('reason', 'postponed')[:80]}
            except:
                pass

        # Score: profit_pct / hours_to_resolve * 10000
        resolve_str = pos_resolve_dt.strftime('%d/%m/%Y') if pos_resolve_dt else '?'
        if pos_resolve_dt:
            pos_hours = max((pos_resolve_dt - now).total_seconds() / 3600, 0.01)
            score = (p.get('expected_profit_pct', 0) / pos_hours) * 10000
        else:
            score = 0.0

        # Build legs data
        mkt_vals = list(p.get('markets', {}).items())
        legs = []
        shares_map = {}
        for mid, mdata in mkt_vals:
            name = mdata.get('name', '?')
            bet = mdata.get('bet_amount', 0)
            ep = mdata.get('entry_price', 0)
            if is_sell:
                no_price = 1.0 - ep
                shares = bet / no_price if no_price > 0 else 0
                side = 'NO'
            else:
                shares = bet / ep if ep > 0 else 0
                no_price = ep
                side = 'YES'
            shares_map[mid] = shares
            legs.append({
                'mid': mid, 'name': name, 'bet': bet, 'price': ep,
                'shares': round(shares, 2),
                'payout': round(shares, 2),
                'side': side,
            })
            # Aggregate tracking
            a = agg_markets[mid]
            a['name'] = name[:65]
            a['total_bet'] += bet
            a['pos_idx'] = idx
            a['side'] = 'NO' if is_sell else 'YES'
            a['entry_prices'].append(ep)

        # Scenarios (for sell arbs: each leg could win YES, losing that NO leg)
        scenarios = []
        guaranteed = 0
        if is_sell and len(mkt_vals) > 1:
            scenario_payouts = []
            for wmid, wmd in mkt_vals:
                wname = wmd.get('name', '?')[:40]
                winners = [(m, shares_map[m]) for m, _ in mkt_vals if m != wmid]
                payout = sum(s for _, s in winners)
                profit = payout - total_cap
                scenarios.append({
                    'winner': wname, 'payout': round(payout, 2),
                    'profit': round(profit, 2),
                    'parts': [round(s, 2) for _, s in winners],
                })
                scenario_payouts.append(payout)
            guaranteed = min(scenario_payouts) if scenario_payouts else 0
        else:
            # Buy arb: guaranteed = min(bet/price) across legs
            all_payouts = [l['payout'] for l in legs]
            guaranteed = min(all_payouts) if all_payouts else 0

        short_name = mkt_vals[0][1].get('name', '?')[:40] if mkt_vals else '?'
        full_names = [v.get('name', '?') for _, v in mkt_vals]

        positions.append({
            'idx': idx, 'short_name': short_name, 'full_names': full_names,
            'strategy': strategy, 'score': round(score, 2),
            'total_cap': round(total_cap, 2),
            'exp_profit': round(exp_profit, 2), 'exp_pct': round(exp_pct, 1),
            'resolve': resolve_str, 'postpone': postpone_info,
            'status': status, 'entry_ts': entry_ts[:16] if entry_ts else '?',
            'is_sell': is_sell, 'legs': legs, 'scenarios': scenarios,
            'guaranteed': round(guaranteed, 2),
            'sort_ts': pos_resolve_dt.isoformat() if pos_resolve_dt else '9999-01-01',
        })

    # Sort positions by resolution date
    positions.sort(key=lambda x: x['sort_ts'])

    # Build aggregates
    aggregates = []
    agg_total = 0
    for mid, a in sorted(agg_markets.items(), key=lambda x: x[1]['pos_idx']):
        avg_price = sum(a['entry_prices']) / len(a['entry_prices']) if a['entry_prices'] else 0
        if a['side'] == 'NO':
            payout = a['total_bet'] / (1.0 - avg_price) if avg_price < 1 else 0
        else:
            payout = a['total_bet'] / avg_price if avg_price > 0 else 0
        agg_total += a['total_bet']
        aggregates.append({
            'name': a['name'], 'side': a['side'],
            'total_bet': round(a['total_bet'], 2),
            'avg_price': round(avg_price, 3),
            'payout': round(payout, 2),
            'pos_idx': a['pos_idx'],
        })

    return {
        'positions': positions,
        'aggregates': aggregates,
        'agg_total': round(agg_total, 2),
        'agg_market_count': len(agg_markets),
        'pos_count': len(open_pos),
    }

def get_opportunities_json():
    """Top opportunities scored and sorted, as JSON."""
    now = datetime.now(timezone.utc)
    markets = _load_markets_dict()
    res_cache = _load_resolution_cache()
    opp_data = load_json(OPP_PATH)
    opps = opp_data.get('opportunities', [])

    # Collect open position market IDs
    state = load_execution_state()
    open_pos = state.get('open_positions', [])
    open_mids = set()
    for p in open_pos:
        for mid in p.get('markets', {}).keys():
            open_mids.add(str(mid))

    result = []
    for opp in opps:
        profit_pct = opp.get('expected_profit_pct', 0) * 100
        cid = opp.get('constraint_id', '')
        latest = None
        all_past = True
        for mid in opp.get('market_ids', []):
            m = markets.get(str(mid))
            if m:
                try:
                    ed = datetime.fromisoformat(m['end_date'])
                    if ed.tzinfo is None:
                        ed = ed.replace(tzinfo=timezone.utc)
                    if (ed - now).total_seconds() > 0:
                        all_past = False
                    if latest is None or ed > latest:
                        latest = ed
                except:
                    pass

        # Resolution time + AI validation
        score = 0
        resolve_str = '?'
        validated = False
        if latest:
            delta_h = (latest - now).total_seconds() / 3600
            if delta_h < 0 or all_past:
                resolve_str = 'PAST'
                score = -1
            else:
                days = delta_h / 24
                # Check AI validation cache
                first_mid = str(opp.get('market_ids', [''])[0]) if opp.get('market_ids') else ''
                gid = cid or first_mid
                ckey = hashlib.md5(str(gid).encode()).hexdigest()[:12]
                cv = res_cache.get(ckey)
                if cv and 'latest_resolution_date' in cv:
                    try:
                        vd = datetime.strptime(cv['latest_resolution_date'], '%Y-%m-%d')
                        vd = vd.replace(hour=23, minute=59, second=59, tzinfo=timezone.utc)
                        delta_h = (vd - now).total_seconds() / 3600
                        days = delta_h / 24
                    except:
                        pass
                    validated = True
                resolve_str = f'{days:.1f}d' if days > 1 else f'{delta_h:.1f}h'
                score = (opp.get('expected_profit_pct', 0) / max(delta_h, 0.01))

        # Build legs
        method = opp.get('metadata', {}).get('method', '')
        is_sell = 'sell' in method
        strategy = method.replace('_', ' ').title() if method else '?'
        opp_prices = opp.get('current_prices', {})
        opp_bets = opp.get('optimal_bets', {})
        opp_names = opp.get('market_names', [])
        opp_mids = opp.get('market_ids', [])
        opp_cap = opp.get('total_capital_required', 10)
        is_held = any(str(m) in open_mids for m in opp_mids)

        legs = []
        for k, omid in enumerate(opp_mids):
            name = opp_names[k] if k < len(opp_names) else '?'
            price = opp_prices.get(str(omid), 0)
            bet = opp_bets.get(str(omid), 0)
            if is_sell:
                no_p = 1.0 - price
                shares = bet / no_p if no_p > 0 else 0
                side = 'NO'
            else:
                shares = bet / price if price > 0 else 0
                side = 'YES'
                no_p = price
            legs.append({
                'mid': str(omid), 'name': name, 'bet': round(bet, 2),
                'price': round(price, 3), 'shares': round(shares, 1),
                'payout': round(shares, 2), 'side': side,
            })

        # Scenarios for sell arbs
        scenarios = []
        if is_sell and len(opp_mids) > 1:
            sh = {}
            for k2, omid2 in enumerate(opp_mids):
                np2 = 1.0 - opp_prices.get(str(omid2), 0)
                sh[omid2] = opp_bets.get(str(omid2), 0) / np2 if np2 > 0 else 0
            for k3, wmid in enumerate(opp_mids):
                wname = opp_names[k3][:40] if k3 < len(opp_names) else '?'
                winners = [(m, sh[m]) for m in opp_mids if m != wmid]
                payout = sum(s for _, s in winners)
                scenarios.append({
                    'winner': wname, 'payout': round(payout, 2),
                    'profit': round(payout - opp_cap, 2),
                })

        opp_net = opp.get('net_profit', opp.get('expected_profit', 0))
        opp_fees = opp.get('fees_estimated', 0)
        short_name = opp_names[0][:50] if opp_names else '?'

        result.append({
            'idx': len(result) + 1, 'profit_pct': round(profit_pct, 1),
            'resolve': resolve_str, 'validated': validated,
            'strategy': strategy, 'score': round(score * 10000, 2),
            'short_name': short_name, 'full_names': opp_names,
            'is_held': is_held, 'is_past': score < 0,
            'is_sell': is_sell, 'legs': legs, 'scenarios': scenarios,
            'total_cap': round(opp_cap, 2),
            'net_profit': round(opp_net, 2), 'fees': round(opp_fees, 4),
            'guaranteed_payout': round(opp_cap + opp_net, 2),
        })

    result.sort(key=lambda x: x.get('score', 0) if x.get('score', 0) >= 0 else -999999, reverse=True)
    # Re-index after sort
    for i, r in enumerate(result):
        r['idx'] = i + 1
    return {'opportunities': result[:20], 'total_found': len(opps)}

def get_closed_json():
    """Closed positions categorised into resolved/replaced."""
    state = load_execution_state()
    closed_pos = state.get('closed_positions', [])
    cats = {'resolved': [], 'replaced_profit': [], 'replaced_loss': [], 'replaced_even': []}

    for p in closed_pos:
        actual = p.get('actual_profit', 0)
        cl_deployed = p.get('total_capital', 0)
        mkt_vals = list(p.get('markets', {}).values())
        short_name = mkt_vals[0].get('name', '?')[:40] if mkt_vals else '?'
        entry_iso = p.get('entry_timestamp', '')
        close_ts = p.get('close_timestamp', 0)
        hold_str = '?'
        reason = p.get('metadata', {}).get('close_reason', 'resolved')

        if entry_iso and close_ts:
            try:
                entry_dt = datetime.fromisoformat(str(entry_iso))
                if entry_dt.tzinfo is None:
                    entry_dt = entry_dt.replace(tzinfo=timezone.utc)
                secs = close_ts - entry_dt.timestamp()
                if secs < 3600:
                    hold_str = f'{secs/60:.0f}m'
                elif secs < 86400:
                    hold_str = f'{secs/3600:.1f}h'
                else:
                    hold_str = f'{secs/86400:.1f}d'
            except:
                pass

        row = {
            'name': short_name, 'deployed': round(cl_deployed, 2),
            'pnl': round(actual, 2), 'hold': hold_str,
        }
        if reason in ('resolved', 'expired'):
            cats['resolved'].append(row)
        elif reason == 'replaced' and actual > 0.01:
            cats['replaced_profit'].append(row)
        elif reason == 'replaced' and actual < -0.01:
            cats['replaced_loss'].append(row)
        else:
            cats['replaced_even'].append(row)

    return {
        'categories': {
            'resolved': {'label': 'Resolved', 'rows': cats['resolved'], 'collapsed': False},
            'replaced_profit': {'label': 'Replaced with Profit', 'rows': cats['replaced_profit'], 'collapsed': False},
            'replaced_loss': {'label': 'Replaced with Loss', 'rows': cats['replaced_loss'], 'collapsed': False},
            'replaced_even': {'label': 'Replaced at Breakeven', 'rows': cats['replaced_even'], 'collapsed': True},
        },
        'total_closed': len(closed_pos),
    }

def get_system_json():
    """Process statuses for system section."""
    now = datetime.now(timezone.utc)
    l1_status = load_json(DATA / 'layer1_status.json')
    engine_status = load_json(ENGINE_STATUS)
    em = engine_status.get('metrics', {})

    return {
        'scanner': {
            'status': l1_status.get('status', '?'),
            'ts': l1_status.get('timestamp', ''),
        },
        'engine': {
            'status': engine_status.get('status', '?'),
            'ts': engine_status.get('timestamp', ''),
            'capital': engine_status.get('capital', 0),
            'positions': engine_status.get('positions', engine_status.get('open_positions', 0)),
        },
        'metrics': em,
        'dashboard_ts': now.strftime('%d/%m/%Y %H:%M'),
    }

def get_shadow_json():
    """Shadow tab: parse recent log files for SHADOW entries."""
    try:
        log_files = sorted(glob.glob(str(WORKSPACE / 'logs' / 'layer4_*.log')), reverse=True)[:2]
        shadow_entries = []
        for lf in log_files:
            with open(lf) as f:
                for line in f:
                    if '[SHADOW]' in line:
                        shadow_entries.append(line.strip())
        shadow_entries = shadow_entries[-50:]

        would_trade = [e for e in shadow_entries if 'WOULD TRADE' in e]
        rejected = [e for e in shadow_entries if 'Rejected' in e]

        from collections import Counter
        reject_reasons = Counter()
        for e in rejected:
            reason = e.split('- ')[-1] if '- ' in e else '?'
            for tag in ['no_tokens', 'no_live_price', 'insufficient_balance',
                        'insufficient_depth', 'price_drift', 'profit_below', 'no_mispricing']:
                if tag in reason:
                    reason = tag
                    break
            reject_reasons[reason] = reject_reasons.get(reason, 0) + 1

        trades_list = []
        for e in would_trade[-10:]:
            parts = e.split(' - [L4] INFO - ')
            ts = parts[0][:19] if parts else ''
            msg = parts[1][:120] if len(parts) > 1 else e[:120]
            trades_list.append({'ts': ts, 'msg': msg})

        return {
            'would_trade': len(would_trade), 'rejected': len(rejected),
            'total': len(shadow_entries),
            'reject_reasons': dict(sorted(reject_reasons.items(), key=lambda x: -x[1])),
            'recent_trades': trades_list,
        }
    except Exception as e:
        return {'error': str(e)}

def get_live_json():
    """Live tab data."""
    bal = _get_live_balance()
    mode_label, _ = _get_mode_info()
    state = load_execution_state()
    open_pos = state.get('open_positions', [])
    live_count = sum(1 for p in open_pos if p.get('metadata', {}).get('live', {}))
    return {
        'balance': bal, 'mode': mode_label,
        'live_positions': live_count,
    }

def make_html_shell():
    """Return the static HTML shell. All data populated by SSE + JS."""
    mode_label, mode_class = _get_mode_info()
    html = '''<!DOCTYPE html>
<html><head>
<meta charset="UTF-8">
<title>Prediction Trader Dashboard</title>
<style>
  body { font-family: 'Courier New', monospace; background: #0a0a0a; color: #ccc; padding: 20px; margin: 0; }
  .header { display: flex; justify-content: space-between; align-items: center; border-bottom: 2px solid #0f0; padding-bottom: 12px; margin-bottom: 18px; }
  .header h1 { color: #0f0; margin: 0; }
  .mode-badge { display: inline-block; padding: 3px 12px; border-radius: 4px; font-size: 13px; font-weight: bold; margin-left: 12px; vertical-align: middle; letter-spacing: 1px; border: 1px solid; }
  .mode-shadow { color: #fa0; background: #332800; border-color: #fa0; }
  .mode-live { color: #f44; background: #3a1111; border-color: #f44; }
  .meta { text-align: right; line-height: 1.8; font-size: 12px; color: #888; }
  .meta span { color: #ccc; }
  h2 { color: #0af; margin: 20px 0 8px 0; font-size: 16px; }
  .stats { display: flex; gap: 12px; margin: 10px 0; flex-wrap: wrap; }
  .stat { background: #1a1a1a; border: 1px solid #333; padding: 8px 14px; border-radius: 8px; }
  .stat .label { color: #888; font-size: 11px; }
  .stat .value { color: #0f0; font-size: 16px; font-weight: bold; white-space: nowrap; }
  .value.bad { color: #f44; }
  .value.good { color: #0f0; }
</style>
'''
    html += '''<style>
  table { border-collapse: collapse; width: 100%; margin: 5px 0; }
  th { background: #1a1a1a; color: #0af; text-align: left; padding: 6px 10px; font-size: 12px; }
  td { border-bottom: 1px solid #222; padding: 5px 10px; font-size: 12px; }
  td[title] { cursor: help; }
  tr:hover { background: #1a1a2a; }
  .pos-row { cursor: pointer; }
  .pos-row:hover { background: #1a2a1a !important; }
  .pos-row td:first-child::before { content: '\\25B6 \\00a0'; color: #555; font-size: 10px; }
  .pos-row.expanded td:first-child::before { content: '\\25BC \\00a0'; color: #0af; font-size: 10px; }
  .detail-row { display: none; }
  .detail-row.show { display: table-row; }
  .detail-row td { padding: 4px 10px 8px 30px; color: #999; font-size: 11px; border-bottom: 1px solid #333; background: #111; }
  .leg { margin: 2px 0; }
  .amt { color: #0f0; }
  .mkt { color: #ccc; }
  .win { color: #ff0; }
  .bad { color: #f44; }
  .good { color: #0f0; }
  .dup-opp td { color: #555 !important; }
  .dup-opp:hover { background: #111 !important; }
  .color-positive { color: #4c4; }
  .color-negative { color: #f55; }
  .color-green { color: #0f0; }
  .color-red { color: #f44; }
  .color-amber { color: #fa0; }
  .color-cyan { color: #0af; }
  .color-muted { color: #555; }
  .color-dim { color: #888; }
</style>
'''
    html += '''<style>
  .sub-section-title { cursor:pointer; font-size:13px; color:#aaa; padding:4px 8px; margin:8px 0 2px 0; border-left:3px solid #333; }
  .sub-section-title:hover { color:#fff; }
  .sub-section-title::before { content:'\\25BC '; color:#555; font-size:10px; }
  .sub-section-title.collapsed::before { content:'\\25B6 '; color:#555; font-size:10px; }
  .section-title { cursor: pointer; user-select: none; }
  .section-title:hover { color: #0f0; }
  .section-title::before { content: '\\25BC \\00a0'; font-size: 10px; }
  .section-title.collapsed::before { content: '\\25B6 \\00a0'; font-size: 10px; }
  .section-content.hidden { display: none; }
  .collapse-all-btn { background: #333; color: #0af; border: 1px solid #0af; padding: 2px 10px; font-size: 11px; cursor: pointer; margin-left: 12px; font-family: 'Courier New', monospace; border-radius: 4px; vertical-align: middle; }
  .collapse-all-btn:hover { background: #0af; color: #000; }
  .tab-bar { display: flex; gap: 0; margin: 12px 0 0 0; border-bottom: 2px solid #333; }
  .tab-btn { padding: 8px 20px; background: #111; color: #888; border: 1px solid #333; border-bottom: none; cursor: pointer; font-family: 'Courier New', monospace; font-size: 13px; font-weight: bold; letter-spacing: 0.5px; border-radius: 6px 6px 0 0; margin-right: 2px; transition: all 0.15s; }
  .tab-btn:hover { color: #ccc; background: #1a1a2a; }
  .tab-btn.active { color: #0af; background: #0a0a0a; border-color: #0af; border-bottom: 2px solid #0a0a0a; margin-bottom: -2px; }
  .tab-content { display: none; padding-top: 5px; }
  .tab-content.active { display: block; }
  .tab-annualized { display: inline-block; font-size: 11px; padding: 2px 8px; border-radius: 3px; margin-left: 8px; vertical-align: middle; }
  .val-tag { font-size: 0.68em; margin-left: 3px; padding: 1px 3px; border-radius: 3px; vertical-align: middle; }
  .vtick { color: #4c4; background: #1a2e1a; border: 1px solid #2a4a2a; }
  .vapi { color: #888; background: #1e1e1e; border: 1px solid #333; }
  .pl-indent { padding-left: 25px; }
  .scenario-header { margin-top: 6px; font-size: 11px; }
  .scenario-line { font-size: 11px; }
  .guaranteed-line { margin-top: 4px; font-weight: bold; }
  .agg-total-row { border-top: 2px solid #0af; font-weight: bold; }
  .sse-dot { display: inline-block; width: 8px; height: 8px; border-radius: 50%; background: #555; vertical-align: middle; margin-right: 4px; transition: background 0.3s; }
  .sse-dot.connected { background: #0f0; }
  .sse-dot.disconnected { background: #f44; }
  .sse-dot.reconnecting { background: #fa0; animation: blink 0.8s step-start infinite; }
  @keyframes blink { 50% { opacity: 0; } }
</style>
'''
    html += f'''</head><body>
<div class="header">
  <h1>&#x1F4C8; PREDICTION TRADER <span id="mode-badge" class="mode-badge {mode_class}">{mode_label}</span></h1>
  <div class="meta">
    Started: <span id="meta-first-trade">...</span><br>
    System Restarted: <span id="meta-start-time">...</span> UTC<br>
    Starting Capital: <span id="meta-init-cap">...</span>
  </div>
</div>

<div id="stats-bar" class="stats">
  <!-- Stats populated by JS -->
</div>

<div class="tab-bar">
  <div class="tab-btn active" data-tab="positions" onclick="switchTab('positions')">Positions <span id="tab-ann-paper" class="tab-annualized"></span></div>
  <div class="tab-btn" data-tab="shadow" onclick="switchTab('shadow')">Shadow</div>
  <div class="tab-btn" data-tab="live" onclick="switchTab('live')">Live</div>
</div>

<div id="tab-positions" class="tab-content active">
  <h2 class="section-title" onclick="toggleSection(this)">OPEN POSITIONS (<span id="pos-count">0</span>)
    <button class="collapse-all-btn" onclick="event.stopPropagation(); collapseAll()">Collapse All</button>
  </h2>
  <div class="section-content">
    <table><thead>
      <tr><th>#</th><th>Market</th><th>Strategy</th><th>Score</th><th>Deployed</th><th>Expected P&amp;L</th><th>Resolves</th><th>Status</th><th>Entered</th></tr>
    </thead><tbody id="positions-body"></tbody></table>
  </div>
'''
    html += '''
  <h2 class="section-title collapsed" onclick="toggleSection(this)">AGGREGATE HOLDINGS (<span id="agg-count">0</span> markets)</h2>
  <div class="section-content hidden">
    <table><thead>
      <tr><th>Market</th><th>Side</th><th>Deployed</th><th>Avg Price</th><th>Payout (if wins)</th><th>Pos#</th></tr>
    </thead><tbody id="agg-body"></tbody></table>
  </div>

  <h2 class="section-title collapsed" onclick="toggleSection(this)">OPPORTUNITIES (<span id="opp-total">0</span> found, top 20 by score)</h2>
  <div class="section-content hidden">
    <table><thead>
      <tr><th>#</th><th>Profit%</th><th>Resolves</th><th>Strategy</th><th>Score</th><th>Market</th></tr>
    </thead><tbody id="opp-body"></tbody></table>
  </div>

  <h2 class="section-title collapsed" onclick="toggleSection(this)">SYSTEM</h2>
  <div class="section-content hidden">
    <table><thead>
      <tr><th>Process</th><th>Status</th><th>Info</th></tr>
    </thead><tbody id="system-body"></tbody></table>
  </div>

  <h2 class="section-title collapsed" onclick="toggleSection(this)">CLOSED POSITIONS (<span id="closed-count">0</span>)</h2>
  <div class="section-content hidden" id="closed-container"></div>
</div><!-- end tab-positions -->

<div id="tab-shadow" class="tab-content"></div>
<div id="tab-live" class="tab-content"></div>

<div class="footer" style="color:#555;font-size:11px;margin-top:20px;">
  Updated: <span id="last-update">...</span> UTC | <span class="sse-dot" id="sse-dot"></span> Live via SSE
</div>
'''
    html += '''<script>
// === Utility functions ===
function esc(s) { var d=document.createElement('div'); d.textContent=s; return d.innerHTML; }
function toggleSection(el) { el.classList.toggle('collapsed'); var c=el.nextElementSibling; if(c) c.classList.toggle('hidden'); }
function switchTab(name) {
  document.querySelectorAll('.tab-btn').forEach(b => b.classList.remove('active'));
  document.querySelectorAll('.tab-content').forEach(c => c.classList.remove('active'));
  document.getElementById('tab-'+name).classList.add('active');
  document.querySelectorAll('.tab-btn').forEach(b => { if(b.getAttribute('data-tab')===name) b.classList.add('active'); });
  window.location.hash = name;
}
function collapseAll() {
  document.querySelectorAll('#positions-body .detail-row.show').forEach(r => r.classList.remove('show'));
  document.querySelectorAll('#positions-body .pos-row.expanded').forEach(r => r.classList.remove('expanded'));
}
function toggleRow(prefix, idx) {
  var d = document.getElementById(prefix+'-detail-'+idx);
  var m = document.getElementById(prefix+'-row-'+idx);
  if(d) d.classList.toggle('show');
  if(m) m.classList.toggle('expanded');
}
(function(){ var h=window.location.hash.replace('#',''); if(h && document.getElementById('tab-'+h)) switchTab(h); })();

// === Stats bar renderer ===
function renderStats(d) {
  var bar = document.getElementById('stats-bar');
  if (!bar) return;
  function st(label, id, val, cls) {
    return '<div class="stat"><div class="label">'+label+'</div><div id="'+id+'" class="value '+(cls||'')+'">'+val+'</div></div>';
  }
  var h = '';
  h += st('TOTAL VALUE', 'stat-total', '$'+d.total_value.toFixed(2), d.total_value >= d.init_cap ? 'good':'bad');
  h += st('CASH', 'stat-cash', '$'+d.cash.toFixed(2));
  h += st('DEPLOYED', 'stat-deployed', '$'+d.deployed.toFixed(2));
  h += st('FEES PAID', 'stat-fees', '$'+d.fees.toFixed(2));
  if (d.live_balance !== null && d.live_balance !== undefined)
    h += st('USDC (LIVE)', 'stat-usdc', '$'+d.live_balance.toFixed(2), 'color-amber');
  h += st('RETURN', 'stat-return', (d.ret_pct>=0?'+':'')+d.ret_pct.toFixed(1)+'%', d.ret_pct>=0?'good':'bad');
  h += st('TRADES', 'stat-trades', d.trades);
  h += st('OPEN', 'stat-open', d.open_count);
  h += st('REALIZED P&L', 'stat-realized', '$'+d.realized.toFixed(2), d.realized>=0?'good':'bad');
  h += st('ANNUALIZED', 'stat-annualized', d.annualized, d.annualized_ret>=0?'good':'bad');
  bar.innerHTML = h;
  // Meta
  var el;
  el=document.getElementById('meta-first-trade'); if(el) el.textContent=d.first_trade+' UTC';
  el=document.getElementById('meta-start-time'); if(el) el.textContent=d.start_time;
  el=document.getElementById('meta-init-cap'); if(el) el.textContent='$'+d.init_cap.toFixed(2);
  el=document.getElementById('last-update'); if(el) el.textContent=d.timestamp;
  // Mode badge
  el=document.getElementById('mode-badge'); if(el) { el.textContent=d.mode_label; el.className='mode-badge '+d.mode_class; }
  // Tab annualized
  el=document.getElementById('tab-ann-paper');
  if(el && d.annualized && d.annualized!=='N/A') { el.textContent=d.annualized+' ann.'; el.className='tab-annualized '+(d.annualized_ret>=0?'color-green':'color-red'); }
}

// === Positions renderer ===
function renderPositions(d) {
  document.getElementById('pos-count').textContent = d.pos_count;
  var tb = document.getElementById('positions-body');
  if (!tb) return;
  var h = '';
  d.positions.forEach(function(p) {
    var resolveHtml = esc(p.resolve);
    if (p.postpone) resolveHtml += ' <span class="bad" title="'+esc(p.postpone.reason)+'">&#9888; POSTPONED</span>';
    h += '<tr id="pos-row-'+p.idx+'" class="pos-row" onclick="toggleRow(\\\'pos\\\','+p.idx+')">';
    h += '<td>'+p.idx+'</td>';
    h += '<td title="'+esc(p.full_names.join(' | '))+'">'+esc(p.short_name)+'</td>';
    h += '<td>'+esc(p.strategy)+'</td>';
    h += '<td>'+p.score.toFixed(2)+'</td>';
    h += '<td>$'+p.total_cap.toFixed(2)+'</td>';
    h += '<td>$'+p.exp_profit.toFixed(2)+' ('+p.exp_pct.toFixed(1)+'%)</td>';
    h += '<td>'+resolveHtml+'</td>';
    h += '<td>'+esc(p.status)+'</td>';
    h += '<td>'+esc(p.entry_ts)+'</td></tr>';
    // Detail row with legs
    h += '<tr id="pos-detail-'+p.idx+'" class="detail-row"><td colspan="9">';
    p.legs.forEach(function(l) {
      h += '<div class="leg"><span class="amt">$'+l.bet.toFixed(2)+'</span> buying '+l.side+' ';
      h += '<span class="mkt">'+esc(l.name)+'</span> ';
      h += '('+l.side+' @ '+l.price.toFixed(3)+', '+l.shares+' shares) ';
      h += '&rarr; payout <span class="win">$'+l.payout.toFixed(2)+'</span></div>';
    });
    // Scenarios (sell arbs)
    if (p.scenarios && p.scenarios.length > 0) {
      h += '<div class="leg scenario-header color-dim">Scenarios (one wins YES, that NO leg loses):</div>';
      p.scenarios.forEach(function(s) {
        var css = s.profit >= 0 ? 'color-positive' : 'color-negative';
        var parts = s.parts ? s.parts.map(function(v){return '<span class="color-positive">$'+v.toFixed(2)+'</span>';}).join(' + ') : '';
        h += '<div class="leg scenario-line">If <b>'+esc(s.winner)+'</b> wins: '+parts;
        h += ' = <span class="'+css+'">$'+s.payout.toFixed(2)+'</span>';
        h += ' (<span class="'+css+'">'+((s.profit>=0?'+':'')+s.profit.toFixed(2))+'</span>)</div>';
      });
    }
    // Guaranteed payout
    var gp = p.guaranteed;
    var gprofit = gp - p.total_cap;
    var gcss = gprofit >= 0 ? 'color-positive' : 'color-negative';
    h += '<div class="leg guaranteed-line color-cyan">Guaranteed payout: <span class="win">$'+gp.toFixed(2)+'</span>';
    h += ' (profit <span class="'+gcss+'">$'+(gprofit>=0?'+':'')+gprofit.toFixed(2)+'</span>)</div>';
    h += '</td></tr>';
  });
  if (d.positions.length === 0) h = '<tr><td colspan="9" class="color-muted">No open positions</td></tr>';
  tb.innerHTML = h;

  // Aggregates
  document.getElementById('agg-count').textContent = d.agg_market_count;
  var ab = document.getElementById('agg-body');
  if (ab) {
    var ah = '';
    d.aggregates.forEach(function(a) {
      ah += '<tr><td>'+esc(a.name)+'</td><td>'+a.side+'</td>';
      ah += '<td>$'+a.total_bet.toFixed(2)+'</td><td>'+a.avg_price.toFixed(3)+'</td>';
      ah += '<td>$'+a.payout.toFixed(2)+'</td><td>'+a.pos_idx+'</td></tr>';
    });
    ah += '<tr class="agg-total-row"><td>TOTAL ('+d.agg_market_count+' markets)</td><td></td>';
    ah += '<td>$'+d.agg_total.toFixed(2)+'</td><td></td><td></td><td>'+d.pos_count+' pos</td></tr>';
    ab.innerHTML = ah;
  }
}

// === Opportunities renderer ===
function renderOpportunities(d) {
  document.getElementById('opp-total').textContent = d.total_found;
  var ob = document.getElementById('opp-body');
  if (!ob) return;
  var h = '';
  d.opportunities.forEach(function(o) {
    var css = o.is_held ? 'dup-opp' : (o.is_past ? 'bad' : '');
    var held = o.is_held ? ' [HELD]' : '';
    var valTag = o.validated ? '<span class="val-tag vtick">&#10003;</span>' : '<span class="val-tag vapi">[API]</span>';
    var resolveHtml = (o.resolve === 'PAST' ? '<span class="bad">PAST</span>' : esc(o.resolve)) + valTag;
    h += '<tr id="opp-row-'+o.idx+'" class="pos-row '+css+'" onclick="toggleRow(\\\'opp\\\','+o.idx+')">';
    h += '<td>'+o.idx+'</td><td>'+o.profit_pct.toFixed(1)+'%</td>';
    h += '<td>'+resolveHtml+'</td><td>'+esc(o.strategy)+'</td>';
    h += '<td>'+o.score.toFixed(2)+'</td>';
    h += '<td title="'+esc(o.full_names.join(' | '))+'">'+esc(o.short_name)+held+'</td></tr>';
    // Detail row
    h += '<tr id="opp-detail-'+o.idx+'" class="detail-row"><td colspan="6">';
    o.legs.forEach(function(l) {
      h += '<div class="leg"><span class="amt">$'+l.bet.toFixed(2)+'</span> buying '+l.side+' ';
      h += '<span class="mkt">'+esc(l.name)+'</span> ('+l.side+' @ '+l.price.toFixed(3)+', '+l.shares+' shares) ';
      h += '&rarr; payout <span class="win">$'+l.payout.toFixed(2)+'</span></div>';
    });
    if (o.scenarios && o.scenarios.length > 0) {
      h += '<div class="leg scenario-header color-dim">Scenarios (one wins YES, that NO leg loses):</div>';
      o.scenarios.forEach(function(s) {
        var css2 = s.profit >= 0 ? 'color-positive' : 'color-negative';
        h += '<div class="leg scenario-line">If <b>'+esc(s.winner)+'</b> wins: ';
        h += '<span class="'+css2+'">$'+s.payout.toFixed(2)+'</span>';
        h += ' (<span class="'+css2+'">'+(s.profit>=0?'+':'')+s.profit.toFixed(2)+'</span>)</div>';
      });
    }
    h += '<div class="leg guaranteed-line color-cyan">Guaranteed payout: $'+o.guaranteed_payout.toFixed(2);
    h += ' (profit $'+o.net_profit.toFixed(2)+', fees $'+o.fees.toFixed(4)+')</div>';
    h += '</td></tr>';
  });
  if (d.opportunities.length === 0) h = '<tr><td colspan="6" class="color-muted">No opportunities</td></tr>';
  ob.innerHTML = h;
}

// === System renderer ===
function renderSystem(d) {
  var sb = document.getElementById('system-body');
  if (!sb) return;
  var h = '';
  var scCls = (d.scanner.status==='running'||d.scanner.status==='scanning') ? 'color-green' : 'color-amber';
  h += '<tr><td>Market Scanner</td><td class="'+scCls+'">'+esc(d.scanner.status)+'</td><td>'+esc(d.scanner.ts||'')+'</td></tr>';
  var eCls = d.engine.status==='running' ? 'color-green' : 'color-amber';
  h += '<tr><td>Trading Engine</td><td class="'+eCls+'">'+esc(d.engine.status)+'</td>';
  h += '<td>'+esc(d.engine.ts||'')+' | cash=$'+(d.engine.capital||0).toFixed(2)+' | pos='+(d.engine.positions||0)+'</td></tr>';
  var m = d.metrics || {};
  if (m) {
    var rustTag = m.has_rust ? '<span class="color-green">Rust</span>' : '<span class="color-amber">Python</span>';
    h += '<tr><td class="pl-indent color-dim">Arb Engine</td><td>'+rustTag+'</td>';
    h += '<td>constraints='+(m.constraints||0)+' | markets='+(m.markets_total||0)+' | iter='+(m.iteration||0)+'</td></tr>';
    var wl=m.ws_live||0, wt=m.markets_total||1, wp=wt?Math.round(wl/wt*100):0;
    var wCls = wp>20?'color-green':'color-amber';
    h += '<tr><td class="pl-indent color-dim">WebSocket</td><td class="'+wCls+'">subs='+(m.ws_subscribed||0)+'</td>';
    h += '<td>msgs='+(m.ws_msgs||0).toLocaleString()+' | live='+wl+'/'+wt+' ('+wp+'%)</td></tr>';
    var qb=m.queue_background||0, qu=m.queue_urgent||0;
    var qCls = qb<500?'color-green':(qb<2000?'color-amber':'color-red');
    h += '<tr><td class="pl-indent color-dim">Eval Queue</td><td class="'+qCls+'">bg='+qb+'</td>';
    h += '<td>urgent='+qu+' | bg='+qb+'</td></tr>';
    var lp=m.lat_p50_ms||0, l9=m.lat_p95_ms||0, lm=m.lat_max_ms||0;
    var lCls = lp<100?'color-green':(lp<1000?'color-amber':'color-red');
    h += '<tr><td class="pl-indent color-dim">Latency</td><td class="'+lCls+'">p50='+lp+'ms</td>';
    h += '<td>p50='+lp+'ms | p95='+l9+'ms | max='+lm+'ms</td></tr>';
  }
  h += '<tr><td>Dashboard</td><td class="color-positive">running</td><td>'+esc(d.dashboard_ts)+'</td></tr>';
  sb.innerHTML = h;
}
// === Closed positions renderer ===
function renderClosed(d) {
  document.getElementById('closed-count').textContent = d.total_closed;
  var cc = document.getElementById('closed-container');
  if (!cc) return;
  var h = '';
  var order = ['resolved','replaced_profit','replaced_loss','replaced_even'];
  order.forEach(function(key) {
    var cat = d.categories[key];
    if (!cat || cat.rows.length === 0) return;
    var cls = cat.collapsed ? 'collapsed' : '';
    var hide = cat.collapsed ? 'hidden' : '';
    h += '<h3 class="sub-section-title '+cls+'" onclick="toggleSection(this)">'+esc(cat.label)+' ('+cat.rows.length+')</h3>';
    h += '<div class="section-content '+hide+'"><table>';
    h += '<tr><th>Market</th><th>Deployed</th><th>P&amp;L</th><th>Held</th></tr>';
    cat.rows.forEach(function(r) {
      var cls2 = r.pnl > 0.01 ? 'good' : (r.pnl < -0.01 ? 'bad' : '');
      h += '<tr class="'+cls2+'"><td>'+esc(r.name)+'</td>';
      h += '<td>$'+r.deployed.toFixed(2)+'</td>';
      h += '<td>$'+(r.pnl>=0?'+':'')+r.pnl.toFixed(2)+'</td>';
      h += '<td>'+esc(r.hold)+'</td></tr>';
    });
    h += '</table></div>';
  });
  cc.innerHTML = h;
}

// === Shadow tab renderer ===
function renderShadow(d) {
  var el = document.getElementById('tab-shadow');
  if (!el) return;
  if (d.error) { el.innerHTML = '<p class="color-red">Error: '+esc(d.error)+'</p>'; return; }
  var h = '<div class="stats">';
  h += '<div class="stat"><div class="label">WOULD TRADE</div><div class="value good">'+(d.would_trade||0)+'</div></div>';
  h += '<div class="stat"><div class="label">REJECTED</div><div class="value">'+(d.rejected||0)+'</div></div>';
  h += '<div class="stat"><div class="label">TOTAL CHECKED</div><div class="value">'+(d.total||0)+'</div></div>';
  h += '</div>';
  // Rejection reasons
  if (d.reject_reasons && Object.keys(d.reject_reasons).length > 0) {
    h += '<h3 style="color:#fa0;margin:15px 0 5px">Rejection Reasons</h3><table><tr><th>Reason</th><th>Count</th></tr>';
    Object.entries(d.reject_reasons).forEach(function(e) { h += '<tr><td>'+esc(e[0])+'</td><td>'+e[1]+'</td></tr>'; });
    h += '</table>';
  }
  // Recent trades
  h += '<h3 style="color:#0f0;margin:15px 0 5px">Recent Would-Trade Signals</h3>';
  if (d.recent_trades && d.recent_trades.length > 0) {
    h += '<table><tr><th>Time</th><th>Opportunity</th></tr>';
    d.recent_trades.forEach(function(t) { h += '<tr><td>'+esc(t.ts)+'</td><td class="color-green">'+esc(t.msg)+'</td></tr>'; });
    h += '</table>';
  } else {
    h += '<p class="color-muted">No would-trade signals yet.</p>';
  }
  el.innerHTML = h;
}

// === Live tab renderer ===
function renderLive(d) {
  var el = document.getElementById('tab-live');
  if (!el) return;
  var h = '<div class="stats">';
  if (d.balance !== null && d.balance !== undefined) {
    h += '<div class="stat"><div class="label">USDC BALANCE</div><div class="value color-amber">$'+d.balance.toFixed(2)+'</div></div>';
    var st = d.mode==='LIVE' ? 'ACTIVE' : 'SHADOW ONLY';
    var cls = d.mode==='LIVE' ? 'color-green' : 'color-amber';
    h += '<div class="stat"><div class="label">STATUS</div><div class="value '+cls+'">'+st+'</div></div>';
  } else {
    h += '<div class="stat"><div class="label">STATUS</div><div class="value color-muted">NOT CONNECTED</div></div>';
  }
  h += '</div>';
  h += '<h3 style="color:#0af;margin:15px 0 5px">Live Positions</h3>';
  if (d.live_positions > 0) {
    h += '<p>'+d.live_positions+' positions with live CLOB orders</p>';
  } else {
    h += '<p class="color-muted">No live positions. Set shadow_only: false in config and fund your account to start live trading.</p>';
  }
  el.innerHTML = h;
}

// === SSE connection manager ===
(function() {
  var retryDelay = 2000;
  var es;
  function dot() { return document.getElementById('sse-dot'); }

  function connect() {
    if (es) { try { es.close(); } catch(e) {} }
    es = new EventSource('/stream');
    es.onopen = function() { retryDelay = 2000; var d=dot(); if(d) d.className='sse-dot connected'; };

    es.addEventListener('stats', function(e) {
      try { renderStats(JSON.parse(e.data)); } catch(ex) { console.warn('stats parse error', ex); }
    });
    es.addEventListener('positions', function(e) {
      try { renderPositions(JSON.parse(e.data)); } catch(ex) { console.warn('positions parse error', ex); }
    });
    es.addEventListener('opportunities', function(e) {
      try { renderOpportunities(JSON.parse(e.data)); } catch(ex) { console.warn('opp parse error', ex); }
    });
    es.addEventListener('system', function(e) {
      try { renderSystem(JSON.parse(e.data)); } catch(ex) { console.warn('system parse error', ex); }
    });
    es.addEventListener('closed', function(e) {
      try { renderClosed(JSON.parse(e.data)); } catch(ex) { console.warn('closed parse error', ex); }
    });
    es.addEventListener('shadow', function(e) {
      try { renderShadow(JSON.parse(e.data)); } catch(ex) { console.warn('shadow parse error', ex); }
    });
    es.addEventListener('live', function(e) {
      try { renderLive(JSON.parse(e.data)); } catch(ex) { console.warn('live parse error', ex); }
    });

    es.onerror = function() {
      var d2=dot(); if(d2) d2.className='sse-dot disconnected';
      es.close();
      setTimeout(function() {
        var d3=dot(); if(d3) d3.className='sse-dot reconnecting';
        connect();
        retryDelay = Math.min(retryDelay * 2, 30000);
      }, retryDelay);
    };
    // Heartbeat timeout — if no event for 12s, mark disconnected
    var heartbeat;
    es.addEventListener('stats', function() {
      var d=dot(); if(d) d.className='sse-dot connected';
      clearTimeout(heartbeat);
      heartbeat = setTimeout(function(){ var d2=dot(); if(d2) d2.className='sse-dot disconnected'; }, 12000);
    });
  }
  connect();
})();
</script>
</body></html>'''
    return html

# ===== HTTP Server =====

class ThreadingDashboardServer(ThreadingMixIn, HTTPServer):
    daemon_threads = True

class DashboardHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        if self.path == '/stream':
            self._handle_sse()
        elif self.path == '/status':
            body = json.dumps(load_execution_state(), indent=2).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        else:
            self._handle_html()

    def _handle_html(self):
        try:
            page = make_html_shell()
        except Exception:
            import traceback
            page = f'<html><body style="background:#0a0a0a;color:#f44;font-family:monospace;padding:20px"><h1>Dashboard Error</h1><pre>{traceback.format_exc()}</pre></body></html>'
        body = page.encode()
        self.send_response(200)
        self.send_header('Content-Type', 'text/html')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _handle_sse(self):
        """SSE endpoint: sends typed events at different intervals."""
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Cache-Control', 'no-cache')
        self.send_header('Connection', 'keep-alive')
        self.send_header('Access-Control-Allow-Origin', '*')
        self.end_headers()

        def send_event(event_type, data_dict):
            payload = json.dumps(data_dict)
            self.wfile.write(f'event: {event_type}\ndata: {payload}\n\n'.encode())
            self.wfile.flush()

        tick = 0  # 1-second ticks
        try:
            # Send everything immediately on connect
            send_event('stats', get_stats_json())
            send_event('positions', get_positions_json())
            send_event('opportunities', get_opportunities_json())
            send_event('system', get_system_json())
            send_event('closed', get_closed_json())
            send_event('shadow', get_shadow_json())
            send_event('live', get_live_json())

            while True:
                time.sleep(5)
                tick += 5

                # Stats + positions: every 5s
                try:
                    send_event('stats', get_stats_json())
                    send_event('positions', get_positions_json())
                except Exception:
                    pass

                # System: every 10s
                if tick % 10 == 0:
                    try:
                        send_event('system', get_system_json())
                    except Exception:
                        pass

                # Opportunities: every 15s
                if tick % 15 == 0:
                    try:
                        send_event('opportunities', get_opportunities_json())
                    except Exception:
                        pass

                # Shadow + Live: every 30s
                if tick % 30 == 0:
                    try:
                        send_event('shadow', get_shadow_json())
                        send_event('live', get_live_json())
                    except Exception:
                        pass

                # Closed: every 60s
                if tick % 60 == 0:
                    try:
                        send_event('closed', get_closed_json())
                    except Exception:
                        pass

        except (BrokenPipeError, ConnectionResetError, OSError):
            pass  # Client disconnected

    def log_message(self, format, *args):
        pass  # suppress access logs


if __name__ == '__main__':
    port = 5556
    print(f'Dashboard starting on http://localhost:{port} (full SSE, no refresh needed)')
    ThreadingDashboardServer(('0.0.0.0', port), DashboardHandler).serve_forever()
