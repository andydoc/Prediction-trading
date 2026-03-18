# User Guide

**System**: Prediction Market Arbitrage Trading System
**Version**: v0.14.7

---

## 1. What This System Does

Automated detection and execution of guaranteed-profit arbitrage on Polymarket prediction markets. The system:

1. **Scans** all 33k+ active markets from Polymarket
2. **Detects** mutex groups — outcomes where exactly one must win
3. **Evaluates** prices via WebSocket; when a group's prices drift from fair value, computes optimal bet sizes
4. **Executes** orders across all legs to lock in a risk-free profit regardless of outcome
5. **Manages** positions until resolution; replaces with better opportunities when found
6. **Resolves** collects payout when outcome is determined

**This is not prediction or gambling. It is pricing-error exploitation with mathematically guaranteed returns.**

---

## 2. Prerequisites

| Requirement | Detail |
|-------------|--------|
| Rust toolchain | `rustup` + `cargo` (stable channel) |
| OS | Linux (Ubuntu 24.04 for VPS) or WSL2 for development |
| VPS (production) | 4 vCPU, 8 GB RAM, 25 GB NVMe |
| Polymarket account | For live trading (not needed for shadow mode) |
| USDC | ~$1,000 on Polygon for live trading |
| POL | ~5 POL for transaction gas fees |
| Telegram bot | Optional, for notifications (recommended) |
| Anthropic API key | For AI resolution validation + postponement detection |

---

## 3. Setup from Scratch

### Clone and build
```bash
git clone <repo-url>
cd prediction-trader
cargo build --release
```

The binary is at `target/release/prediction-trader`.

### Configure secrets

Create `config/secrets.yaml`:
```yaml
polymarket:
  private_key: '0x...'           # Polygon EOA private key
  funder_address: '0x...'        # Wallet address
  signature_type: 1              # 1=email/Magic, 2=browser, 0=EOA
  host: 'https://clob.polymarket.com'
  chain_id: 137
resolution_validation:
  enabled: true
  anthropic_api_key: 'sk-ant-...'
telegram_bot_token: '...'        # Optional
```

**Do not commit this file.**

### Configure the system

Edit `config/config.yaml` — key settings:

```yaml
live_trading:
  shadow_only: true              # true = paper trading, false = real money
  initial_capital: 100           # Starting capital ($)

arbitrage:
  capital_per_trade_pct: 0.10    # 10% of capital per position
  max_concurrent_positions: 20
  min_profit_threshold: 0.02     # Minimum 2% expected profit
  max_days_to_resolution: 90

dashboard:
  port: 5558

notifications:
  enabled: true
  phone_number: '<telegram_chat_id>'
```

### Optional: Per-machine overrides

Create `config/config.local.yaml` for overrides that shouldn't be committed:
```yaml
dashboard:
  port: 5557
monitoring:
  logging:
    level: info
```

---

## 4. Starting and Stopping

### Start
```bash
bash scripts/start.sh                            # Shadow mode (default)
bash scripts/start.sh --mode live                # Live trading
bash scripts/start.sh --instance shadow-a        # Named instance
bash scripts/start.sh --dry-run                  # Print config only
```

The script pulls latest code, builds if needed, starts the binary, and verifies the dashboard is responsive.

### Stop
```bash
bash scripts/kill.sh                             # Graceful shutdown
bash scripts/kill.sh --emergency                 # Emergency: cancel orders + shadow mode
```

### Restart
```bash
bash scripts/restart.sh                          # Stop → pull → rebuild → start
bash scripts/restart.sh --clean                  # Also purge stale cache
```

---

## 5. Dashboard

Access at `http://localhost:5558` (default port). For VPS, use an SSH tunnel:
```bash
ssh -L 5558:127.0.0.1:5558 vps-ubuntu
```

### Tabs

**Positions** (default): Open positions table (strategy, score, capital, expected P&L, resolution date, entry time). Closed positions grouped by category (resolved, proactive exit, replaced). Aggregate holdings by market.

**Monitor**: System metrics (CPU, RAM, disk), WebSocket stats (connections, message rate, latency), financial time-series charts (total value, deployed %, drawdown, realized/unrealized P&L).

**Shadow**: Paper trade validation — would-trade signals, rejection reasons, recent shadow entries.

**Live**: Real trading stats (when `shadow_only = false`) — filled orders, settlement tracking, CLOB API status.

### Header controls

- **Mode badge**: SHADOW (orange) or LIVE (red)
- **KILL SWITCH button**: Emergency shutdown (confirmation required). Cancels orders, switches to shadow mode, sends Telegram alert.
- **Stats**: Starting capital, total value, open position count, POL gas balance.

---

## 6. Mode Switching

| Mode | Config | Behaviour |
|------|--------|-----------|
| Shadow | `shadow_only: true` | Paper trades only. No real money at risk. |
| Live | `shadow_only: false` | Real CLOB orders with real USDC. |

