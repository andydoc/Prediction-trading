"""Standalone Dashboard - replaces the basic one in main.py
Run: python dashboard_server.py
Serves on http://localhost:5556
"""
import json, os, sys, html as html_mod
from pathlib import Path
from datetime import datetime, timezone
from http.server import HTTPServer, BaseHTTPRequestHandler

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

# Track when dashboard/system started
START_TIME = datetime.now(timezone.utc)

def load_json(p):
    try:
        return json.loads(Path(p).read_text())
    except:
        return {}

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

    # Layer statuses
    layers = {}
    for name in ['layer1','layer2','layer3','layer4']:
        layers[name] = load_json(DATA / f'{name}_status.json')

    # Execution state
    state = load_json(EXEC_STATE)

    # Load trading mode
    cfg = load_config()
    config = cfg  # alias for template
    trading_mode = cfg.get('mode', 'paper_trading')
    live_cfg = cfg.get('live_trading', {})
    live_enabled = live_cfg.get('enabled', False)
    shadow_only = live_cfg.get('shadow_only', False)
    if live_enabled and shadow_only:
        mode_label = 'SHADOW'
        mode_color = '#fa0'
        mode_bg = '#332800'
    elif trading_mode == 'live_trading' or (live_enabled and not shadow_only and trading_mode != 'dual'):
        mode_label = 'LIVE'
        mode_color = '#f44'
        mode_bg = '#3a1111'
    elif trading_mode == 'dual' and live_enabled and not shadow_only:
        mode_label = 'DUAL (PAPER+LIVE)'
        mode_color = '#fa0'
        mode_bg = '#3a2a00'
    else:
        mode_label = 'PAPER'
        mode_color = '#0f0'
        mode_bg = '#113311'

    # Fetch live USDC balance if live/dual mode
    live_balance = None
    if trading_mode in ('live_trading', 'dual') or live_enabled:
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
            legs_html += '<div class="leg" style="margin-top:6px;color:#888;font-size:11px">Scenarios (exactly one wins YES, that NO leg loses):</div>'
            scenario_payouts = []
            for wmid, wmd in market_list:
                wname = wmd.get('name', '?')[:40]
                winners = [(mid2, shares_map[mid2]) for mid2 in shares_map if mid2 != wmid]
                payout = sum(s for _, s in winners)
                profit = payout - total_cap
                parts = ' + '.join(f'<span style="color:#4c4">${s:.2f}</span>' for _, s in winners)
                css = 'color:#4c4' if profit >= 0 else 'color:#f55'
                legs_html += (f'<div class="leg" style="font-size:11px">'
                             f'If <b>{escape_html(wname)}</b> wins: {parts}'
                             f' = <span style="{css}">${payout:.2f}</span>'
                             f' (<span style="{css}">{profit:+.2f}</span>)</div>')
                scenario_payouts.append(payout)
            guaranteed = min(scenario_payouts) if scenario_payouts else 0
            gprofit = guaranteed - total_cap
            gcss = 'color:#4c4' if gprofit >= 0 else 'color:#f55'
            legs_html += (f'<div class="leg" style="margin-top:4px;color:#0af;font-weight:bold">'
                         f'Guaranteed payout: ${guaranteed:.2f} '
                         f'(profit <span style="{gcss}">${gprofit:+.2f}</span>)'
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
                legs_html += (f'<div class="leg" style="margin-top:4px;color:#0af">'
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
    agg_rows_html += (f'<tr style="border-top:2px solid #0af;font-weight:bold">'
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
            opp_legs += '<div class="leg" style="margin-top:6px;color:#888;font-size:11px">Scenarios (one wins YES, that NO leg loses):</div>'
            for k3, wmid in enumerate(opp_mids):
                wname = opp_names[k3][:40] if k3 < len(opp_names) else '?'
                winners = [(m, sh_map[m]) for m in opp_mids if m != wmid]
                spayout = sum(s for _, s in winners)
                sprofit = spayout - opp_cap
                sparts = ' + '.join(f'<span style="color:#4c4">${s:.2f}</span>' for _, s in winners)
                scss = 'color:#4c4' if sprofit >= 0 else 'color:#f55'
                opp_legs += (f'<div class="leg" style="font-size:11px">'
                            f'If <b>{escape_html(wname)}</b> wins: {sparts}'
                            f' = <span style="{scss}">{spayout:.2f}</span>'
                            f' (<span style="{scss}">{sprofit:+.2f}</span>)</div>')
        # Guaranteed payout
        opp_net = opp_dict.get('net_profit', opp_dict.get('expected_profit', 0))
        opp_fees = opp_dict.get('fees_estimated', 0)
        opp_legs += (f'<div class="leg" style="margin-top:4px;color:#0af;font-weight:bold">'
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

    # Layer status rows
    layer_html = ''
    for name, info in layers.items():
        st = info.get('status', '?')
        ts = format_datetime(info.get('timestamp', ''))
        extra = ''
        if name == 'layer4':
            extra = f' | cash=${info.get("capital",0):.2f} | pos={info.get("open_positions",0)}'
        color = '#0f0' if st in ('running','scanning') else '#f80'
        layer_labels = {
            'layer1': '1 Market Data',
            'layer2': '2 Constraint Detection',
            'layer3': '3 Arbitrage Math',
            'layer4': '4 Execution Engine',
            'dashboard': 'Dashboard',
        }
        label = layer_labels.get(name, name)
        layer_html += f'<tr><td>{label}</td><td style="color:{color}">{st}</td><td>{ts}{extra}</td></tr>\n'

    # --- Build annualized badges ---
    config = cfg  # alias for control panel template
    paper_ann_badge = ''
    if annualized_str and annualized_str != 'N/A':
        a_color = '#0f0' if annualized_ret >= 0 else '#f44'
        paper_ann_badge = f' <span class="tab-annualized" style="color:{a_color}">{annualized_str} ann.</span>'

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
        reject_table = '<h3 style="color:#fa0;margin:15px 0 5px">Rejection Reasons</h3><table><tr><th>Reason</th><th>Count</th></tr>'
        for reason, cnt in sorted(reject_reasons.items(), key=lambda x: -x[1]):
            reject_table += f'<tr><td>{html_mod.escape(reason)}</td><td>{cnt}</td></tr>'
        reject_table += '</table>'

        # Last 10 would-trade entries
        trade_log = '<h3 style="color:#0f0;margin:15px 0 5px">Recent Would-Trade Signals</h3>'
        if would_trade:
            trade_log += '<table><tr><th>Time</th><th>Opportunity</th><th>Details</th></tr>'
            for e in would_trade[-10:]:
                parts = e.split(' - [L4] INFO - ')
                ts = parts[0][:19] if parts else ''
                msg = parts[1] if len(parts) > 1 else e
                trade_log += f'<tr><td>{html_mod.escape(ts)}</td><td colspan="2" style="color:#0f0">{html_mod.escape(msg[:120])}</td></tr>'
            trade_log += '</table>'
        else:
            trade_log += '<p style="color:#555">No would-trade signals yet. Opportunities that pass all live validation checks will appear here.</p>'

        shadow_tab_html_val = shadow_stats + reject_table + trade_log
    except Exception as e:
        shadow_tab_html_val = f'<p style="color:#f44">Error loading shadow data: {html_mod.escape(str(e))}</p>'

    # --- Build Live Tab ---
    live_bal_html = ''
    if live_balance is not None:
        live_bal_html = f"""<div class="stats">
  <div class="stat"><div class="label">USDC BALANCE</div><div class="value" style="color:#fa0">${live_balance:.2f}</div></div>
  <div class="stat"><div class="label">STATUS</div><div class="value" style="color:{'#0f0' if mode_label in ('LIVE','DUAL (PAPER+LIVE)') else '#fa0'}">{'ACTIVE' if mode_label in ('LIVE','DUAL (PAPER+LIVE)') else 'SHADOW ONLY'}</div></div>
</div>"""
    else:
        live_bal_html = '<div class="stats"><div class="stat"><div class="label">STATUS</div><div class="value" style="color:#555">NOT CONNECTED</div></div></div>'

    # Live positions (positions with live metadata)
    live_positions_html = '<h3 style="color:#0af;margin:15px 0 5px">Live Positions</h3>'
    live_pos_count = 0
    for p in open_pos:
        live_meta = p.get('metadata', {}).get('live', {})
        if live_meta:
            live_pos_count += 1
    if live_pos_count > 0:
        live_positions_html += f'<p>{live_pos_count} positions with live CLOB orders</p>'
    else:
        live_positions_html += '<p style="color:#555">No live positions. Switch to DUAL or LIVE mode and fund your account to start.</p>'

    live_tab_html_val = live_bal_html + live_positions_html

    # --- Build Control Panel Tab ---
    ctrl_mode_html = f'Mode: <span style="color:{mode_color}">{mode_label}</span><br>'
    control_tab_html_val = """
<div style="display:grid;grid-template-columns:1fr 1fr;gap:20px;margin-top:10px">
  <div>
    <h3 style="color:#0af;margin:0 0 10px">Mode Control</h3>
    <div style="display:flex;flex-direction:column;gap:8px">
      <button class="ctrl-btn paper-btn" onclick="setMode('paper')">Switch to PAPER</button>
      <button class="ctrl-btn shadow-btn" onclick="setMode('shadow')">Switch to SHADOW</button>
      <button class="ctrl-btn live-btn" onclick="setMode('live')">Switch to LIVE ⚠️</button>
    </div>
    <h3 style="color:#0af;margin:20px 0 10px">System</h3>
    <div style="display:flex;flex-direction:column;gap:8px">
      <button class="ctrl-btn" onclick="ctrlAction('restart')">Restart All Layers</button>
      <button class="ctrl-btn" onclick="ctrlAction('restart_l4')">Restart L4 Only</button>
      <button class="ctrl-btn danger-btn" onclick="ctrlAction('stop')">EMERGENCY STOP</button>
    </div>
  </div>
  <div>
    <h3 style="color:#0af;margin:0 0 10px">Parameters</h3>
    <div style="display:grid;grid-template-columns:180px 80px;gap:6px;align-items:center">
      <label style="color:#888">Max Positions:</label>
      <input type="number" class="ctrl-input" id="param-max-pos" value=""" + f'"{config.get("max_open_positions", 20)}"' + """ min="1" max="50">
      <label style="color:#888">Capital Per Trade ($):</label>
      <input type="number" class="ctrl-input" id="param-cap-trade" value=""" + f'"{config.get("live_trading",dict()).get("capital_per_trade", 10)}"' + """ min="1" max="1000">
      <label style="color:#888">Min Profit (%):</label>
      <input type="number" class="ctrl-input" id="param-min-profit" value=""" + f'"{config.get("fees",dict()).get("min_profit_threshold", 0.03) * 100:.1f}"' + """ min="0.1" max="50" step="0.1">
      <label style="color:#888">Max Price Drift (%):</label>
      <input type="number" class="ctrl-input" id="param-drift" value=""" + f'"{config.get("live_trading",dict()).get("max_price_drift_pct", 0.05) * 100:.0f}"' + """ min="1" max="20">
    </div>
    <button class="ctrl-btn" style="margin-top:12px" onclick="saveParams()">Save Parameters</button>
    <p style="color:#555;font-size:10px;margin-top:8px">Changes require L4 restart to take effect</p>
    <h3 style="color:#0af;margin:20px 0 10px">Quick Info</h3>
    <div style="color:#888;font-size:11px;line-height:1.6">
' + ctrl_mode_html + '
      Config: config/config.yaml<br>
      Secrets: config/secrets.yaml<br>
      L4 Log: logs/layer4_*.log
    </div>
  </div>
</div>
<div id="ctrl-status" style="margin-top:15px;padding:8px;display:none;border:1px solid #333;border-radius:4px;font-size:12px"></div>
"""

    # --- Build annualized badges ---
    paper_ann_badge = ''
    if annualized_str and annualized_str != 'N/A':
        a_color = '#0f0' if annualized_ret >= 0 else '#f44'
        paper_ann_badge = f' <span class="tab-annualized" style="color:{a_color}">{annualized_str} ann.</span>'

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
        reject_table = '<h3 style="color:#fa0;margin:15px 0 5px">Rejection Reasons</h3><table><tr><th>Reason</th><th>Count</th></tr>'
        for reason, cnt in sorted(reject_reasons.items(), key=lambda x: -x[1]):
            reject_table += f'<tr><td>{html_mod.escape(reason)}</td><td>{cnt}</td></tr>'
        reject_table += '</table>'

        # Last 10 would-trade entries
        trade_log = '<h3 style="color:#0f0;margin:15px 0 5px">Recent Would-Trade Signals</h3>'
        if would_trade:
            trade_log += '<table><tr><th>Time</th><th>Opportunity</th><th>Details</th></tr>'
            for e in would_trade[-10:]:
                parts = e.split(' - [L4] INFO - ')
                ts = parts[0][:19] if parts else ''
                msg = parts[1] if len(parts) > 1 else e
                trade_log += f'<tr><td>{html_mod.escape(ts)}</td><td colspan="2" style="color:#0f0">{html_mod.escape(msg[:120])}</td></tr>'
            trade_log += '</table>'
        else:
            trade_log += '<p style="color:#555">No would-trade signals yet. Opportunities that pass all live validation checks will appear here.</p>'

        shadow_tab_html_val = shadow_stats + reject_table + trade_log
    except Exception as e:
        shadow_tab_html_val = f'<p style="color:#f44">Error loading shadow data: {html_mod.escape(str(e))}</p>'

    # --- Build Live Tab ---
    live_bal_html = ''
    if live_balance is not None:
        live_bal_html = f"""<div class="stats">
  <div class="stat"><div class="label">USDC BALANCE</div><div class="value" style="color:#fa0">${live_balance:.2f}</div></div>
  <div class="stat"><div class="label">STATUS</div><div class="value" style="color:{'#0f0' if mode_label in ('LIVE','DUAL (PAPER+LIVE)') else '#fa0'}">{'ACTIVE' if mode_label in ('LIVE','DUAL (PAPER+LIVE)') else 'SHADOW ONLY'}</div></div>
</div>"""
    else:
        live_bal_html = '<div class="stats"><div class="stat"><div class="label">STATUS</div><div class="value" style="color:#555">NOT CONNECTED</div></div></div>'

    # Live positions (positions with live metadata)
    live_positions_html = '<h3 style="color:#0af;margin:15px 0 5px">Live Positions</h3>'
    live_pos_count = 0
    for p in open_pos:
        live_meta = p.get('metadata', {}).get('live', {})
        if live_meta:
            live_pos_count += 1
    if live_pos_count > 0:
        live_positions_html += f'<p>{live_pos_count} positions with live CLOB orders</p>'
    else:
        live_positions_html += '<p style="color:#555">No live positions. Switch to DUAL or LIVE mode and fund your account to start.</p>'

    live_tab_html_val = live_bal_html + live_positions_html

    # --- Build Control Panel Tab ---
    control_tab_html_val = """
<div style="display:grid;grid-template-columns:1fr 1fr;gap:20px;margin-top:10px">
  <div>
    <h3 style="color:#0af;margin:0 0 10px">Mode Control</h3>
    <div style="display:flex;flex-direction:column;gap:8px">
      <button class="ctrl-btn paper-btn" onclick="setMode('paper')">Switch to PAPER</button>
      <button class="ctrl-btn shadow-btn" onclick="setMode('shadow')">Switch to SHADOW</button>
      <button class="ctrl-btn live-btn" onclick="setMode('live')">Switch to LIVE ⚠️</button>
    </div>
    <h3 style="color:#0af;margin:20px 0 10px">System</h3>
    <div style="display:flex;flex-direction:column;gap:8px">
      <button class="ctrl-btn" onclick="ctrlAction('restart')">Restart All Layers</button>
      <button class="ctrl-btn" onclick="ctrlAction('restart_l4')">Restart L4 Only</button>
      <button class="ctrl-btn danger-btn" onclick="ctrlAction('stop')">EMERGENCY STOP</button>
    </div>
  </div>
  <div>
    <h3 style="color:#0af;margin:0 0 10px">Parameters</h3>
    <div style="display:grid;grid-template-columns:180px 80px;gap:6px;align-items:center">
      <label style="color:#888">Max Positions:</label>
      <input type="number" class="ctrl-input" id="param-max-pos" value=""" + f'"{config.get("max_open_positions", 20)}"' + """ min="1" max="50">
      <label style="color:#888">Capital Per Trade ($):</label>
      <input type="number" class="ctrl-input" id="param-cap-trade" value=""" + f'"{config.get("live_trading",{}).get("capital_per_trade", 10)}"' + """ min="1" max="1000">
      <label style="color:#888">Min Profit (%):</label>
      <input type="number" class="ctrl-input" id="param-min-profit" value=""" + f'"{config.get("fees",{}).get("min_profit_threshold", 0.03) * 100:.1f}"' + """ min="0.1" max="50" step="0.1">
      <label style="color:#888">Max Price Drift (%):</label>
      <input type="number" class="ctrl-input" id="param-drift" value=""" + f'"{config.get("live_trading",{}).get("max_price_drift_pct", 0.05) * 100:.0f}"' + """ min="1" max="20">
    </div>
    <button class="ctrl-btn" style="margin-top:12px" onclick="saveParams()">Save Parameters</button>
    <p style="color:#555;font-size:10px;margin-top:8px">Changes require L4 restart to take effect</p>
    <h3 style="color:#0af;margin:20px 0 10px">Quick Info</h3>
    <div style="color:#888;font-size:11px;line-height:1.6">
      Mode: <span style="color:{mode_color}">{mode_label}</span><br>
      Config: config/config.yaml<br>
      Secrets: config/secrets.yaml<br>
      L4 Log: logs/layer4_*.log
    </div>
  </div>
</div>
<div id="ctrl-status" style="margin-top:15px;padding:8px;display:none;border:1px solid #333;border-radius:4px;font-size:12px"></div>
"""

    start_str = START_TIME.strftime('%d/%m/%Y %H:%M')
    now_str = now.strftime('%d/%m/%Y %H:%M:%S')

    html = f'''<!DOCTYPE html>
<html><head>
<meta charset="UTF-8">
<!-- refresh handled by JS -->
<title>Prediction Trader Dashboard</title>
<style>
  body {{ font-family: 'Courier New', monospace; background: #0a0a0a; color: #ccc; padding: 20px; margin: 0; }}
  .header {{ display: flex; justify-content: space-between; align-items: baseline; margin: 0 0 5px 0; }}
  .header h1 {{ color: #0f0; margin: 0; }}
  .mode-badge {{ display: inline-block; padding: 3px 12px; border-radius: 4px; font-size: 13px; font-weight: bold; margin-left: 12px; vertical-align: middle; letter-spacing: 1px; border: 1px solid; }}
  .header .meta {{ color: #888; font-size: 12px; text-align: right; }}
  .header .meta span {{ color: #aaa; }}
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
  .section-content {{ }}
  .section-content.hidden {{ display: none; }}
  .section-btn {{ background: #333; color: #0af; border: 1px solid #0af; padding: 2px 10px; font-size: 11px; cursor: pointer; margin-left: 12px; font-family: 'Courier New', monospace; border-radius: 4px; display: none; vertical-align: middle; }}
  .section-btn:hover {{ background: #0af; color: #000; }}
  .section-btn.paused {{ color: #f80; border-color: #f80; }}
  .section-btn.paused:hover {{ background: #f80; color: #000; }}
  .refresh-indicator {{ color: #555; font-size: 11px; margin-left: 8px; }}
  .refresh-indicator.paused {{ color: #f80; }}

  /* Control panel */
  .ctrl-btn {{ padding: 8px 16px; background: #1a1a2a; color: #0af; border: 1px solid #0af; cursor: pointer; font-family: 'Courier New', monospace; font-size: 12px; border-radius: 4px; transition: all 0.15s; width: 100%; }}
  .ctrl-btn:hover {{ background: #0af; color: #000; }}
  .danger-btn {{ color: #f44; border-color: #f44; }}
  .danger-btn:hover {{ background: #f44; color: #000; }}
  .shadow-btn {{ color: #fa0; border-color: #fa0; }}
  .shadow-btn:hover {{ background: #fa0; color: #000; }}
  .paper-btn {{ color: #0f0; border-color: #0f0; }}
  .paper-btn:hover {{ background: #0f0; color: #000; }}
  .live-btn {{ color: #f44; border-color: #f44; }}
  .live-btn:hover {{ background: #f44; color: #000; }}
  .ctrl-input {{ background: #111; color: #ccc; border: 1px solid #444; padding: 4px 8px; font-family: 'Courier New', monospace; font-size: 12px; border-radius: 3px; width: 70px; }}
  .ctrl-input:focus {{ border-color: #0af; outline: none; }}

  /* Control panel */
  .ctrl-btn {{ padding: 8px 16px; background: #1a1a2a; color: #0af; border: 1px solid #0af; border-radius: 4px; cursor: pointer; font-family: 'Courier New', monospace; font-size: 12px; transition: all 0.15s; }}
  .ctrl-btn:hover {{ background: #0af; color: #000; }}
  .danger-btn {{ color: #f44; border-color: #f44; }}
  .danger-btn:hover {{ background: #f44; color: #000; }}
  .shadow-btn {{ color: #fa0; border-color: #fa0; }}
  .shadow-btn:hover {{ background: #fa0; color: #000; }}
  .paper-btn {{ color: #0f0; border-color: #0f0; }}
  .paper-btn:hover {{ background: #0f0; color: #000; }}
  .live-btn {{ color: #f44; border-color: #f44; }}
  .live-btn:hover {{ background: #f44; color: #000; }}
  .ctrl-input {{ background: #111; color: #ccc; border: 1px solid #444; padding: 4px 8px; font-family: 'Courier New', monospace; font-size: 12px; border-radius: 3px; width: 70px; }}
  .ctrl-input:focus {{ border-color: #0af; outline: none; }}
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
        .val-tag { font-size: 0.68em; margin-left: 3px; padding: 1px 3px; border-radius: 3px; vertical-align: middle; }
        .vtick { color: #4c4; background: #1a2e1a; border: 1px solid #2a4a2a; }
        .vapi  { color: #888; background: #1e1e1e; border: 1px solid #333; }
</style>
<script>
var refreshBehaviourNormal = true;

    function toggleOpp(idx) {{
      var d = document.getElementById("opp-detail-" + idx);
      if (d) {{
        d.classList.toggle("show");
        var prev = d.previousElementSibling;
        if (prev) prev.classList.toggle("expanded");
      }}
      refreshBehaviourNormal = true;
      updateRefreshState();
    }}
function togglePos(idx) {{
  var row = document.getElementById('detail-' + idx);
  var main = document.getElementById('pos-' + idx);
  if (row) {{
    row.classList.toggle('show');
    main.classList.toggle('expanded');
  }}
  refreshBehaviourNormal = true;
  updateRefreshState();
}}

function collapseAll() {{
  document.querySelectorAll('.detail-row.show').forEach(function(r) {{ r.classList.remove('show'); }});
  document.querySelectorAll('.pos-row.expanded').forEach(function(r) {{ r.classList.remove('expanded'); }});
  refreshBehaviourNormal = true;
  updateRefreshState();
}}

function toggleSection(el) {{
  el.classList.toggle('collapsed');
  var content = el.nextElementSibling;
  if (content) content.classList.toggle('hidden');
  refreshBehaviourNormal = true;
  updateRefreshState();
}}

function toggleRefresh(btn, evt) {{
  evt.stopPropagation();
  refreshBehaviourNormal = !refreshBehaviourNormal;
  updateRefreshState();
}}

function updateRefreshState() {{
  var detailOpen = document.querySelectorAll('.detail-row.show').length > 0;

  // Positions: show collapse-all when details expanded, else show refresh btn when section open
  var colBtn = document.getElementById('collapse-all-btn');
  var posRefBtn = document.getElementById('pos-refresh-btn');
  var posTitle = document.getElementById('section-positions');
  var posOpen = posTitle && !posTitle.classList.contains('collapsed');
  if (colBtn) colBtn.style.display = detailOpen ? 'inline-block' : 'none';
  if (posRefBtn) {{
    if (posOpen && !detailOpen) {{
      posRefBtn.style.display = 'inline-block';
    }} else {{
      posRefBtn.style.display = 'none';
    }}
  }}

  // All refresh-toggle-btn: show when parent section is expanded
  document.querySelectorAll('.refresh-toggle-btn').forEach(function(btn) {{
    var title = btn.closest('h2');
    var isExpanded = title && !title.classList.contains('collapsed');
    // For positions, already handled above
    if (btn.id === 'pos-refresh-btn') return;
    if (isExpanded) {{
      btn.style.display = 'inline-block';
    }} else {{
      btn.style.display = 'none';
    }}
  }});

  // Update text on ALL visible refresh-toggle-btn
  document.querySelectorAll('.refresh-toggle-btn').forEach(function(btn) {{
    if (btn.style.display !== 'none') {{
      if (refreshBehaviourNormal) {{
        btn.textContent = '\u25B6 Resume Refresh';
      }} else {{
        btn.textContent = '\u23F8 Pause Refresh';
      }}
    }}
  }});

  // Footer indicator
  var ind = document.getElementById('refresh-status');
  if (ind) {{
    var anythingExpanded = document.querySelectorAll('.detail-row.show').length > 0;
    document.querySelectorAll('.section-title').forEach(function(t) {{
      if (!t.classList.contains('collapsed') && t.id !== 'section-positions') anythingExpanded = true;
    }});
    var blocked = (anythingExpanded && refreshBehaviourNormal);
    if (blocked) {{
      ind.textContent = '(paused)';
      ind.classList.add('paused');
    }} else {{
      ind.textContent = '(active)';
      ind.classList.remove('paused');
    }}
  }}
}}

// === Auto-refresh: reload after 60s with no user interaction ===
(function() {{
  var IDLE_MS = 60000;
  var deadline = Date.now() + IDLE_MS;
  function resetIdle() {{ deadline = Date.now() + IDLE_MS; }}
  ['mousemove','keydown','mousedown','touchstart','scroll','click'].forEach(function(ev) {{
    document.addEventListener(ev, resetIdle, {{passive: true}});
  }});
  setInterval(function() {{
    var anythingExpanded = document.querySelectorAll('.detail-row.show').length > 0;
    document.querySelectorAll('.section-title').forEach(function(t) {{
      if (!t.classList.contains('collapsed') && t.id !== 'section-positions') anythingExpanded = true;
    }});
    var controlOpen = document.getElementById('tab-control') && document.getElementById('tab-control').classList.contains('active');
    var blocked = (anythingExpanded && refreshBehaviourNormal) || controlOpen;
    if (!blocked && Date.now() >= deadline) {{
      location.reload();
    }}
  }}, 5000);
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
// Restore tab from URL hash on load
(function() {{
  var hash = window.location.hash.replace('#', '');
  if (hash && document.getElementById('tab-' + hash)) {{
    switchTab(hash);
  }}
}})();

// === Control panel functions ===
function showCtrlStatus(msg, color) {{
  var el = document.getElementById('ctrl-status');
  if (el) {{ el.style.display = 'block'; el.style.color = color || '#0af'; el.textContent = msg; }}
}}
function setMode(mode) {{
  var msg = 'Switch to ' + mode.toUpperCase() + ' mode?';
  if (mode === 'live') msg += ' WARNING: This will use REAL money!';
  if (!confirm(msg)) return;
  fetch('/api/mode', {{method:'POST',headers:{{'Content-Type':'application/json'}},body:JSON.stringify({{mode:mode}})}})
    .then(function(r){{return r.json()}}).then(function(d){{showCtrlStatus(d.message||'Done',d.ok?'#0f0':'#f44')}})
    .catch(function(e){{showCtrlStatus('Error: '+e,'#f44')}});
}}
function ctrlAction(action) {{
  if (action === 'stop') {{
    if (!confirm('EMERGENCY STOP: This will halt all trading immediately. Continue?')) return;
  }}
  fetch('/api/action', {{method:'POST',headers:{{'Content-Type':'application/json'}},body:JSON.stringify({{action:action}})}})
    .then(function(r){{return r.json()}}).then(function(d){{showCtrlStatus(d.message||'Done',d.ok?'#0f0':'#f44')}})
    .catch(function(e){{showCtrlStatus('Error: '+e,'#f44')}});
}}
function saveParams() {{
  var params = {{
    max_positions: parseInt(document.getElementById('param-max-pos').value),
    capital_per_trade: parseInt(document.getElementById('param-cap-trade').value),
    min_profit_pct: parseFloat(document.getElementById('param-min-profit').value) / 100,
    max_price_drift_pct: parseFloat(document.getElementById('param-drift').value) / 100
  }};
  fetch('/api/params', {{method:'POST',headers:{{'Content-Type':'application/json'}},body:JSON.stringify(params)}})
    .then(function(r){{return r.json()}}).then(function(d){{showCtrlStatus(d.message||'Saved',d.ok?'#0f0':'#f44')}})
    .catch(function(e){{showCtrlStatus('Error: '+e,'#f44')}});
}}
</script>
</head><body>

<div class="header">
  <h1>&#x1F4C8; PREDICTION TRADER <span class="mode-badge" style="color:{mode_color};background:{mode_bg};border-color:{mode_color}">{mode_label}</span></h1>
  <div class="meta">
    Started: <span>{first_trade_str}</span><br>
    System Restarted: <span>{start_str} UTC</span><br>
    Starting Capital: <span>${init_cap:.2f}</span>
  </div>
</div>

<div class="stats">
  <div class="stat">
    <div class="label">TOTAL VALUE</div>
    <div class="value {'good' if total_value >= init_cap else 'bad'}">${total_value:.2f}</div>
  </div>
  <div class="stat">
    <div class="label">CASH</div>
    <div class="value">${cap:.2f}</div>
  </div>
  <div class="stat">
    <div class="label">DEPLOYED</div>
    <div class="value">${deployed:.2f}</div>
  </div>
  <div class="stat">
    <div class="label">FEES PAID</div>
    <div class="value">${total_fees:.2f}</div>
  </div>
  {f'<div class="stat"><div class="label">USDC (LIVE)</div><div class="value" style="color:#fa0">${live_balance:.2f}</div></div>' if live_balance is not None else ''}
  <div class="stat">
    <div class="label">RETURN</div>
    <div class="value {'good' if ret_pct >= 0 else 'bad'}">{ret_pct:+.1f}%</div>
  </div>
  <div class="stat">
    <div class="label">TRADES</div>
    <div class="value">{trades}</div>
  </div>
  <div class="stat">
    <div class="label">OPEN</div>
    <div class="value">{len(open_pos)}</div>
  </div>
  <div class="stat">
    <div class="label">REALIZED P&L</div>
    <div class="value {'good' if total_realized >= 0 else 'bad'}">${total_realized:.2f}</div>
  </div>
  <div class="stat">
    <div class="label">ANNUALIZED (closed)</div>
    <div class="value {'good' if annualized_ret >= 0 else 'bad'}">{annualized_str}</div>
  </div>
</div>

<div class="tab-bar">
  <div class="tab-btn active" data-tab="paper" onclick="switchTab('paper')">Paper{paper_ann_badge}</div>
  <div class="tab-btn" data-tab="shadow" onclick="switchTab('shadow')">Shadow{shadow_ann_badge}</div>
  <div class="tab-btn" data-tab="live" onclick="switchTab('live')">Live{live_ann_badge}</div>
  <div class="tab-btn" data-tab="control" onclick="switchTab('control')">Control Panel</div>
</div>

<div id="tab-paper" class="tab-content active">
<h2 class="section-title" id="section-positions" onclick="toggleSection(this)">OPEN POSITIONS ({len(open_pos)})<button id="pos-refresh-btn" class="section-btn refresh-toggle-btn" onclick="toggleRefresh(this, event)" style="display:none"></button><button id="collapse-all-btn" class="section-btn" onclick="event.stopPropagation(); collapseAll()">Collapse All</button></h2>
<div class="section-content">
<table>
<tr><th>#</th><th>Market</th><th>Strategy</th><th>Score</th><th>Deployed</th><th>Expected P&L</th><th>Resolves</th><th>Status</th><th>Entered</th></tr>
{pos_rows_html if pos_rows_html else '<tr><td colspan="8" style="color:#555">No open positions</td></tr>'}
</table>
</div>

<h2 class="section-title collapsed" onclick="toggleSection(this)">AGGREGATE HOLDINGS ({len(agg_markets)} markets)<button class="section-btn refresh-toggle-btn" onclick="toggleRefresh(this, event)" style="display:none"></button></h2>
<div class="section-content hidden">
<table>
<tr><th>Market</th><th>Side</th><th>Deployed</th><th>Avg Price</th><th>Payout (if wins)</th><th>Pos#</th></tr>
{agg_rows_html}
</table>
</div>

<h2 class="section-title collapsed" onclick="toggleSection(this)">OPPORTUNITIES ({len(opps)} found, top 20 by score)<button class="section-btn refresh-toggle-btn" onclick="toggleRefresh(this, event)" style="display:none"></button></h2>
<div class="section-content hidden">
<table>
<tr><th>#</th><th>Profit%</th><th>Resolves</th><th>Strategy</th><th>Score</th><th>Market</th></tr>
{opp_rows_html if opp_rows_html else '<tr><td colspan="7" style="color:#555">No opportunities</td></tr>'}
</table>
</div>

<h2 class="section-title collapsed" onclick="toggleSection(this)">LAYERS<button class="section-btn refresh-toggle-btn" onclick="toggleRefresh(this, event)" style="display:none"></button></h2>
<div class="section-content hidden">
<table>
<tr><th>Layer</th><th>Status</th><th>Info</th></tr>
{layer_html}
</table>
</div>

{'<h2 class="section-title collapsed" onclick="toggleSection(this)">CLOSED POSITIONS<button class="section-btn refresh-toggle-btn" onclick="toggleRefresh(this, event)" style="display:none"></button></h2><div class="section-content hidden">' + closed_html + '</div>' if closed_html else ''}

</div><!-- end tab-paper -->

<div id="tab-shadow" class="tab-content">
{shadow_tab_html_val}
</div>

<div id="tab-live" class="tab-content">
{live_tab_html_val}
</div>

<div id="tab-control" class="tab-content">
{control_tab_html_val}
</div>

<div class="footer">Updated: {now_str} UTC | Auto-refresh: 10s <span id="refresh-status">(active)</span></div>
</body></html>'''
    return html

class DashboardHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == '/status':
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.end_headers()
            state = load_json(EXEC_STATE)
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

    def do_POST(self):
        try:
            length = int(self.headers.get('Content-Length', 0))
            body = json.loads(self.rfile.read(length)) if length else {}
        except:
            return self._json_response({'ok': False, 'message': 'Invalid JSON'}, 400)

        if self.path == '/api/mode':
            return self._handle_mode(body)
        elif self.path == '/api/action':
            return self._handle_action(body)
        elif self.path == '/api/params':
            return self._handle_params(body)
        else:
            return self._json_response({'ok': False, 'message': 'Unknown endpoint'}, 404)

    def _handle_mode(self, body):
        import yaml, subprocess
        mode = body.get('mode', '')
        cfg = load_config()
        if mode == 'paper':
            cfg['mode'] = 'paper_trading'
            cfg.setdefault('live_trading', {})['enabled'] = False
            cfg['live_trading']['shadow_only'] = False
        elif mode == 'shadow':
            cfg['mode'] = 'dual'
            cfg.setdefault('live_trading', {})['enabled'] = True
            cfg['live_trading']['shadow_only'] = True
        elif mode == 'live':
            cfg['mode'] = 'dual'
            cfg.setdefault('live_trading', {})['enabled'] = True
            cfg['live_trading']['shadow_only'] = False
        else:
            return self._json_response({'ok': False, 'message': f'Unknown mode: {mode}'})
        with open(CONFIG_PATH, 'w') as f:
            yaml.dump(cfg, f, default_flow_style=False)
        # Restart L4 to pick up new config
        subprocess.Popen(['pkill', '-f', 'layer4_runner'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return self._json_response({'ok': True, 'message': f'Switched to {mode.upper()}. L4 restarting...'})

    def _handle_action(self, body):
        import subprocess
        action = body.get('action', '')
        if action == 'stop':
            subprocess.Popen(['pkill', '-f', 'main.py'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            subprocess.Popen(['pkill', '-f', 'layer'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            return self._json_response({'ok': True, 'message': 'EMERGENCY STOP executed. All processes killed.'})
        elif action == 'restart':
            subprocess.Popen(['pkill', '-f', 'layer'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            return self._json_response({'ok': True, 'message': 'All layers killed. Supervisor will restart them.'})
        elif action == 'restart_l4':
            subprocess.Popen(['pkill', '-f', 'layer4_runner'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            return self._json_response({'ok': True, 'message': 'L4 killed. Supervisor will restart it.'})
        else:
            return self._json_response({'ok': False, 'message': f'Unknown action: {action}'})

    def _handle_params(self, body):
        import yaml
        cfg = load_config()
        if 'max_positions' in body:
            cfg['max_open_positions'] = int(body['max_positions'])
        if 'capital_per_trade' in body:
            cfg.setdefault('live_trading', {})['capital_per_trade'] = int(body['capital_per_trade'])
        if 'min_profit_pct' in body:
            cfg.setdefault('fees', {})['min_profit_threshold'] = float(body['min_profit_pct'])
        if 'max_price_drift_pct' in body:
            cfg.setdefault('live_trading', {})['max_price_drift_pct'] = float(body['max_price_drift_pct'])
        with open(CONFIG_PATH, 'w') as f:
            yaml.dump(cfg, f, default_flow_style=False)
        return self._json_response({'ok': True, 'message': 'Parameters saved. Restart L4 to apply.'})

if __name__ == '__main__':
    port = 5556
    print(f'Dashboard starting on http://localhost:{port}')
    HTTPServer(('127.0.0.1', port), DashboardHandler).serve_forever()
