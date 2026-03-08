# Prediction Market Arbitrage System
# User Guide · Architecture · Roadmap · Progress

> **Version**: v0.03.05 (pre-release, shadow trading)
> **Last updated**: 2026-03-07 15:00 UTC
> **Mode**: SHADOW | Laptop: $5.42 cash, 10 open, 197 closed | VPS: $100.00 cash, 0 open, fresh start
> **VPS**: ZAP-Hosting Lifetime (193.23.127.99) — 4 cores, 4GB RAM, Ubuntu 24.04, systemd auto-restart
> **Git**: https://github.com/andydoc/Prediction-trading (branch: `main`)

---

## 1. What This System Does

Automated detection and exploitation of pricing inefficiencies across Polymarket prediction markets. Uses mathematical arbitrage (marginal polytope construction, Bregman projection with KL divergence, Frank-Wolfe optimization) to find groups of related markets where prices are logically inconsistent, then bets across all outcomes to lock in guaranteed profit regardless of which outcome occurs.

**Not prediction. Not gambling. Pure pricing-error exploitation.**

---

## 2. Architecture

### 2.1 Four-Layer Pipeline

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
├── paper_trading.py                          ← Paper trading engine (position lifecycle)
├── live_trading.py                           ← Live CLOB trading engine
├── resolution_validator.py                   ← AI-powered resolution date validation
├── dashboard_server.py                       ← Web dashboard (port 5556)
├── execution_control.py                      ← Multi-machine lock server (port 5557)
├── execution_control_client.py               ← Lock client (used by L4)
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

---

## 7. Changelog

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

*Last updated: 2026-03-07 15:00 UTC*
*System: WSL Ubuntu on Windows (laptop) + ZAP-Hosting VPS (193.23.127.99)*
*Machines: Laptop (WSL) + VPS (ZAP) + Desktop HP-800G2 (dormant)*
*Dashboard: http://localhost:5556 | Exec Control: port 5557*
*Git: https://github.com/andydoc/Prediction-trading (branch: main)*
