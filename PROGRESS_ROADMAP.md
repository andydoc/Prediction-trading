# Prediction Market Arbitrage Trading System — Progress & Roadmap

## Overview
Automated system that detects and exploits pricing inefficiencies across Polymarket prediction markets using mathematical arbitrage (marginal polytope construction, Bregman projection, Frank-Wolfe optimization). Runs on WSL (Ubuntu) with a multi-process supervisor architecture.

## Architecture

### Four-Layer Design
| Layer | File | Frequency | Purpose |
|-------|------|-----------|---------|
| L1 Market Data | `layer1_runner.py` → `layer1_market_data/market_data.py` | Every 30s | Collects market prices from Polymarket API |
| L2 Constraints | `layer2_runner.py` → `layer2_constraint_detection/constraint_detector.py` | Every 5min | Detects logical relationships (mutex groups) between markets |
| L3 Arbitrage | `layer3_runner.py` → `layer3_arbitrage_math/arbitrage_engine.py` | Continuous | Scans for pricing inconsistencies, calculates optimal bets via CVXPY LP |
| L4 Execution | `layer4_runner.py` → `paper_trading.py` + `live_trading.py` | Continuous | Executes trades (paper/shadow/live), monitors positions, handles resolution |

### Supervisor
- `main.py` — Starts all 4 layers + dashboard + exec_ctrl as subprocesses, monitors health, restarts failed components
- `dashboard_server.py` — Flask web UI on port 5556 with tabs (Paper/Shadow/Live/Control)
- `execution_control.py` — Flask API on port 5557 for multi-machine leader election

### Multi-Machine Architecture
Only ONE machine may run L4 execution at a time. The execution control server (Flask, port 5557) manages this:
- **Endpoints**: `GET /status`, `POST /claim`, `POST /release`, `POST /heartbeat`, `GET /health`
- **L4 integration**: `layer4_runner.py` imports `ExecutionLock` from `execution_control_client.py`, checks lock before every trade cycle
- **Fail-open**: If control server is unreachable, L4 allows execution (avoids blocking if server is down)
- **TTL**: Leader must heartbeat every 300s or lock auto-expires — prevents stuck locks if a machine crashes
- **Config**: `execution_control.enabled` and `execution_control.url` in config.yaml
- **Remote claim**: From laptop: `curl -X POST http://desktop-ip:5557/claim -d '{"machine":"laptop"}'`

### Trading Modes
| Mode | Config | Behaviour |
|------|--------|-----------|
| PAPER | `live_trading.enabled: false` | Paper trades only, no CLOB interaction |
| SHADOW | `live_trading.enabled: true, shadow_only: true` | Paper trades + validates against live orderbook |
| LIVE | `live_trading.enabled: true, shadow_only: false` | Real money trades via Polymarket CLOB |

