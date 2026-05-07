# Handover — 2026-05-07 (session 3)

Self-contained briefing for the next session. Supersedes `HANDOVER_2026-05-07b.md`.

---

## Where the bot is right now

**Dublin VPS** (`38.180.151.102`, SSH `ssh dublin`):

| Field | Value |
|---|---|
| systemd unit | `prediction-trader.service` — active, PID 46154 |
| Binary commit | `c38d030` (session-3 build) |
| Mode | live |
| Strategy params | **C** — `min_profit=3%`, `capital_per_trade=20%`, `max_concurrent=1`, `min_trade_size=$5` |
| pUSD on-chain | ~$97.37 |
| Open positions | 0 |
| Realised P&L | +$0.14 |

---

## What shipped this session (deployed)

### Bug fixes

1. **Phantom drawdown from startup sequencing** (`orchestrator.rs`).
   Dashboard ticks fire before `load_state()` runs, seeding `MonitorState.peak_value`
   with the config `initial_capital` ($100) rather than the DB value ($97.37).
   Result: a permanent 2.63% false drawdown. Fixed: `load_state()` ends with
   `self.engine.monitor.lock().peak_value = total_value`.

2. **Total Value header shown red due to floating-point gap** (`dashboard.html`).
   `current_capital = 97.36805…` vs `initial_capital = 97.37` triggered the
   exact `>=` threshold. Fixed with a $0.01 epsilon:
   `total_value >= init_cap - 0.01`.

### VPS config patches (not in git — applied directly to `config/config.yaml`)

| Setting | Before | After | Effect |
|---|---|---|---|
| `websocket.tier_b_max_connections` | 17 | 20 | Stops 642-asset subscription warning |
| `safety.usdc_monitor` | missing | added (enabled, publicnode RPC, 300 s) | USDC (LIVE) shows on dashboard |
| `notifications.hostname` | `laptop` | `dublin` | Telegram tags correct |

---

## Outstanding items (in priority order)

### Operator — when ready to switch strategy

**Switch to Strategy D** (when bankroll ≥ $100):

1. Edit `/etc/systemd/system/prediction-trader.service`. Replace the current
   `ExecStart` line with:
   ```
   ExecStart=/usr/local/bin/pt-service-wrapper.sh -m live -i live-d
   ```
   This loads `config/instances/live-d.yaml` which contains:
   - Strategy D params (`max_concurrent=8`, `capital_per_trade=30%`, `min_profit=4%`)
   - `usdc_monitor` (enabled, publicnode RPC) — replaces manual patch in config.yaml
   - `notifications.hostname: dublin` — replaces manual patch
   - `circuit_breaker.max_drawdown_pct: 0.35` (was 0.10 in base config)
   - `tier_b_max_connections` is NOT in live-d.yaml — keep the manual patch in
     `config.yaml` or add it to `live-d.yaml` first.

2. Revert the three manual patches from `config.yaml` (usdc_monitor, hostname,
   tier_b_max_connections) — they'll be redundant once live-d.yaml loads.
   Exception: `tier_b_max_connections: 20` — add this to `live-d.yaml` before
   switching so it isn't lost.

3. `sudo systemctl daemon-reload && sudo systemctl restart prediction-trader`.

**Alternatively**, to raise `max_concurrent` to 15 (stay on Strategy C):
Edit the `ExecStart` `--set arbitrage.max_concurrent_positions=1` → `15`.
No other changes needed.

### Code — next-session work

| # | Item | Priority | Notes |
|---|------|----------|-------|
| **#6** | Full auto-unwind: wire `evaluate_arb_fills` → `execute_position_exit` in partial-submit branch | HIGH | Replace CB-trip bridge. Wait until WS passive monitor has a few days of data to confirm fill events arrive reliably. |
| **#5b** | Dashboard: render red brackets where `fill_status != accepted` or `intended_shares != actual_shares` | LOW | JS-only change. Per-leg data is in the position JSON. |

### Monitoring — first 48 h (continued)

Still watching for first real trades under session-2/3 build. Key log patterns:

```
[WS-user] order=... status=MATCHED → EnteredSuspense    ← fill lifecycle working
[WS-user] order=... status=CONFIRMED → Confirmed         ← accounting updated
USDC.e balance: $xx.xx (accounting: $xx.xx)              ← USDC monitor firing
```

---

## VPS config state (post-session-3 patches)

`/home/ubuntu/prediction-trader/config/config.yaml` differs from git in these ways:

```yaml
# These keys added/changed directly on VPS (not in git):
websocket:
  tier_b_max_connections: 20   # was 17 in git

safety:
  usdc_monitor:                # entire block absent from git base config
    enabled: true
    rpc_url: https://polygon-bor-rpc.publicnode.com
    check_interval_seconds: 300
    drift_threshold: 1.0
    warning_balance: 15.0
    critical_balance: 5.0

notifications:
  hostname: dublin             # was 'laptop' in git

live_trading:
  execute_orders: true         # added in session 2 (also absent from git base config)
```

All four overrides collapse into `live-d.yaml` (except `tier_b_max_connections`)
when the service loads `-i live-d`.

---

## Audit status — all session-2 findings remain as documented in HANDOVER_2026-05-07b.md

No new audit findings opened this session.

---

## Restart + deploy procedure

```bash
# 1. Build on VPS
ssh dublin "cd /home/ubuntu/prediction-trader && \
  PATH=/home/ubuntu/.cargo/bin:/home/ubuntu/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin:\$PATH \
  cargo build --release -p prediction-trader 2>&1 | tail -5"

# 2. Restart
ssh dublin "sudo systemctl restart prediction-trader"

# 3. Verify (wait ~40 s)
ssh dublin "sleep 40 && sudo journalctl -u prediction-trader --since '45 seconds ago' \
  --no-pager | grep -E 'ARMED|WS-user|Orchestrator ready|State loaded|USDC monitor'"
```

Expected lines:
```
[G1] LIVE EXECUTOR ARMED
USDC monitor enabled: drift>1, warn<$15, critical<$5
[WS-user] User channel started (NNNN condition IDs subscribed)
State loaded: $97.37 capital, 0 open, 0 closed (CB peak=$97.37)
Orchestrator ready [LIVE]
```

---

## Quick-reference — key files changed this session

| File | What changed |
|---|---|
| `rust_supervisor/src/orchestrator.rs` | Monitor peak reset after `load_state()` |
| `rust_engine/static/dashboard.html` | Total Value header $0.01 epsilon |
| `CHANGELOG.md` | Session-3 entry added |
