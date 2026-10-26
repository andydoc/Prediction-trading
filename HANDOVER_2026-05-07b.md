# Handover — 2026-05-07 (session 2)

Self-contained briefing for the next session. Read top-to-bottom.

---

## Where the bot is right now

**Dublin VPS** (`[dublin-vps-ip]`, SSH `ssh dublin` or `ssh dublin-ubuntu`):

| Field | Value |
|---|---|
| systemd unit | `prediction-trader.service` (active — NOT yet redeployed with session-2 build) |
| Mode | live |
| Strategy params | **C** — `min_profit=3%`, `capital_per_trade=20%`, `max_concurrent=1`, `min_trade_size=$5` |
| pUSD on-chain | ~$97.37 |
| Open positions | 0 |
| Realised P&L | +$0.14 |

**Why `max_concurrent=1` still?** The replacement-liquidation phantom-credit bug is now fixed, and the WS user channel is wired — both blockers from session 1 are cleared. **It is now safe to raise `max_concurrent` back to 15 (Strategy C) or 8 (Strategy D).** See operator tasks below.

---

## What shipped this session (not yet deployed)

### Bug fixes
1. **Replacement-liquidation phantom-credit** (`orchestrator.rs:3113`). Mirror of the INC-020 proactive-exit fix. Replacement path now calls `execute_position_exit` + `apply_exit_fills` in live mode. Pre-go-live blocker for `max_concurrent > 1` — **cleared**.

2. **`pm_open_count > 0` gate** (`orchestrator.rs`, 5 locations). Reconciliation now runs even when PM is empty (`pm_open_count > 0 || (!shadow_only && clob_auth.is_some())`). Orphan venue positions are now discoverable at startup.

3. **Dead no-auth reconcile wrappers deleted** (`lib.rs`). Regression foothold removed.

4. **Dead `OrchestratorConfig` fields** (`orchestrator.rs`). `mode` and `min_profit_ratio` deleted (never wired). `postponement_rescore_days` now actually governs the 14-day wait in `check_postponements()` (was hardcoded at 1 day).

5. **Dead code swept**: `update_leg_with_fill`, `strategy_tracker::resolve`, `format_tick_size`, `_clob_host` doc fix.

### New features
6. **WS user channel wired** (`orchestrator.rs`). Dedicated tokio runtime drives `UserChannelClient`. Each tick, `drain_user_events()` dispatches fills to the executor state machine (`Submitted → Matched → Confirmed`) and the full B3.2 accounting suspense cycle. INC-021 phantom-payout guard, INC-020 proactive-exit terminal detection, and `RevertRateTracker` are all now live.

7. **Per-leg fill status** (`position.rs`, `orchestrator.rs`). `MarketLeg` gains `intended_shares` / `actual_shares` / `fill_status: pending|accepted|rejected`. All `#[serde(default)]` — zero migration cost. Entry and replacement paths stamp each leg immediately after CLOB response; WS confirmation upgrades `pending → accepted` and populates `actual_shares`. `fill_quality::record_actual` now has its first production caller.

---

## Outstanding items (in priority order)

### Immediate operator action
- **Deploy session-2 build to Dublin VPS** (see restart procedure below).
- **Raise `max_concurrent`**: bump from 1 to 15 (Strategy C) or switch to Strategy D (8 concurrent) once bankroll ≥ $100. The two blockers are cleared.

### Code — next-session work

| # | Item | Priority | Notes |
|---|------|----------|-------|
| **#6** | Full auto-unwind: wire `evaluate_arb_fills` → `execute_position_exit` in partial-submit branch | HIGH | Replace the CB-trip bridge. Wait until WS passive monitor has a few days of data to confirm fill events arrive reliably. |
| **#5b** | Dashboard: render red brackets where `fill_status != accepted` or `intended_shares != actual_shares` | LOW | JS-only change. Per-leg data is now in the position JSON. |
| **Strategy D** | Switch to `min_profit=4%`, `capital_per_trade=30%`, `max_concurrent=8`, `max_profit=20%` | — | When bankroll ≥ $100. |

**Completed this session (not in table above):**
- ✅ **#11** `execute_arb_batch` wired as default for both entry + replacement paths.
- ✅ **B4.5** `confirm_fills_parallel` wired via `wait_for_exit_fills` helper; covers ~20% WS dropout rate.

### Monitoring — first 48 h post-deploy

Watch for these in `journalctl -u prediction-trader -f`:

```
[WS-user] User channel started (N condition IDs subscribed)   ← WS connected
[WS-user] order=... status=MATCHED → EnteredSuspense          ← fill lifecycle working
[WS-user] order=... status=CONFIRMED → Confirmed              ← accounting updated
```

If you see `CONFIRMED` events then the B3.2 suspense cycle, INC-021 reversal guard, and `RevertRateTracker` are all live. If WS events are absent after 24h of trading, suspect connection issue — check `[WS-user] connecting` logs.

---

## Architecture decisions documented

### #11: `execute_arb_batch` vs `execute_arb`

`execute_arb_batch` submits all legs in a single `POST /orders` (array body). Built-in fallback to sequential `execute_arb` on: >15 legs, HTTP non-2xx, network error. Return type identical to `execute_arb` (`ArbExecutionResult`). Wire it by replacing two `execute_arb(...)` calls in orchestrator.