## File Structure (WSL — authoritative source)
```
/home/andydoc/prediction-trader/          ← Running code (WSL)
├── main.py                               ← Supervisor (starts L1-L4)
├── dashboard_server.py                   ← Web dashboard (port 5556)
├── execution_control.py                  ← Multi-machine lock server (port 5557)
├── execution_control_client.py           ← Lock client (imported by L4)
├── paper_trading.py                      ← Paper/shadow trading engine
├── live_trading.py                       ← Live CLOB trading engine
├── layer1_runner.py                      ← L1 process entry point
├── layer2_runner.py                      ← L2 process entry point
├── layer3_runner.py                      ← L3 process entry point
├── layer4_runner.py                      ← L4 process entry point
├── layer1_market_data/
│   └── market_data.py                    ← MarketDataManager, PolymarketCollector
├── layer2_constraint_detection/
│   └── constraint_detector.py            ← ConstraintDetector (mutex groups)
├── layer3_arbitrage_math/
│   └── arbitrage_engine.py               ← ArbitrageMathEngine, MarginalPolytope
├── config/
│   ├── config.yaml                       ← Trading parameters (in git)
│   └── secrets.yaml                      ← Polymarket API keys (NOT in git)
├── data/system_state/
│   ├── execution_state.json              ← All positions, capital, trade history
│   └── execution_lock.json               ← Multi-machine leader lock
├── logs/
│   ├── layer1_YYYYMMDD.log
│   ├── layer2_YYYYMMDD.log
│   ├── layer3_YYYYMMDD.log
│   └── layer4_YYYYMMDD.log
├── test_clob_connect.py                  ← CLOB connectivity test
├── requirements.txt
├── PROGRESS_ROADMAP.md                   ← This file
└── HEARTBEAT.md                          ← Agent instruction file

/home/andydoc/prediction-trader-env/      ← Python venv (all dependencies)

C:\Users\andyd\ai-workspace\prediction-trader\  ← Git repo mirror
├── scripts/
│   ├── restart.sh [--clean]              ← Kill all + restart (--clean purges stale L2/L3)
│   ├── stop.sh [--dash|--l4]            ← Kill processes (all/dashboard/L4)
│   ├── mode.sh paper|shadow|live|stop    ← Switch trading mode
│   ├── status.sh [--full]                ← Quick status or full health check
│   ├── accounting.py                     ← Capital breakdown
│   ├── reset.py [--soft|--hard]          ← Soft: return deployed. Hard: wipe to $100
│   ├── START_TRADER.bat                  ← Manual Windows start (with console)
│   ├── START_TRADER_SILENT.bat           ← Silent start for Task Scheduler
│   └── START_TRADER_HIDDEN.vbs           ← Boot auto-start (Windows Startup folder)
└── (all .py files mirrored from WSL)
```

## Setup from Scratch

### Prerequisites
- Windows PC with WSL2 (Ubuntu) installed
- Python 3.10+ in WSL
- Polymarket account with API credentials

### Step 1: Create project structure
```bash
# In WSL
mkdir -p ~/prediction-trader/{config,data/system_state,logs}
mkdir -p ~/prediction-trader/{layer1_market_data,layer2_constraint_detection,layer3_arbitrage_math}
```

### Step 2: Python virtual environment
```bash
python3 -m venv ~/prediction-trader-env
source ~/prediction-trader-env/bin/activate
pip install pyyaml aiohttp requests numpy scipy cvxpy flask py-clob-client
```

### Step 3: Configure secrets
```bash
cat > ~/prediction-trader/config/secrets.yaml << 'EOF'
polymarket:
  host: "https://clob.polymarket.com"
  chain_id: 137
  private_key: "0xYOUR_PRIVATE_KEY"
  funder_address: "0xYOUR_FUNDER_ADDRESS"
  signature_type: 0
EOF
chmod 600 ~/prediction-trader/config/secrets.yaml
```

### Step 4: Deploy code
Copy all `.py` files to `~/prediction-trader/` (from git repo or previous backup).

### Step 5: Test CLOB connectivity
```bash
source ~/prediction-trader-env/bin/activate
cd ~/prediction-trader
python test_clob_connect.py
```

### Step 6: Start the system
```bash
source ~/prediction-trader-env/bin/activate
cd ~/prediction-trader
rm -f *.pid
nohup python main.py > logs/main.log 2>&1 &
```

### Step 7: Verify
- Open http://localhost:5556 (dashboard)
- Check: `tail -20 logs/layer4_$(date +%Y%m%d).log`
- All 4 layers should show "running" within ~60 seconds

