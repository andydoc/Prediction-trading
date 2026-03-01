# Prediction Trader - Progress & Roadmap

## System Architecture
- **Location**: `/home/andydoc/prediction-trader/` (WSL, local)
- **Python env**: `/home/andydoc/prediction-trader-env/`
- **Supervisor**: `main.py` → starts L1-L4 + dashboard as subprocesses
- **Quick start**: `cd /home/andydoc/prediction-trader && nohup python main.py > logs/main.log 2>&1 &`
- **Dashboard**: http://localhost:5556 (standalone `dashboard_server.py`, managed by supervisor)

## Layer Structure
| Layer | Runner | Purpose | Status |
|-------|--------|---------|--------|
| L1 | layer1_runner.py | Pulls ~10K markets from Polymarket API → `data/latest_markets.json` | ✅ Working |
| L2 | layer2_runner.py | Detects constraints via negRiskMarketID ONLY → `layer2_.../data/latest_constraints.json` | ✅ Working |
| L3 | layer3_runner.py | Arbitrage math → `layer3_.../data/latest_opportunities.json` (~88 opps/scan, ~11s) | ✅ Working |
| L4 | layer4_runner.py | Paper + Live trading: dynamic capital, resolution ranking, position replacement | ✅ Working |
| Dash | dashboard_server.py | Real-time dashboard on port 5556 | ✅ Working |
| Live | live_trading_engine.py | Polymarket CLOB API: orders, fills, liquidation, shadow mode | ✅ Ready |

## Key Files
- `config/config.yaml` — main config (fees, thresholds, capital, max positions, live_trading section)
- `config/secrets.yaml` — Polymarket private key, funder address, API host (gitignored)
- `live_trading_engine.py` — CLOB client wrapper (825 lines): auth, balance, tokens, orderbook, orders, fills, liquidation, shadow
- `layer3_arbitrage_math/arbitrage_engine.py` — arb math (mutex direct + polytope)
- `paper_trading_complete.py` — position management, state persistence, liquidation
- `dashboard_server.py` — standalone dashboard on port 5556 with mode badge
- `data/system_state/execution_state.json` — live trading state (positions, capital)

## Live Trading Architecture
### CLOB Integration (live_trading_engine.py)
- **Auth**: Email/Magic signature (type=1), API key derived from private key
- **Token resolution**: L1 metadata `clobTokenIds` → YES/NO token IDs (91% success rate)
- **Fallback**: Gamma API slug lookup for missing tokens
- **Balance**: `get_balance_allowance(COLLATERAL)` → raw/1e6 = USDC
- **Prices**: `get_midpoint(token_id)` for live executable prices
- **Orderbook**: Depth check before placing orders (min $50 liquidity)
- **Fees**: Dynamic `get_fee_rate()` per token (0% most markets, up to 3% crypto)
- **Orders**: GTC limit orders at midpoint, batch placement for multi-leg arbs
- **Fill monitoring**: Poll every 2s for 60s, handle partial fills
- **Shadow mode**: Full validation without placing orders (skip balance check)

### L4 Integration
- L4 imports LiveTradingEngine based on config `mode` and `live_trading.enabled`
- **Paper mode**: Existing paper trading only
- **Shadow mode**: Paper trades + shadow validation logged
- **Dual mode**: Paper trades + real CLOB orders in parallel
- **Live mode**: Real CLOB orders only (not yet implemented, dual recommended)
- Live metadata stored in position for later liquidation tracking
- Replacement liquidation also executes live CLOB sell if position has live metadata

### Validation Pipeline (6 checks)
1. Token IDs resolvable for all markets
2. Live midpoint prices available
3. Price drift < 5% from L3 calculation
4. Orderbook depth > $50 within 2% of midpoint
5. Net profit > 3% after estimated fees
6. USDC balance sufficient (skipped in shadow mode)

## Config Summary (config/config.yaml)
- **mode**: `dual` (paper + live), `paper_trading`, `live_trading`
- **live_trading.enabled**: true/false
- **live_trading.shadow_only**: true = validate but don't place orders
- Fee model: ~1bp per leg (polymarket_taker_fee: 0.0001)
- Max profit cap: 30%, Min threshold: 3%
- Capital per trade: `max(10, min(balance*0.1, 1000))` — dynamic
- Starting capital: $100 (paper), max 20 concurrent positions
- Position replacement: 20% improvement threshold, skip positions <24h to resolve

