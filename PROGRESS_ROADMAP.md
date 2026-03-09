# Prediction Market Arbitrage System
# User Guide · Architecture · Roadmap · Progress

> **Version**: v0.04.03-dev (pre-release, shadow trading)
> **Last updated**: 2026-03-09 ~18:00 UTC
> **Mode**: SHADOW | Laptop: running | VPS (193.23.127.99): $100 fresh capital, all layers healthy
> **VPS**: ZAP-Hosting Lifetime (193.23.127.99) — 4 cores, 4GB RAM, Ubuntu 24.04, systemd auto-restart
> **Git**: https://github.com/andydoc/Prediction-trading (branch: `main`)

---

## 1. What This System Does

Automated detection and exploitation of pricing inefficiencies across Polymarket prediction markets. Uses mathematical arbitrage (marginal polytope construction, Bregman projection with KL divergence, Frank-Wolfe optimization) to find groups of related markets where prices are logically inconsistent, then bets across all outcomes to lock in guaranteed profit regardless of which outcome occurs.

**Not prediction. Not gambling. Pure pricing-error exploitation.**

---

## 2. Architecture

### 2.0 Current Architecture (v0.04.00+, event-driven)

Two-process system replacing the old four-layer poll-based pipeline:

| Process | Script | Purpose |
|---------|--------|---------|
| **Market Scanner** | `layer1_runner.py` | Fetches all active markets from Polymarket Gamma API (33k+), writes `latest_markets.json`. Runs at startup + periodic refresh. |
| **Trading Engine** | `trading_engine.py` | Event-driven core: constraint detection (inline), arb math, execution, position management. Reacts to WS price events in real-time. |
| Dashboard | `dashboard_server.py` | Web UI on port 5556 |
| Exec Control | `execution_control.py` | Multi-machine lock server on port 5557 |

**Data flow:**
```
WS price_change/book event
  → _update_market_price_from_ws(): writes bid/ask into MarketData in-place
  → _queue_by_efp(): compute effective fill price (VWAP at trade size)
      → if EFP drift >0.5c from last eval → urgent queue
      → if >5s since last eval + new data → background queue
  → _process_pending_evals(): urgent first, background fills remaining
      → _evaluate_constraint(): arb math on live ask prices (spread-aware)
      → _try_enter_or_replace(): enter or replace positions
```

**Key metrics (measured 2026-03-09):**
- WS: 9 shards × 2000 assets, ~1700 msgs/sec, ~8000 markets with live bid/ask
- Queue: p50 latency 2-6s (bottleneck: GIL contention, NOT arb math)
- Rust arb math: 4.2µs/eval (19000× faster than Python 80ms), but only 8% of wall time
- Eval batch wall time: p50=3026ms for 100 evals containing ~10ms actual CPU work (300× overhead)
- Background queue: ~1400 constraints perpetually queued, never drains
- Exec lock HTTP: 12.9ms/iteration overhead (12,780 calls in session)
- Stability: 0 disconnects in steady state (ThreadPoolExecutor prevents heartbeat blocking)

### 2.1 Legacy Four-Layer Pipeline (pre-v0.04.00, kept for reference)

| Layer | Runner | Core Module | Frequency | Purpose |
|-------|--------|-------------|-----------|---------|
| **L1** Market Data | `layer1_runner.py` | `layer1_market_data/market_data.py` | 30s | Fetches all active markets from Polymarket API (33k+) |
| **L2** Constraints | `layer2_runner.py` | `layer2_constraint_detection/constraint_detector.py` | 5min | Detects mutex groups (logically exclusive market sets) |
| **L3** Arbitrage | `layer3_runner.py` | `layer3_arbitrage_math/arbitrage_engine.py` | Continuous | Scans for mispricing via CVXPY LP, calculates optimal bets |
| **L4** Execution | `layer4_runner.py` | `paper_trading.py` / `live_trading.py` | Continuous | Executes trades, monitors positions, handles resolution |

All layers run as independent processes under `main.py` (supervisor). Communication is via JSON files. If a layer crashes, the supervisor restarts it.

### 2.2 Position Lifecycle

```
L3 finds opportunity → L4 opens position → monitors prices →
  ├─ Better opportunity found → position REPLACED (capital recycled, ~$0.001 cost)
  ├─ All markets in group resolve (price→1.0) → position CLOSED (profit realised)
  └─ Market still active → keeps monitoring (capital stays locked)
```

Key: positions are **never** closed by time expiry. Capital remains locked until markets actually resolve. This prevents phantom P&L on long-dated markets.

### 2.3 Resolution Validator (NEW — v0.03.00)

Before entering a position, L4 now optionally calls the Anthropic API to validate the market's actual resolution date against its rules text. This catches cases where the API `endDate` is misleading (e.g. shows "March 31" but rules say "December 31").

- **Module**: `resolution_validator.py`
- **Config**: `config.yaml` → `arbitrage.resolution_validation.enabled: true`
- **API key**: `config/secrets.yaml` → `resolution_validation.anthropic_api_key` *(moved out of config.yaml in v0.03.02)*
- **Entry filter**: `arbitrage.max_days_to_resolution: 60` — new positions are skipped if the market resolves >60 days away.
- **Replacement filter**: `arbitrage.max_days_to_replacement: 30` — when evaluating candidates to *replace* an existing position, only opportunities resolving within 30 days are considered. This is intentionally stricter than the entry filter: replacement should only occur when the incoming opportunity offers faster capital velocity. Because all entered positions already satisfy <60 days, and replacement candidates must satisfy <30 days, replacement scoring always favours near-term resolution over longer-dated alternatives.
- **Cache**: Results cached for 168 hours (1 week) per constraint group

### 2.4 Multi-Machine Execution Control

Only ONE machine may run L4 at a time. A Flask server manages leader election:

- **Server**: `execution_control.py` (port 5557)
- **Client**: `execution_control_client.py` (imported by L4)
- **Behaviour**: TTL-based lock with heartbeat. Fail-open (if server unreachable, allow execution).
- **Commands**: `exec_claim.sh status|claim|release`

### 2.6 VPS Deployment (NEW — v0.03.05)

A ZAP-Hosting Lifetime VPS runs an independent copy of the trading system:

- **Host**: 193.23.127.99 (ZAP-Hosting, Frankfurt/Eygelshoven, Germany)
- **Specs**: 4 vCPUs (AMD EPYC), 4GB RAM, 25GB NVMe, Ubuntu 24.04 x86_64
- **Cost**: $76 one-time (lifetime, no recurring fees)
- **Service**: `prediction-trader.service` (systemd, auto-start on reboot, Restart=always)
- **Dashboard**: http://193.23.127.99:5556
- **Exec Control**: http://193.23.127.99:5557 (API only, not browser-rendered)
- **Paths**: Repo at `/root/prediction-trader`, venv at `/root/prediction-trader-env`
- **Note**: Paths use `/root/` not `/home/andydoc/` — sed replacements applied to all runners
- **Caveat**: Must log into ZAP dashboard every 3 months or server suspended

**SSH access**: `ssh root@193.23.127.99`
**Monitoring**: `ssh root@193.23.127.99 'tail -f /root/prediction-trader/logs/layer4_$(date +%Y%m%d).log'`

### 2.7 Multi-Machine Operation

Currently both laptop and VPS run independently in SHADOW mode with separate execution states. The execution control server on each machine claims its own lock. To coordinate:
- **Both independent**: Each machine trades its own $100 paper capital (current state)
- **Coordinated**: Point one machine's `execution_control.url` to the other's `:5557` so only one trades at a time
- **Leader election**: The machine that claims the lock first becomes the active trader; the other monitors only

