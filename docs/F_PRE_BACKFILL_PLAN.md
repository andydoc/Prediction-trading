# F-Pre / F-Main Backfill Plan

Plan for closing the gates that were missed, partially missed, or deferred
during the rush to live execution, **without halting the current $100.02
live run on Dublin**. Written 2026-04-28.

The live executor is armed and accruing trade history. We do not want to
restart it more often than the safe-reboot cycle requires (kernel/libc
updates) since each restart loses some short-term in-memory state and any
trades in flight at that moment.

---

## Audit summary (as of 2026-04-28)

| Gate | Spec status | Reality | Backfill priority |
|------|-------------|---------|-------------------|
| F-pre-1 (executor wired) | ⬚ in v2 spec, ✅ in CHANGELOG (G1) | ✅ entry+unwind, ⚠️ proactive-exit was missed → **INC-020 fixed today, untested** | P1 — needs first live exit to validate |
| F-pre-2 (TLS verification) | ⬚ in v2 spec, ✅ in CHANGELOG (G2) | ✅ done | DONE |
| F-pre-3 (zeroize keys) | ⬚ in v2 spec, ✅ in CHANGELOG (G3) | ✅ done | DONE |
| F-pre-4 (fill quality logging) | ⬚ in v2 spec, ⚠️ infra-only (G4-infra) | infrastructure complete, 95% validation requires real fills (zero so far) | P2 — gather data once trades fire |
| F-pre-5 (capital velocity) | ⬚ in v2 spec, ✅ in CHANGELOG (G5) | ✅ done, surfaced in strategy summary | DONE |
| F-pre-6 (geoblock runbook) | ⬚ deferred to Dublin migration | ✅ written today (`OPS_GEOBLOCK_RUNBOOK.md`) | DONE |
| F-pre-7 (Gamma freshness) | ⬚ in v2 spec, ✅ wired (G7), ⚠️ broken on real Gamma | ✅ INC-019 fix shipped 2026-04-27, boot probe added today | DONE (modulo periodic re-probe) |
| F-pre-8 (deferred audit items) | ❌ deferred per audit v5 low-impact | unchanged | P3 — case-by-case |
| F-pre-9 (D9 partial fill) | ✅ | ✅ | DONE |
| F1 (deposit $1k) | ⬚ | scaled down to $100.02 deliberately for de-risked first run | Scale up after F4-F5 pass |
| F2 (winning parameters) | ⬚ | applied but untested in live | F4 validates |
| F3 (live mode) | ⬚ | ✅ shadow_only=false | DONE |
| F4 (supervised first trades) | ⬚ | ❌ zero live trades; INC-019 was eating every entry until 2026-04-27 | P1 — passive, fires on first opportunity |
| F5 (48h supervised) | ⬚ | ❌ not started | P2 — starts after F4 |
| F6 (steady state) | ⬚ | ❌ not started | P3 — starts after F5 |

Three classes of gap remain:

- **Code-complete but never field-tested** (F-pre-1 proactive exit, F-pre-7).
  Need a real opportunity to exercise. Backfill = gather first-trade evidence.
- **Awaiting data** (F-pre-4 fill quality 95%). Backfill = wait for ≥10 trades,
  then run validation.
- **Audit cleanups** (F-pre-8). Backfill = case-by-case review, no urgency.

---

## P1 — Backfills that need the first real live trade

These cannot be validated by code review or unit tests alone. They unlock
on the first opportunity that crosses the entry gate.

### F-pre-1 (proactive exit live execution) — INC-020 validation

**What's needed**: a live position that crosses the 1.2× proactive-exit gate,
producing a real CLOB sell submission and partial-fill reconciliation. The
new code path is:

```
check_proactive_exits → execute_position_exit (FAK SELL)
                     → poll sell_orders_for_position (~3s)
                     → apply_exit_fills (actual fills, not bid×shares)
```

**Validation criteria**:
- Telegram receives `[ProactiveExit]` notification with `net` and `profit`
  matching the wallet's USDC delta within $0.10 (fee dust + price slippage)
- `evaluated_opportunities.rejected_reason` for the constraint shows entry
  succeeded, then later rows show closure
- `journalctl -u prediction-trader | grep "INC-020"` shows the submit + apply
  sequence with no error or `apply_exit_fills returned None`
- If partial: position remains open with reduced share count; subsequent
  ticks may attempt re-exit if ratio still favorable

**Pass conditions to mark validated**: 5 successful proactive exits with no
ledger drift > $1.

**Failure modes to watch for**:
- "zero fills on all submitted legs" → bid moved before FAK landed. Acceptable
  occasionally; if persistent, investigate book staleness.
- `apply_exit_fills returned None` → race / position already gone. Should not
  happen given current locking but worth logging.
- ledger drift > $1 → real bug, file new INC.

### F-pre-7 (Gamma freshness against real API)

**What's needed**: the boot probe runs on every restart and emits its verdict.
Operator confirms via Telegram + journal that the verdict matches expectation
(currently expected: `DEGRADED: Gamma filter saturated at 100`).

**Validation criteria**:
- Boot probe message present in journal within 30s of every startup
- Telegram alert fires on `FilterBroken` (one-shot per boot)
- No `FilterWorking` reports unless Polymarket fixes their filter (in which
  case we'd want to know — that's the *whole* point of the probe)

