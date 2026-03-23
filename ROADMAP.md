# Roadmap

Phases numbered by execution priority. Each phase: goal, status, task summary. Cross-references [ARCHITECTURE.md](ARCHITECTURE.md) for technical detail.

---

## Milestone A: Complete Rust Port — ✅ Complete

**Goal**: Single compiled Rust binary with zero Python runtime. All hardcoded thresholds parameterised in `config.yaml`.

| Status | Count |
|--------|-------|
| ✅ Complete | 13/13 |

All core modules ported: WebSocket, order book mirror, arb math, position manager, constraint detection, market scanner, resolution validator, postponement detector, dashboard, notifications. Three rationalised scripts (`start.sh`, `restart.sh`, `kill.sh`). CHANGELOG.md and INCIDENT_LOG.md created.

---

## Milestone B: Build Execution Infrastructure — ✅ Complete

**Goal**: Code-complete execution path — all modules built, unit-tested, and shadow-testable. No funded account required.

| Status | Count |
|--------|-------|
| ✅ Complete | 29/29 |

### B-Part 1: Liquidity & Position Management (3/3 ✅)
Depth gating, replacement chain analytics, closed position retention.

### B-Part 2: Signing & Instrument Model (4/4 ✅)
EIP-712 order signing, dynamic tick size handling. B2.3: FAK vs GTC precision verified with 21 comprehensive tests across all tick sizes and order types.

### B-Part 3: Executor, Pipeline & Rate Limiting (8/8 ✅)
Market BUY quantity guard, LiveExecutor, rate limiting, timestamp normalisation, error handling, partial fills, batch orders. B3.2: Suspense accounting (MATCHED → suspense, CONFIRMED → real, FAILED → reverse + sell opposing legs). `FillAction` enum + `process_fill_event()`.

### B-Part 4: Reconciliation & Tracking (6/6 ✅)
Cross-asset fill matching, FOK/FAK overfill handling, live P&L tracking. B4.0/B4.1: Enhanced reconciliation with Data API freshness polling (`query_clob_positions_fresh`) + auto `apply_reconciliation`. B4.5: Parallel WS + Data API trade confirmation (`fill_confirmation.rs`). B4.6: USDC.e balance monitor (`usdc_monitor.rs`) with dashboard integration.

### B-Part 5: Documentation (2/2 ✅)
ARCHITECTURE.md, ROADMAP.md (this file).

**Verification**: 101/101 unit tests pass. Milestone D 8/8 PASS validates execution path end-to-end.

---

## Milestone C: Safety Infrastructure — ✅ Complete

**Goal**: System can protect capital automatically and alert the operator. No funded account required.

| Status | Count |
|--------|-------|
| ✅ Complete | 10/10 |

**Done**: Circuit breaker (C1), POL gas balance monitoring (C1.1), kill switch (C2), Telegram notifications (C3), daily P&L report (C4), seamless close transition (C4.1), proactive near-resolution exit (C4.2), OPS_RUNBOOK.md (C5), USER_GUIDE.md (C6), retire PROGRESS_ROADMAP.md (C7).

---

## Milestone D: CLOB Integration Test — ✅ Complete

**Goal**: Prove full execution path works against real Polymarket CLOB. Place, fill, cancel, and reconcile real micro-orders. Target: 1 week.

| Status | Count |
|--------|-------|
| ✅ Complete | 8/8 |

**VPS**: is*hosting Madrid (176.97.72.199). Interim location — Dublin (Interxion DC, 0.83ms latency) planned when capacity available.
**Funds**: ~$50 USDC.e deposited, wallet 0x21f1...fb1.
**First real CLOB order**: 2026-03-20 from Madrid VPS.

| Task | Status | Notes |
|------|--------|-------|
| D1: Deposit test funds | ✅ | REST API balance check |
| D2: Submit + cancel order | ✅ | GTC order, verify on book, cancel |
| D3: Real micro-fill | ✅ | FAK BUY at market price |
| D4: negRisk market fill | ✅ | negRisk-specific signing |
| D5: Multi-leg arb execution | ✅ | WS User Channel fill tracking confirmed. Auth fix: WS uses raw creds (not HMAC). |
| D6: Cold-start reconciliation | ✅ | Data API freshness polling, venue state change detection, apply_reconciliation (venue source of truth). FAK/GTC amount precision fix. |
| D7a: Circuit breaker | ✅ | Engine state validation |
| D7b: Kill switch | ✅ | Executor cancel-all |
| D8: Closeout positions | ✅ | Real SELL orders + accounting verification |

