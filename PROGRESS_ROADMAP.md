# Prediction Market Arbitrage System
# User Guide · Architecture · Roadmap · Progress

> **Version**: 0.3.2 (pre-release, paper trading)
> **Last updated**: 2026-03-03 18:00 UTC
> **Mode**: PAPER | Capital: ~$9.99 | 9 open, 138 closed
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

### 2.3 Resolution Validator (NEW — v0.3.0)

Before entering a position, L4 now optionally calls the Anthropic API to validate the market's actual resolution date against its rules text. This catches cases where the API `endDate` is misleading (e.g. shows "March 31" but rules say "December 31").

- **Module**: `resolution_validator.py`
- **Config**: `config.yaml` → `resolution_validation.enabled: true`
- **API key**: `config.yaml` → `resolution_validation.anthropic_api_key`
- **Filter**: `max_days_to_resolution: 60` — skip markets resolving beyond N days
- **Cache**: Results cached for 168 hours (1 week) per constraint group

### 2.4 Multi-Machine Execution Control

Only ONE machine may run L4 at a time. A Flask server manages leader election:

- **Server**: `execution_control.py` (port 5557)
- **Client**: `execution_control_client.py` (imported by L4)
- **Behaviour**: TTL-based lock with heartbeat. Fail-open (if server unreachable, allow execution).
- **Commands**: `exec_claim.sh status|claim|release`

### 2.5 Trading Modes

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

C:\Users\andyd\ai-workspace\prediction-trader\   ← Windows git mirror
```

### Key Config Files

**config.yaml** (in git — safe, no secrets):
```yaml
trading:
  capital_per_trade_pct: 0.10          # 10% of capital per position
  max_concurrent_positions: 20
  max_days_to_resolution: 60           # Skip markets >60 days out
  max_profit_threshold: 0.3            # Skip >30% (likely bad data)
  resolution_validation:
    enabled: true
    anthropic_api_key: YOUR_KEY_HERE   # Or use ${ANTHROPIC_API_KEY} env var
    cache_ttl_hours: 168               # Cache AI results for 1 week
  fees:
    taker_fee: 0.0001
```

**secrets.yaml** (NOT in git — contains private keys):
```yaml
polymarket:
  host: "https://clob.polymarket.com"
  chain_id: 137
  private_key: "0x..."
  funder_address: "0x..."
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
scripts/START_TRADER.bat              # With console window
scripts/START_TRADER_SILENT.bat       # Silent (for Task Scheduler)

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

# Pull on other machine
cd ~/prediction-trader && git pull --ff-only origin main
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
- [ ] **Capital reset** — Current capital ~$9.87 includes ~$1.74 phantom profit (Arkansas $0.89 + TX-31 $0.85 from pre-validator era); needs manual reset before go-live

### Phase 2 — Go Live
- [ ] Deposit $100+ USDC to Polymarket wallet
- [ ] Run shadow mode 24h+ for orderbook validation
- [ ] First live trade
- [ ] Live P&L tracking on dashboard
- [ ] Position reconciliation (CLOB vs paper)

### Phase 3 — Reliability & Monitoring
- [ ] Wire dashboard control panel (mode switch, parameter save, restart via API)
- [ ] Fix supervisor double-instance bug (L4 sometimes spawns twice)
- [ ] WhatsApp notifications via OpenClaw (trade alerts, errors, daily summary)
- [ ] API key auth for execution_control.py (before exposing to public IP)
- [ ] Oracle Cloud VPS for execution control server

### Phase 4 — Scale & Optimise
- [ ] Increase capital_per_trade from 10% to 30-50% (backtested)
- [ ] Historical performance analytics on dashboard
- [ ] Multi-exchange support (Kalshi)
- [ ] VPS migration for 24/7 operation
- [ ] Git versioning: branches for features, tags for releases (see §8)

---

## 7. Changelog

### v0.3.2 (2026-03-03) — Replacement Loop Fix & Fee Config
- **FIXED** Replacement loop bug: same opportunity could liquidate multiple positions in one round (T20 World Cup replaced 5 positions at once). Added `used_opp_cids` set to prevent reuse within a round.
- **FIXED** Fee config mismatch: `paper_trading.py` read `polymarket_taker_fee` from wrong config section (always fell back to hardcoded default). Now reads from `arbitrage.fees.polymarket_taker_fee` via `self.taker_fee` set during init.
- **FIXED** Performance counters: reset stale `total_trades` (was 1304, now matches actual position count of 147)
- **MOVED** 57 one-off investigation scripts to `scripts/debug/` (gitignored)
- **NOTED** Caracas FC churn: position replaced/re-entered 14+ times due to score hovering near replacement threshold — not a bug, but highlights need for hysteresis or cooldown on re-entry