### Step 8: Windows auto-start (optional)
Place `START_TRADER_HIDDEN.vbs` in Windows Startup folder:
`C:\Users\andyd\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup\`

## Go-Live Procedure
1. Deposit $100+ USDC to your Polymarket wallet
2. Verify balance: `test_clob_connect.py` or dashboard
3. Switch mode: `scripts/mode.sh live` (runs pre-flight checks + confirmation)
4. Monitor: dashboard Live tab, L4 logs
5. Emergency stop: `scripts/mode.sh stop` (reverts to paper + cancels all CLOB orders)

## Completed Features
1. ✅ **L1 market data**: Polymarket API with pagination, 30s refresh
2. ✅ **L2 constraint detection**: Mutex group identification from neg-risk markets
3. ✅ **L3 arbitrage engine**: CVXPY LP for marginal polytope, buy+sell strategies
4. ✅ **L4 paper trading**: Position lifecycle (open→monitor→replace/resolve→close)
5. ✅ **Dashboard**: Flask web UI with Paper/Shadow/Live/Control tabs
6. ✅ **Multi-process supervisor**: main.py manages L1-L4 with health monitoring
7. ✅ **Market-based dedup**: Prevents re-trading same market groups
8. ✅ **Position replacement**: Better opportunities replace weaker ones (scored by %/hr)
9. ✅ **Profit caps & price guards**: Prevent phantom arbitrage from bad data
10. ✅ **Resolution detection**: Monitors markets for expiry, calculates actual P&L
11. ✅ **CLOB integration**: py-clob-client auth, token resolution, orderbook depth
12. ✅ **Shadow mode**: Validates trades against live orderbook without placing orders
13. ✅ **Live trading engine**: Multi-leg order placement, fill monitoring, liquidation
14. ✅ **Dynamic capital**: Adjusts trade size based on available balance
15. ✅ **Dashboard tabs**: Paper/Shadow/Live/Control Panel with mode badge
16. ✅ **L4 live integration**: Shadow/dual/live paths, live liquidation on replacements
17. ✅ **Dashboard mode badge**: PAPER/SHADOW/LIVE/DUAL with color coding
18. ✅ **Dashboard USDC balance**: Shows live balance when in dual/live mode
19. ✅ **Fee correction**: 0.0001 taker fee (was 0.02)
20. ✅ **Annualized return fix**: Uses total hold time
21. ✅ **Replacement bug fix**: Normalized position scores to %/hr (was $/hr)
22. ✅ **Shadow balance fix**: shadow_trade() skips balance check
23. ✅ **Dashboard tabs**: Paper/Shadow/Live/Control Panel tabbed interface
24. ✅ **Score column**: profit_pct/hours * 10000 in positions + opportunities
25. ✅ **Descriptive layer names**: "1 Market Data", "2 Constraint Detection", etc.
26. ✅ **Shadow tab**: Would-trade signals, rejection breakdown, recent shadow trades
27. ✅ **Control panel tab**: Mode buttons, parameter inputs
28. ✅ **Badge fix**: Shows SHADOW when live_trading.enabled=true + shadow_only=true
29. ✅ **Auto-refresh logic**: refreshBehaviourNormal toggle, pauses when content expanded
30. ✅ **Script rationalisation**: 21 scripts → 8 (merged overlapping scripts)
31. ✅ **Git repo**: https://github.com/andydoc/Prediction-trading
32. ✅ **Naming standardisation**: DashboardHandler, _render_closed_subsection, opportunity_overlaps_held, live_trading.py, deleted dead layer4_execution/
33. ✅ **Merged SETUP_INSTRUCTIONS into PROGRESS_ROADMAP** (single source of truth)

## TODO / Roadmap
### Pending (waiting for USDC deposit)
- [ ] **First live trade**: Deposit $100 USDC, run `mode.sh live`
- [ ] **Live P&L tracking**: Dashboard shows live fills, actual fees, real P&L
- [ ] **Position reconciliation**: Verify CLOB positions match paper positions

### Future
- [ ] **Wire control panel**: Mode switching, parameter save, restart/stop via dashboard API
- [ ] **Fix supervisor double-instance**: L4 sometimes spawns twice (rare)
- [ ] **Ollama resolution estimation**: Use LLM for unknown resolve dates
- [ ] **VPS migration**: For 24/7 operation
- [ ] **WA notifications**: Alert on new trades, position resolutions, errors
- [ ] **Scale up**: Increase capital/positions after live validation
- [ ] **Trade size optimisation**: Increase from 10% to 30-50% per trade (backtested)

## How to Resume After Crash / PC Reboot
### Automatic (on login)
- **Auto-start is enabled** via Windows Startup folder (`START_TRADER_HIDDEN.vbs`)
- System starts silently ~15s after login
- Open http://localhost:5556 to verify

### Manual (if auto-start fails)
1. Double-click **`C:\Users\andyd\ai-workspace\prediction-trader\scripts\START_TRADER.bat`** — OR:
2. WSL: `wsl bash -c "source /home/andydoc/prediction-trader-env/bin/activate && cd /home/andydoc/prediction-trader && rm -f *.pid && nohup python main.py > logs/main.log 2>&1 &"`
3. Wait ~15s, open http://localhost:5556

### Key notes
- **CRITICAL**: venv must be activated — bare `python` won't work
- All state persists in `data/system_state/execution_state.json`
- Check status: `scripts/status.sh` or `scripts/status.sh --full`

## Useful Commands
```bash
# === SCRIPTS (from Windows, all in prediction-trader/scripts/) ===
S="prediction-trader/scripts"
wsl bash /mnt/c/Users/andyd/ai-workspace/$S/restart.sh          # Restart system
wsl bash /mnt/c/Users/andyd/ai-workspace/$S/restart.sh --clean   # Restart + purge stale data
wsl bash /mnt/c/Users/andyd/ai-workspace/$S/stop.sh              # Kill all
wsl bash /mnt/c/Users/andyd/ai-workspace/$S/stop.sh --dash       # Kill dashboard only
wsl bash /mnt/c/Users/andyd/ai-workspace/$S/stop.sh --l4         # Kill L4 only
wsl bash /mnt/c/Users/andyd/ai-workspace/$S/mode.sh shadow       # Switch to shadow
wsl bash /mnt/c/Users/andyd/ai-workspace/$S/mode.sh live         # Go live (with checks)
wsl bash /mnt/c/Users/andyd/ai-workspace/$S/mode.sh paper        # Back to paper
wsl bash /mnt/c/Users/andyd/ai-workspace/$S/mode.sh stop         # Emergency stop + cancel orders
wsl bash /mnt/c/Users/andyd/ai-workspace/$S/status.sh            # Quick status
wsl bash /mnt/c/Users/andyd/ai-workspace/$S/status.sh --full     # Full health + P&L

