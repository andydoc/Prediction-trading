"""Standalone Dashboard - replaces the basic one in main.py
Run: python dashboard_server.py
Serves on http://localhost:5556
"""
import json, os, sys, html as html_mod, time, threading
from pathlib import Path
from datetime import datetime, timezone
from http.server import HTTPServer, BaseHTTPRequestHandler
from socketserver import ThreadingMixIn

WORKSPACE = Path('/home/andydoc/prediction-trader')
CONFIG_PATH = WORKSPACE / 'config' / 'config.yaml'

def load_config():
    try:
        import yaml
        return yaml.safe_load(CONFIG_PATH.read_text())
    except:
        return {}
DATA = WORKSPACE / 'data'
OPP_PATH = WORKSPACE / 'layer3_arbitrage_math' / 'data' / 'latest_opportunities.json'
EXEC_STATE = DATA / 'system_state' / 'execution_state.json'
EXEC_STATE_DB = DATA / 'system_state' / 'execution_state.db'
ENGINE_STATUS = DATA / 'trading_engine_status.json'

# Track when dashboard/system started
START_TIME = datetime.now(timezone.utc)

def load_json(p):
    try:
        return json.loads(Path(p).read_text())
    except:
        return {}

def load_execution_state():
    """Load execution state: SQLite primary, JSON fallback."""
    if EXEC_STATE_DB.exists():
        try:
            from state_db import read_state_from_disk
            result = read_state_from_disk(str(EXEC_STATE_DB))
            if result:
                return result
        except Exception:
            pass
    return load_json(EXEC_STATE)

def format_datetime(iso_str):
    """Format ISO datetime string to dd/mm/yyyy hh:mm"""
    if not iso_str:
        return '?'
    try:
        dt = datetime.fromisoformat(str(iso_str))
        return dt.strftime('%d/%m/%Y %H:%M')
    except:
        return str(iso_str)[:16]

def escape_html(text):
    """HTML-escape text for safe attribute use"""
    return html_mod.escape(str(text), quote=True)