> See **§5.2** for mode-switching commands.

### 2.8 Trading Modes

| Mode | Config | Behaviour |
|------|--------|-----------|
| **PAPER** | `live_trading.enabled: false` | Simulated trades only |
| **SHADOW** | `live_trading.enabled: true, shadow_only: true` | Paper + validates against live orderbook |
| **LIVE** | `live_trading.enabled: true, shadow_only: false` | Real money via Polymarket CLOB |

---

## 3. File Structure

```
/home/andydoc/prediction-trader/              ← WSL (authoritative running code)
├── main.py                                   ← Supervisor: starts L1-L4 + dashboard
├── trading_engine.py                         ← v0.04.00+: Event-driven core (replaces L2+L3+L4)
├── paper_trading.py                          ← Paper trading engine (position lifecycle)
├── live_trading.py                           ← Live CLOB trading engine
├── resolution_validator.py                   ← AI-powered resolution date validation
├── dashboard_server.py                       ← Web dashboard (port 5556)
├── execution_control.py                      ← Multi-machine lock server (port 5557)
├── execution_control_client.py               ← Lock client (used by L4)
├── orderbook_depth.py                        ← Phase 5a: CLOB book depth analysis
├── websocket_manager.py                      ← Phase 6: WS market+user channels, local book mirror
├── layer1_runner.py                          ← L1 process entry
├── layer2_runner.py                          ← L2 process entry
├── layer3_runner.py                          ← L3 process entry
├── layer4_runner.py                          ← L4 process entry (orchestrates trading)
├── layer1_market_data/
│   ├── market_data.py                        ← MarketDataManager, PolymarketCollector
│   └── data/polymarket/latest.json           ← Current market snapshot (33k+ markets)
├── layer2_constraint_detection/
│   ├── constraint_detector.py                ← ConstraintDetector (mutex group finder)
│   └── data/latest_constraints.json          ← Detected constraint groups (regenerated)
├── layer3_arbitrage_math/
│   ├── arbitrage_engine.py                   ← CVXPY LP, Bregman, Frank-Wolfe
│   └── data/opportunities_*.json             ← Found opportunities (regenerated)
├── config/
│   ├── config.yaml                           ← All parameters (in git)
│   └── secrets.yaml                          ← Polymarket API keys (NOT in git)
├── data/system_state/
│   ├── execution_state.json                  ← Positions, capital, trade history
│   └── execution_lock.json                   ← Multi-machine leader lock
├── logs/
│   └── layer{1-4}_YYYYMMDD.log              ← Daily rotating logs per layer
├── scripts/                                  ← Operational scripts (see §6)
├── PROGRESS_ROADMAP.md                       ← This file
└── HEARTBEAT.md                              ← Agent instruction file

/home/andydoc/prediction-trader-env/          ← Python venv

# RETIRED: Windows mirror removed 2026-03-07 (WSL is authoritative, VPS is independent)

root@193.23.127.99:/root/prediction-trader/       ← VPS (ZAP-Hosting, independent copy)
root@193.23.127.99:/root/prediction-trader-env/    ← VPS Python venv
```

### Key Config Files

**config.yaml** (in git — safe, no secrets):
```yaml
arbitrage:
  capital_per_trade_pct: 0.10          # 10% of capital per position (floor=$10, cap=$1000)
  max_concurrent_positions: 20
  max_days_to_resolution: 60           # Entry filter: skip new positions resolving >60 days away
  max_days_to_replacement: 30          # Replacement filter: only consider candidates resolving <30 days (faster velocity)
  max_profit_threshold: 0.3            # Skip >30% (likely bad data)
  resolution_validation:
    enabled: true
    # anthropic_api_key: MOVED to secrets.yaml
    cache_ttl_hours: 168               # Cache AI results for 1 week
  fees:
    polymarket_taker_fee: 0.0001
live_trading:
  enabled: true
  shadow_only: true                    # SHADOW mode; set false for LIVE
```

**secrets.yaml** (NOT in git — contains private keys):
```yaml
polymarket:
  host: "https://clob.polymarket.com"
  chain_id: 137
  private_key: "0x..."
  funder_address: "0x..."
resolution_validation:
  anthropic_api_key: "sk-ant-..."      # Moved here from config.yaml in v0.03.02
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
# 1. Clone
cd ~ && git clone https://github.com/andydoc/Prediction-trading prediction-trader
mkdir -p ~/prediction-trader/data/system_state ~/prediction-trader/logs

# 2. Python environment
python3 -m venv ~/prediction-trader-env
source ~/prediction-trader-env/bin/activate
pip install pyyaml aiohttp requests numpy scipy cvxpy flask py-clob-client anthropic

# 3. Configure secrets
cp ~/prediction-trader/config/secrets.yaml.example ~/prediction-trader/config/secrets.yaml
nano ~/prediction-trader/config/secrets.yaml   # Add your Polymarket keys

# 4. Add Anthropic API key to config.yaml
# Edit config.yaml → resolution_validation.anthropic_api_key

# 5. Start
cd ~/prediction-trader && rm -f *.pid
nohup python main.py > logs/main.log 2>&1 &

# 6. Verify
curl -s http://localhost:5556/ | head -5   # Dashboard should return HTML
tail -20 logs/layer4_$(date +%Y%m%d).log   # Should show L4 activity
```

### Windows Auto-Start
Place `START_TRADER_HIDDEN.vbs` in:
`C:\Users\andyd\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup\`

---

## 5. Operating the System

### 5.1 Starting / Stopping
```bash
# Start (from Windows)
scripts\START_TRADER.bat              # With console window
scripts\START_TRADER_SILENT.bat       # Silent (for Task Scheduler)
scripts\START_TRADER_HIDDEN.vbs       # Hidden (for Startup folder)

# Stop (from Windows)
scripts\STOP_TRADER.bat               # Kill all layers

# Restart (from Windows)
scripts\RESTART_TRADER.bat            # Stop + git pull + restart

# Start (from WSL)
source ~/prediction-trader-env/bin/activate && cd ~/prediction-trader
rm -f *.pid && nohup python main.py > logs/main.log 2>&1 &

# Stop
wsl bash scripts/stop.sh             # Kill all
wsl bash scripts/stop.sh --dash      # Kill dashboard only
wsl bash scripts/stop.sh --l4        # Kill L4 only

# Restart (pulls latest git first)
wsl bash scripts/restart.sh          # Normal restart
wsl bash scripts/restart.sh --clean  # + purge stale L2/L3 data
```

### 5.2 Mode Switching
```bash
wsl bash scripts/mode.sh paper       # Paper trading (default)
wsl bash scripts/mode.sh shadow      # Paper + orderbook validation
wsl bash scripts/mode.sh live        # Real money (pre-flight checks)
wsl bash scripts/mode.sh stop        # Emergency: revert to paper + cancel orders
```

### 5.3 Monitoring
```bash
# Dashboard
http://localhost:5556

# Quick status
wsl bash scripts/status.sh           # PID check + capital
wsl bash scripts/status.sh --full    # Full health + P&L breakdown

# Tail logs
wsl tail -30 /home/andydoc/prediction-trader/logs/layer4_$(date +%Y%m%d).log