**Pass conditions**: 5 consecutive boots produce a clear, accurate verdict
with no false positives.

### F4 (supervised first trades)

**What's needed**: 3-5 live trades with the operator watching in real time
(dashboard + Telegram). For each trade, manually verify:
- Order placed on CLOB within expected price window
- Fills come back at expected size and price
- P&L tracking on dashboard matches the wallet delta
- Trade-status pipeline transitions cleanly: Submitted → Matched → Confirmed

**Pass conditions**: 3/5 trades clean; 2/5 with minor drift (<$0.10) is
acceptable for v0; >$0.10 drift = halt and investigate.

---

## P2 — Backfills awaiting trade data

Run as analysis tasks once trade volume > 10 trades. No code changes; pure
verification.

### F-pre-4 fill quality 95% validation

`scripts/validate_fill_quality.py` already exists. Once we have ≥10 live
fills (or a full week of trading, whichever first), run:

```bash
ssh dublin-ubuntu 'python3 /home/ubuntu/prediction-trader/scripts/validate_fill_quality.py \
    --jsonl /home/ubuntu/prediction-trader/data/fill_quality.jsonl \
    --threshold 0.70'
```

**Pass conditions**: ≥95% of trades have actual realised profit ≥ 70% of
expected profit at decision time.

**Action on fail**: investigate per-leg. Likely culprits: book staleness at
decision time, micro-moves between decision and submission, taker fee not
modelled.

### F5 48h supervised period

Starts after F4 passes. Operator checks dashboard + Telegram every 2-4 hours
for 48h. **No code/restart needed** — it's a supervision protocol.

**Pass conditions**:
- No alerts requiring intervention
- Daily summary numbers consistent with positions and wallet
- Dashboard never goes blank or shows stale data
- No INC-class events

---

## P3 — Audit cleanups (low priority)

### F-pre-8 deferred audit items

| Item | Description | Action |
|------|-------------|--------|
| ACC-2 | Suspense reversal idempotency | Review on next accounting work |
| NT-2 | Re-entry blocked during pending fills | Review with B4.5 work |
| NT-4 | Order ID dedup on re-submit | Already in tracked map, double-check |
| ACC-6 | Daily P&L reconciliation lag | Acceptable per audit v5 |
| ACC-7 | Wallet vs accounting reconciliation cadence | Periodic check exists |
| API-9 | Gamma rate-limit handling | Single REST call, not hot path |

**Action**: open a single follow-up Plan-mode session in 30 days to walk
each item against current code and decide done / defer / fix. None are
critical.

---

## Strategy — how to backfill without halting the live run

**Principle**: every backfill is either documentation, observation (waiting
for data), or a code change that ships via the existing safe-reboot cycle.

**Cadence**:
- **Daily**: check dashboard, journal for new errors, Telegram for alerts.
  No restart needed.
- **Weekly**: roll up trades, run `validate_fill_quality.py`, decide if F-pre-4
  is met. No restart needed.
- **As needed**: code changes ship via:
  ```
  WSL: edit + cargo build + commit + push
  Dublin: ssh, git pull, cargo build (incremental, ~60s),
          write reason tag, sudo systemctl restart
  ```
  Restart takes ~30s; engine reconciles open positions on startup so no
  trade state is lost.

**Restart budget**: aim for ≤1 restart per day for backfill work. The
pt-safe-reboot timer already restarts ~weekly for kernel/libc updates;
batching backfill changes around those reboots is even cheaper.

**No halts unless**:
- Critical bug discovered (= new INC entry, immediate restart with kill switch)
- F4 supervised trades reveal divergence > $0.10 (= halt + investigate)
- Geoblock fires (= follow OPS_GEOBLOCK_RUNBOOK.md)

---

## Acceptance ladder for promotion to F-Main / Milestone G

The gates compound — each one is a precondition for the next.

```
P1 backfills (F-pre-1, F-pre-7, F4)         → first 3-5 trades validated
        ↓
P2 backfills (F-pre-4, F5)                   → 48h clean operation, ≥10 trades
        ↓
F1 scale-up to $1,000                        → bigger positions, real margin tests
        ↓
F6 steady state (autonomous, daily check)    → 30 days clean
        ↓
Milestone G: scale-up + post-launch features
```

Today's state: **P1 backfills pending the first real trade**. The bot is
live, armed, and waiting. The plan does not require another deploy until
P2 starts (or a critical fix lands).

---

## Cross-references

- `INCIDENT_LOG.md` — INC-018, INC-019, INC-020 — the gaps that surfaced
  during this backfill audit
- `OPS_GEOBLOCK_RUNBOOK.md` — F-pre-6 backfill (this session)
- `CHANGELOG.md` — v0.19.0 / v0.20.x risk-mitigation entries that completed
  most F-pre items
- `PRODUCT_SPEC_v2.md` § Milestone F — original gate definitions
- `scripts/validate_fill_quality.py` — F-pre-4 validation entry point
- `scripts/ops/pt-safe-reboot.sh` — restart cadence (drain-aware, kernel-update
  triggered)