def make_html():
    now = datetime.now(timezone.utc)

    # Process statuses (new 2-process architecture)
    layer1_status = load_json(DATA / 'layer1_status.json')
    engine_status = load_json(ENGINE_STATUS)

    # Execution state (SQLite primary, JSON fallback)
    state = load_execution_state()

    # Load trading mode
    cfg = load_config()
    config = cfg  # alias for template
    live_cfg = cfg.get('live_trading', {})
    shadow_only = live_cfg.get('shadow_only', True)  # Default shadow (paper retired)
    if shadow_only:
        mode_label = 'SHADOW'
        mode_class = 'mode-shadow'
    else:
        mode_label = 'LIVE'
        mode_class = 'mode-live'

    # Fetch live USDC balance
    live_balance = None
    if True:  # Always attempt — we're always in shadow or live
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
            live_balance = float(_bal.get('balance', 0)) / 1e6
        except Exception:
            live_balance = None
    cap = state.get('current_capital', 0)
    init_cap = state.get('initial_capital', 100)
    perf = state.get('performance', {})
    open_pos = state.get('open_positions', [])
    closed_pos = state.get('closed_positions', [])

    deployed = sum(sum(m.get('bet_amount', 0) for m in p.get('markets', {}).values()) for p in open_pos)
    cap_per_trade = deployed / len(open_pos) if open_pos else 0
    total_fees = sum(p.get('fees_paid', 0) for p in open_pos) + sum(p.get('fees_paid', 0) for p in closed_pos)
    repl_count = sum(1 for p in closed_pos if p.get('metadata', {}).get('close_reason') == 'replaced')
    repl_cost = sum(p.get('fees_paid', 0) for p in closed_pos if p.get('metadata', {}).get('close_reason') == 'replaced')
    repl_cost += sum(abs(p.get('actual_profit', 0)) for p in closed_pos if p.get('metadata', {}).get('close_reason') == 'replaced' and p.get('actual_profit', 0) < 0)
    total_value = cap + deployed
    ret_pct = ((total_value - init_cap) / init_cap * 100) if init_cap else 0
    trades = perf.get('total_trades', 0)
    wins = perf.get('wins', 0)
    winrate = (wins / trades * 100) if trades > 0 else 0

    # Opportunities
    opp_data = load_json(OPP_PATH)
    opps = opp_data.get('opportunities', [])

    # Markets for end_date lookup + full names
    market_data = load_json(DATA / 'latest_markets.json')
    markets = {str(m['market_id']): m for m in market_data.get('markets', [])}

    # Load resolution cache for date validation badges
    import hashlib
    _res_cache_dir = DATA / 'resolution_cache'
    _res_cache = {}
    if _res_cache_dir.exists():
        for _cf in _res_cache_dir.glob('*.json'):
            try:
                _cd = json.loads(_cf.read_text())
                if 'latest_resolution_date' in _cd:
                    _res_cache[_cf.stem] = _cd
            except:
                pass

    # Build opportunity rows with resolution info
    opp_rows = []
    for opp in opps:
        profit_pct = opp.get('expected_profit_pct', 0) * 100
        latest = None
        all_past = True
        cid = opp.get('constraint_id', '')
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
        if latest:
            delta_h = (latest - now).total_seconds() / 3600
            if delta_h < 0 or all_past:
                res_str = '<span class="bad">PAST</span>'
                score = -1
            else:
                days = delta_h / 24
                # check AI-validated cache
                _first_mid = str(opp.get('market_ids', [''])[0]) if opp.get('market_ids') else ''
                _gid = cid or _first_mid
                _ckey = hashlib.md5(str(_gid).encode()).hexdigest()[:12]
                _cv = _res_cache.get(_ckey)
                if _cv and 'latest_resolution_date' in _cv:
                    try:
                        from datetime import timezone as _tz
                        _vd = datetime.strptime(_cv['latest_resolution_date'], '%Y-%m-%d')
                        _vd = _vd.replace(hour=23, minute=59, second=59, tzinfo=_tz.utc)
                        delta_h = (_vd - now).total_seconds() / 3600
                    except:
                        pass
                _conf = _cv.get('confidence','') if _cv else ''
                _vtag = (f'<span class="val-tag vtick" title="AI-validated ({_conf})">&#10003;</span>'
                         if _gid and _ckey in _res_cache
                         else '<span class="val-tag vapi" title="Raw API endDate">[API]</span>')
                res_str = (f'{days:.1f}d' if days > 1 else f'{delta_h:.1f}h') + _vtag
                score = (opp.get('expected_profit_pct', 0) / max(delta_h, 0.01))
        else:
            _gid2 = opp.get('constraint_id','') or (str(opp.get('market_ids',[''])[0]) if opp.get('market_ids') else '')
            _ckey2 = hashlib.md5(str(_gid2).encode()).hexdigest()[:12]
            _vtag2 = ('<span class="val-tag vtick">&#10003;</span>' if _ckey2 in _res_cache
                      else '<span class="val-tag vapi">[API]</span>')
            res_str = '?' + _vtag2
            score = 0

        names = opp.get('market_names', [])
        short_name = names[0][:50] if names else '?'
        full_names = ' | '.join(names) if names else '?'
        n_markets = len(opp.get('market_ids', []))
        strategy = opp.get('metadata', {}).get('method', '?').replace('_', ' ').title()

        opp_rows.append((score, profit_pct, res_str, strategy, n_markets, short_name, full_names, opp.get('constraint_id', ''), opp))

    opp_rows.sort(key=lambda x: x[0], reverse=True)

    # Build position rows, sorted by ascending resolution date
    pos_row_list = []
    for p in open_pos:
        total_cap = p.get('total_capital', 0)
        exp_profit = p.get('expected_profit', 0)
        exp_pct = p.get('expected_profit_pct', 0) * 100
        n_mkts = len(p.get('markets', {}))
        entry_ts = format_datetime(p.get('entry_timestamp', ''))
        status = p.get('status', '?')
        mkt_vals = list(p.get('markets', {}).values())
        short_name = mkt_vals[0].get('name', '?')[:40] if mkt_vals else '?'
        full_names = ' | '.join(v.get('name', '?') for v in mkt_vals)
        # Get latest (max) end_date across position markets for resolution date
        # Position markets dict uses market_id as KEY
        pos_resolve_dt = None
        for mid_str in p.get('markets', {}).keys():
            mkt_data = markets.get(str(mid_str))
            if mkt_data:
                try:
                    ed = datetime.fromisoformat(mkt_data['end_date'])
                    if ed.tzinfo is None:
                        ed = ed.replace(tzinfo=timezone.utc)
                    if pos_resolve_dt is None or ed > pos_resolve_dt:
                        pos_resolve_dt = ed
                except:
                    pass
        pos_resolve = pos_resolve_dt.strftime('%d/%m/%Y') if pos_resolve_dt else '?'
        sort_key = pos_resolve_dt if pos_resolve_dt else datetime(9999, 1, 1, tzinfo=timezone.utc)
        # Compute score: profit_pct / hours_to_resolve (matching opp scoring)
        if pos_resolve_dt:
            pos_hours = max((pos_resolve_dt - now).total_seconds() / 3600, 0.01)
            pos_score = (p.get('expected_profit_pct', 0) / pos_hours) * 10000
        else:
            pos_score = 0.0
        # Extract strategy from metadata (preferred) or opportunity_id (fallback)
        opp_id = p.get('opportunity_id', '')
        pos_meta = p.get('metadata', {})
        method = pos_meta.get('method', '')
        if method:
            pos_strategy = method.replace('_', ' ').title()  # e.g. "Mutex Sell All"
        elif 'buy_mutex' in opp_id:
            pos_strategy = 'Mutex Buy All'
        elif 'sell_mutex' in opp_id:
            pos_strategy = 'Mutex Sell All'
        elif 'polytope' in opp_id:
            pos_strategy = 'Polytope Arb'
        else:
            pos_strategy = opp_id.split('_')[1].title() if '_' in opp_id else '?'
        # Build market legs detail
        pos_idx = len(pos_row_list) + 1
        legs_html = ''
        is_sell = 'sell' in opp_id
        for mid, mdata in p.get('markets', {}).items():
            mname = mdata.get('name', '?')
            bet = mdata.get('bet_amount', 0)
            ep = mdata.get('entry_price', 0)
            if is_sell:
                no_price = 1.0 - ep
                shares = bet / no_price if no_price > 0 else 0
                legs_html += (f'<div class="leg">'
                             f'<span class="amt">${bet:.2f}</span> buying NO '
                             f'<span class="mkt">{escape_html(mname)}</span> '
                             f'(NO @ {no_price:.3f}, {shares:.1f} shares) '
                             f'&rarr; payout <span class="win">${shares:.2f}</span>'
                             f'</div>')
            else:
                shares = bet / ep if ep > 0 else 0
                legs_html += (f'<div class="leg">'
                             f'<span class="amt">${bet:.2f}</span> buying '
                             f'<span class="mkt">{escape_html(mname)}</span> '
                             f'(YES @ {ep:.3f}, {shares:.1f} shares) '
                             f'&rarr; payout <span class="win">${shares:.2f}</span>'
                             f'</div>')
        # Scenario / guaranteed payout summary
        if is_sell:
            # Sell-all: bought NO on every market. One resolves YES (loses), rest NO (win)
            market_list = list(p.get('markets', {}).items())
            shares_map = {}
            for mid2, md2 in market_list:
                np2 = 1.0 - md2.get('entry_price', 0)
                shares_map[mid2] = md2.get('bet_amount', 0) / np2 if np2 > 0 else 0
            legs_html += '<div class="leg scenario-header color-dim">Scenarios (exactly one wins YES, that NO leg loses):</div>'
            scenario_payouts = []
            for wmid, wmd in market_list:
                wname = wmd.get('name', '?')[:40]
                winners = [(mid2, shares_map[mid2]) for mid2 in shares_map if mid2 != wmid]
                payout = sum(s for _, s in winners)
                profit = payout - total_cap
                parts = ' + '.join(f'<span class="color-positive">${s:.2f}</span>' for _, s in winners)
                css = 'color-positive' if profit >= 0 else 'color-negative'
                legs_html += (f'<div class="leg scenario-line">'
                             f'If <b>{escape_html(wname)}</b> wins: {parts}'
                             f' = <span class="{css}">${payout:.2f}</span>'
                             f' (<span class="{css}">{profit:+.2f}</span>)</div>')
                scenario_payouts.append(payout)
            guaranteed = min(scenario_payouts) if scenario_payouts else 0
            gprofit = guaranteed - total_cap
            gcss = 'color-positive' if gprofit >= 0 else 'color-negative'
            legs_html += (f'<div class="leg guaranteed-line color-cyan">'
                         f'Guaranteed payout: ${guaranteed:.2f} '
                         f'(profit <span class="{gcss}">${gprofit:+.2f}</span>)'
                         f'</div>')
        else:
            # Buy-all: original guaranteed payout display
            all_payouts = []
            for mdata in p.get('markets', {}).values():
                b = mdata.get('bet_amount', 0)
                ep2 = mdata.get('entry_price', 0)
                all_payouts.append(b / ep2 if ep2 > 0 else 0)
            if all_payouts:
                guaranteed = min(all_payouts)
                legs_html += (f'<div class="leg guaranteed-line color-cyan">'
                             f'Guaranteed payout: <span class="win">${guaranteed:.2f}</span> '
                             f'(profit <span class="win">${guaranteed - total_cap:.2f}</span>)'
                             f'</div>')

        row_html = (f'<tr id="pos-{pos_idx}" class="pos-row" onclick="togglePos({pos_idx})">'
                   f'<td>{pos_idx}</td><td title="{escape_html(full_names)}">{escape_html(short_name)}</td><td>{pos_strategy}</td><td>{pos_score:.2f}</td>'
                   f'<td>${total_cap:.2f}</td><td>${exp_profit:.2f} ({exp_pct:.1f}%)</td>'
                   f'<td>{pos_resolve}</td><td>{status}</td><td>{entry_ts}</td></tr>\n'
                   f'<tr id="detail-{pos_idx}" class="detail-row"><td colspan="9">{legs_html}</td></tr>\n')
        pos_row_list.append((sort_key, row_html))
    pos_row_list.sort(key=lambda x: x[0])
    pos_rows_html = ''.join(r for _, r in pos_row_list)

    # Build aggregate market holdings
    from collections import defaultdict
    agg_markets = defaultdict(lambda: {'name': '', 'total_bet': 0.0, 'pos_idx': 0, 'outcome': '', 'entry_prices': [], 'is_sell': False})
    for pos_idx, p in enumerate(open_pos, 1):
        opp_id = p.get('opportunity_id', '')
        is_sell = 'sell' in opp_id
        for mid, mdata in p.get('markets', {}).items():
            a = agg_markets[mid]
            a['name'] = mdata.get('name', '?')[:65]
            a['total_bet'] += mdata.get('bet_amount', 0)
            a['pos_idx'] = pos_idx
            a['outcome'] = mdata.get('outcome', 'yes')
            a['entry_prices'].append(mdata.get('entry_price', 0))
            a['is_sell'] = is_sell
    agg_total_bet = sum(a['total_bet'] for a in agg_markets.values())
    agg_rows_html = ''
    for mid, a in sorted(agg_markets.items(), key=lambda x: x[1]['pos_idx']):
        avg_price = sum(a['entry_prices']) / len(a['entry_prices']) if a['entry_prices'] else 0
        # Payout: buy=bet/price, sell=bet/(1-price)
        if a['is_sell']:
            payout = a['total_bet'] / (1.0 - avg_price) if avg_price < 1 else 0
            side = 'NO'
        else:
            payout = a['total_bet'] / avg_price if avg_price > 0 else 0
            side = 'YES'
        dup_flag = ''
        agg_rows_html += (f'<tr><td>{escape_html(a["name"])}</td>'
                         f'<td>{side}</td>'
                         f'<td>${a["total_bet"]:.2f}</td>'
                         f'<td>{avg_price:.3f}</td>'
                         f'<td>${payout:.2f}</td>'
                         f'<td>{a["pos_idx"]}{dup_flag}</td></tr>\n')
    # Summary row
    agg_rows_html += (f'<tr class="agg-total-row">'
                     f'<td>TOTAL ({len(agg_markets)} markets)</td><td></td>'
                     f'<td>${agg_total_bet:.2f}</td><td></td><td></td>'
                     f'<td>{len(open_pos)} pos</td></tr>\n')

    # Build opp rows HTML (top 20) - expandable
    # Collect open position market IDs to flag duplicates
    open_market_ids = set()
    for _p in open_pos:
        for _mid in _p.get('markets', {}).keys():
            open_market_ids.add(str(_mid))
    opp_rows_html = ''
    opp_idx = 0
    for score, pct, res, strategy, _n_mkts, short_name, full_names, cid, opp_dict in opp_rows[:20]:
        opp_idx += 1
        is_dup = any(str(m) in open_market_ids for m in opp_dict.get('market_ids', []))
        css = 'dup-opp' if is_dup else ('bad' if score < 0 else '')
        dup_label = ' [HELD]' if is_dup else ''
        opp_rows_html += (f'<tr class="pos-row {css}" onclick="toggleOpp({opp_idx})">' 
                          f'<td>{opp_idx}</td><td>{pct:.1f}%</td><td>{res}</td>'
                          f'<td>{strategy}</td>'
                          f'<td>{score*10000:.2f}</td><td title="{escape_html(full_names)}">{escape_html(short_name)}{dup_label}</td></tr>\n')
        # Build detail row
        opp_legs = ''
        method = opp_dict.get('metadata', {}).get('method', '')
        is_sell_opp = 'sell' in method
        opp_prices = opp_dict.get('current_prices', {})
        opp_bets = opp_dict.get('optimal_bets', {})
        opp_names = opp_dict.get('market_names', [])
        opp_mids = opp_dict.get('market_ids', [])
        opp_cap = opp_dict.get('total_capital_required', 10)
        for k, omid in enumerate(opp_mids):
            omname = opp_names[k] if k < len(opp_names) else '?'
            oprice = opp_prices.get(str(omid), 0)
            obet = opp_bets.get(str(omid), 0)
            if is_sell_opp:
                no_p = 1.0 - oprice
                oshares = obet / no_p if no_p > 0 else 0
                opp_legs += (f'<div class="leg">'
                            f'<span class="amt">${obet:.2f}</span> buying NO '
                            f'<span class="mkt">{escape_html(omname)}</span> '
                            f'(NO @ {no_p:.3f}, {oshares:.1f} shares) '
                            f'&rarr; payout <span class="win">${oshares:.2f}</span></div>')
            else:
                oshares = obet / oprice if oprice > 0 else 0
                opp_legs += (f'<div class="leg">'
                            f'<span class="amt">${obet:.2f}</span> buying YES '
                            f'<span class="mkt">{escape_html(omname)}</span> '
                            f'(YES @ {oprice:.3f}, {oshares:.1f} shares) '
                            f'&rarr; payout <span class="win">${oshares:.2f}</span></div>')
        # Scenario lines for sell-all
        if is_sell_opp and len(opp_mids) > 1:
            sh_map = {}
            for k2, omid2 in enumerate(opp_mids):
                np2 = 1.0 - opp_prices.get(str(omid2), 0)
                sh_map[omid2] = opp_bets.get(str(omid2), 0) / np2 if np2 > 0 else 0
            opp_legs += '<div class="leg scenario-header color-dim">Scenarios (one wins YES, that NO leg loses):</div>'
            for k3, wmid in enumerate(opp_mids):
                wname = opp_names[k3][:40] if k3 < len(opp_names) else '?'
                winners = [(m, sh_map[m]) for m in opp_mids if m != wmid]
                spayout = sum(s for _, s in winners)
                sprofit = spayout - opp_cap
                sparts = ' + '.join(f'<span class="color-positive">${s:.2f}</span>' for _, s in winners)
                scss = 'color-positive' if sprofit >= 0 else 'color-negative'
                opp_legs += (f'<div class="leg scenario-line">'
                            f'If <b>{escape_html(wname)}</b> wins: {sparts}'
                            f' = <span class="{scss}">{spayout:.2f}</span>'
                            f' (<span class="{scss}">{sprofit:+.2f}</span>)</div>')
        # Guaranteed payout
        opp_net = opp_dict.get('net_profit', opp_dict.get('expected_profit', 0))
        opp_fees = opp_dict.get('fees_estimated', 0)
        opp_legs += (f'<div class="leg guaranteed-line color-cyan">'
                    f'Guaranteed payout: ${opp_cap + opp_net:.2f} '
                    f'(profit ${opp_net:.2f}, fees ${opp_fees:.4f})</div>')
        opp_rows_html += f'<tr id="opp-detail-{opp_idx}" class="detail-row"><td colspan="6">{opp_legs}</td></tr>\n'

    # Closed positions summary with annualized return
    # Categorize closed positions into 4 groups
    cat_resolved = []  # resolved/expired
    cat_repl_profit = []  # replaced with profit
    cat_repl_loss = []  # replaced with loss
    cat_repl_even = []  # replaced at breakeven
    total_realized = 0
    total_closed_capital = 0
    total_hold_seconds = 0
    n_closed = 0
    for p in closed_pos:
        actual = p.get('actual_profit', 0)
        cl_deployed = p.get('total_capital', 0)
        total_realized += actual
        total_closed_capital += cl_deployed
        mkt_vals = list(p.get('markets', {}).values())
        short_name = mkt_vals[0].get('name', '?')[:40] if mkt_vals else '?'
        full_names = ' | '.join(v.get('name', '?') for v in mkt_vals)
        entry_iso = p.get('entry_timestamp', '')
        close_ts = p.get('close_timestamp', 0)
        hold_str = '?'
        hold_secs = 0
        reason = p.get('metadata', {}).get('close_reason', 'resolved')
        if entry_iso and close_ts:
            try:
                entry_dt = datetime.fromisoformat(str(entry_iso))
                if entry_dt.tzinfo is None:
                    entry_dt = entry_dt.replace(tzinfo=timezone.utc)
                hold_secs = close_ts - entry_dt.timestamp()
                total_hold_seconds += hold_secs
                n_closed += 1
                if hold_secs < 3600:
                    hold_str = f'{hold_secs/60:.0f}m'
                elif hold_secs < 86400:
                    hold_str = f'{hold_secs/3600:.1f}h'
                else:
                    hold_str = f'{hold_secs/86400:.1f}d'
            except:
                pass
        pnl_class = 'good' if actual > 0.01 else ('bad' if actual < -0.01 else '')
        row = (f'<tr class="{pnl_class}"><td title="{escape_html(full_names)}">{escape_html(short_name)}</td>'
               f'<td>${cl_deployed:.2f}</td><td>${actual:+.2f}</td>'
               f'<td>{hold_str}</td></tr>\n')
        if reason in ('resolved', 'expired'):
            cat_resolved.append(row)
        elif reason == 'replaced' and actual > 0.01:
            cat_repl_profit.append(row)
        elif reason == 'replaced' and actual < -0.01:
            cat_repl_loss.append(row)
        else:
            cat_repl_even.append(row)

    # Build subsection HTML for each category
    def _render_closed_subsection(title, rows, collapsed=False):
        if not rows:
            return ''
        css_cls = 'collapsed' if collapsed else ''
        hide = 'hidden' if collapsed else ''
        hdr = f'<h3 class="sub-section-title {css_cls}" onclick="toggleSection(this)">{title} ({len(rows)})</h3>'
        tbl = f'<div class="section-content {hide}"><table><tr><th>Market</th><th>Deployed</th><th>P&amp;L</th><th>Held</th></tr>'
        tbl += ''.join(rows)
        tbl += '</table></div>'
        return hdr + tbl

    closed_html = ''
    closed_html += _render_closed_subsection('Resolved', cat_resolved, collapsed=False)
    closed_html += _render_closed_subsection('Replaced with Profit', cat_repl_profit, collapsed=False)
    closed_html += _render_closed_subsection('Replaced with Loss', cat_repl_loss, collapsed=False)
    closed_html += _render_closed_subsection('Replaced at Breakeven', cat_repl_even, collapsed=True)

    # Annualized: total portfolio return / calendar days since first trade
    annualized_ret = 0.0
    annualized_str = 'N/A'
    first_trade_dt = None
    # Primary source: earliest entry_timestamp in execution state
    _op = state.get('open_positions', [])
    _all_pos = state.get('closed_positions', []) + (list(_op.values()) if isinstance(_op, dict) else _op)
    for _p in _all_pos:
        _ts = _p.get('entry_timestamp', '') if isinstance(_p, dict) else ''
        if _ts:
            try:
                _dt = datetime.fromisoformat(str(_ts)[:19])
                if _dt.tzinfo is None:
                    _dt = _dt.replace(tzinfo=timezone.utc)
                if first_trade_dt is None or _dt < first_trade_dt:
                    first_trade_dt = _dt
            except:
                pass
    # Fallback: brain-inbox EXECUTION files
    if first_trade_dt is None:
        try:
            import glob as _glob
            for _bf in sorted(_glob.glob('/home/andydoc/ai-workspace/brain-inbox/EXECUTION_*.json')):
                try:
                    _ts2 = json.loads(open(_bf).read()).get('timestamp', '')
                    if _ts2:
                        first_trade_dt = datetime.fromisoformat(_ts2[:19]).replace(tzinfo=timezone.utc)
                        break
                except:
                    continue
        except:
            pass
    if first_trade_dt is None:
        first_trade_dt = START_TIME
    first_trade_str = first_trade_dt.strftime('%d/%m/%Y %H:%M') + ' UTC'
    _elapsed = (datetime.now(timezone.utc) - first_trade_dt).total_seconds() / 86400
    _ic = state.get('initial_capital', 10000)
    _cc = state.get('current_capital', _ic)
    # current_capital is cash only; add capital deployed in open positions
    _deployed = sum(p.get('total_capital', 0) for p in _op if isinstance(p, dict))
    _total_val = _cc + _deployed
    if _elapsed > 0.01 and _ic > 0:
        annualized_ret = ((_total_val - _ic) / _ic) * (365.0 / _elapsed) * 100
        annualized_str = f'{annualized_ret:+.0f}%'

    # Process status rows (new 2-process architecture + engine metrics)
    layer_html = ''
    # Market Scanner (L1)
    l1_st = layer1_status.get('status', '?')
    l1_ts = format_datetime(layer1_status.get('timestamp', ''))
    l1_cls = 'color-green' if l1_st in ('running','scanning') else 'color-amber'
    layer_html += f'<tr><td>Market Scanner</td><td class="{l1_cls}">{l1_st}</td><td>{l1_ts}</td></tr>\n'
    # Trading Engine (with rich metrics)
    eng_st = engine_status.get('status', '?')
    eng_ts = format_datetime(engine_status.get('timestamp', ''))
    eng_cls = 'color-green' if eng_st == 'running' else 'color-amber'
    eng_extra = ''
    em = engine_status.get('metrics', {})
    if engine_status:
        eng_extra = f' | cash=${engine_status.get("capital", 0):.2f} | pos={engine_status.get("positions", engine_status.get("open_positions", 0))}'
    layer_html += f'<tr><td>Trading Engine</td><td class="{eng_cls}">{eng_st}</td><td>{eng_ts}{eng_extra}</td></tr>\n'
    # Engine metrics sub-rows
    if em:
        rust_tag = '<span class="color-green">Rust</span>' if em.get('has_rust') else '<span class="color-amber">Python</span>'
        layer_html += (f'<tr><td class="pl-indent color-dim">Arb Engine</td>'
                      f'<td>{rust_tag}</td>'
                      f'<td>constraints={em.get("constraints",0)} | markets={em.get("markets_total",0)} | iter={em.get("iteration",0)}</td></tr>\n')
        ws_live = em.get('ws_live', 0)
        ws_total = em.get('markets_total', 1)
        ws_pct = int(ws_live / ws_total * 100) if ws_total else 0
        ws_cls = 'color-green' if ws_pct > 20 else 'color-amber'
        layer_html += (f'<tr><td class="pl-indent color-dim">WebSocket</td>'
                      f'<td class="{ws_cls}">subs={em.get("ws_subscribed",0)}</td>'
                      f'<td>msgs={em.get("ws_msgs",0):,} | live={ws_live}/{ws_total} ({ws_pct}%)</td></tr>\n')
        q_urg = em.get('queue_urgent', 0)
        q_bg = em.get('queue_background', 0)
        q_cls = 'color-green' if q_bg < 500 else ('color-amber' if q_bg < 2000 else 'color-red')
        layer_html += (f'<tr><td class="pl-indent color-dim">Eval Queue</td>'
                      f'<td class="{q_cls}">bg={q_bg}</td>'
                      f'<td>urgent={q_urg} | bg={q_bg}</td></tr>\n')
        lat_p50 = em.get('lat_p50_ms', 0)
        lat_p95 = em.get('lat_p95_ms', 0)
        lat_cls = 'color-green' if lat_p50 < 100 else ('color-amber' if lat_p50 < 1000 else 'color-red')
        layer_html += (f'<tr><td class="pl-indent color-dim">Latency</td>'
                      f'<td class="{lat_cls}">p50={lat_p50}ms</td>'
                      f'<td>p50={lat_p50}ms | p95={lat_p95}ms | max={em.get("lat_max_ms",0)}ms</td></tr>\n')
    # Dashboard (always running if we're rendering this)
    dash_ts = now.strftime('%d/%m/%Y %H:%M')
    layer_html += f'<tr><td>Dashboard</td><td class="color-positive">running</td><td>{dash_ts}</td></tr>\n'

    # --- Build annualized badges ---
    paper_ann_badge = ''
    if annualized_str and annualized_str != 'N/A':
        a_cls = 'color-green' if annualized_ret >= 0 else 'color-red'
        paper_ann_badge = f' <span class="tab-annualized {a_cls}">{annualized_str} ann.</span>'

    shadow_ann_badge = ''  # TODO: track shadow-only P&L
    live_ann_badge = ''    # TODO: track live-only P&L

    # --- Build Shadow Tab ---
    shadow_rows = ''
    try:
        import glob
        log_files = sorted(glob.glob(str(WORKSPACE / 'logs' / 'layer4_*.log')), reverse=True)[:2]
        shadow_entries = []
        for lf in log_files:
            with open(lf) as f:
                for line in f:
                    if '[SHADOW]' in line:
                        shadow_entries.append(line.strip())
        shadow_entries = shadow_entries[-50:]  # last 50
        would_trade = [e for e in shadow_entries if 'WOULD TRADE' in e]
        rejected = [e for e in shadow_entries if 'Rejected' in e]
        # Count rejection reasons
        from collections import Counter
        reject_reasons = Counter()
        for e in rejected:
            reason = e.split('- ')[-1] if '- ' in e else '?'
            # Simplify reason
            if 'no_tokens' in reason: reason = 'no_tokens'
            elif 'no_live_price' in reason: reason = 'no_live_price'
            elif 'insufficient_balance' in reason: reason = 'insufficient_balance'
            elif 'insufficient_depth' in reason: reason = 'insufficient_depth'
            elif 'price_drift' in reason: reason = 'price_drift'
            elif 'profit_below' in reason: reason = 'profit_below_threshold'
            elif 'no_mispricing' in reason: reason = 'no_mispricing'
            reject_reasons[reason] = reject_reasons.get(reason, 0) + 1

        shadow_stats = f"""<div class="stats">
  <div class="stat"><div class="label">WOULD TRADE</div><div class="value good">{len(would_trade)}</div></div>
  <div class="stat"><div class="label">REJECTED</div><div class="value">{len(rejected)}</div></div>
  <div class="stat"><div class="label">TOTAL CHECKED</div><div class="value">{len(shadow_entries)}</div></div>
</div>"""

        # Rejection breakdown table
        reject_table = '<h3 class="color-amber sub-heading">Rejection Reasons</h3><table><tr><th>Reason</th><th>Count</th></tr>'
        for reason, cnt in sorted(reject_reasons.items(), key=lambda x: -x[1]):
            reject_table += f'<tr><td>{html_mod.escape(reason)}</td><td>{cnt}</td></tr>'
        reject_table += '</table>'

        # Last 10 would-trade entries
        trade_log = '<h3 class="color-green sub-heading">Recent Would-Trade Signals</h3>'
        if would_trade:
            trade_log += '<table><tr><th>Time</th><th>Opportunity</th><th>Details</th></tr>'
            for e in would_trade[-10:]:
                parts = e.split(' - [L4] INFO - ')
                ts = parts[0][:19] if parts else ''
                msg = parts[1] if len(parts) > 1 else e
                trade_log += f'<tr><td>{html_mod.escape(ts)}</td><td colspan="2" class="color-green">{html_mod.escape(msg[:120])}</td></tr>'
            trade_log += '</table>'
        else:
            trade_log += '<p class="color-muted">No would-trade signals yet. Opportunities that pass all live validation checks will appear here.</p>'

        shadow_tab_html_val = shadow_stats + reject_table + trade_log
    except Exception as e:
        shadow_tab_html_val = f'<p class="color-red">Error loading shadow data: {html_mod.escape(str(e))}</p>'

    # --- Build Live Tab ---
    live_bal_html = ''
    if live_balance is not None:
        live_bal_html = f"""<div class="stats">
  <div class="stat"><div class="label">USDC BALANCE</div><div class="value color-amber">${live_balance:.2f}</div></div>
  <div class="stat"><div class="label">STATUS</div><div class="value {'color-green' if mode_label == 'LIVE' else 'color-amber'}">{'ACTIVE' if mode_label == 'LIVE' else 'SHADOW ONLY'}</div></div>
</div>"""
    else:
        live_bal_html = '<div class="stats"><div class="stat"><div class="label">STATUS</div><div class="value color-muted">NOT CONNECTED</div></div></div>'

    # Live positions (positions with live metadata)
    live_positions_html = '<h3 class="color-cyan sub-heading">Live Positions</h3>'
    live_pos_count = 0
    for p in open_pos:
        live_meta = p.get('metadata', {}).get('live', {})
        if live_meta:
            live_pos_count += 1
    if live_pos_count > 0:
        live_positions_html += f'<p>{live_pos_count} positions with live CLOB orders</p>'
    else:
        live_positions_html += '<p class="color-muted">No live positions. Set shadow_only: false in config and fund your account to start live trading.</p>'

    live_tab_html_val = live_bal_html + live_positions_html

    start_str = START_TIME.strftime('%d/%m/%Y %H:%M')
    now_str = now.strftime('%d/%m/%Y %H:%M:%S')

    html = f'''<!DOCTYPE html>
<html><head>
<meta charset="UTF-8">
<!-- refresh handled by JS -->
<title>Prediction Trader Dashboard</title>
<style>
  body {{ font-family: 'Courier New', monospace; background: #0a0a0a; color: #ccc; padding: 20px; margin: 0; }}
  .header h1 {{ color: #0f0; margin: 0; }}
  .mode-badge {{ display: inline-block; padding: 3px 12px; border-radius: 4px; font-size: 13px; font-weight: bold; margin-left: 12px; vertical-align: middle; letter-spacing: 1px; border: 1px solid; }}
  h2 {{ color: #0af; margin: 20px 0 8px 0; font-size: 16px; }}
  .stats {{ display: flex; gap: 12px; margin: 10px 0; flex-wrap: wrap; }}
  .stat {{ background: #1a1a1a; border: 1px solid #333; padding: 8px 14px; border-radius: 8px; }}
  .stat .label {{ color: #888; font-size: 11px; }}
  .stat .value {{ color: #0f0; font-size: 16px; font-weight: bold; white-space: nowrap; }}
  .stat .value.bad {{ color: #f44; }}
  .stat .value.good {{ color: #0f0; }}
  table {{ border-collapse: collapse; width: 100%; margin: 5px 0; }}
  th {{ background: #1a1a1a; color: #0af; text-align: left; padding: 6px 10px; font-size: 12px; }}
  td {{ border-bottom: 1px solid #222; padding: 5px 10px; font-size: 12px; }}
  td[title] {{ cursor: help; }}
  tr:hover {{ background: #1a1a2a; }}
  tr.good td {{ color: #0f0; }}
  tr.bad td {{ color: #f44; }}
  .bad {{ color: #f44; }}
  .good {{ color: #0f0; }}
  .pos-row {{ cursor: pointer; }}
  .pos-row:hover {{ background: #1a2a1a !important; }}
  .pos-row td:first-child::before {{ content: '\\25B6 \\00a0'; color: #555; font-size: 10px; }}
  .pos-row.expanded td:first-child::before {{ content: '\\25BC \\00a0'; color: #0af; font-size: 10px; }}
  .detail-row {{ display: none; }}
  .detail-row.show {{ display: table-row; }}
  .detail-row td {{ padding: 4px 10px 8px 30px; color: #999; font-size: 11px; border-bottom: 1px solid #333; background: #111; }}
  .leg {{ margin: 2px 0; }}
  .leg .amt {{ color: #0f0; }}
  .leg .mkt {{ color: #ccc; }}
  .leg .win {{ color: #ff0; }}
  tr.dup-opp td {{ color: #555 !important; }}
  tr.dup-opp:hover {{ background: #111 !important; }}
  tr.dup-opp td:first-child::before {{ color: #333 !important; }}
  .footer {{ color: #555; font-size: 11px; margin-top: 20px; }}
.sub-section-title {{ cursor:pointer; font-size:13px; color:#aaa; padding:4px 8px; margin:8px 0 2px 0; border-left:3px solid #333; }}
.sub-section-title:hover {{ color:#fff; }}
.sub-section-title::before {{ content:'▼ '; color:#555; font-size:10px; }}
.sub-section-title.collapsed::before {{ content:'▶ '; color:#555; font-size:10px; }}
  .section-title {{ cursor: pointer; user-select: none; }}
  .section-title:hover {{ color: #0f0; }}
  .section-title::before {{ content: '\\25BC \\00a0'; font-size: 10px; }}
  .section-title.collapsed::before {{ content: '\\25B6 \\00a0'; font-size: 10px; }}
  .section-content.hidden {{ display: none; }}
  .collapse-all-btn {{ background: #333; color: #0af; border: 1px solid #0af; padding: 2px 10px; font-size: 11px; cursor: pointer; margin-left: 12px; font-family: 'Courier New', monospace; border-radius: 4px; vertical-align: middle; }}
  .collapse-all-btn:hover {{ background: #0af; color: #000; }}

  /* Tab navigation */
  .tab-bar {{ display: flex; gap: 0; margin: 12px 0 0 0; border-bottom: 2px solid #333; }}
  .tab-btn {{ padding: 8px 20px; background: #111; color: #888; border: 1px solid #333; border-bottom: none; cursor: pointer; font-family: 'Courier New', monospace; font-size: 13px; font-weight: bold; letter-spacing: 0.5px; border-radius: 6px 6px 0 0; margin-right: 2px; transition: all 0.15s; }}
  .tab-btn:hover {{ color: #ccc; background: #1a1a2a; }}
  .tab-btn.active {{ color: #0af; background: #0a0a0a; border-color: #0af; border-bottom: 2px solid #0a0a0a; margin-bottom: -2px; }}
  .tab-content {{ display: none; padding-top: 5px; }}
  .tab-content.active {{ display: block; }}
  .tab-annualized {{ display: inline-block; font-size: 11px; padding: 2px 8px; border-radius: 3px; margin-left: 8px; vertical-align: middle; }}
  /* === Header flex layout === */
  .header {{ display: flex; justify-content: space-between; align-items: center; border-bottom: 2px solid #0f0; padding-bottom: 12px; margin-bottom: 18px; }}
  .header h1 {{ margin: 0; }}
  .meta {{ text-align: right; line-height: 1.8; font-size: 12px; color: #888; }}
  .meta span {{ color: #ccc; }}
        .val-tag {{ font-size: 0.68em; margin-left: 3px; padding: 1px 3px; border-radius: 3px; vertical-align: middle; }}
        .vtick {{ color: #4c4; background: #1a2e1a; border: 1px solid #2a4a2a; }}
        .vapi  {{ color: #888; background: #1e1e1e; border: 1px solid #333; }}
  /* Dynamic utility classes — replace inline style= for easy theming */
  .color-positive {{ color: #4c4; }}
  .color-negative {{ color: #f55; }}
  .color-green {{ color: #0f0; }}
  .color-red {{ color: #f44; }}
  .color-amber {{ color: #fa0; }}
  .color-cyan {{ color: #0af; }}
  .color-muted {{ color: #555; }}
  .color-dim {{ color: #888; }}
  .text-bold {{ font-weight: bold; }}
  .text-sm {{ font-size: 11px; }}
  .text-xs {{ font-size: 10px; }}
  .mt-sm {{ margin-top: 4px; }}
  .mt-md {{ margin-top: 6px; }}
  .mt-lg {{ margin-top: 15px; }}
  .mb-sm {{ margin-bottom: 5px; }}
  .pl-indent {{ padding-left: 25px; }}
  .scenario-header {{ margin-top: 6px; font-size: 11px; }}
  .scenario-line {{ font-size: 11px; }}
  .guaranteed-line {{ margin-top: 4px; font-weight: bold; }}
  .sub-heading {{ margin: 15px 0 5px; }}
  .agg-total-row {{ border-top: 2px solid #0af; font-weight: bold; }}
  /* Mode badge — set dynamically via Python class */
  .mode-shadow {{ color: #fa0; background: #332800; border-color: #fa0; }}
  .mode-live {{ color: #f44; background: #3a1111; border-color: #f44; }}
  /* SSE connection indicator */
  .sse-dot {{ display: inline-block; width: 8px; height: 8px; border-radius: 50%; background: #555; vertical-align: middle; margin-right: 4px; transition: background 0.3s; }}
  .sse-dot.connected {{ background: #0f0; }}
  .sse-dot.disconnected {{ background: #f44; }}
</style>
<script>
function toggleOpp(idx) {{
  var d = document.getElementById("opp-detail-" + idx);
  if (d) {{
    d.classList.toggle("show");
    var prev = d.previousElementSibling;
    if (prev) prev.classList.toggle("expanded");
  }}
}}
function togglePos(idx) {{
  var row = document.getElementById('detail-' + idx);
  var main = document.getElementById('pos-' + idx);
  if (row) {{
    row.classList.toggle('show');
    main.classList.toggle('expanded');
  }}
}}
function collapseAll() {{
  document.querySelectorAll('.detail-row.show').forEach(function(r) {{ r.classList.remove('show'); }});
  document.querySelectorAll('.pos-row.expanded').forEach(function(r) {{ r.classList.remove('expanded'); }});
}}
function toggleSection(el) {{
  el.classList.toggle('collapsed');
  var content = el.nextElementSibling;
  if (content) content.classList.toggle('hidden');
}}

// === SSE: live stats updates without page reload ===
(function() {{
  var es = new EventSource('/stream');
  var dot = document.getElementById('sse-dot');
  es.onmessage = function(e) {{
    try {{
      var d = JSON.parse(e.data);
      function upd(id, val, cls) {{
        var el = document.getElementById(id);
        if (el) {{ el.textContent = val; if (cls !== undefined) el.className = 'value ' + cls; }}
      }}
      upd('stat-total', '$' + d.total_value.toFixed(2), d.total_value >= d.init_cap ? 'good' : 'bad');
      upd('stat-cash', '$' + d.cash.toFixed(2));
      upd('stat-deployed', '$' + d.deployed.toFixed(2));
      upd('stat-fees', '$' + d.fees.toFixed(2));
      upd('stat-return', (d.ret_pct >= 0 ? '+' : '') + d.ret_pct.toFixed(1) + '%', d.ret_pct >= 0 ? 'good' : 'bad');
      upd('stat-trades', '' + d.trades);
      upd('stat-open', '' + d.open_count);
      upd('stat-realized', '$' + d.realized.toFixed(2), d.realized >= 0 ? 'good' : 'bad');
      if (d.annualized) upd('stat-annualized', d.annualized, d.annualized_ret >= 0 ? 'good' : 'bad');
      var ts = document.getElementById('last-update');
      if (ts) ts.textContent = d.timestamp;
      if (dot) {{ dot.className = 'sse-dot connected'; clearTimeout(dot._t); dot._t = setTimeout(function(){{ dot.className = 'sse-dot'; }}, 1500); }}
    }} catch(ex) {{ console.warn('SSE parse error', ex); }}
  }};
  es.onerror = function() {{
    if (dot) dot.className = 'sse-dot disconnected';
  }};
}})();

// === Tab switching with URL hash persistence ===
function switchTab(tabName) {{
  document.querySelectorAll('.tab-btn').forEach(function(b) {{ b.classList.remove('active'); }});
  document.querySelectorAll('.tab-content').forEach(function(c) {{ c.classList.remove('active'); }});
  document.getElementById('tab-' + tabName).classList.add('active');
  var btns = document.querySelectorAll('.tab-btn');
  for (var i = 0; i < btns.length; i++) {{ if (btns[i].getAttribute('data-tab') === tabName) btns[i].classList.add('active'); }}
  window.location.hash = tabName;
}}
(function() {{
  var hash = window.location.hash.replace('#', '');
  if (hash && document.getElementById('tab-' + hash)) switchTab(hash);
}})();
</script>
</head><body>

<div class="header">
  <h1>&#x1F4C8; PREDICTION TRADER <span class="mode-badge {mode_class}">{mode_label}</span></h1>
  <div class="meta">
    Started: <span>{first_trade_str}</span><br>
    System Restarted: <span>{start_str} UTC</span><br>
    Starting Capital: <span>${init_cap:.2f}</span>
  </div>
</div>

<div class="stats">
  <div class="stat">
    <div class="label">TOTAL VALUE</div>
    <div id="stat-total" class="value {'good' if total_value >= init_cap else 'bad'}">${total_value:.2f}</div>
  </div>
  <div class="stat">
    <div class="label">CASH</div>
    <div id="stat-cash" class="value">${cap:.2f}</div>
  </div>
  <div class="stat">
    <div class="label">DEPLOYED</div>
    <div id="stat-deployed" class="value">${deployed:.2f}</div>
  </div>
  <div class="stat">
    <div class="label">FEES PAID</div>
    <div id="stat-fees" class="value">${total_fees:.2f}</div>
  </div>
  {f'<div class="stat"><div class="label">USDC (LIVE)</div><div id="stat-usdc" class="value color-amber">${live_balance:.2f}</div></div>' if live_balance is not None else ''}
  <div class="stat">
    <div class="label">RETURN</div>
    <div id="stat-return" class="value {'good' if ret_pct >= 0 else 'bad'}">{ret_pct:+.1f}%</div>
  </div>
  <div class="stat">
    <div class="label">TRADES</div>
    <div id="stat-trades" class="value">{trades}</div>
  </div>
  <div class="stat">
    <div class="label">OPEN</div>
    <div id="stat-open" class="value">{len(open_pos)}</div>
  </div>
  <div class="stat">
    <div class="label">REALIZED P&L</div>
    <div id="stat-realized" class="value {'good' if total_realized >= 0 else 'bad'}">${total_realized:.2f}</div>
  </div>
  <div class="stat">
    <div class="label">ANNUALIZED (closed)</div>
    <div id="stat-annualized" class="value {'good' if annualized_ret >= 0 else 'bad'}">{annualized_str}</div>
  </div>
</div>

<div class="tab-bar">
  <div class="tab-btn active" data-tab="positions" onclick="switchTab('positions')">Positions{paper_ann_badge}</div>
  <div class="tab-btn" data-tab="shadow" onclick="switchTab('shadow')">Shadow{shadow_ann_badge}</div>
  <div class="tab-btn" data-tab="live" onclick="switchTab('live')">Live{live_ann_badge}</div>
</div>

<div id="tab-positions" class="tab-content active">
<h2 class="section-title" id="section-positions" onclick="toggleSection(this)">OPEN POSITIONS ({len(open_pos)})<button class="collapse-all-btn" onclick="event.stopPropagation(); collapseAll()">Collapse All</button></h2>
<div class="section-content">
<table>
<tr><th>#</th><th>Market</th><th>Strategy</th><th>Score</th><th>Deployed</th><th>Expected P&L</th><th>Resolves</th><th>Status</th><th>Entered</th></tr>
{pos_rows_html if pos_rows_html else '<tr><td colspan="8" class="color-muted">No open positions</td></tr>'}
</table>
</div>

<h2 class="section-title collapsed" onclick="toggleSection(this)">AGGREGATE HOLDINGS ({len(agg_markets)} markets)</h2>
<div class="section-content hidden">
<table>
<tr><th>Market</th><th>Side</th><th>Deployed</th><th>Avg Price</th><th>Payout (if wins)</th><th>Pos#</th></tr>
{agg_rows_html}
</table>
</div>

<h2 class="section-title collapsed" onclick="toggleSection(this)">OPPORTUNITIES ({len(opps)} found, top 20 by score)</h2>
<div class="section-content hidden">
<table>
<tr><th>#</th><th>Profit%</th><th>Resolves</th><th>Strategy</th><th>Score</th><th>Market</th></tr>
{opp_rows_html if opp_rows_html else '<tr><td colspan="7" class="color-muted">No opportunities</td></tr>'}
</table>
</div>

<h2 class="section-title collapsed" onclick="toggleSection(this)">SYSTEM</h2>
<div class="section-content hidden">
<table>
<tr><th>Process</th><th>Status</th><th>Info</th></tr>
{layer_html}
</table>
</div>

{'<h2 class="section-title collapsed" onclick="toggleSection(this)">CLOSED POSITIONS</h2><div class="section-content hidden">' + closed_html + '</div>' if closed_html else ''}

</div><!-- end tab-positions -->

<div id="tab-shadow" class="tab-content">
{shadow_tab_html_val}
</div>

<div id="tab-live" class="tab-content">
{live_tab_html_val}
</div>

<div class="footer">Updated: <span id="last-update">{now_str}</span> UTC | <span class="sse-dot" id="sse-dot"></span> Live via SSE</div>
</body></html>'''
    return html