### Known Issues / Next Steps (from D)

- **Trade confirmation reliability**: WS User Channel drops ~20% of CONFIRMED events. Current mitigation: accept MATCHED, fall back to Data API. **Next**: run WS + Data API poll in async parallel, take first confirmation. Needs integration in: fill_tracker, D6 helper, D8 closeout, production executor.
- **USDC balance monitor**: Add periodic on-chain USDC.e balance check to production engine (like gas_monitor for POL).
- **Suspense accounting**: MATCHED trades should go to suspense account, CONFIRMED promotes to real position. Failed matches reverse + sell opposing arb legs.

---

## Milestone E: Shadow Validation (14-Day Gate) — 🔧 In Progress

**Goal**: Prove complete system works without real money. Determine optimal parameters via 6 parallel shadow instances.

| Status | Count |
|--------|-------|
| ✅ Complete | 6/12 |
| 🔄 In Progress | 1/12 |
| ⬚ Remaining | 5/12 |

### Completed
- **E1**: Multi-instance support — `--instance <n>` flag, per-instance config overlays.
- **E2.5**: Stress test harness — `scripts/stress_test.py` with `--profile worst-case`, crash simulation, safe-zone analysis, config auto-update. `/metrics` endpoint (35 fields).
- **E3**: Comparison dashboard — Strategies tab with Sharpe/Sortino/Recovery Factor/Profit Factor/Max Drawdown/Capital Utilisation. `--test-period` auto-stop. Test period countdown banner.
- **Audit v5-v7**: 22 findings fixed (API auth, accounting, robustness, fill validation). 101/101 tests pass.
- **Risk mitigations** (external risk analysis, 2026-03-23):
  - P1: Geoblock health check (`scripts/geoblock_check.sh`) — 15-min cron, CLOB + Gamma 403 detection.
  - P2: Risk register expanded 14 → 19 risks, corrected ratings (INC-001, INC-012).
  - P3: Dashboard security documented (SSH tunnel, kill switch exposure).
  - P4: Suspicious arb flagging — arbs > 20% profit flagged WARN + dashboard `[!]`.
  - P5: negRisk correlated exposure cap — proportional scaling to 50% max.
  - P6: UMA dispute active monitoring — detects `proposed`/`disputed` status, excludes from replacement.
- **Sports WebSocket** (`sports_ws.rs`): Persistent connection to Polymarket Sports WS. Pre-screens postponement detection before AI calls. SQLite persistence. 6 unit tests. 107/107 tests pass.

### In Progress
- **E2.6**: Running on VPS (~35hrs, worst-case profile, target-cpu=native). **CRITICAL: Review results and update config.yaml parameters BEFORE starting E4.**

### Remaining
E4 (14-day run), E5 (execution validation), E6 (reconciliation clean), E7 (performance baseline), E9 (parameter selection), E10 (CTO sign-off).

Shadow-F targets short-lived crypto price markets (5-15 min resolution) with fast rebuild intervals (60s) and low replacement cooldown (10s).

---

## Milestone F: Go Live — ⬚ Not Started

**Goal**: First real money trades with optimised parameters from shadow validation.

| Status | Count |
|--------|-------|
| ⬚ Remaining | 6/6 |

**Funds required**: $1,000 USDC additional (~$1,050 total including D1 deposit).

Tasks: deposit funds, configure winning parameters, switch to live mode, supervised first trades, 48h monitoring period, steady state confirmation.

---

## Milestone G: Stabilise & Scale — ⬚ Not Started

**Goal**: Post-launch improvements. Not blocking go-live. Prioritise based on operational experience.

| Status | Count |
|--------|-------|
| ⬚ Remaining | 4/4 |

- Increase capital allocation (after 30 days profitable)
- Historical performance dashboard
- Dashboard control panel (mode switch, config edit)
- Multi-exchange support (Kalshi)

---

## Summary

| Milestone | Tasks | Status |
|-----------|-------|--------|
| A: Rust Port | 13/13 | ✅ Complete |
| B: Execution Infrastructure | 29/29 | ✅ Complete |
| C: Safety | 10/10 | ✅ Complete |
| D: CLOB Integration Test | 8/8 | ✅ Complete |
| E: Shadow Validation | 6/12 | 🔧 In Progress |
| F: Go Live | 0/6 | ⬚ Planned |
| G: Scale | 0/4 | ⬚ Future |