# Capital check
wsl python3 -c "import json; d=json.load(open('/home/andydoc/prediction-trader/data/system_state/execution_state.json')); print(f'Cap=\${d[\"current_capital\"]:.2f} Open={len(d[\"open_positions\"])} Closed={len(d[\"closed_positions\"])}')"
```

### 5.4 Multi-Machine Control
```bash
wsl bash scripts/exec_claim.sh status              # Who holds the lock?
wsl bash scripts/exec_claim.sh claim laptop         # Claim execution rights
wsl bash scripts/exec_claim.sh release --force      # Force-release
EXEC_CTRL_URL=http://<ip>:5557 scripts/exec_claim.sh claim laptop   # Remote
```

### 5.5 Git Sync
```bash
# Push from WSL
wsl bash scripts/sync.sh "commit message"

# Pull on VPS
ssh root@193.23.127.99 "cd /root/prediction-trader && git pull --ff-only origin main"
```

### 5.6 Recovery After Crash / Reboot
Auto-start is enabled via Windows Startup folder. If it fails:
1. Double-click `scripts/START_TRADER.bat`, OR
2. WSL: `source ~/prediction-trader-env/bin/activate && cd ~/prediction-trader && rm -f *.pid && nohup python main.py > logs/main.log 2>&1 &`
3. Wait 15s, check http://localhost:5556

All state persists in `data/system_state/execution_state.json`. No data is lost on restart.

### 5.7 Go-Live Checklist
1. Deposit $100+ USDC to Polymarket wallet
2. Verify: `python test_clob_connect.py`
3. Run shadow mode for 24h+ to validate orderbook fills
4. Switch: `scripts/mode.sh live`
5. Monitor dashboard Live tab and L4 logs
6. Emergency: `scripts/mode.sh stop`

---

## 6. Roadmap

### Phase 1 — Core Stability (CURRENT)
- [x] Four-layer pipeline (L1-L4) with supervisor
- [x] Paper trading with position lifecycle
- [x] Dashboard with Paper/Shadow/Live/Control tabs
- [x] Multi-machine execution control
- [x] L1 full pagination (33k+ markets)
- [x] L3 polytope mutex guard
- [x] Resolution validator (Anthropic API)
- [x] Remove time-based expiry (positions held until resolution)
- [x] Max days-to-resolution filter (60 days)
- [x] **Anthropic API key configuration** — Key configured, validator making successful API calls
- [x] **Verify resolution validator in production** — Confirmed: catches Somaliland (392d), unrepresented outcomes (6+ markets blocked)
- [x] **24h replacement protection** — Positions resolving within 24h protected from replacement (uses AI-validated dates)

### Phase 2 — Go Live
- [ ] Refactor `paper_trading.py` → PositionManager + TradingExecutor (see §6.1)
- [ ] Pre-trade validation: walk fresh books at execution time (see §6.2)
- [ ] negRisk order placement: pass `negRisk: true` flag for negRisk markets
- [ ] Deposit $100+ USDC to Polymarket wallet
- [ ] Run shadow mode 24h+ for orderbook validation
- [ ] First live trade
- [ ] Live P&L tracking on dashboard
- [ ] Position reconciliation (CLOB vs paper)

### Phase 3 — Reliability & Monitoring
- [x] **Supervisor double-instance bug fixed** — `check_pid_lock()` added to main.py; stale PID files cleaned on startup
- [ ] Wire dashboard control panel (mode switch, parameter save, restart via API)
- [ ] WhatsApp notifications via OpenClaw (trade alerts, errors, daily summary)
- [ ] API key auth for execution_control.py (before exposing to public IP)
- [ ] Oracle Cloud VPS for execution control server
- [ ] File structure reorganisation (runners into layer dirs, L4 modes into sub-folder, services area for dashboard/exec-ctrl) — **Phase 3 cleanup only, do not refactor during active trading**

### Phase 4 — Scale & Optimise
- [ ] Increase capital_per_trade from 10% to 30-50% (backtested)
- [ ] Historical performance analytics on dashboard
- [ ] Multi-exchange support (Kalshi)
- [ ] VPS migration for 24/7 operation
- [ ] Git versioning: branches for features, tags for releases (see §8)

### Phase 5 — Risk Management (designed 2026-03-08, not yet implemented)

#### Risk 1: Order Book Depth / Liquidity
**Problem:** System enters arbs based on price without checking if the book can absorb the trade.
**Solution — Pre-trade depth check:**
1. Fetch CLOB order book for every leg before entry/replacement
2. Calculate effective depth at 80% of reported book (phantom order haircut)
3. Min depth across all legs sets max trade size: `trade_size = min(10% capital, min_leg_depth)`
4. If min depth < floor ($5), skip the opportunity entirely
5. Place orders at furthest price within usable depth (not best ask)
6. Live trading: use FAK (Fill And Kill) order type — fills available, cancels rest
   (Confirmed: all Polymarket orders are limit orders, FAK is a supported `time_in_force`)

**Partial fill handling (score-based):**
- After submission, check actual fills on all legs
- Calculate arb score of actual filled position
- If score ≥ threshold → keep everything (imbalanced position is acceptable)
- If score < threshold → unwind minimum needed on overfilled leg(s) to restore score
- If no partial retention produces acceptable score → full unwind all legs
- Key: the score is the arbiter, not a "must match exactly" rule

#### Risk 2: Scaling / Thin Markets
**Problem:** As capital scales, profitability may decline due to thin books. Current config masks this.
**Solution — Config changes:**
- `initial_capital`: 100 → 10000
- `max_capital`: 100 → 10000
- `max_positions`: remove cap (replace with soft log warning at 50)
- `capital_per_trade_pct`: stays at 0.10 (10%)

**Shadow trading honesty:**
- Cap simulated fills at 80% of actual book depth per leg (ties to Risk 1)
- If system wants $1000 but book only supports $80, shadow fill = $64 (80% of $80)
- Log "depth-limited" trades separately to measure how often depth constrains deployment
- This reveals real capital deployment ceiling before going live

#### Risk 3: Replacement Chain Capital Lockup
**Problem:** Replacement scoring treats each trade in isolation, ignoring cumulative capital lockup.
**Agreed approach — HYBRID (forward-looking decisions, chain-aware reporting):**

**For DECISIONS (replacement logic):** Forward-looking scoring only.
- Sunk time is sunk — only ask "what's the best use of this capital from now?"
- Prevents irrational holding of degrading positions due to chain penalty
- Capital velocity maximised by marginal return decisions

**For REPORTING (analytics):** Track full chain history.
- `chain_id`: UUID, inherited through replacements
- `chain_start_time`: timestamp of first entry in chain, inherited
- `chain_cumulative_fees`: running total of all fees in chain
- True return = `(payout - total_fees) / chain_duration` — for performance analysis only
- If chains consistently drag returns → tighten entry criteria, not replacement criteria

**Chain splitting on partial replacement (reporting only):**
- If replacing A→B but only partially unwind A due to liquidity:
  - Residual A: keeps chain_id, tracked separately until A resolves
  - New B: inherits chain_id + chain_start_time (capital locked since original entry)

#### Implementation Sequence
- [x] **5a** — Order book depth infrastructure: CLOB book fetching, 80% depth calc (**orderbook_depth.py** — tested, working)
- [ ] **5a.2** — Integrate depth check into L4 entry/replacement flow + log depth-limited trades
- [ ] **5a.3** — Display depth info on dashboard
- [ ] **5b** — Shadow trading honesty: config to $10k, remove max_positions, cap shadow fills at book depth
- [ ] **5c** — Partial fill handling: score-based unwind logic, minimum unwind calc, fill metadata
- [ ] **5d** — Replacement chain tracking (reporting): chain_id/start_time/fees in metadata, dashboard view
- [ ] **5e** — FAK live trading (future): FAK time_in_force, real fill checking, live unwind

### Phase 6 — WebSocket Integration (designed 2026-03-08)

**Goal:** Replace REST polling with persistent WebSocket connections for price data, orderbook depth, fill confirmation, and resolution detection. Reduces latency and slippage across all L3 opportunities (direct LP, Bregman/FW polytope — math-path-agnostic).

**Module:** `websocket_manager.py` (NEW — created 2026-03-08)

**Architecture:**
- Two persistent WS connections: market channel (public, no auth) + user channel (auth required)
- Market channel: `wss://ws-subscriptions-clob.polymarket.com/ws/market`
  - Events: `book` (full snapshot), `price_change` (incremental), `best_bid_ask`, `last_trade_price`, `market_resolved`
  - Subscribes to asset_ids (CLOB token IDs) for all markets in active L2 constraint groups
  - Maintains local orderbook mirror (dict[asset_id] → LocalOrderBook) queryable by L3/L4