def get_stats_json():
    """Return key dashboard metrics as a JSON-serializable dict for SSE."""
    state = load_execution_state()
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
    # Annualized
    annualized_str = 'N/A'
    annualized_ret = 0
    if closed_pos:
        timestamps = [p.get('entry_timestamp') for p in closed_pos if p.get('entry_timestamp')]
        if timestamps:
            try:
                first_dt = min(datetime.fromisoformat(str(t)) for t in timestamps)
                if first_dt.tzinfo is None:
                    first_dt = first_dt.replace(tzinfo=timezone.utc)
                days = (datetime.now(timezone.utc) - first_dt).total_seconds() / 86400
                if days > 1:
                    annualized_ret = (total_realized / init_cap) * (365 / days) * 100
                    annualized_str = f'{annualized_ret:+.0f}%'
            except Exception:
                pass
    now = datetime.now(timezone.utc)
    return {
        'cash': cap, 'deployed': deployed, 'total_value': total_value,
        'init_cap': init_cap, 'fees': total_fees, 'ret_pct': ret_pct,
        'trades': trades, 'open_count': len(open_pos),
        'realized': total_realized, 'annualized': annualized_str,
        'annualized_ret': annualized_ret,
        'timestamp': now.strftime('%d/%m/%Y %H:%M:%S'),
    }

