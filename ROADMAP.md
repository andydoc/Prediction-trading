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
| ✅ Complete | 27/27 |

### B-Part 1: Liquidity & Position Management (3/3 ✅)
Depth gating, replacement chain analytics, closed position retention.

### B-Part 2: Signing & Instrument Model (4/4 ✅)
EIP-712 order signing (pure Rust, alloy stack), formal instrument model, dynamic tick size handling.

### B-Part 3: Executor, Pipeline & Rate Limiting (8/8 ✅)
Market BUY quantity guard, LiveExecutor (dry-run), trade status pipeline, rate limiting, timestamp normalisation, error handling matrix, partial fill handling, batch order submission.

### B-Part 4: Reconciliation & Tracking (4/4 ✅)
Periodic + startup reconciliation, cross-asset fill matching, FOK/FAK overfill handling, live P&L tracking (unrealized + realized on dashboard).

### B-Part 5: Documentation (2/2 ✅)
ARCHITECTURE.md, ROADMAP.md (this file).

**Verification**: 44/44 unit tests pass. Shadow mode verified on VPS. Reconciliation runs cleanly. Dashboard P&L charts and stats bar confirmed. Tier B WS stable at 16 connections.

---

## Milestone C: Safety Infrastructure — ✅ Complete

**Goal**: System can protect capital automatically and alert the operator. No funded account required.

| Status | Count |
|--------|-------|
| ✅ Complete | 10/10 |

**Done**: Circuit breaker (C1), POL gas balance monitoring (C1.1), kill switch (C2), Telegram notifications (C3), daily P&L report (C4), seamless close transition (C4.1), proactive near-resolution exit (C4.2), OPS_RUNBOOK.md (C5), USER_GUIDE.md (C6), retire PROGRESS_ROADMAP.md (C7).

---

## Milestone D: CLOB Integration Test — 🔧 In Progress

**Goal**: Prove full execution path works against real Polymarket CLOB. Place, fill, cancel, and reconcile real micro-orders. Target: 1 week.

| Status | Count |
|--------|-------|
| ✅ Complete | 7/8 |
| 🔧 In Progress | 1/8 |

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
| D6: Cold-start reconciliation | 🔧 | GTC sell placed, waiting for WS MATCHED→CONFIRMED (48h patience). Helper restarts main on venue change. |
| D7a: Circuit breaker | ✅ | Engine state validation |
| D7b: Kill switch | ✅ | Executor cancel-all |
| D8: Closeout positions | ✅ | Real SELL orders + accounting verification |

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
| D: CLOB Integration Test | 7/8 | 🔧 In Progress |
| E: Shadow Validation | 1/10 | 🔧 In Progress |
| F: Go Live | 0/6 | ⬚ Planned |
| G: Scale | 0/4 | ⬚ Future |