**Why batch is better**: one round-trip vs N, smaller partial-fill window (legs arrive atomically at CLOB), sequential fallback means zero new failure modes.

**Why you might not bother now**: `max_concurrent=1` means 2-leg positions only, so sequential is two round-trips. Latency benefit is negligible at this scale. Correct time to wire: when `max_concurrent > 4` and positions have 4+ legs.

### B4.5: `confirm_fills_parallel`

`dispatch_user_event` is a passive monitor (processes WS fills as they arrive tick by tick). `confirm_fills_parallel` is an active, synchronous confirmation call made *at order submission time*, racing:
- WS (`wait_for_matched_fills`) — typically <1 s, but CONFIRMED drops ~20% of the time
- Data API (`poll_data_api_for_fills`) — polls `data-api.polymarket.com/positions` delta every N seconds, with a 2 s head start for WS

Without B4.5: trades where WS drops CONFIRMED stay in `Matched` forever. `is_terminal()` never fires. Proactive-exit polling loop gives up after 6×500ms and logs "position holds, no bookkeeping update". The position sits open until manual intervention.

With B4.5: the Data API delta catch covers the ~20% WS dropout rate. Position accounting is always reconciled.

**Wire it in**: replace `user_channel.wait_for_confirmed_fills(...)` in `execute_position_exit` (`executor.rs:~750`) with `confirm_fills_parallel(ws_receiver, http_client, auth, asset_ids, baseline_sizes, ...)`. Requires plumbing `baseline_sizes` (pre-submission position snapshot) from the caller.

---

## Audit status — 17 findings

| # | Severity | Description | Status |
|---|----------|-------------|--------|
| Root cause A | HIGH | WS user channel not wired | ✅ Fixed |
| Root cause B | HIGH | Replacement liquidation phantom-credit | ✅ Fixed |
| #5 | HIGH | `pm_open_count > 0` gate | ✅ Fixed |
| #6 | HIGH | `evaluate_arb_fills` unwired (full auto-unwind) | ❌ CB-trip bridge only |
| #8 | MEDIUM | Dead no-auth reconcile wrappers | ✅ Fixed |
| #11 | MEDIUM | `execute_arb_batch` zero callers | ✅ Fixed (wired as default) |
| #12 | MEDIUM | Dead OrchestratorConfig fields / postponement_rescore_days | ✅ Fixed |
| #13 | MEDIUM | `update_leg_with_fill` zero callers | ✅ Deleted |
| #15 | LOW | `strategy_tracker::resolve` zero callers | ✅ Deleted |
| #16 | LOW | `query_clob_positions` doc / param | ✅ Fixed |
| #17 | LOW | `format_tick_size` dead helper | ✅ Deleted |
| B3.2 | HIGH | Suspense subsystem zero callers | ✅ Fixed via process_fill_event |
| B4.5 | HIGH | `confirm_fills_parallel` zero callers | ✅ Fixed (wired via `wait_for_exit_fills`) |
| `fill_quality::record_actual` | MEDIUM | Zero callers | ✅ Fixed (wired in dispatch) |
| Per-leg fill_status | MEDIUM | No intended/actual share tracking | ✅ Fixed |

---

## Known on-chain dust (cosmetic, not worth selling)

- `0.0028 Draw-NO` shares (Germany v Curaçao) — ~$0.003.
- `12.37 Spurs YES` shares (2026 NBA champion) — ~$0.49 at entry.

Both invisible to bot (PM has 0 positions). They sit until market resolution.

---

## Restart + deploy procedure

```bash
# 1. Build on VPS (or scp the binary)
ssh dublin-ubuntu "cd /home/andydoc/prediction-trader && cargo build --release 2>&1 | tail -5"

# 2. Restart service
ssh dublin "sudo systemctl restart prediction-trader"

# 3. Verify
ssh dublin "sleep 35 && sudo journalctl -u prediction-trader --since '60 seconds ago' --no-pager \
  | grep -E 'Config loaded|Capital=|WS-user|user channel started'"
```

Expected new log lines post-deploy:
```
[WS-user] User channel started (N condition IDs subscribed)
```

If CB is tripped (partial fill):
```bash
ssh dublin "sudo -u ubuntu /tmp/unwind_venv/bin/python \
  /home/ubuntu/prediction-trader/scripts/ops/manual_unwind_v2.py --auto"
ssh dublin "sudo systemctl restart prediction-trader"
```

---

## Quick-reference — key files

| File | What changed |
|---|---|
| `rust_supervisor/src/orchestrator.rs` | Replacement fix, pm_open_count gate, WS user channel, per-leg fill update |
| `rust_engine/src/position.rs` | `LegFillStatus` enum, `intended_shares`/`actual_shares`/`fill_status` on `MarketLeg`, `update_leg_fill_status()` |
| `rust_engine/src/eval.rs` | `ConstraintStore::market_ids()` |
| `rust_engine/src/executor.rs` | `find_trade_id_by_clob_order_id`, `get_tracked_order_clone` |
| `rust_engine/src/lib.rs` | Dead `reconcile_startup`/`reconcile_periodic` deleted |
| `rust_engine/src/strategy_tracker.rs` | Dead `resolve` deleted |
| `rust_engine/src/reconciliation.rs` | `_clob_host` param + doc fix |
| `CHANGELOG.md` | Session 2 entry added |