- User channel: `wss://ws-subscriptions-clob.polymarket.com/ws/user`
  - Events: `trade` (MATCHED→CONFIRMED lifecycle), `order` (placement/cancellation)
  - Subscribes by condition_id for all open positions
  - Provides instant fill confirmation (replaces REST `GET /orders` polling)
- Auto-reconnect with exponential backoff (1s→60s)
- PING heartbeat every 10s (Polymarket requirement)
- Dynamic subscription: add/remove assets without reconnecting
- Callback registration for L3/L4 consumers

**Integration Points:**

| Consumer | Current (REST) | New (WS) | Benefit |
|----------|---------------|----------|---------|
| L3 price monitoring | 30s stale snapshot via `latest_markets.json` | Live `price_change`/`best_bid_ask` callbacks | Instant mispricing detection |
| L4 pre-trade depth | REST `GET /book` per leg (Phase 5a) | Local book mirror from WS `book` events | Zero-latency depth check |
| L4 fill confirmation | REST `GET /orders` polling | WS user `trade` events | Instant fill status |
| L4 resolution detection | L1 polling + price→1.0 check | WS `market_resolved` event | Instant resolution trigger |
| orderbook_depth.py | REST fetch per check | Query `ws_manager.get_book(asset_id)` | Eliminates REST calls |

**Implementation Sequence:**
- [x] **6a** — Core `websocket_manager.py`: WebSocketManager class, market+user channel loops, local book mirror, callback system, auto-reconnect, heartbeat, dynamic subscription (**DONE** 2026-03-08)
- [x] **6b** — L4 integration: start WS manager in `layer4_runner.py`, subscribe open position assets, register resolution callback, register fill confirmation callback, user auth from live engine creds, periodic subscription refresh, WS stats in log output (**DONE** 2026-03-08)
- [x] **6c** — L3→WS price bridge: L4 writes `data/ws_prices.json` (market_id→{Yes, No} actual prices from WS), L3 overlays live prices onto MarketData before scanning. Uses ACTUAL No prices (not 1-Yes). Resolved assets pruned from bridge via `market_resolved` event. (**DONE** 2026-03-08, fixed 2026-03-09)
- [ ] **6d** — orderbook_depth.py WS mode: add `get_depth_from_ws(ws_manager, asset_id)` path; fall back to REST if WS book stale (>30s)
- [ ] **6e** — Dashboard: WS connection status, message rates, subscription count, book staleness indicators
- [ ] **6f** — Config: add `websocket:` section to `config.yaml` with enable/disable, URLs, heartbeat interval

**Config (planned):**
```yaml
websocket:
  enabled: true
  market_channel_url: wss://ws-subscriptions-clob.polymarket.com/ws/market
  user_channel_url: wss://ws-subscriptions-clob.polymarket.com/ws/user
  heartbeat_interval: 10
  reconnect_base_delay: 1.0
  reconnect_max_delay: 60.0
  max_book_staleness_secs: 30    # Fall back to REST if WS book older than this
```

---

### Phase 7 — Architecture Evolution (designed 2026-03-09)

#### §6.1 PositionManager / Executor Refactor
**Problem:** `paper_trading.py` bundles position tracking with simulated execution. Live trading requires real fill prices, not theoretical.
**Solution:** Split into:
- **PositionManager** — tracks positions, capital, state, resolution, P&L. `record_entry(opp, fills)` takes actual fill prices.
- **TradingExecutor** (abstract) → PaperExecutor, ShadowExecutor, LiveExecutor
- Trading engine calls: `fills = executor.execute(opp)` → `position_manager.record_entry(opp, fills)`
**Status:** Design complete, implementation deferred to Phase 2 (go-live prerequisite)

#### §6.2 Pre-trade Validation
**Problem:** Between arb detection and execution, the book may have changed. Multi-leg arbs are especially vulnerable.
**Solution — Two-phase commit:**
1. **Detection** (current): WS price events → constraint eval → arb math → candidate opportunity
2. **Validation** (new, pre-execution): For each candidate, re-read local book mirror for all legs, walk at trade size, compute actual VWAP execution cost vs expected profit. If any leg book >5s stale, REST fallback for that leg only. Abort if `real_profit / expected_profit < 0.7` or insufficient depth.
**Status:** Design complete, implementation as part of Phase 2

#### §6.3 Rust Port (Performance)
**Bottleneck:** Arb math in Python (~80ms/constraint). p50 queue→eval latency stuck at ~8s due to batch processing time.
**Pragmatic path:** PyO3/maturin extension for hot path only:
- `_arb_mutex_direct()` — price sum + bet sizing (10-20× speedup potential)
- `frank_wolfe_optimal_bets()` — iterative LP (50-100× with SIMD)
- Keep Python for everything else (WS, state, orchestration)
**Expected impact:** p50 latency from ~8s → <1s
**Status:** Design complete, implementation deferred (high effort, not blocking shadow trading)
**Future comparison:** Approach 4 (Weighted Book Distance) queue metric should be paper-traded against current Approach 1 (EFP) once Rust speeds up eval enough to handle higher queue volume.

#### §6.4 SQLite Migration
**Problem:** JSON state files are not atomic, don't support partial reads, and grow linearly with closed positions.
**Proposed schema:**
```sql
markets (market_id PK, data JSON, updated_at)
constraints (constraint_id PK, data JSON, updated_at)
positions (position_id PK, status, data JSON, opened_at, closed_at)
trades (trade_id PK, position_id FK, data JSON, executed_at)
price_history (asset_id, timestamp, bid, ask, efp)
```
**Migration order:** execution_state.json first (biggest pain point) → markets → constraints
**Status:** Design complete, implementation deferred (not blocking, incremental migration possible)

#### §6.5 negRisk Execution
**Context:** Polymarket negRisk contracts reduce collateral for mutex groups. All our constraint groups ARE negRisk (detected via `negRiskMarketID`).
**Key rules:**
- Buy arbs (sum < 1.0): Do NOT use negRisk — use standard markets (lower cost = the profit)
- Sell arbs (sum > 1.0): negRisk reduces collateral from sum(NO asks) to $1.00 per unit
- Orders must pass `negRisk: true` to CLOB API for negRisk markets
- Different contract addresses (NegRiskCtfExchange vs standard CTFExchange)
**Implementation:**
- [x] **7a** — negRisk metadata tag in arb opportunities (`metadata.neg_risk: true/false`)
- [x] **7b** — negRisk flag passed to CLOB `create_order()` via `PartialCreateOrderOptions`
- [ ] **7c** — Sell arb capital calculation rework for negRisk (collateral = $1.00, not sum(NO))
- [ ] **7d** — Shadow validation: compare negRisk vs standard fill simulation
**Status:** 7a+7b done, 7c+7d next