**Switch via CLI:**
```bash
bash scripts/start.sh --mode shadow              # Paper trading
bash scripts/start.sh --mode live                # Real trading
```

**Switch via config**: Set `live_trading.shadow_only` in `config/config.yaml` and restart.

**Emergency switch to shadow**: Use the kill switch (dashboard button or `kill.sh --emergency`). This is irreversible without a restart.

---

## 7. Multi-Instance Mode

Run multiple instances with different parameter sets for comparison:

```bash
bash scripts/start.sh --instance shadow-a        # Port 5560
bash scripts/start.sh --instance shadow-b        # Port 5561
bash scripts/start.sh --instance shadow-c        # Port 5562
```

Each instance auto-isolates: separate database, logs, PID file, and dashboard port.

**Instance configs** are in `config/instances/{name}.yaml`. Pre-configured instances:

| Instance | Strategy | Positions | Capital/trade |
|----------|----------|-----------|---------------|
| shadow-a | Max diversification | 40 | 5% |
| shadow-b | Baseline | 20 | 10% |
| shadow-c | Moderate | 15 | 15% |
| shadow-d | Conservative | 10 | 20% |
| shadow-e | Concentrated | 8 | 50% |
| shadow-f | Fast markets (5-15 min crypto) | 50 | 5% |

**Shadow-F** targets short-lived crypto price prediction markets (5-15 minute resolution). It uses aggressive timing parameters: 60s constraint rebuild interval, 60s minimum resolution time, 10s replacement cooldown, and 6-minute replacement protection. See PRODUCT_SPEC_v2.md for full parameter grid.

---

## 8. Recovery After Crash

The system persists all state to SQLite every 30 seconds. On restart:

1. Loads positions, capital, metrics from `data/state_rust.db`
2. Reconciles against Polymarket CLOB (live mode)
3. Resumes trading from last checkpoint

**If state is corrupted:**
```bash
# Restore from automatic backup
cp data/.state_rust.db.bak data/state_rust.db
bash scripts/restart.sh

# Or start fresh (loses position history)
rm data/state_rust.db
bash scripts/restart.sh
```

**Logs** are in `logs/` with daily rotation and 30-day retention.

---

## 9. Safety Systems

### Circuit Breaker (C1)
Automatically halts trading when:
- Portfolio drawdown exceeds 10% from peak
- 3+ API errors within 5 minutes
- CLOB API unreachable for 10 minutes
- POL gas balance critically low

**Reset**: Restart the process (`bash scripts/restart.sh`). Fix the underlying issue first.

### Kill Switch (C2)
Two trigger paths:
- **Dashboard**: Click KILL SWITCH button
- **Shell**: `bash scripts/kill.sh --emergency`

Actions: Cancel all orders, switch to shadow mode, Telegram alert.

### POL Gas Monitor (C1.1)
Checks balance hourly. Warning at < 1 POL, circuit breaker trip at critical threshold.

### Daily P&L Report (C4)
Automated Telegram summary at midnight UTC: entries, exits, fees, net P&L, capital utilisation, drawdown from peak. Also persisted to SQLite.

---

## 10. Go-Live Checklist

Before switching from shadow to live trading:

- [ ] **Milestone C complete**: Circuit breaker, kill switch, notifications all working
- [ ] **Milestone D**: CLOB integration test with ~$50 USDC — place and cancel real micro-orders, verify execution path
- [ ] **Milestone E**: 14 consecutive days of shadow trading at $1,000 capital across 6 instances with zero unhandled errors
- [ ] **Parameter selection**: Compare shadow instances, select winning config
- [ ] **Fund account**: Deposit ~$1,000 USDC + ~5 POL to Polymarket wallet
- [ ] **CTO sign-off**: Written approval after reviewing 14-day comparison results
- [ ] **Switch mode**: `bash scripts/start.sh --mode live`
- [ ] **Supervised period**: 48 hours with 2-4 hourly check-ins
- [ ] **Autonomous**: Daily check-ins via Telegram summaries

---

## 11. Glossary

| Term | Meaning |
|------|---------|
| **Arb / Arbitrage** | Risk-free profit from pricing errors across mutex outcomes |
| **CLOB** | Central Limit Order Book (Polymarket's trading API) |
| **Constraint** | A group of mutually exclusive market outcomes |
| **EFP** | Expected Fair Price — what an outcome "should" cost |
| **Mutex group** | Set of outcomes where exactly one must win |
| **POL** | Polygon's native gas token (pays transaction fees) |
| **Shadow mode** | Paper trading — evaluates and records trades without real money |
| **Tier B / C** | WebSocket connection tiers: B = hot constraints, C = new market detection |
| **USDC** | USD-pegged stablecoin used for Polymarket trading |
