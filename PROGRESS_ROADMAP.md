# Prediction Market Arbitrage System
# User Guide · Architecture · Roadmap · Progress

> **Version**: v0.04.07  
> **Last updated**: 2026-03-11 ~10:30 UTC  
> **Mode**: SHADOW  
> **Laptop**: running (authoritative development machine)  
> **VPS**: ZAP-Hosting Lifetime (193.23.127.99) — 4 cores, 4 GB RAM, Ubuntu 24.04, systemd auto-restart, $100 fresh capital  
> **Git**: https://github.com/andydoc/Prediction-trading (branch: `main`)

---

## Contents

1. [What This System Does](#1-what-this-system-does)
2. [Architecture](#2-architecture)
3. [File Structure](#3-file-structure)
4. [Setup From Scratch](#4-setup-from-scratch)
5. [Operating the System](#5-operating-the-system)
6. [Roadmap](#6-roadmap)
7. [Version History](#7-version-history)
8. [Incident Log](#8-incident-log)
9. [Performance](#9-performance)
10. [Configuration Reference](#10-configuration-reference)
11. [Glossary](#11-glossary)
12. [Git Versioning Strategy](#12-git-versioning-strategy)

---

## 1. What This System Does

Automated detection and exploitation of pricing inefficiencies across Polymarket prediction markets. The system uses mathematical arbitrage — marginal polytope construction, Bregman projection with KL divergence, and Frank-Wolfe optimisation — to find groups of logically related markets where prices are mutually inconsistent, then bets across all outcomes to lock in a guaranteed profit regardless of which outcome occurs.

**This is not prediction. Not gambling. It is pure pricing-error exploitation.**

---

## 2. Architecture

### 2.1 Current Architecture (v0.04.00+, event-driven)

Two persistent processes replace the old four-layer polling pipeline:

| Process | Script | Purpose |
|---------|--------|---------|
| **Market Scanner** | `layer1_runner.py` | Fetches all active markets from Polymarket Gamma API (33 k+); writes `latest_markets.json`. Runs at startup and on a periodic refresh schedule. |
| **Trading Engine** | `trading_engine.py` | Event-driven core: constraint detection, arb math, execution, and position management. Reacts to WebSocket price events in real time. |
| **Dashboard** | `dashboard_server.py` | Web UI on port 5556. Live updates via SSE (no page reload required). |


**Data flow (Trading Engine):**
```
WS price_change / book event
  → _update_market_price_from_ws()   writes bid/ask into MarketData in-place
  → _queue_by_efp()                  computes Effective Fill Price (VWAP at trade size)
      → EFP drift > $0.005 since last eval  → urgent queue
      → > 5 s since last eval + new data    → background queue
  → _process_pending_evals()         urgent first; background fills remainder
      → _evaluate_constraint()       arb math on live ask prices (spread-aware)
      → _try_enter_or_replace()      enter new position or replace existing one
```

**Measured system performance (2026-03-09, post-P0/P1 latency fixes):**

| Metric | Value |
|--------|-------|
| WS shards | 9 × 2,000 assets |
| WS message rate | ~1,700 msg/s |
| Markets with live bid/ask | ~8,000 |
| Steady-state p50 latency | 19–167 ms (was 2–6 s pre-fix) |
| Background queue | drains to 0 (was permanently ~1,400) |
| WS reconnect spike (p50) | 1–4 s during ~30 s recovery window, then < 200 ms |
| Rust arb math | 4.2 µs/eval (19,000× faster than Python 80 ms) |
| Exec lock HTTP overhead | **REMOVED** in v0.04.04 |

### 2.2 Historical Note: Four-Layer Pipeline (pre-v0.04.00)

Prior to v0.04.00, the system ran four independent processes under a supervisor (`main.py`), communicating via JSON files:

- **L1** (`layer1_runner.py`) — market data collection, 30 s poll  
- **L2** (`layer2_runner.py`) — mutex constraint detection, 5 min poll  
- **L3** (`layer3_runner.py`) — arbitrage scanning via CVXPY LP  
- **L4** (`layer4_runner.py`) — execution and position monitoring  

The runner scripts are retained in the repo as reference. They are not used by the current supervisor. The event-driven `trading_engine.py` subsumes L2, L3, and L4.

### 2.3 Position Lifecycle

```
Trading Engine finds opportunity
  → _try_enter_or_replace()
      ├─ No existing position → ENTER (capital deployed)
      ├─ Better opportunity found → REPLACE (capital recycled, ~$0.001 fee)
      └─ Position open, monitoring → price drift triggers re-evaluation

Position resolution:
  ├─ All markets in group resolve (price → 1.0) → CLOSE (profit realised)
  └─ WS market_resolved event → instant resolution trigger (Phase 6b)
```

**Key rule:** Positions are **never** closed by time expiry. Capital stays locked until markets actually resolve. This prevents phantom P&L on long-dated markets. (See INC-002 for the incident that motivated this rule.)


### 2.4 Resolution Validator

Before entering a position, the Trading Engine optionally calls the Anthropic API to validate the market's actual resolution date against its rules text. This catches cases where the API `endDate` is misleading (e.g. shows "March 31" but the rules say "December 31").

| Setting | Location | Value |
|---------|----------|-------|
| Enable/disable | `config.yaml` → `arbitrage.resolution_validation.enabled` | `true` |
| API key | `config/secrets.yaml` → `resolution_validation.anthropic_api_key` | `sk-ant-...` |
| Entry filter | `config.yaml` → `arbitrage.max_days_to_resolution` | 60 days |
| Replacement filter | `config.yaml` → `arbitrage.max_days_to_replacement` | 30 days (stricter than entry — only swap in faster-resolving opportunities) |
| Cache TTL | `config.yaml` → `resolution_validation.cache_ttl_hours` | 168 h (1 week) |

**Replacement filter rationale:** Since all entered positions already satisfy the 60-day entry filter, replacement candidates must satisfy the tighter 30-day threshold. This ensures replacement only occurs when the incoming opportunity offers meaningfully faster capital velocity.

### 2.5 Trading Modes

| Mode | `live_trading.enabled` | `shadow_only` | Behaviour |
|------|------------------------|---------------|-----------|
| **PAPER** | `false` | — | Simulated trades only; no CLOB calls |
| **SHADOW** | `true` | `true` | Paper trades + validates fills against live order book |
| **LIVE** | `true` | `false` | Real money via Polymarket CLOB API |

Switch modes with `scripts/mode.sh` (see §5.2).

### 2.6 VPS Deployment

A ZAP-Hosting Lifetime VPS runs an independent copy of the trading system.

| Property | Value |
|----------|-------|
| Host | 193.23.127.99 (Frankfurt/Eygelshoven, Germany) |
| Specs | 4 vCPUs (AMD EPYC), 4 GB RAM, 25 GB NVMe, Ubuntu 24.04 |
| One-time cost | $76 (lifetime, no recurring fees) |
| Service | `prediction-trader.service` (systemd, Restart=always) |
| Dashboard | http://193.23.127.99:5556 |
| Repo path | `/root/prediction-trader` |
| Venv path | `/root/prediction-trader-env` |
| Path note | Uses `/root/` not `/home/andydoc/` — sed replacements applied to runner files on VPS; not committed to git |
| Suspension risk | Must log into ZAP dashboard every 3 months to avoid suspension |

```bash
# SSH access
ssh root@193.23.127.99

# Monitor VPS logs
ssh root@193.23.127.99 'tail -f /root/prediction-trader/logs/layer4_$(date +%Y%m%d).log'

# Pull latest code to VPS
ssh root@193.23.127.99 "cd /root/prediction-trader && git pull --ff-only origin main"
```

### 2.7 Multi-Machine Coordination

The execution control server (`execution_control.py`) and its client were removed in v0.04.04 to eliminate HTTP overhead from the trading loop. Multi-machine coordination strategy is **TBD**. Current arrangement: VPS runs the production trading system independently; laptop and desktop are used for development only.


---

## 3. File Structure

```
/home/andydoc/prediction-trader/              ← WSL Ubuntu (authoritative running code)
│
├── main.py                                   ← Supervisor: starts Market Scanner + Trading Engine + Dashboard
├── trading_engine.py                         ← v0.04.00+: event-driven core (replaces L2+L3+L4)
├── paper_trading.py                          ← Paper/shadow position lifecycle engine
├── live_trading.py                           ← Live CLOB trading engine
├── resolution_validator.py                   ← AI-powered resolution date validation (Anthropic API)
├── dashboard_server.py                       ← Web dashboard (port 5556, SSE live updates)
├── orderbook_depth.py                        ← Phase 5a: CLOB book depth analysis
├── state_db.py                               ← SQLite in-memory state + WAL disk mirror (Phase 8e/8f)
├── websocket_manager.py                      ← Phase 6: WS market+user channels, local book mirror
│
├── layer1_runner.py                          ← Market Scanner process entry
├── layer2_runner.py                          ← [LEGACY] L2 process entry (not used by supervisor)
├── layer3_runner.py                          ← [LEGACY] L3 process entry (not used by supervisor)
├── layer4_runner.py                          ← [LEGACY] L4 process entry (not used by supervisor)
│
├── layer1_market_data/
│   ├── market_data.py                        ← MarketDataManager, PolymarketCollector
│   └── data/polymarket/latest.json           ← Current market snapshot (33 k+ markets)
├── layer2_constraint_detection/
│   ├── constraint_detector.py                ← ConstraintDetector (mutex group finder)
│   └── data/latest_constraints.json          ← Detected constraint groups (regenerated)
├── layer3_arbitrage_math/
│   ├── arbitrage_engine.py                   ← CVXPY LP, Bregman projection, Frank-Wolfe optimisation
│   └── data/opportunities_*.json             ← Found opportunities (regenerated)
│
├── config/
│   ├── config.yaml                           ← All parameters (in git — no secrets)
│   └── secrets.yaml                          ← Polymarket + Anthropic API keys (NOT in git)
│
├── data/system_state/
│   └── execution_state.json                  ← Positions, capital, trade history (also mirrored in SQLite)
│
├── logs/
│   └── layer{1-4}_YYYYMMDD.log              ← Daily rotating logs per layer
│
├── scripts/                                  ← Operational scripts (see §5)
│   ├── start.sh / START_TRADER.bat           ← Start system
│   ├── stop.sh / STOP_TRADER.bat             ← Stop system
│   ├── restart.sh / RESTART_TRADER.bat       ← Stop + git pull + restart
│   ├── mode.sh                               ← Switch paper/shadow/live/stop
│   ├── status.sh                             ← PID check + capital summary
│   ├── sync.sh                               ← WSL git commit + push
│   ├── START_TRADER_HIDDEN.vbs               ← For Windows Startup folder (hidden window)
│   └── debug/                                ← One-off investigation scripts (gitignored)
│
├── PROGRESS_ROADMAP.md                       ← This file
└── HEARTBEAT.md                              ← Agent standing instructions

/home/andydoc/prediction-trader-env/          ← Python virtual environment

root@193.23.127.99:/root/prediction-trader/   ← VPS (ZAP-Hosting, independent copy)
root@193.23.127.99:/root/prediction-trader-env/ ← VPS Python venv
```

> **Note:** Windows mirror (`C:\Users\andyd\ai-workspace\`) was retired 2026-03-07. WSL is the sole authoritative source. The VPS is an independent production instance, not a mirror.


### Key Config Files

**`config/config.yaml`** — in git, safe to commit (no secrets):
```yaml
arbitrage:
  capital_per_trade_pct: 0.10          # 10% of capital per position (floor $10, cap $1,000)
  max_concurrent_positions: 20
  max_days_to_resolution: 60           # Skip new positions resolving > 60 days away
  max_days_to_replacement: 30          # Only swap in candidates resolving < 30 days (faster velocity)
  max_profit_threshold: 0.3            # Skip > 30% arbs (likely bad data)
  min_profit_threshold: 0.03           # Skip < 3% arbs (not worth fees)
  resolution_validation:
    enabled: true
    cache_ttl_hours: 168
  fees:
    polymarket_taker_fee: 0.0001
live_trading:
  enabled: true
  shadow_only: true                    # SHADOW mode — set false to go LIVE
```

**`config/secrets.yaml`** — NOT in git:
```yaml
polymarket:
  host: "https://clob.polymarket.com"
  chain_id: 137
  private_key: "0x..."
  funder_address: "0x..."
resolution_validation:
  anthropic_api_key: "sk-ant-..."
```

---

## 4. Setup From Scratch

### Prerequisites
- Windows PC with WSL2 (Ubuntu)
- Python 3.10+ in WSL
- Polymarket account with API credentials
- Anthropic API key (for resolution validation)

### Quick Setup
```bash
# 1. Clone the repo
cd ~ && git clone https://github.com/andydoc/Prediction-trading prediction-trader
mkdir -p ~/prediction-trader/data/system_state ~/prediction-trader/logs

# 2. Create Python environment
python3 -m venv ~/prediction-trader-env
source ~/prediction-trader-env/bin/activate
pip install pyyaml aiohttp requests numpy scipy cvxpy flask py-clob-client anthropic

# 3. Configure secrets
cp ~/prediction-trader/config/secrets.yaml.example ~/prediction-trader/config/secrets.yaml
nano ~/prediction-trader/config/secrets.yaml    # Add Polymarket private key + Anthropic API key

# 4. Start the system
cd ~/prediction-trader && rm -f *.pid
nohup python main.py > logs/main.log 2>&1 &

# 5. Verify it's running
curl -s http://localhost:5556/ | head -5         # Dashboard should return HTML
tail -20 logs/layer4_$(date +%Y%m%d).log        # Should show Trading Engine activity
```

### Windows Auto-Start
Copy `scripts/START_TRADER_HIDDEN.vbs` to:
```
C:\Users\andyd\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup\
```


---

## 5. Operating the System

### 5.1 Starting and Stopping

```bash
# ── From Windows ──────────────────────────────────────────────────
scripts\START_TRADER.bat              # Start with visible console window
scripts\START_TRADER_SILENT.bat       # Start silently (for Task Scheduler)
scripts\START_TRADER_HIDDEN.vbs       # Start hidden (for Startup folder)
scripts\STOP_TRADER.bat               # Stop all layers
scripts\RESTART_TRADER.bat            # Stop + git pull + restart

# ── From WSL ──────────────────────────────────────────────────────
source ~/prediction-trader-env/bin/activate
cd ~/prediction-trader
rm -f *.pid && nohup python main.py > logs/main.log 2>&1 &

wsl bash scripts/stop.sh              # Stop all
wsl bash scripts/stop.sh --dash       # Stop dashboard only
wsl bash scripts/stop.sh --l4         # Stop Trading Engine only
wsl bash scripts/restart.sh           # Stop + git pull + restart
wsl bash scripts/restart.sh --clean   # As above + purge stale L2/L3 data
```

### 5.2 Mode Switching

```bash
wsl bash scripts/mode.sh paper        # Paper trading (safe default)
wsl bash scripts/mode.sh shadow       # Paper + live order book validation
wsl bash scripts/mode.sh live         # Real money — runs pre-flight checks first
wsl bash scripts/mode.sh stop         # Emergency: revert to paper + cancel open orders
```

### 5.3 Monitoring

```bash
# Dashboard (primary)
http://localhost:5556                  # Laptop
http://193.23.127.99:5556             # VPS

# Quick status
wsl bash scripts/status.sh            # PID check + capital summary
wsl bash scripts/status.sh --full     # Full health + P&L breakdown

# Live log tail
wsl tail -30 /home/andydoc/prediction-trader/logs/layer4_$(date +%Y%m%d).log

# Capital snapshot (one-liner)
wsl python3 -c "
import json
d = json.load(open('/home/andydoc/prediction-trader/data/system_state/execution_state.json'))
print(f'Cap=\${d[\"current_capital\"]:.2f}  Open={len(d[\"open_positions\"])}  Closed={len(d[\"closed_positions\"])}')"
```

### 5.4 Multi-Machine Control

> **Status: TBD.** The execution control server was removed in v0.04.04. A replacement strategy for coordinating laptop and VPS has not yet been designed. For now, treat them as fully independent instances.

### 5.5 Git Sync

```bash
# Push from WSL (laptop → GitHub)
wsl bash scripts/sync.sh "your commit message"

# Pull on VPS (GitHub → VPS)
ssh root@193.23.127.99 "cd /root/prediction-trader && git pull --ff-only origin main"
```

### 5.6 Recovery After Crash or Reboot

Auto-start is enabled via the Windows Startup folder. If auto-start fails:

1. Double-click `scripts/START_TRADER.bat`, **or**
2. In WSL: `source ~/prediction-trader-env/bin/activate && cd ~/prediction-trader && rm -f *.pid && nohup python main.py > logs/main.log 2>&1 &`
3. Wait 15 seconds, then verify: http://localhost:5556

All state persists in `data/system_state/execution_state.json`. No data is lost on restart.

### 5.7 Go-Live Checklist

1. Deposit $100+ USDC to your Polymarket wallet
2. Verify CLOB connectivity: `python test_clob_connect.py`
3. Run shadow mode for at least 24 hours to validate order book fill assumptions
4. Switch to live: `scripts/mode.sh live`
5. Monitor the dashboard Live tab and the layer4 log
6. Emergency revert: `scripts/mode.sh stop`


---

## 6. Roadmap

Status key: ✅ Done · 🔲 Not started · 🔧 In progress · ⏸ Deferred

---

### Phase 1 — Core Stability ✅ COMPLETE

| # | Item | Status |
|---|------|--------|
| 1.1 | Four-layer pipeline (L1–L4) with supervisor | ✅ |
| 1.2 | Paper trading with full position lifecycle | ✅ |
| 1.3 | Dashboard with Paper / Shadow / Live tabs | ✅ |
| 1.4 | L1 full pagination (33 k+ markets) | ✅ |
| 1.5 | L3 polytope mutex completeness guard | ✅ |
| 1.6 | Resolution validator (Anthropic API) | ✅ |
| 1.7 | Remove time-based position expiry | ✅ |
| 1.8 | Max days-to-resolution entry filter (60 days) | ✅ |
| 1.9 | Max days-to-replacement filter (30 days, stricter than entry) | ✅ |
| 1.10 | 24 h replacement protection (positions near resolution immune from swap) | ✅ |
| 1.11 | Sell arb payout formula corrected | ✅ |
| 1.12 | Supervisor double-instance bug fixed | ✅ |

---

### Phase 2 — Go Live 🔲

Pre-requisites before any real money moves:

| # | Item | Status |
|---|------|--------|
| 2.1 | Refactor `paper_trading.py` → PositionManager + TradingExecutor (see §7.1) | 🔲 |
| 2.2 | Pre-trade validation: re-read live books at execution time (see §7.2) | 🔲 |
| 2.3 | negRisk sell arb capital calculation (collateral = $1.00, not sum(NO asks)) — §7.5c | 🔲 |
| 2.4 | negRisk shadow validation: compare negRisk vs standard fill simulation — §7.5d | 🔲 |
| 2.5 | Deposit $100+ USDC to Polymarket wallet | 🔲 |
| 2.6 | Run shadow mode 24 h+ to validate order book fill assumptions | 🔲 |
| 2.7 | First live trade | 🔲 |
| 2.8 | Live P&L tracking on dashboard | 🔲 |
| 2.9 | Position reconciliation: CLOB fills vs paper record | 🔲 |


---

### Phase 3 — Reliability and Monitoring 🔧 Partial

| # | Item | Status |
|---|------|--------|
| 3.1 | Wire dashboard control panel (mode switch, parameter save, restart via API) | 🔲 |
| 3.2 | WhatsApp notifications via OpenClaw (trade alerts, errors, daily summary) | 🔲 |
| 3.3 | Multi-machine coordination strategy (replaces removed exec control server) | 🔲 |
| 3.4 | File structure reorganisation (runners into layer dirs, services area) | ⏸ Phase 3 only — do not refactor during active trading |

---

### Phase 4 — Scale and Optimise 🔲

| # | Item | Status |
|---|------|--------|
| 4.1 | Increase `capital_per_trade_pct` from 10% → 30–50% (after backtesting) | 🔲 |
| 4.2 | Historical performance analytics on dashboard | 🔲 |
| 4.3 | Multi-exchange support (Kalshi) | 🔲 |
| 4.4 | Git branch strategy and release tagging (see §12) | 🔲 |


---

### Phase 5 — Risk Management 🔧 Partial (designed 2026-03-08)

#### 5a — Order Book Depth and Liquidity

**Problem:** The system currently enters arbs based on price without checking whether the order book can absorb the full trade size.

**Designed solution:**
1. Fetch the CLOB order book for every leg before entry or replacement
2. Apply 80% haircut to reported depth (phantom order allowance)
3. `trade_size = min(10% capital, min_leg_depth_after_haircut)`
4. If min depth across all legs < $5, skip the opportunity entirely
5. Place orders at the furthest price within usable depth (not best ask)
6. In live trading, use FAK (Fill And Kill) orders — fills what's available, cancels the rest

**Partial fill handling (score-based):**
- After order submission, check actual fills on all legs
- Compute arb score of the actual filled position
- If score ≥ threshold → accept the imbalanced position
- If score < threshold → unwind the minimum needed on overfilled legs
- If no partial retention yields an acceptable score → full unwind all legs

| # | Item | Status |
|---|------|--------|
| 5a.1 | Order book depth infrastructure: CLOB fetching, 80% haircut (`orderbook_depth.py`) | ✅ |
| 5a.2 | Integrate depth check into entry/replacement flow; log depth-limited trades | 🔲 |
| 5a.3 | Display depth data on dashboard | 🔲 |

#### 5b — Shadow Trading Honesty (Scale Simulation)

**Problem:** At scale, profitability may decline due to thin books. Current shadow mode does not reflect this.

**Designed solution:**
- Set `initial_capital` / `max_capital` to $10,000 to expose real deployment ceiling
- Remove `max_positions` hard cap (replace with soft warning at 50)
- Cap simulated shadow fills at 80% of actual book depth per leg
- Log "depth-limited" trades separately to measure how often depth constrains capital deployment

| # | Item | Status |
|---|------|--------|
| 5b | Config to $10k, remove max_positions, depth-limited shadow fills | 🔲 |

#### 5c — Partial Fill Handling (Live)

| # | Item | Status |
|---|------|--------|
| 5c | Score-based unwind logic, minimum unwind calc, fill metadata logging | 🔲 |

#### 5d — Replacement Chain Tracking (Reporting)

**Design decision — hybrid approach:**
- **For replacement decisions:** Forward-looking only. Sunk time is sunk. Prevents irrational holding due to historical chain cost. Maximises capital velocity.
- **For analytics reporting:** Track full chain history via `chain_id`, `chain_start_time`, `chain_cumulative_fees`. True return = `(payout − total_fees) / chain_duration`.
- If chains consistently drag returns, tighten *entry* criteria — not replacement criteria.

| # | Item | Status |
|---|------|--------|
| 5d | chain_id / chain_start_time / chain_cumulative_fees in metadata; dashboard chain view | 🔲 |

#### 5e — FAK Live Trading

| # | Item | Status |
|---|------|--------|
| 5e | FAK `time_in_force`, real fill checking, live unwind on partial fills | 🔲 |


---

### Phase 6 — WebSocket Integration 🔧 Partial (designed 2026-03-08)

**Goal:** Replace REST polling with persistent WebSocket connections for price data, order book depth, fill confirmation, and resolution detection. Latency improvement is math-path-agnostic — benefits direct LP, Bregman/FW, and polytope paths equally.

**Architecture:**
- **Market channel** (`wss://ws-subscriptions-clob.polymarket.com/ws/market`): `book`, `price_change`, `best_bid_ask`, `last_trade_price`, `market_resolved` events. Subscribes to asset IDs for all markets in active constraint groups. Maintains local order book mirror.
- **User channel** (`wss://ws-subscriptions-clob.polymarket.com/ws/user`): `trade` (MATCHED → CONFIRMED lifecycle), `order` events. Provides instant fill confirmation, replacing REST polling.
- Auto-reconnect with exponential backoff (1 s → 60 s). PING heartbeat every 10 s (Polymarket requirement). Dynamic subscription without reconnecting.

**Integration summary:**

| Consumer | Before (REST) | After (WS) | Benefit |
|----------|--------------|------------|---------|
| Price monitoring | 30 s stale snapshot | Live `price_change` callbacks | Instant mispricing detection |
| Pre-trade depth | REST `GET /book` per leg | Local book mirror | Zero-latency depth check |
| Fill confirmation | REST `GET /orders` polling | WS `trade` events | Instant fill status |
| Resolution detection | L1 poll + price→1.0 check | WS `market_resolved` event | Instant trigger |

| # | Item | Status |
|---|------|--------|
| 6a | Core `websocket_manager.py`: market+user channel loops, local book mirror, callback system, auto-reconnect | ✅ |
| 6b | Trading Engine integration: start WS in engine, subscribe open assets, resolution + fill callbacks | ✅ |
| 6c | WS→L3 price bridge: engine writes `data/ws_prices.json`; L3 overlays live prices onto MarketData before scanning. Uses actual No prices (not `1 − Yes`). Resolved assets pruned via `market_resolved`. | ✅ |
| 6d | `orderbook_depth.py` WS mode: `get_depth_from_ws()`; REST fallback if WS book stale > 30 s | 🔲 |
| 6e | Dashboard: WS connection status, message rates, subscription count, book staleness | 🔲 |
| 6f | `websocket:` section in `config.yaml` (enable/disable, URLs, heartbeat interval, staleness threshold) | 🔲 |


---

### Phase 7 — Architecture Evolution ⏸ Designed, deferred (2026-03-09)

#### 7.1 PositionManager / Executor Refactor

**Problem:** `paper_trading.py` bundles position tracking with simulated execution. Live trading requires actual fill prices, not theoretical ones.

**Design:** Split into:
- **PositionManager** — tracks positions, capital, state, resolution, P&L. `record_entry(opp, fills)` takes actual fill prices.
- **TradingExecutor** (abstract) → `PaperExecutor`, `ShadowExecutor`, `LiveExecutor`
- Engine calls: `fills = executor.execute(opp)` → `position_manager.record_entry(opp, fills)`

**Status:** Design complete. Implementation deferred to Phase 2 (go-live prerequisite).

#### 7.2 Pre-Trade Validation (Two-Phase Commit)

**Problem:** Between arb detection and execution, the book can move. Multi-leg arbs are especially exposed.

**Design:**
1. **Detection** (current): WS price event → constraint eval → arb math → candidate opportunity
2. **Validation** (new, pre-execution): Re-read local book mirror for all legs at trade size. If any leg book > 5 s stale, REST fallback for that leg only. Abort if `real_profit / expected_profit < 0.70` or insufficient depth.

**Status:** Design complete. Implementation as part of Phase 2.

#### 7.3 Rust Port (Performance)

**Context:** Python arb math runs ~80 ms/constraint. p50 queue-to-eval latency is ~8 s due to batch processing under GIL.

**Pragmatic path (PyO3/maturin extension, hot path only):**
- `_arb_mutex_direct()` — price sum + bet sizing (10–20× speedup)
- `frank_wolfe_optimal_bets()` — iterative LP (50–100× with SIMD)
- Keep Python for everything else (WS, state, orchestration)

**Expected p50 latency:** ~8 s → < 1 s

**Status:** Design complete. High effort; not blocking shadow trading. Deferred.

**Future note:** Once Rust speeds up eval throughput, paper-trade Approach 4 (Weighted Book Distance queue metric) against current Approach 1 (EFP) to determine if queue metric matters.

#### 7.4 SQLite Migration

**Problem:** JSON state files are non-atomic, don't support partial reads, and grow linearly with closed positions.

**Proposed schema:**
```sql
markets     (market_id PK, data JSON, updated_at)
constraints (constraint_id PK, data JSON, updated_at)
positions   (position_id PK, status, data JSON, opened_at, closed_at)
trades      (trade_id PK, position_id FK, data JSON, executed_at)
price_history (asset_id, timestamp, bid, ask, efp)
```
Migration order: `execution_state.json` first → markets → constraints.

**Status:** Phase 8e/8f completed partial migration (in-memory SQLite + WAL disk mirror). Full schema migration deferred.

#### 7.5 negRisk Execution

**Context:** Polymarket negRisk contracts reduce collateral for mutex groups by recognising mutual exclusivity at the contract level. All our constraint groups use negRisk (detected via `negRiskMarketID`).

**Key rules:**
- **Buy arbs** (sum < 1.0): Do **not** use negRisk — standard markets have lower cost, which is the profit source
- **Sell arbs** (sum > 1.0): negRisk reduces collateral from `sum(NO asks)` to $1.00 per unit
- Orders must pass `negRisk: true` to the CLOB API for negRisk markets

| # | Item | Status |
|---|------|--------|
| 7.5a | negRisk metadata tag in arb opportunities (`metadata.neg_risk: true/false`) | ✅ |
| 7.5b | negRisk flag passed to CLOB `create_order()` via `PartialCreateOrderOptions` | ✅ |
| 7.5c | Sell arb capital calculation rework: collateral = $1.00, not sum(NO asks) | 🔲 |
| 7.5d | Shadow validation: compare negRisk vs standard fill simulation | 🔲 |


---

### Phase 8 — Latency Optimisation 🔧 Partial (designed + P0/P1 complete 2026-03-09)

#### Root Cause Analysis (2026-03-09 bottleneck audit)

The Rust arb math port achieved 19,000× speedup (80 ms → 4.2 µs) but total system latency barely improved. Root cause: arb math was only 8% of wall time.

| Bottleneck | Share of wall time | Detail |
|------------|-------------------|--------|
| GIL contention | ~80% | ~1,700 WS callbacks/s each grab the GIL; eval thread starved. 100 evals take 3 s wall time despite ~10 ms CPU. |
| `asyncio.sleep(1.0)` | ~10% | Urgent evals still waited up to 999 ms before processing. |
| Exec lock HTTP | ~2% | 2 synchronous round-trips per iteration (12.9 ms). **Now removed in v0.04.04.** |

| Measured metric | Value |
|-----------------|-------|
| Eval batch wall time p50 | 3,026 ms for ~10 ms of actual CPU |
| Background queue steady state | ~1,400 items (never drained) |
| Exec lock HTTP total | 82 s over session (12,780 calls) |
| Eval batches finding arb | 33.9% (66% of eval CPU wasted on non-arb constraints) |

**Measured improvement after P0+P1 fixes (v0.04.03/04, 697 samples):**

| Metric | Pre-P0 (v0.04.02) | Post-P0/P1 (v0.04.04) | Improvement |
|--------|-------------------|----------------------|-------------|
| p50 steady-state | 2–6 s | 19–167 ms | **~30×** |
| p95 steady-state | 60–300 s | 54–2,408 ms | **~30–100×** |
| Background queue | ~1,400 (permanent) | 0 (drains fully) | **∞** |
| Iteration rate | ~17 evals/s | ~500 evals/s | **~30×** |
| Exec lock overhead | 12.9 ms/iter | 0 (removed) | **∞** |

> Remaining spikes (p50 1–4 s) occur exclusively during WS shard reconnects (~every 10 min, ~30 s recovery). Steady-state is within P0+P1 targets.

#### P0 — Quick Python Fixes ✅ COMPLETE

| # | Item | Status |
|---|------|--------|
| 8a | Replace `asyncio.sleep(1.0)` with `asyncio.Event`-based wake (instant urgent processing, 50 ms fallback) | ✅ |
| 8b | Cache exec lock status (check every 30 s, not every iteration) | ✅ |
| 8c | Increase `MAX_EVALS_PER_BATCH` from 100 → 500 | ✅ |
| 8d | Remove `indent=2` from JSON state serialisation (2.2 MB → ~700 KB, 2–3× faster writes) | ✅ |

#### P1 — SQLite State ✅ COMPLETE

| # | Item | Status |
|---|------|--------|
| 8e | SQLite in-memory DB + WAL journal for `execution_state`; periodic `db.backup()` to disk | ✅ |
| 8f | Incremental position updates (INSERT/UPDATE single rows, not full file rewrite) | ✅ |
| 8f.1 | `state_db.py`: `read_state_from_disk()` for dashboard read-only access; compatibility aliases | ✅ |
| 8f.2 | Dashboard reads engine metrics from `write_status()` + SQLite state | ✅ |

#### P2 — Rust Bregman + Polytope ✅

| # | Item | Status |
|---|------|--------|
| 8g | Port Bregman KL projection to Rust (iterative Dykstra, ~100 µs vs CVXPY 80 ms) | ✅ Skipped Bregman; FW alone at ~181µs is fast enough |
| 8h | Port polytope construction to Rust (combinatorics only) | ✅ `build_scenarios()` in Rust |
| 8i | Reintroduce polytope check for mutex constraints where direct check found no arb (partial hedges) | ✅ `polytope_arb()` wired for all constraint types |

#### P3 — Reduce Python↔Rust Boundary Crossings ✅

| # | Item | Status |
|---|------|--------|
| 8j | Batch EFP computation: single `Vec<(asset_id, asks)>` → Rust → `Vec<(asset_id, efp, drift)>` | ✅ `batch_effective_fill_prices()` + dirty-asset buffering |
| 8k | Move constraint index (`asset_to_constraints`) into Rust `DashMap` | ✅ Addressed by 8j: index only queried ~20/sec (batch) instead of ~1700/sec (per-event) |
| 8l | WS stale-asset re-subscribe sweep (replaces REST fallback — WS re-sub ~10 ms vs REST ~200 ms) | ✅ 60s periodic sweep, re-subscribes assets >30s stale |

#### P4 — Full Rust Engine 🔲

| # | Item | Status |
|---|------|--------|
| 8m | `tokio-tungstenite` WS client with local book mirror (replaces Python `websockets`) | 🔲 |
| 8n | Rust eval queue with `tokio::select!` instant wake (no polling, no sleep) | 🔲 |
| 8o | `rusqlite` state persistence (WAL mode, incremental updates) | 🔲 |
| 8p | Single Rust binary: WS + queue + eval + state. Python kept for dashboard + resolution validator only. | 🔲 |
| 8q | Full Rust port: dashboard (axum/warp), resolution validator, everything. Zero Python. | 🔲 |

#### Expected Latency by Phase

| Phase | p50 | p95 | Background queue | Status |
|-------|-----|-----|-----------------|--------|
| Pre-P0 (baseline) | 2–6 s | 60–300 s | ~1,400 (growing) | — |
| After P0 | ~200 ms | ~2 s | ~500 (draining) | ✅ Done |
| After P1 | ~200 ms | ~1 s | ~200 | ✅ Done |
| After P2 | ~150 ms | ~800 ms | ~100 | ✅ |
| After P3 | ~50 ms | ~300 ms | ~50 | ✅ (restart required) |
| After P4 | < 1 ms | < 5 ms | 0 (instant) | 🔲 |


---

## 7. Version History

Most recent first. Each entry summarises what changed and why. Full implementation detail is in the git log.

---

### v0.04.07 (2026-03-11) — Code Cleanup, Paper Retirement, Run-Once Scanner
- **ARCHIVED** Legacy runners to `archive/`: `layer2_runner.py`, `layer3_runner.py`, `layer4_runner.py`, `start_all.sh`, `setup.ps1`, `.bak` files
- **RENAMED** `layer1_runner.py` → `initial_market_scanner.py` — runs once at startup (not a persistent process)
- **CHANGED** `main.py`: scanner runs once (blocking, 120s timeout with `TimeoutExpired` catch), then engine + dashboard supervised
- **RETIRED** Paper trading mode — shadow is now minimum operating mode, live when ready
- **REMOVED** `paper_trading:` config section from `config.yaml`
- **CHANGED** `live_trading.enabled` always `true`; `shadow_only` controls shadow vs live (default: `true`)
- **CHANGED** Mode display simplified: `shadow` or `live` (no more `paper`)
- **MEASURED** Post-P3 stabilised latency (199 samples, current session):
  - Steady-state (bg=0): **p50=35ms, p90=195ms, min=2ms**
  - Overall session: **p50=165ms, p90=5s** (spikes during WS reconnects only)
  - Background queue: drains to 0 in 33% of samples (rest recovering from ~10-min WS reconnect cycle)

### v0.04.06 (2026-03-10) — Batch EFP + Dirty-Asset Buffering + Stale Re-subscribe (Phase 8 P3)
- **ADDED** `batch_effective_fill_prices()` in Rust: computes EFPs for all dirty assets in one PyO3 call (was 1700 individual calls/sec)
- **CHANGED** WS callbacks now buffer `asset_id` into `_dirty_assets` set (1 Python op) instead of per-event queue processing (~10 Python ops)
- **ADDED** `_process_dirty_assets()`: batch processes all buffered assets at start of each eval loop — collects books, batch Rust EFP, queue decisions
- **ADDED** Stale-asset WS re-subscribe sweep: every 60s, re-subscribes assets with book data >30s old (replaces REST fallback plan, WS re-sub ~10ms vs REST ~200ms)
- **NOTED** 8k (Rust DashMap for constraint index) addressed by 8j: index now queried ~20/sec in batch vs ~1700/sec per-event — 85× reduction, Rust DashMap unnecessary
- **REQUIRES** System restart to pick up P3 changes

### v0.04.05 (2026-03-10) — Rust Polytope Reintroduction (Phase 8 P2)
- **ADDED** `polytope_arb()` in Rust: full polytope pipeline (scenario construction + Frank-Wolfe) in ~181µs vs Python CVXPY ~80ms (440× speedup)
- **ADDED** `build_scenarios()` in Rust: generates valid outcome matrices for mutex, complementary, and logical_implication constraint types
- **ADDED** `_arb_via_rust_polytope()` in `arbitrage_engine.py`: wires Rust polytope for all constraint types when `HAS_RUST=True`
- **CHANGED** Polytope now runs for mutex constraints where direct buy/sell check found no arb — catches partial hedges
- **CHANGED** Complementary and logical_implication constraints also route through Rust polytope when available
- **DESIGN** Skipped Bregman KL pre-filter; Rust FW is fast enough (~181µs) that running it unconditionally is cheaper than the pre-filter
- **MEASURED** 0 polytope arbs found in first day (expected — partial hedges are rare, but system now detects them)
- **MEASURED** 30k+ Rust mutex evals today, system stable at iter 129k+

### v0.04.04 (2026-03-10) — Dashboard SSE + Exec Control Removal
- **Replaced** dashboard AJAX polling with Server-Sent Events (SSE); `/stream` endpoint pushes capital, positions, queue depth, latency, and WS stats every 5 s
- **Removed** execution control server (`execution_control.py`), client (`execution_control_client.py`), and `exec_claim.sh` — eliminated 12.9 ms/iteration HTTP overhead from trading loop
- **Removed** Control Panel tab (mode switching, parameter editing, emergency stop)
- **Consolidated** all inline `style=` attributes into head CSS utility classes
- **Removed** 136-line dead code section (merge artifact)
- **Dashboard** now reads from SQLite state DB (primary) with JSON fallback; reduced from 1,391 → 1,071 lines (23% smaller)
- **Measured** post-P0/P1 latency (697 samples): steady-state p50 = 19–167 ms (was 2–6 s); background queue drains to 0 (was permanently ~1,400)
- **Noted** exec control strategy TBD for VPS-based multi-machine access

### v0.04.03 (2026-03-10) — Latency Bottleneck Analysis + P0/P1 Fixes
- **Analysed** full bottleneck audit: Rust arb math (19,000×) addressed only 8% of wall time; GIL contention is 80%
- **Measured** eval batch p50 = 3,026 ms for ~10 ms of CPU; exec lock HTTP = 82 s total, 12,780 calls; background queue permanently ~1,400 items
- **Added** Phase 8 (Latency Optimisation) to roadmap with P0–P4 progressive plan
- **Added** `state_db.py`: SQLite read-only access for dashboard + compatibility aliases
- **Completed** all P0 fixes: event-based wake, exec lock caching, batch size 500, no-indent JSON
- **Completed** all P1 fixes: SQLite in-memory + WAL persistence, incremental position updates

### v0.04.02 (2026-03-09) — EFP Queue Metric + negRisk Tagging
- **Added** Effective Fill Price (EFP) as 2D queue metric: VWAP at trade size captures both price and depth drift in a single value
- **Added** `LocalOrderBook.effective_fill_price(trade_size_usd)` — walks ask book to compute execution cost
- **Added** Priority queue: urgent (EFP drift > $0.005) processed first; background (> 5 s stale) fills remainder
- **Added** Real latency instrumentation: p50/p95/max reported in stats from queue time → eval time
- **Added** negRisk metadata tag and CLOB order flag (`PartialCreateOrderOptions(neg_risk=True)`)
- **Added** Phase 7 (Architecture Evolution) to roadmap

### v0.04.01 (2026-03-09) — Threaded Arb Eval + WS Stability
- **Added** `ThreadPoolExecutor` (2 workers) for CPU-bound arb evaluation — asyncio event loop stays free for WS heartbeats
- **Added** `MAX_EVALS_PER_BATCH = 100` (raised to 500 in v0.04.03)
- **Reduced** `ASSETS_PER_CONNECTION` from 4,000 → 2,000 (smaller shards, more stable)
- **Result** Zero WS disconnects over 8+ minutes (was 8+ disconnects per 8 min)

### v0.04.00 (2026-03-08/09) — Event-Driven Trading Engine
- **Created** `trading_engine.py` — single async event-driven process replacing L2+L3+L4
- **Architecture** becomes two-process: Market Scanner + Trading Engine
- **Added** bid/ask spread-aware arb math using actual ask prices (not midpoints)
- **Added** `asset_to_market` reverse lookup for instant WS → MarketData price updates
- **Added** `has_live_prices()` gate — constraints only evaluated when all markets have live WS data
- **Added** WS sharded connection pool (N connections × 2,000 assets each)
- **Kept** legacy layer runners (`layer2/3/4_runner.py`) as reference only


### v0.03.06 (2026-03-08/09) — WebSocket Integration (Phase 6a+6b+6c)
- **Added** `websocket_manager.py` — persistent WS connections (market + user channels), local order book mirror, callback system, auto-reconnect, dynamic subscription
- **Integrated** L4 runner: WS manager starts with engine, subscribes open position assets, fires resolution and fill callbacks
- **Added** WS→L3 price bridge (`data/ws_prices.json`): actual No prices from WS book (not computed as `1 − Yes`)
- **Added** Resolved market pruning via `market_resolved` WS event

### v0.03.05 (2026-03-06/07) — Dynamic Resolution Delay Model + VPS Deployment
- **Added** Resolution delay scoring: `effective_hours = raw_hours + P95_category_delay + volume_penalty`
- **Added** Dynamic P95 table loaded from `data/resolution_delay_p95.json` (12-month rolling window, 1 h cache)
- **Added** `scripts/debug/update_delay_table.py` — weekly updater, triggered by engine at startup and every ~24 h
- **Deployed** ZAP-Hosting Lifetime VPS (193.23.127.99) — $100 fresh paper capital, systemd auto-restart
- **Harvested** 512,894 resolved markets from Gamma API for delay analysis (731 MB)
- **Finding** Polymarket resolution speed improved ~6× between 2024-H1 and 2025-H2
- **12-month P95 values** (as of 2026-03-06): football 15.1 h · us_sports 44.4 h · esports 12.5 h · crypto 3.4 h · politics 702.4 h · sports_props 10.1 h · mma_boxing 26.1 h · gov_policy 41.2 h · other 35.1 h

### v0.03.04 (2026-03-04) — Sell Arb Payout Formula Fix
- **Fixed** Critical payout error in `paper_trading.py` for `mutex_sell_all` positions. Sell arb buys NO on every leg; when outcome resolves, the winning leg's NO *loses* and all other legs' NO bets *win*. Corrected formula: `payout = sum(bet_amount_k / (1 − entry_price_k))` for all legs except the winning market. (Previously used the buy-arb formula, which severely understated sell arb P&L.)

### v0.03.03 (2026-03-04) — Replacement Filter + Restart Fix
- **Added** `max_days_to_replacement: 30` — replacement candidates must resolve within 30 days (stricter than the 60-day entry filter)
- **Fixed** `restart.sh` awk syntax error; `>` → `>>` to append main.log on restart; added `disown`

### v0.03.02 (2026-03-03) — Replacement Loop Fix + Fee Config
- **Fixed** Replacement loop: same opportunity could liquidate multiple positions per round. Added `used_opp_cids` set to prevent reuse within a single round.
- **Fixed** Fee config: `paper_trading.py` was reading `polymarket_taker_fee` from wrong config path (always fell back to hardcoded default)
- **Moved** 57 one-off investigation scripts to `scripts/debug/` (gitignored)

### v0.03.01 (2026-03-03) — Replacement Protection + Validator Verification
- **Added** 24 h replacement protection: positions whose AI-validated resolution date is < 24 h away are immune from replacement scoring
- **Fixed** Replacement scoring uses AI-validated resolution date from cache (not raw API `end_date`) for the 24 h window
- **Verified** Resolution validator working in production: catching long-dated markets and unrepresented outcomes
- **Confirmed** Trump Truth Social position is legitimate: L2 correctly narrowed to 3 live brackets covering all possible outcomes

### v0.03.00 (2026-03-03) — Resolution Safety
- **Removed** `_expire_position()` — positions no longer close by time; capital stays locked until markets resolve
- **Added** `_check_group_resolved()` — all markets in group must show price → 1.0 before closing
- **Added** `resolution_validator.py` — Anthropic API call to validate true resolution dates from rules text
- **Added** `max_days_to_resolution: 60` entry filter
- **Fixed** L1 pagination (removed 10 k market cap; now fetches all 33 k+)
- **Fixed** L3 polytope path: added mutex completeness guard (price_sum < 0.90)
- **Cleaned** Japan unemployment positions (INC-001) and Somaliland positions (INC-002)

### v0.02.00 (2026-02-28) — Dashboard and Scripts
- Dashboard tabs: Paper / Shadow / Live / Control Panel
- Score column (`profit_pct / hours × 10,000`)
- Mode badge with colour coding
- Script rationalisation: 21 → 8 scripts; naming standardised across codebase
- Git repository established

### v0.01.00 (2026-02-17) — Initial System
- Four-layer pipeline (L1–L4) with supervisor (`main.py`)
- Paper trading engine with full position lifecycle
- CVXPY LP arbitrage detection
- Multi-machine execution control
- CLOB integration and shadow mode


---

## 8. Incident Log

### INC-006: Argentine Fecha 9 Postponement — Capital Locked Until May (identified 2026-03-11)
**Markets:** 1360750–1360752 (River Plate vs CA Tucumán), 1360744–1360746 (Lanús vs CD Riestra)
**Impact:** $20.00 capital locked for ~55 extra days ($10 per position). No loss, but capital velocity degraded.

Both matches were part of Argentine Liga Profesional Fecha 9, scheduled March 8, 2026. The AFA executive committee unanimously suspended all football activities March 5–8 in protest of an ARCA investigation into unpaid social security contributions. The entire round was rescheduled to the weekend of May 3, 2026. Our positions entered before the postponement was announced.

**Root cause:** External event (political/labour dispute) postponing an entire fixture round. The resolution validator cannot anticipate strike action.

**Status:** Positions correctly remain in `monitoring` — no phantom P&L. Capital will be returned when games are played (~May 3, 2026). No system fix needed; this is a market risk event. Future consideration: add a "days since entry" alert for positions exceeding 2× expected resolution time.

---

### INC-005: WSL Disk Full — 200 GB Market Snapshots (identified & fixed 2026-03-11)
**Impact:** WSL VHDX grew to 424 GB, leaving 5.21 GB free on C: drive. WSL became read-only.

L1 market scanner (`market_data.py`) was writing timestamped 39 MB JSON snapshots every 30 seconds. Over 24 days: 20,383 files × 39 MB = 200 GB in `layer1_market_data/data/polymarket/`. Only `latest.json` is needed by the system.

**Root cause:** `store_markets()` wrote both `latest.json` and `{timestamp}.json` on every collection cycle. No retention policy or disk monitoring existed.

**Fixes applied:** Removed timestamped snapshot writes (only `latest.json` kept). Added `cleanup_old_logs(max_days=3)` to `main.py` startup. Manual cleanup: deleted 20,382 snapshot files, compacted VHDX via `diskpart compact vdisk`.

---

### INC-004: TX-31 Republican Primary (identified 2026-03-03, cleared 2026-03-10)
**Markets:** 704392–704394 (Carter, Gomez, Hamden — 3 of 4+ outcomes; "Other" market missing)  
**Impact:** $0.85 phantom profit. **Cleared.**

Three positions opened Feb 21–24 (pre-validator era). Position [71] replaced by [380] normally; [380] and [465] expired by old `_expire_position()` on still-active markets. Rules state: resolves to "Other" if no nominee by Nov 3, 2026. No "Other" market exists on Polymarket.

**Root cause:** Same dual failure as INC-002 — time-based expiry on unresolved markets + unrepresented outcome not detected.

---

### INC-003: Arkansas Governor Democratic Primary (identified 2026-03-03, cleared 2026-03-10)
**Markets:** 824818–824819 (Love, Xayprasith-Mays — 2 of 3+ outcomes; "Other" and run-off markets missing)  
**Impact:** $0.89 phantom profit. **Cleared.**

Two positions opened Feb 22–27 (pre-validator) and expired by `_expire_position()` days before the actual primary (March 3, 2026).

**Root cause:** Same as INC-004 and INC-002.

---

### INC-002: Somaliland Parliamentary Election (identified and cleaned 2026-03-03)
**Markets:** 948391–948394 (4 outcomes + "no election")  
**Impact:** $0.93 phantom profit credited, then cleaned.

System entered a mutex arb. API `endDate` was 2026-03-31 (scheduled election date), but rules text permitted resolution as late as 2027-03-31 (results unknown). After 28 hours, `_expire_position()` closed the position and credited expected profit as realised profit — on a market still actively trading.

**Root cause:** Two compounding failures: (1) time-based expiry credited phantom profit on unresolved markets; (2) no validation that API `endDate` reflects the true latest resolution date in the rules.

**Fixes applied (v0.03.00):** Removed `_expire_position()`; added `_check_group_resolved()` requiring price → 1.0 on all legs; added `resolution_validator.py`; added `max_days_to_resolution: 60` filter.

---

### INC-001: Japan Unemployment — Incomplete Mutex (identified and cleaned 2026-03-03)
**Markets:** 1323418–1323422 (5 of 7 outcomes; outcomes for 2.6% and ≥ 2.7% missing)  
**Impact:** $10.00 loss (all 5 positions lost).

L1 had a hard cap of 10,000 markets. Polymarket had ~33,800. The two missing outcomes fell beyond the cutoff. L2 detected only 5 of 7 (sum = 0.889, passed the 0.85 guard). L3 direct path correctly blocked it (0.90 guard), but the Bregman/FW polytope fallback had no completeness guard — treated 5 incomplete markets as a valid arb. The outcome resolved to one of the two missing markets.

**Fixes applied (v0.03.00):** L1 now paginates fully (33 k+); L3 polytope path has mutex completeness guard at sum < 0.90.

---

## 9. Performance

> **Note:** All figures below are from the laptop instance. The VPS was deployed with $100 fresh capital but is not currently running (paused during architecture work). Combined performance view is a future feature.

### Current State (laptop, v0.04.07, 2026-03-11)

| Metric | Value |
|--------|-------|
| Cash (current_capital) | $8.83 |
| Capital deployed (open positions) | ~$100.00 |
| Total portfolio value | ~$108.83 |
| Open positions | 10 |
| Closed positions | ~1,284 |
| Net gain vs initial $100 | +$8.83 |

### Latency (post-P3 stabilised, measured 2026-03-11 10:10–10:22 UTC, 199 samples)

| Metric | Steady-state (bg=0) | Overall session | During WS reconnect |
|--------|--------------------:|----------------:|--------------------:|
| p50 | **35 ms** | 165 ms | 1–4 s |
| p90 | **195 ms** | 5,098 ms | 30–85 s |
| min | 2 ms | 2 ms | — |
| bg_queue | **0** | 263 median | 400–790 |

**Context:** Polymarket WS connections cycle every ~10 minutes (server-side). During the ~30s recovery window, live market count drops (e.g. 8500→3000→8500) and the background queue fills. In steady state between reconnects, p50 is consistently **26–78 ms** with queue at zero.

### Resolved Arb Audit

| Item | Value |
|------|-------|
| Legitimate profitable resolutions | 3 positions, $1.61 total |
| SC Sagamihara sell arb | $10.72 payout on $10.00 deployed = **+$0.72** (7.2%) — first correct sell arb resolution |
| Win rate on legitimately resolved arbs | 100% |
| Average return per resolved arb | ~$0.54 |
| Phantom profit cleaned | $0.93 (Somaliland INC-002), $10.00 loss recorded (Japan INC-001), $0.89 (Arkansas INC-003), $0.85 (TX-31 INC-004) |

### Key Lessons

- **Sell arb payout is fundamentally different from buy arb.** In a sell arb (buy NO on all legs), the winning market's NO bet *loses*; profit comes from all *other* legs resolving NO. Using the buy-arb formula would severely understate sell arb P&L on every resolved position.
- **The arb math is correct.** Both major incidents (INC-001, INC-002) were data quality and guard failures, not mathematical failures.
- **Replacement churn is negligible individually but accumulates.** ~$0.001/swap × ~1,200 swaps ≈ $1.20 total — worth tracking, not worth preventing.
- **Resolution validation prevents the INC-002 class of bug.** Full pagination prevents the INC-001 class. Both guards are now in place.

### Latency Performance (v0.04.04, 2026-03-10)

| Metric | Pre-P0 (v0.04.02) | Post-P0/P1 (v0.04.03) | Post-SSE (v0.04.04) |
|--------|-------------------|----------------------|---------------------|
| **p50 (steady-state)** | 2–6 s | 0.8–2.4 s | 19–167 ms |
| **p95 (steady-state)** | 60–300 s | 2.5–8 s | 54–2,408 ms |
| **bg_queue** | ~1,400 (permanent) | 188–750 (drains) | **0** (fully drains) |
| **Eval throughput** | ~17 evals/s | ~100/s | ~500/s |
| **Exec lock HTTP** | 12.9 ms/iter (12,780 calls) | cached (30 s) | **removed** |
| **State save** | JSON 2.2 MB rewrite | SQLite WAL backup | SQLite WAL backup |

**WS reconnect behaviour:** Every ~10 min, one or more WS shards reconnect. During the ~30 s recovery window, p50 spikes to 1–4 s and bg_queue fills temporarily. Once `live` count recovers to >8,000, latency returns to steady-state <200 ms.

**Bottleneck breakdown (measured v0.04.02, GIL-era baseline):**
| Source | % of wall time | Fix applied |
|--------|---------------|-------------|
| GIL contention (WS callbacks vs eval thread) | 80% | P0: event-based wake, batch 500 |
| `asyncio.sleep(1.0)` | 10% | P0: `asyncio.Event` with 50 ms fallback |
| Exec lock HTTP (sync `requests.get`) | 2% | P0→v0.04.04: removed entirely |
| Arb math (Python CVXPY) | 8% | Rust PyO3 (19,000× speedup) |


---

## 10. Configuration Reference

| Parameter | File | YAML path | Value | Purpose |
|-----------|------|-----------|-------|---------|
| `capital_per_trade_pct` | config.yaml | `arbitrage.capital_per_trade_pct` | 0.10 | 10% of cash per position (floor $10, cap $1,000) |
| `max_concurrent_positions` | config.yaml | `arbitrage.max_concurrent_positions` | 20 | Maximum open positions |
| `max_days_to_resolution` | config.yaml | `arbitrage.max_days_to_resolution` | 60 | Entry filter: skip new positions resolving > 60 days away |
| `max_days_to_replacement` | config.yaml | `arbitrage.max_days_to_replacement` | 30 | Replacement filter: only swap in candidates resolving < 30 days |
| `max_profit_threshold` | config.yaml | `arbitrage.max_profit_threshold` | 0.30 | Skip arbs > 30% (likely bad data) |
| `min_profit_threshold` | config.yaml | `arbitrage.min_profit_threshold` | 0.03 | Skip arbs < 3% (not worth fees) |
| `resolution_validation.enabled` | config.yaml | `arbitrage.resolution_validation.enabled` | true | AI date validation on/off |
| `cache_ttl_hours` | config.yaml | `arbitrage.resolution_validation.cache_ttl_hours` | 168 | Cache AI resolution results for 1 week |
| `polymarket_taker_fee` | config.yaml | `arbitrage.fees.polymarket_taker_fee` | 0.0001 | Polymarket fee per trade |
| `live_trading.enabled` | config.yaml | `live_trading.enabled` | true | false = paper; true = shadow or live |
| `live_trading.shadow_only` | config.yaml | `live_trading.shadow_only` | true | true = shadow; false = live |
| `anthropic_api_key` | **secrets.yaml** | `resolution_validation.anthropic_api_key` | sk-ant-... | API key for resolution validator |
| `private_key` | **secrets.yaml** | `polymarket.private_key` | 0x... | Polymarket wallet private key |
| L2 `min_price_sum` | constraint_detector.py | hardcoded | 0.85 | Minimum sum for a valid mutex group |
| L3 direct mutex guard | arbitrage_engine.py | hardcoded | 0.90 | Skip direct arb if raw sum < 0.90 |
| L3 polytope mutex guard | arbitrage_engine.py | hardcoded | 0.90 | Skip Bregman/FW if sum < 0.90 |

---

## 11. Glossary

| Term | Meaning |
|------|---------|
| **CLOB** | Central Limit Order Book — Polymarket's matching engine and trading API |
| **EFP** | Effective Fill Price — VWAP computed by walking the ask book at a given trade size. Captures both price and depth drift in a single metric. |
| **EFP drift** | Change in EFP since the last arb evaluation for a constraint. Triggers re-evaluation when > $0.005. |
| **FAK** | Fill And Kill — order type that fills as much as possible immediately, then cancels the remainder |
| **FOK** | Fill Or Kill — order type that fills entirely or cancels entirely |
| **GTC** | Good Till Cancelled — order that stays on the book until filled or manually cancelled |
| **GIL** | Global Interpreter Lock — Python's per-process mutex; prevents true parallel thread execution, causing WS callbacks to starve the eval thread |
| **KL** | Kullback-Leibler divergence — measure of distance between probability distributions; used in Bregman projection |
| **LP** | Linear Program — mathematical optimisation with linear objective and linear constraints (solved by CVXPY) |
| **MCP** | Model Context Protocol — Anthropic's standard for AI tool integration |
| **Mutex** | Mutually exclusive — a set of outcomes where exactly one resolves YES and all others resolve NO |
| **negRisk** | Negative Risk — Polymarket contract structure for mutex groups; reduces collateral by recognising mutual exclusivity at the contract level |
| **P95** | 95th percentile — value below which 95% of observations fall; used in resolution delay model |
| **PyO3** | Rust-to-Python bridge allowing Rust code to be called from Python as a native extension module |
| **Shadow mode** | Paper trading with live order book validation — no real money moves, but fills are cross-checked against actual CLOB depth |
| **Shard** | One of multiple parallel WS connections, each handling ≤ 2,000 assets to avoid server-side overload |
| **VWAP** | Volume-Weighted Average Price — average price weighted by order size at each level in the book |
| **WS** | WebSocket — persistent bidirectional connection for real-time data streaming |


---

## 12. Git Versioning Strategy

Current state: all work on `main` branch, no tags applied. Intended approach:

### Branch Strategy

| Branch | Purpose |
|--------|---------|
| `main` | Stable, tested code only |
| `dev` | Active development |
| `feature/<name>` | Individual features (e.g. `feature/resolution-validator`) |
| `hotfix/<name>` | Urgent production fixes |

### Tag Convention

Format: `vMAJOR.MINOR.PATCH` with zero-padded two-digit minor and patch (e.g. `v0.03.02`).

| Tag | Description |
|-----|-------------|
| `v0.01.00` | Initial four-layer system |
| `v0.02.00` | Dashboard and scripts |
| `v0.03.00` | Resolution safety |
| `v0.03.01` | Replacement protection + validator verification |
| `v0.03.02` | Replacement loop fix + fee config + API key to secrets.yaml |
| `v0.03.03` | Separate replacement filter (30 d) from entry filter (60 d) |
| `v0.03.04` | Sell arb payout formula fix |
| `v0.03.05` | Dynamic resolution delay model + VPS deployment |
| `v0.03.06` | WebSocket integration (Phase 6a+6b+6c) |
| `v0.04.00` | Event-driven trading engine |
| `v0.04.01` | Threaded arb eval + WS stability |
| `v0.04.02` | EFP queue metric + negRisk tagging |
| `v0.04.03` | Latency bottleneck analysis + P0/P1 fixes |
| `v0.04.04` | Dashboard SSE rewrite + exec control removal *(current)* |
| `v1.00.00` | First successful live trade |

### Implementation Steps

```bash
# Tag existing milestones retrospectively
git tag -a v0.01.00 <initial-commit-hash> -m "Initial four-layer system"
# ... repeat for each milestone above ...
git push origin --tags

# Create dev branch going forward
git checkout -b dev
git push -u origin dev
```

---

*Last updated: 2026-03-11 ~10:30 UTC*  
*Laptop: WSL Ubuntu (authoritative) · VPS: ZAP-Hosting 193.23.127.99 · Desktop: dormant*  
*Dashboard: http://localhost:5556 (laptop) · http://193.23.127.99:5556 (VPS)*  
*Git: https://github.com/andydoc/Prediction-trading (branch: main)*