### Phase 8 — Latency Optimization (designed 2026-03-09)

**Root Cause Analysis (2026-03-09 bottleneck audit):**
Rust arb math port achieved 19,000× speedup (80ms→4.2µs) but total system latency barely improved.
Root cause: arb math was only 8% of wall time. The other 92% is:
- **GIL contention (80%)**: ~1,700 WS callbacks/sec each grab the GIL, starving the eval thread. 100 evals take 3s wall time despite ~10ms CPU.
- **`asyncio.sleep(1.0)` (10%)**: Even urgent evals wait up to 999ms before processing.
- **Exec lock HTTP (2%)**: 2 synchronous HTTP round-trips per iteration (12.9ms), blocking the event loop.

**Measured evidence:**
| Metric | Value |
|--------|-------|
| Eval batch wall time p50 | 3,026ms (for ~10ms of actual work) |
| Gap between batches p50 | 3,263ms |
| Background queue steady state | ~1,400 items (never drains) |
| Exec lock HTTP total | 82s over session (12,780 calls) |
| Batches finding arb | 33.9% (66% of eval CPU wasted) |

#### P0 — Quick Python Fixes (hours)
- [ ] **8a** — Replace `asyncio.sleep(1.0)` with `asyncio.Event`-based wake (instant urgent processing, 50ms fallback)
- [ ] **8b** — Cache exec lock status (check every 30s, not every iteration)
- [ ] **8c** — Increase `MAX_EVALS_PER_BATCH` from 100 to 500
- [ ] **8d** — Remove `indent=2` from JSON state serialization (2.2MB → ~700KB, 2-3× faster writes)

#### P1 — SQLite State (hours)
- [ ] **8e** — SQLite in-memory DB + WAL journal for `execution_state`, periodic `db.backup()` to disk
- [ ] **8f** — Incremental position updates (INSERT/UPDATE single rows, not rewrite entire file)

#### P2 — Rust Bregman + Polytope Reintroduction (1-2 days)
- [ ] **8g** — Port Bregman KL projection to Rust (iterative Dykstra, ~100µs vs CVXPY 80ms)
- [ ] **8h** — Port polytope construction to Rust (combinatorics only)
- [ ] **8i** — Reintroduce polytope check for mutex constraints where direct check found no arb (partial hedges)

#### P3 — Reduce Python↔Rust Boundary Crossings (hours)
- [ ] **8j** — Batch EFP computation: single `Vec<(asset_id, asks)>` → Rust → `Vec<(asset_id, efp, drift)>`
- [ ] **8k** — Move constraint index (`asset_to_constraints`) into Rust `DashMap`
- [ ] **8l** — WS stale-asset re-subscribe sweep (replaces REST fallback plan — WS re-sub is 10ms vs REST 200ms)

#### P4 — Full Rust Engine (1-2 weeks)
- [ ] **8m** — Rust `tokio-tungstenite` WS client with local book mirror (replaces Python `websockets`)
- [ ] **8n** — Rust eval queue with `tokio::select!` instant wake (no polling, no sleep)
- [ ] **8o** — Rust `rusqlite` state persistence (WAL mode, incremental updates)
- [ ] **8p** — Single Rust binary: WS + queue + eval + state. Python kept for dashboard + resolution validator only.
- [ ] **8q** — Full Rust port: dashboard (axum/warp), resolution validator, everything. Zero Python.

**Expected latency after each phase:**
| Phase | p50 | p95 | bg_queue |
|-------|-----|-----|----------|
| Current | 2-6s | 60-300s | ~1400 (growing) |
| After P0 | ~200ms | ~2s | ~500 (draining) |
| After P1 | ~200ms | ~1s | ~200 |
| After P2 | ~150ms | ~800ms | ~100 (polytope adds load but Rust handles it) |
| After P3 | ~50ms | ~300ms | ~50 |
| After P4 | <1ms | <5ms | 0 (instant processing) |

---

### v0.04.03-dev (2026-03-09) — Latency Bottleneck Analysis + Phase 8 Plan
- **ANALYSED** Full bottleneck audit: Rust arb math (19000×) only addressed 8% of wall time; GIL contention is 80%
- **MEASURED** Eval batch p50=3026ms for ~10ms CPU work (300× overhead from GIL + sleep + HTTP)
- **MEASURED** Exec lock HTTP overhead: 12.9ms/iter, 82s total, 12780 calls
- **MEASURED** Background queue permanently at ~1400 items, eval throughput ~17/sec (need ~500/sec)
- **MEASURED** 66.1% of eval batches find zero arbs (wasted CPU on non-arb constraints)
- **ADDED** Phase 8 (Latency Optimization) to roadmap: P0-P4 progressive plan from Python fixes → full Rust port
- **ADDED** `scripts/debug/batch_gaps.py`, `batch_exec_time.py`, `http_overhead.py` — bottleneck profiling scripts

### v0.04.02 (2026-03-09) — EFP Queue Metric + negRisk Tagging + Latency Instrumentation
- **ADDED** Effective Fill Price (EFP) as 2D queue metric: VWAP at trade size captures both price AND depth drift
- **ADDED** `LocalOrderBook.effective_fill_price(trade_size_usd)` — walks ask book to compute execution cost
- **ADDED** Priority queue: urgent (EFP drift >0.5c) processed first, background (>5s stale) fills remaining
- **ADDED** Real latency instrumentation: p50/p95/max reported in stats line from queue_time→eval_time
- **ADDED** negRisk metadata tag (`metadata.neg_risk`) in all arb opportunity types
- **ADDED** negRisk flag on CLOB order placement via `PartialCreateOrderOptions(neg_risk=True)`
- **CHANGED** Queue uses cumulative EFP drift from last eval, no per-constraint cooldown
- **MEASURED** p50=5-10s (stable), p95=170-470s (background starvation), bottleneck is Python arb math ~80ms/eval
- **ADDED** Phase 7 (Architecture Evolution) to roadmap: PositionManager refactor, pre-trade validation, Rust port, SQLite, negRisk execution

### v0.04.01 (2026-03-09) — Threaded Arb Eval + WS Stability
- **ADDED** `ThreadPoolExecutor` (2 workers) for CPU-bound arb evaluation — asyncio event loop stays free for WS heartbeats
- **ADDED** `MAX_EVALS_PER_BATCH = 100` — processes at most 100 constraints per loop iteration, defers rest to next tick
- **CHANGED** Constraint detection + index building runs in thread pool during startup and rebuilds
- **CHANGED** `ASSETS_PER_CONNECTION` reduced from 4000 to 2000 — smaller shards are more stable
- **CHANGED** `initial_dump` set to `True` — full orderbook snapshot on WS subscribe for accurate depth data
- **RESULT** Zero WS disconnects over 8+ minutes (was 8+ disconnects per 8 min before). 821k msgs in 8 min.