class ThreadingDashboardServer(ThreadingMixIn, HTTPServer):
    daemon_threads = True

class DashboardHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == '/stream':
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Cache-Control', 'no-cache')
            self.send_header('Connection', 'keep-alive')
            self.send_header('Access-Control-Allow-Origin', '*')
            self.end_headers()
            try:
                while True:
                    data = json.dumps(get_stats_json())
                    self.wfile.write(f'data: {data}\n\n'.encode())
                    self.wfile.flush()
                    time.sleep(5)
            except (BrokenPipeError, ConnectionResetError, OSError):
                pass  # Client disconnected
            return
        elif self.path == '/status':
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.end_headers()
            state = load_execution_state()
            self.wfile.write(json.dumps(state, indent=2).encode())
        else:
            try:
                page = make_html()
            except Exception as e:
                import traceback
                page = f'<html><body style="background:#0a0a0a;color:#f44;font-family:monospace;padding:20px"><h1>Dashboard Error</h1><pre>{traceback.format_exc()}</pre></body></html>'
            self.send_response(200)
            self.send_header('Content-Type', 'text/html')
            self.end_headers()
            self.wfile.write(page.encode())
    def log_message(self, format, *args):
        pass  # suppress access logs

    def _json_response(self, data, code=200):
        self.send_response(code)
        self.send_header('Content-Type', 'application/json')
        self.end_headers()
        self.wfile.write(json.dumps(data).encode())

if __name__ == '__main__':
    port = 5556
    print(f'Dashboard starting on http://localhost:{port} (SSE-enabled)')
    ThreadingDashboardServer(('0.0.0.0', port), DashboardHandler).serve_forever()