### v0.3.1 (2026-03-03) — Replacement Protection & Validator Verification
- **ADDED** 24h replacement protection: positions whose AI-validated resolution date is <24h away are immune from replacement scoring (prevents last-minute swaps before payout)
- **FIXED** Replacement scoring now uses AI-validated resolution date from cache (not raw API `end_date`) for the 24h protection window
- **VERIFIED** Resolution validator working in production: Anthropic API returning 200s, caching results, catching long-dated markets and unrepresented outcomes
- **VERIFIED** 23:59:59 timestamp handling already correct in both cached and fresh code paths
- **IDENTIFIED** Arkansas Governor Democratic Primary: 2 positions ($0.89 phantom) from pre-validator `_expire_position()` era — same class as Somaliland
- **IDENTIFIED** TX-31 Republican Primary: 2 expired positions ($0.85 phantom) + 1 replaced — pre-validator era, "Other" outcome unrepresented
- **CONFIRMED** Trump Truth Social position is legitimate: L2 correctly narrowed to 3 remaining live brackets (160-179, 180-199, 200+) covering all possible outcomes, ~3.5% arb

### v0.3.0 (2026-03-03) — Resolution Safety
- **REMOVED** `_expire_position()` — positions no longer close by time; capital stays locked until markets resolve
- **ADDED** `_check_group_resolved()` — checks all markets in group for price→1.0 before closing
- **ADDED** `resolution_validator.py` — Anthropic API call to validate true resolution dates from rules text
- **ADDED** `max_days_to_resolution: 60` config filter — skips far-dated opportunities
- **FIXED** L1 pagination: removed 10k market cap, now fetches all (~33k+)
- **FIXED** L3 polytope path: added mutex completeness guard (price_sum < 0.90)
- **CLEANED** Japan unemployment positions (markets 1323418-1323422) — $10 loss from incomplete mutex
- **CLEANED** Somaliland parliamentary positions (markets 948391-948394) — $0.93 phantom profit from false expiry

### v0.2.0 (2026-02-28) — Dashboard & Scripts
- Dashboard tabs: Paper/Shadow/Live/Control Panel
- Score column (profit_pct/hours * 10000)
- Mode badge with colour coding
- Script rationalisation: 21→8 scripts
- Naming standardisation across codebase
- Git repository established

### v0.1.0 (2026-02-17) — Initial System
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
- `v0.1.0`, `v0.2.0`, `v0.3.0` — retrospective tags for milestones above
- `v1.0.0` — first successful live trade
- Semantic versioning: MAJOR.MINOR.PATCH

### Implementation Steps
```bash
# Tag existing milestones retrospectively
git tag -a v0.1.0 <initial-commit-hash> -m "Initial four-layer system"
git tag -a v0.2.0 <dashboard-commit-hash> -m "Dashboard and scripts"
git tag -a v0.3.0 HEAD -m "Resolution safety"
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

### Audit (2026-03-03, updated v0.3.1)
After cleaning Somaliland + Japan, with Arkansas and TX-31 identified but not yet cleaned:
- **Closed positions**: 1,208
- **Open positions**: 9 (including 1 confirmed-legitimate Trump Truth Social arb)
- **Legitimately profitable** (>$0.01): 5 positions, $2.04 total (pre-reset)
- **Phantom profit cleaned**: Somaliland $0.93, Arkansas $0.89, TX-31 $0.85 — all from pre-validator `_expire_position()` era
- **Current capital**: ~$9.99 (after state reset, performance counters corrected)
- **Open positions**: 9 | **Closed positions**: 138

### Key Metrics (post-reset)
- **Initial capital**: $100.00
- **Net P&L**: -$90.01 (dominated by Japan $10 loss + replacement churn + incident cleanups)
- **Win rate on resolved arbs**: 100% (5/5 legitimate, pre-reset)
- **Avg return per resolved arb**: ~$0.41 (pre-reset)

### Lessons
- Replacement churn ($0.001/swap × ~1200 swaps ≈ $1.20) is negligible individually but adds up
- The system correctly identifies profitable arbs — both incidents were data/guard failures, not math failures
- Resolution validation will prevent the Somaliland class of bug
- Full pagination prevents the Japan class of bug

---

## 11. Key Configuration Reference

| Parameter | Location | Current Value | Purpose |
|-----------|----------|---------------|---------|
| `capital_per_trade_pct` | config.yaml | 0.10 | % of capital per position |
| `max_concurrent_positions` | config.yaml | 20 | Max open positions |
| `max_days_to_resolution` | config.yaml | 60 | Skip markets resolving >N days |
| `max_profit_threshold` | config.yaml | 0.3 | Skip >30% arbs (likely bad data) |
| `resolution_validation.enabled` | config.yaml | true | AI date validation on/off |
| `resolution_validation.anthropic_api_key` | config.yaml | (needs key) | Anthropic API key |
| `taker_fee` | config.yaml | 0.0001 | Polymarket fee per trade |
| `L2 min_price_sum` | constraint_detector.py | 0.85 | Min sum for mutex group |
| `L3 direct mutex guard` | arbitrage_engine.py | 0.90 | Skip if raw_sum < 0.90 |
| `L3 polytope mutex guard` | arbitrage_engine.py | 0.90 | Skip Bregman/FW if sum < 0.90 |

---

*Last updated: 2026-03-03 18:00 UTC*
*System: WSL Ubuntu on Windows | Machines: Laptop + Desktop*
*Dashboard: http://localhost:5556 | Exec Control: port 5557*
*Git: https://github.com/andydoc/Prediction-trading (branch: main)*