### v0.04.00 (2026-03-08/09) — Event-Driven Trading Engine Refactor
- **CREATED** `trading_engine.py` — replaces L2+L3+L4 with single async event-driven process
- **ARCHITECTURE** Two-process system: Market Scanner (`layer1_runner.py`) + Trading Engine (`trading_engine.py`)
- **ADDED** Bid/ask spread-aware arb math: `MarketData.outcome_bids`, `outcome_asks`, `get_entry_price()`, `get_exit_price()`
- **ADDED** Arb engine uses actual ask prices (entry cost) instead of midpoints; sell arbs use real NO ask prices from book
- **ADDED** `asset_to_market` reverse lookup: `asset_id → (market_id, token_index)` for instant WS→MarketData price updates
- **ADDED** `has_live_prices()` gate — constraints only evaluated when all markets have live WS bid/ask data
- **ADDED** WS callbacks update `MarketData` in-place: `price_change`/`book` → `outcome_bids`/`outcome_asks` directly
- **ADDED** WS sharded connection pool: N connections × 2000 assets each, prevents server-side data flood
- **ADDED** `on_new_market` WS callback → buffers new markets for batch constraint rebuild every 10 min
- **ADDED** `calc_position_liq_value()` uses bid prices (exit) for liquidation valuation
- **MODIFIED** `main.py` supervisor: starts Market Scanner + Trading Engine (no more L2/L3/L4 separate processes)
- **MODIFIED** `arbitrage_engine.py`: `_arb_mutex_direct()` and `_arb_via_polytope()` use `get_entry_price()` for asks
- **MODIFIED** `layer1_market_data/market_data.py`: added `outcome_bids`, `outcome_asks` fields + helper methods
- **KEPT** Old `layer2_runner.py`, `layer3_runner.py`, `layer4_runner.py` as reference (not used by supervisor)

### v0.03.06 (2026-03-08/09) — WebSocket Integration (Phase 6a+6b+6c)
- **ADDED** `websocket_manager.py` — persistent WebSocket connections to Polymarket market + user channels
- **ADDED** Local orderbook mirror: `LocalOrderBook` with bid/ask levels, depth calculations, staleness tracking
- **ADDED** Market channel: `book`, `price_change`, `best_bid_ask`, `last_trade_price`, `market_resolved` events
- **ADDED** User channel: `trade` (MATCHED→CONFIRMED) and `order` events for fill confirmation
- **ADDED** Dynamic subscription: add/remove asset_ids without reconnecting (Polymarket subscription limit removed)
- **ADDED** Auto-reconnect with exponential backoff (1s→60s), PING heartbeat every 10s
- **ADDED** Callback system: `on_price_change`, `on_book_update`, `on_trade_confirm`, `on_market_resolved`
- **ADDED** Convenience functions: `get_asset_ids_for_constraints()`, `get_condition_ids_for_positions()`
- **INTEGRATED** L4 runner: WS manager starts with L4, subscribes open position + L3 opportunity assets, fires `market_resolved` callback for instant resolution detection, logs trade confirmations
- **INTEGRATED** User channel auth derived from live engine's CLOB API creds (when live engine available)
- **INTEGRATED** Periodic subscription refresh (every 120s), WS stats in status log every 10th iteration
- **INTEGRATED** New position asset subscription on trade entry
- **ADDED** `websocket:` config section in config.yaml (enabled, URLs, heartbeat, staleness)
- **ADDED** Phase 6 section to PROGRESS_ROADMAP.md with full integration plan
- **ADDED** Phase 6c: WS price bridge — L4 writes `data/ws_prices.json` with both Yes AND No actual WS prices per market; L3 reads and overlays onto MarketData before scanning
- **ADDED** `export_price_cache()` in websocket_manager.py — exports all local book prices for bridge
- **ADDED** `build_token_to_market_map()` — reverse map: CLOB token_id → (market_id, token_index)
- **ADDED** `overlay_ws_prices()` in layer3_runner.py — reads bridge file, applies live prices to MarketData, skips stale (>30s)
- **FIXED** L3 now uses ACTUAL No prices from WS (was incorrectly computing `No = 1 - Yes`, which is wrong due to bid-ask spread)
- **ADDED** Resolved market pruning: `market_resolved` WS event removes asset from local book mirror + excludes from price bridge export
- **ADDED** `_resolved_assets` tracking set in WebSocketManager; resolved count in stats output

### v0.03.05 (2026-03-06/07) — Resolution Delay Model (Dynamic) + VPS Deployment
- **ADDED** Resolution delay scoring model in `layer4_runner.py`. Scoring formula now uses `effective_hours = raw_hours + P95_category_delay + volume_penalty` instead of raw hours.
- **ADDED** Dynamic P95 table loaded from `data/resolution_delay_p95.json` at runtime (1h cache). Falls back to hardcoded values if file missing.
- **ADDED** Rolling 12-month P95 window — captures seasonal variation in sports/politics. Temporal analysis showed Polymarket resolution speed improved dramatically (all-category P95: 161h in 2024-H1 → 24h in 2026-H1).
- **ADDED** `scripts/debug/compute_delay_table.py` — computes 12-month rolling P95 from harvest data, writes JSON.
- **ADDED** `scripts/debug/update_delay_table.py` — weekly updater: harvests last 30 days from Gamma API, appends to master file, recomputes P95 table. Triggered automatically by L4 at startup and every ~24h (iteration 2880).
- **ADDED** `classify_opportunity_category()` — classifies opportunities into delay categories from market names/descriptions.
- **ADDED** `get_volume_penalty_hours()` — soft penalty: `max(0, (5 - log10(vol+1)) * 2)`. Adds ~6h for $100 vol, ~2h for $10K, ~0 for $100K+.
- **ADDED** `get_min_volume()` — uses minimum volume_24h across all markets in an opportunity.
- **ADDED** Same delay model applied to replacement scoring.
- **DEPLOYED** ZAP-Hosting Lifetime VPS (193.23.127.99) — 4 cores, 4GB RAM, 25GB NVMe, Ubuntu 24.04. Full system running with $100 fresh paper capital. Dashboard at :5556, exec control at :5557. Systemd service with auto-restart on reboot.
- **ADDED** `scripts/cloud_init_oracle.sh` and `scripts/cloud_init_oracle_v2.sh` — Oracle Cloud ARM A1 cloud-init scripts.
- **ADDED** `scripts/setup_vps.sh` — manual VPS setup script.
- **ADDED** `scripts/setup_remote_vps.sh` — remote setup from laptop via SSH.
- **ADDED** `scripts/oci_retry_create.sh` — Oracle Cloud ARM instance auto-retry (capacity usually exhausted).
- **NOTE** VPS uses `/root/` paths (not `/home/andydoc/`) — sed replacements applied to all runner files on VPS only (not committed to git).
- **NOTE** ZAP-Hosting requires dashboard login every 3 months to avoid suspension.
- **DATA** Harvested 512,894 resolved markets from Gamma API to D:\ClaudeData\resolved_markets_harvest.jsonl (731MB).
- **FINDING** Resolution delays are NOT static — Polymarket got ~6x faster between 2024-H1 and 2025-H2.
- **FINDING** Football resolution delay is ~5h median, consistent across leagues. Volume affects tail risk not median.
- **FINDING** South American low-volume leagues responsible for stuck positions — low UMA oracle incentive.
- **12-month P95 values** (as of 2026-03-06): football 15.1h, us_sports 44.4h, esports 12.5h, crypto 3.4h, politics 702.4h, sports_props 10.1h, mma_boxing 26.1h, gov_policy 41.2h, other 35.1h

### v0.03.04 (2026-03-04) — Sell Arb Payout Formula Fix
- **FIXED** Critical payout formula error in `paper_trading.py` for sell arb positions (`mutex_sell_all`). Previously, payout was calculated the same way as buy arb — using only the *winning* market's YES shares — which is wrong for sell arb. In a sell arb, the system buys NO on every market. When the outcome is known, the winning market's NO bet *loses* (outcome was YES), and every other market's NO bet *wins* (outcome was NO, paying $1/share). Corrected formula: `payout = sum(bet_amount_k / (1 - entry_price_k))` for all legs `k` except the winning market.

