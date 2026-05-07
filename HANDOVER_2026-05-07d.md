# Handover — 2026-05-07 (session 4)

Self-contained briefing for the next session. Supersedes `HANDOVER_2026-05-07c.md`.

---

## Where the bot is right now

**Dublin VPS** (`38.180.151.102`, SSH `ssh dublin`):

| Field | Value |
|---|---|
| systemd unit | `prediction-trader.service` — active, PID 50929 |
| Binary commit | `04fbb23` (session-4 build) |
| Mode | live |
| Strategy params | **C** — `min_profit=3%`, `capital_per_trade=20%`, `max_concurrent=1`, `min_trade_size=$5` |
| pUSD on-chain | ~$97.37 |
| Open positions | 0 |
| Closed positions | 1 (phantom Egyptian football position — recovered) |
| Realised P&L | +$0.14 (from earlier Germany vs Curaçao trade, session-2) |

---

## What happened this session

### Incident: phantom position (14:34:44 UTC)

A 3-leg arb on Egyptian football (Tala'ea El Gaish vs Ghazl El Mahalla, markets
1930074/1930075/1930076) submitted via CLOB batch endpoint. CLOB accepted all 3
orders (`success=true`, valid `orderID`s) then immediately cancelled them (FAK,
no liquidity). On-chain USDC unchanged ($97.37) but accounting committed $19.47.
Bot showed `Capital=$77.89 positions=1` for ~55 minutes.

**Root causes:**
1. `executor.rs` `[BATCH]` log used `results.len()` (counting rejected legs too)
   instead of counting `OrderResult::Accepted` — obscured that 0 real fills happened.
2. `apply_reconciliation` for `PositionMissingOnVenue` logged but never closed the
   phantom position — it stayed open until the bot was manually restarted.

**Recovery:**
1. Stopped bot.
2. Ran `/tmp/phantom_recovery.py` (DB surgery): added 7 reversal journal entries,
   restored `scalars.current_capital` 77.89 → 97.37, marked position closed.
3. Committed both code fixes, pushed to GitHub, pulled + rebuilt on VPS.
4. Restarted. Confirmed: `State loaded: $97.37 capital, 0 open, 1 closed`.

### Code fixes deployed (commit `04fbb23`)

| File | Fix |
|---|---|
| `rust_engine/src/executor.rs` | `[BATCH]` accepted-count now counts only `OrderResult::Accepted` |
| `rust_engine/src/lib.rs` | `apply_reconciliation` auto-rolls back positions where all legs are `MissingOnVenue` with `actual_shares==0` |

### Session-5 DB fixes (same day, post-restart)

Two further issues discovered after the session-4 recovery:

**1. USDC drift persisting across restarts**

Root cause: the `AccountingLedger` is persisted as a full JSON blob in `checkpoints` (key=`accounting_ledger`), not reconstructed from the `journal` table on load. Session-4 DB surgery wrote reversal entries to `journal` only — the checkpoint blob retained `cash=$77.89`. Fixed by patching the checkpoint JSON directly: `cash` $77.89 → $97.37, `positions_deployed` $19.47 → $0, `fees_total` $0.0019 → $0. Also cleared stale holdings for phantom markets (1930074–1930076) and zeroed resolved G vs C holdings (2164700–2164702). Documented in OPS_RUNBOOK §10.

**2. Virtual portfolio phantom positions (Strategies tab)**

Root cause: when the real executor entered the Egyptian football FAK batch, the strategy tracker recorded virtual entries for all 6 shadow strategies simultaneously. When `apply_reconciliation` rolled back the real position, it did not notify the strategy tracker — virtual positions were never reversed. All 5 affected strategies (Shadow-A/B/C/D/F) showed 1 open phantom position and reduced capital.

Fixed via `strategy_open_positions` DELETE + `strategy_portfolios.current_capital` restore (Python script run on VPS). All 6 strategies restored to $97.37, 0 open.

**Known gap**: the strategy tracker has no hook into `apply_reconciliation` — if the reconciler rolls back a real position, virtual positions for the same constraint are not reversed. This should be addressed in the same session as item #6 (auto-unwind wiring).

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
   - `usdc_monitor` (enabled, publicnode RPC)
   - `notifications.hostname: dublin`
   - `circuit_breaker.max_drawdown_pct: 0.35` (was 0.10 in base config)
   - `tier_b_max_connections` is NOT in live-d.yaml — keep the manual patch in
     `config.yaml` or add it to `live-d.yaml` first.

2. Revert manual patches in `config.yaml` (usdc_monitor, hostname) — redundant once live-d.yaml loads.
   Keep `tier_b_max_connections: 20` until added to `live-d.yaml`.

3. `sudo systemctl daemon-reload && sudo systemctl restart prediction-trader`.

### Code — next-session work

| # | Item | Priority | Notes |
|---|------|----------|-------|
| **#6** | Full auto-unwind: wire `evaluate_arb_fills` → `execute_position_exit` in partial-submit branch | HIGH | Replace CB-trip bridge. Wait until WS passive monitor has a few days of data. |
| **#5b** | Dashboard: render red brackets where `fill_status != accepted` or `intended_shares != actual_shares` | LOW | JS-only change. Per-leg data is in the position JSON. |

---

## VPS config state (post-session-3 patches — unchanged this session)

`/home/ubuntu/prediction-trader/config/config.yaml` differs from git in these ways:

```yaml
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
  execute_orders: true         # added in session 2
```

---

## Audit status

No new audit findings this session. Session-2 findings remain as documented in `HANDOVER_2026-05-07b.md`.

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
State loaded: $97.37 capital, 0 open, 1 closed (CB peak=$97.37)
Orchestrator ready [LIVE]
```

---

## Key files changed this session

| File | What changed |
|---|---|
| `rust_engine/src/executor.rs` | `[BATCH]` accepted-count log fixed |
| `rust_engine/src/lib.rs` | `apply_reconciliation` auto-rollback for phantom positions |
| `CHANGELOG.md` | Session-4 entry added |
| `HANDOVER_2026-05-07d.md` | This file |