# === MANUAL ===
# Start system
wsl bash -c "cd /home/andydoc/prediction-trader && source ~/prediction-trader-env/bin/activate && rm -f *.pid && nohup python main.py > logs/main.log 2>&1 &"

# L4 log tail
wsl tail -30 /home/andydoc/prediction-trader/logs/layer4_$(date +%Y%m%d).log

# Check state
wsl python3 -c "import json; d=json.load(open('/home/andydoc/prediction-trader/data/system_state/execution_state.json')); print(f'Cap=\${d[\"current_capital\"]:.2f} Open={len(d[\"open_positions\"])} Closed={len(d[\"closed_positions\"])}')"

# Kill everything
wsl bash -c "kill \$(ps aux|grep 'main.py\|layer[1-4]_runner\|dashboard_server'|grep -v grep|awk '{print \$2}')"

# Dashboard
http://localhost:5556

# === EXECUTION CONTROL ===
wsl bash /home/andydoc/prediction-trader/scripts/exec_claim.sh status
wsl bash /home/andydoc/prediction-trader/scripts/exec_claim.sh claim laptop
wsl bash /home/andydoc/prediction-trader/scripts/exec_claim.sh release --force
# Remote (from laptop hitting desktop/oracle):
EXEC_CTRL_URL=http://<server-ip>:5557 ./scripts/exec_claim.sh claim laptop