### v0.03.03 (2026-03-04) — Replacement Filter Tightening & Restart Fix
- **ADDED** `max_days_to_replacement: 30` config param — replacement candidates must resolve within 30 days (stricter than the 60-day entry filter). Wired into `rank_opportunities()` call and AI validation check inside replacement loop.
- **FIXED** `restart.sh` awk syntax error (`{print $2, $NF}` eaten by Windows batch); replaced with `tr -s ' ' | cut -d' ' -f2,11`. Also: `>` → `>>` to append main.log on restart, added `disown`, added capital summary readout.

### v0.03.02 (2026-03-03) — Replacement Loop Fix & Fee Config
- **FIXED** Replacement loop bug: same opportunity could liquidate multiple positions in one round (T20 World Cup replaced 5 positions at once). Added `used_opp_cids` set to prevent reuse within a round.
- **FIXED** Fee config mismatch: `paper_trading.py` read `polymarket_taker_fee` from wrong config section (always fell back to hardcoded default). Now reads from `arbitrage.fees.polymarket_taker_fee` via `self.taker_fee` set during init.
- **FIXED** Performance counters: reset stale `total_trades` (was 1304, now matches actual position count of 147)
- **MOVED** 57 one-off investigation scripts to `scripts/debug/` (gitignored)
- **NOTED** Caracas FC churn: position replaced/re-entered 14+ times due to score hovering near replacement threshold — not a bug, but highlights need for hysteresis or cooldown on re-entry

### v0.03.01 (2026-03-03) — Replacement Protection & Validator Verification
- **ADDED** 24h replacement protection: positions whose AI-validated resolution date is <24h away are immune from replacement scoring (prevents last-minute swaps before payout)
- **FIXED** Replacement scoring now uses AI-validated resolution date from cache (not raw API `end_date`) for the 24h protection window
- **VERIFIED** Resolution validator working in production: Anthropic API returning 200s, caching results, catching long-dated markets and unrepresented outcomes
- **VERIFIED** 23:59:59 timestamp handling already correct in both cached and fresh code paths
- **IDENTIFIED** Arkansas Governor Democratic Primary: 2 positions ($0.89 phantom) from pre-validator `_expire_position()` era — same class as Somaliland
- **IDENTIFIED** TX-31 Republican Primary: 2 expired positions ($0.85 phantom) + 1 replaced — pre-validator era, "Other" outcome unrepresented
- **CONFIRMED** Trump Truth Social position is legitimate: L2 correctly narrowed to 3 remaining live brackets (160-179, 180-199, 200+) covering all possible outcomes, ~3.5% arb

### v0.03.00 (2026-03-03) — Resolution Safety
- **REMOVED** `_expire_position()` — positions no longer close by time; capital stays locked until markets resolve
- **ADDED** `_check_group_resolved()` — checks all markets in group for price→1.0 before closing
- **ADDED** `resolution_validator.py` — Anthropic API call to validate true resolution dates from rules text
- **ADDED** `max_days_to_resolution: 60` config filter — skips far-dated opportunities
- **FIXED** L1 pagination: removed 10k market cap, now fetches all (~33k+)
- **FIXED** L3 polytope path: added mutex completeness guard (price_sum < 0.90)
- **CLEANED** Japan unemployment positions (markets 1323418-1323422) — $10 loss from incomplete mutex
- **CLEANED** Somaliland parliamentary positions (markets 948391-948394) — $0.93 phantom profit from false expiry

### v0.02.00 (2026-02-28) — Dashboard & Scripts
- Dashboard tabs: Paper/Shadow/Live/Control Panel
- Score column (profit_pct/hours * 10000)
- Mode badge with colour coding
- Script rationalisation: 21→8 scripts
- Naming standardisation across codebase
- Git repository established

### v0.01.00 (2026-02-17) — Initial System
- Four-layer pipeline with supervisor
- Paper trading engine with position lifecycle
- CVXPY LP arbitrage detection
- Multi-machine execution control
- CLOB integration and shadow mode

---

## 8. Git Versioning (TODO)

Current state: all work on `main` branch, no tags. Plan:

### Branch Strategy
- `main` — stable, tested code only
- `dev` — active development
- `feature/<name>` — individual features (e.g. `feature/resolution-validator`)
- `hotfix/<name>` — urgent production fixes

### Tag Convention
- Format: `vMAJOR.MINOR.PATCH` with zero-padded 2-digit minor/patch — e.g. `v0.03.02`
- `v0.01.00` → initial four-layer system
- `v0.02.00` → dashboard and scripts  
- `v0.03.00` → resolution safety
- `v0.03.01` → replacement protection & validator verification
- `v0.03.02` → replacement loop fix + fee config + API key to secrets.yaml
- `v0.03.03` → separate replacement filter (30d) from entry filter (60d)
- `v0.03.04` → sell arb payout formula fix + restart.sh fixes *(current)*
- `v1.00.00` → first successful live trade

### Implementation Steps
```bash
# Tag existing milestones retrospectively
git tag -a v0.01.00 <initial-commit-hash> -m "Initial four-layer system"
git tag -a v0.02.00 <dashboard-commit-hash> -m "Dashboard and scripts"
git tag -a v0.03.00 HEAD -m "Resolution safety"
git push origin --tags

# Create dev branch
git checkout -b dev
git push -u origin dev
```

---

## 9. Incident Log

### INC-004: TX-31 Republican Primary (2026-03-03)
**Markets**: 704392-704394 (Carter, Gomez, Hamden — 3 of 4+ outcomes, missing "Other")
**Impact**: $0.85 phantom profit (not yet cleaned)

**What happened**: Constraint `mutex_0x71f93b23...` contained only 3 named candidates. Rules state: resolves to "Other" if no nominee by Nov 3, 2026. No "Other" market exists. Three positions were opened Feb 21-24 (pre-validator): position [71] was replaced by [380] (normal), then [380] and [465] were both expired by old `_expire_position()` crediting phantom profit on unresolved markets.

**Root cause**: Same dual issue as Somaliland — time-based expiry + no unrepresented outcome detection. All positions predate the validator.

**Status**: Identified, not yet cleaned. Will be handled at capital reset.

### INC-003: Arkansas Governor Democratic Primary (2026-03-03)
**Markets**: 824818-824819 (Love, Xayprasith-Mays — 2 of 3+ outcomes, missing "Other")
**Impact**: $0.89 phantom profit (not yet cleaned)

**What happened**: Primary scheduled for March 3, 2026. Rules mention "Other" outcome and "run-off" scenario — no corresponding markets. Two positions opened Feb 22-27 (pre-validator) and expired by old `_expire_position()` days before the actual primary.

**Root cause**: Same as TX-31 and Somaliland — time-based expiry on unresolved markets with unrepresented outcomes.

**Status**: Identified, not yet cleaned. Will be handled at capital reset.

### INC-002: Somaliland Parliamentary Election (2026-03-03)
**Markets**: 948391-948394 (4 outcomes + "no election")
**Impact**: $0.93 phantom profit credited (cleaned)

**What happened**: System entered a mutex arb on Somaliland parliamentary election markets. The API `endDate` was 2026-03-31 (election scheduled date), but the rules text says resolution could be as late as 2026-12-31 (no election) or 2027-03-31 (results unknown). After 28 hours, the old `_expire_position()` method closed the position and credited the expected profit as actual profit — even though the market is still live.

**Root cause**: Two compounding issues: (1) Time-based expiry credited phantom profit on unresolved markets; (2) No validation that API `endDate` reflects the true latest resolution date from the rules.