## Current State (2026-02-24 17:00 UTC)
- **Mode**: SHADOW (validates live but doesn't place orders)
- **Paper capital**: $6.40 cash, ~$110 deployed, ~$116 total value
- **USDC balance**: $1.65 (insufficient for live — waiting for deposit)
- **Open positions**: 11 (all slots full)
- **Closed positions**: 441+ (21+ resolved, rest replaced at breakeven)
- **CLOB API**: ✅ Authenticated, healthy, token resolution working
- **Dashboard**: Tabbed UI (Paper/Shadow/Live/Control Panel), SHADOW badge, score column, descriptive layer names

## Go-Live Procedure
1. Deposit $100+ USDC to funder address on Polygon
2. Run: `wsl bash /mnt/c/Users/andyd/ai-workspace/GO_LIVE.sh`
3. Script checks balance, health, confirms, updates config, restarts
4. Monitor: http://localhost:5556 (badge changes to DUAL)
5. Emergency stop: `wsl bash /mnt/c/Users/andyd/ai-workspace/STOP_LIVE.sh`

## Features Implemented
1. ✅ **4-layer architecture**: market data → constraints → arb math → paper trading
2. ✅ **Direct mutex check**: O(N) buy-all/sell-all before polytope
3. ✅ **Memory-safe polytope**: N≤12 guard, per-constraint 5s timeout
4. ✅ **Dynamic capital**: `max(10, min(balance*0.1, 1000))`
5. ✅ **Resolution ranking**: opportunities scored by `profit_pct / hours_to_resolution`
6. ✅ **Position replacement**: worst open vs best available, 20%+ improvement threshold
7. ✅ **Liquidation engine**: paper + live CLOB liquidation
8. ✅ **Dashboard**: capital, return, positions, opps, layers, tooltips, mode badge
9. ✅ **Expandable positions**: click for leg-by-leg breakdown + scenario analysis
10. ✅ **Expandable opportunities**: detail rows with legs, scenarios, guaranteed payout
11. ✅ **Closed position subcategories**: Resolved/Profit/Loss/Breakeven
12. ✅ **Duplicate opportunity greying**: [HELD] label for already-held opps
13. ✅ **Auto-start on boot**: VBS in Windows Startup folder
14. ✅ **Live trading engine**: 825-line CLOB wrapper with full order lifecycle
15. ✅ **Shadow mode**: Validates opportunities against live orderbooks without trading
16. ✅ **L4 live integration**: Shadow/dual/live paths, live liquidation on replacements
17. ✅ **Dashboard mode badge**: PAPER/SHADOW/LIVE/DUAL with color coding
18. ✅ **Dashboard USDC balance**: Shows live balance when in dual/live mode
19. ✅ **Fee correction**: 0.0001 taker fee (was 0.02)
20. ✅ **Annualized return fix**: Uses total hold time
21. ✅ **Replacement bug fix**: Normalized position scores to %/hr (was $/hr, caused 10x inflation)
22. ✅ **Shadow balance fix**: shadow_trade() skips balance check (was rejecting all with $1.65)
23. ✅ **Dashboard tabs**: Paper/Shadow/Live/Control Panel tabbed interface
24. ✅ **Score column**: Replaced #Mkts with score (profit_pct/hours * 10000) in positions + opportunities
25. ✅ **Descriptive layer names**: "1 Market Data", "2 Constraint Detection", etc.
26. ✅ **Shadow tab**: Shows would-trade signals, rejection breakdown, recent shadow trades
27. ✅ **Control panel tab**: Mode buttons, parameter inputs (max positions, capital per trade, min profit, drift)
28. ✅ **Badge fix**: Shows SHADOW when live_trading.enabled=true + shadow_only=true (was showing LIVE)

## TODO / Roadmap
### Done
- [x] All paper trading features (L1-L4, dashboard, replacement, liquidation)
- [x] CLOB auth & connectivity (secrets.yaml, ClobClient, API key derivation)
- [x] Token resolution (L1 metadata clobTokenIds + Gamma fallback)
- [x] Live price feeds (midpoints, orderbook depth)
- [x] Shadow mode (validates without placing orders)
- [x] L4 integration (shadow/dual/live paths)
- [x] Dashboard mode indicator (PAPER/SHADOW/LIVE/DUAL badge)
- [x] Dashboard USDC balance display
- [x] Go-live script (GO_LIVE.sh with balance/health checks)
- [x] Emergency stop script (STOP_LIVE.sh)

### Pending (waiting for USDC deposit)
- [ ] **First live trade**: Deposit $100 USDC, run GO_LIVE.sh
- [ ] **Live P&L tracking**: Dashboard shows live fills, actual fees, real P&L
- [ ] **Position reconciliation**: Verify CLOB positions match paper positions

### Future
- [ ] **Wire control panel**: Mode switching, parameter save, restart/stop via dashboard API endpoints
- [ ] **Fix supervisor double-instance**: L4 sometimes spawns twice (rare)
- [ ] **Ollama resolution estimation**: Use LLM for unknown resolve dates
- [ ] **VPS migration**: For 24/7 operation
- [ ] **WA notifications**: Alert on new trades, position resolutions, errors
- [ ] **Scale up**: Increase capital/positions after live validation

## How to Resume After Crash / PC Reboot
### Automatic (on login)
- **Auto-start is enabled** via Windows Startup folder (`START_TRADER_HIDDEN.vbs`)
- System starts silently ~15s after login
- Open http://localhost:5556 to verify

### Manual (if auto-start fails)
1. Double-click **`C:\Users\andyd\ai-workspace\START_TRADER.bat`** — OR:
2. WSL: `wsl bash -c "source /home/andydoc/prediction-trader-env/bin/activate && cd /home/andydoc/prediction-trader && rm -f *.pid && nohup python main.py > logs/main.log 2>&1 &"`
3. Wait ~15s, open http://localhost:5556

### Key notes
- **CRITICAL**: venv must be activated — bare `python` won't work
- All state persists in `data/system_state/execution_state.json`
- **Check status**: `wsl bash /mnt/c/Users/andyd/ai-workspace/status.sh`

### Control Scripts (all in `C:\Users\andyd\ai-workspace\`)
| Script | Purpose |
|--------|---------|
| `START_TRADER.bat` | Manual start (with console) |
| `START_TRADER_SILENT.bat` | Silent start |
| `START_TRADER_HIDDEN.vbs` | Boot auto-start (in Windows Startup) |
| `GO_LIVE.sh` | Activate live trading (checks balance, health, confirms) |
| `STOP_LIVE.sh` | Emergency revert to paper-only |
| `status.sh` | Quick status check |
| `restart_dash.sh` | Restart dashboard only |

## Useful Commands
```bash
# Start system
wsl bash -c "cd /home/andydoc/prediction-trader && rm -f *.pid && nohup python main.py > logs/main.log 2>&1 &"

# L4 log tail
wsl tail -30 /home/andydoc/prediction-trader/logs/layer4_$(date +%Y%m%d).log

# Check state
wsl python3 -c "import json; d=json.load(open('/home/andydoc/prediction-trader/data/system_state/execution_state.json')); print(f'Cap=\${d[\"current_capital\"]:.2f} Open={len(d[\"open_positions\"])} Closed={len(d[\"closed_positions\"])}')"

# Check USDC balance
wsl bash -c "source /home/andydoc/prediction-trader-env/bin/activate && cd /home/andydoc/prediction-trader && python3 -c \"
import yaml
from py_clob_client.client import ClobClient
from py_clob_client.clob_types import BalanceAllowanceParams, AssetType
with open('config/secrets.yaml') as f: s=yaml.safe_load(f)['polymarket']
c=ClobClient(s['host'],key=s['private_key'],chain_id=s['chain_id'],signature_type=s['signature_type'],funder=s['funder_address'])
cr=c.create_or_derive_api_creds();c.set_api_creds(cr)
b=c.get_balance_allowance(BalanceAllowanceParams(asset_type=AssetType.COLLATERAL))
print(f'USDC: \${float(b[\\\"balance\\\"])/1e6:.2f}')
\""

# Kill everything
wsl bash -c "kill \$(ps aux|grep 'main.py\|layer[1-4]_runner\|dashboard_server'|grep -v grep|awk '{print \$2}')"

# Dashboard
http://localhost:5556
```

---
*Last updated: 2026-02-24 17:15 UTC*
*Mode: SHADOW | 11 positions open, 441+ closed | USDC: $1.65 (waiting for deposit)*
*Dashboard: Tabbed (Paper/Shadow/Live/Control), Score column, Descriptive layers, SHADOW badge*
*Session 7: Badge fix (SHADOW not LIVE), text fixes (cash=, Starting Capital), score column, descriptive layer names, tab structure added (2a shell done, 2b content builders in progress)*
*Live trading engine ready — run GO_LIVE.sh after depositing $100+ USDC*