# === GIT SYNC (all repos) ===
# From desktop (WSL → push → pull Windows clone):
wsl bash /home/andydoc/prediction-trader/scripts/sync.sh "commit message"
# From laptop (WSL):
cd /home/andydoc/prediction-trader && git add -A && git commit -m "msg" && git push
# From laptop (Windows PowerShell):
cd C:\Users\andyd\ai-workspace\prediction-trader; git pull origin main

# === EXPORT STATE (for migration / new machine setup) ===
# Copy secrets + state to Windows Desktop for transfer:
cp /home/andydoc/prediction-trader/config/secrets.yaml /mnt/c/Users/andyd/Desktop/
cp /home/andydoc/prediction-trader/data/system_state/execution_state.json /mnt/c/Users/andyd/Desktop/
# On target machine, place them back:
cp secrets.yaml /home/andydoc/prediction-trader/config/
cp execution_state.json /home/andydoc/prediction-trader/data/system_state/
```

## Git Repository
- **Remote**: https://github.com/andydoc/Prediction-trading
- **Local (laptop)**: `C:\Users\andyd\ai-workspace\prediction-trader\`
- **Local (desktop)**: `\\wsl$\Ubuntu\home\andydoc\prediction-trader\` (WSL)
- **Branch**: `main`
- **Excludes** (via .gitignore): secrets.yaml, data/, logs/, __pycache__, *.pid, *.zip, *.tar.gz, .env
- **Sync workflow**: Edit on either machine → `git push` → other machine does `git pull`

## TODO / Roadmap
### Immediate
- [ ] **API key auth for execution_control.py** — Add `X-API-Key` header check before deploying to public IP. ~10 lines, update client + exec_claim.sh to send key.
- [ ] **Oracle Cloud setup script** — Provision Always Free AMD instance, install Python/Flask, deploy execution_control.py, open port 5557. Write `scripts/oracle_setup.sh` for one-command deploy.
- [ ] **Desktop: copy secrets.yaml + execution_state.json** — Files exported to `C:\Users\andyd\Desktop\`, email to `andydoc1@gmail.com` or transfer via USB. Place into desktop WSL at:
  ```
  /home/andydoc/prediction-trader/config/secrets.yaml
  /home/andydoc/prediction-trader/data/system_state/execution_state.json
  ```
- [ ] **Deposit USDC** — Fund Polymarket wallet ($100+ minimum) to go live

### Short-term
- [ ] Migrate execution control server from desktop to Oracle Cloud instance
- [ ] Update `execution_control.url` in config.yaml on all machines to Oracle public IP
- [ ] Set up WhatsApp (OpenClaw) control integration
- [ ] Increase capital_per_trade once live trading is verified

### Medium-term
- [ ] VPS migration — move full trading system to always-on cloud server
- [ ] Historical performance analytics dashboard
- [ ] Multi-exchange support (Kalshi, etc.)

## Performance (as of 2026-03-02)
- **Initial capital**: $100.00
- **Resolved trades**: 65 (100% win rate)
- **Total resolved profit**: $47.35
- **Avg return per trade**: 7.3% (range 3.0%–23.9%)
- **Avg hold time**: 27 hours
- **Replacement churn**: 927 swaps at ~$0.001 each ($0.92 total cost)
- **Backtested optimal**: 30-50% cash per trade would have returned 57-71% vs actual 30%

---
*Last updated: 2026-03-02 11:00 UTC*
*Mode: SHADOW | 14 positions open, 989 closed (65 resolved, 100% win rate, $47.35 profit)*
*Dashboard: port 5556 | Execution Control: port 5557*
*Multi-machine: Leader election via execution_control.py (L4 checks lock before trading)*
*Machines: Laptop (andyd/andydoc) + Desktop HP-800G2 (Andrew Thompson/andydoc)*
*Repos synced: Laptop WSL + Laptop Win + Desktop WSL + Desktop Win (all @ main)*
*Scripts: 9 (+ exec_claim.sh, sync.sh) | Naming standardised | Single doc*
*Git: https://github.com/andydoc/Prediction-trading (14 commits)*
*Next: API key auth → Oracle Cloud deploy → go live*