**Fixes**: Removed `_expire_position()` entirely; added `_check_group_resolved()` that requires all markets to show price→1.0 before crediting profit; added `resolution_validator.py` (Anthropic API) to check true dates; added `max_days_to_resolution: 60` filter.

### INC-001: Japan Unemployment — Incomplete Mutex (2026-03-03)
**Markets**: 1323418-1323422 (5 of 7 outcomes)
**Impact**: $10.00 loss (all 5 positions lost)

**What happened**: L1 had a hard cap of 10,000 markets. Polymarket had ~33,800. The missing 2 markets (2.6% and ≥2.7%) fell beyond the cutoff. L2 saw only 5 of 7 outcomes (sum=0.889, passed 0.85 guard). L3 direct path blocked it (0.90 guard) but Bregman/FW polytope fallback had no guard — treated 5 incomplete markets as valid arbitrage. Result was 2.7%, which was in the missing markets.

**Fixes**: L1 now paginates fully (33k+ markets); L3 polytope path has mutex completeness guard at 0.90.

---

## 10. Performance

### Current State (2026-03-04, v0.03.04)
Live figures from `execution_state.json` (post sell-arb payout correction):
- **Cash (current_capital)**: $1.24 | **Deployed**: $100.00 | **Total**: $101.24
- **Open**: 10 | **Closed**: 181
- **4 football positions** (Independiente Petrolero, Academia Puerto Cabello, Barcelona SC, CD Cobresal) from Mar 3 games — in `monitoring`, correctly awaiting Polymarket resolution before closing


### Audit (2026-03-04, updated v0.03.04)
- **Open positions**: 9 | **Closed**: 180
- **Profitable resolved arbs**: 3 (SC Sagamihara +$0.72, Trump Truth Social pending, others)
- **Phantom profit cleaned**: Somaliland $0.93, Japan loss $10.00 (INC-001, INC-002)
- **Phantom profit pending**: Arkansas $0.89 + TX-31 $0.85 = $1.74 — will be corrected at next capital reset
- **Sell arb payout bug**: Sagamihara position retroactively corrected from -$3.58 to +$0.72 in state file

### Key Metrics (2026-03-04)
- **Initial capital**: $100.00
- **Current capital**: $1.24 cash + $100.00 deployed = **$101.24** (+$1.24 vs initial)
- **Profitable resolved arbs**: 3 positions, $1.61 total legitimate profit
- **SC Sagamihara sell arb**: $10.72 payout on $10.00 invested = **+$0.72** (7.2%) — first correct sell arb resolution
- **Win rate on resolved arbs**: 100% (all legitimate completions)
- **Avg return per resolved arb**: ~$0.54

### Lessons
- **Sell arb payout is fundamentally different from buy arb**: in a sell arb (buy-NO on all legs), the winning market's NO bet *loses*; profit comes from all *other* legs resolving NO. Using the buy arb formula (winning leg only) would have caused severe P&L understatement on every resolved sell arb.
- Replacement churn ($0.001/swap × ~1200 swaps ≈ $1.20) is negligible individually but adds up
- The system correctly identifies profitable arbs — both incidents were data/guard failures, not math failures
- Resolution validation will prevent the Somaliland class of bug
- Full pagination prevents the Japan class of bug

---

## 11. Key Configuration Reference

| Parameter | File | YAML path | Current Value | Purpose |
|-----------|------|-----------|---------------|---------|
| `capital_per_trade_pct` | config.yaml | `arbitrage.capital_per_trade_pct` | 0.10 | 10% of cash per position (floor $10, cap $1000) |
| `max_concurrent_positions` | config.yaml | `arbitrage.max_concurrent_positions` | 20 | Max open positions |
| `max_days_to_resolution` | config.yaml | `arbitrage.max_days_to_resolution` | 60 | Entry filter: skip new positions resolving >60 days |
| `max_days_to_replacement` | config.yaml | `arbitrage.max_days_to_replacement` | 30 | Replacement filter: only swap in candidates resolving <30 days (stricter than entry) |
| `max_profit_threshold` | config.yaml | `arbitrage.max_profit_threshold` | 0.3 | Skip >30% arbs (likely bad data) |
| `min_profit_threshold` | config.yaml | `arbitrage.min_profit_threshold` | 0.03 | Skip <3% arbs (not worth fees) |
| `resolution_validation.enabled` | config.yaml | `arbitrage.resolution_validation.enabled` | true | AI date validation on/off |
| `polymarket_taker_fee` | config.yaml | `arbitrage.fees.polymarket_taker_fee` | 0.0001 | Polymarket fee per trade |
| `live_trading.enabled` | config.yaml | `live_trading.enabled` | true | false=paper, true=shadow or live |
| `live_trading.shadow_only` | config.yaml | `live_trading.shadow_only` | true | true=shadow, false=live |
| `anthropic_api_key` | **secrets.yaml** | `resolution_validation.anthropic_api_key` | (your key) | API key for resolution validator |
| `private_key` | **secrets.yaml** | `polymarket.private_key` | (your key) | Polymarket wallet private key |
| `L2 min_price_sum` | constraint_detector.py | hardcoded | 0.85 | Min sum for mutex group |
| `L3 direct mutex guard` | arbitrage_engine.py | hardcoded | 0.90 | Skip if raw_sum < 0.90 |
| `L3 polytope mutex guard` | arbitrage_engine.py | hardcoded | 0.90 | Skip Bregman/FW if sum < 0.90 |

---

## Glossary

| Term | Meaning |
|------|---------|
| **CLOB** | Central Limit Order Book — Polymarket's matching engine and trading API |
| **EFP** | Effective Fill Price — VWAP (Volume-Weighted Average Price) computed by walking the ask book at a given trade size. Single metric capturing both price and depth. |
| **FAK** | Fill And Kill — order type that fills as much as possible immediately, cancels the rest |
| **FOK** | Fill Or Kill — order type that fills entirely or cancels entirely |
| **GTC** | Good Till Cancelled — order that stays on book until filled or manually cancelled |
| **KL** | Kullback-Leibler divergence — measure of distance between probability distributions, used in Bregman projection |
| **LP** | Linear Program — mathematical optimization with linear objective and constraints (CVXPY solves these) |
| **MCP** | Model Context Protocol — Anthropic's standard for AI tool integration |
| **negRisk** | Negative Risk — Polymarket contract structure for mutex groups: reduces collateral by recognising mutual exclusivity at the contract level |
| **P95** | 95th percentile — value below which 95% of observations fall (used for resolution delay model) |
| **VWAP** | Volume-Weighted Average Price — average price weighted by size at each level in the order book |
| **WS** | WebSocket — persistent bidirectional connection for real-time data streaming |
| **Mutex** | Mutually exclusive — a set of outcomes where exactly one will resolve YES and all others NO |
| **Shard** | One of multiple parallel WS connections, each handling ≤2000 assets to prevent server overload |
| **EFP drift** | Change in effective fill price since last arb evaluation — triggers re-evaluation when >$0.005 |
| **Shadow mode** | Paper trading + orderbook validation against live CLOB — no real money moves |
| **PyO3** | Rust→Python bridge allowing Rust code to be called from Python as native extension modules |

---

*Last updated: 2026-03-09 ~14:00 UTC*
*System: WSL Ubuntu on Windows (laptop) + ZAP-Hosting VPS (193.23.127.99)*
*Machines: Laptop (WSL authoritative) + VPS (ZAP-Hosting 193.23.127.99) + Desktop HP-800G2 (dormant)*
*Dashboard: http://localhost:5556 | Exec Control: port 5557*
*Git: https://github.com/andydoc/Prediction-trading (branch: main)*
