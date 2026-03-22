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

## Milestone B: Build Execution Infrastructure — 🔧 Rework In Progress

**Goal**: Code-complete execution path — all modules built, unit-tested, and shadow-testable. No funded account required.

| Status | Count |
|--------|-------|
| ✅ Complete | 24/29 |
| 🔧 Rework | 3/29 (B2.3, B3.2, B4.0, B4.1) |
| 🔧 New | 2/29 (B4.5, B4.6) |

### B-Part 1: Liquidity & Position Management (3/3 ✅)
Depth gating, replacement chain analytics, closed position retention.

### B-Part 2: Signing & Instrument Model (3/4 ✅, 1 🔧)
EIP-712 order signing, dynamic tick size handling. **B2.3 rework**: FAK vs GTC have different amount precision rules — verify rounding across all tick sizes and order types.

### B-Part 3: Executor, Pipeline & Rate Limiting (7/8 ✅, 1 🔧)
Market BUY quantity guard, LiveExecutor, rate limiting, timestamp normalisation, error handling, partial fills, batch orders. **B3.2 rework**: Trade lifecycle must use WS `id` as correlation key, accept MATCHED for suspense entry, handle ~20% WS CONFIRMED drop rate with Data API fallback.

### B-Part 4: Reconciliation & Tracking (3/6 ✅, 3 🔧)
Cross-asset fill matching, FOK/FAK overfill handling, live P&L tracking. **B4.0/B4.1 rework**: enhanced reconciliation (Data API freshness polling, apply_reconciliation, venue = source of truth). **B4.5 new**: parallel WS + Data API trade confirmation. **B4.6 new**: USDC.e balance monitor.

### B-Part 5: Documentation (2/2 ✅)
ARCHITECTURE.md, ROADMAP.md (this file).

**Verification**: 44/44 unit tests pass. Milestone D 8/8 PASS validates execution path end-to-end.

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
| ✅ Complete | 1/10 |
| ⬚ Remaining | 9/10 |

**Done**: Multi-instance support (E1) — `--instance <n>` flag, per-instance config overlays.

**Remaining**: Deploy 6 instances (Shadow-A through Shadow-F), stress tests, comparison dashboard, 14-day continuous run, parameter selection, CTO sign-off.

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
| B: Execution Infrastructure | 27/27 | ✅ Complete |
| C: Safety | 10/10 | ✅ Complete |
| D: CLOB Integration Test | 8/8 | ✅ Complete |
| E: Shadow Validation | 1/10 | 🔧 In Progress |
| F: Go Live | 0/6 | ⬚ Planned |
| G: Scale | 0/4 | ⬚ Future |
